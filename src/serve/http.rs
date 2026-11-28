//! A small JSON HTTP API over std::net: one thread per connection,
//! `Connection: close`, bearer-token auth on every route except `GET /health`.
//!
//! - `GET /health` returns liveness and version JSON without authentication.
//! - `GET /tools` lists the tools.
//! - `POST /tools/<name>` with a JSON arguments object runs one.
//! - `GET /runs/<id>/events[?project=DIR]` streams a run's events (SSE).
//! - `POST /mcp` speaks MCP's Streamable HTTP transport (JSON responses).

use super::{Server, mcp, runs::Message, tools};
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::RecvTimeoutError,
    },
    time::{Duration, Instant},
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const KEEPALIVE: Duration = Duration::from_secs(15);
/// Connections still sending their request, before any authentication.
const MAX_PENDING: usize = 16;
/// Authenticated requests and event streams.
const MAX_CONNECTIONS: usize = 32;
/// Authenticated long-lived streams/waits (`events` and `wait_run`), leaving
/// quick requests (`cancel_run`, `run_status`) runnable.
const MAX_LONG_CONNECTIONS: usize = 24;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a pending connection is safe from being cut off: long enough
/// for its worker to read headers that arrived with it.
const PENDING_GRACE: Duration = Duration::from_millis(500);

/// Connections that have not yet sent authenticated headers. Anyone who can
/// reach the port can open one and send nothing, so they never share a
/// budget with authenticated clients, and when the queue is full the oldest
/// is cut off rather than refusing the newcomer: idle sockets cannot lock
/// out a client that sends its headers promptly. Only a connection pending
/// longer than `PENDING_GRACE` is cut off; until then newcomers wait in the
/// listen backlog, so a burst accepted faster than workers run is not cut
/// off with its headers unread. A connection leaves the queue as soon as its
/// headers authenticate, before its body is read.
#[derive(Default)]
struct Pending {
    next: u64,
    streams: VecDeque<(u64, Instant, TcpStream)>,
}

/// One connection's place in admission.
struct Admission {
    pending: Arc<Mutex<Pending>>,
    id: u64,
    active: Arc<AtomicUsize>,
    long: Arc<AtomicUsize>,
    /// Whether it was still queued when it first left.
    left: Cell<Option<bool>>,
}

fn reserve_slot(counter: &Arc<AtomicUsize>, max: usize) -> Option<ConnectionSlot> {
    // A compare-exchange loop: `fetch_update` is deprecated on newer
    // toolchains and its replacement is newer than the MSRV.
    let mut n = counter.load(Ordering::Acquire);
    loop {
        if n >= max {
            return None;
        }
        match counter.compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(ConnectionSlot(counter.clone())),
            Err(current) => n = current,
        }
    }
}

