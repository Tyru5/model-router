use super::{
    actions::ActionId,
    app::{App, Job, Page, draw},
    modal::{Ask, Modal, PickValue},
    path_picker::PathPicker,
    run_slot::Worker,
    theme::{self, Theme},
    widgets::textarea,
    workflow_page::StageEditor,
};
use crate::{
    catalog::{Catalog, Model},
    config::{self, Role, Settings, Stage},
    engine::{Action, Event as RunEvent, Status},
    presets,
};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::Terminal;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

fn key(code: Key) -> KeyEvent {
    KeyEvent::new(code, Mods::NONE)
}

fn theme_app(project: &std::path::Path) -> App {
    let mut app = App::new(
        project.into(),
        Settings::default(),
        project.join("config.toml"),
        Arc::new(AtomicBool::new(false)),
    );
    app.ui.theme_path = project.join("appearance.json");
    app.harnesses.checks.probe = |_, _| vec![];
    app
}

#[test]
fn vim_opt_in_persists_without_changing_the_theme_or_project() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    assert!(!app.ui.vim_motions);
    assert!(!theme::load(&app.ui.theme_path).unwrap().vim_motions);
    std::fs::write(&app.ui.theme_path, r#"{"theme":"dracula"}"#).unwrap();
    let legacy = theme::load(&app.ui.theme_path).unwrap();
    assert!(!legacy.vim_motions);
    app.ui.theme = legacy.theme;
    app.key(key(Key::F(8))).unwrap();
    let saved = theme::load(&app.ui.theme_path).unwrap();
    assert!(app.ui.vim_motions && saved.vim_motions);
    assert_eq!(saved.theme, app.ui.theme);
    assert!(!app.editor.dirty);
    app.pick(PickValue::Theme(Theme::CatppuccinLatte)).unwrap();
    assert!(theme::load(&app.ui.theme_path).unwrap().vim_motions);
    app.key(key(Key::Char(':'))).unwrap();
    app.paste("Vim motions");
    app.key(key(Key::Enter)).unwrap();
    assert!(!app.ui.vim_motions);
    assert!(!theme::load(&app.ui.theme_path).unwrap().vim_motions);
    assert_eq!(app.ui.theme, Theme::CatppuccinLatte);
    let obstruction = project.path().join("file");
    std::fs::write(&obstruction, "unchanged").unwrap();
    app.ui.theme_path = obstruction.join("appearance.json");
    assert!(app.key(key(Key::F(8))).is_err());
    assert!(!app.ui.vim_motions);
}

#[test]
fn vim_toggle_sits_below_header_options_on_the_right() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    for width in [65, 80, 120] {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
        for enabled in [false, true] {
            app.ui.vim_motions = enabled;
            for page in Page::ALL {
                app.ui.page = page;
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                let buffer = terminal.backend().buffer();
                let first_row = (0..width)
                    .map(|x| buffer[(x, 0)].symbol())
                    .collect::<String>();
                assert!(!first_row.contains("[F8]"), "{first_row}");
                let label = if enabled {
                    "[F8] VIM ON"
                } else {
                    "[F8] Vim OFF"
                };
                let toggle_row = (1..4)
                    .find_map(|y| {
                        let row = (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>();
                        row.contains(label).then_some(row)
                    })
                    .expect("Vim toggle below the header");
                assert_eq!(
                    toggle_row,
                    format!("{:>width$} ", label, width = usize::from(width) - 1)
                );
                let header = buffer.content[..usize::from(width) * 3]
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(header.contains("4 Harnesses"), "{header}");
            }
        }
    }
    app.ui.page = Page::Workflow;
    app.editor.task = "Keep this draft".into();
    app.key(key(Key::Char('t'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::F(8))).unwrap();
    assert!(!app.ui.vim_motions && app.ui.modal.is_some());
    app.key(key(Key::F(8))).unwrap();
    assert!(app.ui.vim_motions && app.ui.modal.is_some());
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(65, 20)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let display = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(display.contains("VIM INSERT"));
    let field = app.ui.modal.as_mut().unwrap().active_input().unwrap();
    assert_eq!(super::widgets::content(field), "Keep this draft");
    assert!(field.mode_hint().unwrap().contains("INSERT"));
}

#[test]
fn vim_task_editing_routes_escape_motions_paste_and_save() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.editor.task = "first\nsecond".into();
    app.key(key(Key::Char('t'))).unwrap();
    app.key(key(Key::Char('h'))).unwrap();
    assert_eq!(
        super::widgets::content(app.ui.modal.as_mut().unwrap().active_input().unwrap()),
        "first\nsecondh"
    );
    app.key(key(Key::Esc)).unwrap();
    assert!(
        app.ui.modal.is_none(),
        "default Esc still cancels immediately"
    );
    app.key(key(Key::F(8))).unwrap();
    app.key(key(Key::Char('t'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.modal.is_some());
    for c in "gg0i".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.paste("Prefix ");
    app.key(key(Key::Esc)).unwrap();
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let display = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(display.contains("VIM NORMAL"));
    app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
        .unwrap();
    assert!(app.ui.modal.is_none());
    assert_eq!(app.editor.task, "Prefix first\nsecond");
    app.key(key(Key::Char('t'))).unwrap();
    assert_eq!(
        app.ui
            .modal
            .as_mut()
            .unwrap()
            .active_input()
            .unwrap()
            .cursor(),
        (1, 6)
    );
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.modal.is_none());
}

#[test]
fn vim_stage_navigation_keeps_choices_and_refocus_behavior() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.key(key(Key::F(8))).unwrap();
    app.key(key(Key::Char('e'))).unwrap();
    let original = super::widgets::content(app.ui.modal.as_mut().unwrap().active_input().unwrap());
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('0'))).unwrap();
    app.key(key(Key::Char('g'))).unwrap();
    app.key(key(Key::Down)).unwrap(); // Role remains a choice, not a Vim input.
    assert!(app.ui.modal.as_mut().unwrap().active_input().is_none());
    app.key(key(Key::Up)).unwrap();
    app.key(key(Key::Char('!'))).unwrap();
    let field = app.ui.modal.as_mut().unwrap().active_input().unwrap();
    assert_eq!(super::widgets::content(field), format!("{original}!"));
    assert!(field.mode_hint().unwrap().contains("INSERT"));
    // Up goes to instructions; j/k move within the multiline input instead.
    app.key(key(Key::Up)).unwrap();
    app.paste("\nlast line");
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('k'))).unwrap();
    assert!(app.ui.modal.as_mut().unwrap().active_input().is_some());
    // Down returns to the last line first; only there does it leave the field.
    app.key(key(Key::Down)).unwrap();
    assert_eq!(
        app.ui
            .modal
            .as_mut()
            .unwrap()
            .active_input()
            .unwrap()
            .lines()
            .last()
            .unwrap(),
        "last line"
    );
    app.key(key(Key::Down)).unwrap();
    assert_eq!(
        super::widgets::content(app.ui.modal.as_mut().unwrap().active_input().unwrap()),
        format!("{original}!")
    );
}

