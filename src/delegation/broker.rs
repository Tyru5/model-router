//! The per-turn delegation server inside the driving `mr`: a private Unix
//! socket that `mr __mcp-bridge` (started by the harness) connects to with
//! the turn's token, then speaks MCP over.
use crate::serve::mcp::{self, ToolSet};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

pub const SOCK_VAR: &str = "MODEL_ROUTER_ORCH_SOCK";
pub const TOKEN_VAR: &str = "MODEL_ROUTER_ORCH_TOKEN_FILE";
/// What the bridge program's argument list starts with.
pub const BRIDGE_ARG: &str = "__mcp-bridge";
/// Socket file names: `orch-<16 hex>.sock`.
const NAME_LEN: usize = "orch-0123456789abcdef.sock".len();

/// The private directory delegation sockets and token files live in,
/// beside the terminal host's but never one of its directories (those stay
/// hidden from every stage). Socket paths are limited to about 100 bytes.
pub fn orch_socket_dir() -> Result<PathBuf> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let candidate = match std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
    {
        Some(runtime) => runtime.join("model-router-orch"),
        None => std::env::temp_dir().join(format!("model-router-orch-{uid}")),
    };
    let dir = if candidate.as_os_str().len() + 1 + NAME_LEN > 100 {
        PathBuf::from(format!("/tmp/model-router-orch-{uid}"))
    } else {
        candidate
    };
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("Create {}", dir.display())),
    }
    anyhow::ensure!(
        crate::process::host::is_private_dir(&dir),
        "{} must be a private directory owned by you",
        dir.display()
    );
    Ok(dir)
}

/// Every location `orch_socket_dir` uses or would use with another
/// environment: hidden from every sandboxed stage, so no stage reaches
/// another turn's socket or token.
pub fn orch_socket_dirs() -> Vec<PathBuf> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut dirs = vec![
        std::env::temp_dir().join(format!("model-router-orch-{uid}")),
        PathBuf::from(format!("/tmp/model-router-orch-{uid}")),
        PathBuf::from(format!("/run/user/{uid}/model-router-orch")),
    ];
    dirs.extend(
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|runtime| runtime.join("model-router-orch")),
    );
    dirs
}

/// 32 bytes from the OS RNG, hex-encoded.
fn new_token() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("The OS random number generator failed"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time comparison, so a wrong token reveals nothing by timing.
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One turn's socket and token file, and what a launch needs to reach them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub sock: PathBuf,
    pub token_file: PathBuf,
    /// The bridge program: this `mr` (or a test's stand-in).
    pub program: PathBuf,
}

/// The program `mr __mcp-bridge` runs as. Tests set
/// `MODEL_ROUTER_BRIDGE_PROGRAM`, since their binary is the test harness.
pub fn bridge_program() -> Result<PathBuf> {
    if let Some(program) = std::env::var_os("MODEL_ROUTER_BRIDGE_PROGRAM") {
        return Ok(PathBuf::from(program));
    }
    std::env::current_exe().context("Find the model-router executable for the delegation bridge")
}

/// The running server of one turn. Dropping it closes it.
pub struct Broker {
    pub endpoint: Endpoint,
    close: Arc<AtomicBool>,
    wake: Option<UnixStream>,
    active: Arc<Mutex<Option<UnixStream>>>,
    thread: Option<JoinHandle<()>>,
}

