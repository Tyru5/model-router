use super::input::InputField;
use super::theme::Palette;
use super::widgets::{content, menu_key, textarea};
use crate::{catalog, storage};
use anyhow::{Context, Result, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{HighlightSpacing, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tui_textarea::CursorMove;

struct Entry {
    path: PathBuf,
    repository: bool,
    directory: bool,
}

impl Entry {
    fn new(path: PathBuf) -> Self {
        // Linked worktrees and submodules use a .git file instead of a directory.
        let git = path.join(".git");
        Self {
            directory: path.is_dir(),
            path,
            repository: git.is_dir() || git.is_file(),
        }
    }
    fn name(&self) -> String {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }
}

pub(super) enum PickerAction {
    Stay,
    Select(PathBuf),
    Close,
}

pub(super) struct PathPicker {
    directory: PathBuf,
    entries: Vec<Entry>,
    recent: Vec<Entry>,
    show_recent: bool,
    selected: usize,
    filter: InputField<'static>,
    path_input: Option<InputField<'static>>,
    pub error: String,
    files: bool,
}

impl PathPicker {
    pub(super) fn active_input(&mut self) -> &mut InputField<'static> {
        self.path_input.as_mut().unwrap_or(&mut self.filter)
    }
    pub fn new(project: &Path, recent: Vec<PathBuf>) -> Result<Self> {
        let mut picker = Self {
            directory: project.into(),
            entries: vec![],
            recent: recent
                .into_iter()
                .filter(|p| p.is_dir())
                .map(Entry::new)
                .collect(),
            show_recent: false,
            selected: 0,
            filter: textarea(""),
            path_input: None,
            error: String::new(),
            files: false,
        };
        let parent = project.parent().unwrap_or(project);
        picker.visit(parent).or_else(|_| picker.visit(project))?;
        picker.select_path(project);
        Ok(picker)
    }

    pub fn files(project: &Path) -> Result<Self> {
        let mut picker = Self {
            directory: project.into(),
            entries: vec![],
            recent: vec![],
            show_recent: false,
            selected: 0,
            filter: textarea(""),
            path_input: None,
            error: String::new(),
            files: true,
        };
        picker.visit(project)?;
        Ok(picker)
    }

    fn visit(&mut self, path: &Path) -> Result<()> {
        let directory = catalog::check_project(path)?;
        let mut entries: Vec<_> = fs::read_dir(&directory)
            .with_context(|| format!("Cannot browse {}", directory.display()))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_name() != ".git"
                    && (entry.path().is_dir() || (self.files && entry.path().is_file()))
            })
            .map(|entry| Entry::new(entry.path()))
            .collect();
        entries.sort_by_cached_key(|entry| {
            (
                if self.files {
                    !entry.directory
                } else {
                    !entry.repository
                },
                entry.name().to_lowercase(),
                entry.path.clone(),
            )
        });
        // Only replace the view after a successful read, so failed navigation is recoverable.
        self.directory = directory;
        self.entries = entries;
        self.show_recent = false;
        self.selected = 0;
        self.filter = textarea("");
        self.error.clear();
        Ok(())
    }

    fn entries(&self) -> &[Entry] {
        if self.show_recent {
            &self.recent
        } else {
            &self.entries
        }
    }

    // None represents the "Use this folder" row. It disappears while filtering.
    fn matches(&self) -> Vec<Option<usize>> {
        let query = content(&self.filter).to_lowercase();
        let mut matches = vec![];
        if !self.files && !self.show_recent && query.is_empty() {
            matches.push(None);
        }
        matches.extend(
            self.entries()
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    let label = if self.show_recent {
                        entry.path.display().to_string()
                    } else {
                        entry.name()
                    };
                    label.to_lowercase().contains(&query).then_some(Some(index))
                }),
        );
        matches
    }

    fn select_path(&mut self, path: &Path) {
        self.selected = self
            .matches()
            .iter()
            .position(|index| index.is_some_and(|index| self.entries()[index].path == path))
            .unwrap_or(0);
    }

    fn highlighted(&self) -> Option<(PathBuf, bool)> {
        let matches = self.matches();
        match matches.get(self.selected.min(matches.len().saturating_sub(1)))? {
            Some(index) => {
                let entry = &self.entries()[*index];
                Some((
                    entry.path.clone(),
                    if self.files {
                        !entry.directory
                    } else {
                        self.show_recent || entry.repository
                    },
                ))
            }
            None => Some((self.directory.clone(), true)),
        }
    }

    fn resolve_path(&self, value: &str) -> Result<PathBuf> {
        let value = value.trim();
        ensure!(!value.is_empty(), "Enter a directory path");
        if value == "~" || value.starts_with("~/") {
            let dirs = directories::BaseDirs::new().context("Home directory unavailable")?;
            Ok(dirs.home_dir().join(value.strip_prefix("~/").unwrap_or("")))
        } else {
            Ok(self.directory.join(value))
        }
    }

    pub fn paste(&mut self, value: &str) {
        let field = self.path_input.as_mut().unwrap_or(&mut self.filter);
        field.insert_str(value.replace(['\r', '\n'], ""));
        self.selected = 0;
        self.error.clear();
    }

    pub fn key(&mut self, key: KeyEvent) -> Result<PickerAction> {
        self.error.clear();
        if let Some(input) = &mut self.path_input {
            match key.code {
                Key::Esc => self.path_input = None,
                Key::Enter => {
                    let value = content(input);
                    let path = self.resolve_path(&value)?;
                    if self.files && !path.is_dir() {
                        return Ok(PickerAction::Select(path));
                    }
                    self.visit(&path)?;
                    self.path_input = None;
                }
                _ => {
                    input.input(key);
                }
            }
            return Ok(PickerAction::Stay);
        }
        if key.modifiers.contains(Mods::CONTROL) && key.code == Key::Char('l') {
            let mut input = textarea(&self.directory.display().to_string());
            input.select_all();
            self.path_input = Some(input);
            return Ok(PickerAction::Stay);
        }
        // Multiplexers often claim Ctrl+L, so starting a path with / or ~ also
        // opens path entry; folder names cannot contain / anyway.
        if let Key::Char(c @ ('/' | '~')) = key.code {
            if !key.modifiers.contains(Mods::CONTROL) && content(&self.filter).is_empty() {
                let mut input = textarea(&c.to_string());
                input.move_cursor(CursorMove::End);
                self.path_input = Some(input);
                return Ok(PickerAction::Stay);
            }
        }
        match menu_key(key) {
            Key::Esc => return Ok(PickerAction::Close),
            Key::Tab | Key::BackTab => {
                if !self.files {
                    self.show_recent = !self.show_recent;
                    self.filter = textarea("");
                    self.selected = 0;
                }
            }
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => {
                self.selected = (self.selected + 1).min(self.matches().len().saturating_sub(1))
            }
            Key::PageUp => self.selected = self.selected.saturating_sub(10),
            Key::PageDown => {
                self.selected = (self.selected + 10).min(self.matches().len().saturating_sub(1))
            }
            Key::Left => {
                let previous = self.directory.clone();
                let parent = previous.parent().unwrap_or(&previous);
                self.visit(parent)?;
                self.select_path(&previous);
            }
            Key::Enter | Key::Right => {
                if let Some((path, selectable)) = self.highlighted() {
                    if key.code == Key::Enter && selectable {
                        return Ok(PickerAction::Select(path));
                    }
                    if !self.files || !selectable {
                        self.visit(&path)?;
                    }
                }
            }
            _ => {
                self.filter.input(key);
                self.selected = 0;
            }
        }
        Ok(PickerAction::Stay)
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let block = palette.modal(
            if self.files {
                "Import task/spec · choose file"
            } else {
                "Choose repository"
            },
            &[],
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(if self.error.is_empty() { 2 } else { 4 }),
            Constraint::Length(3),
        ])
        .split(inner);
        if self.files {
            frame.render_widget(
                Paragraph::new("Markdown / text files · [Ctrl+U] import a URL")
                    .style(Style::default().fg(palette.muted)),
                chunks[0],
            );
        } else {
            frame.render_widget(
                Tabs::new(["Folders", "Recent"])
                    .select(usize::from(self.show_recent))
                    .style(Style::default().fg(palette.muted))
                    .highlight_style(palette.selected_style()),
                chunks[0],
            );
        }
        frame.render_widget(
            Paragraph::new(storage::clean_terminal(
                &self.directory.display().to_string(),
            ))
            .style(Style::default().fg(palette.fg))
            .wrap(Wrap { trim: false }),
            chunks[1],
        );
        if let Some(input) = &mut self.path_input {
            palette.style_textarea(input);
            input.set_block(palette.panel(if self.files {
                " File or directory path · [Enter] open · [Esc] back "
            } else {
                " Directory path · [Enter] browse · [Esc] back "
            }));
            frame.render_widget(&*input, chunks[2]);
        } else {
            palette.style_textarea(&mut self.filter);
            self.filter
                .set_block(palette.panel(" Type to filter · [/] [~] or [Ctrl+L] enter path "));
            frame.render_widget(&self.filter, chunks[2]);
        }
        let matches = self.matches();
        self.selected = self.selected.min(matches.len().saturating_sub(1));
        if matches.is_empty() {
            let message = if self.show_recent && content(&self.filter).is_empty() {
                "No recent projects. Press [Tab] to browse folders."
            } else if self.files {
                "No matching files or folders. Clear the filter, then type [/] or [~] (or [Ctrl+L])."
            } else {
                "No matching folders. Clear the filter, then type [/] or [~] (or [Ctrl+L])."
            };
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().fg(palette.muted))
                    .wrap(Wrap { trim: false }),
                chunks[3],
            );
        } else {
            let rows = matches.iter().map(|index| match index {
                None => ListItem::new(Line::from(vec![
                    Span::styled("[use]     ", Style::default().fg(palette.accent)),
                    Span::styled(
                        "Use this folder",
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ])),
                Some(index) => {
                    let entry = &self.entries()[*index];
                    let label = if self.show_recent {
                        entry.path.display().to_string()
                    } else {
                        entry.name()
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            if !entry.directory {
                                "[file]    "
                            } else if entry.repository && !self.files {
                                "[repo]    "
                            } else {
                                "[folder]  "
                            },
                            Style::default().fg(if !entry.directory {
                                palette.blue
                            } else if entry.repository {
                                palette.accent
                            } else {
                                palette.muted
                            }),
                        ),
                        Span::raw(storage::clean_terminal(&label)),
                    ]))
                }
            });
            frame.render_stateful_widget(
                List::new(rows)
                    .highlight_style(palette.selected_style())
                    .highlight_symbol("▌ ")
                    .highlight_spacing(HighlightSpacing::Always),
                chunks[3],
                &mut ListState::default().with_selected(Some(self.selected)),
            );
        }
        let detail = if self.error.is_empty() {
            self.highlighted()
                .map(|(path, _)| path.display().to_string())
                .unwrap_or_default()
        } else {
            self.error.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        frame.render_widget(
            Paragraph::new(storage::clean_terminal(&detail))
                .style(Style::default().fg(if self.error.is_empty() {
                    palette.muted
                } else {
                    palette.red
                }))
                .wrap(Wrap { trim: false }),
            chunks[4],
        );
        frame.render_widget(
            Paragraph::new(if self.files {
                vec![
                    palette.hints(&[
                        ("↑↓ / Ctrl+N/P", "select"),
                        ("Enter", "open folder / preview file"),
                        ("←", "parent"),
                    ]),
                    palette.hints(&[
                        ("/ / ~ / Ctrl+L", "enter path"),
                        ("Ctrl+U", "URL"),
                        ("Esc", "cancel"),
                    ]),
                    Line::styled(
                        "Preview, edit, then append to or replace your task.",
                        Style::default().fg(palette.muted),
                    ),
                ]
            } else {
                vec![
                    palette.hints(&[
                        ("↑↓ / Ctrl+N/P", "select"),
                        ("Enter", "choose repo / open folder"),
                        ("→", "browse repo"),
                    ]),
                    palette.hints(&[
                        ("←", "parent"),
                        ("Tab", "recent/folders"),
                        ("Esc", "cancel"),
                    ]),
                    Line::styled(
                        "Switching projects saves workflow edits; task/run view resets.",
                        Style::default().fg(palette.muted),
                    ),
                ]
            })
            .wrap(Wrap { trim: false }),
            chunks[5],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_file_mode_browses_directories_and_selects_files() {
        let temp = tempfile::tempdir().unwrap();
        // Canonical root: macOS temp dirs live behind the /var -> /private/var symlink.
        let dir = fs::canonicalize(temp.path()).unwrap();
        let docs = dir.join("docs");
        fs::create_dir(&docs).unwrap();
        let spec = docs.join("Feature 雪.md");
        fs::write(&spec, "# A task").unwrap();
        let mut picker = PathPicker::files(&dir).unwrap();
        picker.paste("docs");
        assert!(matches!(
            key(&mut picker, Key::Enter).unwrap(),
            PickerAction::Stay
        ));
        assert_eq!(picker.directory, docs);
        assert!(
            !picker.matches().contains(&None),
            "Files mode must not select a directory as a task"
        );
        picker.paste("Feature");
        assert!(
            matches!(key(&mut picker, Key::Enter).unwrap(), PickerAction::Select(path) if path == spec)
        );
        assert!(matches!(
            key(&mut picker, Key::Right).unwrap(),
            PickerAction::Stay
        ));
        assert!(
            matches!(open_path(&mut picker, &spec.display().to_string()).unwrap(), PickerAction::Select(path) if path == spec)
        );
    }

    fn key(picker: &mut PathPicker, code: Key) -> Result<PickerAction> {
        picker.key(KeyEvent::new(code, Mods::NONE))
    }

    fn open_path(picker: &mut PathPicker, path: &str) -> Result<PickerAction> {
        picker.key(KeyEvent::new(Key::Char('l'), Mods::CONTROL))?;
        picker.paste(path);
        key(picker, Key::Enter)
    }

    #[test]
    fn slash_or_tilde_opens_path_entry_when_filter_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        let mut picker = PathPicker::new(&root, vec![]).unwrap();
        key(&mut picker, Key::Char('/')).unwrap();
        picker.paste(&target.display().to_string()[1..]);
        key(&mut picker, Key::Enter).unwrap();
        assert_eq!(picker.directory, target);
        assert!(picker.path_input.is_none());

        key(&mut picker, Key::Char('~')).unwrap();
        assert_eq!(content(picker.path_input.as_ref().unwrap()), "~");
        key(&mut picker, Key::Esc).unwrap();
        picker.paste("ab");
        key(&mut picker, Key::Char('/')).unwrap();
        assert!(picker.path_input.is_none());
    }

    #[test]
    fn browses_folders_and_selects_repositories_and_linked_worktrees() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = root.join("current");
        let group = root.join("projects");
        let repo = group.join("Repo with spaces 雪");
        let linked = group.join("linked-worktree");
        fs::create_dir(&project).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir(&linked).unwrap();
        fs::write(linked.join(".git"), "gitdir: /elsewhere/worktrees/linked\n").unwrap();
        let mut picker = PathPicker::new(&project, vec![]).unwrap();
        picker.paste("projects");
        assert!(matches!(
            key(&mut picker, Key::Enter).unwrap(),
            PickerAction::Stay
        ));
        assert_eq!(picker.directory, fs::canonicalize(&group).unwrap());
        picker.paste("rEpO WITH SPACES 雪");
        assert!(
            matches!(key(&mut picker, Key::Enter).unwrap(), PickerAction::Select(path) if path == repo)
        );
        key(&mut picker, Key::Right).unwrap();
        assert_eq!(picker.directory, repo);
        assert!(
            picker.entries.is_empty(),
            "Do not offer Git's metadata as a project"
        );
        key(&mut picker, Key::Left).unwrap();
        assert!(
            matches!(key(&mut picker, Key::Enter).unwrap(), PickerAction::Select(path) if path == repo)
        );
        picker.filter = textarea("linked");
        picker.selected = 0;
        assert!(
            matches!(key(&mut picker, Key::Enter).unwrap(), PickerAction::Select(path) if path == linked)
        );
    }

    #[test]
    fn invalid_paths_and_empty_search_keep_the_browser_open() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = root.join("current");
        fs::create_dir(&project).unwrap();
        let mut picker = PathPicker::new(&project, vec![]).unwrap();
        picker.paste("no such repo");
        assert!(matches!(
            key(&mut picker, Key::Enter).unwrap(),
            PickerAction::Stay
        ));
        let directory = picker.directory.clone();
        assert!(open_path(&mut picker, "./does-not-exist").is_err());
        assert_eq!(picker.directory, directory);
        assert!(picker.path_input.is_some());
        assert!(matches!(
            key(&mut picker, Key::Esc).unwrap(),
            PickerAction::Stay
        ));
        assert!(picker.path_input.is_none());
        assert!(matches!(
            key(&mut picker, Key::Esc).unwrap(),
            PickerAction::Close
        ));
    }

    #[test]
    fn direct_paths_and_non_git_directories_can_be_selected() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        let other = root.join("empty folder 雪");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&other).unwrap();
        let mut picker = PathPicker::new(&project, vec![]).unwrap();
        open_path(&mut picker, "./empty folder 雪").unwrap();
        assert!(
            matches!(key(&mut picker, Key::Enter).unwrap(), PickerAction::Select(path) if path == other)
        );
        open_path(&mut picker, &project.display().to_string()).unwrap();
        assert!(
            matches!(key(&mut picker, Key::Enter).unwrap(), PickerAction::Select(path) if path == project)
        );
        let home = directories::BaseDirs::new()
            .unwrap()
            .home_dir()
            .to_path_buf();
        assert_eq!(picker.resolve_path("~").unwrap(), home);
        assert_eq!(
            picker.resolve_path("~/projects").unwrap(),
            home.join("projects")
        );
        assert!(picker.resolve_path(" ").is_err());
    }

    #[test]
    fn recent_projects_are_searchable_and_can_be_browsed() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = root.join("first");
        let second = root.join("second");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&second).unwrap();
        let mut picker = PathPicker::new(
            &project,
            vec![project.clone(), root.join("deleted"), second.clone()],
        )
        .unwrap();
        key(&mut picker, Key::Tab).unwrap();
        assert_eq!(picker.recent.len(), 2);
        picker.paste("second");
        assert!(
            matches!(key(&mut picker, Key::Enter).unwrap(), PickerAction::Select(path) if path == second)
        );
        key(&mut picker, Key::Right).unwrap();
        assert_eq!(picker.directory, second);
        assert!(!picker.show_recent);
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_browsable_without_recursive_scanning() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = root.join("repo");
        fs::create_dir_all(project.join(".git")).unwrap();
        symlink(&project, root.join("repo-alias")).unwrap();
        symlink(&root, root.join("loop")).unwrap();
        symlink(root.join("missing"), root.join("broken")).unwrap();
        let mut picker = PathPicker::new(&project, vec![]).unwrap();
        picker.paste("repo-alias");
        assert!(
            matches!(key(&mut picker, Key::Enter).unwrap(), PickerAction::Select(path) if path == root.join("repo-alias"))
        );
        key(&mut picker, Key::Right).unwrap();
        assert_eq!(picker.directory, fs::canonicalize(&project).unwrap());
        key(&mut picker, Key::Left).unwrap();
        picker.filter = textarea("loop");
        picker.selected = 0;
        key(&mut picker, Key::Enter).unwrap();
        assert_eq!(picker.directory, fs::canonicalize(&root).unwrap());
        picker.paste("broken");
        assert!(picker.matches().is_empty());
    }
}