#[test]
fn vim_motions_reach_harness_path_import_and_filter_inputs() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.ui.vim_motions = true;
    for modal in [
        Modal::Harness(Box::new(super::harness_editor::HarnessEditor::new(
            Stage::default(),
            Default::default(),
        ))),
        Modal::Repositories(Box::new(PathPicker::new(project.path(), vec![]).unwrap())),
        Modal::Import(Box::new(
            super::task_importer::TaskImporter::new(project.path(), "").unwrap(),
        )),
        Modal::Picker {
            title: "Picker".into(),
            items: vec![],
            selected: 0,
            filter: textarea(""),
            note: String::new(),
        },
    ] {
        app.ui.modal = Some(modal);
        app.paste("abc");
        app.key(key(Key::Esc)).unwrap();
        app.key(key(Key::Char('0'))).unwrap();
        let field = app.ui.modal.as_mut().unwrap().active_input().unwrap();
        assert_eq!(field.cursor(), (0, 0));
        assert_eq!(super::widgets::content(field), "abc");
        app.key(key(Key::Char('A'))).unwrap();
        app.key(key(Key::Char('!'))).unwrap();
        assert_eq!(
            super::widgets::content(app.ui.modal.as_mut().unwrap().active_input().unwrap()),
            "abc!"
        );
    }
    app.ui.modal = None;
    app.ui.page = Page::Runs;
    app.key(key(Key::Char('/'))).unwrap();
    app.paste("abc");
    app.key(key(Key::Esc)).unwrap();
    assert!(app.runs_filtering());
    app.key(key(Key::Char('0'))).unwrap();
    app.key(key(Key::Char('i'))).unwrap();
    app.key(key(Key::Char('x'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(!app.runs_filtering());
}

#[test]
fn theme_preview_is_cancelable_and_only_enter_persists() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.editor.task = "Keep my task".into();
    let settings = toml::to_string(&app.editor.settings).unwrap();
    app.key(key(Key::Char('T'))).unwrap();
    app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
        .unwrap();
    assert_eq!(app.ui.displayed_theme(), Theme::Terminal);
    assert_eq!(app.ui.theme, Theme::ModelRouter);
    assert!(!app.ui.theme_path.exists());
    app.key(KeyEvent::new(Key::Char('p'), Mods::CONTROL))
        .unwrap();
    assert_eq!(app.ui.displayed_theme(), Theme::ModelRouter);
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(app.ui.displayed_theme(), Theme::ModelRouter);
    assert!(!app.ui.theme_path.exists());

    app.key(key(Key::Char('T'))).unwrap();
    for c in "latte".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    assert_eq!(app.ui.displayed_theme(), Theme::CatppuccinLatte);
    app.key(key(Key::Enter)).unwrap();
    assert!(app.ui.modal.is_none());
    assert_eq!(app.ui.theme, Theme::CatppuccinLatte);
    assert_eq!(
        theme::load(&app.ui.theme_path).unwrap().theme,
        Theme::CatppuccinLatte
    );

    app.key(key(Key::Char('T'))).unwrap();
    for c in "no-such-theme".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
        .unwrap();
    app.key(key(Key::Enter)).unwrap();
    assert!(
        app.ui.modal.is_some(),
        "An empty search must not choose a theme"
    );
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(app.ui.displayed_theme(), Theme::CatppuccinLatte);
    assert_eq!(
        theme::load(&app.ui.theme_path).unwrap().theme,
        Theme::CatppuccinLatte
    );
    assert!(!app.editor.dirty);
    assert_eq!(app.editor.task, "Keep my task");
    assert_eq!(toml::to_string(&app.editor.settings).unwrap(), settings);
}

#[test]
fn failed_theme_save_preserves_committed_theme_and_allows_cancel() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let obstruction = project.path().join("not-a-directory");
    std::fs::write(&obstruction, "keep this file").unwrap();
    app.ui.theme_path = obstruction.join("appearance.json");
    app.choose_theme();
    for c in "dracula".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    assert!(app.key(key(Key::Enter)).is_err());
    assert!(
        matches!(&app.ui.modal, Some(Modal::Picker { note, .. }) if note.contains("Could not apply selection"))
    );
    assert_eq!(app.ui.theme, Theme::ModelRouter);
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(app.ui.displayed_theme(), Theme::ModelRouter);
    assert_eq!(
        std::fs::read_to_string(obstruction).unwrap(),
        "keep this file"
    );
    assert!(!app.editor.dirty);
}

#[test]
fn every_theme_reaches_pages_dialogs_and_text_inputs() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.editor.task = "Preserve editor content".into();
    for theme in Theme::ALL {
        app.ui.theme = theme;
        let palette = theme.palette();
        for (width, height) in [(120, 36), (80, 24)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            // `2 o o` revisits the Monitor with a finished run: results, the
            // output log, then results again.
            for (step, action) in [
                '1', '2', '3', '4', '1', 't', 'e', 'c', 'p', 'f', 'w', '?', 'T', ':', '2', 'o', 'o',
            ]
            .into_iter()
            .enumerate()
            {
                app.ui.modal = None;
                if step == 14 {
                    app.session.adopt(crate::report::tests::sample());
                }
                app.key(key(Key::Char(action))).unwrap();
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer[(0, 0)].bg, palette.bg, "{theme:?} {action}");
                assert!(
                    buffer.content.iter().all(|cell| {
                        [
                            palette.bg,
                            palette.panel,
                            palette.selection_bg,
                            palette.accent,
                        ]
                        .contains(&cell.bg)
                    }),
                    "Unstyled background in {theme:?}, view {action}"
                );
            }
            assert!(app.results_shown());
            app.session.runs.clear();
            app.ui.modal = None;
        }
    }
    assert_eq!(app.editor.task, "Preserve editor content");
    assert!(!app.ui.theme_path.exists());
    assert!(!app.editor.dirty);
}

#[test]
fn stage_presets_are_inserted_only_after_save_and_have_unique_ids() {
    let project = tempfile::tempdir().unwrap();
    let mut app = App::new(
        project.path().into(),
        Settings::default(),
        project.path().join("config.toml"),
        Arc::new(AtomicBool::new(false)),
    );
    let original = toml::to_string(&app.editor.settings).unwrap();
    let preset = presets::STAGES
        .iter()
        .position(|p| p.key == "plan")
        .unwrap();
    app.pick(PickValue::StagePreset(preset)).unwrap();
    assert_eq!(toml::to_string(&app.editor.settings).unwrap(), original);
    app.modal_key(key(Key::Esc)).unwrap();
    assert!(!app.editor.dirty);
    assert_eq!(toml::to_string(&app.editor.settings).unwrap(), original);
    for _ in 0..2 {
        app.pick(PickValue::StagePreset(preset)).unwrap();
        app.modal_key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
            .unwrap();
        assert_eq!(app.editor.stage().role, Role::Planning);
    }
    let stages = &app.editor.settings.workflows[0].stages;
    assert_eq!(stages.len(), 6);
    assert_eq!(stages[0].id, "research");
    assert_eq!(stages[3].id, "implement");
    assert_eq!(stages[5].id, "summary");
    assert_ne!(stages[1].id, stages[2].id);
    app.editor.settings.validate().unwrap();
    // Adding or duplicating at the limit must not leave an invalid workflow.
    app.editor.settings.workflows[0].stages = (0..64).map(|_| Stage::default()).collect();
    assert!(app.key(key(Key::Char('a'))).is_err());
    assert!(app.key(key(Key::Char('d'))).is_err());
    assert_eq!(app.editor.settings.workflows[0].stages.len(), 64);
}

#[test]
fn workflow_presets_preserve_existing_workflows_and_task() {
    let project = tempfile::tempdir().unwrap();
    let mut app = App::new(
        project.path().into(),
        Settings::default(),
        project.path().join("config.toml"),
        Arc::new(AtomicBool::new(false)),
    );
    let original = toml::to_string(&app.editor.settings.workflows[0]).unwrap();
    app.editor.task = "Keep the imported spec".into();
    app.pick(PickValue::WorkflowPreset(2)).unwrap();
    app.pick(PickValue::WorkflowPreset(2)).unwrap();
    assert_eq!(app.editor.settings.workflows.len(), 3);
    assert_eq!(app.editor.settings.workflows[1].name, "Bug fix");
    assert_eq!(app.editor.settings.workflows[2].name, "Bug fix (2)");
    assert_eq!(app.editor.task, "Keep the imported spec");
    assert_eq!(
        toml::to_string(&app.editor.settings.workflows[0]).unwrap(),
        original
    );
    app.editor.settings.validate().unwrap();
}

#[test]
fn renders_starter_at_common_sizes() {
    let project = tempfile::tempdir().unwrap();
    for (width, height) in [(120, 36), (80, 24), (65, 20), (40, 10)] {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut app = App::new(
            project.path().to_path_buf(),
            Settings::default(),
            PathBuf::from("/tmp/config"),
            Arc::new(AtomicBool::new(false)),
        );
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        if width >= 65 {
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("MODEL ROUTER"));
        }
        app.ui.modal = Some(Modal::Repositories(Box::new(
            PathPicker::new(&app.project, vec![]).unwrap(),
        )));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        if width >= 65 {
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("Choose repository"));
            assert!(text.contains("Ctrl+L"));
        }
    }
}
#[test]
fn harness_editor_applies_only_to_selected_stage_and_supports_cancel() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let original = serde_json::to_value(&app.editor.settings).unwrap();
    app.key(key(Key::Char('c'))).unwrap();
    for _ in 0..5 {
        app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
            .unwrap();
    }
    app.key(key(Key::Right)).unwrap();
    // A changed draft asks before Esc discards it; [n] returns to it.
    app.key(key(Key::Esc)).unwrap();
    assert!(matches!(&app.ui.modal, Some(Modal::Ask(_))));
    app.key(key(Key::Char('n'))).unwrap();
    assert!(matches!(&app.ui.modal, Some(Modal::Harness(_))));
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.modal.is_none());
    assert_eq!(
        serde_json::to_value(&app.editor.settings).unwrap(),
        original
    );
    assert!(!app.editor.dirty);

    app.key(key(Key::Char('c'))).unwrap();
    for _ in 0..5 {
        app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
            .unwrap();
    }
    app.key(key(Key::Right)).unwrap();
    app.key(key(Key::Right)).unwrap();
    app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
        .unwrap();
    app.key(key(Key::Right)).unwrap();
    app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
        .unwrap();
    assert!(app.ui.modal.is_none());
    assert!(app.editor.dirty);
    assert_eq!(
        app.editor.stage().harness_settings["sandbox"],
        "workspace-write"
    );
    assert_eq!(
        app.editor.stage().harness_settings["approval_policy"],
        "on-request"
    );
    for i in 1..3 {
        assert_eq!(
            serde_json::to_value(&app.editor.settings.workflows[0].stages[i]).unwrap(),
            original["workflows"][0]["stages"][i]
        );
    }
    app.key(key(Key::Char('s'))).unwrap();
    assert!(!app.editor.dirty);
    let (saved, _) = config::load(project.path(), Some(&app.editor.config_path)).unwrap();
    assert_eq!(
        saved.workflows[0].stages[0].harness_settings,
        app.editor.stage().harness_settings
    );
    let saved_settings = serde_json::to_value(&app.editor.settings).unwrap();
    app.key(key(Key::Char('c'))).unwrap();
    for _ in 0..4 {
        app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
            .unwrap();
    }
    app.key(KeyEvent::new(Key::Char('a'), Mods::CONTROL))
        .unwrap();
    app.key(KeyEvent::new(Key::Char('k'), Mods::CONTROL))
        .unwrap();
    app.key(key(Key::Char('0'))).unwrap();
    app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
        .unwrap();
    assert!(
        matches!(&app.ui.modal, Some(Modal::Harness(e)) if e.error.contains("greater than zero"))
    );
    assert_eq!(
        serde_json::to_value(&app.editor.settings).unwrap(),
        saved_settings
    );
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.modal.is_none());

    // Both ways of switching harness discard incompatible structured options.
    let mut editor = StageEditor::new(
        app.editor.stage().clone(),
        vec!["codex".into(), "claude".into()],
    );
    editor.fields[2] = textarea("claude");
    assert!(editor.finish().unwrap().harness_settings.is_empty());
    app.pick(PickValue::Harness("claude".into())).unwrap();
    assert!(app.editor.stage().harness_settings.is_empty());
}