impl Admission {
    /// Leaves the pending queue; false when this connection was cut off
    /// for a newer one. Later calls repeat the first answer.
    fn read(&self) -> bool {
        if let Some(stayed) = self.left.get() {
            return stayed;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let before = pending.streams.len();
        pending.streams.retain(|(id, _, _)| *id != self.id);
        let stayed = pending.streams.len() < before;
        self.left.set(Some(stayed));
        stayed
    }

    /// An authenticated slot, or None when all are in use.
    fn authenticated(&self) -> Option<ConnectionSlot> {
        reserve_slot(&self.active, MAX_CONNECTIONS)
    }

    /// A slot for a long-lived call (`wait_run` or `/events`), capped below
    /// `MAX_CONNECTIONS` so short calls are never starved.
    fn long_lived(&self) -> Option<ConnectionSlot> {
        reserve_slot(&self.long, MAX_LONG_CONNECTIONS)
    }
}

struct ConnectionSlot(Arc<AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A random bearer token: 32 bytes from the OS RNG, hex-encoded.
pub fn new_token() -> anyhow::Result<String> {
    crate::trust::random_hex_token()
}

fn wait_for_listener(listener: &TcpListener) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd references an open listener descriptor for this thread.
        let _ = unsafe { libc::poll(&mut pfd, 1, 200) };
    }
    #[cfg(not(unix))]
    {
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn serve(listener: TcpListener, server: Arc<Server>, token: String, stop: Arc<AtomicBool>) {
    if let Err(error) = listener.set_nonblocking(true) {
        super::log(format!("HTTP listener: {error}"));
        return;
    }
    let token = Arc::new(token);
    let active = Arc::new(AtomicUsize::new(0));
    let long = Arc::new(AtomicUsize::new(0));
    let pending = Arc::new(Mutex::new(Pending::default()));
    let calls: Arc<McpCalls> = Arc::default();
    while !stop.load(Ordering::Relaxed) {
        let wait = {
            let queue = pending.lock().unwrap_or_else(|e| e.into_inner());
            queue.streams.len() >= MAX_PENDING
                && queue
                    .streams
                    .front()
                    .is_some_and(|(_, at, _)| at.elapsed() < PENDING_GRACE)
        };
        if wait {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let Ok(handle) = stream.try_clone() else {
                    continue;
                };
                // Workers are bounded by the queue: a cut-off connection's
                // read fails at once and its worker ends.
                let id = {
                    let mut queue = pending.lock().unwrap_or_else(|e| e.into_inner());
                    if queue.streams.len() >= MAX_PENDING {
                        if let Some((_, _, oldest)) = queue.streams.pop_front() {
                            let _ = oldest.shutdown(Shutdown::Both);
                        }
                    }
                    queue.next += 1;
                    let id = queue.next;
                    queue.streams.push_back((id, Instant::now(), handle));
                    id
                };
                let admission = Admission {
                    pending: pending.clone(),
                    id,
                    active: active.clone(),
                    long: long.clone(),
                    left: Cell::new(None),
                };
                let (server, token, stop, calls) =
                    (server.clone(), token.clone(), stop.clone(), calls.clone());
                let spawned = std::thread::Builder::new().spawn(move || {
                    if let Err(error) =
                        connection(stream, &server, &token, &admission, &calls, &stop)
                    {
                        if error.kind() != io::ErrorKind::BrokenPipe {
                            super::log(format!("HTTP connection: {error}"));
                        }
                    }
                    // Also when setting up the connection failed early.
                    admission.read();
                });
                if let Err(error) = spawned {
                    let mut queue = pending.lock().unwrap_or_else(|e| e.into_inner());
                    queue.streams.retain(|(queued, _, _)| *queued != id);
                    super::log(format!("HTTP worker: {error}"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_for_listener(&listener);
            }
            Err(error) => {
                super::log(format!("HTTP accept: {error}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    authorization: Option<String>,
    origin: Option<String>,
    protocol_version: Option<String>,
    body: Vec<u8>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

struct RequestReader<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for RequestReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Request deadline exceeded",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buffer)
    }
}

/// Reads up to `\n` into `out`, failing once stored bytes would exceed `max_bytes`.
fn read_http_line(
    reader: &mut impl BufRead,
    out: &mut String,
    max_bytes: usize,
) -> io::Result<usize> {
    out.clear();
    let mut raw = Vec::new();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let take = buf
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |pos| pos + 1);
        if raw.len() + take > max_bytes {
            return Err(invalid("Headers too large or truncated"));
        }
        let done = buf[take - 1] == b'\n';
        raw.extend_from_slice(&buf[..take]);
        reader.consume(take);
        if done {
            break;
        }
    }
    let text = String::from_utf8(raw).map_err(|_| invalid("Headers must be UTF-8"))?;
    let total = text.len();
    *out = text;
    Ok(total)
}

/// Calls `admit` once the headers authenticate, before reading the body;
/// when it returns false (cut off, or no authenticated slot) reading stops.
fn read_request(
    stream: &TcpStream,
    token: &str,
    timeout: Duration,
    admit: impl FnOnce() -> bool,
) -> io::Result<Request> {
    let source = RequestReader {
        stream,
        deadline: Instant::now() + timeout,
    };
    let mut reader = BufReader::new(source.take((MAX_HEADER_BYTES + MAX_BODY_BYTES) as u64));
    let mut line = String::new();
    let mut header_bytes = read_http_line(&mut reader, &mut line, MAX_HEADER_BYTES)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(invalid("Malformed request line"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut request = Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        authorization: None,
        origin: None,
        protocol_version: None,
        body: vec![],
    };
    let mut length = 0usize;
    loop {
        let remaining = MAX_HEADER_BYTES.saturating_sub(header_bytes);
        let read = read_http_line(&mut reader, &mut line, remaining)?;
        header_bytes += read;
        if read == 0 || header_bytes > MAX_HEADER_BYTES {
            return Err(invalid("Headers too large or truncated"));
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => {
                length = value
                    .parse()
                    .map_err(|_| invalid("Invalid Content-Length"))?;
            }
            "authorization" => request.authorization = Some(value.to_owned()),
            "origin" => request.origin = Some(value.to_owned()),
            "mcp-protocol-version" => request.protocol_version = Some(value.to_owned()),
            "transfer-encoding" => return Err(invalid("Chunked bodies are not supported")),
            _ => {}
        }
    }
    if length > MAX_BODY_BYTES {
        return Err(invalid("Body too large"));
    }
    // Never wait for an unauthenticated client's declared body, and
    // `GET /health` takes no body and answers without auth.
    if is_health(&request) || !authorized(&request, token) {
        return Ok(request);
    }
    if !admit() {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "Not admitted",
        ));
    }
    request.body.resize(length, 0);
    reader.read_exact(&mut request.body)?;
    Ok(request)
}

fn is_health(request: &Request) -> bool {
    request.method == "GET" && request.path.trim_matches('/') == "health"
}

/// Constant-time comparison, so a wrong token reveals nothing by timing.
fn same(a: &[u8], b: &[u8]) -> bool {
    crate::trust::constant_time_eq(a, b)
}

fn authorized(request: &Request, token: &str) -> bool {
    request
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|given| same(given.trim().as_bytes(), token.as_bytes()))
}

fn respond(stream: &mut TcpStream, code: u16, body: &Value) -> io::Result<()> {
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{}\r\n{body}",
        body.len(),
        if code == 401 {
            "WWW-Authenticate: Bearer\r\n"
        } else {
            ""
        }
    )?;
    stream.flush()
}

fn connection(
    mut stream: TcpStream,
    server: &Server,
    token: &str,
    admission: &Admission,
    calls: &McpCalls,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    // A client that stops reading an event stream must not pin its thread.
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    // An authenticated slot covers reading the body too; without one, the
    // body is never read.
    let mut slot = None;
    let mut full = false;
    let request = read_request(&stream, token, REQUEST_TIMEOUT, || {
        let queued = admission.read();
        if queued {
            slot = admission.authenticated();
            full = slot.is_none();
        }
        slot.is_some()
    });
    if !admission.read() {
        // Cut off for a newer connection; nobody is listening for an answer.
        return Ok(());
    }
    let request = match request {
        Ok(request) => request,
        Err(_) if full => {
            return respond(
                &mut stream,
                503,
                &json!({"error": "Too many authenticated connections"}),
            );
        }
        Err(error) => return respond(&mut stream, 400, &json!({"error": error.to_string()})),
    };
    if is_health(&request) {
        return respond(
            &mut stream,
            200,
            &json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}),
        );
    }
    if !authorized(&request, token) {
        return respond(
            &mut stream,
            401,
            &json!({"error": "Missing or wrong bearer token"}),
        );
    }
    let Some(_slot) = slot else {
        return respond(
            &mut stream,
            503,
            &json!({"error": "Too many authenticated connections"}),
        );
    };
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["tools"]) => respond(&mut stream, 200, &json!({"tools": tools::list()})),
        ("POST", ["tools", name]) => {
            if !tools::exists(name) {
                return respond(
                    &mut stream,
                    404,
                    &json!({"error": format!("Unknown tool: {name}")}),
                );
            }
            let arguments: Value = if request.body.iter().all(u8::is_ascii_whitespace) {
                json!({})
            } else {
                match serde_json::from_slice(&request.body) {
                    Ok(value) => value,
                    Err(error) => {
                        return respond(
                            &mut stream,
                            400,
                            &json!({"error": format!("Invalid JSON body: {error}")}),
                        );
                    }
                }
            };
            let _long = if tools::blocks(name) {
                let Some(slot) = admission.long_lived() else {
                    return respond(
                        &mut stream,
                        503,
                        &json!({"error": "Too many concurrent long-lived connections"}),
                    );
                };
                Some(slot)
            } else {
                None
            };
            match tools::call_with(server, name, &arguments, Some(stop.as_ref()), &mut |_| {}) {
                Ok(result) => respond(&mut stream, 200, &json!({"result": result})),
                Err(error) => respond(&mut stream, 400, &json!({"error": format!("{error:#}")})),
            }
        }
        ("POST", ["mcp"]) => mcp_post(&mut stream, server, &request, admission, calls, stop),
        ("GET", ["runs", id, "events"]) => {
            let Some(_long) = admission.long_lived() else {
                return respond(
                    &mut stream,
                    503,
                    &json!({"error": "Too many concurrent long-lived connections"}),
                );
            };
            events(stream, server, id, &request.query, stop)
        }
        // A webhook schedule's trigger; the bearer token guards it like every route.
        ("POST", ["schedules", id, "trigger"]) => match server.fire_schedules(Some(id)) {
            Ok(runs) if runs.is_empty() => respond(
                &mut stream,
                409,
                &json!({"error": format!("Schedule {id} is not an approved, enabled webhook schedule")}),
            ),
            Ok(runs) => respond(&mut stream, 200, &json!({"result": {"runs": runs}})),
            Err(error) => respond(
                &mut stream,
                if error.is::<super::LaunchFailed>() {
                    500
                } else {
                    400
                },
                &json!({"error": format!("{error:#}")}),
            ),
        },
        (
            _,
            ["health"]
            | ["tools"]
            | ["tools", _]
            | ["mcp"]
            | ["runs", _, "events"]
            | ["schedules", _, "trigger"],
        ) => respond(&mut stream, 405, &json!({"error": "Method not allowed"})),
        _ => respond(&mut stream, 404, &json!({"error": "Not found"})),
    }
}

