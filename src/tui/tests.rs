use super::{
    actions::ActionId,
    app::{App, Worker, draw},
    modal::{Modal, PickValue},
    path_picker::PathPicker,
    theme::{self, Theme},
    widgets::textarea,
    workflow_page::StageEditor,
};
use crate::{
    catalog::{Catalog, Model},
    config::{self, Role, Settings, Stage},
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
    app.theme_path = project.join("appearance.json");
    app.harness_checks.probe = |_, _| vec![];
    app
}

#[test]
fn vim_opt_in_persists_without_changing_the_theme_or_project() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    assert!(!app.vim_motions);
    assert!(!theme::load(&app.theme_path).unwrap().vim_motions);
    std::fs::write(&app.theme_path, r#"{"theme":"dracula"}"#).unwrap();
    let legacy = theme::load(&app.theme_path).unwrap();
    assert!(!legacy.vim_motions);
    app.theme = legacy.theme;
    app.key(key(Key::F(8))).unwrap();
    let saved = theme::load(&app.theme_path).unwrap();
    assert!(app.vim_motions && saved.vim_motions);
    assert_eq!(saved.theme, app.theme);
    assert!(!app.dirty);
    app.pick(PickValue::Theme(Theme::CatppuccinLatte)).unwrap();
    assert!(theme::load(&app.theme_path).unwrap().vim_motions);
    app.key(key(Key::Char(':'))).unwrap();
    app.paste("Vim motions");
    app.key(key(Key::Enter)).unwrap();
    assert!(!app.vim_motions);
    assert!(!theme::load(&app.theme_path).unwrap().vim_motions);
    assert_eq!(app.theme, Theme::CatppuccinLatte);
    let obstruction = project.path().join("file");
    std::fs::write(&obstruction, "unchanged").unwrap();
    app.theme_path = obstruction.join("appearance.json");
    assert!(app.key(key(Key::F(8))).is_err());
    assert!(!app.vim_motions);
}

#[test]
fn vim_toggle_sits_below_header_options_on_the_right() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    for width in [65, 80, 120] {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
        for enabled in [false, true] {
            app.vim_motions = enabled;
            for page in 0..4 {
                app.page = page;
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
    app.page = 0;
    app.task = "Keep this draft".into();
    app.key(key(Key::Char('t'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::F(8))).unwrap();
    assert!(!app.vim_motions && app.modal.is_some());
    app.key(key(Key::F(8))).unwrap();
    assert!(app.vim_motions && app.modal.is_some());
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(65, 20)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let display = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(display.contains("VIM INSERT"));
    let field = app.modal.as_mut().unwrap().active_input().unwrap();
    assert_eq!(super::widgets::content(field), "Keep this draft");
    assert!(field.mode_hint().unwrap().contains("INSERT"));
}

#[test]
fn vim_task_editing_routes_escape_motions_paste_and_save() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.task = "first\nsecond".into();
    app.key(key(Key::Char('t'))).unwrap();
    app.key(key(Key::Char('h'))).unwrap();
    assert_eq!(
        super::widgets::content(app.modal.as_mut().unwrap().active_input().unwrap()),
        "first\nsecondh"
    );
    app.key(key(Key::Esc)).unwrap();
    assert!(app.modal.is_none(), "default Esc still cancels immediately");
    app.key(key(Key::F(8))).unwrap();
    app.key(key(Key::Char('t'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(app.modal.is_some());
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
    assert!(app.modal.is_none());
    assert_eq!(app.task, "Prefix first\nsecond");
    app.key(key(Key::Char('t'))).unwrap();
    assert_eq!(
        app.modal.as_mut().unwrap().active_input().unwrap().cursor(),
        (1, 6)
    );
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(app.modal.is_none());
}

#[test]
fn vim_stage_navigation_keeps_choices_and_refocus_behavior() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.key(key(Key::F(8))).unwrap();
    app.key(key(Key::Char('e'))).unwrap();
    let original = super::widgets::content(app.modal.as_mut().unwrap().active_input().unwrap());
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('0'))).unwrap();
    app.key(key(Key::Char('g'))).unwrap();
    app.key(key(Key::Down)).unwrap(); // Role remains a choice, not a Vim input.
    assert!(app.modal.as_mut().unwrap().active_input().is_none());
    app.key(key(Key::Up)).unwrap();
    app.key(key(Key::Char('!'))).unwrap();
    let field = app.modal.as_mut().unwrap().active_input().unwrap();
    assert_eq!(super::widgets::content(field), format!("{original}!"));
    assert!(field.mode_hint().unwrap().contains("INSERT"));
    // Up goes to instructions; j/k move within the multiline input instead.
    app.key(key(Key::Up)).unwrap();
    app.paste("\nlast line");
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('k'))).unwrap();
    assert!(app.modal.as_mut().unwrap().active_input().is_some());
    app.key(key(Key::Down)).unwrap();
    assert_eq!(
        super::widgets::content(app.modal.as_mut().unwrap().active_input().unwrap()),
        format!("{original}!")
    );
}

