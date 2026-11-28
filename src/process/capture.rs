use super::*;

pub fn capture(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
    on_output: impl FnMut(bool, &str),
) -> Result<Outcome> {
    capture_with(inv, cwd, dir, timeout, cancel, None, |_| Ok(()), on_output)
}

/// The program and arguments to launch: `inv`'s, inside `sandbox` if any.
pub(super) fn launch(inv: &Invocation, sandbox: Option<&Plan>) -> (String, Vec<String>) {
    match sandbox {
        Some(plan) => plan.wrap(&inv.program, &inv.args),
        None => (inv.program.clone(), inv.args.clone()),
    }
}

/// `capture` inside an optional sandbox, reporting the harness's process
/// group once it starts, so the caller can record it for crash recovery.
#[allow(clippy::too_many_arguments)]
pub fn capture_with(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
    sandbox: Option<&Plan>,
    on_spawn: impl FnOnce(Group) -> Result<()>,
    on_output: impl FnMut(bool, &str),
) -> Result<Outcome> {
    capture_with_stall(
        inv, cwd, dir, timeout, None, cancel, sandbox, on_spawn, on_output,
    )
}

fn is_cancelled(cancel: &AtomicBool, abort_flags: &[&AtomicBool]) -> bool {
    cancel.load(Ordering::Relaxed) || abort_flags.iter().any(|flag| flag.load(Ordering::Relaxed))
}

/// `capture_with` with an optional inactivity timeout: stops the process group
/// when neither stdout nor stderr produces any bytes for `stall_timeout`.
#[allow(clippy::too_many_arguments)]
pub fn capture_with_stall(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    timeout: Duration,
    stall_timeout: Option<Duration>,
    cancel: &AtomicBool,
    sandbox: Option<&Plan>,
    on_spawn: impl FnOnce(Group) -> Result<()>,
    on_output: impl FnMut(bool, &str),
) -> Result<Outcome> {
    capture_with_stall_and_abort(
        inv,
        cwd,
        dir,
        timeout,
        stall_timeout,
        cancel,
        None,
        sandbox,
        on_spawn,
        on_output,
    )
}

/// `capture_with_stall` with an optional secondary `abort` flag alongside
/// `cancel`, so a scheduler can stop sibling workers without mutating the
/// caller's cancellation flag.
#[allow(clippy::too_many_arguments)]
pub fn capture_with_stall_and_abort(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    timeout: Duration,
    stall_timeout: Option<Duration>,
    cancel: &AtomicBool,
    abort: Option<&AtomicBool>,
    sandbox: Option<&Plan>,
    on_spawn: impl FnOnce(Group) -> Result<()>,
    on_output: impl FnMut(bool, &str),
) -> Result<Outcome> {
    let flags: [&AtomicBool; 1];
    let abort_flags: &[&AtomicBool] = match abort {
        Some(flag) => {
            flags = [flag];
            &flags
        }
        None => &[],
    };
    capture_with_controls(
        inv,
        cwd,
        dir,
        timeout,
        stall_timeout,
        cancel,
        abort_flags,
        sandbox,
        on_spawn,
        on_output,
    )
}