#[test]
fn stage_editor_model_field_picks_from_catalog_and_keeps_draft() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let original = serde_json::to_value(&app.editor.settings).unwrap();
    app.key(key(Key::Char('e'))).unwrap();
    // Name, then a typed edit survives the round trip through the picker.
    for c in " draft".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    for _ in 0..3 {
        app.key(key(Key::Tab)).unwrap();
    }
    // The model field does not take typing; Enter opens the catalog.
    app.key(key(Key::Char('x'))).unwrap();
    assert!(
        matches!(&app.ui.modal, Some(Modal::Stage { editor, .. }) if !editor.finish().unwrap().model.contains('x'))
    );
    app.key(key(Key::Enter)).unwrap();
    assert!(app.stage_editor.is_some());
    assert!(matches!(app.ui.modal, Some(Modal::Loading(_))));
    app.key(key(Key::Esc)).unwrap();
    assert!(app.stage_editor.is_none());
    assert!(
        matches!(&app.ui.modal, Some(Modal::Stage { editor, .. }) if editor.finish().unwrap().name.contains("draft"))
    );

    // A failed catalog returns to the editor with the error shown.
    app.key(key(Key::Enter)).unwrap();
    let (sender, receiver) = mpsc::channel();
    sender.send(Err("Offline catalog".into())).unwrap();
    app.catalog = Some(receiver);
    app.poll().unwrap();
    assert!(matches!(&app.ui.modal, Some(Modal::Stage { .. })));
    assert!(app.ui.alert.contains("Offline catalog"));

    // A pick fills model and provider in the draft, not the saved stage.
    app.key(key(Key::Enter)).unwrap();
    let (sender, receiver) = mpsc::channel();
    sender
        .send(Ok(Catalog {
            note: "Offline fixture".into(),
            models: vec![Model {
                id: "picked-model".into(),
                provider: "prov".into(),
                label: "Model".into(),
                source: "test".into(),
                availability: "local".into(),
                ..Model::default()
            }],
            ..Catalog::default()
        }))
        .unwrap();
    app.catalog = Some(receiver);
    app.poll().unwrap();
    assert!(
        matches!(&app.ui.modal, Some(Modal::Picker { title, .. }) if title.starts_with("codex "))
    );
    app.key(key(Key::Enter)).unwrap();
    let draft = match &app.ui.modal {
        Some(Modal::Stage { editor, .. }) => editor.finish().unwrap(),
        other => panic!("expected stage editor, got {}", other.is_some()),
    };
    assert_eq!(draft.model, "picked-model");
    assert_eq!(draft.provider, "prov");
    assert_eq!(
        serde_json::to_value(&app.editor.settings).unwrap(),
        original
    );

    // Backspace returns to the harness default; i allows a custom ID.
    app.key(key(Key::Backspace)).unwrap();
    app.key(key(Key::Char('i'))).unwrap();
    for c in "custom-id".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
        .unwrap();
    assert!(app.ui.modal.is_none());
    assert_eq!(app.editor.stage().model, "custom-id");
    assert_eq!(app.editor.stage().provider, "prov");
}

#[test]
fn repository_line_follows_the_project() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    assert!(app.repo.is_none());
    let repo = project.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        vec!["init", "-q", "-b", "trunk"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    ] {
        crate::storage::git(&repo, &args).unwrap();
    }
    std::fs::write(repo.join("new.txt"), "x").unwrap();
    app.switch_project(repo.clone()).unwrap();
    let info = app.repo.clone().unwrap();
    assert_eq!(info.name, "repo");
    assert_eq!(info.head, "trunk");
    assert!(!info.detached);
    assert_eq!(info.changes, 1);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let header: String = terminal.backend().buffer().content()[..240]
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(header.contains("⎇ repo  trunk  ● 1 changed"), "{header}");
    // Wide enough: the repository shares the header row, flush right.
    let row: String = terminal.backend().buffer().content()[..120]
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        row.contains("4 Harnesses") && row.contains("⎇ repo"),
        "{row}"
    );
    assert!(row.trim_end().ends_with("repo"), "{row}");
}

#[test]
fn harness_model_catalog_restores_draft_on_cancel_error_and_selection() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let original = serde_json::to_value(&app.editor.settings).unwrap();
    app.key(key(Key::Char('c'))).unwrap();
    app.key(KeyEvent::new(Key::Char('a'), Mods::CONTROL))
        .unwrap();
    app.key(KeyEvent::new(Key::Char('k'), Mods::CONTROL))
        .unwrap();
    for c in "draft-model".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(key(Key::F(7))).unwrap();
    assert!(app.model_editor.is_some());
    app.key(key(Key::Esc)).unwrap();
    assert!(
        matches!(&app.ui.modal, Some(Modal::Harness(e)) if e.finish().unwrap().model == "draft-model")
    );

    app.key(key(Key::F(7))).unwrap();
    let (sender, receiver) = mpsc::channel();
    sender.send(Err("Offline catalog".into())).unwrap();
    app.catalog = Some(receiver);
    app.poll().unwrap();
    assert!(
        matches!(&app.ui.modal, Some(Modal::Harness(e)) if e.error.contains("Offline catalog") && e.finish().unwrap().model == "draft-model")
    );

    app.key(key(Key::F(7))).unwrap();
    let (sender, receiver) = mpsc::channel();
    sender
        .send(Ok(Catalog {
            note: "Offline fixture".into(),
            models: vec![Model {
                id: "selected-model".into(),
                provider: "provider".into(),
                label: "Model".into(),
                source: "test".into(),
                availability: "local".into(),
                ..Model::default()
            }],
            ..Catalog::default()
        }))
        .unwrap();
    app.catalog = Some(receiver);
    app.poll().unwrap();
    app.key(key(Key::Enter)).unwrap();
    assert!(
        matches!(&app.ui.modal, Some(Modal::Harness(e)) if e.finish().unwrap().model == "selected-model")
    );
    assert_eq!(
        serde_json::to_value(&app.editor.settings).unwrap(),
        original
    );
    app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
        .unwrap();
    assert_eq!(app.editor.stage().model, "selected-model");
    assert_eq!(app.editor.stage().provider, "provider");
}

/// Choose `id` from the command menu, as a user would.
fn choose(app: &mut App, id: ActionId) -> bool {
    app.key(key(Key::Char(':'))).unwrap();
    let Some(Modal::Picker { items, .. }) = &app.ui.modal else {
        panic!("The command menu did not open");
    };
    let position = items
        .iter()
        .position(|choice| matches!(choice.value, PickValue::Action(a) if a == id))
        .unwrap_or_else(|| panic!("{id:?} is not in the menu"));
    for _ in 0..position {
        app.key(key(Key::Down)).unwrap();
    }
    app.key(key(Key::Enter)).unwrap()
}

fn menu(app: &mut App) -> Vec<ActionId> {
    app.key(key(Key::Char(':'))).unwrap();
    let Some(Modal::Picker { items, .. }) = app.ui.modal.take() else {
        panic!("The command menu did not open");
    };
    items
        .into_iter()
        .map(|choice| match choice.value {
            PickValue::Action(id) => id,
            _ => panic!("The command menu lists a non-action"),
        })
        .collect()
}

fn dialog(app: &App) -> Option<&str> {
    app.ui.modal.as_ref().map(|modal| match modal {
        Modal::Picker { title, .. } | Modal::Text { title, .. } | Modal::Input { title, .. } => {
            title.as_str()
        }
        _ => "another dialog",
    })
}

