//! Unix process supervision. No command is evaluated by a shell.
use crate::{adapters::Invocation, sandbox::Plan};
use anyhow::{Context, Result, ensure};
use portable_pty::{CommandBuilder, PtySize};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, IsTerminal, Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
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

/// Whether any process, including a zombie, is still in group `pgid`.
pub fn group_alive(pgid: u32) -> bool {
    // SAFETY: signal 0 only checks existence and permission.
    pgid > 1 && unsafe { libc::kill(-(pgid as i32), 0) } == 0
}

/// How long a cancelled or timed-out harness may take to save its session
/// (for takeover) after SIGTERM, before SIGKILL. Also how long leftover
/// background children get after a harness exits.
pub const GRACE: Duration = Duration::from_secs(5);

/// Ends process group `pgid`: SIGTERM, up to `grace` for a clean exit, then
/// SIGKILL for whatever is left. `reap` reaps the leader when it is our
/// child, so its zombie does not count as alive.
pub fn end_group(pgid: u32, grace: Duration, mut reap: impl FnMut()) {
    reap();
    if !group_alive(pgid) {
        return;
    }
    signal_group(pgid, libc::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
        reap();
        if !group_alive(pgid) {
            return;
        }
    }
    kill_group(pgid);
    reap();
}

/// A harness leader and its process group. Background children (dev
/// servers, watchers) must not keep editing the workspace after its lock is
/// released, so dropping this always ends the whole group, after a normal
/// exit too: SIGTERM, `GRACE`, then SIGKILL.
struct ProcessGroup {
    child: Child,
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let child = &mut self.child;
        end_group(child.id(), GRACE, || {
            let _ = child.try_wait();
        });
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Decodes a byte stream as UTF-8 across reads: a character split between
/// two chunks is held back, not replaced.
#[derive(Default)]
pub struct Utf8Stream {
    partial: Vec<u8>,
}
impl Utf8Stream {
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(bytes);
        let mut text = String::with_capacity(data.len());
        let mut rest = &data[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(length) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[length..];
                        }
                        // An incomplete character at the end: wait for the rest.
                        None => {
                            self.partial = after.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        text
    }

    /// Whatever incomplete character the stream ended with.
    pub fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned()
    }
}

