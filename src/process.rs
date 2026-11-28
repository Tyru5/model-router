//! Unix process supervision. No command is evaluated by a shell.
pub mod group;
pub mod host;

mod capture;
mod interactive;
mod terminal;

use capture::launch;
pub use capture::{
    capture, capture_with, capture_with_controls, capture_with_stall, capture_with_stall_and_abort,
};
pub use group::{Group, Identity, Verdict};
pub use interactive::{AttachOutcome, InteractiveSession};
use interactive::{INPUT_LIMIT, OutputDecoder, Parked, PtyChild, SessionState};
#[cfg(test)]
use interactive::{InputDecoder, NativeModes, OUTPUT_LIMIT, SEQUENCE_LIMIT};
use terminal::drain_pty;
pub use terminal::{
    Survivor, Terminal, interactive, interactive_with_poll, reconnect, reconnect_with_poll,
    recorded_exit,
};

use crate::{adapters::Invocation, sandbox::Plan, storage::FileLock};
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, IsTerminal, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
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
    /// Stopped because the unattended harness produced no output for its
    /// configured `stall_timeout_seconds`.
    pub stalled: bool,
    /// An interactive harness left running in its terminal host for a
    /// later mr; it has not ended.
    pub released: bool,
    /// An interactive harness stopped cleanly by the user once it had already
    /// written its handoff file, so the router concludes the stage from it.
    pub finished_with_handoff: bool,
}
impl Outcome {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.cancelled && !self.timed_out && !self.stalled
    }
}

pub(super) fn nonblocking(fd: i32) -> io::Result<()> {
    // SAFETY: fcntl does not take ownership; fd is borrowed from a live pipe/PTY.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn kill_group(pid: u32) {
    signal_group(pid, libc::SIGKILL);
}

/// `-pgid` for kill(2), for a value that can name a process group. IDs come
/// from saved runs too: 0 or 1 would signal our own group or every process,
/// and a value past `i32::MAX` would wrap and signal a single process.
pub(super) fn group_target(pgid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pgid)
        .ok()
        .filter(|&id| id > 1)
        .map(|id| -id)
}

pub(super) fn signal_group(pid: u32, signal: i32) {
    if let Some(target) = group_target(pid) {
        // SAFETY: kill takes no pointers; the target is a whole group.
        unsafe {
            libc::kill(target, signal);
        }
    }
}

/// Whether any process, including a zombie, is still in group `pgid`.
pub fn group_alive(pgid: u32) -> bool {
    // SAFETY: signal 0 only checks existence and permission.
    group_target(pgid).is_some_and(|target| unsafe { libc::kill(target, 0) } == 0)
}

/// How long a cancelled or timed-out harness may take to save its session
/// (for takeover) after SIGTERM, before SIGKILL. Also how long leftover
/// background children get after a harness exits.
pub const GRACE: Duration = Duration::from_secs(5);

/// Ends process group `pgid`: SIGTERM, up to `grace` for a clean exit, then
/// SIGKILL for whatever is left. `reap` reaps the leader when it is our
/// child, so its zombie does not count as alive.
pub fn end_group(pgid: u32, grace: Duration, reap: impl FnMut()) {
    end_group_if(pgid, grace, reap, || true);
}

