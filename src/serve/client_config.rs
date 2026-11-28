//! `serve --client-config CLIENT`: the snippet that registers this exact
//! `serve --mcp` command with an MCP client. Printed only; no client's
//! config file is edited.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// MCP clients `--client-config` writes a snippet for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Client {
    /// Claude Code: a `claude mcp add` command.
    Claude,
    /// Codex: `~/.codex/config.toml`.
    Codex,
    /// Cursor: `.cursor/mcp.json` or `~/.cursor/mcp.json`.
    Cursor,
    /// Gemini CLI: `~/.gemini/settings.json`.
    Gemini,
    /// OpenCode: `opencode.json`.
    Opencode,
    /// VS Code and GitHub Copilot: `.vscode/mcp.json`.
    Vscode,
    /// Amp: `amp.mcpServers` in Amp's `settings.json`.
    Amp,
    /// GitHub Copilot CLI: `~/.copilot/mcp-config.json`.
    Copilot,
    /// Pi: `~/.pi/agent/mcp.json` or `.pi/mcp.json`.
    Pi,
    /// oh-my-pi: `~/.omp/agent/mcp.json` or `.omp/mcp.json`.
    Omp,
    /// goose: `extensions` in `~/.config/goose/config.yaml`.
    Goose,
    /// Any client that reads the common `{"mcpServers": {...}}` JSON.
    Json,
}

/// Seconds a client should allow one tool call; `wait_run` blocks up to the
/// `timeout_seconds` it is given, so this leaves room for long waits.
const TOOL_TIMEOUT_SECONDS: u64 = 600;
const NAME: &str = "model-router";

/// A path as text; snippets cannot carry one that is not UTF-8.
fn utf8(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).with_context(|| {
        format!(
            "{} is not valid UTF-8; MCP client configs cannot hold it",
            path.display()
        )
    })
}

/// The `serve --mcp` argv for this server, after the program.
pub fn serve_args(
    project: &Path,
    config: Option<&Path>,
    allow_roots: &[PathBuf],
    metrics: Option<&Path>,
) -> Result<Vec<String>> {
    let mut args = vec!["serve".into(), "--mcp".into(), "-C".into(), utf8(project)?];
    if let Some(config) = config {
        args.extend(["--config".into(), utf8(config)?]);
    }
    if let Some(metrics) = metrics {
        args.extend(["--metrics-file".into(), utf8(metrics)?]);
    }
    for root in allow_roots {
        args.extend(["--allow-root".into(), utf8(root)?]);
    }
    Ok(args)
}

/// The program path to write: the path this binary was started by (argv[0],
/// looked up on PATH when bare) when that is the running file, so a symlink
/// such as a package manager's `bin/model-router` survives upgrades; else
/// the resolved executable.
pub fn program() -> Result<String> {
    let exe = std::env::current_exe().context("Locate the model-router executable")?;
    let real = std::fs::canonicalize(&exe).ok();
    let invoked = std::env::args_os().next().and_then(|arg0| {
        let arg0 = PathBuf::from(arg0);
        let found = if arg0.components().count() > 1 {
            arg0
        } else {
            crate::adapters::executable(arg0.to_str()?)?
        };
        let found = std::path::absolute(found).ok()?;
        (real.is_some() && std::fs::canonicalize(&found).ok() == real).then_some(found)
    });
    utf8(&invoked.unwrap_or(exe))
}

/// A YAML double-quoted scalar. JSON escaping covers C0 controls, quotes,
/// and backslashes; YAML also refuses or folds DEL, C1 controls, line and
/// paragraph separators, the BOM, and U+FFFE/U+FFFF, so those are escaped.
fn yaml_string(text: &str) -> String {
    json!(text)
        .to_string()
        .chars()
        .map(|c| match c {
            '\u{7f}'..='\u{9f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{feff}'
            | '\u{fffe}'
            | '\u{ffff}' => {
                format!("\\u{:04x}", c as u32)
            }
            c => c.to_string(),
        })
        .collect()
}