#[test]
fn command_menu_actions_match_their_keys() {
    let project = tempfile::tempdir().unwrap();
    for (id, code) in [
        (ActionId::ToggleStage, Key::Char(' ')),
        (ActionId::NextView, Key::Tab),
        (ActionId::ShowRuns, Key::Char('3')),
        (ActionId::Themes, Key::Char('T')),
        (ActionId::Help, Key::Char('?')),
        (ActionId::Quit, Key::Char('q')),
    ] {
        let mut pressed = theme_app(project.path());
        let mut chosen = theme_app(project.path());
        let exit = pressed.key(key(code)).unwrap();
        assert_eq!(choose(&mut chosen, id), exit, "{id:?}");
        assert_eq!(
            serde_json::to_value(&chosen.editor.settings).unwrap(),
            serde_json::to_value(&pressed.editor.settings).unwrap(),
            "{id:?}"
        );
        assert_eq!(
            (
                chosen.ui.page,
                chosen.editor.dirty,
                &chosen.ui.message,
                dialog(&chosen)
            ),
            (
                pressed.ui.page,
                pressed.editor.dirty,
                &pressed.ui.message,
                dialog(&pressed)
            ),
            "{id:?}"
        );
    }
    // Errors surface as a key's do: the menu closes and the caller reports them.
    let mut app = theme_app(project.path());
    app.editor.settings.workflows[0].stages.truncate(1);
    let pressed = app.key(key(Key::Char('x'))).unwrap_err().to_string();
    app.key(key(Key::Char(':'))).unwrap();
    for c in "remove".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    assert_eq!(app.key(key(Key::Enter)).unwrap_err().to_string(), pressed);
    assert!(app.ui.modal.is_none());
}

#[test]
fn command_menu_lists_only_actions_for_the_current_context() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let workflow = menu(&mut app);
    assert!(workflow.contains(&ActionId::Run) && workflow.contains(&ActionId::Commands));
    assert!(!workflow.contains(&ActionId::CancelStage) && !workflow.contains(&ActionId::OpenRun));

    app.ui.page = Page::Monitor;
    let idle = menu(&mut app);
    assert!(idle.contains(&ActionId::Continue) && idle.contains(&ActionId::Handoff));
    assert!(!idle.contains(&ActionId::CancelStage) && !idle.contains(&ActionId::Run));

    app.session.adopt(sample_run("a"));
    app.slot_mut().worker = Some(Worker::stub());
    let running = menu(&mut app);
    assert!(running.contains(&ActionId::CancelStage) && running.contains(&ActionId::Handoff));
    assert!(!running.contains(&ActionId::Continue) && !running.contains(&ActionId::Advance));

    // Runs snapshot their workflow, so editing and starting another continue.
    app.ui.page = Page::Workflow;
    let editing = menu(&mut app);
    assert!(editing.contains(&ActionId::Run) && editing.contains(&ActionId::EditTask));
    assert!(!editing.contains(&ActionId::CancelStage));

    // A menu opened during a run does nothing once the run has stopped.
    app.ui.page = Page::Monitor;
    app.key(key(Key::Char(':'))).unwrap();
    app.slot_mut().worker = None;
    for c in "cancel stage".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(key(Key::Enter)).unwrap();
    assert!(app.ui.modal.is_none() && !app.slot().cancel.load(Ordering::Relaxed));

    app.ui.page = Page::Runs;
    assert!(menu(&mut app).contains(&ActionId::OpenRun));
    app.ui.page = Page::Harnesses;
    assert!(menu(&mut app).contains(&ActionId::HarnessDetails));
}

fn screen(terminal: &Terminal<ratatui::backend::TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

#[test]
fn effort_levels_come_from_the_harness_catalog_in_both_editors() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let harness = app.editor.stage().harness.clone();
    let mut listed = Model {
        id: "listed-model".into(),
        efforts: vec!["low".into(), "max".into()],
        default_effort: "max".into(),
        source: "fixture cache".into(),
        ..Model::default()
    };
    listed.label = listed.id.clone();
    app.catalogs.insert(
        harness.clone(),
        Catalog {
            models: vec![listed],
            efforts: vec!["low".into(), "high".into()],
            effort_source: "fixture help".into(),
            ..Catalog::default()
        },
    );
    app.editor.stage_mut().effort = "custom".into();
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();

    // Workflow detail flags a level the catalog does not list.
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("not listed by fixture help"), "{text}");

    // Stage editor: the effort field cycles through harness-wide levels, keeping
    // the unlisted value selectable, and the catalog arrives from the cache.
    app.key(key(Key::Char('e'))).unwrap();
    for _ in 0..5 {
        app.key(key(Key::Tab)).unwrap();
    }
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("custom  not listed"), "{text}");
    assert!(
        text.contains("Levels from fixture help for gpt-6-astra"),
        "{text}"
    );
    app.key(key(Key::Right)).unwrap(); // custom → wraps to Harness default
    app.key(key(Key::Right)).unwrap(); // low
    app.key(key(Key::Right)).unwrap(); // high
    app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
        .unwrap();
    assert_eq!(app.editor.stage().effort, "high");

    // Harness editor: a listed model narrows the levels to its own.
    app.editor.stage_mut().model = "listed-model".into();
    app.key(key(Key::Char('c'))).unwrap();
    app.key(key(Key::Tab)).unwrap(); // provider
    app.key(key(Key::Tab)).unwrap(); // effort
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("max  default"), "{text}");
    assert!(text.contains("high  not listed"), "{text}");
    assert!(
        text.contains("Levels from fixture cache for listed-model"),
        "{text}"
    );
    app.key(key(Key::Right)).unwrap(); // high (unlisted, last) → wraps to default
    app.key(key(Key::Right)).unwrap(); // low
    app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
        .unwrap();
    assert_eq!(app.editor.stage().effort, "low");
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(screen(&terminal).contains("listed by fixture cache"));

    // Switching harness in the stage editor clears the levels and asks again.
    app.key(key(Key::Char('e'))).unwrap();
    app.key(key(Key::Tab)).unwrap();
    app.key(key(Key::Tab)).unwrap();
    app.key(key(Key::Right)).unwrap();
    let changed = match &app.ui.modal {
        Some(Modal::Stage { editor, .. }) => editor.harness(),
        _ => panic!("stage editor expected"),
    };
    assert_ne!(changed, harness);
    app.catalogs.insert(changed.clone(), Catalog::default());
    app.request_catalog(changed);
    for _ in 0..3 {
        app.key(key(Key::Tab)).unwrap();
    }
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(!text.contains("Levels from"), "{text}");
}

fn sample_run(id: &str) -> crate::engine::Run {
    let mut run = crate::report::tests::sample();
    run.id = id.into();
    run
}

fn session(app: &App) -> String {
    app.session
        .slots()
        .map(|(slot, focused)| {
            let id = slot.run.id.clone();
            if focused { format!("{id}*") } else { id }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn session_runs_keep_their_order_as_focus_moves_and_runs_close() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    for id in ["a", "b", "c"] {
        app.session.adopt(sample_run(id));
    }
    assert_eq!(session(&app), "a b c*");
    app.session.cycle_focus(true);
    assert_eq!(session(&app), "a* b c");
    app.session.cycle_focus(false);
    assert_eq!(session(&app), "a b c*");
    assert!(app.session.focus_run("b") && !app.session.focus_run("missing"));
    assert_eq!(session(&app), "a b* c");

    // An active run cannot be closed; closing focuses its neighbour.
    app.slot_mut().worker = Some(Worker::stub());
    assert!(app.session.dismiss().is_err());
    app.slot_mut().worker = None;
    app.session.dismiss().unwrap();
    assert_eq!(session(&app), "a c*");
    app.session.adopt(sample_run("d"));
    assert_eq!(session(&app), "a c d*");
    for _ in 0..3 {
        app.session.dismiss().unwrap();
    }
    assert_eq!(session(&app), "");
    assert!(app.session.run().is_none() && app.session.runs.is_empty());
}

#[test]
fn a_busy_checkout_sends_the_next_run_to_a_worktree() {
    use crate::engine::{Placement, Status};
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    assert_eq!(app.placement(false).unwrap(), (Placement::Project, None));
    let mut busy = sample_run("busy");
    busy.workspace = std::fs::canonicalize(project.path()).unwrap();
    busy.status = Status::AwaitingConfirmation;
    app.session.adopt(busy);
    assert!(
        app.placement(false)
            .unwrap_err()
            .to_string()
            .contains("not a Git repository")
    );
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(init.success());
    app.repo = crate::storage::repo_info(project.path());
    assert_eq!(
        app.placement(false).unwrap(),
        (Placement::WorktreeFromHead, Some("run busy".into()))
    );
    assert_eq!(app.placement(true).unwrap().0, Placement::Worktree);
    // A finished run frees the checkout.
    app.session.run_mut().unwrap().status = Status::Complete;
    assert_eq!(app.placement(false).unwrap(), (Placement::Project, None));
}

fn shift(c: char) -> KeyEvent {
    KeyEvent::new(Key::Char(c), Mods::SHIFT)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(Key::Char(c), Mods::CONTROL)
}

/// The app's screen as text rows at `width` × `height`.
fn rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

/// A Monitor focused on a run whose unattended stage is running.
fn running_app(project: &std::path::Path) -> App {
    let mut app = theme_app(project);
    let mut run = sample_run("live");
    run.status = Status::Running;
    run.cursor = 0;
    app.session.adopt(run);
    app.slot_mut().worker = Some(Worker::stub());
    app.ui.page = Page::Monitor;
    app
}

#[test]
fn cancel_has_its_own_key_and_asks_and_c_never_continues_a_stage_that_just_stopped() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    let cancelled = |app: &App| app.slot().cancel.load(Ordering::Relaxed);
    // `c` continues an idle run; it does nothing to a running one.
    app.key(key(Key::Char('c'))).unwrap();
    assert!(!cancelled(&app) && app.ui.modal.is_none());
    app.key(shift('C')).unwrap();
    assert!(
        matches!(&app.ui.modal, Some(Modal::Ask(ask)) if matches!(**ask, Ask::CancelStage { .. }))
    );
    assert!(!cancelled(&app), "Cancelling waits for confirmation");
    assert!(rows(&mut app, 65, 20).join("\n").contains("Cancel stage"));
    app.key(key(Key::Char('n'))).unwrap();
    assert!(app.ui.modal.is_none() && !cancelled(&app));

    // The stage stops while the question is open: yes cancels nothing.
    app.key(shift('C')).unwrap();
    let run = app.slot().run.clone();
    app.slot()
        .worker
        .as_ref()
        .unwrap()
        .finish(run, Ok(Action::Stopped));
    app.poll().unwrap();
    assert!(!app.session.running());
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.modal.is_none() && !cancelled(&app));
    assert!(
        app.ui.message.contains("nothing was cancelled"),
        "{}",
        app.ui.message
    );
    // `c` pressed as the stage stopped does not start the next one.
    assert!(app.key(key(Key::Char('c'))).is_err());
    assert!(!app.session.running());

    // A confirmed cancel of a running stage.
    app.slot_mut().worker = Some(Worker::stub());
    app.key(shift('C')).unwrap();
    app.key(key(Key::Enter)).unwrap();
    assert!(cancelled(&app));

    // Taking over an unattended stage stops it only once confirmed.
    let mut app = running_app(project.path());
    app.key(key(Key::Char('t'))).unwrap();
    assert!(
        matches!(&app.ui.modal, Some(Modal::Ask(ask)) if matches!(**ask, Ask::TakeOver { .. }))
    );
    assert!(!cancelled(&app) && !app.slot().takeover);
    app.key(key(Key::Char('y'))).unwrap();
    assert!(cancelled(&app) && app.slot().takeover);
}

