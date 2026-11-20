use super::*;

// The transcript is on disk; these bounds apply regardless of how long the
// harness runs, whether it is attached, or whether either terminal is blocked.
pub(super) const INPUT_LIMIT: usize = 64 * 1024;
pub(super) const OUTPUT_LIMIT: usize = 256 * 1024;
pub(super) const SEQUENCE_LIMIT: usize = 64 * 1024;
pub(super) const TERMINAL_RESET: &[u8] = b"\x18\x1b\\\x1b[?2026l\x1b[?1049l\x1b[0m\x1b[r\x1b[?6l\x1b[?7h\x1b[4l\x1b[?25h\x1b[0 q\x1b[?1l\x1b>\x1b[?2004l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1004l\x1b[<65535u\x1b[=0u\x1b[>4;0m";

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
    /// Wakes the supervisor for queued input and flags, and the attachment
    /// for new output, instead of waiting out their poll timeouts.
    pub(super) wakes: Arc<(Wake, Wake)>,
}

/// Wakes a thread blocked in poll(2) on `fd`; wakes before it looks
/// coalesce. Without a socket pair (descriptors exhausted), pollers fall
/// back to their timeouts.
pub(super) struct Wake(Option<(UnixStream, UnixStream)>);
impl Wake {
    pub(super) fn new() -> Self {
        Self(
            UnixStream::pair()
                .and_then(|(rx, tx)| {
                    rx.set_nonblocking(true)?;
                    tx.set_nonblocking(true)?;
                    Ok((rx, tx))
                })
                .ok(),
        )
    }
    pub(super) fn wake(&self) {
        if let Some((_, tx)) = &self.0 {
            let _ = (&*tx).write(&[0]);
        }
    }
    /// Takes pending wakes; call before checking what they announce.
    pub(super) fn clear(&self) {
        if let Some((rx, _)) = &self.0 {
            let mut bytes = [0u8; 64];
            while matches!((&*rx).read(&mut bytes), Ok(n) if n > 0) {}
        }
    }
    pub(super) fn fd(&self) -> i32 {
        self.0.as_ref().map_or(-1, |(rx, _)| rx.as_raw_fd())
    }
}

pub(super) struct SessionState {
    pub(super) parser: vt100::Parser,
    pub(super) primary_screen: Option<vt100::Parser>,
    pub(super) modes: NativeModes,
    pub(super) queries: Vec<u8>,
    pub(super) size: (u16, u16),
    pub(super) input: VecDeque<u8>,
    pub(super) output: Vec<u8>,
    pub(super) attached: bool,
    pub(super) redraw: bool,
    pub(super) started: bool,
    pub(super) finished: bool,
    pub(super) cancelled: bool,
    /// Leave the harness running in its host when this mr lets go.
    pub(super) released: bool,
    /// The released connection, kept open until the caller saved the run
    /// and hands the locks over with `InteractiveSession::hand_over`.
    pub(super) parked: Option<Parked>,
}

/// A released session's connection to its host and the frames it still owes.
pub(super) struct Parked {
    pub(super) terminal: Terminal,
    pub(super) pending: Vec<u8>,
}

pub(super) fn bounded_size(cols: u16, rows: u16) -> (u16, u16) {
    // Also bound the parser's cell allocation for bogus terminal dimensions.
    (cols.clamp(1, 500), rows.clamp(1, 300))
}

impl SessionState {
    pub(super) fn new(cols: u16, rows: u16) -> Self {
        let size = bounded_size(cols, rows);
        Self {
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
            released: false,
            parked: None,
        }
    }