#[test]
fn vim_motions_reach_harness_path_import_and_filter_inputs() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.vim_motions = true;
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
        app.modal = Some(modal);
        app.paste("abc");
        app.key(key(Key::Esc)).unwrap();
        app.key(key(Key::Char('0'))).unwrap();
        let field = app.modal.as_mut().unwrap().active_input().unwrap();
        assert_eq!(field.cursor(), (0, 0));
        assert_eq!(super::widgets::content(field), "abc");
        app.key(key(Key::Char('A'))).unwrap();
        app.key(key(Key::Char('!'))).unwrap();
        assert_eq!(
            super::widgets::content(app.modal.as_mut().unwrap().active_input().unwrap()),
            "abc!"
        );
    }
    app.modal = None;
    app.page = 2;
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
    app.task = "Keep my task".into();
    let settings = toml::to_string(&app.settings).unwrap();
    app.key(key(Key::Char('T'))).unwrap();
    app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
        .unwrap();
    assert_eq!(app.displayed_theme(), Theme::CatppuccinMocha);
    assert_eq!(app.theme, Theme::ModelRouter);
    assert!(!app.theme_path.exists());
    app.key(KeyEvent::new(Key::Char('p'), Mods::CONTROL))
        .unwrap();
    assert_eq!(app.displayed_theme(), Theme::ModelRouter);
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(app.displayed_theme(), Theme::ModelRouter);
    assert!(!app.theme_path.exists());

    app.key(key(Key::Char('T'))).unwrap();
    for c in "latte".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    assert_eq!(app.displayed_theme(), Theme::CatppuccinLatte);
    app.key(key(Key::Enter)).unwrap();
    assert!(app.modal.is_none());
    assert_eq!(app.theme, Theme::CatppuccinLatte);
    assert_eq!(
        theme::load(&app.theme_path).unwrap().theme,
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
        app.modal.is_some(),
        "An empty search must not choose a theme"
    );
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(app.displayed_theme(), Theme::CatppuccinLatte);
    assert_eq!(
        theme::load(&app.theme_path).unwrap().theme,
        Theme::CatppuccinLatte
    );
    assert!(!app.dirty);
    assert_eq!(app.task, "Keep my task");
    assert_eq!(toml::to_string(&app.settings).unwrap(), settings);
}

#[test]
fn failed_theme_save_preserves_committed_theme_and_allows_cancel() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let obstruction = project.path().join("not-a-directory");
    std::fs::write(&obstruction, "keep this file").unwrap();
    app.theme_path = obstruction.join("appearance.json");
    app.choose_theme();
    for c in "dracula".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    assert!(app.key(key(Key::Enter)).is_err());
    assert!(
        matches!(&app.modal, Some(Modal::Picker { note, .. }) if note.contains("Could not apply selection"))
    );
    assert_eq!(app.theme, Theme::ModelRouter);
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(app.displayed_theme(), Theme::ModelRouter);
    assert_eq!(
        std::fs::read_to_string(obstruction).unwrap(),
        "keep this file"
    );
    assert!(!app.dirty);
}

