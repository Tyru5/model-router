//! Page-level actions: the one table behind key dispatch, footer hints, the
//! keyboard guide (`?`), and the command menu (`:`).
//!
//! To add an action:
//! 1. Add an `ActionId` variant.
//! 2. Add `act(id, scope, key, label, description)` to `ACTIONS` in its
//!    scope's section. Chain `.also()` for more keys, `.more()` for extra
//!    guide lines, and `.footer()` to show it in footers, which should fit two
//!    rows at 80 columns.
//! 3. Handle the variant in its scope's perform function: `perform_workflow`
//!    (workflow_page.rs), `perform_monitor` (monitor.rs), `perform_runs`
//!    (history.rs), `perform_harnesses` (harnesses.rs), or `perform_global`
//!    (app.rs).
//!
//! The guide and menu then list it. Tests reject duplicate bindings, including
//! page keys that shadow global ones. Dialogs (pickers, editors, importers, the
//! repository browser) keep handling their own keys.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActionId {
    // Workflow
    Run,
    EditTask,
    ImportTask,
    Repositories,
    ChooseWorkflow,
    RenameWorkflow,
    DeleteWorkflow,
    PreviousStage,
    NextStage,
    EditStage,
    HarnessSettings,
    PickHarness,
    Models,
    ToggleStage,
    ToggleMode,
    AddStage,
    DuplicateStage,
    RemoveStage,
    Undo,
    MoveStageDown,
    MoveStageUp,
    ToggleWorktree,
    RepairLimit,
    Preview,
    Save,
    SaveGlobal,
    // Monitor
    CancelStage,
    TakeOver,
    PreviousSessionRun,
    NextSessionRun,
    CloseRun,
    Advance,
    Continue,
    Retry,
    RetryInteractive,
    RetryCandidate,
    ReplaceModel,
    Handoff,
    EditSummary,
    ScrollUp,
    ScrollDown,
    OutputTop,
    OutputBottom,
    FollowOutput,
    FindOutput,
    OpenLog,
    Diff,
    ToggleResults,
    BackToWorkflow,
    // Runs
    PreviousRun,
    NextRun,
    OpenRun,
    RefreshRuns,
    FilterRuns,
    CycleStatus,
    Rerun,
    CompareAttempts,
    CleanUp,
    // Harnesses
    PreviousHarness,
    NextHarness,
    HarnessDetails,
    RefreshHarnesses,
    // Everywhere
    NextView,
    ShowWorkflow,
    ShowMonitor,
    ShowRuns,
    ShowHarnesses,
    Themes,
    Stats,
    ToggleVim,
    Help,
    Quit,
    Commands,
    SessionRuns,
    NextNeedsInput,
    Notices,
    Notifications,
    Interrupt,
}

/// Where an action applies. The app is always in one concrete scope: a page,
/// with Monitor split by whether a run is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    Global,
    Workflow,
    /// Monitor, running or idle.
    Monitor,
    MonitorRunning,
    MonitorIdle,
    Runs,
    Harnesses,
}

impl Scope {
    /// Whether actions in `self` apply in the concrete scope `here`.
    pub(super) fn covers(self, here: Scope) -> bool {
        self == here
            || self == Self::Global
            || (self == Self::Monitor && matches!(here, Self::MonitorRunning | Self::MonitorIdle))
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Workflow => "WORKFLOW",
            Self::Monitor | Self::MonitorRunning | Self::MonitorIdle => "MONITOR",
            Self::Runs => "RUNS",
            Self::Harnesses => "HARNESSES",
            Self::Global => "EVERYWHERE",
        }
    }
}

enum Footer {
    No,
    /// In every footer the action applies to.
    Yes,
    /// Only in these footers; each must fit two rows at 80 columns.
    Only(&'static [Scope]),
}

type Binding = (KeyCode, KeyModifiers);

pub(super) struct Entry {
    pub(super) id: ActionId,
    pub(super) scope: Scope,
    key: Binding,
    /// More keys for the same action, e.g. Enter for `e`.
    also: &'static [Binding],
    /// Footer text for the key, e.g. "↑↓" for an up/down pair. Identical
    /// adjacent hints show once.
    alias: Option<&'static str>,
    /// Footer and menu label.
    pub(super) label: &'static str,
    /// Guide and menu description.
    pub(super) help: &'static str,
    /// Guide lines under `help`.
    pub(super) more: &'static [&'static str],
    footer: Footer,
}