    pub(super) fn resize(&mut self, cols: u16, rows: u16) {
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

    pub(super) fn output(&mut self, bytes: &[u8]) {
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

    pub(super) fn take_output(&mut self, output: &mut Vec<u8>) {
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
        Self {
            shared: Arc::new(Mutex::new(SessionState::new(cols, rows))),
            wakes: Arc::new((Wake::new(), Wake::new())),
        }
    }

    pub(super) fn wake_supervisor(&self) {
        self.wakes.0.wake();
    }

    pub(super) fn wake_attachment(&self) {
        self.wakes.1.wake();
    }

    pub(super) fn state(&self) -> MutexGuard<'_, SessionState> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wake an attachment on every engine exit, including errors before spawn.
    /// Calling this for a running supervisor also stops and reaps its child.
    pub fn finish(&self) {
        self.state().finished = true;
        self.wake_supervisor();
        self.wake_attachment();
    }

    /// Lets go of a running harness without stopping it: its terminal host
    /// keeps it, and a later mr reattaches. No effect once it has ended.
    pub fn release(&self) {
        let mut state = self.state();
        if state.started && !state.finished {
            state.released = true;
            drop(state);
            self.wake_supervisor();
        }
    }

    /// Closes a released session's connection to its terminal host, handing
    /// `held` (the run's locks, in the host's order) over with it so they are
    /// never free. Call after the last save of the run: the host may give the
    /// locks to another mr at once. Does nothing for a session not released.
    pub fn hand_over(&self, held: &[&FileLock]) {
        let parked = self.state().parked.take();
        if let Some(parked) = parked {
            parked.terminal.hand_over(&parked.pending, held);
        }
    }

    /// Whether `release` let go of a running harness.
    pub fn released(&self) -> bool {
        self.state().released
    }

    /// Whether a harness runs in this session now.
    pub fn live(&self) -> bool {
        let state = self.state();
        state.started && !state.finished
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
            self.wakes.1.clear();
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
                libc::pollfd {
                    fd: self.wakes.1.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: the descriptors are borrowed; the array is initialized.
            let result = unsafe { libc::poll(polls.as_mut_ptr(), 3, 40) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if result == 0 {
                keys.flush(&mut self.state().input);
                self.wake_supervisor();
            }
            ensure!(
                polls[..2]
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
                // After queueing: the supervisor clears wakes before it looks.
                self.wake_supervisor();
            }
        }
    }
}

pub(super) struct AttachmentGuard<'a> {
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

pub(super) struct TerminalGuard {
    was_raw: bool,
    stdout_flags: i32,
}

impl TerminalGuard {
    pub(super) fn enter() -> Result<Self> {
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
pub(super) struct InputDecoder {
    paste: bool,
    matched: usize,
    pending: Vec<u8>,
}

impl InputDecoder {
    pub(super) fn flush(&mut self, input: &mut VecDeque<u8>) {
        input.extend(self.pending.drain(..));
    }

    pub(super) fn forward(
        &mut self,
        bytes: &[u8],
        input: &mut VecDeque<u8>,
    ) -> Option<AttachOutcome> {
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

pub(super) fn enhanced_control(bytes: &[u8]) -> Option<AttachOutcome> {
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

pub(super) fn is_terminal_query(bytes: &[u8]) -> bool {
    if let Some(csi) = bytes.strip_prefix(b"\x1b[") {
        return matches!(csi.last(), Some(b'c' | b'n' | b't'))
            || csi == b"?u"
            || csi.ends_with(b"$p");
    }
    (bytes.starts_with(b"\x1b]") && bytes.contains(&b'?')) || bytes.starts_with(b"\x1bP$q")
}

// These native modes are not represented by vt100's screen serializer.
#[derive(Default)]
pub(super) struct NativeModes {
    pub(super) focus: bool,
    pub(super) cursor_style: u16,
    pub(super) modify_other_keys: u16,
    pub(super) keyboard: u16,
    pub(super) keyboard_stack: [u16; 16],
    pub(super) keyboard_depth: usize,
    pub(super) primary: DrawingModes,
    pub(super) alternate: DrawingModes,
}

pub(super) fn number(bytes: &[u8], default: u16) -> u16 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

impl NativeModes {
    pub(super) fn process(&mut self, bytes: &[u8], alternate: bool) {
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

    pub(super) fn restore(&self, output: &mut Vec<u8>) {
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
pub(super) struct DrawingModes {
    top: u16,
    bottom: u16,
    origin: bool,
    no_wrap: bool,
    insert: bool,
}

impl DrawingModes {
    pub(super) fn process(&mut self, csi: &[u8]) {
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

    pub(super) fn restore(&self, output: &mut Vec<u8>, screen: &vt100::Screen) {
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
pub(super) enum DecodeState {
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
pub(super) struct OutputDecoder {
    pub(super) state: DecodeState,
    pub(super) pending: Vec<u8>,
    pub(super) oversized: bool,
}

impl OutputDecoder {
    pub(super) fn process(&mut self, mut bytes: &[u8], mut emit: impl FnMut(&[u8])) {
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

pub(super) struct SessionFinish<'a>(pub(super) &'a InteractiveSession);

impl Drop for SessionFinish<'_> {
    fn drop(&mut self) {
        self.0.finish();
    }
}
/// A PTY harness and its session's process group, ended on drop like
/// `ProcessGroup`. portable-pty makes the harness a session leader with the
/// PTY as its controlling terminal, so if mr dies, closing the master sends
/// it SIGHUP; PR_SET_PDEATHSIG is not available through portable-pty.
pub(super) struct PtyChild {
    pub(super) child: Box<dyn portable_pty::Child + Send + Sync>,
    pub(super) pid: Option<u32>,
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
