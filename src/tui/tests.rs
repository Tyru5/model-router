use super::{
    actions::{self, ActionId, Scope},
    app::{App, Worker, draw},
    modal::{Modal, PickValue},
    path_picker::PathPicker,
    theme::{self, Theme},
    widgets::textarea,
    workflow_page::StageEditor,
};
use crate::{
    catalog::{self, Catalog, Model},
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
    app
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
        theme::load(&app.theme_path).unwrap(),
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
        theme::load(&app.theme_path).unwrap(),
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
            for action in [
                '1', '2', '3', '4', '1', 't', 'e', 'c', 'p', 'f', 'w', '?', 'T', ':',
            ] {
                app.modal = None;
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
    assert_eq!(stages.len(), 5);
    assert_eq!(stages[0].id, "research");
    assert_eq!(stages[3].id, "implement");
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
            }],
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

    app.worker = Some(Worker::stub());
    let running = menu(&mut app);
    assert!(running.contains(&ActionId::CancelStage) && running.contains(&ActionId::Handoff));
    assert!(!running.contains(&ActionId::Continue) && !running.contains(&ActionId::Advance));

    // Workflow edits wait for the run to stop, so only global actions remain.
    app.page = 0;
    let locked = menu(&mut app);
    assert!(locked.contains(&ActionId::Quit));
    assert!(
        locked
            .iter()
            .all(|id| actions::entry(*id).unwrap().scope == Scope::Global)
    );

    // A menu opened during a run does nothing once the run has stopped.
    app.page = 1;
    app.key(key(Key::Char(':'))).unwrap();
    app.worker = None;
    for c in "cancel stage".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(key(Key::Enter)).unwrap();
    assert!(app.modal.is_none() && !app.cancel.load(Ordering::Relaxed));

    app.page = 2;
    assert!(menu(&mut app).contains(&ActionId::OpenRun));
    app.page = 3;
    assert!(menu(&mut app).contains(&ActionId::HarnessDetails));
}

#[test]
fn harness_rows_are_checked_when_the_page_opens_not_every_frame() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let harnesses = catalog::known_harnesses(&app.settings);
    app.key(key(Key::Char('4'))).unwrap();
    assert_eq!(app.harness_rows.len(), harnesses.len());
    app.harness_rows.truncate(1);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains(&harnesses[0]) && !text.contains(&harnesses[1]));
    app.key(key(Key::Char('r'))).unwrap();
    assert_eq!(app.harness_rows.len(), harnesses.len());
}
