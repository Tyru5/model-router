//! The results view's diff dialog: a unified diff split into files and
//! hunks, coloured by the theme, with a file list, hunk and file jumps,
//! search, and hand-offs to `$PAGER`, `$EDITOR`, and `git difftool`.
//!
//! Only the rows that fit are laid out each frame, and each file keeps at
//! most `FILE_ROWS` rows; the pager shows the rest.
use super::{
    output_search::{find, highlight},
    theme::Palette,
    widgets::rows_for,
};
use crate::storage;
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{cell::Cell, ops::Range};

/// Rows kept per file; the pager shows a longer file whole.
pub(super) const FILE_ROWS: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// `diff --git a/x b/x`
    File,
    /// index, mode, rename, similarity, `---`, and `+++` lines.
    Meta,
    Hunk,
    Add,
    Del,
    Context,
    /// `\ No newline at end of file`
    NoNewline,
    /// `Binary files … differ` or `GIT binary patch`.
    Binary,
    /// Anything else, such as a truncation note.
    Note,
}

/// Classifies every line of a patch, tracking hunk line counts so a removed
/// line that starts with `--` is not mistaken for a header. A trailing `\r`
/// (CRLF files or patches) is dropped.
pub(super) fn classify(patch: &str) -> Vec<(Kind, &str)> {
    let mut out = vec![];
    let (mut old, mut new) = (0usize, 0usize);
    let body = patch.strip_suffix('\n').unwrap_or(patch);
    if body.is_empty() {
        return out;
    }
    for raw in body.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with('\\') {
            out.push((Kind::NoNewline, line));
            continue;
        }
        if old > 0 || new > 0 {
            let kind = match line.as_bytes().first() {
                Some(b'+') => {
                    new = new.saturating_sub(1);
                    Some(Kind::Add)
                }
                Some(b'-') => {
                    old = old.saturating_sub(1);
                    Some(Kind::Del)
                }
                // Some tools strip the space of an empty context line.
                Some(b' ') | None => {
                    old = old.saturating_sub(1);
                    new = new.saturating_sub(1);
                    Some(Kind::Context)
                }
                _ => {
                    (old, new) = (0, 0);
                    None
                }
            };
            if let Some(kind) = kind {
                out.push((kind, line));
                continue;
            }
        }
        let kind = if line.starts_with("diff ") {
            Kind::File
        } else if let Some((o, n)) = hunk_counts(line) {
            (old, new) = (o, n);
            Kind::Hunk
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            Kind::Binary
        } else if [
            "--- ",
            "+++ ",
            "index ",
            "new file mode",
            "deleted file mode",
            "old mode",
            "new mode",
            "similarity index",
            "dissimilarity index",
            "rename from ",
            "rename to ",
            "copy from ",
            "copy to ",
        ]
        .iter()
        .any(|p| line.starts_with(p))
        {
            Kind::Meta
        } else {
            Kind::Note
        };
        out.push((kind, line));
    }
    out
}

