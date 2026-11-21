use super::input::InputField;
use super::path_picker::{PathPicker, PickerAction};
use super::tasks::{Msg, Spawner, ticket};
use super::theme::Palette;
use super::widgets::{content, focus_textarea, textarea};
use crate::{config, storage, task_import};
use anyhow::{Result, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Tabs, Wrap},
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) enum ImportAction {
    Stay,
    Apply(String),
    Close,
}

struct Draft {
    source: String,
    fields: [InputField<'static>; 2],
    mode: usize,
}
impl Draft {
    fn new(source: String, text: &str, existing: &str) -> Self {
        let append = if existing.is_empty() {
            text.into()
        } else {
            format!("{existing}\n\n---\n\n{text}")
        };
        Self {
            source,
            fields: [textarea(text), textarea(&append)],
            mode: usize::from(!existing.is_empty()),
        }
    }
}

struct ImportJob {
    source: String,
    /// Identifies the download's result.
    ticket: u64,
    cancel: Arc<AtomicBool>,
}
impl Drop for ImportJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub(super) struct TaskImporter {
    picker: PathPicker,
    existing: String,
    draft: Option<Draft>,
    url: Option<InputField<'static>>,
    job: Option<ImportJob>,
    error: String,
    spawner: Spawner,
}

impl TaskImporter {
    pub(super) fn active_input(&mut self) -> Option<&mut InputField<'static>> {
        if self.job.is_some() {
            None
        } else if let Some(draft) = &mut self.draft {
            Some(&mut draft.fields[draft.mode])
        } else if let Some(url) = &mut self.url {
            Some(url)
        } else {
            Some(self.picker.active_input())
        }
    }
    pub fn new(project: &Path, existing: &str, spawner: Spawner) -> Result<Self> {
        Ok(Self {
            picker: PathPicker::files(project, spawner.clone())?,
            existing: existing.into(),
            draft: None,
            url: None,
            job: None,
            error: String::new(),
            spawner,
        })
    }

