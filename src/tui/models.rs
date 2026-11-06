//! Harness model catalogs: the model picker ([m], an editor's model field)
//! and the effort levels editors offer.
use super::{
    app::App,
    modal::{Choice, Loading, Modal, PickValue},
    tasks::Msg,
    widgets::textarea,
};
use crate::catalog::{self, Catalog};
use std::collections::{BTreeMap, VecDeque};

/// Catalogs of the open project.
#[derive(Default)]
pub(super) struct Catalogs {
    /// Read so far, by harness; they supply effort levels to editors.
    pub(super) cached: BTreeMap<String, Catalog>,
    /// The harness whose catalog is being read for editors.
    pub(super) reading: Option<String>,
    /// Harnesses to read for editors next.
    pub(super) queue: VecDeque<String>,
}

impl App {
    /// Shows a catalog read for the model picker: the picker on success; on
    /// failure, the reason, in the editor the picker was opened from.
    pub(super) fn show_catalog_result(&mut self, harness: String, result: Result<Catalog, String>) {
        match result {
            Ok(catalog) => {
                self.catalogs
                    .cached
                    .insert(harness.clone(), catalog.clone());
                self.push_catalog(&harness);
                self.show_catalog(&harness, catalog);
            }
            Err(error) => match self.ui.dialog_mut() {
                Some(Modal::Stage { .. }) => self
                    .ui
                    .error(format!("{error}. Type a model ID with [i] instead.")),
                Some(Modal::Harness(editor)) => {
                    let message =
                        format!("{error}. Enter a model ID manually in harness settings [c].");
                    editor.error = message.clone();
                    self.ui.error(message);
                }
                _ => self.ui.error(format!(
                    "{error}. Enter a model ID manually in harness settings [c]."
                )),
            },
        }
    }
    fn show_catalog(&mut self, harness: &str, catalog: Catalog) {
        let items = catalog
            .models
            .into_iter()
            .map(|model| Choice {
                label: format!(
                    "{}{}{}  ·  {}{}",
                    if model.provider.is_empty() {
                        ""
                    } else {
                        &model.provider
                    },
                    if model.provider.is_empty() { "" } else { "/" },
                    model.label,
                    model.availability,
                    if model.efforts.is_empty() {
                        String::new()
                    } else {
                        format!("  ·  {}", model.efforts.join("/"))
                    }
                ),
                value: PickValue::Model(model),
            })
            .collect();
        self.ui.open(Modal::Picker {
            title: format!("{harness} selections · type to filter"),
            items,
            selected: 0,
            filter: textarea(""),
            note: catalog.note,
        });
    }
    /// Hands a cached catalog to the open editors for `harness`, including
    /// any under a picker.
    fn push_catalog(&mut self, harness: &str) {
        let Some(catalog) = self.catalogs.cached.get(harness) else {
            return;
        };
        for dialog in &mut self.ui.dialogs {
            match dialog {
                Modal::Harness(editor) if editor.harness() == harness => {
                    editor.set_catalog(catalog.clone());
                }
                Modal::Stage { editor, .. } if editor.harness() == harness => {
                    editor.set_catalog(catalog.clone());
                }
                _ => {}
            }
        }
    }
    /// Makes `harness`'s catalog available to editors: from the cache now, or
    /// read in the background, one harness at a time, without a picker.
    pub(super) fn request_catalog(&mut self, harness: String) {
        let catalogs = &mut self.catalogs;
        if catalogs.cached.contains_key(&harness) {
            self.push_catalog(&harness);
            return;
        }
        if catalogs.queue.contains(&harness) || catalogs.reading.as_ref() == Some(&harness) {
            return;
        }
        if catalogs.reading.is_some() {
            catalogs.queue.push_back(harness);
            return;
        }
        catalogs.reading = Some(harness.clone());
        self.read_catalog(harness, None);
    }
    /// A catalog that arrived for editors.
    pub(super) fn catalog_read(&mut self, harness: String, result: Result<Catalog, String>) {
        self.catalogs.reading = None;
        if let Ok(catalog) = result {
            self.catalogs.cached.insert(harness.clone(), catalog);
            self.push_catalog(&harness);
        }
        if let Some(next) = self.catalogs.queue.pop_front() {
            self.request_catalog(next);
        }
    }
    /// Opens the model picker for `harness` above the open dialogs.
    pub(super) fn model_picker_for(&mut self, harness: String) {
        if let Some(catalog) = self.catalogs.cached.get(&harness).cloned() {
            self.show_catalog(&harness, catalog);
            return;
        }
        let loading = Loading::new(format!("Reading {harness} model selections…"));
        self.read_catalog(harness, Some(loading.ticket));
        self.ui.open(Modal::Loading(loading));
    }
    /// Reads `harness`'s catalog for the picker waiting with `ticket`, or for editors.
    fn read_catalog(&mut self, harness: String, ticket: Option<u64>) {
        let (settings, project, name) = (
            self.editor.settings.clone(),
            self.project.clone(),
            harness.clone(),
        );
        self.tasks.spawn(
            "Reading the model catalog stopped unexpectedly",
            move || catalog::discover(&name, &settings, &project).map_err(|e| format!("{e:#}")),
            move |result| Msg::Catalog {
                harness,
                ticket,
                result,
            },
        );
    }
}