/// `@@ -a,b +c,d @@` → (old start, old count, new start, new count).
fn hunk_range(line: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let part = |text: &str| -> Option<(usize, usize)> {
        match text.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    let (a, b) = part(old)?;
    let (c, d) = part(new)?;
    Some((a, b, c, d))
}

fn hunk_counts(line: &str) -> Option<(usize, usize)> {
    hunk_range(line).map(|(_, old, _, new)| (old, new))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
}
impl FileStatus {
    pub(super) fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::Renamed => 'R',
            Self::Copied => 'C',
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) kind: Kind,
    pub(super) text: String,
    /// The new-side line number of added and context lines, and a hunk's
    /// first new line.
    pub(super) line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DiffFile {
    /// Path before; None for an added file.
    pub(super) old: Option<String>,
    /// Path after; None for a deleted file.
    pub(super) new: Option<String>,
    pub(super) status: FileStatus,
    pub(super) binary: bool,
    pub(super) adds: usize,
    pub(super) dels: usize,
    pub(super) rows: Vec<Row>,
    /// Rows past `FILE_ROWS`, not kept.
    pub(super) hidden: usize,
    /// Indices of hunk header rows.
    pub(super) hunks: Vec<usize>,
}
impl DiffFile {
    fn new() -> Self {
        Self {
            old: None,
            new: None,
            status: FileStatus::Modified,
            binary: false,
            adds: 0,
            dels: 0,
            rows: vec![],
            hidden: 0,
            hunks: vec![],
        }
    }
    /// The path to show: `old → new` for renames and copies.
    pub(super) fn label(&self) -> String {
        match (&self.old, &self.new) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            (_, Some(path)) | (Some(path), None) => path.clone(),
            (None, None) => "(patch notes)".into(),
        }
    }
    /// Where the editor opens: the file after the change.
    pub(super) fn path(&self) -> Option<&str> {
        self.new.as_deref()
    }
    fn push(&mut self, row: Row) {
        if self.rows.len() >= FILE_ROWS {
            self.hidden += 1;
        } else {
            if row.kind == Kind::Hunk {
                self.hunks.push(self.rows.len());
            }
            self.rows.push(row);
        }
    }
}

/// Strips `a/`/`b/`, quotes, and a plain diff's trailing timestamp.
fn header_path(text: &str, prefix: &str) -> Option<String> {
    let text = text.split('\t').next().unwrap_or(text).trim_end();
    if text == "/dev/null" {
        return None;
    }
    let text = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text);
    Some(text.strip_prefix(prefix).unwrap_or(text).to_owned())
}

/// `diff --git a/x b/y` → (x, y); ambiguous only when a path holds " b/".
fn git_paths(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?;
    let (a, b) = rest
        .rsplit_once(" b/")
        .or_else(|| rest.rsplit_once(" \"b/"))?;
    let a = a.trim_matches('"');
    let b = b.trim_end_matches('"');
    Some((a.strip_prefix("a/").unwrap_or(a).to_owned(), b.to_owned()))
}

/// A parsed patch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Diff {
    pub(super) files: Vec<DiffFile>,
}
impl Diff {
    pub(super) fn parse(patch: &str) -> Self {
        let mut files: Vec<DiffFile> = vec![];
        let mut next_line = 0usize;
        for (kind, text) in classify(patch) {
            let starts_file = match kind {
                Kind::File => true,
                // A plain (non-Git) diff starts each file with `---`.
                Kind::Meta if text.starts_with("--- ") => files
                    .last()
                    .is_none_or(|f| !f.hunks.is_empty() || f.hidden > 0),
                _ => files.is_empty(),
            };
            if starts_file {
                files.push(DiffFile::new());
            }
            let file = files.last_mut().expect("a file was started");
            let mut line = None;
            match kind {
                Kind::File => {
                    if let Some((old, new)) = git_paths(text) {
                        file.old = Some(old);
                        file.new = Some(new);
                    }
                }
                Kind::Meta => {
                    if let Some(path) = text.strip_prefix("--- ") {
                        file.old = header_path(path, "a/");
                        if file.old.is_none() {
                            file.status = FileStatus::Added;
                        }
                    } else if let Some(path) = text.strip_prefix("+++ ") {
                        file.new = header_path(path, "b/");
                        if file.new.is_none() {
                            file.status = FileStatus::Deleted;
                        }
                    } else if text.starts_with("new file mode") {
                        file.status = FileStatus::Added;
                        file.old = None;
                    } else if text.starts_with("deleted file mode") {
                        file.status = FileStatus::Deleted;
                        file.new = None;
                    } else if let Some(path) = text.strip_prefix("rename from ") {
                        file.status = FileStatus::Renamed;
                        file.old = header_path(path, "");
                    } else if let Some(path) = text.strip_prefix("rename to ") {
                        file.status = FileStatus::Renamed;
                        file.new = header_path(path, "");
                    } else if let Some(path) = text.strip_prefix("copy from ") {
                        file.status = FileStatus::Copied;
                        file.old = header_path(path, "");
                    } else if let Some(path) = text.strip_prefix("copy to ") {
                        file.status = FileStatus::Copied;
                        file.new = header_path(path, "");
                    }
                }
                Kind::Hunk => {
                    next_line = hunk_range(text).map_or(1, |(_, _, start, _)| start);
                    line = Some(next_line.max(1));
                }
                Kind::Add | Kind::Context => {
                    file.adds += usize::from(kind == Kind::Add);
                    line = Some(next_line);
                    next_line += 1;
                }
                Kind::Del => file.dels += 1,
                Kind::Binary => file.binary = true,
                Kind::NoNewline | Kind::Note => {}
            }
            file.push(Row {
                kind,
                text: storage::clean_terminal(text).replace('\t', "    "),
                line,
            });
        }
        Self { files }
    }