fn drain(
    reader: &mut impl Read,
    log: &mut File,
    stderr: bool,
    decoder: &mut Utf8Stream,
    on_output: &mut impl FnMut(bool, &str),
) -> Result<()> {
    let mut bytes = [0u8; 8192];
    // Bound each pass so a noisy process cannot starve cancellation/timeouts.
    for _ in 0..32 {
        match reader.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => {
                log.write_all(&bytes[..n])?;
                let text = decoder.decode(&bytes[..n]);
                if !text.is_empty() {
                    on_output(stderr, &text);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Asks the kernel to SIGTERM the harness when the thread that spawned it
/// dies (Linux only): if mr is killed or OOM-killed, the harness must not
/// keep editing a workspace whose lock died with mr. PR_SET_PDEATHSIG fires
/// when the *forking thread* exits, so spawn from the thread that waits on
/// the child, as `capture` does. Other platforms rely on the attempt's
/// recorded process group being stopped by the next `drive` instead.
fn die_with_parent(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getpid is async-signal-safe.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the hook only makes async-signal-safe calls (prctl,
        // getppid, raise) between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // The parent may have died before prctl took effect.
                if libc::getppid() != parent {
                    libc::raise(libc::SIGTERM);
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

pub fn capture(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
    on_output: impl FnMut(bool, &str),
) -> Result<Outcome> {
    capture_with(inv, cwd, dir, timeout, cancel, None, |_| {}, on_output)
}

/// The program and arguments to launch: `inv`'s, inside `sandbox` if any.
fn launch(inv: &Invocation, sandbox: Option<&Plan>) -> (String, Vec<String>) {
    match sandbox {
        Some(plan) => plan.wrap(&inv.program, &inv.args),
        None => (inv.program.clone(), inv.args.clone()),
    }
}

/// `capture` inside an optional sandbox, reporting the harness's process
/// group ID once it starts, so the caller can record it for crash recovery.
#[allow(clippy::too_many_arguments)]
pub fn capture_with(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
    sandbox: Option<&Plan>,
    on_spawn: impl FnOnce(u32),
    mut on_output: impl FnMut(bool, &str),
) -> Result<Outcome> {
    let mut stdout_log = File::create(dir.join("stdout.log"))?;
    let mut stderr_log = File::create(dir.join("stderr.log"))?;
    if let Some(warning) = sandbox.and_then(|plan| plan.warning.as_deref()) {
        writeln!(stderr_log, "{warning}")?;
        on_output(true, &format!("{warning}\n"));
    }
    let (program, args) = launch(inv, sandbox);
    let mut command = Command::new(&program);
    command
        .args(&args)
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
    for name in inv.removed() {
        command.env_remove(name);
    }
    command.envs(inv.environment()?);
    inv.write_files()?;
    die_with_parent(&mut command);
    let mut child = ProcessGroup {
        child: command
            .spawn()
            .with_context(|| format!("Launch {}", inv.program))?,
    };
    on_spawn(child.child.id());
    let mut stdout = child.child.stdout.take().context("No stdout pipe")?;
    let mut stderr = child.child.stderr.take().context("No stderr pipe")?;
    let mut stdin = child.child.stdin.take();
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    if let Some(input) = &stdin {
        nonblocking(input.as_raw_fd())?;
    }
    let (mut out_text, mut err_text) = (Utf8Stream::default(), Utf8Stream::default());
    let input = inv.stdin.as_deref().unwrap_or("").as_bytes();
    let mut sent = 0;
    let started = Instant::now();
    let mut cancelled = false;
    let mut timed_out = false;
    let status = loop {
        drain(
            &mut stdout,
            &mut stdout_log,
            false,
            &mut out_text,
            &mut on_output,
        )?;
        drain(
            &mut stderr,
            &mut stderr_log,
            true,
            &mut err_text,
            &mut on_output,
        )?;
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
        if let Some(status) = child.child.try_wait()? {
            break status;
        }
        cancelled = cancel.load(Ordering::Relaxed);
        timed_out = started.elapsed() >= timeout;
        if cancelled || timed_out {
            // Ask first: harnesses such as omp only save a session they can
            // resume when they exit cleanly.
            signal_group(child.child.id(), libc::SIGTERM);
            let deadline = Instant::now() + GRACE;
            let status = loop {
                drain(
                    &mut stdout,
                    &mut stdout_log,
                    false,
                    &mut out_text,
                    &mut on_output,
                )?;
                drain(
                    &mut stderr,
                    &mut stderr_log,
                    true,
                    &mut err_text,
                    &mut on_output,
                )?;
                if let Some(status) = child.child.try_wait()? {
                    // Leftover group members never get the grace period twice.
                    kill_group(child.child.id());
                    break status;
                }
                if Instant::now() >= deadline {
                    kill_group(child.child.id());
                    let _ = child.child.kill();
                    break child.child.wait()?;
                }
                thread::sleep(Duration::from_millis(25));
            };
            break status;
        }
        thread::sleep(Duration::from_millis(25));
    };
    // Once the leader exits, drain available bytes without waiting for inherited
    // descriptors held by background grandchildren.
    drain(
        &mut stdout,
        &mut stdout_log,
        false,
        &mut out_text,
        &mut on_output,
    )?;
    drain(
        &mut stderr,
        &mut stderr_log,
        true,
        &mut err_text,
        &mut on_output,
    )?;
    for (stderr, decoder) in [(false, &mut out_text), (true, &mut err_text)] {
        let rest = decoder.finish();
        if !rest.is_empty() {
            on_output(stderr, &rest);
        }
    }
    // Background children (dev servers, watchers) end with the stage.
    drop(child);
    stdout_log.sync_all()?;
    stderr_log.sync_all()?;
    Ok(Outcome {
        code: status.code(),
        cancelled,
        timed_out,
    })
}

// The transcript is on disk; these bounds apply regardless of how long the
// harness runs, whether it is attached, or whether either terminal is blocked.
const INPUT_LIMIT: usize = 64 * 1024;
const OUTPUT_LIMIT: usize = 256 * 1024;
const SEQUENCE_LIMIT: usize = 64 * 1024;
const TERMINAL_RESET: &[u8] = b"\x18\x1b\\\x1b[?2026l\x1b[?1049l\x1b[0m\x1b[r\x1b[?6l\x1b[?7h\x1b[4l\x1b[?25h\x1b[0 q\x1b[?1l\x1b>\x1b[?2004l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1004l\x1b[<65535u\x1b[=0u\x1b[>4;0m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachOutcome {
    Detached,
    Finished,
}

/// A live, in-memory PTY session. The engine supervises it independently of
/// foreground attachment; dropping an attachment does not restart the harness.
#[derive(Clone)]
pub struct InteractiveSession {
    shared: Arc<Mutex<SessionState>>,
}

struct SessionState {
    parser: vt100::Parser,
    primary_screen: Option<vt100::Parser>,
    modes: NativeModes,
    queries: Vec<u8>,
    size: (u16, u16),
    input: VecDeque<u8>,
    output: Vec<u8>,
    attached: bool,
    redraw: bool,
    started: bool,
    finished: bool,
    cancelled: bool,
}

fn bounded_size(cols: u16, rows: u16) -> (u16, u16) {
    // Also bound the parser's cell allocation for bogus terminal dimensions.
    (cols.clamp(1, 500), rows.clamp(1, 300))
}

impl SessionState {
    fn resize(&mut self, cols: u16, rows: u16) {
        let size = bounded_size(cols, rows);
        if self.size != size {
            self.size = size;
            self.parser.set_size(size.1, size.0);
            if let Some(primary) = &mut self.primary_screen {
                primary.set_size(size.1, size.0);
            }
            self.redraw = true;
            self.output.clear();
        }
    }

    fn output(&mut self, bytes: &[u8]) {
        // The decoder delivers escape sequences whole. Save the primary
        // screen before switching so leaving the alternate screen after a
        // reattach reveals the harness, not the monitor.
        if !self.parser.screen().alternate_screen()
            && bytes
                .strip_prefix(b"\x1b[?")
                .and_then(|s| s.strip_suffix(b"h"))
                .is_some_and(|params| {
                    params
                        .split(|b| *b == b';')
                        .any(|p| matches!(p, b"47" | b"1047" | b"1049"))
                })
        {
            let mut primary = vt100::Parser::new(self.size.1, self.size.0, 0);
            primary.process(&self.parser.screen().state_formatted());
            self.primary_screen = Some(primary);
        }
        self.parser.process(bytes);
        self.modes
            .process(bytes, self.parser.screen().alternate_screen());
        // Queries emitted during startup/detachment must reach the real
        // terminal on attach, rather than disappearing into a screen redraw.
        if is_terminal_query(bytes) {
            if self.queries.len() + bytes.len() <= SEQUENCE_LIMIT {
                self.queries.extend_from_slice(bytes);
            }
            return;
        }
        if !self.parser.screen().alternate_screen() {
            self.primary_screen = None;
        }
        if !self.attached || self.redraw {
            return;
        }
        if self.output.len() + bytes.len() > OUTPUT_LIMIT {
            self.output.clear();
            self.redraw = true;
        } else {
            self.output.extend_from_slice(bytes);
        }
    }

    fn take_output(&mut self, output: &mut Vec<u8>) {
        output.clear();
        if self.redraw {
            output.extend_from_slice(TERMINAL_RESET);
            if self.parser.screen().alternate_screen() {
                if let Some(primary) = &self.primary_screen {
                    output.extend_from_slice(&primary.screen().state_formatted());
                    self.modes.primary.restore(output, primary.screen());
                }
                output.extend_from_slice(b"\x1b[?1049h\x1b[?6l\x1b[r\x1b[4l\x1b[?7h");
            }
            output.extend_from_slice(&self.parser.screen().state_formatted());
            let drawing = if self.parser.screen().alternate_screen() {
                &self.modes.alternate
            } else {
                &self.modes.primary
            };
            drawing.restore(output, self.parser.screen());
            self.modes.restore(output);
            self.output.clear();
            self.redraw = false;
        } else {
            std::mem::swap(output, &mut self.output);
        }
        output.append(&mut self.queries);
    }
}

impl InteractiveSession {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = bounded_size(cols, rows);
        Self {
            shared: Arc::new(Mutex::new(SessionState {
                modes: NativeModes::default(),
                queries: Vec::new(),
                parser: vt100::Parser::new(size.1, size.0, 0),
                primary_screen: None,
                size,
                input: VecDeque::new(),
                output: Vec::new(),
                attached: false,
                redraw: true,
                started: false,
                finished: false,
                cancelled: false,
            })),
        }
    }

    fn state(&self) -> MutexGuard<'_, SessionState> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wake an attachment on every engine exit, including errors before spawn.
    /// Calling this for a running supervisor also stops and reaps its child.
    pub fn finish(&self) {
        self.state().finished = true;
    }

    /// The only part of the session allowed to access the host terminal.
    /// Ctrl+B returns to the monitor; Ctrl+] cancels this run, not mr itself.
    pub fn attach(&self, cancel: &AtomicBool, shutdown: &AtomicBool) -> Result<AttachOutcome> {
        {
            let mut state = self.state();
            if shutdown.load(Ordering::Relaxed) {
                cancel.store(true, Ordering::Relaxed);
                state.cancelled = true;
            }
            if state.finished {
                return Ok(AttachOutcome::Finished);
            }
            ensure!(!state.attached, "This harness is already attached");
            state.attached = true;
            state.redraw = true;
            state.output.clear();
        }
        let mut attachment = AttachmentGuard {
            session: self,
            cancel,
            clean: false,
        };
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "Interactive stages require a terminal; reopen in the TUI or set this stage to unattended"
        );
        let _terminal = TerminalGuard::enter()?;
        let mut output = Vec::new();
        let mut written = 0;
        let mut input = [0u8; 8192];
        let mut keys = InputDecoder::default();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                cancel.store(true, Ordering::Relaxed);
            }
            let (finished, capacity, stopping) = {
                let mut state = self.state();
                state.cancelled |= cancel.load(Ordering::Relaxed);
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    state.resize(cols, rows);
                }
                if written == output.len() {
                    state.take_output(&mut output);
                    written = 0;
                }
                (
                    state.finished,
                    INPUT_LIMIT - state.input.len() - keys.pending.len(),
                    state.cancelled,
                )
            };
            if finished && (written == output.len() || stopping) {
                attachment.clean = true;
                return Ok(AttachOutcome::Finished);
            }
            let mut polls = [
                libc::pollfd {
                    fd: 0,
                    events: if !stopping && capacity > 0 {
                        libc::POLLIN
                    } else {
                        0
                    },
                    revents: 0,
                },
                libc::pollfd {
                    fd: 1,
                    events: if written < output.len() {
                        libc::POLLOUT
                    } else {
                        0
                    },
                    revents: 0,
                },
            ];
            // SAFETY: both descriptors are borrowed; the array is initialized.
            let result = unsafe { libc::poll(polls.as_mut_ptr(), 2, 40) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if result == 0 {
                keys.flush(&mut self.state().input);
            }
            ensure!(
                polls
                    .iter()
                    .all(|p| p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0),
                "Interactive terminal disconnected"
            );
            if polls[1].revents & libc::POLLOUT != 0 {
                // Nonblocking writes keep a stalled/disappearing terminal from
                // preventing cancellation or blocking the PTY output reader.
                let n = unsafe {
                    libc::write(1, output[written..].as_ptr().cast(), output.len() - written)
                };
                if n > 0 {
                    written += n as usize;
                } else if n < 0 {
                    let error = io::Error::last_os_error();
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) {
                        return Err(error.into());
                    }
                } else {
                    anyhow::bail!("Interactive terminal stopped accepting output");
                }
            }
            if polls[0].revents & libc::POLLIN != 0 {
                let n =
                    unsafe { libc::read(0, input.as_mut_ptr().cast(), capacity.min(input.len())) };
                if n == 0 {
                    anyhow::bail!("Interactive terminal disconnected");
                }
                if n < 0 {
                    let error = io::Error::last_os_error();
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) {
                        continue;
                    }
                    return Err(error.into());
                }
                let mut state = self.state();
                match keys.forward(&input[..n as usize], &mut state.input) {
                    Some(AttachOutcome::Detached) => {
                        keys.flush(&mut state.input);
                        attachment.clean = true;
                        return Ok(AttachOutcome::Detached);
                    }
                    Some(AttachOutcome::Finished) => {
                        cancel.store(true, Ordering::Relaxed);
                        state.cancelled = true;
                    }
                    None => {}
                }
            }
        }
    }
}

