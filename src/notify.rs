//! Notifications when a run needs the user: a stage waits for input, the
//! run stopped, or it finished. Backends are chosen in the preferences file
//! (`appearance.json`, key `notifications`):
//!
//! - terminal: BEL, OSC 9 and OSC 777, written to the controlling terminal
//!   (`/dev/tty`), never into an attached harness PTY;
//! - desktop: `notify-send` (Linux) or `osascript` (macOS), spawned detached;
//! - webhook: an ntfy-style POST from a background thread.
//!
//! Identical events within `REPEAT_WINDOW` are sent once.
use crate::engine::{Action, Run, Status};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Environment variable naming the webhook URL; it turns the webhook on.
pub const URL_ENV: &str = "MODEL_ROUTER_NOTIFY_URL";
/// Environment variable enabling notifications for CLI `run`/`resume`/`rerun`.
pub const CLI_ENV: &str = "MODEL_ROUTER_NOTIFY";
/// The same event is not repeated within this window.
pub const REPEAT_WINDOW: Duration = Duration::from_secs(180);
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);
/// Webhooks still being sent, for `wait_for_webhooks`.
static WEBHOOKS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());
/// Longest title or body sent, in characters.
const MAX_TEXT: usize = 240;

/// Which backends deliver notifications.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Backends {
    pub terminal: bool,
    pub desktop: bool,
    pub webhook: bool,
    /// Webhook URL; `MODEL_ROUTER_NOTIFY_URL` takes precedence.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub webhook_url: String,
}
impl Default for Backends {
    fn default() -> Self {
        Self {
            terminal: true,
            desktop: false,
            webhook: false,
            webhook_url: String::new(),
        }
    }
}
impl Backends {
    /// The webhook URL to post to, if the webhook is on. A URL in the
    /// environment turns it on by itself.
    pub fn webhook_target(&self) -> Option<String> {
        if let Some(url) = std::env::var(URL_ENV)
            .ok()
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty())
        {
            return Some(url);
        }
        let url = self.webhook_url.trim();
        (self.webhook && !url.is_empty()).then(|| url.to_owned())
    }
}

/// The preferences file shared with the TUI's theme and Vim settings.
pub fn preferences_path() -> PathBuf {
    crate::config::config_dir().join("appearance.json")
}

/// Backends saved in the preferences file at `path`; defaults when the file
/// or its `notifications` key is missing or unreadable.
pub fn load_backends(path: &Path) -> Backends {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct File {
        notifications: Option<Backends>,
    }
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<File>(&bytes).ok())
        .and_then(|file| file.notifications)
        .unwrap_or_default()
}

