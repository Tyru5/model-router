//! The terminal host: a small process, separate from mr, that owns an
//! interactive harness's PTY and screen. mr connects over a private Unix
//! socket, so quitting mr leaves the harness running and a later mr
//! reattaches with the screen intact. One mr is attached at a time. While
//! none is, the host holds the run and workspace locks, so no other run
//! takes the checkout. The locks change hands as file descriptors sent over
//! the socket (flock locks belong to the open file, which stays open in
//! flight), so they are never free in between.
use super::{
    FileLock, GRACE, INPUT_LIMIT, OutputDecoder, PtyChild, SessionState, drain_pty, kill_group,
    nonblocking, signal_group,
};
use anyhow::{Context, Result, bail, ensure};
use portable_pty::{CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{DirBuilderExt, MetadataExt, PermissionsExt},
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
    /// Held while no mr is connected, in this order. An mr hands them over
    /// when it lets go; the host takes them by path only when one vanished.
    pub locks: Vec<PathBuf>,
    pub warning: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Exit {
    pub code: Option<i32>,
    pub cancelled: bool,
}

/// Frame tags. Client to host: attach, input, resize, stop, locks (the
/// client's lock descriptors, sent as it lets go). Host to client: the
/// harness's process ID (attach accepted, carrying the host's lock
/// descriptors if it held them), busy (another mr is attached), output, exit.
pub(super) mod tag {
    pub const ATTACH: u8 = b'a';
    pub const INPUT: u8 = b'i';
    pub const RESIZE: u8 = b'r';
    pub const STOP: u8 = b's';
    pub const LOCKS: u8 = b'l';
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

/// Splits a byte stream into frames, keeping the descriptors it carries.
#[derive(Default)]
pub(super) struct Frames {
    buffer: Vec<u8>,
    fds: Vec<OwnedFd>,
}
impl Frames {
    /// One read; 0 at end of stream.
    pub fn read(&mut self, stream: &UnixStream) -> io::Result<usize> {
        let mut bytes = [0u8; 16 * 1024];
        let n = recv(stream, &mut bytes, &mut self.fds)?;
        self.buffer.extend_from_slice(&bytes[..n]);
        // Only one frame carries descriptors; never pile up a peer's extras.
        self.fds.truncate(MAX_FDS);
        Ok(n)
    }
    /// Reads what is available without blocking. False at end of stream or
    /// on an error that ends the connection.
    pub fn fill(&mut self, stream: &UnixStream) -> bool {
        loop {
            match self.read(stream) {
                Ok(0) => return false,
                Ok(_) => {
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
    /// The descriptors received so far, in order; the frame that carried
    /// them has been read once it is returned by `next`.
    pub fn take_fds(&mut self) -> Vec<OwnedFd> {
        std::mem::take(&mut self.fds)
    }
}

/// More descriptors than any frame carries.
const MAX_FDS: usize = 4;
#[cfg(target_os = "linux")]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
const SEND_FLAGS: libc::c_int = 0;

/// Writes the start of `bytes` with `fds` attached (SCM_RIGHTS); how much
/// was written. The descriptors went along if and only if that is above 0.
pub(super) fn send(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let size = std::mem::size_of_val(fds) as libc::c_uint;
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(size) } as usize;
    // u64 elements align the header for both Linux and macOS layouts.
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: msghdr is plain data; zero is valid for every field.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: the control buffer is aligned and CMSG_SPACE(size) long, so
        // its first header and `size` data bytes fit.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(size) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                size as usize,
            );
        }
    }
    loop {
        // SAFETY: msg and the buffers it points to outlive the call.
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, SEND_FLAGS) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Reads into `bytes`, appending any descriptors received to `fds`.
fn recv(stream: &UnixStream, bytes: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
    let mut control = [0u64; 16];
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<RawFd>()) as libc::c_uint) };
    debug_assert!(space as usize <= size_of_val(&control));
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: msghdr is plain data; zero is valid for every field.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    let n = loop {
        // SAFETY: msg and the buffers it points to outlive the call.
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, flags) };
        if n >= 0 {
            break n as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    let mut received = Vec::new();
    // SAFETY: recvmsg filled msg_controllen bytes of headers; each
    // SCM_RIGHTS payload, up to what was delivered, is descriptors this
    // process now owns. A truncated header can claim more (macOS keeps the
    // sender's length), so payloads are bounded by the delivered bytes.
    unsafe {
        let end = msg
            .msg_control
            .cast::<u8>()
            .add(msg.msg_controllen as usize);
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                let claimed =
                    ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                let delivered = (end as usize).saturating_sub(data as usize);
                let data = data.cast::<RawFd>();
                for index in 0..claimed.min(delivered) / size_of::<RawFd>() {
                    let fd = OwnedFd::from_raw_fd(data.add(index).read_unaligned());
                    // Never let a later child inherit a lock. macOS has no
                    // MSG_CMSG_CLOEXEC, so a child another thread spawns
                    // between recvmsg and here could still inherit it; it
                    // then holds the lock until it exits if mr dies first.
                    #[cfg(not(target_os = "linux"))]
                    libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
                    received.push(fd);
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    // Cut short, they are not the set the peer meant: close them all.
    if msg.msg_flags & libc::MSG_CTRUNC == 0 {
        fds.extend(received);
    }
    Ok(n)
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
pub(crate) fn socket_dir() -> Result<PathBuf> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
    {
        Some(runtime) => runtime.join("model-router"),
        None => std::env::temp_dir().join(format!("model-router-{uid}")),
    };
    // Created private, so a concurrent mr never sees it with umask bits.
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        // The umask may have taken owner bits too.
        Ok(()) => fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("Create {}", dir.display())),
    }
    // The socket accepts keystrokes for an agent: refuse a directory another
    // user could have planted or can read.
    ensure!(
        is_private_dir(&dir),
        "{} must be a private directory owned by you",
        dir.display()
    );
    Ok(dir)
}