impl Entry {
    /// Every binding, as the guide and menu show them: "e / Enter".
    pub(super) fn keys(&self) -> String {
        std::iter::once(&self.key)
            .chain(self.also)
            .map(|&binding| key_name(binding))
            .collect::<Vec<_>>()
            .join(" / ")
    }

    const fn also(self, also: &'static [Binding]) -> Self {
        Self { also, ..self }
    }
    const fn alias(self, alias: &'static str) -> Self {
        Self {
            alias: Some(alias),
            ..self
        }
    }
    const fn more(self, more: &'static [&'static str]) -> Self {
        Self { more, ..self }
    }
    const fn footer(self) -> Self {
        Self {
            footer: Footer::Yes,
            ..self
        }
    }
    const fn footer_in(self, scopes: &'static [Scope]) -> Self {
        Self {
            footer: Footer::Only(scopes),
            ..self
        }
    }
}

const fn act(
    id: ActionId,
    scope: Scope,
    key: Binding,
    label: &'static str,
    help: &'static str,
) -> Entry {
    Entry {
        id,
        scope,
        key,
        also: &[],
        alias: None,
        label,
        help,
        more: &[],
        footer: Footer::No,
    }
}
const fn ch(c: char) -> Binding {
    (KeyCode::Char(c), KeyModifiers::NONE)
}
const fn key(code: KeyCode) -> Binding {
    (code, KeyModifiers::NONE)
}
const fn ctrl(c: char) -> Binding {
    (KeyCode::Char(c), KeyModifiers::CONTROL)
}