struct AttachmentGuard<'a> {
    session: &'a InteractiveSession,
    cancel: &'a AtomicBool,
    clean: bool,
}

impl Drop for AttachmentGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.session.state();
        state.attached = false;
        state.output.clear();
        if !self.clean {
            state.cancelled = true;
            self.cancel.store(true, Ordering::Relaxed);
        }
    }
}

struct TerminalGuard {
    was_raw: bool,
    stdout_flags: i32,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        let was_raw = crossterm::terminal::is_raw_mode_enabled()?;
        // SAFETY: stdout is a borrowed live terminal descriptor.
        let stdout_flags = unsafe { libc::fcntl(1, libc::F_GETFL) };
        ensure!(stdout_flags >= 0, "Cannot inspect terminal output flags");
        let guard = Self {
            was_raw,
            stdout_flags,
        };
        crossterm::terminal::enable_raw_mode()?;
        nonblocking(1)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best effort, bounded restoration even when the terminal is gone or
        // flow-controlled. Always restore termios and file flags afterwards.
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut written = 0;
        while written < TERMINAL_RESET.len() && Instant::now() < deadline {
            let mut poll = libc::pollfd {
                fd: 1,
                events: libc::POLLOUT,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 10) };
            if ready > 0 && poll.revents & libc::POLLOUT != 0 {
                let n = unsafe {
                    libc::write(
                        1,
                        TERMINAL_RESET[written..].as_ptr().cast(),
                        TERMINAL_RESET.len() - written,
                    )
                };
                if n > 0 {
                    written += n as usize;
                } else {
                    break;
                }
            } else if ready != 0 {
                break;
            }
        }
        unsafe {
            libc::fcntl(1, libc::F_SETFL, self.stdout_flags);
        }
        if !self.was_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

