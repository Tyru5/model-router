//! Unix process supervision. No command is evaluated by a shell.
use crate::adapters::Invocation;
use anyhow::{Context, Result, ensure};
use portable_pty::{CommandBuilder, PtySize};
use std::{
    fs::File,
    io::{self, IsTerminal, Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct Outcome {
    pub code: Option<i32>,
    pub cancelled: bool,
    pub timed_out: bool,
}
impl Outcome {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.cancelled && !self.timed_out
    }
}

fn nonblocking(fd: i32) -> io::Result<()> {
    // SAFETY: fcntl does not take ownership; fd is borrowed from a live pipe/PTY.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn kill_group(pid: u32) {
    signal_group(pid, libc::SIGKILL);
}

fn signal_group(pid: u32, signal: i32) {
    // SAFETY: spawned children have a dedicated process group/session; never use 0.
    if pid > 1 {
        unsafe {
            libc::kill(-(pid as i32), signal);
        }
    }
}

/// How long a cancelled or timed-out harness may take to save its session
/// (for takeover) after SIGTERM, before SIGKILL.
const GRACE: Duration = Duration::from_secs(5);

struct ChildGuard(Child, bool);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.1 {
            kill_group(self.0.id());
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn drain(
    reader: &mut impl Read,
    log: &mut File,
    stderr: bool,
    on_output: &mut impl FnMut(bool, &str),
) -> Result<()> {
    let mut bytes = [0u8; 8192];
    // Bound each pass so a noisy process cannot starve cancellation/timeouts.
    for _ in 0..32 {
        match reader.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => {
                log.write_all(&bytes[..n])?;
                on_output(stderr, &String::from_utf8_lossy(&bytes[..n]));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn capture(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
    mut on_output: impl FnMut(bool, &str),
) -> Result<Outcome> {
    let mut stdout_log = File::create(dir.join("stdout.log"))?;
    let mut stderr_log = File::create(dir.join("stderr.log"))?;
    let mut command = Command::new(&inv.program);
    command
        .args(&inv.args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if inv.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .process_group(0);
    command.envs(inv.environment()?);
    let mut child = ChildGuard(
        command
            .spawn()
            .with_context(|| format!("Launch {}", inv.program))?,
        true,
    );
    let mut stdout = child.0.stdout.take().context("No stdout pipe")?;
    let mut stderr = child.0.stderr.take().context("No stderr pipe")?;
    let mut stdin = child.0.stdin.take();
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    if let Some(input) = &stdin {
        nonblocking(input.as_raw_fd())?;
    }
    let input = inv.stdin.as_deref().unwrap_or("").as_bytes();
    let mut sent = 0;
    let started = Instant::now();
    let mut cancelled = false;
    let mut timed_out = false;
    let status = loop {
        drain(&mut stdout, &mut stdout_log, false, &mut on_output)?;
        drain(&mut stderr, &mut stderr_log, true, &mut on_output)?;
        if let Some(pipe) = &mut stdin {
            match pipe.write(&input[sent..]) {
                Ok(n) => sent += n,
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => sent = input.len(),
                Err(e) => return Err(e.into()),
            }
            if sent == input.len() {
                stdin.take();
            }
        }
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        cancelled = cancel.load(Ordering::Relaxed);
        timed_out = started.elapsed() >= timeout;
        if cancelled || timed_out {
            // Ask first: harnesses such as omp only save a session they can
            // resume when they exit cleanly.
            signal_group(child.0.id(), libc::SIGTERM);
            let deadline = Instant::now() + GRACE;
            let status = loop {
                drain(&mut stdout, &mut stdout_log, false, &mut on_output)?;
                drain(&mut stderr, &mut stderr_log, true, &mut on_output)?;
                if let Some(status) = child.0.try_wait()? {
                    // Leftover group members never get the grace period twice.
                    kill_group(child.0.id());
                    break status;
                }
                if Instant::now() >= deadline {
                    kill_group(child.0.id());
                    let _ = child.0.kill();
                    break child.0.wait()?;
                }
                thread::sleep(Duration::from_millis(25));
            };
            break status;
        }
        thread::sleep(Duration::from_millis(25));
    };
    // Once the leader exits, drain available bytes without waiting for inherited
    // descriptors held by background grandchildren.
    drain(&mut stdout, &mut stdout_log, false, &mut on_output)?;
    drain(&mut stderr, &mut stderr_log, true, &mut on_output)?;
    stdout_log.sync_all()?;
    stderr_log.sync_all()?;
    child.1 = false;
    Ok(Outcome {
        code: status.code(),
        cancelled,
        timed_out,
    })
}

struct RawGuard;
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
struct PtyChild {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    finished: bool,
}
impl Drop for PtyChild {
    fn drop(&mut self) {
        if !self.finished {
            if let Some(pid) = self.child.process_id() {
                kill_group(pid);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Forward raw bytes, including bracketed paste and escape sequences, to a real
/// PTY. This avoids re-encoding keys or embedding one harness's UI in another.
pub fn interactive(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    cancel: &AtomicBool,
) -> Result<Outcome> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "Interactive stages require a terminal; reopen in the TUI or set this stage to unattended"
    );
    let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
    let pair = portable_pty::native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new(&inv.program);
    cmd.args(&inv.args);
    for (name, value) in inv.environment()? {
        cmd.env(name, value);
    }
    cmd.cwd(cwd);
    let mut child = PtyChild {
        child: pair.slave.spawn_command(cmd)?,
        finished: false,
    };
    drop(pair.slave);
    let fd = pair
        .master
        .as_raw_fd()
        .context("PTY has no file descriptor")?;
    nonblocking(fd)?;
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let mut transcript = File::create(dir.join("stdout.log"))?;
    File::create(dir.join("stderr.log"))?;
    println!(
        "\nLaunching {}. Exit the harness to inspect the handoff. [Ctrl+]] stops this stage.\n",
        inv.program
    );
    crossterm::terminal::enable_raw_mode()?;
    let _raw = RawGuard;
    let mut output = io::stdout();
    let mut bytes = [0u8; 8192];
    let mut input = Vec::new();
    let mut size = (cols, rows);
    let mut cancelled = false;
    let status = loop {
        let mut polls = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: valid, initialized array of two pollfds; raw input is borrowed.
        let poll_result = unsafe { libc::poll(polls.as_mut_ptr(), 2, 40) };
        if poll_result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error().into());
        }
        // A disconnected PTY may not send SIGHUP (for example, when it is not
        // our controlling terminal). Stop on hangup/EOF instead of busy-polling.
        cancelled |= polls[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
        if !cancelled && polls[0].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(0, bytes.as_mut_ptr().cast(), bytes.len()) };
            if n > 0 {
                if bytes[..n as usize].contains(&0x1d) {
                    cancelled = true;
                } else {
                    input.extend_from_slice(&bytes[..n as usize]);
                }
            } else if n == 0 {
                cancelled = true;
            } else {
                let error = io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err(error.into());
                }
            }
        }
        // Cancel before forwarding output to a terminal that has disappeared.
        if cancelled || cancel.load(Ordering::Relaxed) {
            cancelled = true;
            if let Some(status) = child.child.try_wait()? {
                break status;
            }
            if let Some(pid) = child.child.process_id() {
                kill_group(pid);
            }
            child.child.kill()?;
            break child.child.wait()?;
        }
        if !input.is_empty() {
            match writer.write(&input) {
                Ok(n) => {
                    input.drain(..n);
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => input.clear(),
                Err(e) => return Err(e.into()),
            }
        }
        for _ in 0..32 {
            match reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    transcript.write_all(&bytes[..n])?;
                    output.write_all(&bytes[..n])?;
                    output.flush()?;
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.raw_os_error() == Some(libc::EIO) =>
                {
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(status) = child.child.try_wait()? {
            break status;
        }
        if let Ok(next_size) = crossterm::terminal::size() {
            if next_size != size {
                size = next_size;
                pair.master.resize(PtySize {
                    cols: size.0,
                    rows: size.1,
                    pixel_width: 0,
                    pixel_height: 0,
                })?;
            }
        }
    };
    // Pick up final bytes emitted immediately before child exit.
    for _ in 0..32 {
        match reader.read(&mut bytes) {
            Ok(n) if n > 0 => {
                transcript.write_all(&bytes[..n])?;
                output.write_all(&bytes[..n])?;
            }
            _ => break,
        }
    }
    output.flush()?;
    transcript.sync_all()?;
    child.finished = true;
    Ok(Outcome {
        code: Some(status.exit_code() as i32),
        cancelled,
        timed_out: false,
    })
}

pub fn read_tail(path: &Path, max: usize) -> Result<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(max as u64)))?;
    let mut data = Vec::new();
    file.take(max as u64).read_to_end(&mut data)?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}