// Table order is guide, menu, and footer order; page actions precede global ones.
#[rustfmt::skip]
const ACTIONS: &[Entry] = {
    use ActionId::*;
    use Scope::*;
    &[
        // Workflow
        act(Run, Workflow, ch('r'), "run", "Start workflow").footer(),
        act(EditTask, Workflow, ch('t'), "task", "Edit task ([Ctrl+S] applies; [Enter] adds a line)").footer(),
        act(ImportTask, Workflow, ch('f'), "import", "Import task/spec: browse files; [Ctrl+U] opens URL import")
            .more(&["Preview and edit; [Tab] append/replace; [Ctrl+S] apply; [Esc] back"])
            .footer(),
        act(Repositories, Workflow, ch('p'), "repos", "Browse repositories (type to filter; [Tab] for recent)")
            .more(&[
                "[Enter] selects a repo or opens a folder; [→] browses inside a repo",
                "[←] goes to parent; [/], [~] or [Ctrl+L] enters a path; [Esc] cancels",
            ])
            .footer(),
        act(ChooseWorkflow, Workflow, ch('w'), "switch", "Select/clone workflow or add a preset"),
        act(RenameWorkflow, Workflow, ch('R'), "rename", "Rename workflow"),
        act(DeleteWorkflow, Workflow, ch('X'), "delete", "Delete workflow ([y] confirms; [u] undoes)"),
        act(PreviousStage, Workflow, key(KeyCode::Up), "select", "Select previous stage").also(&[ch('k')]),
        act(NextStage, Workflow, key(KeyCode::Down), "select", "Select next stage").also(&[ch('j')]),
        act(EditStage, Workflow, ch('e'), "edit", "Edit selected stage").also(&[key(KeyCode::Enter)]),
        act(HarnessSettings, Workflow, ch('c'), "harness", "Configure selected harness (model, effort, execution, permissions)")
            .more(&[
                "[Tab] or [Ctrl+N/P] fields; [←/→] choices; [F7] models",
                "[Ctrl+S] applies; [Esc] cancels; [s] on Workflow saves",
            ])
            .footer(),
        act(PickHarness, Workflow, ch('h'), "pick harness", "Pick harness"),
        act(Models, Workflow, ch('m'), "models", "Discover models").footer(),
        act(ToggleStage, Workflow, ch(' '), "toggle", "Enable/disable stage, including optional review").footer(),
        act(ToggleMode, Workflow, ch('v'), "execution", "Toggle unattended/interactive"),
        act(AddStage, Workflow, ch('a'), "add", "Add stage preset (edit, then [Ctrl+S] inserts; [Esc] cancels)"),
        act(DuplicateStage, Workflow, ch('d'), "duplicate", "Duplicate stage"),
        act(RemoveStage, Workflow, ch('x'), "remove", "Remove stage ([u] undoes)"),
        act(Undo, Workflow, ch('u'), "undo", "Undo the last stage removal or workflow deletion"),
        act(MoveStageDown, Workflow, ch('J'), "move down", "Move stage down"),
        act(MoveStageUp, Workflow, ch('K'), "move up", "Move stage up"),
        act(ToggleWorktree, Workflow, ch('g'), "worktree", "Toggle isolated Git worktree"),
        act(RepairLimit, Workflow, ch('l'), "repairs", "Set automatic repair limit"),
        act(Preview, Workflow, ch('P'), "preview", "Preview commands (no model calls)"),
        act(Save, Workflow, ch('s'), "save", "Save project").footer(),
        act(SaveGlobal, Workflow, ch('S'), "save global", "Save global templates"),
        // Monitor: while running, while idle, or both
        act(CancelStage, MonitorRunning, ch('C'), "cancel stage", "Cancel active stage ([y] confirms)").footer(),
        act(TakeOver, Monitor, ch('t'), "terminal", "Attach to the live interactive terminal, or reopen the stage's saved session (an unattended stage stops first; [y] confirms)")
            .footer_in(&[MonitorRunning]),
        act(PreviousSessionRun, Monitor, ch('['), "runs", "Show the previous run of this session").alias("[ ]").footer_in(&[MonitorRunning]),
        act(NextSessionRun, Monitor, ch(']'), "runs", "Show the next run of this session").alias("[ ]").footer_in(&[MonitorRunning]),
        act(CloseRun, MonitorIdle, ch('x'), "close", "Close this run in the session; its files stay and Runs [3] reopens it"),
        act(Advance, MonitorIdle, ch('a'), "advance", "Accept handoff and advance; also accepts a test stage's not-applicable waiver").footer(),
        act(Continue, MonitorIdle, ch('c'), "continue", "Continue the run when idle (not in the moment a stage stops)").footer(),
        act(Retry, MonitorIdle, ch('y'), "retry", "Explicitly retry current stage").footer(),
        act(RetryInteractive, MonitorIdle, ch('i'), "interactive", "Retry current stage interactively").footer(),
        act(RetryCandidate, MonitorIdle, ch('Y'), "retry candidate", "Retry one failed fan-out candidate of the stopped stage; the others keep their results"),
        act(ReplaceModel, MonitorIdle, ch('m'), "model", "Replace current model and retry").footer(),
        act(Handoff, Monitor, ch('h'), "handoff", "Inspect handoff/error").footer()
            .more(&["Findings by severity, tests by status; [↑↓] picks a changed file, [e]/[Enter] opens it in $EDITOR"]),
        act(EditSummary, MonitorIdle, ch('e'), "summary", "Edit interactive handoff summary").footer(),
        act(ScrollUp, Monitor, key(KeyCode::PageUp), "scroll", "Scroll output up")
            .alias("PgUp/PgDn")
            .footer_in(&[MonitorRunning]),
        act(ScrollDown, Monitor, key(KeyCode::PageDown), "scroll", "Scroll output down")
            .alias("PgUp/PgDn")
            .footer_in(&[MonitorRunning]),
        act(OutputTop, Monitor, key(KeyCode::Home), "top", "Jump to the oldest output (pauses following)"),
        act(OutputBottom, Monitor, key(KeyCode::End), "end", "Jump to the newest output and follow it again"),
        act(FollowOutput, Monitor, ch('f'), "follow", "Pause or resume following new output; scrolling up also pauses"),
        act(FindOutput, Monitor, ch('/'), "search", "Search the output log; matches are highlighted")
            .more(&["Type, then [Enter]; [n]/[N] next/previous match while a search is shown; [Esc] clears it"]),
        act(OpenLog, Monitor, ch('O'), "open log", "Open the focused attempt's raw stdout.log in $PAGER (default less -R)"),
        act(Diff, MonitorIdle, ch('d'), "diff", "Show the diff of changes made during a finished run")
            .more(&[
                "Files left, hunks right: [ ] or n/p file; { } hunk; [/] search, n/N match; [w] wrap",
                "[o] $PAGER; [e] $VISUAL/$EDITOR at the line in view; [D] git difftool --dir-diff (needs diff.tool)",
            ]),
        act(ToggleResults, MonitorIdle, ch('o'), "output", "Switch a finished run between results and output log"),
        act(BackToWorkflow, MonitorIdle, ch('n'), "workflow", "Return to workflow editor").footer(),
        // Runs
        act(PreviousRun, Runs, key(KeyCode::Up), "select", "Select previous run").also(&[ch('k')]).alias("↑↓").footer(),
        act(NextRun, Runs, key(KeyCode::Down), "select", "Select next run").also(&[ch('j')]).alias("↑↓").footer(),
        act(OpenRun, Runs, key(KeyCode::Enter), "open", "Open selected run in the monitor (damaged: show its error)").footer(),
        act(RefreshRuns, Runs, ch('r'), "refresh", "Reload saved runs and sizes"),
        act(FilterRuns, Runs, ch('/'), "filter", "Filter by ID, workflow, task, or status")
            .more(&["Type to filter; [Enter] keeps it; [Esc] clears it"])
            .footer(),
        act(CycleStatus, Runs, ch('f'), "status", "Cycle status: all, active, complete, failed/blocked, cancelled, damaged").footer(),
        act(Rerun, Runs, ch('n'), "rerun", "Rerun as a new run from the saved snapshot (confirm first)").footer(),
        act(CompareAttempts, Runs, ch('c'), "compare", "Compare two attempts of a stage").footer(),
        act(CleanUp, Runs, ch('D'), "clean", "Preview removing the run and its artifacts ([y] confirms)").footer(),
        // Harnesses
        act(PreviousHarness, Harnesses, key(KeyCode::Up), "select", "Select previous harness").also(&[ch('k')]).alias("↑↓").footer(),
        act(NextHarness, Harnesses, key(KeyCode::Down), "select", "Select next harness").also(&[ch('j')]).alias("↑↓").footer(),
        act(HarnessDetails, Harnesses, key(KeyCode::Enter), "details", "Show version, flag checks, and notes").footer(),
        act(RefreshHarnesses, Harnesses, ch('r'), "refresh", "Run version and flag checks again").footer(),
        // Everywhere
        act(NextView, Global, key(KeyCode::Tab), "switch view", "Switch to the next view")
            .footer_in(&[MonitorRunning, Runs, Harnesses]),
        act(ShowWorkflow, Global, ch('1'), "workflow", "Show Workflow"),
        act(ShowMonitor, Global, ch('2'), "monitor", "Show Monitor"),
        act(ShowRuns, Global, ch('3'), "runs", "Show Runs (reloads saved runs)"),
        act(ShowHarnesses, Global, ch('4'), "harnesses", "Show Harnesses"),
        act(Themes, Global, ch('T'), "themes", "Choose theme (type to filter; [↑↓] or [Ctrl+N/P] preview)")
            .more(&["[Enter] saves for all projects; [Esc] restores the current theme"])
            .footer(),
        act(Stats, Global, ch('I'), "stats", "Routing stats: outcomes per role, harness, and model from saved runs (suggestions only)"),
        act(ToggleVim, Global, key(KeyCode::F(8)), "Vim motions", "Toggle optional Vim motions in input fields (saved for all projects)")
            .more(&["Inputs start in Insert mode; Esc enters Normal mode; Esc again cancels", "Normal: h/j/k/l, w/b/e, 0/^/$, gg/G; i/a/I/A resume typing"]),
        act(Help, Global, ch('?'), "help", "Keyboard guide").footer_in(&[Workflow, MonitorIdle, Harnesses]),
        act(Quit, Global, ch('q'), "quit", "Exit, asking to save unsaved workflow edits (stops active work first)")
            .footer_in(&[Workflow, MonitorRunning, Runs, Harnesses]),
        act(Commands, Global, ch(':'), "commands", "Command menu: search this view's actions")
            .footer_in(&[MonitorRunning, Runs, Harnesses]),
        act(SessionRuns, Global, ch('L'), "session runs", "List this session's runs: status, stage, elapsed, needs input")
            .more(&["[Enter] focuses; [n] jumps to the next needing input; [C] cancels a run's stage ([y] confirms)"]),
        act(NextNeedsInput, Global, ch('N'), "needs input", "Jump to the next run of this session that needs input"),
        act(Notices, Global, ch('!'), "notices", "Notice history: what this session's runs reported, newest first"),
        act(Notifications, Global, key(KeyCode::F(9)), "notifications", "Notifications…: terminal, desktop, and webhook backends, with a test")
            .more(&["Sent when a run needs input, stops blocked or failed, or completes (saved for all projects)"]),
        act(Interrupt, Global, ctrl('c'), "cancel/exit", "Cancel work, or exit (asks to save unsaved edits)"),
    ]
};

