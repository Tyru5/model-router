//! Headless server so other agents and clients can drive model-router:
//! MCP over stdio (`serve --mcp`) and a small loopback JSON HTTP API with
//! Server-Sent Events (`serve --http ADDR`). Both expose the same tools.
//!
//! Runs execute unattended on background threads. Stdout carries JSON-RPC
//! only; diagnostics go to stderr.

mod http;
pub mod mcp;
mod runs;
mod tools;

use crate::{catalog, config};
use anyhow::{Context, Result, ensure};
use std::{
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// `model-router serve` options.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Serve the Model Context Protocol on stdin/stdout.
    #[arg(long)]
    pub mcp: bool,
    /// Serve the JSON HTTP API on ADDR, e.g. 127.0.0.1:8737 (port 0 picks one).
    #[arg(long, value_name = "ADDR")]
    pub http: Option<SocketAddr>,
    /// Let --http bind a non-loopback address. Anyone with the token can run harnesses.
    #[arg(long, requires = "http")]
    pub allow_remote: bool,
    /// A directory whose projects clients may name (repeatable).
    /// Default: the server's project only.
    #[arg(long = "allow-root", value_name = "PATH")]
    pub allow_roots: Vec<PathBuf>,
}

/// Environment variable that fixes the HTTP bearer token.
pub const TOKEN_ENV: &str = "MODEL_ROUTER_SERVE_TOKEN";

/// How long shutdown waits for cancelled runs to stop their harnesses.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

pub struct Server {
    project: PathBuf,
    config: Option<PathBuf>,
    roots: Vec<PathBuf>,
    runs: runs::Registry,
}

impl Server {
    pub fn new(project: PathBuf, config: Option<PathBuf>, allow_roots: &[PathBuf]) -> Result<Self> {
        let project = catalog::check_project(&project)?;
        let mut roots = vec![];
        for root in allow_roots {
            roots.push(
                std::fs::canonicalize(root)
                    .with_context(|| format!("Allowed root not found: {}", root.display()))?,
            );
        }
        if roots.is_empty() {
            roots.push(project.clone());
        }
        Ok(Self {
            project,
            config,
            roots,
            runs: runs::Registry::default(),
        })
    }