    pub(super) fn totals(&self) -> (usize, usize) {
        self.files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels))
    }

    /// "3 files · +40 −5"
    pub(super) fn stat(&self) -> String {
        let (adds, dels) = self.totals();
        let files = self.files.len();
        format!(
            "{files} file{} · +{adds} −{dels}",
            if files == 1 { "" } else { "s" }
        )
    }
}

/// The style of a row's text.
pub(super) fn row_style(kind: Kind, palette: Palette) -> Style {
    match kind {
        Kind::File => palette.diff_file(),
        Kind::Meta => palette.diff_meta(),
        Kind::Hunk => palette.diff_hunk(),
        Kind::Add => palette.diff_add(),
        Kind::Del => palette.diff_del(),
        Kind::Context => Style::default().fg(palette.fg),
        Kind::NoNewline | Kind::Binary | Kind::Note => Style::default()
            .fg(palette.muted)
            .add_modifier(Modifier::ITALIC),
    }
}

/// The patch with ANSI colours, for the pager.
pub(super) fn ansi(patch: &str) -> String {
    let mut out = String::with_capacity(patch.len() + patch.len() / 8);
    for (kind, text) in classify(patch) {
        let code = match kind {
            Kind::File => "1",
            Kind::Meta => "1",
            Kind::Hunk => "36",
            Kind::Add => "32",
            Kind::Del => "31",
            Kind::NoNewline | Kind::Binary | Kind::Note => "2",
            Kind::Context => "",
        };
        let text = storage::clean_terminal(text);
        if code.is_empty() {
            out.push_str(&text);
        } else {
            out.push_str(&format!("\x1b[{code}m{text}\x1b[0m"));
        }
        out.push('\n');
    }
    out
}

/// What a key asks of the app.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DiffAction {
    Stay,
    Close,
    Pager,
    /// Open the selected file at this new-side line.
    Editor(String, Option<usize>),
    Difftool,
}

/// The diff dialog's state.
pub(super) struct DiffView {
    pub(super) diff: Diff,
    /// The patch as Git produced it, for the pager.
    pub(super) patch: String,
    pub(super) file: usize,
    /// First row of the file shown.
    pub(super) scroll: usize,
    pub(super) wrap: bool,
    pub(super) query: String,
    pub(super) typing: bool,
    /// Every match: (file, row, byte range).
    pub(super) matches: Vec<(usize, usize, Range<usize>)>,
    pub(super) current: usize,
    /// Rows of the hunk pane, as last drawn.
    pub(super) height: Cell<usize>,
    pub(super) note: String,
}

impl DiffView {
    pub(super) fn new(patch: String) -> Self {
        Self {
            diff: Diff::parse(&patch),
            patch,
            file: 0,
            scroll: 0,
            wrap: false,
            query: String::new(),
            typing: false,
            matches: vec![],
            current: 0,
            height: Cell::new(20),
            note: String::new(),
        }
    }

    fn rows(&self) -> &[Row] {
        self.diff.files.get(self.file).map_or(&[], |f| &f.rows)
    }