#[test]
fn every_theme_reaches_pages_dialogs_and_text_inputs() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.task = "Preserve editor content".into();
    for theme in Theme::ALL {
        app.theme = theme;
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
                app.modal = None;
                if step == 14 {
                    app.focused.run = Some(crate::report::tests::sample());
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
            app.focused.run = None;
            app.modal = None;
        }
    }
    assert_eq!(app.task, "Preserve editor content");
    assert!(!app.theme_path.exists());
    assert!(!app.dirty);
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
    let original = toml::to_string(&app.settings).unwrap();
    let preset = presets::STAGES
        .iter()
        .position(|p| p.key == "plan")
        .unwrap();
    app.pick(PickValue::StagePreset(preset)).unwrap();
    assert_eq!(toml::to_string(&app.settings).unwrap(), original);
    app.modal_key(key(Key::Esc)).unwrap();
    assert!(!app.dirty);
    assert_eq!(toml::to_string(&app.settings).unwrap(), original);
    for _ in 0..2 {
        app.pick(PickValue::StagePreset(preset)).unwrap();
        app.modal_key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
            .unwrap();
        assert_eq!(app.stage().role, Role::Planning);
    }
    let stages = &app.settings.workflows[0].stages;
    assert_eq!(stages.len(), 6);
    assert_eq!(stages[0].id, "research");
    assert_eq!(stages[3].id, "implement");
    assert_eq!(stages[5].id, "summary");
    assert_ne!(stages[1].id, stages[2].id);
    app.settings.validate().unwrap();
    // Adding or duplicating at the limit must not leave an invalid workflow.
    app.settings.workflows[0].stages = (0..64).map(|_| Stage::default()).collect();
    assert!(app.key(key(Key::Char('a'))).is_err());
    assert!(app.key(key(Key::Char('d'))).is_err());
    assert_eq!(app.settings.workflows[0].stages.len(), 64);
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
    let original = toml::to_string(&app.settings.workflows[0]).unwrap();
    app.task = "Keep the imported spec".into();
    app.pick(PickValue::WorkflowPreset(2)).unwrap();
    app.pick(PickValue::WorkflowPreset(2)).unwrap();
    assert_eq!(app.settings.workflows.len(), 3);
    assert_eq!(app.settings.workflows[1].name, "Bug fix");
    assert_eq!(app.settings.workflows[2].name, "Bug fix (2)");
    assert_eq!(app.task, "Keep the imported spec");
    assert_eq!(
        toml::to_string(&app.settings.workflows[0]).unwrap(),
        original
    );
    app.settings.validate().unwrap();
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
        app.modal = Some(Modal::Repositories(Box::new(
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
    let original = serde_json::to_value(&app.settings).unwrap();
    app.key(key(Key::Char('c'))).unwrap();
    for _ in 0..5 {
        app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
            .unwrap();
    }
    app.key(key(Key::Right)).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(serde_json::to_value(&app.settings).unwrap(), original);
    assert!(!app.dirty);

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
    assert!(app.modal.is_none());
    assert!(app.dirty);
    assert_eq!(app.stage().harness_settings["sandbox"], "workspace-write");
    assert_eq!(
        app.stage().harness_settings["approval_policy"],
        "on-request"
    );
    for i in 1..3 {
        assert_eq!(
            serde_json::to_value(&app.settings.workflows[0].stages[i]).unwrap(),
            original["workflows"][0]["stages"][i]
        );
    }
    app.key(key(Key::Char('s'))).unwrap();
    assert!(!app.dirty);
    let (saved, _) = config::load(project.path(), Some(&app.config_path)).unwrap();
    assert_eq!(
        saved.workflows[0].stages[0].harness_settings,
        app.stage().harness_settings
    );
    let saved_settings = serde_json::to_value(&app.settings).unwrap();
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
    assert!(matches!(&app.modal, Some(Modal::Harness(e)) if e.error.contains("greater than zero")));
    assert_eq!(serde_json::to_value(&app.settings).unwrap(), saved_settings);
    app.key(key(Key::Esc)).unwrap();

    // Both ways of switching harness discard incompatible structured options.
    let mut editor = StageEditor::new(app.stage().clone(), vec!["codex".into(), "claude".into()]);
    editor.fields[2] = textarea("claude");
    assert!(editor.finish().unwrap().harness_settings.is_empty());
    app.pick(PickValue::Harness("claude".into())).unwrap();
    assert!(app.stage().harness_settings.is_empty());
}

#[test]
fn stage_editor_model_field_picks_from_catalog_and_keeps_draft() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let original = serde_json::to_value(&app.settings).unwrap();
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
        matches!(&app.modal, Some(Modal::Stage { editor, .. }) if !editor.finish().unwrap().model.contains('x'))
    );
    app.key(key(Key::Enter)).unwrap();
    assert!(app.stage_editor.is_some());
    assert!(matches!(app.modal, Some(Modal::Loading(_))));
    app.key(key(Key::Esc)).unwrap();
    assert!(app.stage_editor.is_none());
    assert!(
        matches!(&app.modal, Some(Modal::Stage { editor, .. }) if editor.finish().unwrap().name.contains("draft"))
    );

    // A failed catalog returns to the editor with the error shown.
    app.key(key(Key::Enter)).unwrap();
    let (sender, receiver) = mpsc::channel();
    sender.send(Err("Offline catalog".into())).unwrap();
    app.catalog = Some(receiver);
    app.poll().unwrap();
    assert!(matches!(&app.modal, Some(Modal::Stage { .. })));
    assert!(app.alert.contains("Offline catalog"));

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
    assert!(matches!(&app.modal, Some(Modal::Picker { title, .. }) if title.starts_with("codex ")));
    app.key(key(Key::Enter)).unwrap();
    let draft = match &app.modal {
        Some(Modal::Stage { editor, .. }) => editor.finish().unwrap(),
        other => panic!("expected stage editor, got {}", other.is_some()),
    };
    assert_eq!(draft.model, "picked-model");
    assert_eq!(draft.provider, "prov");
    assert_eq!(serde_json::to_value(&app.settings).unwrap(), original);

    // Backspace returns to the harness default; i allows a custom ID.
    app.key(key(Key::Backspace)).unwrap();
    app.key(key(Key::Char('i'))).unwrap();
    for c in "custom-id".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
        .unwrap();
    assert!(app.modal.is_none());
    assert_eq!(app.stage().model, "custom-id");
    assert_eq!(app.stage().provider, "prov");
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
    let original = serde_json::to_value(&app.settings).unwrap();
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
        matches!(&app.modal, Some(Modal::Harness(e)) if e.finish().unwrap().model == "draft-model")
    );

    app.key(key(Key::F(7))).unwrap();
    let (sender, receiver) = mpsc::channel();
    sender.send(Err("Offline catalog".into())).unwrap();
    app.catalog = Some(receiver);
    app.poll().unwrap();
    assert!(
        matches!(&app.modal, Some(Modal::Harness(e)) if e.error.contains("Offline catalog") && e.finish().unwrap().model == "draft-model")
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
        matches!(&app.modal, Some(Modal::Harness(e)) if e.finish().unwrap().model == "selected-model")
    );
    assert_eq!(serde_json::to_value(&app.settings).unwrap(), original);
    app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
        .unwrap();
    assert_eq!(app.stage().model, "selected-model");
    assert_eq!(app.stage().provider, "provider");
}