/// `end_group`, sending each signal only if `still_ours` holds just before.
pub(crate) fn end_group_if(
    pgid: u32,
    grace: Duration,
    mut reap: impl FnMut(),
    mut still_ours: impl FnMut() -> bool,
) {
    reap();
    if !group_alive(pgid) || !still_ours() {
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
    if still_ours() {
        kill_group(pgid);
    }
    reap();
}

pub(crate) fn group_reused(recorded: &Option<String>, pid: u32) -> bool {
    let Some(rec) = recorded else {
        return false;
    };
    group::started(pid).is_some_and(|now| now != *rec)
}

/// A harness leader and its process group. Background children (dev
/// servers, watchers) must not keep editing the workspace after its lock is
/// released, so dropping this always ends the whole group, after a normal
/// exit too: SIGTERM, `GRACE`, then SIGKILL.
pub(super) struct ProcessGroup {
    child: Child,
    started: Option<String>,
    leader_reaped: bool,
    killed: bool,
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let pid = self.child.id();
        if self.killed {
            kill_group(pid);
        } else {
            let child = &mut self.child;
            let started = &self.started;
            let leader_reaped = self.leader_reaped;
            end_group_if(
                pid,
                GRACE,
                || {
                    let _ = child.try_wait();
                },
                || !leader_reaped || !group_reused(started, pid),
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Decodes a byte stream as UTF-8 across reads: a character split between
/// two chunks is held back, not replaced.
#[derive(Default)]
pub struct Utf8Stream {
    partial: Vec<u8>,
}
impl Utf8Stream {
    pub fn decode_with(&mut self, mut bytes: &[u8], mut emit: impl FnMut(&str)) {
        while !self.partial.is_empty() && !bytes.is_empty() {
            self.partial.push(bytes[0]);
            bytes = &bytes[1..];
            match std::str::from_utf8(&self.partial) {
                Ok(valid) => {
                    emit(valid);
                    self.partial.clear();
                }
                Err(error) => {
                    if let Some(error_len) = error.error_len() {
                        let valid_up_to = error.valid_up_to();
                        if valid_up_to > 0 {
                            emit(std::str::from_utf8(&self.partial[..valid_up_to]).unwrap_or(""));
                        }
                        emit("\u{fffd}");
                        let drop_len = valid_up_to + error_len;
                        let leftover = self.partial[drop_len..].to_vec();
                        self.partial.clear();
                        self.decode_slice(&leftover, &mut emit);
                    } else if self.partial.len() >= 4 {
                        let pending = std::mem::take(&mut self.partial);
                        self.decode_slice(&pending, &mut emit);
                    }
                }
            }
        }
        if self.partial.is_empty() && !bytes.is_empty() {
            self.decode_slice(bytes, &mut emit);
        }
    }

    fn decode_slice(&mut self, mut rest: &[u8], emit: &mut impl FnMut(&str)) {
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    if !valid.is_empty() {
                        emit(valid);
                    }
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        emit(std::str::from_utf8(&rest[..valid_up_to]).unwrap_or_default());
                    }
                    let after = &rest[valid_up_to..];
                    match error.error_len() {
                        Some(length) => {
                            emit("\u{fffd}");
                            rest = &after[length..];
                        }
                        // An incomplete character at the end: wait for the rest.
                        None => {
                            self.partial.extend_from_slice(after);
                            break;
                        }
                    }
                }
            }
        }
    }

    pub fn decode(&mut self, bytes: &[u8]) -> String {
        let mut text = String::new();
        self.decode_with(bytes, |part| text.push_str(part));
        text
    }

    /// Whatever incomplete character the stream ended with.
    pub fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned()
    }
}

