use super::interactive::{OutputDecoder, SessionFinish};
use super::*;

/// Launches `inv` in its own terminal host (see `host`) and supervises it
/// from here without accessing the host terminal: output keeps draining and
/// being logged while detached; the foreground alone calls `attach`.
/// `on_start` receives the harness's process group and the host's socket
/// once it runs, so the caller can record them for recovery and reattachment.
/// `locks` are the caller's lock paths, which the host takes by path only if
/// a released session's caller vanished before `InteractiveSession::hand_over`.
#[allow(clippy::too_many_arguments)]
pub fn interactive(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    cancel: &AtomicBool,
    session: &InteractiveSession,
    sandbox: Option<&Plan>,
    locks: &[PathBuf],
    on_start: impl FnOnce(Group, &Path),
) -> Result<Outcome> {
    let (cols, rows) = match begin(session, cancel)? {
        Ok(size) => size,
        Err(outcome) => return Ok(outcome),
    };
    let _finish = SessionFinish(session);
    let (program, args) = launch(inv, sandbox);
    let env = inv.environment()?;
    inv.write_files()?;
    let mut stderr_log = crate::storage::create_fresh(&dir.join("stderr.log"))?;
    let warning = sandbox.and_then(|plan| plan.warning.clone());
    if let Some(warning) = &warning {
        writeln!(stderr_log, "{warning}")?;
    }
    let socket = host::socket_path()?;
    let marker = group::new_marker();
    let spec = host::Spec {
        program,
        args,
        cwd: cwd.to_path_buf(),
        dir: dir.to_path_buf(),
        env: env
            .into_iter()
            .chain([(group::MARKER_ENV.to_owned(), marker.clone())])
            .collect(),
        removed: inv
            .removed()
            .into_iter()
            .filter_map(|name| name.into_string().ok())
            .collect(),
        cols,
        rows,
        socket: socket.clone(),
        locks: locks.to_vec(),
        warning,
        report_started: true,
    };
    // The host, not this process, is the harness's parent and reaps it, so
    // only the host can read the leader's start time before its PID can be
    // reused; it reports it with the PID.
    let (pid, started) = host::spawn(&spec)?;
    let group = Group {
        pgid: pid,
        identity: Identity {
            started,
            marker: Some(marker.clone()),
        },
    };
    on_start(group.clone(), &socket);
    let mut terminal = match Terminal::connect(&socket) {
        Ok(terminal) => terminal,
        Err(error) => {
            // Never leave a harness nobody can reach. Its host may have died,
            // leaving a member nothing proves is the harness's: say so, so the
            // caller keeps the record.
            return Err(match Survivor::after(&group) {
                Some(survivor) => error.context(survivor),
                None => error,
            });
        }
    };
    terminal.set_marker(Some(marker));
    terminal.supervise(dir, cancel, session)
}

/// A harness's process group that cleanup after a failure left running,
/// because nothing still proved it was the harness's (see
/// `group::end_recorded`) or it outlived SIGKILL. Carried in the error, so
/// the caller keeps its record rather than forgetting it.
#[derive(Debug)]
pub struct Survivor {
    pub group: Group,
    pub why: &'static str,
}
impl Survivor {
    /// Ends `group` as `group::end_recorded` does; Some when it is left running.
    fn after(group: &Group) -> Option<Self> {
        Self::after_within(group, GRACE)
    }

    pub(super) fn after_within(group: &Group, grace: Duration) -> Option<Self> {
        let why = match group::end_recorded(group, grace) {
            Verdict::Unverified(why) => why,
            Verdict::Ours => "it outlived SIGKILL",
            Verdict::Gone | Verdict::Reused => return None,
        };
        Some(Self {
            group: group.clone(),
            why,
        })
    }
}
impl std::fmt::Display for Survivor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pgid = self.group.pgid;
        write!(
            f,
            "Process group {pgid} may still be running the harness: {}. Stop it with \
             `kill -TERM -- -{pgid}`",
            self.why
        )
    }
}

/// Reconnects `session` to a harness whose host outlived an earlier mr.
pub fn reconnect(
    terminal: Terminal,
    dir: &Path,
    cancel: &AtomicBool,
    session: &InteractiveSession,
) -> Result<Outcome> {
    {
        let mut state = session.state();
        ensure!(
            !state.started && !state.finished,
            "Interactive session has already ended or started"
        );
        state.started = true;
    }
    let _finish = SessionFinish(session);
    terminal.supervise(dir, cancel, session)
}

/// How the harness of an attempt ended while no mr was connected, if its
/// host recorded it.
pub fn recorded_exit(dir: &Path) -> Option<Outcome> {
    host::recorded(dir).map(|exit| Outcome {
        code: exit.code,
        cancelled: exit.cancelled,
        timed_out: false,
        stalled: false,
        released: false,
    })
}