/// Whether an `Origin` header names this machine: `http` or `https`, host
/// `localhost`, `127.0.0.1`, or `[::1]`, and an optional numeric port,
/// nothing else. A browser page served elsewhere (DNS rebinding) sends its
/// own origin, so it is refused even when it somehow holds the token.
fn loopback_origin(origin: &str) -> bool {
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    // `rest` keeps the colon before a port.
    let (host, rest) = match authority.strip_prefix("[::1]") {
        Some(rest) => ("[::1]", rest),
        None => authority.split_at(authority.find(':').unwrap_or(authority.len())),
    };
    let port_ok = rest.is_empty()
        || rest.strip_prefix(':').is_some_and(|digits| {
            (1..=5).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
        });
    matches!(host, "localhost" | "127.0.0.1" | "[::1]") && port_ok
}

/// `tools/call` requests in flight on `POST /mcp`, by JSON-RPC ID, so a
/// `notifications/cancelled` POST can stop one. Without sessions, IDs are
/// shared by every client holding the token; a cancel only ends a wait.
pub(super) type McpCalls = Mutex<HashMap<String, Vec<Arc<AtomicBool>>>>;

/// Sets `cancel` when the server stops or the client closes the connection
/// (a plain JSON response cannot reach a client that left), until `done`.
fn watch_call(
    stream: &TcpStream,
    cancel: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    let stream = stream.try_clone().ok()?;
    std::thread::Builder::new()
        .name("mcp watch".into())
        .spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if stop.load(Ordering::Relaxed) || peer_closed(&stream) {
                    cancel.store(true, Ordering::Relaxed);
                    return;
                }
            }
        })
        .ok()
}