    /// Whether a URL import is running.
    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }
    /// Whether its picker's folder read reports with `ticket`.
    pub(super) fn waits_for(&self, ticket: u64) -> bool {
        self.picker.waits_for(ticket)
    }
    /// Whether the download reports with `ticket`.
    pub(super) fn import_waits_for(&self, ticket: u64) -> bool {
        self.job.as_ref().is_some_and(|job| job.ticket == ticket)
    }
    /// Shows a folder its picker read; false when it is not the picker's.
    pub(super) fn folder_read(
        &mut self,
        ticket: u64,
        result: std::result::Result<Vec<super::path_picker::FolderEntry>, String>,
    ) -> bool {
        self.picker.folder_read(ticket, result)
    }
    /// Shows a downloaded task; false when `ticket` is not the download
    /// waited for, e.g. after Esc.
    pub(super) fn imported(
        &mut self,
        ticket: u64,
        result: std::result::Result<String, String>,
    ) -> bool {
        if !self.import_waits_for(ticket) {
            return false;
        }
        let job = self.job.take().expect("Import result has a job");
        match result {
            Ok(text) => self.draft = Some(Draft::new(job.source.clone(), &text, &self.existing)),
            Err(error) => self.error = error,
        }
        true
    }

    pub fn paste(&mut self, text: &str) {
        if self.job.is_some() {
            return;
        }
        if let Some(draft) = &mut self.draft {
            draft.fields[draft.mode].insert_str(text);
        } else if let Some(url) = &mut self.url {
            url.insert_str(text.replace(['\r', '\n'], ""));
        } else {
            self.picker.paste(text);
        }
        self.error.clear();
    }

    pub fn key(&mut self, key: KeyEvent) -> ImportAction {
        self.error.clear();
        match self.handle_key(key) {
            Ok(action) => action,
            Err(error) => {
                if self.draft.is_none() && self.url.is_none() {
                    self.picker.error = format!("{error:#}");
                } else {
                    self.error = format!("{error:#}");
                }
                ImportAction::Stay
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<ImportAction> {
        if self.job.is_some() {
            if key.code == Key::Esc {
                self.job = None;
                self.error =
                    "Import cancelled. Edit the URL or press [Esc] to return to files.".into();
            }
            return Ok(ImportAction::Stay);
        }
        if let Some(draft) = &mut self.draft {
            match key.code {
                Key::Esc => self.draft = None,
                Key::Tab | Key::BackTab => {
                    draft.mode = 1 - draft.mode;
                    focus_textarea(&mut draft.fields[draft.mode]);
                }
                Key::Char('s') if key.modifiers.contains(Mods::CONTROL) => {
                    let text = content(&draft.fields[draft.mode]);
                    config::validate_task(&text)?;
                    return Ok(ImportAction::Apply(text));
                }
                _ => {
                    draft.fields[draft.mode].input(key);
                }
            }
            return Ok(ImportAction::Stay);
        }
        if let Some(url) = &mut self.url {
            match key.code {
                Key::Esc => self.url = None,
                Key::Enter => {
                    let source = content(url).trim().to_owned();
                    ensure!(
                        !source.is_empty(),
                        "Enter a Markdown/text URL or GitHub issue/file URL"
                    );
                    let cancel = Arc::new(AtomicBool::new(false));
                    let worker_cancel = cancel.clone();
                    let (input, ticket) = (source.clone(), ticket());
                    self.spawner.spawn(
                        "Import stopped unexpectedly; retry the URL.",
                        move || {
                            task_import::read_url(&input, &worker_cancel)
                                .map_err(|e| format!("{e:#}"))
                        },
                        move |result| Msg::Imported { ticket, result },
                    );
                    self.job = Some(ImportJob {
                        source,
                        ticket,
                        cancel,
                    });
                }
                _ => {
                    url.input(key);
                }
            }
            return Ok(ImportAction::Stay);
        }
        if key.code == Key::Char('u') && key.modifiers.contains(Mods::CONTROL) {
            self.url = Some(textarea(""));
            return Ok(ImportAction::Stay);
        }
        match self.picker.key(key)? {
            PickerAction::Close => return Ok(ImportAction::Close),
            PickerAction::Stay => {}
            PickerAction::Select(path) => {
                let text = task_import::read(&path)?;
                self.draft = Some(Draft::new(
                    path.display().to_string(),
                    &text,
                    &self.existing,
                ));
            }
        }
        Ok(ImportAction::Stay)
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        if let Some(job) = &self.job {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![
                        Span::styled(
                            format!("{} ", super::widgets::spinner()),
                            Style::default().fg(palette.accent),
                        ),
                        Span::raw("Fetching task/spec…"),
                    ]),
                    Line::raw(""),
                    Line::raw(storage::clean_terminal(&job.source)),
                    Line::raw(""),
                    Line::raw(
                        "Esc cancels. The current task is unchanged until you apply a preview.",
                    ),
                ])
                .block(palette.modal("Import task/spec", &[("Esc", "cancel")]))
                .style(Style::default().fg(palette.fg))
                .wrap(Wrap { trim: false }),
                rect,
            );
            return;
        }
        if let Some(draft) = &mut self.draft {
            let block = palette.modal("Preview task/spec", &[("Tab", "append/replace")]);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let chunks = Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(inner);
            frame.render_widget(
                Paragraph::new(storage::clean_terminal(&draft.source))
                    .style(Style::default().fg(palette.muted))
                    .wrap(Wrap { trim: false }),
                chunks[0],
            );
            frame.render_widget(
                Tabs::new(["Replace task", "Append to task"])
                    .select(draft.mode)
                    .style(Style::default().fg(palette.muted))
                    .highlight_style(palette.selected_style()),
                chunks[1],
            );
            let field = &mut draft.fields[draft.mode];
            palette.style_textarea(field);
            let bytes = field.lines().iter().map(String::len).sum::<usize>()
                + field.lines().len().saturating_sub(1);
            field.set_block(palette.panel(format!(
                " Editable task · {bytes} / {} bytes ",
                config::MAX_TASK_BYTES
            )));
            frame.render_widget(&*field, chunks[2]);
            let note = if self.error.is_empty() {
                if bytes > config::MAX_TASK_BYTES {
                    "The draft exceeds 32 KiB. Trim it or switch to Replace before applying."
                } else {
                    "Edit this draft freely. The original file or issue is never modified."
                }
            } else {
                &self.error
            };
            frame.render_widget(
                Paragraph::new(storage::clean_terminal(note))
                    .style(Style::default().fg(if self.error.is_empty() {
                        palette.muted
                    } else {
                        palette.red
                    }))
                    .wrap(Wrap { trim: false }),
                chunks[3],
            );
            frame.render_widget(
                Paragraph::new(vec![
                    palette.hints(&[
                        ("Ctrl+S", "use task"),
                        ("Tab", "append/replace"),
                        ("Esc", "back"),
                    ]),
                    Line::styled(
                        "Applying an import does not start a workflow.",
                        Style::default().fg(palette.muted),
                    ),
                ]),
                chunks[4],
            );
        } else if let Some(url) = &mut self.url {
            palette.style_textarea(url);
            let block = palette.modal("Import from URL", &[]);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let chunks = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(5),
                Constraint::Min(2),
                Constraint::Length(2),
            ])
            .split(inner);
            url.set_block(palette.panel(" Markdown/text URL or GitHub issue/file URL "));
            frame.render_widget(&*url, chunks[0]);
            frame.render_widget(Paragraph::new("https://example.com/spec.md\nhttps://github.com/owner/repo/issues/123\nhttps://github.com/owner/repo/blob/main/spec.md\n\nGitHub uses your gh login. Text limit: 32 KiB.").style(Style::default().fg(palette.muted)).wrap(Wrap { trim: false }), chunks[1]);
            frame.render_widget(
                Paragraph::new(storage::clean_terminal(&self.error))
                    .style(Style::default().fg(palette.red))
                    .wrap(Wrap { trim: false }),
                chunks[2],
            );
            frame.render_widget(
                Paragraph::new(
                    palette.hints(&[("Enter", "fetch and preview"), ("Esc", "back to files")]),
                ),
                chunks[3],
            );
        } else {
            self.picker.draw(frame, rect, palette);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(importer: &mut TaskImporter, key: Key) -> ImportAction {
        importer.key(KeyEvent::new(key, Mods::NONE))
    }

    #[test]
    fn preview_is_transactional_and_preserves_edits_in_each_mode() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("spec.md");
        let text = "# Spec 雪\n\n- Do this\n";
        std::fs::write(&source, text).unwrap();
        let (spawner, inbox) = Spawner::detached();
        let mut importer =
            TaskImporter::new(dir.path(), "Keep existing instructions", spawner).unwrap();
        importer.picker.wait(&inbox);
        importer.paste("spec.md");
        assert!(matches!(
            press(&mut importer, Key::Enter),
            ImportAction::Stay
        ));
        assert_eq!(importer.existing, "Keep existing instructions");
        assert_eq!(importer.draft.as_ref().unwrap().mode, 1);
        assert_eq!(
            content(&importer.draft.as_ref().unwrap().fields[1]),
            format!("Keep existing instructions\n\n---\n\n{text}")
        );
        press(&mut importer, Key::Tab);
        importer.paste("Additional instructions");
        press(&mut importer, Key::Tab);
        press(&mut importer, Key::Tab);
        let action = importer.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL));
        assert!(
            matches!(action, ImportAction::Apply(value) if value == format!("{text}Additional instructions"))
        );
        assert_eq!(std::fs::read_to_string(source).unwrap(), text);
    }

    #[test]
    fn cancel_and_oversized_append_leave_the_existing_task_intact() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("spec.md"), "# Additional work").unwrap();
        let existing = "x".repeat(config::MAX_TASK_BYTES);
        let (spawner, inbox) = Spawner::detached();
        let mut importer = TaskImporter::new(dir.path(), &existing, spawner).unwrap();
        importer.picker.wait(&inbox);
        importer.paste("spec.md");
        press(&mut importer, Key::Enter);
        assert!(matches!(
            importer.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL)),
            ImportAction::Stay
        ));
        assert!(importer.error.contains("32768"));
        press(&mut importer, Key::Esc);
        assert!(importer.draft.is_none());
        assert_eq!(importer.existing, existing);
        assert!(matches!(
            press(&mut importer, Key::Esc),
            ImportAction::Close
        ));
    }
}