    fn last_scroll(&self) -> usize {
        self.rows().len().saturating_sub(self.height.get().max(1))
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.last_scroll());
    }

    pub(super) fn select_file(&mut self, file: usize) {
        if file < self.diff.files.len() {
            self.file = file;
            self.scroll = 0;
        }
    }

    fn step_file(&mut self, forward: bool) {
        let count = self.diff.files.len();
        if count == 0 {
            return;
        }
        self.select_file(if forward {
            (self.file + 1) % count
        } else {
            (self.file + count - 1) % count
        });
    }

    /// Next/previous hunk, moving into the next/previous file at the ends.
    pub(super) fn step_hunk(&mut self, forward: bool) {
        let Some(file) = self.diff.files.get(self.file) else {
            return;
        };
        let found = if forward {
            file.hunks.iter().find(|&&h| h > self.scroll).copied()
        } else {
            file.hunks.iter().rev().find(|&&h| h < self.scroll).copied()
        };
        if let Some(row) = found {
            self.scroll = row;
            return;
        }
        let count = self.diff.files.len();
        let mut index = self.file;
        for _ in 1..count {
            index = if forward {
                (index + 1) % count
            } else {
                (index + count - 1) % count
            };
            let hunks = &self.diff.files[index].hunks;
            let target = if forward { hunks.first() } else { hunks.last() };
            if let Some(&row) = target {
                self.file = index;
                self.scroll = row;
                return;
            }
        }
    }

    /// Recomputes matches for the query over every file's kept rows.
    fn search(&mut self) {
        let query = self.query.as_str();
        self.matches = if query.is_empty() {
            vec![]
        } else {
            self.diff
                .files
                .iter()
                .enumerate()
                .flat_map(|(f, file)| {
                    file.rows.iter().enumerate().flat_map(move |(r, row)| {
                        find(&row.text, query)
                            .into_iter()
                            .map(move |range| (f, r, range))
                    })
                })
                .collect()
        };
        // The first match at or after the view.
        self.current = self
            .matches
            .iter()
            .position(|(f, r, _)| (*f, *r) >= (self.file, self.scroll))
            .unwrap_or(0);
    }

    fn show_match(&mut self) {
        let Some((file, row, _)) = self.matches.get(self.current) else {
            return;
        };
        let (file, row) = (*file, *row);
        if file != self.file {
            self.file = file;
            self.scroll = 0;
        }
        let height = self.height.get().max(1);
        if row < self.scroll || row >= self.scroll + height {
            self.scroll = row.saturating_sub(height / 3).min(self.last_scroll());
        }
    }

    fn step_match(&mut self, forward: bool) {
        let count = self.matches.len();
        if count == 0 {
            self.note = format!("No matches for “{}”.", self.query);
            return;
        }
        self.current = if forward {
            (self.current + 1) % count
        } else {
            (self.current + count - 1) % count
        };
        self.show_match();
    }

    /// The new-side line to open: the first numbered row in view, else the
    /// file's first hunk.
    pub(super) fn editor_line(&self) -> Option<usize> {
        let rows = self.rows();
        rows.iter()
            .skip(self.scroll)
            .take(self.height.get().max(1))
            .find_map(|r| r.line)
            .or_else(|| rows.iter().find_map(|r| r.line))
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> DiffAction {
        self.note.clear();
        let plain = key.modifiers - Mods::SHIFT == Mods::NONE;
        if self.typing {
            match key.code {
                Key::Esc => {
                    self.typing = false;
                    self.query.clear();
                    self.search();
                }
                Key::Enter => {
                    self.typing = false;
                    if self.matches.is_empty() && !self.query.is_empty() {
                        self.note = format!("No matches for “{}”.", self.query);
                    }
                    self.show_match();
                }
                Key::Backspace => {
                    self.query.pop();
                    self.search();
                    self.show_match();
                }
                Key::Char(c) if plain => {
                    self.query.push(c);
                    self.search();
                    self.show_match();
                }
                _ => {}
            }
            return DiffAction::Stay;
        }
        if !plain && key.code != Key::BackTab {
            return DiffAction::Stay;
        }
        let page = self.height.get().max(2) as isize - 1;
        match key.code {
            Key::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.search();
            }
            Key::Esc | Key::Char('q') => return DiffAction::Close,
            Key::Up | Key::Char('k') => self.scroll_by(-1),
            Key::Down | Key::Char('j') => self.scroll_by(1),
            Key::PageUp => self.scroll_by(-page),
            Key::PageDown | Key::Char(' ') => self.scroll_by(page),
            Key::Home | Key::Char('g') => self.scroll = 0,
            Key::End | Key::Char('G') => self.scroll = self.last_scroll(),
            Key::Char(']') | Key::Tab => self.step_file(true),
            Key::Char('[') | Key::BackTab => self.step_file(false),
            Key::Char('}') => self.step_hunk(true),
            Key::Char('{') => self.step_hunk(false),
            Key::Char('/') => {
                self.typing = true;
                self.query.clear();
                self.search();
            }
            // With a search, n/N step through matches; otherwise n/p step files.
            Key::Char('n') if !self.query.is_empty() => self.step_match(true),
            Key::Char('N') if !self.query.is_empty() => self.step_match(false),
            Key::Char('n') => self.step_file(true),
            Key::Char('p') => self.step_file(false),
            Key::Char('w') => self.wrap = !self.wrap,
            Key::Char('o') => return DiffAction::Pager,
            Key::Char('D') => return DiffAction::Difftool,
            Key::Char('e') | Key::Enter => {
                let Some(file) = self.diff.files.get(self.file) else {
                    return DiffAction::Stay;
                };
                match file.path() {
                    Some(path) if !file.binary => {
                        return DiffAction::Editor(path.to_owned(), self.editor_line());
                    }
                    Some(path) => return DiffAction::Editor(path.to_owned(), None),
                    None => self.note = "The file was deleted; nothing to open.".into(),
                }
            }
            _ => {}
        }
        DiffAction::Stay
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let title = format!(
            "Changes during this run · {}{}",
            self.diff.stat(),
            if self.wrap { " · wrap" } else { "" }
        );
        let block = palette.modal(&title, &[("Esc", "close")]);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let [body, footer] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(inner);
        let longest = self
            .diff
            .files
            .iter()
            .map(|f| f.label().chars().count() + 14)
            .max()
            .unwrap_or(20) as u16;
        let list_width = longest.clamp(18, (body.width / 3).max(18));
        let [list, pane] =
            Layout::horizontal([Constraint::Length(list_width), Constraint::Min(10)]).areas(body);
        self.draw_files(frame, list, palette);
        self.draw_hunks(frame, pane, palette);
        let line = if self.typing {
            Line::from(vec![
                Span::styled("/", Style::default().fg(palette.accent)),
                Span::styled(format!("{}▏", self.query), Style::default().fg(palette.fg)),
                Span::styled(
                    format!(
                        "  {} matches · [Enter] keeps · [Esc] clears",
                        self.matches.len()
                    ),
                    Style::default().fg(palette.muted),
                ),
            ])
        } else if !self.note.is_empty() {
            Line::styled(self.note.clone(), Style::default().fg(palette.warn))
        } else if !self.query.is_empty() {
            let mut hint = palette.hints(&[("n/N", "match"), ("Esc", "clear search")]);
            hint.spans.insert(
                0,
                Span::styled(
                    format!(
                        "/{} · {}/{}   ",
                        self.query,
                        if self.matches.is_empty() {
                            0
                        } else {
                            self.current + 1
                        },
                        self.matches.len()
                    ),
                    Style::default().fg(palette.accent),
                ),
            );
            hint
        } else {
            palette.hints(&[
                ("[ ]", "file"),
                ("{ }", "hunk"),
                ("/", "search"),
                ("w", "wrap"),
                ("o", "pager"),
                ("e", "editor"),
                ("D", "difftool"),
            ])
        };
        frame.render_widget(Paragraph::new(line), footer);
    }

    fn draw_files(&self, frame: &mut Frame<'_>, area: Rect, palette: Palette) {
        let items = self.diff.files.iter().map(|file| {
            let color = match file.status {
                FileStatus::Added => palette.success,
                FileStatus::Deleted => palette.red,
                FileStatus::Renamed | FileStatus::Copied => palette.blue,
                FileStatus::Modified => palette.warn,
            };
            let mut spans = vec![
                Span::styled(
                    format!("{} ", file.status.letter()),
                    Style::default().fg(color),
                ),
                Span::raw(file.label()),
            ];
            if file.binary {
                spans.push(Span::styled(" bin", Style::default().fg(palette.muted)));
            } else {
                spans.push(Span::styled(format!(" +{}", file.adds), palette.diff_add()));
                spans.push(Span::styled(format!(" −{}", file.dels), palette.diff_del()));
            }
            ListItem::new(Line::from(spans))
        });
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(palette.selected_style())
                .highlight_symbol("▌")
                .highlight_spacing(HighlightSpacing::Always)
                .block(
                    Block::default()
                        .borders(Borders::RIGHT)
                        .border_style(Style::default().fg(palette.line)),
                ),
            area,
            &mut ListState::default().with_selected(Some(self.file)),
        );
    }

    fn draw_hunks(&self, frame: &mut Frame<'_>, area: Rect, palette: Palette) {
        let area = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            ..area
        };
        let [head, pane] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        self.height.set(usize::from(pane.height));
        let Some(file) = self.diff.files.get(self.file) else {
            frame.render_widget(
                Paragraph::new("No changes.").style(Style::default().fg(palette.muted)),
                pane,
            );
            return;
        };
        let hunk = file.hunks.iter().rposition(|&h| h <= self.scroll);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    file.label(),
                    Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    match (hunk, file.hunks.len()) {
                        (_, 0) => String::new(),
                        (None, 1) => "  1 hunk".into(),
                        (None, count) => format!("  {count} hunks"),
                        (Some(h), count) => format!("  hunk {}/{count}", h + 1),
                    },
                    Style::default().fg(palette.muted),
                ),
            ])),
            head,
        );
        // Lay out only rows that can show.
        let height = usize::from(pane.height);
        let width = pane.width.max(1);
        let first = self.matches.partition_point(|(f, _, _)| *f < self.file);
        let mut lines = vec![];
        let mut used = 0usize;
        for (index, row) in file.rows.iter().enumerate().skip(self.scroll) {
            if used >= height {
                break;
            }
            let line = Line::styled(row.text.clone(), row_style(row.kind, palette));
            let start = first
                + self.matches[first..].partition_point(|(f, r, _)| *f == self.file && *r < index);
            let count = self.matches[start..]
                .iter()
                .take_while(|(f, r, _)| *f == self.file && *r == index)
                .count();
            let line = if count == 0 {
                line
            } else {
                let ranges: Vec<_> = self.matches[start..start + count]
                    .iter()
                    .map(|(_, _, r)| r.clone())
                    .collect();
                highlight(
                    line,
                    &ranges,
                    palette.search_match(),
                    self.current.checked_sub(start).filter(|&i| i < count),
                    palette.search_current(),
                )
            };
            used += if self.wrap {
                usize::from(rows_for(&line, width, u16::MAX))
            } else {
                1
            };
            lines.push(line);
        }
        let end = self.scroll + lines.len();
        if end >= file.rows.len() {
            let muted = Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::ITALIC);
            if file.hidden > 0 {
                lines.push(Line::styled(
                    format!("… {} more lines, open in pager [o]", file.hidden),
                    muted,
                ));
            } else if file.binary && file.hunks.is_empty() {
                lines.push(Line::styled("Binary file; no text diff to show.", muted));
            }
        }
        let paragraph = Paragraph::new(lines);
        let paragraph = if self.wrap {
            paragraph.wrap(Wrap { trim: false })
        } else {
            paragraph
        };
        frame.render_widget(paragraph, pane);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn one() {}