/// Waits up to 200 ms for the peer; true once it has closed, half-closed,
/// or reset the connection. The request body is already read and the
/// connection ends after this response, so bytes it sends later (a stray
/// CRLF, a pipelined request) are discarded and do not count.
fn peer_closed(stream: &TcpStream) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut pfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd references an open descriptor owned by `stream`.
        let ready = unsafe { libc::poll(&mut pfd, 1, 200) };
        if ready < 0 {
            std::thread::sleep(Duration::from_millis(200));
            return false;
        }
        if ready == 0 {
            return false;
        }
        // Readable: EOF or an error means the peer is gone; data does not.
        match (&*stream).read(&mut [0u8; 512]) {
            Ok(0) => true,
            Ok(_) => false,
            Err(error) => !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = stream;
        std::thread::sleep(Duration::from_millis(200));
        false
    }
}

fn mcp_error(id: Value, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": message}})
}

/// MCP Streamable HTTP: one JSON-RPC message per POST, answered with one
/// JSON response, `202` with no body for a notification or response, and
/// `400` for a malformed message or an unsupported `MCP-Protocol-Version`.
/// The server keeps no session and offers no server-to-client stream, so
/// `GET /mcp` is 405.
fn mcp_post(
    stream: &mut TcpStream,
    server: &Server,
    request: &Request,
    admission: &Admission,
    calls: &McpCalls,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    if request
        .origin
        .as_deref()
        .is_some_and(|o| !loopback_origin(o))
    {
        return respond(stream, 403, &json!({"error": "Origin not allowed"}));
    }
    let message: Value = match serde_json::from_slice(&request.body) {
        Ok(message) => message,
        Err(error) => {
            return respond(
                stream,
                400,
                &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("Parse error: {error}")}}),
            );
        }
    };
    // Version probes may name a revision this server does not speak; they
    // get their own answer (a negotiated version, or method-not-found).
    let probe = matches!(
        message.get("method").and_then(Value::as_str),
        Some("initialize" | "server/discover")
    );
    if let Some(version) = request.protocol_version.as_deref() {
        if !probe && !mcp::SUPPORTED_VERSIONS.contains(&version) {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            return respond(
                stream,
                400,
                &mcp_error(
                    id,
                    &format!(
                        "Unsupported MCP-Protocol-Version {version:?}; supported: {}",
                        mcp::SUPPORTED_VERSIONS.join(", ")
                    ),
                ),
            );
        }
    }
    let lock = || calls.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(key) = mcp::cancelled_request(&message) {
        for flag in lock().get(&key).into_iter().flatten() {
            flag.store(true, Ordering::Relaxed);
        }
    }
    let blocking = mcp::called_tool(&message).is_some_and(tools::blocks);
    let _long = if blocking {
        let Some(slot) = admission.long_lived() else {
            // A JSON-RPC error with the request's ID, as stdio answers.
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            return respond(
                stream,
                503,
                &json!({"jsonrpc": "2.0", "id": id, "error": {
                    "code": mcp::SERVER_OVERLOADED,
                    "message": "Too many concurrent long-lived connections; retry after one returns",
                }}),
            );
        };
        Some(slot)
    } else {
        None
    };
    // A blocking call ends early when cancelled, the client leaves, or the
    // server stops; the others answer at once.
    let cancel = Arc::new(AtomicBool::new(false));
    let key = mcp::request_key(&message).filter(|_| blocking);
    let done = Arc::new(AtomicBool::new(false));
    let watcher = if let Some(key) = &key {
        lock().entry(key.clone()).or_default().push(cancel.clone());
        watch_call(stream, cancel.clone(), stop.clone(), done.clone())
    } else {
        None
    };
    let reply = if blocking {
        mcp::handle_one(server, message, &cancel)
    } else {
        mcp::handle_one(server, message, stop)
    };
    done.store(true, Ordering::Relaxed);
    if let Some(key) = &key {
        let mut map = lock();
        if let Some(flags) = map.get_mut(key) {
            flags.retain(|flag| !Arc::ptr_eq(flag, &cancel));
            if flags.is_empty() {
                map.remove(key);
            }
        }
    }
    if let Some(watcher) = watcher {
        // Wakes the watcher's poll at once instead of after its timeout;
        // the response still goes out on the write half.
        let _ = stream.shutdown(Shutdown::Read);
        let _ = watcher.join();
    }
    match reply {
        mcp::Reply::Response(response) => respond(stream, 200, &response),
        mcp::Reply::Invalid(error) => respond(stream, 400, &error),
        mcp::Reply::Accepted => {
            write!(
                stream,
                "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
            )?;
            stream.flush()
        }
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name == key).then(|| {
            percent_decode_str(&value.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        })
    })
}