#[derive(Default)]
struct InputDecoder {
    paste: bool,
    matched: usize,
    pending: Vec<u8>,
}

impl InputDecoder {
    fn flush(&mut self, input: &mut VecDeque<u8>) {
        input.extend(self.pending.drain(..));
    }

    fn forward(&mut self, bytes: &[u8], input: &mut VecDeque<u8>) -> Option<AttachOutcome> {
        let mut outcome = None;
        for &byte in bytes {
            if self.paste {
                input.push_back(byte);
                let marker = b"\x1b[201~";
                if byte == marker[self.matched] {
                    self.matched += 1;
                    if self.matched == marker.len() {
                        self.paste = false;
                        self.matched = 0;
                    }
                } else {
                    self.matched = usize::from(byte == 0x1b);
                }
                continue;
            }
            let control = match byte {
                0x02 => Some(AttachOutcome::Detached),
                0x1d => Some(AttachOutcome::Finished),
                _ => None,
            };
            if let Some(control) = control {
                self.flush(input);
                if outcome != Some(AttachOutcome::Finished) {
                    outcome = Some(control);
                }
            } else if !self.pending.is_empty() || byte == 0x1b {
                self.pending.push(byte);
                let complete = (self.pending.len() == 2 && byte != b'[')
                    || (self.pending.len() > 2 && (0x40..=0x7e).contains(&byte))
                    || self.pending.len() == 64;
                if complete {
                    if self.pending == b"\x1b[200~" {
                        self.paste = true;
                    }
                    if let Some(control) = enhanced_control(&self.pending) {
                        self.pending.clear();
                        if outcome != Some(AttachOutcome::Finished) {
                            outcome = Some(control);
                        }
                    } else {
                        self.flush(input);
                    }
                }
            } else {
                input.push_back(byte);
            }
        }
        outcome
    }
}

fn enhanced_control(bytes: &[u8]) -> Option<AttachOutcome> {
    let body = bytes.strip_prefix(b"\x1b[")?;
    let (code, modifiers) = if let Some(body) = body.strip_suffix(b"u") {
        let mut params = body.split(|b| *b == b';');
        let code = params.next()?.split(|b| *b == b':').next()?;
        let modifier = params.next()?.split(|b| *b == b':').next()?;
        (code, modifier)
    } else {
        let body = body.strip_prefix(b"27;")?.strip_suffix(b"~")?;
        let mut params = body.split(|b| *b == b';');
        let modifier = params.next()?;
        (params.next()?, modifier)
    };
    if modifiers != b"5" {
        return None;
    }
    match code {
        b"98" => Some(AttachOutcome::Detached),
        b"93" => Some(AttachOutcome::Finished),
        _ => None,
    }
}

fn is_terminal_query(bytes: &[u8]) -> bool {
    if let Some(csi) = bytes.strip_prefix(b"\x1b[") {
        return matches!(csi.last(), Some(b'c' | b'n' | b't'))
            || csi == b"?u"
            || csi.ends_with(b"$p");
    }
    (bytes.starts_with(b"\x1b]") && bytes.contains(&b'?')) || bytes.starts_with(b"\x1bP$q")
}

// These native modes are not represented by vt100's screen serializer.
#[derive(Default)]
struct NativeModes {
    focus: bool,
    cursor_style: u16,
    modify_other_keys: u16,
    keyboard: u16,
    keyboard_stack: [u16; 16],
    keyboard_depth: usize,
    primary: DrawingModes,
    alternate: DrawingModes,
}

fn number(bytes: &[u8], default: u16) -> u16 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