/// Choose `id` from the command menu, as a user would.
fn choose(app: &mut App, id: ActionId) -> bool {
    app.key(key(Key::Char(':'))).unwrap();
    let Some(Modal::Picker { items, .. }) = &app.modal else {
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
    let Some(Modal::Picker { items, .. }) = app.modal.take() else {
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
    app.modal.as_ref().map(|modal| match modal {
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
            serde_json::to_value(&chosen.settings).unwrap(),
            serde_json::to_value(&pressed.settings).unwrap(),
            "{id:?}"
        );
        assert_eq!(
            (chosen.page, chosen.dirty, &chosen.message, dialog(&chosen)),
            (
                pressed.page,
                pressed.dirty,
                &pressed.message,
                dialog(&pressed)
            ),
            "{id:?}"
        );
    }
    // Errors surface as a key's do: the menu closes and the caller reports them.
    let mut app = theme_app(project.path());
    app.settings.workflows[0].stages.truncate(1);
    let pressed = app.key(key(Key::Char('x'))).unwrap_err().to_string();
    app.key(key(Key::Char(':'))).unwrap();
    for c in "remove".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    assert_eq!(app.key(key(Key::Enter)).unwrap_err().to_string(), pressed);
    assert!(app.modal.is_none());
}

#[test]
fn command_menu_lists_only_actions_for_the_current_context() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let workflow = menu(&mut app);
    assert!(workflow.contains(&ActionId::Run) && workflow.contains(&ActionId::Commands));
    assert!(!workflow.contains(&ActionId::CancelStage) && !workflow.contains(&ActionId::OpenRun));

    app.page = 1;
    let idle = menu(&mut app);
    assert!(idle.contains(&ActionId::Continue) && idle.contains(&ActionId::Handoff));
    assert!(!idle.contains(&ActionId::CancelStage) && !idle.contains(&ActionId::Run));

    app.focused.worker = Some(Worker::stub());
    let running = menu(&mut app);
    assert!(running.contains(&ActionId::CancelStage) && running.contains(&ActionId::Handoff));
    assert!(!running.contains(&ActionId::Continue) && !running.contains(&ActionId::Advance));

    // Runs snapshot their workflow, so editing and starting another continue.
    app.page = 0;
    let editing = menu(&mut app);
    assert!(editing.contains(&ActionId::Run) && editing.contains(&ActionId::EditTask));
    assert!(!editing.contains(&ActionId::CancelStage));

    // A menu opened during a run does nothing once the run has stopped.
    app.page = 1;
    app.key(key(Key::Char(':'))).unwrap();
    app.focused.worker = None;
    for c in "cancel stage".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(key(Key::Enter)).unwrap();
    assert!(app.modal.is_none() && !app.focused.cancel.load(Ordering::Relaxed));

    app.page = 2;
    assert!(menu(&mut app).contains(&ActionId::OpenRun));
    app.page = 3;
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
    let harness = app.stage().harness.clone();
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
    app.stage_mut().effort = "custom".into();
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
    assert_eq!(app.stage().effort, "high");

    // Harness editor: a listed model narrows the levels to its own.
    app.stage_mut().model = "listed-model".into();
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
    assert_eq!(app.stage().effort, "low");
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(screen(&terminal).contains("listed by fixture cache"));

    // Switching harness in the stage editor clears the levels and asks again.
    app.key(key(Key::Char('e'))).unwrap();
    app.key(key(Key::Tab)).unwrap();
    app.key(key(Key::Tab)).unwrap();
    app.key(key(Key::Right)).unwrap();
    let changed = match &app.modal {
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
    app.slots()
        .map(|(slot, focused)| {
            let id = slot.run.as_ref().unwrap().id.clone();
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
        app.adopt(sample_run(id));
    }
    assert_eq!(session(&app), "a b c*");
    app.cycle_focus(true);
    assert_eq!(session(&app), "a* b c");
    app.cycle_focus(false);
    assert_eq!(session(&app), "a b c*");
    assert!(app.focus_run("b") && !app.focus_run("missing"));
    assert_eq!(session(&app), "a b* c");

    // An active run cannot be closed; closing focuses its neighbour.
    app.focused.worker = Some(Worker::stub());
    assert!(app.dismiss().is_err());
    app.focused.worker = None;
    app.dismiss().unwrap();
    assert_eq!(session(&app), "a c*");
    app.adopt(sample_run("d"));
    assert_eq!(session(&app), "a c d*");
    for _ in 0..3 {
        app.dismiss().unwrap();
    }
    assert_eq!(session(&app), "");
    assert!(app.focused.run.is_none() && app.parked.is_empty());
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
    app.adopt(busy);
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
    app.focused.run.as_mut().unwrap().status = Status::Complete;
    assert_eq!(app.placement(false).unwrap(), (Placement::Project, None));
}