/// A real directory, not a link, owned by this user and closed to others.
pub fn is_private_dir(dir: &Path) -> bool {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    fs::symlink_metadata(dir)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0)
}

pub(crate) fn socket_path() -> Result<PathBuf> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    Ok(socket_dir()?.join(format!("{}.sock", &id[..16])))
}

/// The directories `socket_dir` uses here, or would use with no
/// `XDG_RUNTIME_DIR`, the usual one, or the default temporary directory: an
/// earlier mr may have had another environment. One that had another
/// `TMPDIR` or a custom `XDG_RUNTIME_DIR` used a directory not listed here.
/// Sandboxed stages must not reach any of them (see `sandbox::plan_with`).
pub fn socket_dirs() -> Vec<PathBuf> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut dirs = vec![
        std::env::temp_dir().join(format!("model-router-{uid}")),
        PathBuf::from(format!("/tmp/model-router-{uid}")),
        PathBuf::from(format!("/run/user/{uid}/model-router")),
    ];
    dirs.extend(
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|runtime| runtime.join("model-router")),
    );
    dirs
}

/// Whether `path` has the shape of a socket from `socket_path`, in one of the
/// socket directories `socket_dir` uses: an earlier mr may have had another
/// `XDG_RUNTIME_DIR` than this one, or none. A dead socket is removed and a
/// live one is sent keystrokes, so a saved run must not name any other file;
/// a directory that still exists must be private and owned by this user.
pub fn is_socket_path(path: &Path) -> bool {
    let named = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|name| name.strip_suffix(".sock"))
        .is_some_and(|id| {
            id.len() == 16
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
    let Some(parent) = path.parent() else {
        return false;
    };
    named
        && socket_dirs().iter().any(|dir| dir == parent)
        && (fs::symlink_metadata(parent).is_err() || is_private_dir(parent))
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

/// Takes the locks a client handed over as it let go, in order.
fn receive(frames: &mut Frames, locks: &mut Vec<FileLock>, count: usize) {
    let fds = frames.take_fds();
    if locks.is_empty() && fds.len() == count {
        *locks = fds.into_iter().map(FileLock::from_fd).collect();
    }
}

/// Lets go of a client, first taking locks it handed over that are still
/// unread: dropping the connection would close them and free the locks. A
/// write to an mr that has just closed fails even with its frames queued.
fn part(mut gone: Client, locks: &mut Vec<FileLock>, count: usize) {
    gone.frames.fill(&gone.stream);
    while let Ok(Some((tag, _))) = gone.frames.next() {
        if tag == tag::LOCKS {
            receive(&mut gone.frames, locks, count);
        }
    }
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
    let mut locks: Vec<FileLock> = Vec::new();
    let mut stopping: Option<Instant> = None;
    let status = loop {
        // The attached mr first, so one that just let go is gone before a
        // new one asks to attach.
        if let Some(active) = &mut client {
            let mut open = active.frames.fill(&active.stream);
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
                    Ok(Some((tag::LOCKS, _))) => {
                        receive(&mut active.frames, &mut locks, spec.locks.len());
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
                if let Some(gone) = client.take() {
                    part(gone, &mut locks, spec.locks.len());
                }
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
                if let Some(gone) = client.take_if(|active| !active.frames.fill(&active.stream)) {
                    part(gone, &mut locks, spec.locks.len());
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
                // A client never sends descriptors before attaching.
                drop(frames.take_fds());
                let mut written = 0;
                if !locks.is_empty() {
                    // The attached mr takes the locks over, sent with the reply.
                    let fds: Vec<_> = locks.iter().map(|lock| lock.fd().as_raw_fd()).collect();
                    match send(&stream, &out, &fds) {
                        Ok(n) if n > 0 => {
                            for lock in locks.drain(..) {
                                lock.hand_over();
                            }
                            written = n;
                        }
                        // Keep them; that mr sees its connection close.
                        _ => continue,
                    }
                }
                client = Some(Client {
                    stream,
                    frames,
                    out,
                    written,
                });
                state.attached = true;
                state.redraw = true;
                state.output.clear();
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
            // Only after an mr vanished without handing its locks over. All
            // or none, in mr's order; retried until that mr's locks are free.
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
    // Let go before the socket goes, so an mr that sees it gone can lock.
    drop(locks);
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
    fn more_descriptors_than_a_frame_holds_are_all_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        let fds = vec![file.as_raw_fd(); 10];
        let mut out = Vec::new();
        frame(tag::LOCKS, &[], &mut out);
        assert!(send(&a, &out, &fds).unwrap() > 0);
        let mut frames = Frames::default();
        assert!(frames.read(&b).unwrap() > 0);
        // Cut short by the kernel, they are never a valid handover; macOS
        // also reports the full length, past what it delivered.
        assert!(frames.take_fds().is_empty());
        assert_eq!(frames.next().unwrap().unwrap().0, tag::LOCKS);
    }

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
    fn only_sockets_in_a_socket_directory_are_recognized() {
        let socket = socket_path().unwrap();
        assert!(is_socket_path(&socket));
        // The same shape anywhere else, e.g. in a workspace a stage writes.
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("model-router");
        fs::create_dir(&elsewhere).unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!is_socket_path(
            &elsewhere.join(socket.file_name().unwrap())
        ));
        assert!(!is_socket_path(&socket.with_file_name("notes.txt")));
        // A link at a socket directory's name is not one, wherever it leads:
        // the sandbox hides socket directories, and must not hide its target.
        assert!(is_private_dir(&elsewhere));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(!is_private_dir(&link));
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!is_private_dir(&elsewhere));
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