impl NativeModes {
    fn process(&mut self, bytes: &[u8], alternate: bool) {
        if bytes == b"\x1bc" {
            *self = Self::default();
        }
        let Some(csi) = bytes.strip_prefix(b"\x1b[") else {
            return;
        };
        let drawing = if alternate {
            &mut self.alternate
        } else {
            &mut self.primary
        };
        drawing.process(csi);
        if let Some(params) = csi.strip_prefix(b"?") {
            if matches!(params.last(), Some(b'h' | b'l'))
                && params[..params.len() - 1]
                    .split(|b| *b == b';')
                    .any(|p| p == b"1004")
            {
                self.focus = params.last() == Some(&b'h');
            }
        }
        if let Some(style) = csi.strip_suffix(b" q") {
            self.cursor_style = number(style, 0).min(6);
        }
        if let Some(level) = csi.strip_prefix(b">4;").and_then(|s| s.strip_suffix(b"m")) {
            self.modify_other_keys = number(level, 0).min(2);
        }
        if let Some(params) = csi.strip_suffix(b"u") {
            if let Some(flags) = params.strip_prefix(b">") {
                if self.keyboard_depth == self.keyboard_stack.len() {
                    self.keyboard_stack.rotate_left(1);
                    self.keyboard_depth -= 1;
                }
                self.keyboard_stack[self.keyboard_depth] = self.keyboard;
                self.keyboard_depth += 1;
                self.keyboard = number(flags, 0);
            } else if let Some(count) = params.strip_prefix(b"<") {
                for _ in 0..usize::from(number(count, 1)).min(self.keyboard_depth) {
                    self.keyboard_depth -= 1;
                    self.keyboard = self.keyboard_stack[self.keyboard_depth];
                }
            } else if let Some(flags) = params.strip_prefix(b"=") {
                let mut params = flags.split(|b| *b == b';');
                let flags = number(params.next().unwrap_or_default(), 0);
                match number(params.next().unwrap_or_default(), 1) {
                    1 => self.keyboard = flags,
                    2 => self.keyboard |= flags,
                    3 => self.keyboard &= !flags,
                    _ => {}
                }
            }
        }
    }

    fn restore(&self, output: &mut Vec<u8>) {
        if self.focus {
            output.extend_from_slice(b"\x1b[?1004h");
        }
        let _ = write!(
            output,
            "\x1b[{} q\x1b[>4;{}m",
            self.cursor_style, self.modify_other_keys
        );
        if self.keyboard_depth > 0 {
            let _ = write!(output, "\x1b[={}u", self.keyboard_stack[0]);
            for flags in &self.keyboard_stack[1..self.keyboard_depth] {
                let _ = write!(output, "\x1b[>{flags}u");
            }
            let _ = write!(output, "\x1b[>{}u", self.keyboard);
        } else {
            let _ = write!(output, "\x1b[={}u", self.keyboard);
        }
    }
}

#[derive(Default)]
struct DrawingModes {
    top: u16,
    bottom: u16,
    origin: bool,
    no_wrap: bool,
    insert: bool,
}

impl DrawingModes {
    fn process(&mut self, csi: &[u8]) {
        if let Some(params) = csi.strip_suffix(b"r") {
            let mut params = params.split(|b| *b == b';');
            self.top = number(params.next().unwrap_or_default(), 1).max(1);
            self.bottom = number(params.next().unwrap_or_default(), 0);
        }
        if let Some(params) = csi.strip_prefix(b"?") {
            if matches!(params.last(), Some(b'h' | b'l')) {
                let enabled = params.last() == Some(&b'h');
                for param in params[..params.len() - 1].split(|b| *b == b';') {
                    match param {
                        b"6" => self.origin = enabled,
                        b"7" => self.no_wrap = !enabled,
                        _ => {}
                    }
                }
            }
        }
        match csi {
            b"4h" => self.insert = true,
            b"4l" => self.insert = false,
            _ => {}
        }
    }

    fn restore(&self, output: &mut Vec<u8>, screen: &vt100::Screen) {
        let (rows, _) = screen.size();
        let top = self.top.max(1).min(rows);
        let bottom = if self.bottom == 0 {
            rows
        } else {
            self.bottom.min(rows)
        };
        if top < bottom && (top != 1 || bottom != rows) {
            let _ = write!(output, "\x1b[{top};{bottom}r");
            output.extend_from_slice(&screen.cursor_state_formatted());
            output.extend_from_slice(&screen.attributes_formatted());
        }
        if self.origin {
            let (row, col) = screen.cursor_position();
            let row = row.saturating_sub(top - 1) + 1;
            let _ = write!(output, "\x1b[?6h\x1b[{row};{}H", col + 1);
        }
        if self.no_wrap {
            output.extend_from_slice(b"\x1b[?7l");
        }
        if self.insert {
            output.extend_from_slice(b"\x1b[4h");
        }
    }
}

#[derive(Clone, Copy, Default)]
enum DecodeState {
    #[default]
    Ground,
    Escape,
    Csi,
    String {
        osc: bool,
        escape: bool,
    },
    Utf8(u8),
}

/// Only publish complete terminal sequences/codepoints, so a redraw cannot
/// splice into an escape or UTF-8 character. Oversized control strings are
/// discarded from the display model, but always remain in the disk transcript.
#[derive(Default)]
struct OutputDecoder {
    state: DecodeState,
    pending: Vec<u8>,
    oversized: bool,
}