/// `capture_with_stall` accepting zero or more secondary abort flags alongside
/// `cancel` (e.g. a scheduler abort flag and a live mid-stream budget abort flag).
#[allow(clippy::too_many_arguments)]
pub fn capture_with_controls(
    inv: &Invocation,
    cwd: &Path,
    dir: &Path,
    timeout: Duration,
    stall_timeout: Option<Duration>,
    cancel: &AtomicBool,
    abort_flags: &[&AtomicBool],
    sandbox: Option<&Plan>,
    on_spawn: impl FnOnce(Group) -> Result<()>,
    mut on_output: impl FnMut(bool, &str),
) -> Result<Outcome> {
    // Check logs land in a directory the stage could write.
    let mut stdout_log = io::BufWriter::new(crate::storage::create_fresh(&dir.join("stdout.log"))?);
    let mut stderr_log = io::BufWriter::new(crate::storage::create_fresh(&dir.join("stderr.log"))?);
    if let Some(warning) = sandbox.and_then(|plan| plan.warning.as_deref()) {
        writeln!(stderr_log, "{warning}")?;
        stderr_log.flush()?;
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
    let marker = group::new_marker();
    command.env(group::MARKER_ENV, &marker);
    inv.write_files()?;
    die_with_parent(&mut command);
    let spawned = command
        .spawn()
        .with_context(|| format!("Launch {}", inv.program))?;
    // Not reaped yet, so the leader's start time is still readable.
    let group = Group::launched(spawned.id(), Some(marker));
    let mut child = ProcessGroup {
        child: spawned,
        started: group.identity.started.clone(),
        leader_reaped: false,
        killed: false,
    };
    on_spawn(group)?;
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
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let started = Instant::now();
    let mut last_activity = started;
    let mut cancelled = false;
    let mut timed_out = false;
    let mut stalled = false;
    let status = loop {
        let out_bytes = if stdout_eof {
            0
        } else {
            drain(
                &mut stdout,
                &mut stdout_log,
                false,
                &mut out_text,
                &mut on_output,
                &mut stdout_eof,
                32,
            )?
        };
        let err_bytes = if stderr_eof {
            0
        } else {
            drain(
                &mut stderr,
                &mut stderr_log,
                true,
                &mut err_text,
                &mut on_output,
                &mut stderr_eof,
                32,
            )?
        };
        if out_bytes > 0 || err_bytes > 0 {
            last_activity = Instant::now();
        }
        if let Some(pipe) = &mut stdin {
            while sent < input.len() {
                match pipe.write(&input[sent..]) {
                    Ok(0) => {
                        sent = input.len();
                        break;
                    }
                    Ok(n) => sent += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                        sent = input.len();
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            if sent == input.len() {
                stdin.take();
            }
        }
        if let Some(status) = child.child.try_wait()? {
            child.leader_reaped = true;
            break status;
        }
        cancelled = is_cancelled(cancel, abort_flags);
        if cancelled && !cancel.load(Ordering::Relaxed) {
            // A secondary abort (e.g. live budget cap triggered by a final
            // result line) may fire micro-seconds before the leader finishes
            // exiting on its own. Give a brief window for natural exit before
            // signalling the group.
            let natural_deadline = Instant::now() + Duration::from_millis(50);
            let mut exited_naturally = None;
            while Instant::now() < natural_deadline {
                if !stdout_eof {
                    drain(
                        &mut stdout,
                        &mut stdout_log,
                        false,
                        &mut out_text,
                        &mut on_output,
                        &mut stdout_eof,
                        32,
                    )?;
                }
                if !stderr_eof {
                    drain(
                        &mut stderr,
                        &mut stderr_log,
                        true,
                        &mut err_text,
                        &mut on_output,
                        &mut stderr_eof,
                        32,
                    )?;
                }
                if let Some(status) = child.child.try_wait()? {
                    exited_naturally = Some(status);
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            if let Some(status) = exited_naturally {
                child.leader_reaped = true;
                cancelled = false;
                break status;
            }
        }
        timed_out = started.elapsed() >= timeout;
        stalled = !timed_out && stall_timeout.is_some_and(|limit| last_activity.elapsed() >= limit);
        if cancelled || timed_out || stalled {
            // Give children reading stdin EOF before or alongside SIGTERM.
            stdin.take();
            // Ask first: harnesses such as omp only save a session they can
            // resume when they exit cleanly.
            signal_group(child.child.id(), libc::SIGTERM);
            let deadline = Instant::now() + GRACE;
            let status = loop {
                if !stdout_eof {
                    drain(
                        &mut stdout,
                        &mut stdout_log,
                        false,
                        &mut out_text,
                        &mut on_output,
                        &mut stdout_eof,
                        32,
                    )?;
                }
                if !stderr_eof {
                    drain(
                        &mut stderr,
                        &mut stderr_log,
                        true,
                        &mut err_text,
                        &mut on_output,
                        &mut stderr_eof,
                        32,
                    )?;
                }
                if let Some(status) = child.child.try_wait()? {
                    // Leftover group members never get the grace period twice.
                    kill_group(child.child.id());
                    child.leader_reaped = true;
                    child.killed = true;
                    break status;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    kill_group(child.child.id());
                    let _ = child.child.kill();
                    let status = child.child.wait()?;
                    child.leader_reaped = true;
                    child.killed = true;
                    break status;
                }
                let wait_ms = remaining.as_millis().clamp(1, 25) as libc::c_int;
                let mut polls = [
                    libc::pollfd {
                        fd: if stdout_eof { -1 } else { stdout.as_raw_fd() },
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: if stderr_eof { -1 } else { stderr.as_raw_fd() },
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // SAFETY: pipe fds are open and outlive the poll call.
                unsafe { libc::poll(polls.as_mut_ptr(), 2, wait_ms) };
            };
            break status;
        }
        let mut remaining = timeout.saturating_sub(started.elapsed());
        if let Some(limit) = stall_timeout {
            remaining = remaining.min(limit.saturating_sub(last_activity.elapsed()));
        }
        let wait_ms = if remaining.is_zero() {
            1
        } else {
            remaining.as_millis().clamp(1, 50) as libc::c_int
        };
        let mut polls = [
            libc::pollfd {
                fd: if stdout_eof { -1 } else { stdout.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if stderr_eof { -1 } else { stderr.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stdin.as_ref().map_or(-1, |s| s.as_raw_fd()),
                events: libc::POLLOUT,
                revents: 0,
            },
        ];
        // SAFETY: pipe fds are open and outlive the poll call.
        unsafe { libc::poll(polls.as_mut_ptr(), 3, wait_ms) };
    };
    // Once the leader exits, drain available bytes without waiting for inherited
    // descriptors held by background grandchildren.
    if !stdout_eof {
        drain(
            &mut stdout,
            &mut stdout_log,
            false,
            &mut out_text,
            &mut on_output,
            &mut stdout_eof,
            1024,
        )?;
    }
    if !stderr_eof {
        drain(
            &mut stderr,
            &mut stderr_log,
            true,
            &mut err_text,
            &mut on_output,
            &mut stderr_eof,
            1024,
        )?;
    }
    // Background children (dev servers, watchers) end with the stage.
    drop(child);
    if !stdout_eof {
        drain(
            &mut stdout,
            &mut stdout_log,
            false,
            &mut out_text,
            &mut on_output,
            &mut stdout_eof,
            1024,
        )?;
    }
    if !stderr_eof {
        drain(
            &mut stderr,
            &mut stderr_log,
            true,
            &mut err_text,
            &mut on_output,
            &mut stderr_eof,
            1024,
        )?;
    }
    for (stderr, decoder) in [(false, &mut out_text), (true, &mut err_text)] {
        let rest = decoder.finish();
        if !rest.is_empty() {
            on_output(stderr, &rest);
        }
    }
    stdout_log
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?
        .sync_all()?;
    stderr_log
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?
        .sync_all()?;
    Ok(Outcome {
        code: status.code(),
        cancelled,
        timed_out,
        stalled,
        released: false,
        finished_with_handoff: false,
    })
}