#[test]
fn modified_keys_do_not_trigger_page_actions() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let stages = app.editor.settings.workflows[0].stages.len();
    for c in ['r', 'x', 'd', 'X'] {
        app.key(ctrl(c)).unwrap();
        app.key(KeyEvent::new(Key::Char(c), Mods::ALT)).unwrap();
    }
    assert_eq!(app.editor.settings.workflows[0].stages.len(), stages);
    assert!(app.ui.modal.is_none() && app.starting.is_none() && !app.editor.dirty);
    // Ctrl+C still interrupts: with nothing running and nothing unsaved, it exits.
    assert!(app.key(ctrl('c')).unwrap());
}

#[test]
fn removed_stages_and_deleted_workflows_can_be_undone() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let original = serde_json::to_value(&app.editor.settings.workflows).unwrap();
    app.key(key(Key::Char('u'))).unwrap();
    assert_eq!(app.ui.message, "Nothing to undo.");
    let name = app.editor.stage().name.clone();
    app.key(key(Key::Char('x'))).unwrap();
    assert_eq!(
        app.ui.message,
        format!("Removed stage “{name}” — [u] to undo.")
    );
    app.key(key(Key::Char('j'))).unwrap();
    app.key(key(Key::Char('x'))).unwrap();
    app.key(key(Key::Char('u'))).unwrap();
    app.key(key(Key::Char('u'))).unwrap();
    assert_eq!(
        serde_json::to_value(&app.editor.settings.workflows).unwrap(),
        original
    );
    assert_eq!(app.editor.selected, 0);
    assert!(app.editor.dirty);

    app.pick(PickValue::WorkflowPreset(2)).unwrap();
    let both = serde_json::to_value(&app.editor.settings.workflows).unwrap();
    app.key(shift('X')).unwrap();
    assert!(matches!(&app.ui.modal, Some(Modal::Ask(_))));
    assert!(
        rows(&mut app, 65, 20)
            .join("\n")
            .contains("Delete workflow")
    );
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(app.editor.settings.workflows.len(), 2);
    app.key(shift('X')).unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert_eq!(app.editor.settings.workflows.len(), 1);
    assert!(app.ui.message.contains("[u] to undo"), "{}", app.ui.message);
    app.key(key(Key::Char('u'))).unwrap();
    assert_eq!(
        serde_json::to_value(&app.editor.settings.workflows).unwrap(),
        both
    );
    assert_eq!(app.editor.workflow, 1);
}

#[test]
fn exit_asks_to_save_unsaved_edits_from_both_q_and_ctrl_c() {
    let project = tempfile::tempdir().unwrap();
    let saved = |app: &App| app.editor.config_path.exists();
    // Nothing unsaved: both exit at once.
    let mut app = theme_app(project.path());
    assert!(app.key(key(Key::Char('q'))).unwrap());
    assert!(app.key(ctrl('c')).unwrap());

    for (exit, answer, save) in [
        (key(Key::Char('q')), 'n', false),
        (ctrl('c'), 'n', false),
        (key(Key::Char('q')), 'y', true),
        (ctrl('c'), 'y', true),
    ] {
        let _ = std::fs::remove_file(&app.editor.config_path);
        let mut app = theme_app(project.path());
        app.key(key(Key::Char(' '))).unwrap();
        assert!(app.editor.dirty);
        assert!(!app.key(exit).unwrap());
        assert!(
            matches!(&app.ui.modal, Some(Modal::Ask(ask)) if matches!(**ask, Ask::SaveAndExit))
        );
        assert!(rows(&mut app, 65, 20).join("\n").contains("Save changes?"));
        // Esc stays, with the edits.
        app.key(key(Key::Esc)).unwrap();
        assert!(app.ui.modal.is_none() && app.editor.dirty && !saved(&app));
        assert!(!app.key(exit).unwrap());
        assert!(app.key(key(Key::Char(answer))).unwrap(), "{answer} exits");
        assert_eq!(saved(&app), save, "{answer}");
    }
}

#[test]
fn switching_project_drops_catalogs_and_results_for_the_old_one() {
    let project = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let (old_sender, receiver) = mpsc::channel();
    app.prefetch = Some(("codex".into(), Job::from_receiver(app.generation, receiver)));
    app.prefetch_queue.push_back("claude".into());
    app.catalogs.insert("pi".into(), Catalog::default());
    let old = app.generation;
    app.switch_project(other.path().into()).unwrap();
    assert_ne!(app.generation, old);
    assert!(app.catalogs.is_empty() && app.prefetch.is_none() && app.prefetch_queue.is_empty());
    assert!(old_sender.send(Ok(Catalog::default())).is_err());
    // A result tagged with the old project is dropped, even if its job survived.
    let (sender, receiver) = mpsc::channel();
    app.prefetch = Some(("codex".into(), Job::from_receiver(old, receiver)));
    sender.send(Ok(Catalog::default())).unwrap();
    let (runs, receiver) = mpsc::channel();
    app.runs.job = Some(Job::from_receiver(old, receiver));
    runs.send(Ok(crate::history::list(project.path()).unwrap()))
        .unwrap();
    app.poll().unwrap();
    assert!(!app.catalogs.contains_key("codex"));
    assert!(app.prefetch.is_none() && app.runs.job.is_none());

    // From the browser, the project opens in the background; a bad path
    // returns to the browser with the error.
    let picker = Box::new(PathPicker::new(project.path(), vec![]).unwrap());
    app.begin_switch(project.path().join("missing"), picker);
    assert!(matches!(&app.ui.modal, Some(Modal::Loading(_))));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(&app.ui.modal, Some(Modal::Loading(_))) && std::time::Instant::now() < deadline {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(matches!(&app.ui.modal, Some(Modal::Repositories(p)) if p.error.contains("not found")));
    let picker = Box::new(PathPicker::new(project.path(), vec![]).unwrap());
    app.begin_switch(project.path().into(), picker);
    while app.ui.modal.is_some() && std::time::Instant::now() < deadline {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(app.project, std::fs::canonicalize(project.path()).unwrap());
}

#[test]
fn output_wraps_by_display_width() {
    use super::monitor::wrap;
    assert_eq!(wrap("雪雪雪", 4), ["雪雪", "雪"]);
    assert_eq!(wrap("ab😀cd", 3), ["ab", "😀c", "d"]);
    assert_eq!(wrap("", 5), [""]);
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    app.slot_mut().push_log(format!("● {}", "雪".repeat(20)));
    app.slot_mut().push_log("tail 😀 end".into());
    let screen = rows(&mut app, 65, 20).join("");
    // Every character reaches the screen; none is cut at the panel edge.
    assert_eq!(screen.matches('雪').count(), 20, "{screen}");
    // A wide character's second cell renders as a space.
    assert!(
        screen.replace("😀 ", "😀").contains("tail 😀 end"),
        "{screen}"
    );
}

#[test]
fn runs_with_no_stages_and_empty_choices_do_not_panic() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    let run = app.session.run_mut().unwrap();
    run.stages.clear();
    run.workflow.stages.clear();
    run.cursor = 3;
    for (width, height) in [(65, 20), (80, 24)] {
        rows(&mut app, width, height);
    }
    app.slot_mut().worker = None;
    app.session.run_mut().unwrap().status = Status::AwaitingConfirmation;
    app.key(key(Key::Char('h'))).unwrap();
    assert!(matches!(&app.ui.modal, Some(Modal::Text { .. })));
    app.ui.modal = None;
    app.key(key(Key::Char('e'))).unwrap();
    rows(&mut app, 65, 20);
    // A harness field with no harnesses to cycle through.
    let mut editor = StageEditor::new(Stage::default(), vec![]);
    for _ in 0..2 {
        editor.key(key(Key::Down));
    }
    editor.key(key(Key::Right));
    editor.key(key(Key::Left));
}

#[test]
fn worker_output_never_blocks_and_marks_dropped_lines() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    let worker = app.slot().worker.as_ref().unwrap();
    let text: String = (0..2500).map(|n| format!("line {n}\n")).collect();
    worker.send(RunEvent::Output(text));
    let mut first = sample_run("live");
    first.status = Status::Running;
    let mut latest = first.clone();
    latest.repair_passes = 7;
    worker.send(RunEvent::State(Box::new(first)));
    worker.send(RunEvent::State(Box::new(latest)));
    app.poll().unwrap();
    let logs: Vec<_> = app.slot().logs.iter().cloned().collect();
    assert!(logs.len() <= 400);
    assert_eq!(logs.last().unwrap(), "line 2499");
    assert!(logs[0].contains("2101 lines skipped"), "{:?}", &logs[..3]);
    assert_eq!(app.session.run().unwrap().repair_passes, 7);
}