/// Guide text for keys that are not page actions.
const GUIDE_NOTES: &str = "INTERACTIVE HARNESS
\tNative terminal keys and paste are forwarded.
\tExit the harness to inspect its handoff; /exit quits, not detaches.
Ctrl+B\tDetach to Monitor; the session keeps running. [t] reattaches.
Ctrl+]\tStop a stuck stage

SELECTION MENUS
Ctrl+N / Ctrl+P\tNext/previous option (same as [↓]/[↑])

NOTES
Saved runs snapshot their workflow. Template edits affect new runs.
Retries retain filesystem changes. Disabled reviews never gate a run.
Artifacts: <project>/.model-router/runs/
Provider credentials stay with each harness. Catalogs do not prove universal access.";

pub(super) fn entry(id: ActionId) -> Option<&'static Entry> {
    ACTIONS.iter().find(|entry| entry.id == id)
}

/// Actions usable in the concrete scope `here`, in table order.
pub(super) fn available(here: Scope) -> impl Iterator<Item = &'static Entry> {
    ACTIONS.iter().filter(move |entry| entry.scope.covers(here))
}

/// The action `key` triggers in `here`. Modifiers must match exactly, so
/// Ctrl+R is not `r` and Ctrl+C is not `c`. Shift is implied by the character
/// itself (Shift+T arrives as `T`), so it is ignored for character keys.
pub(super) fn resolve(here: Scope, key: KeyEvent) -> Option<ActionId> {
    let mut mods = key.modifiers;
    if matches!(key.code, KeyCode::Char(_)) {
        mods.remove(KeyModifiers::SHIFT);
    }
    available(here)
        .flat_map(|entry| {
            std::iter::once(&entry.key)
                .chain(entry.also)
                .map(move |binding| (entry.id, binding))
        })
        .find(|(_, (code, bound))| *code == key.code && *bound == mods)
        .map(|(id, _)| id)
}

