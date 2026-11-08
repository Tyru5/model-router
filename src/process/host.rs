//! The terminal host: a small process, separate from mr, that owns an
//! interactive harness's PTY and screen. mr connects over a private Unix
//! socket, so quitting mr leaves the harness running and a later mr
//! reattaches with the screen intact. One mr is attached at a time. While
//! none is, the host holds the run and workspace locks, so no other run
//! takes the checkout; an attaching mr takes them over.
use super::{
    GRACE, INPUT_LIMIT, OutputDecoder, PtyChild, SessionState, drain_pty, kill_group, nonblocking,
    signal_group,
};
use anyhow::{Context, Result, bail, ensure};
use portable_pty::{CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

/// The hidden first argument that runs the host instead of the router.
pub const ARG: &str = "__pty-host";
/// Written by the host when the harness ends, so an mr that was away when it
/// exited still learns how.
pub(super) const EXIT_FILE: &str = "terminal-exit.json";

/// What the host launches, sent on its stdin so environment values (which
/// can hold credentials) never touch the disk.
#[derive(Serialize, Deserialize)]
pub(super) struct Spec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// The attempt directory: stdout.log and the exit record go here.
    pub dir: PathBuf,
    pub env: Vec<(String, String)>,
    pub removed: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    pub socket: PathBuf,
    /// Held while no mr is connected, in this order.
    pub locks: Vec<PathBuf>,
    pub warning: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Exit {
    pub code: Option<i32>,
    pub cancelled: bool,
}

/// Frame tags. Client to host: attach, input, resize, stop. Host to client:
/// the harness's process ID (attach accepted), busy (another mr is attached),
/// output, exit.
pub(super) mod tag {
    pub const ATTACH: u8 = b'a';
    pub const INPUT: u8 = b'i';
    pub const RESIZE: u8 = b'r';
    pub const STOP: u8 = b's';
    pub const OUTPUT: u8 = b'o';
    pub const SPAWNED: u8 = b'p';
    pub const BUSY: u8 = b'b';
    pub const EXIT: u8 = b'x';
}
/// No legitimate frame is larger; a bigger length means a corrupt stream.
const FRAME_LIMIT: usize = 1 << 20;
/// Connections that have not attached yet (probes, a racing mr).
const PENDING_LIMIT: usize = 8;

pub(super) fn frame(tag: u8, payload: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
}

/// Splits a byte stream into frames.
#[derive(Default)]
pub(super) struct Frames {
    buffer: Vec<u8>,
}
impl Frames {
    /// Reads what is available without blocking. False at end of stream or
    /// on an error that ends the connection.
    pub fn fill(&mut self, stream: &mut UnixStream) -> bool {
        let mut bytes = [0u8; 16 * 1024];
        loop {
            match stream.read(&mut bytes) {
                Ok(0) => return false,
                Ok(n) => {
                    self.buffer.extend_from_slice(&bytes[..n]);
                    if self.buffer.len() > 4 * FRAME_LIMIT {
                        return true;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
    }
    pub fn next(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        if self.buffer.len() < 5 {
            return Ok(None);
        }
        let length = u32::from_be_bytes(self.buffer[1..5].try_into().expect("four bytes")) as usize;
        ensure!(length <= FRAME_LIMIT, "Terminal host stream is corrupt");
        if self.buffer.len() < 5 + length {
            return Ok(None);
        }
        let tag = self.buffer[0];
        let payload = self.buffer[5..5 + length].to_vec();
        self.buffer.drain(..5 + length);
        Ok(Some((tag, payload)))
    }
}

/// Writes `out[*written..]` without blocking. False when the peer is gone.
pub(super) fn flush(stream: &mut UnixStream, out: &mut Vec<u8>, written: &mut usize) -> bool {
    while *written < out.len() {
        match stream.write(&out[*written..]) {
            Ok(0) => return false,
            Ok(n) => *written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    out.clear();
    *written = 0;
    true
}

/// A private directory for sockets. Socket paths are limited to about 100
/// bytes, so they cannot live in the run directory.
fn socket_dir() -> Result<PathBuf> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
    {
        Some(runtime) => runtime.join("model-router"),
        None => std::env::temp_dir().join(format!("model-router-{uid}")),
    };
    match fs::create_dir(&dir) {
        Ok(()) => fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("Create {}", dir.display())),
    }
    // The socket accepts keystrokes for an agent: refuse a directory another
    // user could have planted or can read.
    let meta = fs::symlink_metadata(&dir)?;
    ensure!(
        meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0,
        "{} must be a private directory owned by you",
        dir.display()
    );
    Ok(dir)
}

pub(super) fn socket_path() -> Result<PathBuf> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    Ok(socket_dir()?.join(format!("{}.sock", &id[..16])))
}

static PROGRAM: OnceLock<PathBuf> = OnceLock::new();

/// Runs hosts from `program` instead of this executable; for test binaries,
/// which cannot act as the host themselves.
pub fn use_program(program: PathBuf) {
    let _ = PROGRAM.set(program);
}

fn program() -> Result<PathBuf> {
    if let Some(program) = PROGRAM.get() {
        return Ok(program.clone());
    }
    // Still runnable after an upgrade replaced the file on disk.
    #[cfg(target_os = "linux")]
    return Ok(PathBuf::from("/proc/self/exe"));
    #[cfg(not(target_os = "linux"))]
    return std::env::current_exe().context("Cannot locate the model-router executable");
}

/// Starts a host for `spec` and returns the harness's process ID once it runs.
pub(super) fn spawn(spec: &Spec) -> Result<u32> {
    #[cfg(test)]
    if PROGRAM.get().is_none() {
        return spawn_thread(spec);
    }
    let log = File::create(spec.dir.join("terminal-host.log"))?;
    let mut command = Command::new(program()?);
    command
        .arg(ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log)
        .current_dir(&spec.cwd);
    // SAFETY: setsid is async-signal-safe. A new session detaches the host
    // from mr's terminal and process group, so neither a closed terminal nor
    // mr's exit signals it.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("Start the terminal host")?;
    let mut stdin = child.stdin.take().context("Terminal host stdin")?;
    stdin.write_all(&serde_json::to_vec(spec)?)?;
    drop(stdin);
    let mut line = String::new();
    BufReader::new(child.stdout.take().context("Terminal host stdout")?).read_line(&mut line)?;
    // mr reaps the host while it lives; after mr exits, init does.
    thread::spawn(move || {
        let _ = child.wait();
    });
    ready(&line, &spec.dir)
}

fn ready(line: &str, dir: &Path) -> Result<u32> {
    let line = line.trim_end();
    if let Some(pid) = line.strip_prefix("ok ") {
        return pid
            .parse()
            .context("Terminal host reported a bad process ID");
    }
    if let Some(error) = line.strip_prefix("error ") {
        bail!("{error}");
    }
    bail!(
        "The terminal host stopped before starting the harness; see {}",
        dir.join("terminal-host.log").display()
    )
}

#[cfg(test)]
fn spawn_thread(spec: &Spec) -> Result<u32> {
    let text = serde_json::to_string(spec)?;
    let dir = spec.dir.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let spec: Spec = serde_json::from_str(&text).expect("spec");
        let _ = serve(&spec, |line| {
            let _ = tx.send(line);
        });
    });
    ready(&rx.recv().unwrap_or_default(), &dir)
}

/// The host's entry point; never returns.
pub fn main() -> ! {
    let result = (|| -> Result<()> {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        let spec: Spec = serde_json::from_str(&text)?;
        serve(&spec, |line| {
            let mut stdout = io::stdout();
            let _ = stdout.write_all(line.as_bytes());
            let _ = stdout.flush();
        })
    })();
    if let Err(error) = &result {
        let _ = writeln!(io::stderr(), "{error:#}");
    }
    std::process::exit(i32::from(result.is_err()))
}

struct Client {
    stream: UnixStream,
    frames: Frames,
    out: Vec<u8>,
    written: usize,
}

/// Binds the socket, launches the harness, reports it through `ready`, and
/// supervises it until it exits.
fn serve(spec: &Spec, ready: impl FnOnce(String)) -> Result<()> {
    let listener = match UnixListener::bind(&spec.socket) {
        Ok(listener) => listener,
        Err(error) => {
            ready(format!("error Bind {}: {error}\n", spec.socket.display()));
            return Err(error.into());
        }
    };
    struct Remove<'a>(&'a Path);
    impl Drop for Remove<'_> {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0);
        }
    }
    let _remove = Remove(&spec.socket);
    let started = (|| -> Result<_> {
        fs::set_permissions(&spec.socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let pair = portable_pty::native_pty_system().openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        for name in &spec.removed {
            cmd.env_remove(name);
        }
        for (name, value) in &spec.env {
            cmd.env(name, value);
        }
        cmd.cwd(&spec.cwd);
        let transcript = File::create(spec.dir.join("stdout.log"))?;
        let fd = pair
            .master
            .as_raw_fd()
            .context("PTY has no file descriptor")?;
        nonblocking(fd)?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let spawned = pair.slave.spawn_command(cmd)?;
        let pid = spawned
            .process_id()
            .context("The harness has no process ID")?;
        Ok((pair.master, fd, reader, writer, transcript, spawned, pid))
    })();
    let (master, fd, mut reader, mut writer, mut transcript, spawned, pid) = match started {
        Ok(started) => started,
        Err(error) => {
            ready(format!("error {error:#}\n"));
            return Err(error);
        }
    };
    let mut child = PtyChild {
        child: spawned,
        pid: Some(pid),
    };
    ready(format!("ok {pid}\n"));

    let mut state = SessionState::new(spec.cols, spec.rows);
    if let Some(warning) = &spec.warning {
        state.output(format!("{warning}\r\n").as_bytes());
    }
    let mut size = state.size;
    let mut decoder = OutputDecoder::default();
    let mut bytes = [0u8; 8192];
    let mut eof = false;
    let mut client: Option<Client> = None;
    let mut pending: Vec<(UnixStream, Frames)> = Vec::new();
    let mut locks: Vec<crate::storage::FileLock> = Vec::new();
    let mut stopping: Option<Instant> = None;
    let status = loop {
        // The attached mr first, so one that just let go is gone before a
        // new one asks to attach.
        if let Some(active) = &mut client {
            let mut open = active.frames.fill(&mut active.stream);
            loop {
                match active.frames.next() {
                    Ok(Some((tag::INPUT, payload))) => {
                        let room = INPUT_LIMIT.saturating_sub(state.input.len());
                        state.input.extend(&payload[..payload.len().min(room)]);
                    }
                    Ok(Some((tag::RESIZE, payload))) if payload.len() == 4 => {
                        let cols = u16::from_be_bytes([payload[0], payload[1]]);
                        let rows = u16::from_be_bytes([payload[2], payload[3]]);
                        state.resize(cols, rows);
                    }
                    Ok(Some((tag::STOP, _))) => {
                        if stopping.is_none() {
                            // Ask first: some harnesses only save a resumable
                            // session when they exit cleanly.
                            signal_group(pid, libc::SIGTERM);
                            stopping = Some(Instant::now() + GRACE);
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {
                        open = false;
                        break;
                    }
                }
            }
            if open && active.written == active.out.len() {
                let mut raw = Vec::new();
                state.take_output(&mut raw);
                if !raw.is_empty() {
                    frame(tag::OUTPUT, &raw, &mut active.out);
                }
            }
            if !open || !flush(&mut active.stream, &mut active.out, &mut active.written) {
                client = None;
                state.attached = false;
                state.output.clear();
            }
        }
        // New connections become the client once they ask to attach; a
        // probe that only connects never disturbs the attached mr.
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_ok() {
                        if pending.len() == PENDING_LIMIT {
                            pending.remove(0);
                        }
                        pending.push((stream, Frames::default()));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let mut index = 0;
        while index < pending.len() {
            let (stream, frames) = &mut pending[index];
            let open = frames.fill(stream);
            if frames.buffer.first() == Some(&tag::ATTACH) && frames.buffer.len() >= 5 {
                let (mut stream, mut frames) = pending.remove(index);
                let _ = frames.next();
                let mut out = Vec::new();
                // The attached mr may have let go since it was last read.
                if client
                    .as_mut()
                    .is_some_and(|active| !active.frames.fill(&mut active.stream))
                {
                    client = None;
                    state.attached = false;
                    state.output.clear();
                }
                if client.is_some() {
                    // Never take a live session from the mr attached to it.
                    frame(tag::BUSY, &[], &mut out);
                    let _ = stream.write(&out);
                    continue;
                }
                frame(tag::SPAWNED, &pid.to_be_bytes(), &mut out);
                client = Some(Client {
                    stream,
                    frames,
                    out,
                    written: 0,
                });
                state.attached = true;
                state.redraw = true;
                state.output.clear();
                // The attached mr takes the locks over.
                locks.clear();
            } else if !open
                || frames
                    .buffer
                    .first()
                    .is_some_and(|first| *first != tag::ATTACH)
            {
                pending.remove(index);
            } else {
                index += 1;
            }
        }
        if client.is_none() && locks.is_empty() && !spec.locks.is_empty() {
            // All or none, in mr's order; retried until the last mr lets go.
            let taken: Result<Vec<_>> =
                spec.locks.iter().map(|p| crate::storage::lock(p)).collect();
            if let Ok(taken) = taken {
                locks = taken;
            }
        }
        if state.size != size {
            size = state.size;
            master.resize(PtySize {
                cols: size.0,
                rows: size.1,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        }
        if !state.input.is_empty() {
            let (front, _) = state.input.as_slices();
            match writer.write(front) {
                Ok(n) => {
                    state.input.drain(..n);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => state.input.clear(),
            }
        }
        drain_pty(
            &mut reader,
            &mut transcript,
            &mut decoder,
            &mut |frame| state.output(frame),
            &mut bytes,
            &mut eof,
        )?;
        if let Some(status) = child.child.try_wait()? {
            break status;
        }
        if stopping.is_some_and(|deadline| Instant::now() >= deadline) {
            kill_group(pid);
            let _ = child.child.kill();
            break child.child.wait()?;
        }
        let mut polls = vec![
            libc::pollfd {
                fd: if eof { -1 } else { fd },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if let Some(active) = &client {
            polls.push(libc::pollfd {
                fd: active.stream.as_raw_fd(),
                events: libc::POLLIN
                    | if active.written < active.out.len() {
                        libc::POLLOUT
                    } else {
                        0
                    },
                revents: 0,
            });
        }
        for (stream, _) in &pending {
            polls.push(libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // SAFETY: every descriptor is borrowed and outlives the call.
        unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, 40) };
    };
    drain_pty(
        &mut reader,
        &mut transcript,
        &mut decoder,
        &mut |frame| state.output(frame),
        &mut bytes,
        &mut eof,
    )?;
    // No descendant outlives the harness, as when mr owned it.
    drop(child);
    let _ = transcript.sync_all();
    let exit = Exit {
        code: Some(status.exit_code() as i32),
        cancelled: stopping.is_some(),
    };
    crate::storage::write_json(&spec.dir.join(EXIT_FILE), &exit)?;
    // Nobody can attach to a finished harness; the record says how it ended.
    let _ = fs::remove_file(&spec.socket);
    if let Some(mut active) = client.take() {
        let mut raw = Vec::new();
        state.take_output(&mut raw);
        if !raw.is_empty() {
            frame(tag::OUTPUT, &raw, &mut active.out);
        }
        frame(tag::EXIT, &serde_json::to_vec(&exit)?, &mut active.out);
        let deadline = Instant::now() + Duration::from_secs(2);
        while flush(&mut active.stream, &mut active.out, &mut active.written)
            && !active.out.is_empty()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

/// How the harness ended, if its host recorded it.
pub(super) fn recorded(dir: &Path) -> Option<Exit> {
    crate::storage::read_json(&dir.join(EXIT_FILE)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_reads_reassemble() {
        let mut out = Vec::new();
        frame(tag::INPUT, b"hello", &mut out);
        frame(tag::RESIZE, &[0, 80, 0, 24], &mut out);
        let mut frames = Frames::default();
        frames.buffer.extend_from_slice(&out[..3]);
        assert!(frames.next().unwrap().is_none());
        frames.buffer.extend_from_slice(&out[3..]);
        assert_eq!(
            frames.next().unwrap(),
            Some((tag::INPUT, b"hello".to_vec()))
        );
        assert_eq!(
            frames.next().unwrap(),
            Some((tag::RESIZE, vec![0, 80, 0, 24]))
        );
        assert!(frames.next().unwrap().is_none());
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let mut frames = Frames::default();
        frames.buffer.push(tag::OUTPUT);
        frames
            .buffer
            .extend_from_slice(&((FRAME_LIMIT + 1) as u32).to_be_bytes());
        assert!(frames.next().is_err());
    }

    #[test]
    fn host_readiness_reports_errors() {
        let dir = Path::new("/nowhere");
        assert_eq!(ready("ok 42\n", dir).unwrap(), 42);
        assert!(
            ready("error No such program\n", dir)
                .unwrap_err()
                .to_string()
                .contains("No such program")
        );
        assert!(ready("", dir).is_err());
    }
}