    /// The project a tool call names, or the server's own, after checking
    /// allowed roots and configuration path.
    fn check_project_root(&self, requested: Option<&str>) -> Result<(PathBuf, Option<&Path>)> {
        let project = match requested.map(str::trim).filter(|p| !p.is_empty()) {
            None => self.project.clone(),
            Some(path) => {
                let path = self.project.join(path);
                let path = catalog::check_project(&path)?;
                ensure!(
                    self.roots.iter().any(|root| path.starts_with(root)),
                    "Project {} is outside the server's allowed roots ({}); start the server with --allow-root",
                    path.display(),
                    self.roots
                        .iter()
                        .map(|r| r.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                path
            }
        };
        let explicit = (project == self.project)
            .then_some(self.config.as_deref())
            .flatten();
        Ok((project, explicit))
    }

    /// The project a tool call names, or the server's own. Relative paths
    /// resolve against the server's project. Rejects projects outside the
    /// allowed roots and untrusted ones.
    fn project(&self, requested: Option<&str>) -> Result<PathBuf> {
        let (project, explicit) = self.check_project_root(requested)?;
        ensure_trusted(&project, explicit)?;
        Ok(project)
    }

    /// Resolves the project, verifies trust, and returns its settings in one load.
    /// Fires the server project's due schedules (or, with `only`, that
    /// webhook schedule) and drives their runs. Returns the runs started.
    pub(crate) fn fire_schedules(&self, only: Option<&str>) -> Result<Vec<String>> {
        let fired =
            crate::delegation::schedules::fire_due(&self.project, chrono::Utc::now(), only)?;
        let mut started = vec![];
        for item in fired {
            match item.run {
                Some(run) => {
                    let id = run.id.clone();
                    match self.runs.drive(&self.project, run) {
                        Ok(()) => started.push(id),
                        Err(error) => log(format!("schedule {}: {error:#}", item.schedule)),
                    }
                }
                None => log(format!(
                    "schedule {}: launch failed: {}",
                    item.schedule,
                    item.error.unwrap_or_default()
                )),
            }
        }
        Ok(started)
    }

    fn project_and_settings(&self, requested: Option<&str>) -> Result<(PathBuf, config::Settings)> {
        let (project, explicit) = self.check_project_root(requested)?;
        let loaded = config::load_config(&project, explicit)?;
        if let Some(review) = loaded.review() {
            anyhow::bail!(
                "{}\nRun `model-router -C {} trust` first.",
                review.text().trim_end(),
                project.display()
            );
        }
        Ok((project, loaded.settings))
    }
}

/// Hook for the project trust model. Every tool resolves its project
/// through here before reading config or starting a harness.
/// Refuses a project whose `.model-router.toml` has untrusted dangerous
/// sections (custom commands, checks, custom providers), like `run` does.
/// Global and `--config` files are the user's own and always trusted.
pub fn ensure_trusted(project: &Path, explicit: Option<&Path>) -> Result<()> {
    let loaded = config::load_config(project, explicit)?;
    if let Some(review) = loaded.review() {
        anyhow::bail!(
            "{}\nRun `model-router -C {} trust` first.",
            review.text().trim_end(),
            project.display()
        );
    }
    Ok(())
}

/// Writes a diagnostic line to stderr; never stdout, which MCP owns.
pub(crate) fn log(message: impl std::fmt::Display) {
    let _ = writeln!(std::io::stderr(), "model-router serve: {message}");
}

/// Runs the server until stdin closes (MCP) or `shutdown` is set (signal).
pub fn run(
    args: Args,
    project: PathBuf,
    config: Option<PathBuf>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    ensure!(
        args.mcp || args.http.is_some(),
        "Choose --mcp, --http ADDR, or both"
    );
    let server = Arc::new(Server::new(project, config, &args.allow_roots)?);
    // Runs a stage's agent launches (and schedules fire) are driven here.
    crate::engine::set_run_host(Arc::new(tools::ServeHost(server.clone())));
    // Fires the server project's approved, due schedules.
    {
        let (server, stop) = (server.clone(), shutdown.clone());
        std::thread::spawn(move || {
            let mut last = std::time::Instant::now() - Duration::from_secs(60);
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if last.elapsed() < Duration::from_secs(30) {
                    continue;
                }
                last = std::time::Instant::now();
                if let Err(error) = server.fire_schedules(None) {
                    log(format!("schedules: {error:#}"));
                }
            }
        });
    }
    if let Some(addr) = args.http {
        ensure!(
            args.allow_remote || addr.ip().is_loopback(),
            "--http binds loopback addresses only; pass --allow-remote to bind {addr}"
        );
        let (token, from_env) = match std::env::var(TOKEN_ENV) {
            Ok(token) if !token.trim().is_empty() => (token.trim().to_owned(), true),
            _ => (http::new_token(), false),
        };
        let listener = std::net::TcpListener::bind(addr)
            .with_context(|| format!("Bind the HTTP API to {addr}"))?;
        let bound = listener.local_addr()?;
        if from_env {
            log(format!(
                "HTTP API on http://{bound} (bearer token from {TOKEN_ENV})"
            ));
        } else {
            log(format!(
                "HTTP API on http://{bound} (bearer token: {token})"
            ));
        }
        let (server, stop) = (server.clone(), shutdown.clone());
        std::thread::spawn(move || http::serve(listener, server, token, stop));
    }
    if args.mcp {
        // A signal cancels every run, gives harnesses time to stop, then exits:
        // the MCP loop may be blocked reading stdin.
        let (server_bg, stop) = (server.clone(), shutdown.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            log("stopping: cancelling active runs");
            server_bg.runs.shutdown(SHUTDOWN_GRACE);
            std::process::exit(130);
        });
        log(format!(
            "MCP on stdio for {} (protocol {})",
            server.project.display(),
            mcp::PROTOCOL_VERSION
        ));
        let stdin = std::io::stdin();
        let calls = mcp::serve(server.clone(), stdin.lock(), std::io::stdout());
        log("stdin closed: cancelling active runs");
        // Cancelling first ends pending wait_run calls; then let every call
        // in flight answer, and cancel any run one of them just started.
        server.runs.shutdown(SHUTDOWN_GRACE);
        let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
        while calls.iter().any(|call| !call.is_finished()) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        server.runs.shutdown(SHUTDOWN_GRACE);
    } else {
        while !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        log("stopping: cancelling active runs");
        server.runs.shutdown(SHUTDOWN_GRACE);
        std::process::exit(130);
    }
    Ok(())
}