pub(super) fn drain(
    reader: &mut impl Read,
    log: &mut impl Write,
    stderr: bool,
    decoder: &mut Utf8Stream,
    on_output: &mut impl FnMut(bool, &str),
    eof: &mut bool,
    max_reads: usize,
) -> Result<usize> {
    let mut bytes = [0u8; 8192];
    let mut total = 0usize;
    let mut reads = 0usize;
    // Bound each pass so a noisy process cannot starve cancellation/timeouts.
    while reads < max_reads {
        match reader.read(&mut bytes) {
            Ok(0) => {
                *eof = true;
                break;
            }
            Ok(n) => {
                reads += 1;
                total += n;
                log.write_all(&bytes[..n])?;
                decoder.decode_with(&bytes[..n], |text| {
                    if !text.is_empty() {
                        on_output(stderr, text);
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    log.flush()?;
    Ok(total)
}

/// Asks the kernel to SIGTERM the harness when the thread that spawned it
/// dies (Linux only): if mr is killed or OOM-killed, the harness must not
/// keep editing a workspace whose lock died with mr. PR_SET_PDEATHSIG fires
/// when the *forking thread* exits, so spawn from the thread that waits on
/// the child, as `capture` does. Other platforms rely on the attempt's
/// recorded process group being stopped by the next `drive` instead.
pub(super) fn die_with_parent(command: &mut Command) {
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

pub fn read_tail(path: &Path, max: usize) -> Result<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(max as u64)))?;
    let mut data = Vec::with_capacity(length.min(max as u64) as usize);
    file.take(max as u64).read_to_end(&mut data)?;
    Ok(String::from_utf8(data)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
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
                &[],
                |_, _| {},
            )
            .unwrap()
        });
        assert!(outcome.cancelled);
        assert!(
            dir.path().join("saved").exists(),
            "SIGKILL before a clean exit"
        );
    }

    fn shell(dir: &Path, script: &str) -> Invocation {
        Invocation {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            stdin: None,
            kind: crate::adapters::OutputKind::Text,
            final_file: dir.join("unused"),
            env: vec![],
            files: vec![],
            assign: None,
        }
    }

    fn wait_for(what: impl Fn() -> bool) {
        let start = Instant::now();
        while !what() {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Runs `during` while another thread checks that the lock at `path`
    /// is held throughout.
    fn never_free<T>(path: &Path, during: impl FnOnce() -> T) -> T {
        struct Done<'a>(&'a AtomicBool);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let done = AtomicBool::new(false);
        let started = AtomicBool::new(false);
        thread::scope(|scope| {
            let watcher = scope.spawn(|| {
                let mut checks = 0u64;
                loop {
                    let free = !crate::storage::is_locked(path);
                    started.store(true, Ordering::Relaxed);
                    if free {
                        return Err(checks);
                    }
                    checks += 1;
                    if done.load(Ordering::Relaxed) {
                        return Ok(checks);
                    }
                }
            });
            // A busy machine may not schedule the watcher until `during` is
            // over; it must have looked at least once before it starts.
            while !started.load(Ordering::Relaxed) {
                thread::yield_now();
            }
            let value = {
                let _done = Done(&done);
                during()
            };
            match watcher.join().unwrap() {
                Ok(checks) => assert!(checks > 0, "the lock was never checked"),
                Err(checks) => panic!("the lock was free after {checks} checks"),
            }
            value
        })
    }

    /// Starts `script` holding the run's lock as mr does, waits for it to
    /// write `ready`, and releases it, handing the lock to the host over the
    /// socket or, as a vanished mr, letting the host take it by path.
    fn release(dir: &Path, script: &str, hand_over: bool) -> PathBuf {
        let inv = shell(dir, script);
        let session = InteractiveSession::new(80, 24);
        let cancel = AtomicBool::new(false);
        let lock = dir.join("run.lock");
        let held = crate::storage::lock(&lock).unwrap();
        let mut socket = PathBuf::new();
        let run = || {
            let handed = [&held];
            let outcome = thread::scope(|scope| {
                scope.spawn(|| {
                    wait_for(|| dir.join("ready").exists());
                    session.release();
                });
                interactive(
                    &inv,
                    dir,
                    dir,
                    &cancel,
                    &session,
                    None,
                    std::slice::from_ref(&lock),
                    |_, path| socket = path.to_path_buf(),
                )
                .unwrap()
            });
            assert!(outcome.released, "{outcome:?}");
            session.hand_over(if hand_over { &handed } else { &[] });
            // As mr exits. A lock it handed over stays with the host.
            drop(held);
            // Longer than the host's retry interval, so a gap would show.
            thread::sleep(Duration::from_millis(200));
        };
        if hand_over {
            never_free(&lock, run);
        } else {
            run();
            wait_for(|| crate::storage::is_locked(&lock));
        }
        socket
    }

    #[test]
    fn a_released_harness_keeps_running_and_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        let socket = release(
            dir.path(),
            "echo ready > ready; read line; echo \"$line\" > got; exit 3",
            true,
        );
        // Detached, the host holds the run's lock; attaching hands it over.
        let lock = dir.path().join("run.lock");
        let mut terminal = never_free(&lock, || Terminal::connect(&socket).unwrap());
        let taken = terminal.take_locks();
        assert_eq!(taken.len(), 1);
        never_free(&lock, || thread::sleep(Duration::from_millis(100)));
        // The host kept no copy: closing the caller's frees it.
        for lock in &taken {
            lock.hand_over();
        }
        drop(taken);
        wait_for(|| !crate::storage::is_locked(&lock));
        // One router at a time.
        let busy = Terminal::connect(&socket).err().unwrap();
        assert!(format!("{busy:#}").contains("another router"), "{busy:#}");
        let session = InteractiveSession::new(80, 24);
        session.state().input.extend(b"hello\r");
        let cancel = AtomicBool::new(false);
        let outcome = reconnect(terminal, dir.path(), &cancel, &session).unwrap();
        assert_eq!(outcome.code, Some(3));
        assert!(!outcome.cancelled && !outcome.released);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("got"))
                .unwrap()
                .trim(),
            "hello"
        );
        assert!(!socket.exists(), "the host removes its socket");
    }

    #[test]
    fn a_harness_that_exits_while_detached_records_how() {
        let dir = tempfile::tempdir().unwrap();
        let socket = release(
            dir.path(),
            "echo ready > ready; while [ ! -e go ]; do sleep 0.05; done; exit 7",
            true,
        );
        std::fs::write(dir.path().join("go"), "").unwrap();
        wait_for(|| !socket.exists());
        assert!(Terminal::connect(&socket).is_err());
        let outcome = recorded_exit(dir.path()).unwrap();
        assert_eq!(outcome.code, Some(7));
        assert!(!outcome.cancelled);
        // The host let go of the lock when it exited.
        assert!(!crate::storage::is_locked(&dir.path().join("run.lock")));
    }

    #[test]
    fn a_reconnected_harness_hands_the_lock_back_when_released_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = release(
            dir.path(),
            "echo ready > ready; while [ ! -e go ]; do sleep 0.05; done; exit 0",
            true,
        );
        let lock = dir.path().join("run.lock");
        never_free(&lock, || {
            let mut terminal = Terminal::connect(&socket).unwrap();
            let held = terminal.take_locks();
            assert_eq!(held.len(), 1);
            let session = InteractiveSession::new(80, 24);
            let cancel = AtomicBool::new(false);
            let outcome = thread::scope(|scope| {
                scope.spawn(|| {
                    wait_for(|| session.live());
                    session.release();
                });
                reconnect(terminal, dir.path(), &cancel, &session).unwrap()
            });
            assert!(outcome.released, "{outcome:?}");
            // Until the caller saved the run and hands over, the connection
            // stays: no other router can take the session or its locks.
            let busy = Terminal::connect(&socket).err().unwrap();
            assert!(format!("{busy:#}").contains("another router"), "{busy:#}");
            session.hand_over(&[&held[0]]);
            drop(held);
            thread::sleep(Duration::from_millis(200));
        });
        std::fs::write(dir.path().join("go"), "").unwrap();
        wait_for(|| !socket.exists());
        wait_for(|| !crate::storage::is_locked(&lock));
    }

    #[test]
    fn an_attached_router_that_gives_up_hands_the_lock_back() {
        let dir = tempfile::tempdir().unwrap();
        let socket = release(
            dir.path(),
            "echo ready > ready; while [ ! -e go ]; do sleep 0.05; done; exit 0",
            true,
        );
        let lock = dir.path().join("run.lock");
        // As when reattaching fails before the harness is reconnected.
        never_free(&lock, || {
            let mut terminal = Terminal::connect(&socket).unwrap();
            let held = terminal.take_locks();
            terminal.hand_over(&[], &[&held[0]]);
            drop(held);
            drop(terminal);
            thread::sleep(Duration::from_millis(200));
        });
        // The host kept them, and still hands them over to the next router.
        let mut terminal = Terminal::connect(&socket).unwrap();
        assert_eq!(terminal.take_locks().len(), 1);
        std::fs::write(dir.path().join("go"), "").unwrap();
        let session = InteractiveSession::new(80, 24);
        let cancel = AtomicBool::new(false);
        let outcome = reconnect(terminal, dir.path(), &cancel, &session).unwrap();
        assert_eq!(outcome.code, Some(0));
    }

    #[test]
    fn a_host_takes_the_lock_of_an_mr_that_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let socket = release(
            dir.path(),
            "echo ready > ready; while [ ! -e go ]; do sleep 0.05; done; exit 0",
            false,
        );
        // Taken by path, it is still handed over on attach, and is a lock on
        // that file and no other.
        let mut terminal = Terminal::connect(&socket).unwrap();
        let locks = terminal.take_locks();
        assert_eq!(locks.len(), 1);
        assert!(locks[0].is(&dir.path().join("run.lock")));
        std::fs::write(dir.path().join("other.lock"), "").unwrap();
        assert!(!locks[0].is(&dir.path().join("other.lock")));
        assert!(!locks[0].is(&dir.path().join("missing.lock")));
        drop(locks);
        std::fs::write(dir.path().join("go"), "").unwrap();
        let session = InteractiveSession::new(80, 24);
        let cancel = AtomicBool::new(false);
        let outcome = reconnect(terminal, dir.path(), &cancel, &session).unwrap();
        assert_eq!(outcome.code, Some(0));
    }

    #[test]
    fn cancelling_a_reconnected_harness_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let socket = release(
            dir.path(),
            "trap 'echo saved > saved; exit 0' TERM; echo ready > ready; while :; do sleep 0.05; done",
            true,
        );
        let terminal = Terminal::connect(&socket).unwrap();
        let pgid = terminal.pid();
        let session = InteractiveSession::new(80, 24);
        let cancel = AtomicBool::new(true);
        let outcome = reconnect(terminal, dir.path(), &cancel, &session).unwrap();
        assert!(outcome.cancelled);
        assert!(!group_alive(pgid), "harness group outlived reconnect()");
        wait_for(|| dir.path().join("saved").exists());
        wait_for(|| !socket.exists());
    }

    #[test]
    fn only_real_process_group_ids_are_signalled() {
        assert_eq!(group_target(0), None);
        assert_eq!(group_target(1), None);
        assert_eq!(group_target(4242), Some(-4242));
        assert_eq!(group_target(i32::MAX as u32), Some(-i32::MAX));
        // Would wrap to a negative ID, i.e. one process rather than a group.
        assert_eq!(group_target(i32::MAX as u32 + 2), None);
        assert_eq!(group_target(u32::MAX), None);
        assert!(!group_alive(u32::MAX));
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

    #[test]
    fn kitty_key_release_and_osc_hyperlinks_do_not_misfire() {
        use interactive::{DrawingModes, enhanced_control, is_terminal_query};
        assert_eq!(
            enhanced_control(b"\x1b[98;5u"),
            Some(AttachOutcome::Detached)
        );
        assert_eq!(
            enhanced_control(b"\x1b[98;5:1u"),
            Some(AttachOutcome::Detached)
        );
        assert_eq!(enhanced_control(b"\x1b[98;5:3u"), None);
        assert_eq!(enhanced_control(b"\x1b[93;5:3u"), None);

        assert!(!is_terminal_query(
            b"\x1b]8;;https://example.com/?a=1\x1b\\"
        ));
        assert!(!is_terminal_query(b"\x1b]0;Title?\x07"));
        assert!(is_terminal_query(b"\x1b]10;?\x07"));
        assert!(is_terminal_query(b"\x1b]4;1;?\x1b\\"));

        // Attached live streaming keeps queries in exact stream order.
        let session = InteractiveSession::new(20, 6);
        let mut state = session.state();
        state.attached = true;
        state.redraw = false;
        state.output(b"before");
        state.output(b"\x1b[6n");
        state.output(b"after");
        let mut live = Vec::new();
        state.take_output(&mut live);
        assert_eq!(live, b"before\x1b[6nafter");

        // Private CSI ? r must not overwrite scroll region.
        let mut modes = DrawingModes::default();
        modes.process(b"2;10r");
        modes.process(b"?1049r");
        let mut out = Vec::new();
        modes.restore(&mut out, state.parser.screen());
        assert!(out.starts_with(b"\x1b[2;6r"), "{out:?}");

        // Interrupted escape prefix emits cleanly without absorbing the next CSI.
        let mut decoder = OutputDecoder::default();
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        decoder.process(b"\x1b[1\x1b[31m", |chunk| chunks.push(chunk.to_vec()));
        assert_eq!(chunks, vec![b"\x1b[1".to_vec(), b"\x1b[31m".to_vec()]);
    }
}
