//! A small JSON HTTP API over std::net: one thread per connection,
//! `Connection: close`, bearer-token auth on every route.
//!
//! - `GET /tools` lists the tools.
//! - `POST /tools/<name>` with a JSON arguments object runs one.
//! - `GET /runs/<id>/events[?project=DIR]` streams a run's events (SSE).

use super::{Server, runs::Message, tools};
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::RecvTimeoutError,
    },
    time::{Duration, Instant},
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const KEEPALIVE: Duration = Duration::from_secs(15);
const MAX_CONNECTIONS: usize = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct ConnectionSlot(Arc<AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A random bearer token (from the OS RNG via UUID v4).
pub fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub fn serve(listener: TcpListener, server: Arc<Server>, token: String, stop: Arc<AtomicBool>) {
    if let Err(error) = listener.set_nonblocking(true) {
        super::log(format!("HTTP listener: {error}"));
        return;
    }
    let token = Arc::new(token);
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                // Only this accept loop increments the count. Refuse overload
                // without creating a worker or waiting for the client to read.
                if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    drop(stream);
                    continue;
                }
                active.fetch_add(1, Ordering::Relaxed);
                let slot = ConnectionSlot(active.clone());
                let (server, token) = (server.clone(), token.clone());
                if let Err(error) = std::thread::Builder::new().spawn(move || {
                    let _slot = slot;
                    if let Err(error) = connection(stream, &server, &token) {
                        if error.kind() != io::ErrorKind::BrokenPipe {
                            super::log(format!("HTTP connection: {error}"));
                        }
                    }
                }) {
                    super::log(format!("HTTP worker: {error}"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
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

fn read_request(stream: &TcpStream, token: &str, timeout: Duration) -> io::Result<Request> {
    let source = RequestReader {
        stream,
        deadline: Instant::now() + timeout,
    };
    let mut reader = BufReader::new(source.take((MAX_HEADER_BYTES + MAX_BODY_BYTES) as u64));
    let mut line = String::new();
    let mut header_bytes = reader.read_line(&mut line)?;
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
        body: vec![],
    };
    let mut length = 0usize;
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
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
            "transfer-encoding" => return Err(invalid("Chunked bodies are not supported")),
            _ => {}
        }
    }
    if length > MAX_BODY_BYTES {
        return Err(invalid("Body too large"));
    }
    // Never wait for an unauthenticated client's declared body.
    if !authorized(&request, token) {
        return Ok(request);
    }
    request.body.resize(length, 0);
    reader.read_exact(&mut request.body)?;
    Ok(request)
}

/// Compares without leaking the position of the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
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

fn connection(mut stream: TcpStream, server: &Server, token: &str) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    // A client that stops reading an event stream must not pin its thread.
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let request = match read_request(&stream, token, REQUEST_TIMEOUT) {
        Ok(request) => request,
        Err(error) => return respond(&mut stream, 400, &json!({"error": error.to_string()})),
    };
    if !authorized(&request, token) {
        return respond(
            &mut stream,
            401,
            &json!({"error": "Missing or wrong bearer token"}),
        );
    }
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
            match tools::call(server, name, &arguments) {
                Ok(result) => respond(&mut stream, 200, &json!({"result": result})),
                Err(error) => respond(&mut stream, 400, &json!({"error": format!("{error:#}")})),
            }
        }
        ("GET", ["runs", id, "events"]) => events(stream, server, id, &request.query),
        (_, ["tools"] | ["tools", _] | ["runs", _, "events"]) => {
            respond(&mut stream, 405, &json!({"error": "Method not allowed"}))
        }
        _ => respond(&mut stream, 404, &json!({"error": "Not found"})),
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
    write!(stream, "event: {event}\ndata: {data}\n\n")?;
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
fn events(mut stream: TcpStream, server: &Server, id: &str, query: &str) -> io::Result<()> {
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
    loop {
        match receiver.recv_timeout(KEEPALIVE) {
            Ok(message) => {
                if !send_message(&mut stream, &message)? {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                stream.write_all(b": keep-alive\n\n")?;
                stream.flush()?;
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
            let result = read_request(&stream, "token", Duration::from_millis(120));
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
    fn token_comparison_and_query_decoding() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert_eq!(
            query_param("a=1&project=%2Ftmp%2Fmy+dir", "project").as_deref(),
            Some("/tmp/my dir")
        );
        assert_eq!(query_param("a=1", "project"), None);
        assert_eq!(new_token().len(), 64);
    }
}