/// Whether CLI runs notify: `--notify`, or `MODEL_ROUTER_NOTIFY` set to a true value.
pub fn cli_enabled(flag: bool) -> bool {
    flag || std::env::var(CLI_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// An interactive checkpoint, a handoff awaiting review, or a waiver.
    NeedsInput,
    Blocked,
    Failed,
    Complete,
    Test,
}
impl Kind {
    #[cfg_attr(test, allow(dead_code))]
    fn tag(self) -> &'static str {
        match self {
            Self::NeedsInput => "bell",
            Self::Blocked => "warning",
            Self::Failed => "x",
            Self::Complete => "white_check_mark",
            Self::Test => "test_tube",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotifyEvent {
    pub kind: Kind,
    pub title: String,
    pub body: String,
    /// Identifies the occurrence, so repeats can be dropped.
    pub key: String,
}
impl NotifyEvent {
    /// What to tell the user when a driver stops with `result`; None when
    /// the user stopped it.
    pub fn from_outcome(run: &Run, result: &std::result::Result<Action, String>) -> Option<Self> {
        let workflow = &run.workflow.name;
        let stage = run.current_name();
        let id = &run.id;
        let (kind, title, body) = match result {
            Ok(Action::Interactive) => (
                Kind::NeedsInput,
                format!("{workflow}: input needed"),
                format!("{stage} is ready in its native terminal ({id})"),
            ),
            Ok(Action::Confirm) => (
                Kind::NeedsInput,
                format!("{workflow}: input needed"),
                match run.pending_waiver() {
                    Some(_) => format!("{stage}: no check applies; accept or retry ({id})"),
                    None => format!("{stage} awaits handoff review ({id})"),
                },
            ),
            Ok(Action::Complete) => (
                Kind::Complete,
                format!("{workflow}: complete"),
                format!("Run {id} finished"),
            ),
            Ok(Action::Stopped) => {
                let kind = match run.status {
                    Status::Cancelled => return None,
                    Status::Blocked => Kind::Blocked,
                    _ => Kind::Failed,
                };
                let reason = run.error.as_deref().unwrap_or("no error recorded");
                (
                    kind,
                    format!(
                        "{workflow}: {}",
                        if kind == Kind::Blocked {
                            "blocked"
                        } else {
                            "failed"
                        }
                    ),
                    format!("{stage}: {} ({id})", first_line(reason)),
                )
            }
            Err(error) => (
                Kind::Failed,
                format!("{workflow}: failed"),
                format!("{stage}: {} ({id})", first_line(error)),
            ),
        };
        let attempts = run.stages.get(run.cursor).map_or(0, |s| s.attempts.len());
        Some(Self {
            kind,
            key: format!("{kind:?}:{id}:{}:{attempts}:{body}", run.cursor),
            title,
            body,
        })
    }
    pub fn test() -> Self {
        Self {
            kind: Kind::Test,
            title: "Model Router".into(),
            body: "Test notification".into(),
            key: String::new(),
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// `text` without control characters (C0, DEL, C1) and at most `MAX_TEXT`
/// characters, so it cannot end or extend an escape sequence.
pub fn clean(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect();
    out.truncate(out.trim_end().len());
    out
}

/// BEL plus OSC 9 and OSC 777 notifications for `event`. Inside tmux the
/// OSC sequences are wrapped for passthrough.
pub fn terminal_sequence(event: &NotifyEvent, tmux: bool) -> String {
    let title = clean(&event.title);
    let body = clean(&event.body);
    // OSC 9 text starting with a digit can read as a progress report
    // (`9;4;…`), so it always starts with the app name.
    let osc9 = format!("\x1b]9;Model Router: {title} — {body}\x07");
    // `;` separates OSC 777 fields.
    let osc777 = format!("\x1b]777;notify;{};{body}\x07", title.replace(';', ","));
    let wrap = |seq: String| {
        if tmux {
            format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
        } else {
            seq
        }
    };
    format!("\x07{}{}", wrap(osc9), wrap(osc777))
}

/// Serializes writes to the real terminal. The TUI holds it while drawing,
/// so a notification never lands inside a frame's escape sequences.
pub fn terminal_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What a delivery did, for the test button.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Delivery {
    pub sent: Vec<&'static str>,
    pub errors: Vec<String>,
}
impl Delivery {
    pub fn summary(&self) -> String {
        let mut parts = vec![];
        if self.sent.is_empty() && self.errors.is_empty() {
            parts.push("No backend is on".to_owned());
        }
        if !self.sent.is_empty() {
            parts.push(format!("Sent: {}", self.sent.join(", ")));
        }
        parts.extend(self.errors.iter().cloned());
        parts.join(" · ")
    }
}

/// Sends events through the chosen backends, dropping repeats.
pub struct Notifier {
    backends: Mutex<Backends>,
    recent: Mutex<HashMap<String, Instant>>,
    #[cfg(test)]
    pub(crate) delivered: Mutex<Vec<NotifyEvent>>,
}
impl Notifier {
    pub fn new(backends: Backends) -> Self {
        Self {
            backends: Mutex::new(backends),
            recent: Mutex::new(HashMap::new()),
            #[cfg(test)]
            delivered: Mutex::new(vec![]),
        }
    }
    pub fn backends(&self) -> Backends {
        relock(&self.backends).clone()
    }
    pub fn set_backends(&self, backends: Backends) {
        *relock(&self.backends) = backends;
    }
    /// Whether `key` was sent within `REPEAT_WINDOW` of `now`; records it otherwise.
    fn repeated(&self, key: &str, now: Instant) -> bool {
        let mut recent = relock(&self.recent);
        recent.retain(|_, at| now.duration_since(*at) < REPEAT_WINDOW);
        if recent.contains_key(key) {
            return true;
        }
        recent.insert(key.to_owned(), now);
        false
    }
    /// Sends `event` unless the same one was sent recently; false when dropped.
    pub fn notify(&self, event: &NotifyEvent) -> bool {
        if self.repeated(&event.key, Instant::now()) {
            return false;
        }
        self.deliver(event);
        true
    }
    /// Sends `event` now through every enabled backend.
    pub fn deliver(&self, event: &NotifyEvent) -> Delivery {
        let backends = self.backends();
        #[cfg(test)]
        {
            relock(&self.delivered).push(event.clone());
            let _ = backends;
            Delivery {
                sent: vec!["test"],
                errors: vec![],
            }
        }
        #[cfg(not(test))]
        {
            let mut delivery = Delivery::default();
            let mut record = |name, result: std::io::Result<()>| match result {
                Ok(()) => delivery.sent.push(name),
                Err(error) => delivery.errors.push(format!("{name}: {error}")),
            };
            if backends.terminal {
                record("terminal", write_terminal(event));
            }
            if backends.desktop {
                record("desktop", desktop(event));
            }
            if let Some(url) = backends.webhook_target() {
                webhook(url, event);
                delivery.sent.push("webhook (background)");
            }
            delivery
        }
    }
}

fn relock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The process-wide notifier, starting from the saved preferences.
pub fn global() -> &'static Notifier {
    static GLOBAL: OnceLock<Notifier> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        Notifier::new(if cfg!(test) {
            Backends::default()
        } else {
            load_backends(&preferences_path())
        })
    })
}

/// Sends `event` through the process-wide notifier.
pub fn notify(event: NotifyEvent) {
    global().notify(&event);
}

/// Waits up to the webhook timeout for webhooks still being sent. A process
/// that exits first drops them, so the CLI calls this before it ends.
pub fn wait_for_webhooks() {
    let pending = std::mem::take(&mut *relock(&WEBHOOKS));
    let deadline = Instant::now() + WEBHOOK_TIMEOUT;
    while pending.iter().any(|call| !call.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Notifies about a driver stopping with `result`, if the user should know.
pub fn notify_outcome(run: &Run, result: &std::result::Result<Action, String>) {
    if let Some(event) = NotifyEvent::from_outcome(run, result) {
        notify(event);
    }
}

#[cfg(not(test))]
fn write_terminal(event: &NotifyEvent) -> std::io::Result<()> {
    use std::io::{IsTerminal, Write};
    let sequence = terminal_sequence(event, std::env::var_os("TMUX").is_some());
    let _guard = terminal_lock();
    #[cfg(unix)]
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        tty.write_all(sequence.as_bytes())?;
        return tty.flush();
    }
    let mut stderr = std::io::stderr();
    if !stderr.is_terminal() {
        return Err(std::io::Error::other("no terminal"));
    }
    stderr.write_all(sequence.as_bytes())?;
    stderr.flush()
}

/// The desktop notifier command for this platform, as program and arguments.
pub fn desktop_command(event: &NotifyEvent) -> Option<(&'static str, Vec<String>)> {
    let (title, body) = (clean(&event.title), clean(&event.body));
    if cfg!(target_os = "macos") {
        // Title and body are script arguments, never script source.
        Some((
            "osascript",
            vec![
                "-e".into(),
                "on run argv".into(),
                "-e".into(),
                "display notification (item 2 of argv) with title (item 1 of argv)".into(),
                "-e".into(),
                "end run".into(),
                title,
                body,
            ],
        ))
    } else if cfg!(unix) {
        Some((
            "notify-send",
            vec!["--app-name=Model Router".into(), "--".into(), title, body],
        ))
    } else {
        None
    }
}

#[cfg(not(test))]
fn desktop(event: &NotifyEvent) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let (program, args) = desktop_command(event)
        .ok_or_else(|| std::io::Error::other("not supported on this platform"))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| std::io::Error::new(e.kind(), format!("{program}: {e}")))?;
    // Reaped in the background so it never becomes a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(not(test))]
fn webhook(url: String, event: &NotifyEvent) {
    let (title, body, kind) = (clean(&event.title), clean(&event.body), event.kind);
    let call = std::thread::spawn(move || {
        use reqwest::header::HeaderValue;
        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(WEBHOOK_TIMEOUT)
            .build()
        else {
            return;
        };
        let mut request = client.post(&url).body(body);
        if let Ok(value) = HeaderValue::from_bytes(title.as_bytes()) {
            request = request.header("Title", value);
        }
        request = request.header("Tags", kind.tag());
        if kind == Kind::NeedsInput {
            request = request.header("Priority", "high");
        }
        let _ = request.send();
    });
    let mut pending = relock(&WEBHOOKS);
    pending.retain(|call| !call.is_finished());
    pending.push(call);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(title: &str, body: &str) -> NotifyEvent {
        NotifyEvent {
            kind: Kind::NeedsInput,
            title: title.into(),
            body: body.into(),
            key: "k".into(),
        }
    }

    #[test]
    fn terminal_sequence_has_bel_osc9_and_osc777() {
        let seq = terminal_sequence(&event("Build: input needed", "plan is ready"), false);
        assert_eq!(
            seq,
            "\x07\x1b]9;Model Router: Build: input needed — plan is ready\x07\
             \x1b]777;notify;Build: input needed;plan is ready\x07"
        );
    }

    #[test]
    fn control_characters_cannot_escape_the_sequence() {
        let hostile = event(
            "a;b\x07\x1b]0;pwned\x1b\\",
            "line\nnext\u{9b}31m\x7f\x1b[2J",
        );
        let seq = terminal_sequence(&hostile, false);
        // Exactly the BEL and two terminators we wrote, and two ESCs.
        assert_eq!(seq.matches('\x07').count(), 3);
        assert_eq!(seq.matches('\x1b').count(), 2);
        assert!(!seq.contains('\u{9b}') && !seq.contains('\x7f') && !seq.contains('\n'));
        // The title's `;` cannot add an OSC 777 field.
        assert!(seq.contains("777;notify;a,b]0,pwned\\;line next31m[2J\x07"));
        assert_eq!(clean(&"x".repeat(1000)).chars().count(), MAX_TEXT);
        assert_eq!(clean("  tail \t\n"), "  tail");
    }

    #[test]
    fn tmux_wraps_osc_for_passthrough() {
        let seq = terminal_sequence(&event("t", "b"), true);
        assert!(seq.starts_with("\x07\x1bPtmux;\x1b\x1b]9;"));
        assert!(seq.ends_with("\x1b\x1b]777;notify;t;b\x07\x1b\\"));
    }

    #[test]
    fn repeats_are_dropped_within_the_window() {
        let notifier = Notifier::new(Backends::default());
        let first = event("t", "b");
        assert!(notifier.notify(&first));
        assert!(!notifier.notify(&first));
        let other = NotifyEvent {
            key: "other".into(),
            ..first.clone()
        };
        assert!(notifier.notify(&other));
        assert_eq!(notifier.delivered.lock().unwrap().len(), 2);
        // Old entries expire.
        let later = Instant::now() + REPEAT_WINDOW + Duration::from_secs(1);
        assert!(!notifier.repeated("k", later));
        assert!(notifier.repeated("k", later));
        // The test button bypasses the window.
        notifier.deliver(&first);
        notifier.deliver(&first);
        assert_eq!(notifier.delivered.lock().unwrap().len(), 4);
    }

    #[test]
    fn backends_default_and_load_from_old_and_new_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("appearance.json");
        assert_eq!(load_backends(&path), Backends::default());
        assert!(Backends::default().terminal && !Backends::default().desktop);
        std::fs::write(&path, r#"{"theme":"dracula","vim_motions":true}"#).unwrap();
        assert_eq!(load_backends(&path), Backends::default());
        std::fs::write(
            &path,
            r#"{"theme":"dracula","notifications":{"terminal":false,"desktop":true}}"#,
        )
        .unwrap();
        let loaded = load_backends(&path);
        assert!(!loaded.terminal && loaded.desktop && !loaded.webhook);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_backends(&path), Backends::default());
    }

    #[test]
    fn webhook_needs_a_url_and_the_toggle() {
        // The environment is process-wide; only assert when it is unset.
        if std::env::var_os(URL_ENV).is_some() {
            return;
        }
        let mut backends = Backends {
            webhook: true,
            ..Backends::default()
        };
        assert_eq!(backends.webhook_target(), None);
        backends.webhook_url = " https://ntfy.sh/topic ".into();
        assert_eq!(
            backends.webhook_target().as_deref(),
            Some("https://ntfy.sh/topic")
        );
        backends.webhook = false;
        assert_eq!(backends.webhook_target(), None);
    }

    #[test]
    fn desktop_command_passes_text_as_arguments() {
        let Some((program, args)) = desktop_command(&event("a \"b\"", "c'; rm -rf /")) else {
            return;
        };
        assert!(program == "notify-send" || program == "osascript");
        assert_eq!(args[args.len() - 2], "a \"b\"");
        assert_eq!(args[args.len() - 1], "c'; rm -rf /");
    }

    #[test]
    fn cli_flag_enables_notifications() {
        assert!(cli_enabled(true));
    }
}