#[test]
fn interactive_terminal_waits_while_a_dialog_is_open() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.session.launch_interactive = true;
    app.ui.modal = Some(Modal::Loading("Reading".into()));
    assert!(!app.take_launch());
    assert!(app.ui.message.contains("press [t]"), "{}", app.ui.message);
    app.ui.modal = None;
    app.session.launch_interactive = true;
    assert!(app.take_launch());
    assert!(!app.take_launch());
}

#[test]
fn text_dialog_scrolling_stops_at_the_last_line() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.text("Three lines", "one\ntwo\nthree".into());
    for _ in 0..3 {
        app.key(key(Key::PageDown)).unwrap();
    }
    assert!(matches!(&app.ui.modal, Some(Modal::Text { scroll: 2, .. })));
    app.key(key(Key::Up)).unwrap();
    assert!(matches!(&app.ui.modal, Some(Modal::Text { scroll: 1, .. })));
}

#[test]
fn stage_editor_asks_before_discarding_changes() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let original = serde_json::to_value(&app.editor.settings).unwrap();
    // Unchanged: Esc closes at once.
    app.key(key(Key::Char('e'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.modal.is_none());
    app.key(key(Key::Char('e'))).unwrap();
    app.key(key(Key::Char('!'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(matches!(&app.ui.modal, Some(Modal::Ask(_))));
    app.key(key(Key::Esc)).unwrap();
    assert!(
        matches!(&app.ui.modal, Some(Modal::Stage { editor, .. }) if editor.finish().unwrap().name.ends_with('!'))
    );
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.modal.is_none());
    assert_eq!(
        serde_json::to_value(&app.editor.settings).unwrap(),
        original
    );
}

#[test]
fn monitor_results_and_dialogs_fit_the_minimum_size() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    for n in 0..50 {
        app.slot_mut().push_log(format!("✓ step {n} 雪"));
    }
    let screen = rows(&mut app, 65, 20).join("\n");
    assert!(screen.contains("✓ step 49 雪"), "{screen}");
    assert!(screen.contains("[C] cancel stage"), "{screen}");
    // Finished: results, and the two panels scroll within their own lengths.
    app.slot_mut().worker = None;
    app.session.adopt(sample_run("done"));
    let screen = rows(&mut app, 65, 20).join("\n");
    assert!(screen.contains("Results"), "{screen}");
    for _ in 0..50 {
        app.key(key(Key::PageDown)).unwrap();
    }
    let limit = app.slot().results_limit.get();
    assert_eq!(app.slot().results_scroll, limit);
    app.key(key(Key::PageUp)).unwrap();
    assert_eq!(app.slot().results_scroll[0], limit[0].saturating_sub(5));
    for modal in [
        Modal::Ask(Box::new(Ask::CancelStage { run: "done".into() })),
        Modal::Loading("Opening project…".into()),
        Modal::Text {
            title: "Guide".into(),
            body: super::actions::guide(),
            scroll: 0,
            guide: true,
        },
    ] {
        app.ui.modal = Some(modal);
        let screen = rows(&mut app, 65, 20).join("\n");
        assert!(
            ["Cancel stage", "Please wait", "WORKFLOW"]
                .iter()
                .any(|text| screen.contains(text)),
            "{screen}"
        );
    }
}

/// A session of four runs: done, awaiting review, running (focused), and
/// waiting for its terminal.
fn mixed_session(project: &std::path::Path) -> App {
    let mut app = theme_app(project);
    for (id, status) in [
        ("done", Status::Complete),
        ("review", Status::AwaitingConfirmation),
        ("busy", Status::Running),
        ("term", Status::Interactive),
    ] {
        let mut run = sample_run(id);
        run.status = status;
        app.session.adopt(run);
        if status == Status::Running {
            app.slot_mut().worker = Some(Worker::stub());
        }
    }
    app.session.focus_run("busy");
    app.ui.page = Page::Monitor;
    app
}

#[test]
fn session_runs_dialog_lists_runs_focuses_cancels_and_jumps_to_input() {
    let project = tempfile::tempdir().unwrap();
    let mut app = mixed_session(project.path());
    assert_eq!(session(&app), "done review busy* term");
    assert_eq!(app.session.needing_input(), 2);
    let text = rows(&mut app, 120, 30).join("\n");
    assert!(text.contains("◆ 2 need input [N]"), "{text}");

    app.key(shift('L')).unwrap();
    assert!(matches!(
        app.ui.modal,
        Some(Modal::SessionRuns { selected: 2 })
    ));
    let text = rows(&mut app, 120, 30).join("\n");
    assert!(text.contains("Session runs · 2 need input"), "{text}");
    for id in ["done", "review", "busy", "term"] {
        assert!(text.contains(id), "{id}: {text}");
    }
    assert_eq!(text.matches("◆ needs input").count(), 2, "{text}");
    assert!(text.contains("running"), "{text}");

    // [C] on the running run asks, then cancels only that run.
    app.key(shift('C')).unwrap();
    assert!(matches!(app.ui.modal.as_deref_ask(), Some(Ask::CancelRun { run }) if run == "busy"));
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.slot().cancel.load(Ordering::Relaxed));
    assert!(
        app.ui.message.contains("Stopping the stage of busy"),
        "{}",
        app.ui.message
    );
    app.slot().cancel.store(false, Ordering::Relaxed);

    // [C] on a run that is not running cancels nothing.
    app.key(shift('L')).unwrap();
    app.key(key(Key::Up)).unwrap();
    app.key(shift('C')).unwrap();
    assert!(matches!(app.ui.modal, Some(Modal::SessionRuns { .. })));
    assert!(
        app.ui.message.contains("nothing to cancel"),
        "{}",
        app.ui.message
    );

    // [Enter] focuses the selected run.
    app.key(key(Key::Enter)).unwrap();
    assert!(app.ui.modal.is_none());
    assert_eq!(session(&app), "done review* busy term");
    assert!(
        app.ui.message.contains("[a] advances"),
        "{}",
        app.ui.message
    );

    // [n] in the dialog and [N] anywhere jump to the next run needing input, wrapping.
    app.key(shift('L')).unwrap();
    app.key(key(Key::Char('n'))).unwrap();
    assert_eq!(session(&app), "done review busy term*");
    assert!(
        app.ui.message.contains("[i] opens it"),
        "{}",
        app.ui.message
    );
    app.ui.page = Page::Workflow;
    app.key(shift('N')).unwrap();
    assert_eq!(session(&app), "done review* busy term");
    assert_eq!(app.ui.page, Page::Monitor);
    for slot in &mut app.session.runs {
        slot.run.status = Status::Complete;
    }
    app.key(shift('N')).unwrap();
    assert!(
        app.ui
            .message
            .contains("No run of this session needs input")
    );
    assert!(!rows(&mut app, 120, 30).join("\n").contains("need input"));
}

trait AskModal {
    fn as_deref_ask(&self) -> Option<&Ask>;
}
impl AskModal for Option<Modal> {
    fn as_deref_ask(&self) -> Option<&Ask> {
        match self {
            Some(Modal::Ask(ask)) => Some(ask),
            _ => None,
        }
    }
}

#[test]
fn background_run_events_queue_notices_instead_of_clobbering() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    for id in ["first", "second", "third"] {
        let mut run = sample_run(id);
        run.status = Status::Running;
        app.session.adopt(run);
        app.slot_mut().worker = Some(Worker::stub());
    }
    app.ui.page = Page::Workflow;
    let finish = |app: &App, index: usize, status, result| {
        let slot = &app.session.runs[index];
        let mut run = slot.run.clone();
        run.status = status;
        run.error = Some("tests failed".into());
        slot.worker.as_ref().unwrap().finish(run, result);
    };
    finish(&app, 0, Status::Blocked, Ok(Action::Stopped));
    finish(&app, 1, Status::AwaitingConfirmation, Ok(Action::Confirm));
    app.poll().unwrap();
    assert_eq!(app.session.notices.len(), 2);
    let workflow = app.session.runs[0].run.workflow.name.clone();
    assert!(app.session.notices[0].alert);
    assert!(
        app.session.notices[0]
            .text
            .starts_with(&format!("{workflow} (first) stopped: tests failed")),
        "{}",
        app.session.notices[0].text
    );
    // The newest shows; the status line counts the rest.
    assert!(app.ui.message.contains("(second)") && app.ui.message.contains("[N] jumps"));
    assert_eq!(app.more_notices(), 1);
    let text = rows(&mut app, 120, 30).join("\n");
    assert!(text.contains("(+1 more · [!] notices)"), "{text}");

    // The focused run's stop is kept too, and [!] lists all, newest first.
    let mut run = app.slot().run.clone();
    run.status = Status::Complete;
    app.slot()
        .worker
        .as_ref()
        .unwrap()
        .finish(run, Ok(Action::Complete));
    app.poll().unwrap();
    assert_eq!(app.more_notices(), 2);
    app.key(shift('!')).unwrap();
    let Some(Modal::Text { body, .. }) = &app.ui.modal else {
        panic!("Notice history did not open");
    };
    let lines: Vec<_> = body.lines().collect();
    assert_eq!(lines.len(), 3, "{body}");
    assert!(lines[0].contains("(third) complete"), "{body}");
    assert!(
        lines[2].contains("✗") && lines[2].contains("(first)"),
        "{body}"
    );
    assert_eq!(app.more_notices(), 0);
    app.key(key(Key::Esc)).unwrap();
    for n in 0..60 {
        app.push_notice(format!("notice {n}"), false);
    }
    assert_eq!(app.session.notices.len(), super::session::NOTICES);
    assert_eq!(app.session.notices.back().unwrap().text, "notice 59");
}

