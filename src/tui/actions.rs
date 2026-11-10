//! Page-level actions: the one table behind key dispatch, footer hints, the
//! keyboard guide (`?`), the command menu (`:`), and the `Space` menu.
//!
//! To add an action:
//! 1. Add an `ActionId` variant.
//! 2. Add `act(id, scope, key, label, description)` to `ACTIONS` in its
//!    scope's section. Chain `.also()` for more keys, `.more()` for extra
//!    guide lines, and `.footer()` to show it in footers, which should fit two
//!    rows at 80 columns. Chain `.menu(group, letter)` to list it in the
//!    `Space` menu; motion keys (arrows, paging) stay out of it.
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
    TestHarness,
    // Everywhere
    Menu,
    NextView,
    ShowWorkflow,
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
/// with Runs split by what its sidebar selects: a saved run (`Runs`), or a
/// run of this session that is driving (`MonitorRunning`) or idle
/// (`MonitorIdle`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    Global,
    Workflow,
    /// A run of this session is selected, running or idle.
    Monitor,
    MonitorRunning,
    MonitorIdle,
    /// The Runs page, whatever its sidebar selects.
    Runs,
    Harnesses,
}

impl Scope {
    /// Whether actions in `self` apply in the concrete scope `here`.
    pub(super) fn covers(self, here: Scope) -> bool {
        let monitoring = matches!(here, Self::MonitorRunning | Self::MonitorIdle);
        self == here
            || self == Self::Global
            || (matches!(self, Self::Monitor | Self::Runs) && monitoring)
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Workflow => "WORKFLOW",
            Self::Monitor | Self::MonitorRunning | Self::MonitorIdle => "SELECTED RUN",
            Self::Runs => "RUNS",
            Self::Harnesses => "HARNESSES",
            Self::Global => "EVERYWHERE",
        }
    }
}

/// A `Space` menu group: the key after `Space`, then an action letter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Group {
    Run,
    Stage,
    Workflow,
    Go,
    View,
}