/// Marks the session started; the error is the outcome when it was
/// cancelled before anything ran.
pub(super) fn begin(
    session: &InteractiveSession,
    cancel: &AtomicBool,
) -> Result<std::result::Result<(u16, u16), Outcome>> {
    let mut state = session.state();
    ensure!(
        !state.started && !state.finished,
        "Interactive session has already ended or started"
    );
    state.started = true;
    if cancel.load(Ordering::Relaxed) || state.cancelled {
        state.finished = true;
        return Ok(Err(Outcome {
            code: None,
            cancelled: true,
            timed_out: false,
            stalled: false,
            released: false,
        }));
    }
    Ok(Ok(state.size))
}

/// A connection to a terminal host that has accepted it as its client.
pub struct Terminal {
    stream: UnixStream,
    frames: host::Frames,
    pid: u32,
    /// The harness's start time as its host read it at launch; None from a
    /// host built before it reported one.
    started: Option<String>,
    marker: Option<String>,
    locks: Vec<FileLock>,
}

impl Terminal {
    /// Connects and attaches. Refused while another mr is attached. Once
    /// this returns, the host has let go of the run's locks; it hands them
    /// over (`take_locks`) if it held them.
    pub fn connect(socket: &Path) -> Result<Self> {
        // An mr that just let go may not have been seen leaving yet.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match Self::attach(socket)? {
                Some(terminal) => return Ok(terminal),
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                None => anyhow::bail!(
                    "This harness terminal is attached to another router. Detach or quit there first"
                ),
            }
        }
    }

    /// One attach attempt; None when the host is busy with another mr.
    fn attach(socket: &Path) -> Result<Option<Self>> {
        let mut stream = UnixStream::connect(socket)
            .with_context(|| format!("Connect to the harness terminal {}", socket.display()))?;
        let mut out = Vec::new();
        // [1]: send the harness's start time too (see `host::attach_request`).
        host::frame(host::tag::ATTACH, &[1], &mut out);
        // Before asking: macOS refuses options on a socket the host has
        // already answered (busy) and closed.
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(&out)?;
        // Read with the descriptors the answer carries; output may follow it.
        let mut frames = host::Frames::default();
        let (tag, payload) = loop {
            if let Some(frame) = frames.next()? {
                break frame;
            }
            match frames.read(&stream) {
                Ok(0) => anyhow::bail!("The harness terminal did not answer"),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("The harness terminal did not answer"),
            }
        };
        let locks: Vec<_> = frames
            .take_fds()
            .into_iter()
            .map(FileLock::from_fd)
            .collect();
        let (pid, started) = match tag {
            host::tag::SPAWNED if payload.len() >= 4 => (
                u32::from_be_bytes(payload[..4].try_into()?),
                String::from_utf8(payload[4..].to_vec())
                    .ok()
                    .filter(|started| !started.is_empty()),
            ),
            host::tag::BUSY => return Ok(None),
            _ => anyhow::bail!("The harness terminal answered unexpectedly"),
        };
        stream.set_nonblocking(true)?;
        Ok(Some(Self {
            stream,
            frames,
            pid,
            started,
            marker: None,
            locks,
        }))
    }

    pub fn set_marker(&mut self, marker: Option<String>) {
        self.marker = marker;
    }

    fn group(&self) -> Group {
        Group {
            pgid: self.pid,
            identity: Identity {
                started: self.started.clone(),
                marker: self.marker.clone(),
            },
        }
    }

    /// The run's locks the host held while detached, in its order; empty
    /// when it held none.
    pub fn take_locks(&mut self) -> Vec<FileLock> {
        std::mem::take(&mut self.locks)
    }

    /// Hands `locks` to the host before the connection closes, so they are
    /// never free; pending frames go first, as one cut short would corrupt
    /// the stream. Without this the host takes them by path, after a gap.
    pub fn hand_over(&self, pending: &[u8], locks: &[&FileLock]) {
        if locks.is_empty() {
            return;
        }
        let mut stream = &self.stream;
        let ready = stream.set_nonblocking(false).is_ok()
            && stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .is_ok()
            && stream.write_all(pending).is_ok();
        if !ready {
            return;
        }
        let mut out = Vec::new();
        host::frame(host::tag::LOCKS, &[], &mut out);
        let fds: Vec<_> = locks.iter().map(|lock| lock.fd().as_raw_fd()).collect();
        if let Ok(n @ 1..) = host::send(stream, &out, &fds) {
            for lock in locks {
                lock.hand_over();
            }
            let _ = stream.write_all(&out[n..]);
        }
    }

    /// The harness's process group.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// When the harness started, as its host read it before it could reap
    /// it; None from a host that did not report it.
    pub fn started(&self) -> Option<String> {
        self.started.clone()
    }

    pub(super) fn supervise(
        mut self,
        dir: &Path,
        cancel: &AtomicBool,
        session: &InteractiveSession,
    ) -> Result<Outcome> {
        let group = self.group();
        let mut frames = std::mem::take(&mut self.frames);
        let mut decoder = OutputDecoder::default();
        let mut out = Vec::new();
        let mut written = 0;
        let mut size = None;
        let mut cancelled = false;
        let mut stopped: Option<Instant> = None;
        loop {
            session.wakes.0.clear();
            let has_more_input = {
                let mut state = session.state();
                if state.released && stopped.is_none() {
                    // Closing the connection leaves the harness to its host;
                    // the caller does that once it saved the run.
                    state.parked = Some(Parked {
                        pending: out[written..].to_vec(),
                        terminal: self,
                    });
                    return Ok(Outcome {
                        code: None,
                        cancelled: false,
                        timed_out: false,
                        stalled: false,
                        released: true,
                    });
                }
                cancelled |= cancel.load(Ordering::Relaxed) || state.cancelled || state.finished;
                if cancelled && stopped.is_none() {
                    cancel.store(true, Ordering::Relaxed);
                    host::frame(host::tag::STOP, &[], &mut out);
                    stopped = Some(Instant::now());
                }
                if size != Some(state.size) {
                    size = Some(state.size);
                    let (cols, rows) = state.size;
                    let c = cols.to_be_bytes();
                    let r = rows.to_be_bytes();
                    host::frame(host::tag::RESIZE, &[c[0], c[1], r[0], r[1]], &mut out);
                }
                if !state.input.is_empty() && out.len() < INPUT_LIMIT {
                    let (first, second) = state.input.as_slices();
                    host::frame_slices(host::tag::INPUT, first, second, &mut out);
                    state.input.clear();
                }
                !state.input.is_empty()
            };
            let flushed = host::flush(&mut self.stream, &mut out, &mut written);
            let filled = frames.fill(&self.stream);
            let open = flushed && filled;
            // The host sends descriptors only with its answer to attaching.
            drop(frames.take_fds());
            let mut output = false;
            let read = (|| -> Result<Option<Outcome>> {
                while let Some((tag, payload)) = frames.next()? {
                    match tag {
                        host::tag::OUTPUT => {
                            let mut state = session.state();
                            decoder.process(&payload, |frame| state.output(frame));
                            output = true;
                        }
                        host::tag::EXIT => {
                            let exit: host::Exit = serde_json::from_slice(&payload)?;
                            return Ok(Some(Outcome {
                                code: exit.code,
                                cancelled: cancelled || exit.cancelled,
                                timed_out: false,
                                stalled: false,
                                released: false,
                            }));
                        }
                        _ => {}
                    }
                }
                Ok(None)
            })();
            if output {
                session.wake_attachment();
            }
            match read {
                Ok(Some(outcome)) => {
                    session.finish();
                    return Ok(outcome);
                }
                Ok(None) => {}
                Err(error) => {
                    session.finish();
                    // Returning alone would leave the harness to its host as
                    // if detached, with no run pointing at it.
                    return Err(match Survivor::after(&group) {
                        Some(survivor) => error.context(survivor),
                        None => error,
                    });
                }
            }
            if !open {
                session.finish();
                if let Some(mut outcome) = recorded_exit(dir) {
                    outcome.cancelled |= cancelled;
                    return Ok(outcome);
                }
                // The host died: its closed PTY hangs the harness up; make sure.
                let error = anyhow::anyhow!("The harness terminal host stopped unexpectedly");
                return Err(match Survivor::after(&group) {
                    Some(survivor) => error.context(survivor),
                    None => error,
                });
            }
            if stopped.is_some_and(|at| at.elapsed() > GRACE * 2) {
                // The host did not end it in time; verify identity before stopping.
                session.finish();
                if let Some(survivor) = Survivor::after_within(&group, Duration::ZERO) {
                    return Err(anyhow::anyhow!("Harness did not stop in time").context(survivor));
                }
                return Ok(Outcome {
                    code: None,
                    cancelled: true,
                    timed_out: false,
                    stalled: false,
                    released: false,
                });
            }
            let mut polls = [
                libc::pollfd {
                    fd: self.stream.as_raw_fd(),
                    events: libc::POLLIN | if out.is_empty() { 0 } else { libc::POLLOUT },
                    revents: 0,
                },
                libc::pollfd {
                    fd: session.wakes.0.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout = if has_more_input && out.is_empty() {
                0
            } else {
                40
            };
            // SAFETY: both descriptors are borrowed; the array is initialized.
            unsafe { libc::poll(polls.as_mut_ptr(), 2, timeout) };
        }
    }
}

pub(super) fn drain_pty(
    reader: &mut impl Read,
    transcript: &mut impl Write,
    decoder: &mut OutputDecoder,
    emit: &mut impl FnMut(&[u8]),
    bytes: &mut [u8; 8192],
    eof: &mut bool,
    max_reads: usize,
) -> Result<()> {
    // Bound each pass, not the lifetime stream, so floods cannot starve cancel.
    let mut reads = 0usize;
    while reads < max_reads {
        match reader.read(bytes) {
            Ok(0) => {
                *eof = true;
                break;
            }
            Ok(n) => {
                reads += 1;
                transcript.write_all(&bytes[..n])?;
                decoder.process(&bytes[..n], &mut *emit);
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
    transcript.flush()?;
    Ok(())
}