/// The snippet for `client`, launching `program` with `args`.
pub fn render(client: Client, program: &str, args: &[String]) -> Result<String> {
    let servers = |entry: Value| json!({"mcpServers": {NAME: entry}});
    let pretty = |value: Value| serde_json::to_string_pretty(&value).map(|s| s + "\n");
    let command: Vec<String> = std::iter::once(program.to_owned())
        .chain(args.iter().cloned())
        .collect();
    Ok(match client {
        Client::Claude => format!("claude mcp add {NAME} -- {}\n", shell_words::join(&command)),
        Client::Codex => {
            let mut server = toml::map::Map::new();
            server.insert("command".into(), program.into());
            server.insert("args".into(), args.to_vec().into());
            server.insert(
                "tool_timeout_sec".into(),
                toml::Value::Integer(TOOL_TIMEOUT_SECONDS as i64),
            );
            let mut table = toml::map::Map::new();
            table.insert(NAME.into(), server.into());
            let mut root = toml::map::Map::new();
            root.insert("mcp_servers".into(), table.into());
            toml::to_string(&toml::Value::Table(root)).context("Render Codex config")?
        }
        Client::Cursor | Client::Pi | Client::Omp | Client::Json => {
            pretty(servers(json!({"command": program, "args": args})))?
        }
        // `tools` is required; the default 30 s timeout is shorter than a
        // default `wait_run`.
        Client::Copilot => pretty(servers(json!({
            "type": "local",
            "command": program,
            "args": args,
            "tools": ["*"],
            "timeout": TOOL_TIMEOUT_SECONDS * 1000,
        })))?,
        Client::Gemini => pretty(servers(json!({
            "command": program,
            "args": args,
            "timeout": TOOL_TIMEOUT_SECONDS * 1000,
        })))?,
        Client::Opencode => pretty(json!({
            "$schema": "https://opencode.ai/config.json",
            "mcp": {NAME: {"type": "local", "command": command, "enabled": true}},
        }))?,
        Client::Vscode => pretty(json!({
            "servers": {NAME: {"type": "stdio", "command": program, "args": args}},
        }))?,
        Client::Amp => pretty(json!({
            "amp.mcpServers": {NAME: {"command": program, "args": args}},
        }))?,
        Client::Goose => format!(
            "extensions:\n  {NAME}:\n    name: {NAME}\n    type: stdio\n    cmd: {}\n    args: [{}]\n    enabled: true\n    timeout: {TOOL_TIMEOUT_SECONDS}\n    envs: {{}}\n",
            yaml_string(program),
            args.iter()
                .map(|a| yaml_string(a))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths with spaces must survive each format's quoting.
    #[test]
    fn snippets_keep_paths_with_spaces_intact() {
        let args = serve_args(
            Path::new("/tmp/my project"),
            None,
            &[PathBuf::from("/src")],
            None,
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "serve",
                "--mcp",
                "-C",
                "/tmp/my project",
                "--allow-root",
                "/src"
            ]
        );
        let program = "/opt/my tools/model-router";

        let claude = render(Client::Claude, program, &args).unwrap();
        let words = shell_words::split(claude.trim()).unwrap();
        assert_eq!(&words[..4], ["claude", "mcp", "add", NAME]);
        assert_eq!(words[4], "--");
        assert_eq!(words[5], program);
        assert_eq!(&words[6..], &args[..]);

        let codex: toml::Value = render(Client::Codex, program, &args)
            .unwrap()
            .parse()
            .unwrap();
        let server = &codex["mcp_servers"][NAME];
        assert_eq!(server["command"].as_str(), Some(program));
        assert_eq!(server["args"][3].as_str(), Some("/tmp/my project"));
        assert_eq!(server["tool_timeout_sec"].as_integer(), Some(600));

        let opencode: Value =
            serde_json::from_str(&render(Client::Opencode, program, &args).unwrap()).unwrap();
        assert_eq!(opencode["mcp"][NAME]["command"][0], program);
        assert_eq!(opencode["mcp"][NAME]["command"][4], "/tmp/my project");

        let goose = render(Client::Goose, program, &args).unwrap();
        assert!(
            goose.contains(r#"    cmd: "/opt/my tools/model-router""#),
            "{goose}"
        );
        assert!(
            goose.contains(
                r#"    args: ["serve", "--mcp", "-C", "/tmp/my project", "--allow-root", "/src"]"#
            ),
            "{goose}"
        );

        let gemini: Value =
            serde_json::from_str(&render(Client::Gemini, program, &args).unwrap()).unwrap();
        assert_eq!(gemini["mcpServers"][NAME]["timeout"], 600_000);

        let copilot: Value =
            serde_json::from_str(&render(Client::Copilot, program, &args).unwrap()).unwrap();
        let server = &copilot["mcpServers"][NAME];
        assert_eq!(server["tools"], json!(["*"]));
        assert_eq!(server["timeout"], 600_000);
        assert_eq!(server["args"][3], "/tmp/my project");
    }

    /// Characters JSON leaves raw but YAML refuses (DEL, C1 controls) or
    /// folds (NEL, line separator) are escaped, so goose reads the path back.
    #[test]
    fn goose_yaml_escapes_what_yaml_refuses() {
        let path = "/tmp/a\u{7f}b\u{85}c\u{2028}d\u{9b}e\"f\\g\th";
        assert_eq!(
            yaml_string(path),
            r#""/tmp/a\u007fb\u0085c\u2028d\u009be\"f\\g\th""#
        );
        // Ordinary non-ASCII text stays as it is.
        assert_eq!(yaml_string("/tmp/día ü"), r#""/tmp/día ü""#);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_refused() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/bad-\xff"));
        assert!(serve_args(path, None, &[], None).is_err());
    }
}