--- a removed line that looks like a header
+++ an added line that looks like a header
+fn two() {}
 fn three() {}
@@ -10,2 +11,2 @@ fn later()
-old
+new
 tail
diff --git a/old name.txt b/new name.txt
similarity index 90%
rename from old name.txt
rename to new name.txt
--- a/old name.txt
+++ b/new name.txt
@@ -1 +1 @@
-before
+after
\\ No newline at end of file
diff --git a/image.png b/image.png
new file mode 100644
index 0000000..3333333
Binary files /dev/null and b/image.png differ
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 4444444..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-b
";

    #[test]
    fn parses_files_hunks_renames_binaries_and_deletions() {
        let diff = Diff::parse(PATCH);
        let labels: Vec<_> = diff.files.iter().map(DiffFile::label).collect();
        assert_eq!(
            labels,
            [
                "src/lib.rs",
                "old name.txt → new name.txt",
                "image.png",
                "gone.txt"
            ]
        );
        let lib = &diff.files[0];
        assert_eq!((lib.adds, lib.dels, lib.hunks.len()), (3, 2, 2));
        assert_eq!(lib.status, FileStatus::Modified);
        // Hunk-count tracking keeps look-alike lines as content.
        let kinds: Vec<_> = lib.rows[4..9].iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [Kind::Hunk, Kind::Context, Kind::Del, Kind::Add, Kind::Add]
        );
        assert_eq!(lib.rows[4].line, Some(1));
        assert_eq!(lib.rows[8].line, Some(3), "new-side line numbers");
        let rename = &diff.files[1];
        assert_eq!(rename.status, FileStatus::Renamed);
        assert_eq!(rename.old.as_deref(), Some("old name.txt"));
        assert_eq!(rename.path(), Some("new name.txt"));
        assert_eq!(rename.rows.last().unwrap().kind, Kind::NoNewline);
        assert_eq!((rename.adds, rename.dels), (1, 1));
        let image = &diff.files[2];
        assert!(image.binary && image.hunks.is_empty());
        assert_eq!(image.status, FileStatus::Added);
        let gone = &diff.files[3];
        assert_eq!(gone.status, FileStatus::Deleted);
        assert_eq!(gone.path(), None);
        assert_eq!(diff.stat(), "4 files · +4 −5");
    }

    #[test]
    fn crlf_patches_parse_like_lf_ones() {
        let crlf = PATCH.replace('\n', "\r\n");
        assert_eq!(Diff::parse(&crlf), Diff::parse(PATCH));
        // Content lines of a CRLF file keep their text without the \r.
        let file =
            "diff --git a/w.txt b/w.txt\n--- a/w.txt\n+++ b/w.txt\n@@ -1 +1 @@\n-x\r\n+y\r\n";
        let diff = Diff::parse(file);
        assert_eq!(diff.files[0].rows.last().unwrap().text, "+y");
        assert_eq!((diff.files[0].adds, diff.files[0].dels), (1, 1));
    }

    #[test]
    fn plain_diffs_and_notes_parse() {
        let plain = "--- a.txt\t2024-01-01\n+++ b.txt\t2024-01-02\n@@ -1 +1 @@\n-a\n+b\n--- c.txt\n+++ c.txt\n@@ -1 +1,2 @@\n x\n+y\n[Patch truncated after 40 bytes]\n";
        let diff = Diff::parse(plain);
        assert_eq!(diff.files.len(), 2);
        assert_eq!(diff.files[0].label(), "a.txt → b.txt");
        assert_eq!(diff.files[1].rows.last().unwrap().kind, Kind::Note);
        assert!(Diff::parse("").files.is_empty());
    }

    #[test]
    fn long_files_keep_a_bounded_number_of_rows() {
        let mut patch = String::from("diff --git a/big b/big\n--- a/big\n+++ b/big\n");
        let lines = FILE_ROWS + 50;
        patch.push_str(&format!("@@ -0,0 +1,{lines} @@\n"));
        for i in 0..lines {
            patch.push_str(&format!("+line {i}\n"));
        }
        let diff = Diff::parse(&patch);
        let file = &diff.files[0];
        assert_eq!(file.rows.len(), FILE_ROWS);
        assert_eq!(file.hidden, 54);
        assert_eq!(file.adds, lines, "stats count every line");
    }

    fn press(view: &mut DiffView, code: Key) -> DiffAction {
        view.key(KeyEvent::new(code, Mods::NONE))
    }

    #[test]
    fn keys_step_files_hunks_and_matches() {
        let mut view = DiffView::new(PATCH.into());
        view.height.set(3);
        assert_eq!(press(&mut view, Key::Char('}')), DiffAction::Stay);
        assert_eq!((view.file, view.scroll), (0, 4));
        press(&mut view, Key::Char('}'));
        assert_eq!((view.file, view.scroll), (0, 10));
        // Past the last hunk: the next file's first hunk.
        press(&mut view, Key::Char('}'));
        assert_eq!((view.file, view.scroll), (1, 6));
        press(&mut view, Key::Char('{'));
        assert_eq!((view.file, view.scroll), (0, 10));
        press(&mut view, Key::Char(']'));
        assert_eq!((view.file, view.scroll), (1, 0));
        press(&mut view, Key::Char('n'));
        assert_eq!(view.file, 2, "n steps files without a search");
        press(&mut view, Key::Char('p'));
        press(&mut view, Key::Char('['));
        assert_eq!(view.file, 0);
        press(&mut view, Key::Char('['));
        assert_eq!(view.file, 3, "files wrap around");

        press(&mut view, Key::Char('/'));
        for c in "after".chars() {
            press(&mut view, Key::Char(c));
        }
        assert!(view.typing);
        press(&mut view, Key::Enter);
        assert_eq!(view.matches.len(), 1);
        assert_eq!(view.file, 1, "the match's file is shown");
        assert_eq!(
            view.rows()[view.scroll..]
                .iter()
                .filter(|r| r.text == "+after")
                .count(),
            1
        );
        press(&mut view, Key::Char('/'));
        for c in "fn".chars() {
            press(&mut view, Key::Char(c));
        }
        press(&mut view, Key::Enter);
        assert_eq!(view.matches.len(), 4);
        let first = view.current;
        press(&mut view, Key::Char('n'));
        assert_eq!(view.current, (first + 1) % 4);
        press(&mut view, Key::Char('N'));
        assert_eq!(view.current, first);
        assert_eq!(view.file, 0);
        // Esc clears the search first, then closes.
        assert_eq!(press(&mut view, Key::Esc), DiffAction::Stay);
        assert!(view.query.is_empty() && view.matches.is_empty());
        assert_eq!(press(&mut view, Key::Esc), DiffAction::Close);
    }

    #[test]
    fn external_keys_name_the_file_and_line() {
        let mut view = DiffView::new(PATCH.into());
        view.height.set(4);
        assert_eq!(press(&mut view, Key::Char('o')), DiffAction::Pager);
        assert_eq!(press(&mut view, Key::Char('D')), DiffAction::Difftool);
        assert_eq!(
            press(&mut view, Key::Char('e')),
            DiffAction::Editor("src/lib.rs".into(), Some(1))
        );
        press(&mut view, Key::Char('}'));
        press(&mut view, Key::Char('}'));
        assert_eq!(
            press(&mut view, Key::Char('e')),
            DiffAction::Editor("src/lib.rs".into(), Some(11))
        );
        view.select_file(3);
        assert_eq!(press(&mut view, Key::Char('e')), DiffAction::Stay);
        assert!(view.note.contains("deleted"));
        press(&mut view, Key::Char('w'));
        assert!(view.wrap);
    }

    #[test]
    fn ansi_colours_every_kind_for_the_pager() {
        let text = ansi(PATCH);
        assert!(text.contains("\x1b[32m+fn two() {}\x1b[0m"));
        assert!(text.contains("\x1b[31m-old\x1b[0m"));
        assert!(text.contains("\x1b[36m@@ -10,2 +11,2 @@ fn later()\x1b[0m"));
        assert!(text.contains("\n fn three() {}\n"));
        // Escape sequences inside the patch cannot reach the terminal.
        assert!(!ansi("+\x1b]0;title\x07\n").contains("\x1b]"));
    }
}