#[test]
fn notifications_dialog_saves_backends_with_older_preferences() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    std::fs::write(
        &app.ui.theme_path,
        r#"{"theme":"nord","vim_motions":false}"#,
    )
    .unwrap();
    let loaded = theme::load(&app.ui.theme_path).unwrap();
    app.ui.theme = loaded.theme;
    app.ui.notifications = loaded.notifications;
    assert!(app.ui.notifications.terminal && !app.ui.notifications.desktop);

    // The command menu reaches the dialog, as F9 does.
    app.key(key(Key::Char(':'))).unwrap();
    app.paste("Notifications");
    app.key(key(Key::Enter)).unwrap();
    assert!(matches!(app.ui.modal, Some(Modal::Notifications { .. })));
    let text = rows(&mut app, 100, 30).join("\n");
    assert!(
        text.contains("[x] Terminal") && text.contains("[ ] Desktop"),
        "{text}"
    );

    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Char(' '))).unwrap();
    let saved = theme::load(&app.ui.theme_path).unwrap();
    assert!(saved.notifications.desktop && saved.notifications.terminal);
    assert_eq!(saved.theme, Theme::Nord);
    assert!(app.ui.notifications.desktop);

    // Webhook URL: invalid ones are refused, a valid one turns the webhook on.
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Enter)).unwrap();
    assert!(matches!(app.ui.modal, Some(Modal::Input { .. })));
    app.paste("ftp://nope");
    assert!(app.key(ctrl('s')).is_err());
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::F(9))).unwrap();
    for _ in 0..3 {
        app.key(key(Key::Down)).unwrap();
    }
    app.key(key(Key::Enter)).unwrap();
    app.paste("https://ntfy.sh/mr-test");
    app.key(ctrl('s')).unwrap();
    assert!(matches!(
        app.ui.modal,
        Some(Modal::Notifications { selected: 3, .. })
    ));
    let saved = theme::load(&app.ui.theme_path).unwrap();
    assert!(saved.notifications.webhook);
    assert_eq!(saved.notifications.webhook_url, "https://ntfy.sh/mr-test");

    // The test button reports what it sent.
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Enter)).unwrap();
    let Some(Modal::Notifications { status, .. }) = &app.ui.modal else {
        panic!("dialog closed");
    };
    assert!(status.starts_with("Sent"), "{status}");
    app.key(key(Key::Esc)).unwrap();

    // Other preference changes keep the backends.
    app.key(key(Key::F(8))).unwrap();
    app.pick(PickValue::Theme(Theme::Dracula)).unwrap();
    let saved = theme::load(&app.ui.theme_path).unwrap();
    assert!(saved.vim_motions && saved.notifications.desktop && saved.notifications.webhook);
    assert_eq!(saved.theme, Theme::Dracula);
}

/// The app drawn into a test buffer.
fn buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// The cell where `text` starts on screen.
fn cell_at(buffer: &ratatui::buffer::Buffer, text: &str) -> ratatui::buffer::Cell {
    let area = buffer.area;
    for y in 0..area.height {
        let row: Vec<_> = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
        let joined: String = row.concat();
        if let Some(byte) = joined.find(text) {
            // Columns, not bytes: count the symbols before the match.
            let mut seen = 0;
            for (x, symbol) in row.iter().enumerate() {
                if seen == byte {
                    return buffer[(x as u16, y)].clone();
                }
                seen += symbol.len();
            }
        }
    }
    panic!("{text:?} is not on screen");
}

const REVIEW_PATCH: &str = "diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
-fn gone() {}
+fn fresh() {}
 fn kept() {}
@@ -20 +20 @@
-old tail
+new tail
diff --git a/docs/notes.md b/docs/notes.md
new file mode 100644
--- /dev/null
+++ b/docs/notes.md
@@ -0,0 +1 @@
+needle here
";

fn diff_app(project: &std::path::Path, theme: Theme) -> App {
    let mut app = theme_app(project);
    app.ui.theme = theme;
    let mut run = sample_run("done");
    run.project = project.into();
    run.workspace = project.into();
    app.session.adopt(run);
    app.ui.page = Page::Monitor;
    app.ui.modal = Some(Modal::Diff(Box::new(super::diff_view::DiffView::new(
        REVIEW_PATCH.into(),
    ))));
    app
}

#[test]
fn diff_dialog_colours_lines_by_theme_and_lists_files() {
    let project = tempfile::tempdir().unwrap();
    for theme in [Theme::ModelRouter, Theme::Terminal, Theme::CatppuccinLatte] {
        let mut app = diff_app(project.path(), theme);
        let palette = theme.palette();
        let screen = buffer(&mut app, 120, 30);
        assert_eq!(
            cell_at(&screen, "+fn fresh").fg,
            palette.success,
            "{theme:?}"
        );
        assert_eq!(cell_at(&screen, "-fn gone").fg, palette.red, "{theme:?}");
        assert_eq!(cell_at(&screen, "@@ -1,2").fg, palette.blue, "{theme:?}");
        assert_eq!(cell_at(&screen, " fn kept").fg, palette.fg, "{theme:?}");
        if theme == Theme::Terminal {
            assert_eq!(
                cell_at(&screen, "+fn fresh").fg,
                ratatui::style::Color::Green
            );
        }
        let text = rows(&mut app, 120, 30).join("\n");
        for expected in [
            "2 files · +3 −2",
            "M src/lib.rs +2 −2",
            "A docs/notes.md +1 −0",
            "src/lib.rs  2 hunks",
        ] {
            assert!(
                text.contains(expected),
                "{theme:?} lacks {expected:?}\n{text}"
            );
        }
    }
    // The smallest screen still shows the file list, hunks, and keys.
    let mut app = diff_app(project.path(), Theme::ModelRouter);
    app.key(key(Key::Char('w'))).unwrap();
    let text = rows(&mut app, 65, 20).join("\n");
    for expected in ["M src/lib.rs", "+fn fresh", "wrap", "[{ }] hunk"] {
        assert!(text.contains(expected), "65x20 lacks {expected:?}\n{text}");
    }
}