fn sse(stream: &mut TcpStream, event: &str, data: &Value) -> io::Result<()> {
    let payload = format!("event: {event}\ndata: {data}\n\n");
    stream.write_all(payload.as_bytes())?;
    stream.flush()
}

fn send_message(stream: &mut TcpStream, message: &Message) -> io::Result<bool> {
    match message {
        Message::State(value) => sse(stream, "state", value)?,
        Message::Output(text) => sse(stream, "output", &json!({"text": text}))?,
        Message::End(value) => {
            sse(stream, "end", value)?;
            return Ok(false);
        }
    }
    Ok(true)
}

/// Streams a run's events until its drive thread finishes. A run this
/// server is not driving gets its current state and `end` at once.
fn events(
    mut stream: TcpStream,
    server: &Server,
    id: &str,
    query: &str,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let project = query_param(query, "project");
    let (run, info) = match tools::load(server, project.as_deref(), id) {
        Ok(loaded) => loaded,
        Err(error) => return respond(&mut stream, 404, &json!({"error": format!("{error:#}")})),
    };
    let subscription = server.runs.subscribe(id);
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
    )?;
    let Some((backlog, receiver)) = subscription else {
        return sse(&mut stream, "end", &tools::status_value(&run, &info));
    };
    sse(&mut stream, "state", &tools::status_value(&run, &info))?;
    for message in &backlog {
        if !send_message(&mut stream, message)? {
            return Ok(());
        }
    }
    let mut last_keepalive = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(message) => {
                last_keepalive = Instant::now();
                if !send_message(&mut stream, &message)? {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if last_keepalive.elapsed() >= KEEPALIVE {
                    stream.write_all(b": keep-alive\n\n")?;
                    stream.flush()?;
                    last_keepalive = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The run ended while this subscriber's queue was full.
                let (run, info) = match tools::load(server, project.as_deref(), id) {
                    Ok(loaded) => loaded,
                    Err(_) => return Ok(()),
                };
                return sse(&mut stream, "end", &tools::status_value(&run, &info));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes after the request do not hide a close: with them pending,
    /// a half-close or a full close is still seen.
    #[cfg(unix)]
    #[test]
    fn peer_closed_sees_a_close_behind_stray_bytes() {
        for half in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (stream, _) = listener.accept().unwrap();
            client.write_all(b"\r\n").unwrap();
            assert!(!peer_closed(&stream), "data alone is not a close");
            client.write_all(b"\r\n").unwrap();
            if half {
                client.shutdown(Shutdown::Write).unwrap();
            } else {
                drop(client);
            }
            let started = Instant::now();
            while !peer_closed(&stream) {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "close not seen (half: {half})"
                );
            }
        }
    }

    #[test]
    fn trickled_requests_have_an_absolute_deadline() {
        for prefix in [
            "GET /tools HTTP/1.1\r\nX-Slow: ",
            "POST /tools/list_workflows HTTP/1.1\r\nAuthorization: Bearer token\r\nContent-Length: 100\r\n\r\n",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (stream, _) = listener.accept().unwrap();
            client.write_all(prefix.as_bytes()).unwrap();
            let writer = std::thread::spawn(move || {
                for _ in 0..40 {
                    if client.write_all(b"x").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let started = Instant::now();
            let result = read_request(&stream, "token", Duration::from_millis(120), || true);
            let error = result.err().expect("trickling must not renew the deadline");
            assert!(matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ));
            assert!(started.elapsed() < Duration::from_millis(600));
            drop(stream);
            writer.join().unwrap();
        }
    }

    #[test]
    fn only_loopback_origins_are_allowed() {
        for allowed in [
            "http://localhost",
            "http://localhost:3000",
            "https://127.0.0.1:8737",
            "http://[::1]",
            "http://[::1]:5173",
        ] {
            assert!(loopback_origin(allowed), "{allowed} should be allowed");
        }
        for refused in [
            "",
            "null",
            "localhost",
            "file://localhost",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com",
            "http://localhost:3000.evil.com",
            "http://localhost:x@evil.com",
            "http://evil.com@localhost",
            "http://[::1]evil",
            "http://[::1]:",
            "http://localhost:123456",
            "http://localhost/",
            "http://LOCALHOST",
            "https://evil.example",
        ] {
            assert!(!loopback_origin(refused), "{refused:?} should be refused");
        }
    }

    #[test]
    fn token_comparison_and_query_decoding() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert_eq!(
            query_param("a=1&project=%2Ftmp%2Fmy+dir", "project").as_deref(),
            Some("/tmp/my dir")
        );
        assert_eq!(query_param("a=1", "project"), None);
        assert_eq!(new_token().unwrap().len(), 64);
    }

    #[test]
    fn long_lived_subcap_leaves_slots_for_quick_requests() {
        let admission = Admission {
            pending: Arc::new(Mutex::new(Pending::default())),
            id: 1,
            active: Arc::new(AtomicUsize::new(0)),
            long: Arc::new(AtomicUsize::new(0)),
            left: Cell::new(Some(true)),
        };
        let mut active_slots = Vec::new();
        let mut long_slots = Vec::new();
        for _ in 0..MAX_LONG_CONNECTIONS {
            active_slots.push(admission.authenticated().expect("auth slot"));
            long_slots.push(admission.long_lived().expect("long slot"));
        }
        // Further long-lived requests are rejected while quick ones remain open.
        assert!(admission.long_lived().is_none());
        let mut quick = Vec::new();
        for _ in 0..(MAX_CONNECTIONS - MAX_LONG_CONNECTIONS) {
            quick.push(admission.authenticated().expect("quick auth slot"));
        }
        assert!(admission.authenticated().is_none());
        drop(long_slots);
        drop(active_slots);
        drop(quick);
        assert_eq!(admission.active.load(Ordering::Acquire), 0);
        assert_eq!(admission.long.load(Ordering::Acquire), 0);
    }

    #[test]
    fn oversized_request_line_is_rejected_at_header_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let writer = std::thread::spawn(move || {
            let line = format!(
                "GET /{} HTTP/1.1\r\n\r\n",
                "a".repeat(MAX_HEADER_BYTES + 128)
            );
            let _ = client.write_all(line.as_bytes());
        });
        let error = read_request(&stream, "token", Duration::from_secs(2), || true)
            .err()
            .expect("oversized request line must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.join().unwrap();
    }

    #[test]
    fn health_skips_auth_and_body_wait_but_keeps_header_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // Unauthenticated GET /health with a declared body that never arrives
        // finishes immediately without invoking `admit`.
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        client
            .write_all(b"GET /health HTTP/1.1\r\nContent-Length: 100\r\n\r\n")
            .unwrap();
        let mut admitted = false;
        let request = read_request(&stream, "token", Duration::from_secs(2), || {
            admitted = true;
            true
        })
        .unwrap();
        assert!(is_health(&request));
        assert!(!admitted);
        assert!(request.body.is_empty());

        // Oversized headers on /health are still rejected.
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let writer = std::thread::spawn(move || {
            let req = format!(
                "GET /health HTTP/1.1\r\nX-Pad: {}\r\n\r\n",
                "a".repeat(MAX_HEADER_BYTES + 128)
            );
            let _ = client.write_all(req.as_bytes());
        });
        let error = read_request(&stream, "token", Duration::from_secs(2), || true)
            .err()
            .expect("oversized health headers must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.join().unwrap();
    }
}