/// Footer hints for a page's scope, as (key, label).
pub(super) fn footer(here: Scope) -> Vec<(String, &'static str)> {
    let mut hints: Vec<_> = available(here)
        .filter(|entry| match entry.footer {
            Footer::No => false,
            Footer::Yes => true,
            Footer::Only(scopes) => scopes.contains(&here),
        })
        .map(|entry| {
            let key = entry.alias.map_or_else(|| entry.keys(), str::to_owned);
            (key, entry.label)
        })
        .collect();
    hints.dedup();
    hints
}

/// The keyboard guide in `guide_text` format: every action under its page.
pub(super) fn guide() -> String {
    let mut text = String::new();
    for heading in ["WORKFLOW", "MONITOR", "RUNS", "HARNESSES", "EVERYWHERE"] {
        text.push_str(heading);
        text.push('\n');
        for entry in ACTIONS.iter().filter(|e| e.scope.heading() == heading) {
            text.push_str(&format!("{}\t{}\n", entry.keys(), entry.help));
            for line in entry.more {
                text.push_str(&format!("\t{line}\n"));
            }
        }
        text.push('\n');
    }
    text + GUIDE_NOTES
}

fn key_name((code, mods): Binding) -> String {
    let name = match code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) if mods.contains(KeyModifiers::CONTROL) => c.to_ascii_uppercase().into(),
        KeyCode::Char(c) => c.into(),
        // crossterm's own names differ by platform ("Return" on macOS).
        KeyCode::Enter => "Enter".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::PageUp => "PgUp".into(),
        KeyCode::PageDown => "PgDn".into(),
        code => code.to_string(),
    };
    let mut text = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        text.push_str("Ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        text.push_str("Alt+");
    }
    text + &name
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_SCOPE: [Scope; 6] = [
        Scope::Global,
        Scope::Workflow,
        Scope::MonitorRunning,
        Scope::MonitorIdle,
        Scope::Runs,
        Scope::Harnesses,
    ];

    #[test]
    fn bindings_are_unique_within_each_scope_and_resolve_to_their_action() {
        for (index, entry) in ACTIONS.iter().enumerate() {
            assert!(
                ACTIONS[index + 1..].iter().all(|e| e.id != entry.id),
                "{:?} is listed twice",
                entry.id
            );
        }
        for here in EVERY_SCOPE {
            let mut seen = vec![];
            for entry in available(here) {
                for &binding in std::iter::once(&entry.key).chain(entry.also) {
                    // Page keys share one namespace with global keys.
                    assert!(
                        !seen.contains(&binding),
                        "{here:?}: {} is bound twice",
                        key_name(binding)
                    );
                    seen.push(binding);
                    assert_eq!(
                        resolve(here, KeyEvent::new(binding.0, binding.1)),
                        Some(entry.id),
                        "{here:?}: {}",
                        key_name(binding)
                    );
                }
            }
        }
        let press = |here, code, mods| resolve(here, KeyEvent::new(code, mods));
        let (shift, control) = (KeyModifiers::SHIFT, KeyModifiers::CONTROL);
        assert_eq!(
            press(Scope::Runs, KeyCode::Char('T'), shift),
            Some(ActionId::Themes)
        );
        assert_eq!(
            press(Scope::MonitorRunning, KeyCode::Char('c'), control),
            Some(ActionId::Interrupt)
        );
        assert_eq!(
            press(Scope::Global, KeyCode::Char('r'), KeyModifiers::NONE),
            None
        );
        // Extra modifiers never reach a plain page key.
        for (here, c) in [
            (Scope::Workflow, 'r'),
            (Scope::Workflow, 'x'),
            (Scope::Workflow, 'd'),
            (Scope::MonitorIdle, 'c'),
            (Scope::Runs, 'D'),
        ] {
            let plain = press(here, KeyCode::Char(c), KeyModifiers::NONE);
            assert!(plain.is_some());
            for mods in [control, KeyModifiers::ALT, control | shift] {
                assert_ne!(press(here, KeyCode::Char(c), mods), plain, "{c} {mods:?}");
            }
        }
        for c in ['r', 'x', 'd'] {
            assert_eq!(press(Scope::Workflow, KeyCode::Char(c), control), None);
        }
        assert_eq!(press(Scope::Workflow, KeyCode::Enter, control), None);
        // Ctrl+C stays distinct from the page's `c`, idle or running.
        assert_eq!(
            press(Scope::MonitorIdle, KeyCode::Char('c'), control),
            Some(ActionId::Interrupt)
        );
        assert_eq!(
            press(Scope::MonitorRunning, KeyCode::Char('C'), shift),
            Some(ActionId::CancelStage)
        );
        assert_eq!(
            press(
                Scope::MonitorRunning,
                KeyCode::Char('c'),
                KeyModifiers::NONE
            ),
            None
        );
    }

    #[test]
    fn guide_lists_every_action_with_its_keys() {
        let guide = guide();
        let lines: Vec<_> = guide.lines().collect();
        for entry in ACTIONS {
            let line = format!("{}\t{}", entry.keys(), entry.help);
            assert!(lines.contains(&line.as_str()), "Guide lacks {line:?}");
            for more in entry.more {
                assert!(lines.contains(&format!("\t{more}").as_str()));
            }
        }
        assert!(lines.contains(&"e / Enter\tEdit selected stage"));
        assert!(lines.contains(&"Ctrl+C\tCancel work, or exit (asks to save unsaved edits)"));
    }
}