impl OutputDecoder {
    fn process(&mut self, mut bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        while !bytes.is_empty() {
            if matches!(self.state, DecodeState::Utf8(_)) && !(0x80..=0xbf).contains(&bytes[0]) {
                emit(b"\xef\xbf\xbd");
                self.pending.clear();
                self.state = DecodeState::Ground;
                continue;
            }
            if matches!(self.state, DecodeState::Ground) {
                let plain = bytes
                    .iter()
                    .position(|b| *b == 0x1b || *b >= 0x80)
                    .unwrap_or(bytes.len());
                if plain > 0 {
                    emit(&bytes[..plain]);
                    bytes = &bytes[plain..];
                    continue;
                }
            }
            let byte = bytes[0];
            bytes = &bytes[1..];
            if self.pending.len() < SEQUENCE_LIMIT && !self.oversized {
                self.pending.push(byte);
            } else {
                self.oversized = true;
                self.pending.clear();
            }
            self.state = match self.state {
                DecodeState::Ground => match byte {
                    0x1b => DecodeState::Escape,
                    0xc2..=0xdf => DecodeState::Utf8(1),
                    0xe0..=0xef => DecodeState::Utf8(2),
                    0xf0..=0xf4 => DecodeState::Utf8(3),
                    _ => DecodeState::Ground,
                },
                DecodeState::Escape => match byte {
                    b'[' => DecodeState::Csi,
                    b']' => DecodeState::String {
                        osc: true,
                        escape: false,
                    },
                    b'P' | b'X' | b'^' | b'_' => DecodeState::String {
                        osc: false,
                        escape: false,
                    },
                    0x20..=0x2f | 0x1b => DecodeState::Escape,
                    _ => DecodeState::Ground,
                },
                DecodeState::Csi => match byte {
                    0x40..=0x7e | 0x18 | 0x1a => DecodeState::Ground,
                    0x1b => DecodeState::Escape,
                    _ => DecodeState::Csi,
                },
                DecodeState::String { osc, escape } => {
                    if (escape && byte == b'\\')
                        || (osc && byte == 0x07)
                        || matches!(byte, 0x18 | 0x1a)
                    {
                        DecodeState::Ground
                    } else {
                        DecodeState::String {
                            osc,
                            escape: byte == 0x1b,
                        }
                    }
                }
                DecodeState::Utf8(remaining) => {
                    if remaining == 1 || !(0x80..=0xbf).contains(&byte) {
                        DecodeState::Ground
                    } else {
                        DecodeState::Utf8(remaining - 1)
                    }
                }
            };
            if matches!(self.state, DecodeState::Ground) {
                if !self.oversized {
                    emit(&self.pending);
                }
                self.pending.clear();
                self.oversized = false;
            }
        }
    }
}

struct SessionFinish<'a>(&'a InteractiveSession);