impl Group {
    pub(super) const ALL: [Group; 5] =
        [Self::Run, Self::Stage, Self::Workflow, Self::Go, Self::View];
    pub(super) fn key(self) -> char {
        match self {
            Self::Run => 'r',
            Self::Stage => 's',
            Self::Workflow => 'w',
            Self::Go => 'g',
            Self::View => 'v',
        }
    }
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Run => "Run",
            Self::Stage => "Stage",
            Self::Workflow => "Workflow",
            Self::Go => "Go",
            Self::View => "View",
        }
    }
    pub(super) fn from_key(key: char) -> Option<Self> {
        Self::ALL.into_iter().find(|group| group.key() == key)
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
    /// Where the `Space` menu lists it: group, then the action's letter.
    menu: Option<(Group, char)>,
    /// The scopes the menu lists it in; empty for every scope it applies to.
    menu_only: &'static [Scope],
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
    const fn menu(self, group: Group, letter: char) -> Self {
        Self {
            menu: Some((group, letter)),
            ..self
        }
    }
    /// Lists it in the `Space` menu only in `scopes`, e.g. a saved-run action
    /// that its key merely explains elsewhere.
    const fn menu_in(self, group: Group, letter: char, scopes: &'static [Scope]) -> Self {
        Self {
            menu: Some((group, letter)),
            menu_only: scopes,
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
        menu: None,
        menu_only: &[],
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
// Footers are one row: the most important hints first, since narrow
// terminals drop them from the end. `Space menu` and `? keys` close every row.
#[rustfmt::skip]
const ACTIONS: &[Entry] = {
    use ActionId::*;
    use Scope::*;
    use Group::{Go, Run as RunMenu, Stage, View, Workflow as WorkflowMenu};
    &[
        // Workflow
        act(Run, Workflow, ch('r'), "run", "Start workflow").footer().menu(RunMenu, 'r'),
        act(EditTask, Workflow, ch('t'), "task", "Edit task ([Ctrl+S] applies; [Enter] adds a line)").footer().menu(WorkflowMenu, 't'),
        act(ImportTask, Workflow, ch('f'), "import", "Import task/spec: browse files; [Ctrl+U] opens URL import")
            .more(&["Preview and edit; [Tab] append/replace; [Ctrl+S] apply; [Esc] back"])
            .menu(WorkflowMenu, 'f'),
        act(Repositories, Workflow, ch('p'), "repos", "Browse repositories (type to filter; [Tab] for recent)")
            .more(&[
                "[Enter] selects a repo or opens a folder; [→] browses inside a repo",
                "[←] goes to parent; [/], [~] or [Ctrl+L] enters a path; [Esc] cancels",
            ])
            .menu(WorkflowMenu, 'p'),
        act(ChooseWorkflow, Workflow, ch('w'), "switch", "Select/clone workflow or add a preset").menu(WorkflowMenu, 'w'),
        act(RenameWorkflow, Workflow, ch('R'), "rename", "Rename workflow").menu(WorkflowMenu, 'r'),
        act(DeleteWorkflow, Workflow, ch('X'), "delete", "Delete workflow ([y] confirms; [u] undoes)").menu(WorkflowMenu, 'X'),
        act(PreviousStage, Workflow, key(KeyCode::Left), "stage", "Select previous stage").also(&[key(KeyCode::Up), ch('k')]).alias("←→"),
        act(NextStage, Workflow, key(KeyCode::Right), "stage", "Select next stage").also(&[key(KeyCode::Down), ch('j')]).alias("←→"),
        act(EditStage, Workflow, ch('e'), "edit", "Edit selected stage").also(&[key(KeyCode::Enter)]).alias("e").footer().menu(Stage, 'e'),
        act(HarnessSettings, Workflow, ch('c'), "harness", "Configure selected harness (model, effort, execution, permissions)")
            .more(&[
                "[Tab] or [Ctrl+N/P] fields; [←/→] choices; [F7] models",
                "[Ctrl+S] applies; [Esc] cancels; [s] on Workflow saves",
            ])
            .menu(Stage, 'c'),
        act(PickHarness, Workflow, ch('h'), "pick harness", "Pick harness").menu(Stage, 'h'),
        act(Models, Workflow, ch('m'), "models", "Discover models").footer().menu(Stage, 'm'),
        act(ToggleStage, Workflow, ch('o'), "on/off", "Enable/disable stage, including optional review").footer().menu(Stage, 'o'),
        act(ToggleMode, Workflow, ch('v'), "execution", "Toggle unattended/interactive").menu(Stage, 'v'),
        act(AddStage, Workflow, ch('a'), "add", "Add stage preset (edit, then [Ctrl+S] inserts; [Esc] cancels)").menu(Stage, 'a'),
        act(DuplicateStage, Workflow, ch('d'), "duplicate", "Duplicate stage").menu(Stage, 'd'),
        act(RemoveStage, Workflow, ch('x'), "remove", "Remove stage ([u] undoes)").menu(Stage, 'x'),
        act(Undo, Workflow, ch('u'), "undo", "Undo the last stage removal or workflow deletion").menu(Stage, 'u'),
        act(MoveStageDown, Workflow, ch('J'), "move down", "Move stage down").menu(Stage, 'j'),
        act(MoveStageUp, Workflow, ch('K'), "move up", "Move stage up").menu(Stage, 'k'),
        act(ToggleWorktree, Workflow, ch('g'), "worktree", "Toggle isolated Git worktree").menu(WorkflowMenu, 'g'),
        act(RepairLimit, Workflow, ch('l'), "repairs", "Set automatic repair limit").menu(WorkflowMenu, 'l'),
        act(Preview, Workflow, ch('P'), "preview", "Preview commands (no model calls)").menu(WorkflowMenu, 'P'),
        act(Save, Workflow, ch('s'), "save", "Save project").footer().menu(WorkflowMenu, 's'),
        act(SaveGlobal, Workflow, ch('S'), "save global", "Save global templates").menu(WorkflowMenu, 'S'),
        // Monitor: while running, while idle, or both
        act(CancelStage, MonitorRunning, ch('C'), "cancel stage", "Cancel active stage ([y] confirms)").footer().menu(RunMenu, 'c'),
        act(TakeOver, Monitor, ch('t'), "terminal", "Attach to the live interactive terminal, or reopen the stage's saved session (an unattended stage stops first; [y] confirms)")
            .footer_in(&[MonitorRunning])
            .menu(RunMenu, 't'),
        act(CloseRun, MonitorIdle, ch('x'), "close", "Close this run in the session; its files stay and it moves to Saved").menu(RunMenu, 'x'),
        act(Advance, MonitorIdle, ch('a'), "advance", "Accept handoff and advance; also accepts a test stage's not-applicable waiver").footer().menu(RunMenu, 'a'),
        act(Continue, MonitorIdle, ch('c'), "continue", "Continue the run when idle (not in the moment a stage stops)").footer().menu(RunMenu, 'c'),
        act(Retry, MonitorIdle, ch('y'), "retry", "Explicitly retry current stage").footer().menu(RunMenu, 'y'),
        act(RetryInteractive, MonitorIdle, ch('i'), "interactive", "Retry current stage interactively").menu(RunMenu, 'i'),
        act(RetryCandidate, MonitorIdle, ch('Y'), "retry candidate", "Retry one failed fan-out candidate of the stopped stage; the others keep their results").menu(RunMenu, 'f'),
        act(ReplaceModel, MonitorIdle, ch('m'), "model", "Replace current model and retry").menu(RunMenu, 'm'),
        act(Handoff, Monitor, ch('h'), "handoff", "Inspect handoff/error").footer()
            .more(&["Findings by severity, tests by status; [↑↓] picks a changed file, [e]/[Enter] opens it in $EDITOR"])
            .menu(View, 'h'),
        act(EditSummary, MonitorIdle, ch('e'), "summary", "Edit interactive handoff summary").menu(RunMenu, 'e'),
        act(ScrollUp, Monitor, key(KeyCode::PageUp), "scroll", "Scroll output up")
            .alias("PgUp/PgDn")
            .footer_in(&[MonitorRunning]),
        act(ScrollDown, Monitor, key(KeyCode::PageDown), "scroll", "Scroll output down")
            .alias("PgUp/PgDn")
            .footer_in(&[MonitorRunning]),
        act(OutputTop, Monitor, key(KeyCode::Home), "top", "Jump to the oldest output (pauses following)").menu(View, 't'),
        act(OutputBottom, Monitor, key(KeyCode::End), "end", "Jump to the newest output and follow it again").menu(View, 'e'),
        act(FollowOutput, Monitor, ch('f'), "follow", "Pause or resume following new output; scrolling up also pauses").menu(View, 'f'),
        act(FindOutput, Monitor, ch('/'), "search", "Search the output log; matches are highlighted")
            .more(&["Type, then [Enter]; [n]/[N] next/previous match while a search is shown; [Esc] clears it"])
            .menu(View, '/'),
        act(OpenLog, Monitor, ch('O'), "open log", "Open the focused attempt's raw stdout.log in $PAGER (default less -R)").menu(View, 'O'),
        act(Diff, MonitorIdle, ch('d'), "diff", "Show the diff of changes made during a finished run")
            .more(&[
                "Files left, hunks right: [ ] or n/p file; { } hunk; [/] search, n/N match; [w] wrap",
                "[o] $PAGER; [e] $VISUAL/$EDITOR at the line in view; [D] git difftool --dir-diff (needs diff.tool)",
            ])
            .menu(View, 'd'),
        act(ToggleResults, MonitorIdle, ch('o'), "output", "Switch a finished run between its brief and output log").menu(View, 'o'),
        // Runs: the sidebar, whatever it selects
        act(PreviousRun, Runs, key(KeyCode::Up), "runs", "Select the previous run in the sidebar").also(&[ch('k')]).alias("↑↓").footer(),
        act(NextRun, Runs, key(KeyCode::Down), "runs", "Select the next run in the sidebar").also(&[ch('j')]).alias("↑↓").footer(),
        act(OpenRun, Runs, key(KeyCode::Enter), "open", "Open the selected saved run here (damaged: show its error)").footer_in(&[Runs]).menu_in(RunMenu, 'o', &[Runs]),
        act(RefreshRuns, Runs, ch('r'), "refresh", "Reload saved runs and sizes").menu(View, 'r'),
        act(FilterRuns, Runs, ch('F'), "filter", "Filter saved runs by ID, workflow, task, or status")
            .more(&["Type to filter; [Enter] keeps it; [Esc] clears it"])
            .footer_in(&[Runs])
            .menu(View, 'F'),
        act(CycleStatus, Runs, ch('s'), "status", "Cycle the saved runs shown: all, active, complete, failed/blocked, cancelled, damaged").footer_in(&[Runs]).menu(View, 's'),
        act(Rerun, Runs, ch('n'), "rerun", "Rerun the selected run as a new run from its saved snapshot (confirm first)").footer_in(&[Runs]).menu(RunMenu, 'n'),
        act(CompareAttempts, Runs, ch('='), "compare", "Compare two attempts of a stage of the selected run").menu(View, 'c'),
        act(CleanUp, Runs, ch('D'), "clean", "Preview removing the selected saved run and its artifacts ([y] confirms)").menu_in(RunMenu, 'D', &[Runs]),
        // Harnesses
        act(PreviousHarness, Harnesses, key(KeyCode::Up), "select", "Select previous harness").also(&[ch('k')]).alias("↑↓").footer(),
        act(NextHarness, Harnesses, key(KeyCode::Down), "select", "Select next harness").also(&[ch('j')]).alias("↑↓").footer(),
        act(HarnessDetails, Harnesses, key(KeyCode::Enter), "details", "Show version, flag checks, and notes").footer().menu(View, 'i'),
        act(RefreshHarnesses, Harnesses, ch('r'), "refresh", "Run version and flag checks again").footer().menu(View, 'r'),
        act(TestHarness, Harnesses, ch('t'), "test", "Test the selected harness with one real, possibly billed, request ([y] confirms)")
            .more(&["Runs scripts/capture_harness_output.py; needs a model-router checkout as the project"])
            .footer()
            .menu(RunMenu, 't'),
        // Everywhere
        act(Menu, Global, ch(' '), "menu", "Action menu: press a group letter, then an action letter")
            .more(&["[Backspace] returns to the groups; [Esc] closes"]),
        act(NextView, Global, key(KeyCode::Tab), "switch view", "Switch to the next view"),
        act(ShowWorkflow, Global, ch('1'), "workflow", "Show Workflow").menu(Go, 'w'),
        act(ShowRuns, Global, ch('2'), "runs", "Show Runs: this session's runs and saved ones (reloads saved runs)").menu(Go, 'r'),
        act(ShowHarnesses, Global, ch('3'), "harnesses", "Show Harnesses").menu(Go, 'h'),
        act(Themes, Global, ch('T'), "themes", "Choose theme (type to filter; [↑↓] or [Ctrl+N/P] preview)")
            .more(&["[Enter] saves for all projects; [Esc] restores the current theme"])
            .menu(View, 'T'),
        act(Stats, Global, ch('I'), "stats", "Routing stats: outcomes per role, harness, and model from saved runs (suggestions only)").menu(Go, 's'),
        act(ToggleVim, Global, key(KeyCode::F(8)), "Vim motions", "Toggle optional Vim motions in input fields (saved for all projects)")
            .more(&["Inputs start in Insert mode; Esc enters Normal mode; Esc again cancels", "Normal: h/j/k/l, w/b/e, 0/^/$, gg/G; i/a/I/A resume typing"])
            .menu(View, 'V'),
        act(Help, Global, ch('?'), "keys", "Keyboard guide").menu(View, '?'),
        act(Quit, Global, ch('q'), "quit", "Exit, asking to save unsaved workflow edits and whether to leave interactive stages running (stops other work first)").menu(Go, 'q'),
        act(Commands, Global, ch(':'), "commands", "Command menu: search this view's actions").menu(View, ':'),
        act(SessionRuns, Global, ch('L'), "session runs", "List this session's runs: status, stage, elapsed, needs input")
            .more(&["[Enter] focuses; [n] jumps to the next needing input; [C] cancels a run's stage ([y] confirms)"])
            .menu(Go, 'l'),
        act(NextNeedsInput, Global, ch('N'), "needs input", "Jump to the next run of this session that needs input").menu(Go, 'n'),
        act(Notices, Global, ch('!'), "notices", "Notice history: what this session's runs reported, newest first").menu(Go, '!'),
        act(Notifications, Global, key(KeyCode::F(9)), "notifications", "Notifications…: terminal, desktop, and webhook backends, with a test")
            .more(&["Sent when a run needs input, stops blocked or failed, or completes (saved for all projects)"])
            .menu(View, 'N'),
        act(Interrupt, Global, ctrl('c'), "cancel/exit", "Cancel work, or exit (asks to save unsaved edits)"),
    ]
};

/// Guide text for keys that are not page actions.
const GUIDE_NOTES: &str = "INTERACTIVE HARNESS
\tNative terminal keys and paste are forwarded.
\tExit the harness to inspect its handoff; /exit quits, not detaches.
Ctrl+B\tDetach to Runs; the session keeps running. [t] reattaches.
\t[q] can leave it running after mr exits; the next mr reattaches.
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

/// Footer hints for a page's scope, as (key, label), less those in `skip`.
pub(super) fn footer(here: Scope, skip: &[ActionId]) -> Vec<(String, &'static str)> {
    let mut hints: Vec<_> = available(here)
        .filter(|entry| !skip.contains(&entry.id))
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

/// The `Space` menu for `here`: each group with its actions as (letter,
/// entry), in table order. Groups with nothing to do here are left out.
/// Actions the `Space` menu and the `:` command menu list in `here`.
pub(super) fn in_menu(here: Scope) -> impl Iterator<Item = &'static Entry> {
    available(here)
        .filter(move |entry| entry.menu_only.is_empty() || entry.menu_only.contains(&here))
}

pub(super) fn menu(here: Scope) -> Vec<(Group, Vec<(char, &'static Entry)>)> {
    Group::ALL
        .into_iter()
        .map(|group| {
            let items: Vec<_> = in_menu(here)
                .filter_map(|entry| match entry.menu {
                    Some((g, letter)) if g == group => Some((letter, entry)),
                    _ => None,
                })
                .collect();
            (group, items)
        })
        .filter(|(_, items)| !items.is_empty())
        .collect()
}

/// The action at `letter` in `group` of the `Space` menu for `here`.
pub(super) fn menu_action(here: Scope, group: Group, letter: char) -> Option<ActionId> {
    in_menu(here)
        .find(|entry| entry.menu == Some((group, letter)))
        .map(|entry| entry.id)
}

/// The keyboard guide in `guide_text` format: every action under its page.
pub(super) fn guide() -> String {
    let mut text = String::new();
    for heading in [
        "WORKFLOW",
        "RUNS",
        "SELECTED RUN",
        "HARNESSES",
        "EVERYWHERE",
    ] {
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
    fn menu_letters_are_unique_in_each_group_and_reach_their_action() {
        for here in EVERY_SCOPE {
            for (group, items) in menu(here) {
                for (index, (letter, entry)) in items.iter().enumerate() {
                    assert!(
                        items[index + 1..].iter().all(|(l, _)| l != letter),
                        "{here:?}: Space {} {letter} is listed twice",
                        group.key()
                    );
                    assert_eq!(menu_action(here, group, *letter), Some(entry.id));
                }
            }
        }
        // Each view's actions are reachable; a running stage cannot advance.
        let workflow = menu(Scope::Workflow);
        assert!(workflow.iter().any(|(g, _)| *g == Group::Stage));
        assert_eq!(
            menu_action(Scope::Workflow, Group::Stage, 'o'),
            Some(ActionId::ToggleStage)
        );
        assert_eq!(menu_action(Scope::MonitorRunning, Group::Run, 'a'), None);
        assert_eq!(
            menu_action(Scope::MonitorIdle, Group::Run, 'a'),
            Some(ActionId::Advance)
        );
        // Opening and removing a saved run are listed only beside one.
        for (letter, id) in [('o', ActionId::OpenRun), ('D', ActionId::CleanUp)] {
            assert_eq!(menu_action(Scope::Runs, Group::Run, letter), Some(id));
            assert_eq!(menu_action(Scope::MonitorIdle, Group::Run, letter), None);
        }
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
