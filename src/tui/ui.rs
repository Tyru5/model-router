//! What every page shares: appearance preferences, the status line, the
//! open dialog, and whether the next frame needs drawing.
use super::{
    app::Page,
    external::Launch,
    modal::{Modal, PickValue, matching_choices},
    theme::{self, Palette, Theme},
};
use crate::{config, notify};
use std::path::PathBuf;

pub(super) struct Ui {
    pub(super) theme: Theme,
    /// Where the theme, Vim motions, and notification backends are saved.
    pub(super) theme_path: PathBuf,
    pub(super) vim_motions: bool,
    /// Notification backends, saved with the theme.
    pub(super) notifications: notify::Backends,
    pub(super) page: Page,
    pub(super) modal: Option<Modal>,
    /// The status line.
    pub(super) message: String,
    /// Last error shown; the status line turns red while it is current.
    pub(super) alert: String,
    /// A pager, editor, or difftool to give the terminal on the next frame.
    pub(super) external: Option<Launch>,
    /// Something changed that the next frame should show.
    pub(super) redraw: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            theme_path: config::config_dir().join("appearance.json"),
            vim_motions: false,
            notifications: notify::Backends::default(),
            page: Page::Workflow,
            modal: None,
            message: "Define a task, choose your stages, then run. Review is optional.".into(),
            alert: String::new(),
            external: None,
            redraw: true,
        }
    }
}

impl Ui {
    /// The theme to draw with: the theme picker previews its selection.
    pub(super) fn displayed_theme(&self) -> Theme {
        if let Some(Modal::Picker {
            items,
            selected,
            filter,
            ..
        }) = &self.modal
        {
            let choices = matching_choices(items, filter);
            if let Some(choice) = choices.get((*selected).min(choices.len().saturating_sub(1))) {
                if let PickValue::Theme(theme) = choice.value {
                    return theme;
                }
            }
        }
        self.theme
    }
    pub(super) fn palette(&self) -> Palette {
        self.displayed_theme().palette()
    }
    pub(super) fn preferences(&self) -> theme::Preferences {
        theme::Preferences {
            theme: self.theme,
            vim_motions: self.vim_motions,
            notifications: self.notifications.clone(),
        }
    }
    /// Shows `message` on the status line, in red.
    pub(super) fn error(&mut self, message: String) {
        self.alert = message.clone();
        self.message = message;
    }
}