impl Drop for SessionFinish<'_> {
    fn drop(&mut self) {
        self.0.finish();
    }
}
/// A PTY harness and its session's process group, ended on drop like
/// `ProcessGroup`. portable-pty makes the harness a session leader with the
/// PTY as its controlling terminal, so if mr dies, closing the master sends
/// it SIGHUP; PR_SET_PDEATHSIG is not available through portable-pty.
struct PtyChild {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pid: Option<u32>,
}
impl Drop for PtyChild {
    fn drop(&mut self) {
        let child = &mut self.child;
        if let Some(pid) = self.pid {
            end_group(pid, GRACE, || {
                let _ = child.try_wait();
            });
        }
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

/// Supervise a PTY without accessing the host terminal. Output keeps draining
/// and being logged while detached; the foreground alone calls `attach`.
pub fn interactive(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    cancel: &AtomicBool,
    session: &InteractiveSession,
    sandbox: Option<&Plan>,
    on_spawn: impl FnOnce(u32),
) -> Result<Outcome> {
    let mut size = {
        let mut state = session.state();
        ensure!(
            !state.started && !state.finished,
            "Interactive session has already ended or started"
        );
        state.started = true;
        if cancel.load(Ordering::Relaxed) || state.cancelled {
            state.finished = true;
            return Ok(Outcome {
                code: None,
                cancelled: true,
                timed_out: false,
            });
        }
        state.size
    };
    let _finish = SessionFinish(session);
    let pair = portable_pty::native_pty_system().openpty(PtySize {
        rows: size.1,
        cols: size.0,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let (program, args) = launch(inv, sandbox);
    let mut cmd = CommandBuilder::new(&program);
    cmd.args(&args);
    for name in inv.removed() {
        cmd.env_remove(name);
    }
    for (name, value) in inv.environment()? {
        cmd.env(name, value);
    }
    inv.write_files()?;
    cmd.cwd(cwd);
    let mut transcript = File::create(dir.join("stdout.log"))?;
    let mut stderr_log = File::create(dir.join("stderr.log"))?;
    if let Some(warning) = sandbox.and_then(|plan| plan.warning.as_deref()) {
        writeln!(stderr_log, "{warning}")?;
        session.state().output(format!("{warning}\r\n").as_bytes());
    }
    let fd = pair
        .master
        .as_raw_fd()
        .context("PTY has no file descriptor")?;
    nonblocking(fd)?;
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let spawned = pair.slave.spawn_command(cmd)?;
    let pid = spawned.process_id();
    let mut child = PtyChild {
        child: spawned,
        pid,
    };
    if let Some(pid) = pid {
        on_spawn(pid);
    }
    drop(pair.slave);
    let mut decoder = OutputDecoder::default();
    let mut bytes = [0u8; 8192];
    let mut cancelled = false;
    let mut eof = false;
    let status = loop {
        {
            let mut state = session.state();
            cancelled |= cancel.load(Ordering::Relaxed) || state.cancelled || state.finished;
            if cancelled {
                cancel.store(true, Ordering::Relaxed);
                break None;
            }
            if state.size != size {
                size = state.size;
                pair.master.resize(PtySize {
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
                    Err(e)
                        if e.kind() == io::ErrorKind::BrokenPipe
                            || e.raw_os_error() == Some(libc::EIO) =>
                    {
                        state.input.clear();
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        drain_pty(
            &mut reader,
            &mut transcript,
            &mut decoder,
            session,
            &mut bytes,
            &mut eof,
        )?;
        if let Some(status) = child.child.try_wait()? {
            break Some(status);
        }
        if eof {
            thread::sleep(Duration::from_millis(40));
        } else {
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut poll, 1, 40) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            }
        }
    };
    let status = match status {
        Some(status) => status,
        // Ask first, as for unattended stages: omp only saves a session it
        // can resume when it exits cleanly. Keep draining so a harness
        // blocked on a full PTY can still exit.
        None => {
            if let Some(pid) = pid {
                signal_group(pid, libc::SIGTERM);
            }
            let deadline = Instant::now() + GRACE;
            loop {
                drain_pty(
                    &mut reader,
                    &mut transcript,
                    &mut decoder,
                    session,
                    &mut bytes,
                    &mut eof,
                )?;
                if let Some(status) = child.child.try_wait()? {
                    if let Some(pid) = pid {
                        kill_group(pid);
                    }
                    break status;
                }
                if Instant::now() >= deadline {
                    if let Some(pid) = pid {
                        kill_group(pid);
                    }
                    let _ = child.child.kill();
                    break child.child.wait()?;
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
    };
    drain_pty(
        &mut reader,
        &mut transcript,
        &mut decoder,
        session,
        &mut bytes,
        &mut eof,
    )?;
    // No descendant is allowed to outlive mr's owned harness session.
    drop(child);
    session.finish();
    transcript.sync_all()?;
    Ok(Outcome {
        code: Some(status.exit_code() as i32),
        cancelled,
        timed_out: false,
    })
}

fn drain_pty(
    reader: &mut impl Read,
    transcript: &mut File,
    decoder: &mut OutputDecoder,
    session: &InteractiveSession,
    bytes: &mut [u8; 8192],
    eof: &mut bool,
) -> Result<()> {
    // Bound each pass, not the lifetime stream, so floods cannot starve cancel.
    for _ in 0..32 {
        match reader.read(bytes) {
            Ok(0) => {
                *eof = true;
                break;
            }
            Ok(n) => {
                transcript.write_all(&bytes[..n])?;
                let mut state = session.state();
                decoder.process(&bytes[..n], |frame| state.output(frame));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                *eof = true;
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_preserves_surrounding_input_and_paste_controls() {
        let mut decoder = InputDecoder::default();
        let mut input = VecDeque::new();
        assert_eq!(decoder.forward(b"before\x1b[20", &mut input), None);
        assert_eq!(
            decoder.forward(b"0~paste\x02\x1d\x1b[201", &mut input),
            None
        );
        assert_eq!(
            decoder.forward(b"~after\x02suffix", &mut input),
            Some(AttachOutcome::Detached)
        );
        assert_eq!(
            input.into_iter().collect::<Vec<_>>(),
            b"before\x1b[200~paste\x02\x1d\x1b[201~aftersuffix"
        );
    }

    #[test]
    fn enhanced_controls_handle_split_sequences_without_losing_text() {
        let mut decoder = InputDecoder::default();
        let mut input = VecDeque::new();
        assert_eq!(decoder.forward(b"a\x1b[98;", &mut input), None);
        assert_eq!(
            decoder.forward(b"5ub", &mut input),
            Some(AttachOutcome::Detached)
        );
        assert_eq!(
            decoder.forward(b"\x1b[27;5;93~c", &mut input),
            Some(AttachOutcome::Finished)
        );
        assert_eq!(input.into_iter().collect::<Vec<_>>(), b"abc");
    }

    #[test]
    fn split_output_never_exposes_partial_escape_or_unicode() {
        let mut decoder = OutputDecoder::default();
        let mut output = Vec::new();
        decoder.process(b"hello\x1b[?20", |bytes| output.extend_from_slice(bytes));
        assert_eq!(output, b"hello");
        decoder.process(b"04h\xe7", |bytes| output.extend_from_slice(bytes));
        assert_eq!(output, b"hello\x1b[?2004h");
        decoder.process(b"\x95\x8c", |bytes| output.extend_from_slice(bytes));
        assert_eq!(output, "hello\x1b[?2004h界".as_bytes());
    }

    #[test]
    fn utf8_split_across_reads_is_decoded_whole() {
        let mut stream = Utf8Stream::default();
        let text = "雪 ok".as_bytes();
        assert_eq!(stream.decode(&text[..1]), "");
        assert_eq!(stream.decode(&text[1..2]), "");
        assert_eq!(stream.decode(&text[2..]), "雪 ok");
        // Invalid bytes are still replaced, and a dangling prefix is flushed.
        assert_eq!(stream.decode(b"a\xffb\xe9"), "a\u{fffd}b");
        assert_eq!(stream.finish(), "\u{fffd}");
    }

    #[test]
    fn cancelling_an_interactive_harness_lets_it_exit_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let inv = Invocation {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "trap 'echo saved > saved; exit 0' TERM; echo ready > ready; while :; do sleep 0.05; done".into(),
            ],
            stdin: None,
            kind: crate::adapters::OutputKind::Text,
            final_file: dir.path().join("unused"),
            env: vec![],
            files: vec![],
            assign: None,
        };
        let session = InteractiveSession::new(80, 24);
        let cancel = AtomicBool::new(false);
        let ready = dir.path().join("ready");
        let outcome = thread::scope(|scope| {
            scope.spawn(|| {
                let start = Instant::now();
                while !ready.exists() && start.elapsed() < Duration::from_secs(10) {
                    thread::sleep(Duration::from_millis(20));
                }
                cancel.store(true, Ordering::Relaxed);
            });
            interactive(
                &inv,
                dir.path(),
                dir.path(),
                &cancel,
                &session,
                None,
                |_| {},
            )
            .unwrap()
        });
        assert!(outcome.cancelled);
        assert!(
            dir.path().join("saved").exists(),
            "SIGKILL before a clean exit"
        );
    }

    #[test]
    fn oversized_control_strings_do_not_grow_or_poison_later_output() {
        let mut decoder = OutputDecoder::default();
        let mut output = Vec::new();
        decoder.process(b"\x1b]52;c;", |bytes| output.extend_from_slice(bytes));
        let chunk = [b'x'; 8192];
        for _ in 0..32 {
            decoder.process(&chunk, |bytes| output.extend_from_slice(bytes));
            assert!(decoder.pending.len() <= SEQUENCE_LIMIT);
        }
        decoder.process(b"\x07visible", |bytes| output.extend_from_slice(bytes));
        assert_eq!(output, b"visible");
    }

    #[test]
    fn redraw_restores_alternate_primary_cursor_and_paste_after_resize() {
        let session = InteractiveSession::new(20, 6);
        let mut state = session.state();
        let mut decoder = OutputDecoder::default();
        decoder.process(
            b"primary\x1b[?1049h\x1b[2J\x1b[Halternate\x1b[?25l\x1b[?2004h",
            |bytes| state.output(bytes),
        );
        state.resize(24, 8);
        let mut replay = Vec::new();
        state.take_output(&mut replay);
        let mut restored = vt100::Parser::new(8, 24, 0);
        restored.process(&replay);
        assert!(restored.screen().alternate_screen());
        assert!(restored.screen().hide_cursor());
        assert!(restored.screen().bracketed_paste());
        assert_eq!(
            restored.screen().contents(),
            state.parser.screen().contents()
        );
        assert_eq!(
            restored.screen().cursor_position(),
            state.parser.screen().cursor_position()
        );
        restored.process(b"\x1b[?1049l");
        assert_eq!(restored.screen().contents(), "primary");
    }

    #[test]
    fn output_overflow_redraws_current_screen_instead_of_dropping_a_suffix() {
        let session = InteractiveSession::new(20, 6);
        let mut state = session.state();
        state.attached = true;
        state.redraw = false;
        let chunk = [b'x'; 8192];
        for _ in 0..40 {
            state.output(&chunk);
            assert!(state.output.len() <= OUTPUT_LIMIT);
        }
        assert!(state.redraw);
        state.output(b"\x1b[2J");
        state.output(b"\x1b[H");
        state.output(b"latest");
        let mut replay = Vec::new();
        state.take_output(&mut replay);
        let mut restored = vt100::Parser::new(6, 20, 0);
        restored.process(&replay);
        assert_eq!(restored.screen().contents(), "latest");
    }

    #[test]
    fn native_keyboard_stack_and_focus_survive_reset_and_replay() {
        let mut modes = NativeModes::default();
        let mut decoder = OutputDecoder::default();
        decoder.process(b"\x1b[?1004h\x1b[5 q\x1b[>1u\x1b[>3u", |bytes| {
            modes.process(bytes, false)
        });
        let mut replay = Vec::new();
        modes.restore(&mut replay);
        let mut restored = NativeModes::default();
        decoder.process(&replay, |bytes| restored.process(bytes, false));
        assert!(restored.focus);
        assert_eq!(restored.cursor_style, 5);
        assert_eq!(restored.keyboard, 3);
        restored.process(b"\x1b[<u", false);
        assert_eq!(restored.keyboard, 1);
        restored.process(b"\x1b[<u", false);
        assert_eq!(restored.keyboard, 0);
    }

    #[test]
    fn detached_terminal_queries_are_replayed_once_on_attach() {
        let session = InteractiveSession::new(20, 6);
        let mut state = session.state();
        let mut decoder = OutputDecoder::default();
        decoder.process(b"ready\x1b[6n\x1b[?u", |bytes| state.output(bytes));
        let mut replay = Vec::new();
        state.take_output(&mut replay);
        assert!(replay.ends_with(b"\x1b[6n\x1b[?u"));
        state.take_output(&mut replay);
        assert!(replay.is_empty());
    }

    #[test]
    fn redraw_preserves_scroll_region_for_subsequent_output() {
        let session = InteractiveSession::new(20, 6);
        let mut state = session.state();
        let mut decoder = OutputDecoder::default();
        decoder.process(
            b"\x1b[Hheader\x1b[6;1Hfooter\x1b[2;5r\x1b[5;1Hlast",
            |bytes| state.output(bytes),
        );
        let mut replay = Vec::new();
        state.take_output(&mut replay);
        let mut restored = vt100::Parser::new(6, 20, 0);
        restored.process(&replay);
        let next = b"\r\nnext";
        restored.process(next);
        decoder.process(next, |bytes| state.output(bytes));
        assert_eq!(
            restored.screen().contents(),
            state.parser.screen().contents()
        );
        assert!(restored.screen().contents().contains("header"));
        assert!(restored.screen().contents().contains("footer"));
    }
}