#[test]
fn diff_dialog_keys_move_between_files_hunks_and_matches() {
    let project = tempfile::tempdir().unwrap();
    let mut app = diff_app(project.path(), Theme::ModelRouter);
    rows(&mut app, 100, 30);
    app.key(key(Key::Char('}'))).unwrap();
    app.key(key(Key::Char('}'))).unwrap();
    assert!(rows(&mut app, 100, 30).join("\n").contains("hunk 2/2"));
    app.key(key(Key::Char(']'))).unwrap();
    let text = rows(&mut app, 100, 30).join("\n");
    assert!(
        text.contains("+needle here") && !text.contains("+fn fresh"),
        "{text}"
    );
    app.key(key(Key::Char('['))).unwrap();
    // Search reaches the other file and highlights the match.
    for c in "/needle".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(key(Key::Enter)).unwrap();
    let palette = Theme::ModelRouter.palette();
    let screen = buffer(&mut app, 100, 30);
    assert_eq!(cell_at(&screen, "needle here").bg, palette.accent);
    assert!(rows(&mut app, 100, 30).join("\n").contains("/needle · 1/1"));
    // Esc clears the search, then closes; the dialog's keys stay local.
    app.key(key(Key::Esc)).unwrap();
    assert!(matches!(app.ui.modal, Some(Modal::Diff(_))));
    assert_eq!(app.ui.page, Page::Monitor);
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.modal.is_none());
}

#[test]
fn diff_dialog_hands_the_patch_and_files_to_external_programs() {
    let project = tempfile::tempdir().unwrap();
    let mut app = diff_app(project.path(), Theme::ModelRouter);
    rows(&mut app, 100, 30);
    app.key(key(Key::Char('o'))).unwrap();
    let launch = app.ui.external.take().expect("pager queued");
    let file = launch.argv.last().unwrap().clone();
    let coloured = std::fs::read_to_string(&file).unwrap();
    assert!(
        coloured.contains("\x1b[32m+fn fresh() {}\x1b[0m"),
        "{coloured}"
    );
    drop(launch);
    assert!(
        !std::path::Path::new(&file).exists(),
        "temporary diff removed"
    );
    assert!(
        matches!(app.ui.modal, Some(Modal::Diff(_))),
        "dialog stays open"
    );

    app.key(key(Key::Char('e'))).unwrap();
    let launch = app.ui.external.take().expect("editor queued");
    let target = project.path().join("src/lib.rs");
    assert!(
        launch.argv.iter().any(|arg| {
            let arg = arg.to_string_lossy();
            arg == target.to_string_lossy() || arg.starts_with(&*target.to_string_lossy())
        }),
        "{launch:?}"
    );
    assert_eq!(launch.cwd.as_deref(), Some(project.path()));
}

#[test]
fn paused_output_keeps_its_place_as_new_lines_arrive() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    app.slot_mut().logs.clear();
    for i in 0..100 {
        app.slot_mut().push_log(format!("line {i:03}"));
    }
    let output = |app: &mut App| -> Vec<String> {
        rows(app, 80, 30)
            .into_iter()
            .filter(|row| row.contains("line "))
            .collect()
    };
    assert!(output(&mut app).iter().any(|r| r.contains("line 099")));
    app.key(key(Key::PageUp)).unwrap();
    let paused = output(&mut app);
    assert!(!paused.iter().any(|r| r.contains("line 099")));
    assert!(
        rows(&mut app, 80, 30)
            .join("\n")
            .contains("▲ paused — End to follow")
    );
    for i in 100..150 {
        app.slot_mut().push_log(format!("line {i:03}"));
    }
    assert_eq!(
        output(&mut app),
        paused,
        "new output must not move a paused view"
    );
    // Old lines leaving the bounded log do not move it either.
    let first = paused[0].clone();
    for i in 150..300 {
        app.slot_mut().push_log(format!("line {i:03}"));
    }
    assert_eq!(output(&mut app)[0], first);
    // Scrolling back down to the end follows again.
    app.key(key(Key::End)).unwrap();
    let text = rows(&mut app, 80, 30).join("\n");
    assert!(
        text.contains("line 299") && !text.contains("paused"),
        "{text}"
    );
    app.key(key(Key::Home)).unwrap();
    assert!(!app.slot().log_view.follow);
    app.key(key(Key::Char('f'))).unwrap();
    assert!(app.slot().log_view.follow);
    app.key(key(Key::Char('f'))).unwrap();
    assert!(!app.slot().log_view.follow);
    for _ in 0..100 {
        app.key(key(Key::PageDown)).unwrap();
    }
    assert!(app.slot().log_view.follow, "reaching the end follows");
}

#[test]
fn output_search_highlights_matches_and_owns_n_while_shown() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    app.slot_mut().logs.clear();
    for i in 0..80 {
        app.slot_mut().push_log(if i % 20 == 5 {
            format!("✗ failure number {i}")
        } else {
            format!("step {i}")
        });
    }
    rows(&mut app, 80, 30);
    app.key(key(Key::Char('/'))).unwrap();
    for c in "FAILURE".chars() {
        app.key(shift(c)).unwrap();
    }
    assert!(rows(&mut app, 80, 30).join("\n").contains("/FAILURE▏"));
    // Smart case: an uppercase query is case-sensitive.
    app.key(key(Key::Enter)).unwrap();
    assert!(rows(&mut app, 80, 30).join("\n").contains("no matches"));
    app.key(key(Key::Char('/'))).unwrap();
    for c in "failure".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(key(Key::Enter)).unwrap();
    let text = rows(&mut app, 80, 30).join("\n");
    assert!(text.contains("/failure · 4/4"), "{text}");
    let palette = app.ui.palette();
    let screen = buffer(&mut app, 80, 30);
    assert_eq!(cell_at(&screen, "failure number 65").bg, palette.accent);
    assert!(!app.slot().log_view.follow, "a match pauses the output");
    app.key(shift('N')).unwrap();
    app.key(shift('N')).unwrap();
    let text = rows(&mut app, 80, 30).join("\n");
    assert!(
        text.contains("2/4") && text.contains("failure number 25"),
        "{text}"
    );
    app.key(key(Key::Char('n'))).unwrap();
    assert!(rows(&mut app, 80, 30).join("\n").contains("3/4"));
    // Idle, `n` returns to the workflow, except while a search is shown.
    app.slot_mut().worker = None;
    app.key(key(Key::Char('n'))).unwrap();
    assert_eq!(app.ui.page, Page::Monitor);
    app.key(key(Key::Esc)).unwrap();
    assert!(app.slot().log_search.is_none());
    app.key(key(Key::Char('n'))).unwrap();
    assert_eq!(app.ui.page, Page::Workflow);
}

#[test]
fn handoff_dialog_colours_findings_and_tests_and_opens_changed_files() {
    use crate::handoff::{Finding, Severity, TestEvidence, TestStatus};
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    let run = app.session.run_mut().unwrap();
    run.workspace = project.path().into();
    let handoff = run.stages[0].handoff.get_or_insert_with(|| {
        serde_json::from_str(r#"{"summary":"s","verdict":"needs_changes"}"#).unwrap()
    });
    handoff.summary = "Implemented the parser".into();
    handoff.findings = vec![
        Finding {
            severity: Severity::Blocking,
            description: "Crash on empty input".into(),
        },
        Finding {
            severity: Severity::Advisory,
            description: "Rename a helper".into(),
        },
    ];
    handoff.tests = vec![
        TestEvidence {
            command: "cargo test".into(),
            status: TestStatus::Passed,
            required: true,
        },
        TestEvidence {
            command: "cargo clippy".into(),
            status: TestStatus::Failed,
            required: false,
        },
    ];
    handoff.files_changed = vec!["src/parser.rs".into(), "README.md".into()];
    app.key(key(Key::Char('h'))).unwrap();
    assert!(matches!(app.ui.modal, Some(Modal::Handoff(_))));
    let palette = app.ui.palette();
    let screen = buffer(&mut app, 120, 36);
    assert_eq!(cell_at(&screen, "! blocking").fg, palette.red);
    assert_eq!(cell_at(&screen, "· advisory").fg, palette.warn);
    assert_eq!(cell_at(&screen, "✓ cargo test").fg, palette.success);
    assert_eq!(cell_at(&screen, "✗ cargo clippy").fg, palette.red);
    let text = rows(&mut app, 120, 36).join("\n");
    for expected in [
        "Implemented the parser",
        "passed · required",
        "failed · optional",
        "Files changed (2)",
        "README.md",
    ] {
        assert!(text.contains(expected), "lacks {expected:?}\n{text}");
    }
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Enter)).unwrap();
    let launch = app.ui.external.take().expect("editor queued");
    assert_eq!(
        launch.argv.last().unwrap(),
        project.path().join("README.md").as_os_str()
    );
    assert!(matches!(app.ui.modal, Some(Modal::Handoff(_))));
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.modal.is_none());
}

#[test]
fn open_log_pages_the_focused_attempts_stdout() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    let run = app.session.run_mut().unwrap();
    let attempt = run.stages[0].attempts.last_mut().unwrap();
    attempt.directory = project.path().join("attempt");
    assert!(app.key(shift('O')).is_err(), "no log yet");
    std::fs::create_dir_all(project.path().join("attempt")).unwrap();
    std::fs::write(project.path().join("attempt/stdout.log"), "raw").unwrap();
    app.key(shift('O')).unwrap();
    let launch = app.ui.external.take().expect("pager queued");
    assert_eq!(
        launch.argv.last().unwrap(),
        project.path().join("attempt/stdout.log").as_os_str()
    );
}
