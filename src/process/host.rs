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
    fs,
    io::{self, BufWriter, Read, Write},
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
    /// Report the harness's start time with its PID (see `ready`). False
    /// from an mr built before it could read one: on macOS an upgrade can
    /// leave that mr starting this host from the replaced executable.
    #[serde(default)]
    pub report_started: bool,
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

pub(super) fn frame_slices(tag: u8, first: &[u8], second: &[u8], out: &mut Vec<u8>) {
    let len = first.len() + second.len();
    out.push(tag);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.extend_from_slice(first);
    out.extend_from_slice(second);
}

/// Terminal output as frames mr accepts: a redraw of a large screen can pass
/// `FRAME_LIMIT`, and mr's decoder carries a sequence across frames.
fn output_frames(raw: &[u8], out: &mut Vec<u8>) {
    for chunk in raw.chunks(FRAME_LIMIT) {
        frame(tag::OUTPUT, chunk, out);
    }
}

/// Splits a byte stream into frames, keeping the descriptors it carries.
#[derive(Default)]
pub(super) struct Frames {
    buffer: Vec<u8>,
    pos: usize,
    fds: Vec<OwnedFd>,
}
impl Frames {
    pub(super) fn unread(&self) -> &[u8] {
        &self.buffer[self.pos..]
    }
    fn unread_len(&self) -> usize {
        self.buffer.len() - self.pos
    }
    pub(super) fn has_room(&self) -> bool {
        self.unread_len() <= 4 * FRAME_LIMIT
    }
    /// One read; 0 at end of stream.
    pub fn read(&mut self, stream: &UnixStream) -> io::Result<usize> {
        let mut bytes = [0u8; 16 * 1024];
        let n = recv(stream, &mut bytes, &mut self.fds)?;
        self.buffer.extend_from_slice(&bytes[..n]);
        // Only one frame carries descriptors; never pile up a peer's extras.
        self.fds.truncate(MAX_FDS);
        Ok(n)
    }
    /// Reads what is available without blocking, leaving the rest in the
    /// socket once enough is buffered, so a peer that sends faster than the
    /// frames are used waits. False at end of stream or on an error that
    /// ends the connection.
    pub fn fill(&mut self, stream: &UnixStream) -> bool {
        while self.has_room() {
            match self.read(stream) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        true
    }
    /// The tag of the next frame, whole or not.
    fn peek(&self) -> Option<u8> {
        self.unread().first().copied()
    }
    pub fn next(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        let unread = self.unread();
        if unread.len() < 5 {
            return Ok(None);
        }
        let length = u32::from_be_bytes(unread[1..5].try_into().expect("four bytes")) as usize;
        ensure!(length <= FRAME_LIMIT, "Terminal host stream is corrupt");
        if unread.len() < 5 + length {
            return Ok(None);
        }
        let tag = unread[0];
        let payload = unread[5..5 + length].to_vec();
        self.pos += 5 + length;
        if self.pos == self.buffer.len() {
            self.buffer.clear();
            self.pos = 0;
            if self.buffer.capacity() > 256 * 1024 {
                self.buffer.shrink_to(64 * 1024);
            }
        } else if self.pos >= 64 * 1024 && self.pos * 2 >= self.buffer.len() {
            self.buffer.copy_within(self.pos.., 0);
            self.buffer.truncate(self.buffer.len() - self.pos);
            self.pos = 0;
        }
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
    let candidate = match std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
    {
        Some(runtime) => runtime.join("model-router"),
        None => std::env::temp_dir().join(format!("model-router-{uid}")),
    };
    let dir = if candidate.join("0123456789abcdef.sock").as_os_str().len() > 100 {
        PathBuf::from(format!("/tmp/model-router-{uid}"))
    } else {
        candidate
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
/// A private scratch directory inside `socket_dir`, which sandboxed stages
/// can neither write nor (under bubblewrap) see, for files the router trusts
/// while stages run: the sanitized Git directory and the staged capture
/// script. The default temp directory is writable by Seatbelt stages.
pub(crate) fn private_temp_dir() -> Result<PathBuf> {
    let dir = socket_dir()?.join("tmp");
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("Create {}", dir.display())),
    }
    ensure!(
        is_private_dir(&dir),
        "{} must be a private directory owned by you",
        dir.display()
    );
    Ok(dir)
}

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

fn read_ready_line(
    stdout: &mut std::process::ChildStdout,
    timeout: Duration,
    dir: &Path,
) -> Result<String> {
    nonblocking(stdout.as_raw_fd())?;
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::with_capacity(128);
    let mut chunk = [0u8; 512];
    'read: loop {
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break 'read,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.contains(&b'\n') || buf.len() >= 4096 {
                        break 'read;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "The terminal host timed out starting the harness; see {}",
                dir.join("terminal-host.log").display()
            );
        }
        let wait_ms = remaining.as_millis().clamp(1, 50) as libc::c_int;
        let mut pfd = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: stdout is borrowed and outlives the poll call.
        unsafe { libc::poll(&mut pfd, 1, wait_ms) };
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Starts a host for `spec` and returns the harness's process group once it
/// runs: its ID (the leader's PID) and the leader's start time, which the
/// host reads before it can reap the leader (see `ready`).
pub(super) fn spawn(spec: &Spec) -> Result<(u32, Option<String>)> {
    #[cfg(test)]
    if PROGRAM.get().is_none() {
        return spawn_thread(spec);
    }
    struct KillOnDrop(Option<std::process::Child>);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let log = crate::storage::create_fresh(&spec.dir.join("terminal-host.log"))?;
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
    let mut guard = KillOnDrop(Some(command.spawn().context("Start the terminal host")?));
    let child = guard.0.as_mut().expect("spawned child");
    let mut stdin = child.stdin.take().context("Terminal host stdin")?;
    stdin.write_all(&serde_json::to_vec(spec)?)?;
    drop(stdin);
    let mut stdout = child.stdout.take().context("Terminal host stdout")?;
    let line = read_ready_line(&mut stdout, Duration::from_secs(10), &spec.dir)?;
    if let Some(mut child) = guard.0.take() {
        // mr reaps the host while it lives; after mr exits, init does.
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
    ready(&line, &spec.dir)
}

/// Reads the host's one-line report: `ok PID STARTED` once the harness runs,
/// where STARTED is `group::started` for it, read by the host before it can
/// reap the harness and its PID be reused; `ok PID` when the host could not
/// read it (the group is then identified by its marker alone); `error TEXT`
/// when the harness did not start.
fn ready(line: &str, dir: &Path) -> Result<(u32, Option<String>)> {
    let line = line.trim_end();
    if let Some(rest) = line.strip_prefix("ok ") {
        let (pid, started) = match rest.split_once(' ') {
            Some((pid, started)) => (pid, Some(started.to_owned())),
            None => (rest, None),
        };
        let pid = pid
            .parse()
            .context("Terminal host reported a bad process ID")?;
        return Ok((pid, started.filter(|s| !s.is_empty())));
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
fn spawn_thread(spec: &Spec) -> Result<(u32, Option<String>)> {
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

/// Lets go of a client, first taking locks and stop signals it sent before
/// closing: dropping the connection would close unread lock descriptors and
/// drop a queued STOP behind a large paste.
fn part(
    mut gone: Client,
    locks: &mut Vec<FileLock>,
    count: usize,
    pid: u32,
    stopping: &mut Option<Instant>,
) {
    for _ in 0..64 {
        let open = gone.frames.fill(&gone.stream);
        let mut drained = false;
        while let Ok(Some((tag, _))) = gone.frames.next() {
            drained = true;
            match tag {
                tag::LOCKS => receive(&mut gone.frames, locks, count),
                tag::STOP if stopping.is_none() => {
                    signal_group(pid, libc::SIGTERM);
                    *stopping = Some(Instant::now() + GRACE);
                }
                _ => {}
            }
        }
        if !open || !drained {
            break;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AttachCheck {
    Incomplete,
    Ready(bool),
    Invalid,
}

/// Whether `buffer` starts with a whole attach request, and if so whether it
/// asks for the harness's start time with its PID: payload `[1]`. An mr
/// built before it could read one sends an empty payload, and a host built
/// before then ignores the payload and answers with the PID alone.
fn attach_request(buffer: &[u8]) -> AttachCheck {
    let Some(&first) = buffer.first() else {
        return AttachCheck::Incomplete;
    };
    if first != tag::ATTACH {
        return AttachCheck::Invalid;
    }
    let Some(header) = buffer.get(1..5) else {
        return AttachCheck::Incomplete;
    };
    let len = u32::from_be_bytes(header.try_into().expect("four bytes")) as usize;
    if len > FRAME_LIMIT {
        return AttachCheck::Invalid;
    }
    if buffer.len() < 5 + len {
        return AttachCheck::Incomplete;
    }
    // Larger payloads within FRAME_LIMIT still attach as an older/unknown mr.
    AttachCheck::Ready(len == 1 && buffer.get(5) == Some(&1))
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
        let transcript =
            BufWriter::new(crate::storage::create_fresh(&spec.dir.join("stdout.log"))?);
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
        // Now, while the harness cannot have been reaped: this host reaps it,
        // and once it has, its PID may name another process.
        let leader = super::group::started(pid);
        Ok((
            pair.master,
            fd,
            reader,
            writer,
            transcript,
            spawned,
            pid,
            leader,
        ))
    })();
    let (master, fd, mut reader, mut writer, mut transcript, spawned, pid, leader) = match started {
        Ok(started) => started,
        Err(error) => {
            ready(format!("error {error:#}\n"));
            return Err(error);
        }
    };
    let mut child = PtyChild {
        child: spawned,
        pid: Some(pid),
        started: leader.clone(),
        leader_reaped: false,
        killed: false,
    };
    ready(match (&leader, spec.report_started) {
        (Some(leader), true) => format!("ok {pid} {leader}\n"),
        _ => format!("ok {pid}\n"),
    });

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
    let mut raw = Vec::new();
    let mut next_lock_try = Instant::now();
    let status = loop {
        // The attached mr first, so one that just let go is gone before a
        // new one asks to attach.
        if let Some(active) = &mut client {
            let mut open = active.frames.fill(&active.stream);
            let mut gone = false;
            loop {
                loop {
                    // Input the harness has not read yet stays with mr; dropping
                    // it would cut a paste short.
                    if state.input.len() >= INPUT_LIMIT && active.frames.peek() == Some(tag::INPUT)
                    {
                        break;
                    }
                    match active.frames.next() {
                        Ok(Some((tag::INPUT, payload))) => state.input.extend(&payload),
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
                if gone {
                    break;
                }
                if open && active.written == active.out.len() {
                    state.take_output(&mut raw);
                    output_frames(&raw, &mut active.out);
                }
                if open && flush(&mut active.stream, &mut active.out, &mut active.written) {
                    break;
                }
                // Gone, perhaps while this wrote: what it sent before
                // closing, such as STOP, still counts.
                gone = true;
                active.frames.fill(&active.stream);
            }
            if gone {
                if let Some(gone) = client.take() {
                    part(gone, &mut locks, spec.locks.len(), pid, &mut stopping);
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
            match attach_request(frames.unread()) {
                AttachCheck::Ready(report_started) => {
                    let (mut stream, mut frames) = pending.remove(index);
                    if frames.next().is_err() {
                        continue;
                    }
                    let mut out = Vec::new();
                    // The attached mr may have let go since it was last read.
                    if let Some(gone) = client.take_if(|active| !active.frames.fill(&active.stream))
                    {
                        part(gone, &mut locks, spec.locks.len(), pid, &mut stopping);
                        state.attached = false;
                        state.output.clear();
                    }
                    if client.is_some() {
                        // Never take a live session from the mr attached to it.
                        frame(tag::BUSY, &[], &mut out);
                        let _ = stream.write(&out);
                        continue;
                    }
                    let mut spawned = pid.to_be_bytes().to_vec();
                    if let (true, Some(leader)) = (report_started, &leader) {
                        spawned.extend_from_slice(leader.as_bytes());
                    }
                    frame(tag::SPAWNED, &spawned, &mut out);
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
                }
                AttachCheck::Invalid => {
                    pending.remove(index);
                }
                AttachCheck::Incomplete if !open => {
                    pending.remove(index);
                }
                AttachCheck::Incomplete => {
                    index += 1;
                }
            }
        }
        if client.is_none()
            && locks.is_empty()
            && !spec.locks.is_empty()
            && Instant::now() >= next_lock_try
        {
            next_lock_try = Instant::now() + Duration::from_millis(100);
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
            let _ = master.resize(PtySize {
                cols: size.0,
                rows: size.1,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        while !state.input.is_empty() {
            let (front, _) = state.input.as_slices();
            match writer.write(front) {
                Ok(0) => {
                    state.input.clear();
                    break;
                }
                Ok(n) => {
                    state.input.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
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
            32,
        )?;
        if let Some(status) = child.child.try_wait()? {
            child.leader_reaped = true;
            if stopping.is_some() {
                kill_group(pid);
                child.killed = true;
            }
            break status;
        }
        if stopping.is_some_and(|deadline| Instant::now() >= deadline) {
            kill_group(pid);
            let _ = child.child.kill();
            let status = child.child.wait()?;
            child.leader_reaped = true;
            child.killed = true;
            break status;
        }
        let mut polls = vec![
            libc::pollfd {
                fd: if eof { -1 } else { fd },
                events: libc::POLLIN
                    | if state.input.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    },
                revents: 0,
            },
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if let Some(active) = &client {
            let read_client = active.frames.has_room()
                && !(state.input.len() >= INPUT_LIMIT && active.frames.peek() == Some(tag::INPUT));
            let mut events = 0;
            if read_client {
                events |= libc::POLLIN;
            }
            if active.written < active.out.len() {
                events |= libc::POLLOUT;
            }
            polls.push(libc::pollfd {
                fd: if events == 0 {
                    -1
                } else {
                    active.stream.as_raw_fd()
                },
                events,
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
        // Output read this pass goes to an idle client now, not after a timeout.
        let waiting = client
            .as_ref()
            .is_some_and(|active| active.written == active.out.len())
            && (state.redraw || !state.output.is_empty() || !state.queries.is_empty());
        let timeout = if waiting { 0 } else { 40 };
        // SAFETY: every descriptor is borrowed and outlives the call.
        unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout) };
    };
    if !eof {
        drain_pty(
            &mut reader,
            &mut transcript,
            &mut decoder,
            &mut |frame| state.output(frame),
            &mut bytes,
            &mut eof,
            1024,
        )?;
    }
    // No descendant outlives the harness, as when mr owned it.
    drop(child);
    if !eof {
        drain_pty(
            &mut reader,
            &mut transcript,
            &mut decoder,
            &mut |frame| state.output(frame),
            &mut bytes,
            &mut eof,
            1024,
        )?;
    }
    transcript
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?
        .sync_all()?;
    let exit = Exit {
        code: status.signal().is_none().then(|| status.exit_code() as i32),
        cancelled: stopping.is_some(),
    };
    crate::storage::write_json(&spec.dir.join(EXIT_FILE), &exit)?;
    // Let go before the socket goes, so an mr that sees it gone can lock.
    drop(locks);
    // Nobody can attach to a finished harness; the record says how it ended.
    let _ = fs::remove_file(&spec.socket);
    if let Some(mut active) = client.take() {
        state.take_output(&mut raw);
        output_frames(&raw, &mut active.out);
        frame(tag::EXIT, &serde_json::to_vec(&exit)?, &mut active.out);
        let deadline = Instant::now() + Duration::from_secs(2);
        while flush(&mut active.stream, &mut active.out, &mut active.written)
            && !active.out.is_empty()
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let wait_ms = remaining.as_millis().clamp(1, 50) as libc::c_int;
            let mut pfd = libc::pollfd {
                fd: active.stream.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: active.stream is open and borrowed for this poll call.
            unsafe { libc::poll(&mut pfd, 1, wait_ms) };
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

    fn spec(dir: &Path, script: &str) -> Spec {
        Spec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: dir.to_path_buf(),
            dir: dir.to_path_buf(),
            env: vec![],
            removed: vec![],
            cols: 80,
            rows: 24,
            socket: socket_path().unwrap(),
            locks: vec![],
            warning: None,
            report_started: true,
        }
    }

    fn await_exit(dir: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dir.join(EXIT_FILE).exists() {
            assert!(
                Instant::now() < deadline,
                "the host never recorded the exit"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_host_reports_the_harness_start_time_before_it_can_reap_it() {
        // A running harness: the reported start time is its own.
        let dir = tempfile::tempdir().unwrap();
        let (pid, started) = spawn(&spec(dir.path(), "sleep 30")).unwrap();
        assert!(started.is_some(), "start time unreadable");
        assert_eq!(super::super::group::started(pid), started);
        super::super::end_group(pid, Duration::from_millis(200), || {});
        await_exit(dir.path());
        // One that exits at once: the host has reaped it, so its PID may name
        // another process by now, yet the start time was read while it was
        // still the harness.
        let dir = tempfile::tempdir().unwrap();
        let (_, started) = spawn(&spec(dir.path(), "exit 0")).unwrap();
        await_exit(dir.path());
        assert!(started.is_some(), "start time read after the harness ended");
    }

    #[test]
    fn the_host_reports_a_start_time_only_to_an_mr_that_asks() {
        // An mr built before start times were reported: the PID alone, in
        // the readiness line and in the attach reply.
        let dir = tempfile::tempdir().unwrap();
        let mut old = spec(dir.path(), "sleep 30");
        old.report_started = false;
        let socket = old.socket.clone();
        let (pid, started) = spawn(&old).unwrap();
        assert_eq!(started, None);
        let mut stream = UnixStream::connect(&socket).unwrap();
        let mut out = Vec::new();
        frame(tag::ATTACH, &[], &mut out);
        stream.write_all(&out).unwrap();
        let mut frames = Frames::default();
        let reply = loop {
            if let Some(reply) = frames.next().unwrap() {
                break reply;
            }
            assert!(frames.read(&stream).unwrap() > 0);
        };
        assert_eq!(reply, (tag::SPAWNED, pid.to_be_bytes().to_vec()));
        drop(stream);
        // This build's mr asks, and the attach reply carries what the host
        // read at launch.
        let terminal = super::super::Terminal::connect(&socket).unwrap();
        assert_eq!(terminal.pid(), pid);
        assert!(terminal.started().is_some());
        assert_eq!(terminal.started(), super::super::group::started(pid));
        drop(terminal);
        super::super::end_group(pid, Duration::from_millis(200), || {});
        await_exit(dir.path());
    }

    #[test]
    fn a_redraw_larger_than_a_frame_reaches_mr_whole() {
        // A truecolour screen whose redraw is well past FRAME_LIMIT.
        let dir = tempfile::tempdir().unwrap();
        let drawn = dir.path().join("drawn");
        let mut big = spec(
            dir.path(),
            r#"awk 'BEGIN { for (i = 0; i < 40000; i++) printf "\033[38;2;%d;%d;%dm\033[48;2;%d;%d;%dmx", i % 256, int(i / 256) % 256, (i * 7) % 256, (i * 3) % 256, (i * 5) % 256, (i * 11) % 256 }'; touch "$0"; sleep 2"#,
        );
        big.args.push(drawn.display().to_string());
        (big.cols, big.rows) = (400, 100);
        let socket = big.socket.clone();
        spawn(&big).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !drawn.exists() {
            assert!(Instant::now() < deadline, "the screen was never drawn");
            thread::sleep(Duration::from_millis(20));
        }
        // Attaching asks for a redraw of the whole screen.
        let terminal = super::super::Terminal::connect(&socket).unwrap();
        let session = super::super::InteractiveSession::new(400, 100);
        let outcome = terminal
            .supervise(
                dir.path(),
                &std::sync::atomic::AtomicBool::new(false),
                &session,
            )
            .unwrap();
        assert_eq!(outcome.code, Some(0));
    }

    #[test]
    fn a_paste_larger_than_the_input_queue_reaches_the_harness_whole() {
        let dir = tempfile::tempdir().unwrap();
        let (ready, received) = (dir.path().join("ready"), dir.path().join("received"));
        let mut cat = spec(dir.path(), r#"stty raw -echo; touch "$0"; exec cat > "$1""#);
        cat.args.push(ready.display().to_string());
        cat.args.push(received.display().to_string());
        let socket = cat.socket.clone();
        let (pid, _) = spawn(&cat).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "the harness never started");
            thread::sleep(Duration::from_millis(20));
        }
        let mut stream = UnixStream::connect(&socket).unwrap();
        let mut out = Vec::new();
        frame(tag::ATTACH, &[], &mut out);
        // 16 times the host's input queue, faster than a PTY takes it.
        let paste: Vec<u8> = (0..16 * INPUT_LIMIT)
            .map(|i| b'a' + (i % 26) as u8)
            .collect();
        for chunk in paste.chunks(16 * 1024) {
            frame(tag::INPUT, chunk, &mut out);
        }
        stream.write_all(&out).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while fs::metadata(&received).map_or(0, |m| m.len()) < paste.len() as u64 {
            assert!(
                Instant::now() < deadline,
                "the harness received {} of {} bytes",
                fs::metadata(&received).map_or(0, |m| m.len()),
                paste.len()
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            fs::read(&received).unwrap() == paste,
            "the paste arrived changed"
        );
        super::super::end_group(pid, Duration::from_millis(200), || {});
        await_exit(dir.path());
    }

    #[test]
    fn attach_requests_are_read_whole() {
        let mut out = Vec::new();
        frame(tag::ATTACH, &[], &mut out);
        assert_eq!(attach_request(&out), AttachCheck::Ready(false));
        assert_eq!(attach_request(&out[..3]), AttachCheck::Incomplete);
        out.clear();
        frame(tag::ATTACH, &[1], &mut out);
        assert_eq!(attach_request(&out), AttachCheck::Ready(true));
        assert_eq!(attach_request(&out[..5]), AttachCheck::Incomplete);
        out.clear();
        frame(tag::INPUT, &[1], &mut out);
        assert_eq!(attach_request(&out), AttachCheck::Invalid);
        // Oversized attach frame is rejected immediately instead of attaching.
        let mut huge = vec![tag::ATTACH];
        huge.extend_from_slice(&((FRAME_LIMIT + 1) as u32).to_be_bytes());
        assert_eq!(attach_request(&huge), AttachCheck::Invalid);
    }

    #[test]
    fn frames_cursor_compacts_across_many_small_frames() {
        let mut frames = Frames::default();
        for i in 0..2000u32 {
            frame(tag::INPUT, &i.to_be_bytes(), &mut frames.buffer);
        }
        for i in 0..2000u32 {
            let (tag, payload) = frames.next().unwrap().unwrap();
            assert_eq!(tag, tag::INPUT);
            assert_eq!(payload, i.to_be_bytes());
        }
        assert!(frames.next().unwrap().is_none());
        assert!(frames.unread().is_empty());
    }

    #[test]
    fn signal_terminated_harness_records_no_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path(), "kill -KILL $$");
        let socket = spec.socket.clone();
        spawn(&spec).unwrap();
        await_exit(dir.path());
        let deadline = Instant::now() + Duration::from_secs(5);
        while socket.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let exit = recorded(dir.path()).unwrap();
        assert_eq!(exit.code, None);
    }

    #[test]
    fn host_readiness_reports_errors() {
        let dir = Path::new("/nowhere");
        assert_eq!(
            ready("ok 42 1700000000.000123\n", dir).unwrap(),
            (42, Some("1700000000.000123".into()))
        );
        // A host that could not read the start time reports the ID alone.
        assert_eq!(ready("ok 42\n", dir).unwrap(), (42, None));
        assert!(ready("ok forty-two\n", dir).is_err());
        assert!(
            ready("error No such program\n", dir)
                .unwrap_err()
                .to_string()
                .contains("No such program")
        );
        assert!(ready("", dir).is_err());
    }
}
