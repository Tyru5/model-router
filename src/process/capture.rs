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
    mut on_output: impl FnMut(bool, &str),
) -> Result<Outcome> {
    // Check logs land in a directory the stage could write.
    let mut stdout_log = crate::storage::create_fresh(&dir.join("stdout.log"))?;
    let mut stderr_log = crate::storage::create_fresh(&dir.join("stderr.log"))?;
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
    let marker = group::new_marker();
    command.env(group::MARKER_ENV, &marker);
    inv.write_files()?;
    die_with_parent(&mut command);
    let mut child = ProcessGroup {
        child: command
            .spawn()
            .with_context(|| format!("Launch {}", inv.program))?,
    };
    // Not reaped yet, so the leader's start time is still readable.
    on_spawn(Group::launched(child.child.id(), Some(marker)))?;
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
    let mut last_activity = started;
    let mut cancelled = false;
    let mut timed_out = false;
    let mut stalled = false;
    let status = loop {
        let out_bytes = drain(
            &mut stdout,
            &mut stdout_log,
            false,
            &mut out_text,
            &mut on_output,
        )?;
        let err_bytes = drain(
            &mut stderr,
            &mut stderr_log,
            true,
            &mut err_text,
            &mut on_output,
        )?;
        if out_bytes > 0 || err_bytes > 0 {
            last_activity = Instant::now();
        }
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
        stalled = !timed_out && stall_timeout.is_some_and(|limit| last_activity.elapsed() >= limit);
        if cancelled || timed_out || stalled {
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
        stalled,
        released: false,
    })
}
