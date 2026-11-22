//! `mr __mcp-bridge`: the stdio MCP server a delegating harness starts. It
//! holds no state: it presents the turn's token to the driving `mr`'s
//! socket and copies lines both ways. When the socket refuses (the turn is
//! over, or the parent was released) it answers every call itself with
//! `parent_not_active`.
use super::broker::{SOCK_VAR, TOKEN_VAR, read_token};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

/// Runs the bridge until the harness closes its stdin or the router closes
/// the session. Returns the process exit code.
pub fn run() -> i32 {
    let connected = connect();
    match connected {
        Ok(stream) => forward(stream),
        Err(_) => {
            answer_inactive(io::stdin().lock(), io::stdout());
            0
        }
    }
}

fn connect() -> io::Result<UnixStream> {
    let sock = std::env::var_os(SOCK_VAR).map(PathBuf::from);
    let token = std::env::var_os(TOKEN_VAR).map(PathBuf::from);
    let (Some(sock), Some(token)) = (sock, token) else {
        return Err(io::Error::other("delegation variables are not set"));
    };
    let token = read_token(&token).map_err(io::Error::other)?;
    let mut stream = UnixStream::connect(sock)?;
    stream.write_all(format!("{token}\n").as_bytes())?;
    Ok(stream)
}

fn forward(stream: UnixStream) -> i32 {
    let Ok(mut upstream) = stream.try_clone() else {
        return 1;
    };
    // Harness → router; on EOF, tell the router no more requests come.
    let to_router = std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let _ = pump(&mut stdin, &mut upstream);
        let _ = upstream.shutdown(std::net::Shutdown::Write);
    });
    // Router → harness, until the router closes the session.
    let mut downstream = stream;
    let mut stdout = io::stdout().lock();
    let _ = pump(&mut downstream, &mut stdout);
    drop(to_router);
    0
}

/// Copies until EOF, flushing after every read. Not `io::copy`: between a
/// socket and a pipe its kernel fast path may hold data back, and each
/// JSON-RPC line must reach the other side at once.
fn pump(from: &mut impl io::Read, to: &mut impl Write) -> io::Result<()> {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        to.write_all(&buffer[..n])?;
        to.flush()?;
    }
}

/// A minimal MCP server for a closed session: initialize and list succeed
/// (with no tools), every call fails with `parent_not_active`.
pub fn answer_inactive(input: impl BufRead, mut output: impl Write) {
    for line in input.lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": crate::serve::mcp::PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "model-router", "version": env!("CARGO_PKG_VERSION")},
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": []}),
            "tools/call" => crate::serve::mcp::tool_result(Err(crate::serve::mcp::ToolError::new(
                "parent_not_active",
                "This stage turn is over or its tools were closed; delegation is unavailable",
            )
            .into())),
            _ => {
                let _ = writeln!(
                    output,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})
                );
                continue;
            }
        };
        if writeln!(
            output,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        )
        .is_err()
        {
            break;
        }
        let _ = output.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_session_answers_every_call_with_parent_not_active() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delegate_task","arguments":{}}}"#,
            "\n"
        );
        let mut out = Vec::new();
        answer_inactive(input.as_bytes(), &mut out);
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["result"]["serverInfo"]["name"], "model-router");
        assert_eq!(lines[1]["result"]["isError"], true);
        assert_eq!(
            lines[1]["result"]["structuredContent"]["error"]["code"],
            "parent_not_active"
        );
    }
}