impl Broker {
    /// Binds a new socket and writes a new token for one turn, then serves
    /// `tools` to the bridge that presents the token.
    pub fn start<T: ToolSet>(tools: Arc<T>, close: Arc<AtomicBool>) -> Result<Self> {
        let dir = orch_socket_dir()?;
        let id = new_token()?;
        let stem = format!("orch-{}", &id[..16]);
        let sock = dir.join(format!("{stem}.sock"));
        let token_file = dir.join(format!("{stem}.token"));
        let token = new_token()?;
        let listener =
            UnixListener::bind(&sock).with_context(|| format!("Bind {}", sock.display()))?;
        fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&token_file)
            .and_then(|mut file| file.write_all(token.as_bytes()));
        if let Err(error) = written {
            let _ = fs::remove_file(&sock);
            bail!("Write {}: {error}", token_file.display());
        }
        let (wake_read, wake) = UnixStream::pair()?;
        let active: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let thread = {
            let (close, active) = (close.clone(), active.clone());
            std::thread::Builder::new()
                .name("delegation broker".into())
                .spawn(move || accept_loop(listener, wake_read, token, tools, close, active))?
        };
        Ok(Self {
            endpoint: Endpoint {
                sock,
                token_file,
                program: bridge_program()?,
            },
            close,
            wake: Some(wake),
            active,
            thread: Some(thread),
        })
    }

    /// Stops accepting, shuts the live connection, waits for every call to
    /// return (their waits watch the close flag), and removes the files.
    pub fn close(&mut self) {
        self.close.store(true, Ordering::SeqCst);
        if let Some(mut wake) = self.wake.take() {
            let _ = wake.write_all(b"x");
        }
        if let Some(stream) = self.active.lock().unwrap_or_else(|e| e.into_inner()).take() {
            end(&stream);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.endpoint.sock);
        let _ = fs::remove_file(&self.endpoint.token_file);
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.close();
    }
}

fn readable(listener: &UnixListener, wake: &UnixStream) -> bool {
    let mut fds = [
        libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: `fds` is a valid array of two pollfd structs.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        return fds[1].revents == 0 && fds[0].revents & libc::POLLIN != 0;
    }
}

fn accept_loop<T: ToolSet>(
    listener: UnixListener,
    wake: UnixStream,
    token: String,
    tools: Arc<T>,
    close: Arc<AtomicBool>,
    active: Arc<Mutex<Option<UnixStream>>>,
) {
    let mut connection: Option<JoinHandle<()>> = None;
    while !close.load(Ordering::SeqCst) && readable(&listener, &wake) {
        let Ok((stream, _)) = listener.accept() else {
            continue;
        };
        if close.load(Ordering::SeqCst) {
            break;
        }
        // One live connection per turn; a new one replaces a closed one (a
        // harness may restart its MCP servers).
        if connection.as_ref().is_some_and(|c| !c.is_finished()) {
            continue;
        }
        if let Some(done) = connection.take() {
            let _ = done.join();
        }
        let Some((reader, output)) = handshake(stream, &token) else {
            continue;
        };
        if let Ok(shutdown) = output.try_clone() {
            *active.lock().unwrap_or_else(|e| e.into_inner()) = Some(shutdown);
        }
        let tools = tools.clone();
        let Ok(ending) = output.try_clone() else {
            continue;
        };
        connection = std::thread::Builder::new()
            .name("delegation session".into())
            .spawn(move || {
                let calls = mcp::serve(tools, reader, output);
                for call in calls {
                    let _ = call.join();
                }
                // The harness closed its end; close ours so the bridge exits.
                end(&ending);
            })
            .ok();
    }
    if let Some(stream) = active.lock().unwrap_or_else(|e| e.into_inner()).take() {
        end(&stream);
    }
    if let Some(connection) = connection {
        let _ = connection.join();
    }
}

/// Shuts both directions of a session socket, one at a time: macOS refuses
/// `Shutdown::Both` (ENOTCONN, writing side left open) once the bridge has
/// shut its own writing side, and the bridge then never sees EOF.
fn end(stream: &UnixStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.shutdown(std::net::Shutdown::Read);
}

/// The bridge's first line is the token; a wrong or slow one is dropped.
/// Returns the session's reader (it may already hold the first requests)
/// and writer.
fn handshake(stream: UnixStream, token: &str) -> Option<(BufReader<UnixStream>, UnixStream)> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = Vec::new();
    (&mut reader).take(256).read_until(b'\n', &mut line).ok()?;
    let presented = line.strip_suffix(b"\n").unwrap_or(&line);
    if !same(presented, token.as_bytes()) {
        return None;
    }
    stream.set_read_timeout(None).ok()?;
    Some((reader, stream))
}

/// Reads a token file written by `Broker::start`.
pub fn read_token(path: &Path) -> Result<String> {
    let mut text = String::new();
    fs::File::open(path)
        .with_context(|| format!("Open {}", path.display()))?
        .take(256)
        .read_to_string(&mut text)?;
    Ok(text.trim().to_owned())
}
