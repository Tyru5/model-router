use super::{
    actions::ActionId,
    app::{App, Page, draw},
    modal::{Ask, Loading, Modal, PickValue},
    path_picker::PathPicker,
    run_slot::Worker,
    tasks::Msg,
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
    app.tour.path = project.join("tour.json");
    app.harnesses.checks.probe = |_, _| vec![];
    app
}

#[test]
fn the_new_run_view_strips_stages_and_edits_the_task_in_place() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let names: Vec<String> = app.editor.settings.workflows[0]
        .stages
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert!(names.len() >= 3);
    let screen = rows(&mut app, 80, 24).join("\n");
    assert!(
        screen.contains(&format!("stage 1 of {}", names.len())),
        "{screen}"
    );
    // At the minimum size the stage keys give way to the label.
    let small = rows(&mut app, 65, 20);
    let label = small.iter().find(|row| row.contains("Stages")).unwrap();
    assert!(label.contains("Stages  "), "{label}");
    assert!(
        screen.contains("Instructions"),
        "details show at 80 columns\n{screen}"
    );
    // → selects the next stage; its neighbours keep their names in the strip.
    app.key(key(Key::Right)).unwrap();
    assert_eq!(app.editor.selected, 1);
    let screen = rows(&mut app, 80, 24).join("\n");
    assert!(
        screen.contains(&format!("stage 2 of {}", names.len())),
        "{screen}"
    );
    for name in &names[..3] {
        assert!(screen.contains(name.as_str()), "lacks {name}\n{screen}");
    }
    app.key(key(Key::Left)).unwrap();
    assert_eq!(app.editor.selected, 0);
    // t edits the task in its own panel, with the page still in view.
    app.key(key(Key::Char('t'))).unwrap();
    app.paste("Fix the cache 雪");
    let screen = rows(&mut app, 80, 24).join("\n");
    assert!(screen.contains("Fix the cache 雪"), "{screen}");
    assert!(screen.contains("[Ctrl+S] done"), "{screen}");
    assert!(screen.contains("Stages"), "{screen}");
    // Keys type text now, so the footer names the editor's keys.
    let footer = rows(&mut app, 80, 24).pop().unwrap();
    assert!(footer.contains("[Enter] new line"), "{footer}");
    assert!(
        !footer.contains("[r] run") && !footer.contains("menu"),
        "{footer}"
    );
    app.key(ctrl('s')).unwrap();
    assert!(app.ui.dialog().is_none());
    assert_eq!(app.editor.task, "Fix the cache 雪");
    assert!(rows(&mut app, 80, 24).join("\n").contains("[t] edit"));
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
                assert!(header.contains("3 Harnesses"), "{header}");
            }
        }
    }
    app.ui.page = Page::Workflow;
    app.editor.task = "Keep this draft".into();
    app.key(key(Key::Char('t'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::F(8))).unwrap();
    assert!(!app.ui.vim_motions && app.ui.dialog().is_some());
    app.key(key(Key::F(8))).unwrap();
    assert!(app.ui.vim_motions && app.ui.dialog().is_some());
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(65, 20)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let display = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(display.contains("VIM INSERT"));
    let field = app.ui.dialog_mut().unwrap().active_input().unwrap();
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
        super::widgets::content(app.ui.dialog_mut().unwrap().active_input().unwrap()),
        "first\nsecondh"
    );
    app.key(key(Key::Esc)).unwrap();
    assert!(
        app.ui.dialog().is_none(),
        "default Esc still cancels immediately"
    );
    app.key(key(Key::F(8))).unwrap();
    app.key(key(Key::Char('t'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.dialog().is_some());
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
    assert!(app.ui.dialog().is_none());
    assert_eq!(app.editor.task, "Prefix first\nsecond");
    app.key(key(Key::Char('t'))).unwrap();
    assert_eq!(
        app.ui
            .dialog_mut()
            .unwrap()
            .active_input()
            .unwrap()
            .cursor(),
        (1, 6)
    );
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.dialog().is_none());
}

#[test]
fn vim_stage_navigation_keeps_choices_and_refocus_behavior() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.key(key(Key::F(8))).unwrap();
    app.key(key(Key::Char('e'))).unwrap();
    let original = super::widgets::content(app.ui.dialog_mut().unwrap().active_input().unwrap());
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('0'))).unwrap();
    app.key(key(Key::Char('g'))).unwrap();
    app.key(key(Key::Down)).unwrap(); // Role remains a choice, not a Vim input.
    assert!(app.ui.dialog_mut().unwrap().active_input().is_none());
    app.key(key(Key::Up)).unwrap();
    app.key(key(Key::Char('!'))).unwrap();
    let field = app.ui.dialog_mut().unwrap().active_input().unwrap();
    assert_eq!(super::widgets::content(field), format!("{original}!"));
    assert!(field.mode_hint().unwrap().contains("INSERT"));
    // Up goes to instructions; j/k move within the multiline input instead.
    app.key(key(Key::Up)).unwrap();
    app.paste("\nlast line");
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('k'))).unwrap();
    assert!(app.ui.dialog_mut().unwrap().active_input().is_some());
    // Down returns to the last line first; only there does it leave the field.
    app.key(key(Key::Down)).unwrap();
    assert_eq!(
        app.ui
            .dialog_mut()
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
        super::widgets::content(app.ui.dialog_mut().unwrap().active_input().unwrap()),
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
        Modal::Repositories(Box::new(
            PathPicker::new(project.path(), vec![], app.tasks.spawner()).unwrap(),
        )),
        Modal::Import(Box::new(
            super::task_importer::TaskImporter::new(project.path(), "", app.tasks.spawner())
                .unwrap(),
        )),
        Modal::Picker {
            title: "Picker".into(),
            items: vec![],
            selected: 0,
            filter: textarea(""),
            note: String::new(),
        },
    ] {
        app.ui.open(modal);
        app.paste("abc");
        app.key(key(Key::Esc)).unwrap();
        app.key(key(Key::Char('0'))).unwrap();
        let field = app.ui.dialog_mut().unwrap().active_input().unwrap();
        assert_eq!(field.cursor(), (0, 0));
        assert_eq!(super::widgets::content(field), "abc");
        app.key(key(Key::Char('A'))).unwrap();
        app.key(key(Key::Char('!'))).unwrap();
        assert_eq!(
            super::widgets::content(app.ui.dialog_mut().unwrap().active_input().unwrap()),
            "abc!"
        );
    }
    app.ui.dialogs.clear();
    app.ui.page = Page::Runs;
    app.key(key(Key::Char('F'))).unwrap();
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
    assert!(app.ui.dialog().is_none());
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
        app.ui.dialog().is_some(),
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
        matches!(app.ui.dialog(), Some(Modal::Picker { note, .. }) if note.contains("Could not apply selection"))
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
            // Each page, the Space menu, then `2 o o` revisits Runs with a
            // finished run: its brief, the output, then the brief again.
            for (step, action) in [
                '1', '2', '3', '1', ' ', 't', 'e', 'c', 'p', 'f', 'w', '?', 'T', ':', '2', 'o', 'o',
            ]
            .into_iter()
            .enumerate()
            {
                app.ui.dialogs.clear();
                if step == 14 {
                    app.session.adopt(crate::report::tests::sample());
                }
                app.key(key(Key::Char(action))).unwrap();
                assert!(action != ' ' || app.ui.dialog().is_some(), "no Space menu");
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
            app.ui.dialogs.clear();
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
        app.ui.open(Modal::Repositories(Box::new(
            PathPicker::new(&app.project, vec![], app.tasks.spawner()).unwrap(),
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
    assert!(matches!(app.ui.dialog(), Some(Modal::Ask(_))));
    app.key(key(Key::Char('n'))).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Harness(_))));
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.dialog().is_none());
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
    assert!(app.ui.dialog().is_none());
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
        matches!(app.ui.dialog(), Some(Modal::Harness(e)) if e.error.contains("greater than zero"))
    );
    assert_eq!(
        serde_json::to_value(&app.editor.settings).unwrap(),
        saved_settings
    );
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.dialog().is_none());

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
        matches!(app.ui.dialog(), Some(Modal::Stage { editor, .. }) if !editor.finish().unwrap().model.contains('x'))
    );
    app.key(key(Key::Enter)).unwrap();
    // The picker opens above the editor, which keeps the draft.
    assert!(matches!(
        app.ui.dialogs.as_slice(),
        [Modal::Stage { .. }, Modal::Loading(_)]
    ));
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(app.ui.dialogs.len(), 1);
    assert!(
        matches!(app.ui.dialog(), Some(Modal::Stage { editor, .. }) if editor.finish().unwrap().name.contains("draft"))
    );

    // A failed catalog returns to the editor with the error shown.
    app.key(key(Key::Enter)).unwrap();
    let (sender, receiver) = mpsc::channel();
    sender.send(Err("Offline catalog".into())).unwrap();
    deliver(&mut app, receiver);
    app.poll().unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Stage { .. })));
    assert!(app.ui.alert.contains("Offline catalog"));

    // A pick fills model and provider in the draft, not the saved stage.
    // The editor's own background read of the real catalog may have landed
    // in the cache; drop it so the picker waits for the fixture below.
    settle_catalog_reads(&mut app);
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
    deliver(&mut app, receiver);
    app.poll().unwrap();
    assert!(
        matches!(app.ui.dialog(), Some(Modal::Picker { title, .. }) if title.starts_with("codex "))
    );
    app.key(key(Key::Enter)).unwrap();
    let draft = match app.ui.dialog() {
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
    assert!(app.ui.dialog().is_none());
    assert_eq!(app.editor.stage().model, "custom-id");
    assert_eq!(app.editor.stage().provider, "prov");
}

#[test]
fn repository_line_follows_the_project() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    assert!(app.repo.info.is_none());
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
    let info = app.repo.info.clone().unwrap();
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
        row.contains("3 Harnesses") && row.contains("⎇ repo"),
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
    assert!(matches!(
        app.ui.dialogs.as_slice(),
        [Modal::Harness(_), Modal::Loading(_)]
    ));
    app.key(key(Key::Esc)).unwrap();
    assert!(
        matches!(app.ui.dialog(), Some(Modal::Harness(e)) if e.finish().unwrap().model == "draft-model")
    );

    app.key(key(Key::F(7))).unwrap();
    let (sender, receiver) = mpsc::channel();
    sender.send(Err("Offline catalog".into())).unwrap();
    deliver(&mut app, receiver);
    app.poll().unwrap();
    assert!(
        matches!(app.ui.dialog(), Some(Modal::Harness(e)) if e.error.contains("Offline catalog") && e.finish().unwrap().model == "draft-model")
    );

    settle_catalog_reads(&mut app);
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
    deliver(&mut app, receiver);
    app.poll().unwrap();
    app.key(key(Key::Enter)).unwrap();
    assert!(
        matches!(app.ui.dialog(), Some(Modal::Harness(e)) if e.finish().unwrap().model == "selected-model")
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
    let Some(Modal::Picker { items, .. }) = app.ui.dialog() else {
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
    let Some(Modal::Picker { items, .. }) = app.ui.dialogs.pop() else {
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

/// Hands the picker waiting on top the catalog `receiver` holds, as its read would.
fn deliver(app: &mut App, receiver: mpsc::Receiver<Result<Catalog, String>>) {
    let msg = Msg::Catalog {
        harness: app.editor.stage().harness.clone(),
        ticket: Some(waiting(app)),
        result: receiver.recv().unwrap(),
    };
    app.update(app.tasks.generation(), msg);
}

/// Waits for the editors' background catalog reads, then forgets what they
/// cached, so a test's picker reads its fixture instead of the real catalog.
fn settle_catalog_reads(app: &mut App) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while app.catalogs.reading.is_some() || !app.catalogs.queue.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "catalog reads did not finish"
        );
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    app.catalogs.cached.clear();
}

/// The ticket of the Loading dialog on top.
fn waiting(app: &App) -> u64 {
    match app.ui.dialog() {
        Some(Modal::Loading(loading)) => loading.ticket,
        _ => panic!("nothing is loading"),
    }
}

fn dialog(app: &App) -> Option<&str> {
    app.ui.dialog().map(|modal| match modal {
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
        (ActionId::ToggleStage, Key::Char('o')),
        (ActionId::NextView, Key::Tab),
        (ActionId::ShowRuns, Key::Char('2')),
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
    assert!(app.ui.dialog().is_none());
}

#[test]
fn space_menu_runs_actions_like_their_keys() {
    let project = tempfile::tempdir().unwrap();
    let mut pressed = theme_app(project.path());
    pressed.key(key(Key::Char('o'))).unwrap();
    let mut chosen = theme_app(project.path());
    chosen.key(key(Key::Char(' '))).unwrap();
    let sheet = rows(&mut chosen, 80, 24).join("\n");
    assert!(
        sheet.contains("Menu") && sheet.contains("s Stage"),
        "{sheet}"
    );
    // A letter outside the group does nothing; Backspace returns to groups.
    chosen.key(key(Key::Char('s'))).unwrap();
    chosen.key(key(Key::Char('Z'))).unwrap();
    assert!(
        rows(&mut chosen, 80, 24)
            .join("\n")
            .contains("Menu · Stage")
    );
    chosen.key(key(Key::Backspace)).unwrap();
    chosen.key(key(Key::Char('s'))).unwrap();
    chosen.key(key(Key::Char('o'))).unwrap();
    assert!(chosen.ui.dialog().is_none());
    assert!(chosen.editor.dirty);
    assert_eq!(
        serde_json::to_value(&chosen.editor.settings).unwrap(),
        serde_json::to_value(&pressed.editor.settings).unwrap()
    );
    // Space again or Esc closes without acting.
    for close in [Key::Char(' '), Key::Esc] {
        let mut app = theme_app(project.path());
        app.key(key(Key::Char(' '))).unwrap();
        app.key(key(close)).unwrap();
        assert!(app.ui.dialog().is_none() && !app.editor.dirty);
    }
}

#[test]
fn footer_is_one_row_with_menu_and_guide_keys_on_the_right() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    for width in [80, 120] {
        let screen = rows(&mut app, width, 24);
        let last = screen.last().unwrap();
        assert!(
            last.trim_end().ends_with("[Space] menu   [?] keys"),
            "{last}"
        );
        assert!(last.contains("[r] run"), "{last}");
    }
}

#[test]
fn command_menu_lists_only_actions_for_the_current_context() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let workflow = menu(&mut app);
    assert!(workflow.contains(&ActionId::Run) && workflow.contains(&ActionId::Commands));
    assert!(!workflow.contains(&ActionId::CancelStage) && !workflow.contains(&ActionId::OpenRun));

    // Runs without a run of this session: only the sidebar's actions.
    app.ui.page = Page::Runs;
    let saved = menu(&mut app);
    assert!(saved.contains(&ActionId::OpenRun) && saved.contains(&ActionId::FilterRuns));
    assert!(!saved.contains(&ActionId::Continue) && !saved.contains(&ActionId::Handoff));

    // A selected run of this session adds its actions to the sidebar's.
    app.session.adopt(sample_run("a"));
    let idle = menu(&mut app);
    assert!(idle.contains(&ActionId::Continue) && idle.contains(&ActionId::Handoff));
    assert!(idle.contains(&ActionId::Rerun) && idle.contains(&ActionId::NextRun));
    assert!(!idle.contains(&ActionId::CancelStage) && !idle.contains(&ActionId::Run));

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
    app.ui.page = Page::Runs;
    app.key(key(Key::Char(':'))).unwrap();
    app.slot_mut().worker = None;
    for c in "cancel stage".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(key(Key::Enter)).unwrap();
    assert!(app.ui.dialog().is_none() && !app.slot().cancel.load(Ordering::Relaxed));

    // Opening a saved run is listed only when one is selected, not beside
    // a run of this session, where it could only say it is already open.
    app.ui.page = Page::Runs;
    assert!(!menu(&mut app).contains(&ActionId::OpenRun));
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
    app.catalogs.cached.insert(
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
        text.contains("Levels from fixture help for any model"),
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
    let changed = match app.ui.dialog() {
        Some(Modal::Stage { editor, .. }) => editor.harness(),
        _ => panic!("stage editor expected"),
    };
    assert_ne!(changed, harness);
    app.catalogs
        .cached
        .insert(changed.clone(), Catalog::default());
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
    // The sidebar moves focus; with no saved runs it stops at the last.
    app.ui.page = Page::Runs;
    app.key(key(Key::Up)).unwrap();
    app.key(key(Key::Char('k'))).unwrap();
    assert_eq!(session(&app), "a* b c");
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Char('j'))).unwrap();
    assert_eq!(session(&app), "a b c*");
    assert!(!app.on_saved());
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
    app.repo.info = crate::storage::repo_info(project.path());
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
    run.project = app.project.clone();
    app.session.adopt(run);
    app.slot_mut().worker = Some(Worker::stub());
    app.ui.page = Page::Runs;
    app
}

#[test]
fn cancel_has_its_own_key_and_asks_and_c_never_continues_a_stage_that_just_stopped() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    let cancelled = |app: &App| app.slot().cancel.load(Ordering::Relaxed);
    // `c` continues an idle run; it does nothing to a running one.
    app.key(key(Key::Char('c'))).unwrap();
    assert!(!cancelled(&app) && app.ui.dialog().is_none());
    app.key(shift('C')).unwrap();
    assert!(
        matches!(app.ui.dialog(), Some(Modal::Ask(ask)) if matches!(**ask, Ask::CancelStage { .. }))
    );
    assert!(!cancelled(&app), "Cancelling waits for confirmation");
    assert!(rows(&mut app, 65, 20).join("\n").contains("Cancel stage"));
    app.key(key(Key::Char('n'))).unwrap();
    assert!(app.ui.dialog().is_none() && !cancelled(&app));

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
    assert!(app.ui.dialog().is_none() && !cancelled(&app));
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
        matches!(app.ui.dialog(), Some(Modal::Ask(ask)) if matches!(**ask, Ask::TakeOver { .. }))
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
    assert!(app.ui.dialog().is_none() && !app.starting && !app.editor.dirty);
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
    assert!(matches!(app.ui.dialog(), Some(Modal::Ask(_))));
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
        app.key(key(Key::Char('o'))).unwrap();
        assert!(app.editor.dirty);
        assert!(!app.key(exit).unwrap());
        assert!(
            matches!(app.ui.dialog(), Some(Modal::Ask(ask)) if matches!(**ask, Ask::SaveAndExit))
        );
        assert!(rows(&mut app, 65, 20).join("\n").contains("Save changes?"));
        // Esc stays, with the edits.
        app.key(key(Key::Esc)).unwrap();
        assert!(app.ui.dialog().is_none() && app.editor.dirty && !saved(&app));
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
    app.catalogs.reading = Some("codex".into());
    app.catalogs.queue.push_back("claude".into());
    app.catalogs.cached.insert("pi".into(), Catalog::default());
    let old = app.tasks.generation();
    app.switch_project(other.path().into()).unwrap();
    assert_ne!(app.tasks.generation(), old);
    assert!(
        app.catalogs.cached.is_empty()
            && app.catalogs.reading.is_none()
            && app.catalogs.queue.is_empty()
    );
    // Results tagged with the old project are dropped when they arrive.
    app.update(
        old,
        Msg::Catalog {
            harness: "codex".into(),
            ticket: None,
            result: Ok(Catalog::default()),
        },
    );
    app.runs.loading = true;
    app.update(
        old,
        Msg::Runs(Ok(crate::history::list(project.path()).unwrap())),
    );
    assert!(!app.catalogs.cached.contains_key("codex"));
    assert!(
        app.runs.loading,
        "a stale result leaves the current read pending"
    );

    // From the browser, the project opens in the background; a bad path
    // returns to the browser with the error.
    let picker = Box::new(PathPicker::new(project.path(), vec![], app.tasks.spawner()).unwrap());
    app.ui.open(Modal::Repositories(picker));
    app.begin_switch(project.path().join("missing"));
    assert!(matches!(app.ui.dialog(), Some(Modal::Loading(_))));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(app.ui.dialog(), Some(Modal::Loading(_))) && std::time::Instant::now() < deadline
    {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        matches!(app.ui.dialog(), Some(Modal::Repositories(p)) if p.error.contains("not found"))
    );
    let picker = Box::new(PathPicker::new(project.path(), vec![], app.tasks.spawner()).unwrap());
    app.ui.dialogs.clear();
    app.ui.open(Modal::Repositories(picker));
    app.begin_switch(project.path().into());
    while app.ui.dialog().is_some() && std::time::Instant::now() < deadline {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(app.project, std::fs::canonicalize(project.path()).unwrap());
}

#[test]
fn esc_while_opening_a_project_returns_to_the_browser_and_drops_the_result() {
    let project = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let picker = Box::new(PathPicker::new(project.path(), vec![], app.tasks.spawner()).unwrap());
    app.ui.open(Modal::Repositories(picker));
    app.begin_switch(other.path().into());
    assert!(matches!(
        app.ui.dialogs.as_slice(),
        [Modal::Repositories(_), Modal::Loading(_)]
    ));
    app.key(key(Key::Esc)).unwrap();
    assert!(matches!(
        app.ui.dialogs.as_slice(),
        [Modal::Repositories(_)]
    ));
    // The read finishes after the wait was closed: nothing changes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.tasks.busy() && std::time::Instant::now() < deadline {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(!app.tasks.busy());
    assert_eq!(app.project, project.path());
    assert!(matches!(
        app.ui.dialogs.as_slice(),
        [Modal::Repositories(_)]
    ));
}

#[test]
fn shift_y_retries_one_failed_fan_out_candidate_of_the_stopped_stage() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.ui.page = Page::Runs;
    let mut run = crate::report::tests::sample();
    // No harness here exists, so the restarted driver cannot launch one.
    for stage in &mut run.workflow.stages {
        stage.harness = "no-such-harness".into();
    }
    run.workflow.stages[0].fan_out = Some(crate::config::FanOut {
        candidates: ["c1", "c2"]
            .map(|id| crate::config::Candidate {
                id: id.into(),
                harness: "no-such-harness".into(),
                ..Default::default()
            })
            .into(),
        judge: crate::config::AUTO_JUDGE.into(),
    });
    run.stages[0].fan_out = Some(
        serde_json::from_value(serde_json::json!({
            "base": "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            "base_commit": "0123456789abcdef0123456789abcdef01234567",
            "candidates": [
                {"id": "c1", "harness": "no-such-harness", "model": "", "status": "complete",
                 "attempts": [], "handoff": null},
                {"id": "c2", "harness": "no-such-harness", "model": "m2", "status": "failed",
                 "attempts": [], "handoff": null, "error": "Harness exited"}
            ]
        }))
        .unwrap(),
    );
    run.stages[0].status = Status::Failed;
    run.status = Status::Failed;
    run.cursor = 0;
    run.project = project.path().into();
    run.workspace = project.path().into();
    run.directory = crate::storage::run_dir(project.path(), &run.id).unwrap();
    // Saved runs keep attempt paths in their own directory.
    for (n, attempt) in run
        .stages
        .iter_mut()
        .flat_map(|s| &mut s.attempts)
        .enumerate()
    {
        attempt.directory = run
            .directory
            .join("01-implement")
            .join(format!("attempt-{n:03}"));
        attempt.handoff_file = attempt.directory.join("handoff.json");
        for (c, check) in attempt.checks.iter_mut().enumerate() {
            check.directory = attempt
                .directory
                .join("checks")
                .join(format!("{:02}", c + 1));
        }
    }
    std::fs::create_dir_all(&run.directory).unwrap();
    run.persist().unwrap();
    app.session.adopt(run.clone());

    app.key(shift('Y')).unwrap();
    let Some(Modal::Picker { items, title, .. }) = app.ui.dialog() else {
        panic!("no candidate picker");
    };
    assert_eq!(title, "Retry a fan-out candidate");
    let labels: Vec<_> = items.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["Implement · c2 · no-such-harness m2 · failed"]);
    app.key(key(Key::Enter)).unwrap();
    assert!(app.ui.dialog().is_none(), "{}", app.ui.message);
    assert!(
        app.ui.message.starts_with("Retrying candidate c2"),
        "{}",
        app.ui.message
    );
    let saved = crate::engine::load(project.path(), &run.id).unwrap();
    let fan = saved.stages[0].fan_out.as_ref().unwrap();
    assert_eq!(
        fan.candidates[0].status,
        Status::Complete,
        "c1 keeps its result"
    );
    assert_eq!(fan.candidates[1].status, Status::Pending);
    // Let the driver stop.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while app.session.running() && std::time::Instant::now() < deadline {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!app.session.running());

    // Nothing to retry: the stage itself is not stopped, or no candidate is.
    let mut app = theme_app(project.path());
    app.ui.page = Page::Runs;
    app.session.adopt(crate::report::tests::sample());
    let error = app.perform(ActionId::RetryCandidate).unwrap_err();
    assert!(
        format!("{error}").contains("No failed fan-out candidate"),
        "{error}"
    );
    assert!(app.ui.dialog().is_none());
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
    assert!(matches!(app.ui.dialog(), Some(Modal::Text { .. })));
    app.ui.dialogs.clear();
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
    app.ui.open(Modal::Loading(Loading::new("Reading")));
    assert!(!app.take_launch());
    assert!(app.ui.message.contains("press [t]"), "{}", app.ui.message);
    app.ui.dialogs.clear();
    app.session.launch_interactive = true;
    assert!(app.take_launch());
    assert!(!app.take_launch());
}

#[test]
fn interactive_worker_failure_before_launch_finishes_its_terminal() {
    let session = crate::process::InteractiveSession::new(80, 24);
    let mut slot = super::run_slot::RunSlot::new(sample_run("refused"));
    slot.worker = Some(Worker::spawn(
        Some(session.clone()),
        |_| false,
        |_| (sample_run("refused"), Err("Replay approval refused".into())),
    ));
    let start = std::time::Instant::now();
    loop {
        if let Some(result) = slot.poll().1 {
            assert_eq!(result, Err("Replay approval refused".into()));
            break;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // A finished session returns before touching the host terminal, rather
    // than attaching the person to a blank session with no running harness.
    assert_eq!(
        session
            .attach(&AtomicBool::new(false), &AtomicBool::new(false))
            .unwrap(),
        crate::process::AttachOutcome::Finished
    );
}

#[test]
fn a_panicking_worker_fails_the_run_and_finishes_its_terminal() {
    let session = crate::process::InteractiveSession::new(80, 24);
    let mut slot = super::run_slot::RunSlot::new(sample_run("crashed"));
    slot.run.status = Status::Running;
    slot.worker = Some(Worker::spawn(
        Some(session.clone()),
        |_| false,
        |emit| {
            emit(RunEvent::Output("before the panic".into()));
            panic!("engine bug {}", 7)
        },
    ));
    let start = std::time::Instant::now();
    let result = loop {
        if let Some(result) = slot.poll().1 {
            break result;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let error = result.unwrap_err();
    assert!(error.contains("panicked: engine bug 7"), "{error}");
    assert!(slot.worker.is_none());
    assert!(!slot.in_progress());
    assert_eq!(slot.run.status, Status::Failed);
    assert_eq!(slot.run.error.as_deref(), Some(error.as_str()));
    assert!(slot.logs.iter().any(|line| line == "before the panic"));
    assert_eq!(
        session
            .attach(&AtomicBool::new(false), &AtomicBool::new(false))
            .unwrap(),
        crate::process::AttachOutcome::Finished
    );
}

#[test]
fn a_worker_that_panics_after_saving_unreported_state_leaves_the_saved_revision() {
    let project = tempfile::tempdir().unwrap();
    let mut run = sample_run("saved-then-crashed");
    run.project = project.path().into();
    run.workspace = project.path().into();
    run.directory = crate::storage::run_dir(project.path(), &run.id).unwrap();
    for (n, attempt) in run
        .stages
        .iter_mut()
        .flat_map(|s| &mut s.attempts)
        .enumerate()
    {
        attempt.directory = run
            .directory
            .join("01-implement")
            .join(format!("attempt-{n:03}"));
        attempt.handoff_file = attempt.directory.join("handoff.json");
        for (c, check) in attempt.checks.iter_mut().enumerate() {
            check.directory = attempt
                .directory
                .join("checks")
                .join(format!("{:02}", c + 1));
        }
        attempt.worktree = None;
    }
    run.resume = None;
    for state in &mut run.stages {
        state.fan_out = None;
    }
    std::fs::create_dir_all(&run.directory).unwrap();
    run.status = Status::Running;
    run.persist().unwrap();
    let mut slot = super::run_slot::RunSlot::new(run.clone());
    slot.worker = Some(Worker::spawn(
        None,
        |_| false,
        move |emit| {
            emit(RunEvent::State(Box::new(run.clone())));
            // As a launch records its process group: saved, never reported.
            run.persist().unwrap();
            panic!("after the save")
        },
    ));
    let start = std::time::Instant::now();
    let result = loop {
        if let Some(result) = slot.poll().1 {
            break result;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert!(result.unwrap_err().contains("after the save"));
    let saved = crate::engine::load(project.path(), &slot.run.id).unwrap();
    assert_eq!(slot.run.revision, saved.revision);
    assert_eq!(slot.run.status, Status::Failed);
    // So the next command is not refused as a change by another process.
    let error = crate::engine::command(
        project.path(),
        &slot.run.id,
        Some(slot.run.revision),
        crate::engine::Cmd::Retry {
            interactive: false,
            model: None,
        },
    )
    .err()
    .map(|e| format!("{e:#}"))
    .unwrap_or_default();
    assert!(!error.contains("changed in another process"), "{error}");
}

#[test]
fn a_panic_off_the_tui_thread_leaves_the_terminal_alone() {
    let tui = std::thread::current().id();
    let restored = std::sync::atomic::AtomicUsize::new(0);
    let restore = || {
        restored.fetch_add(1, Ordering::Relaxed);
    };
    std::thread::scope(|scope| {
        scope
            .spawn(|| super::on_panic(tui, &restore))
            .join()
            .unwrap();
    });
    assert_eq!(restored.load(Ordering::Relaxed), 0);
    super::on_panic(tui, &restore);
    assert_eq!(restored.load(Ordering::Relaxed), 1);
}

#[test]
fn text_dialog_scrolling_stops_at_the_last_line() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.text("Three lines", "one\ntwo\nthree".into());
    for _ in 0..3 {
        app.key(key(Key::PageDown)).unwrap();
    }
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Text { scroll: 2, .. })
    ));
    app.key(key(Key::Up)).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Text { scroll: 1, .. })
    ));
}

#[test]
fn stage_editor_asks_before_discarding_changes() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let original = serde_json::to_value(&app.editor.settings).unwrap();
    // Unchanged: Esc closes at once.
    app.key(key(Key::Char('e'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.dialog().is_none());
    app.key(key(Key::Char('e'))).unwrap();
    app.key(key(Key::Char('!'))).unwrap();
    app.key(key(Key::Esc)).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Ask(_))));
    app.key(key(Key::Esc)).unwrap();
    assert!(
        matches!(app.ui.dialog(), Some(Modal::Stage { editor, .. }) if editor.finish().unwrap().name.ends_with('!'))
    );
    app.key(key(Key::Esc)).unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.dialog().is_none());
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
    // Finished: the brief, which scrolls only as far as its end.
    app.slot_mut().worker = None;
    app.session.adopt(sample_run("done"));
    let screen = rows(&mut app, 65, 20).join("\n");
    assert!(
        screen.contains("Next") && screen.contains("[d] diff"),
        "{screen}"
    );
    for _ in 0..50 {
        app.key(key(Key::PageDown)).unwrap();
    }
    let limit = app.slot().results_limit.get();
    assert!(limit > 0);
    assert_eq!(app.slot().results_scroll, limit);
    assert!(rows(&mut app, 65, 20).join("\n").contains("Totals"));
    app.key(key(Key::PageUp)).unwrap();
    assert_eq!(app.slot().results_scroll, limit.saturating_sub(5));
    for modal in [
        Modal::Ask(Box::new(Ask::CancelStage { run: "done".into() })),
        Modal::Loading(Loading::new("Opening project…")),
        Modal::Text {
            title: "Guide".into(),
            body: super::actions::guide(),
            scroll: 0,
            guide: true,
        },
    ] {
        app.ui.open(modal);
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
    app.ui.page = Page::Runs;
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
        app.ui.dialog(),
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
    assert!(
        matches!(app.ui.dialog().as_deref_ask(), Some(Ask::CancelRun { run }) if run == "busy")
    );
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
    assert!(matches!(app.ui.dialog(), Some(Modal::SessionRuns { .. })));
    assert!(
        app.ui.message.contains("nothing to cancel"),
        "{}",
        app.ui.message
    );

    // [Enter] focuses the selected run.
    app.key(key(Key::Enter)).unwrap();
    assert!(app.ui.dialog().is_none());
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
    assert_eq!(app.ui.page, Page::Runs);
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
impl AskModal for Option<&Modal> {
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
    let Some(Modal::Text { body, .. }) = app.ui.dialog() else {
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
    assert!(matches!(app.ui.dialog(), Some(Modal::Notifications { .. })));
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
    assert!(matches!(app.ui.dialog(), Some(Modal::Input { .. })));
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
        app.ui.dialog(),
        Some(Modal::Notifications { selected: 3, .. })
    ));
    let saved = theme::load(&app.ui.theme_path).unwrap();
    assert!(saved.notifications.webhook);
    assert_eq!(saved.notifications.webhook_url, "https://ntfy.sh/mr-test");

    // The test button reports what it sent.
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Enter)).unwrap();
    let Some(Modal::Notifications { status, .. }) = app.ui.dialog() else {
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
    app.ui.page = Page::Runs;
    app.ui
        .open(Modal::Diff(Box::new(super::diff_view::DiffView::new(
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
    assert!(matches!(app.ui.dialog(), Some(Modal::Diff(_))));
    assert_eq!(app.ui.page, Page::Runs);
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.dialog().is_none());
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
        matches!(app.ui.dialog(), Some(Modal::Diff(_))),
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
fn a_search_still_being_typed_keeps_its_keys_when_the_run_finishes() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    app.key(key(Key::Char('/'))).unwrap();
    app.key(key(Key::Char('e'))).unwrap();
    app.slot_mut().worker = None;
    app.slot_mut().run.status = Status::Complete;
    app.poll_completion();
    // x would close the run from its brief; here it is part of the query.
    app.key(key(Key::Char('x'))).unwrap();
    assert_eq!(app.session.runs.len(), 1);
    assert_eq!(app.slot().log_search.as_ref().unwrap().query, "ex");
}

#[test]
fn arrows_typed_into_a_search_do_not_move_to_another_run() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    app.session.adopt(sample_run("second"));
    app.key(key(Key::Char('/'))).unwrap();
    app.key(key(Key::Char('a'))).unwrap();
    app.key(key(Key::Up)).unwrap();
    // x would close the run the arrow moved to; here it goes into the query.
    app.key(key(Key::Char('x'))).unwrap();
    assert_eq!(app.session.runs.len(), 2);
    assert_eq!(app.session.run().unwrap().id, "second");
    assert_eq!(app.slot().log_search.as_ref().unwrap().query, "ax");
    // Letters type into the query, so the footer names the search's keys.
    let footer = rows(&mut app, 120, 36).pop().unwrap();
    assert!(footer.contains("[Enter] find   [Esc] cancel"), "{footer}");
    assert!(
        !footer.contains("[h]") && !footer.contains("[Space]"),
        "{footer}"
    );
}

#[test]
fn the_footer_offers_only_keys_the_selected_run_accepts() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let mut run = sample_run("paused");
    run.status = Status::Pending;
    app.session.adopt(run);
    app.ui.page = Page::Runs;
    // A paused run continues, but has no handoff to accept.
    let footer = rows(&mut app, 120, 36).pop().unwrap();
    assert!(footer.contains("[c] continue   [y] retry"), "{footer}");
    assert!(!footer.contains("[a]"), "{footer}");
    // An interrupted one must be retried; continuing would fail it.
    app.slot_mut().run.status = Status::Running;
    let footer = rows(&mut app, 120, 36).pop().unwrap();
    assert!(footer.contains("[y] retry"), "{footer}");
    assert!(!footer.contains("[c]"), "{footer}");
    app.slot_mut().run.status = Status::AwaitingConfirmation;
    let footer = rows(&mut app, 120, 36).pop().unwrap();
    // A waiting one moves on by accepting it, not by continuing.
    assert!(footer.contains("[a] advance   [y] retry"), "{footer}");
    assert!(!footer.contains("[c]"), "{footer}");
}

/// Waits for the focused run's driver to stop.
fn settle_driver(app: &mut App) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while app.session.running() && std::time::Instant::now() < deadline {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!app.session.running());
}

/// The saved run fixture its repair limit stopped, installed in the app's
/// project and focused on Runs. No harness it names exists, so a driver
/// started from it stops before launching anything.
fn repair_blocked_app(project: &std::path::Path) -> App {
    let mut app = theme_app(project);
    app.ui.page = Page::Runs;
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/runs/blocked-repair-limit.json"
    ))
    .unwrap()
    .replace("{{PROJECT}}", app.project.to_str().unwrap());
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["adapters"] = serde_json::json!({});
    for stage in value["workflow"]["stages"].as_array_mut().unwrap() {
        stage["harness"] = "no-such-harness".into();
    }
    let id = value["id"].as_str().unwrap().to_owned();
    let directory = crate::storage::run_dir(&app.project, &id).unwrap();
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("run.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let run = crate::engine::load(&app.project, &id).unwrap();
    assert!(run.repair_blocked());
    crate::trust::approve_run(&run, &Settings::default(), None).unwrap();
    app.session.adopt(run);
    app
}

#[test]
fn allow_repair_is_offered_only_at_the_repair_limit_and_resumes_once_confirmed() {
    let project = tempfile::tempdir().unwrap();
    let mut app = repair_blocked_app(project.path());
    let id = app.slot().run.id.clone();
    let screen = rows(&mut app, 80, 24).join("\n");
    let footer = screen.lines().last().unwrap();
    assert!(footer.contains("[R] allow repair"), "{footer}");
    assert!(screen.contains("[R] allow repair   [y] retry"), "{screen}");
    assert!(!screen.contains("[B]"), "{screen}");

    // The dialog says how many passes ran; declining changes nothing.
    app.key(shift('R')).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Ask(ask)) if matches!(**ask, Ask::AllowRepair { passes: 2, limit: 2, .. })
    ));
    let screen = rows(&mut app, 100, 30).join("\n");
    assert!(screen.contains("after 2 repair passes"), "{screen}");
    app.key(key(Key::Char('n'))).unwrap();
    assert!(app.ui.dialog().is_none());
    let saved = crate::engine::load(&app.project, &id).unwrap();
    assert_eq!(
        (saved.status, saved.extra_repair_passes),
        (Status::Blocked, 0)
    );

    // Confirming allows one more pass and sends the review's findings back.
    app.key(shift('R')).unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.dialog().is_none(), "{}", app.ui.message);
    assert!(
        app.ui.message.starts_with("Allowed 1 more repair pass"),
        "{}",
        app.ui.message
    );
    let saved = crate::engine::load(&app.project, &id).unwrap();
    assert_eq!((saved.extra_repair_passes, saved.cursor), (1, 1));
    settle_driver(&mut app);

    // Any other stop neither offers nor accepts it.
    let mut app = theme_app(project.path());
    app.ui.page = Page::Runs;
    let mut run = sample_run("failed");
    run.status = Status::Failed;
    run.cursor = 0;
    app.session.adopt(run);
    let screen = rows(&mut app, 120, 36).join("\n");
    assert!(!screen.contains("[R]"), "{screen}");
    let error = app.perform(ActionId::AllowRepair).unwrap_err();
    assert!(format!("{error}").contains("repair limit"), "{error}");
    assert!(app.ui.dialog().is_none());
}

/// A saved run its $0.10 run cap stopped after $0.25 of reported spend. No
/// harness it names exists.
fn budget_blocked_app(project: &std::path::Path) -> App {
    let mut app = theme_app(project);
    app.ui.page = Page::Runs;
    let mut run = sample_run("budget");
    for stage in &mut run.workflow.stages {
        stage.harness = "no-such-harness".into();
    }
    run.project = app.project.clone();
    run.workspace = app.project.clone();
    run.directory = crate::storage::run_dir(&app.project, &run.id).unwrap();
    for (n, attempt) in run
        .stages
        .iter_mut()
        .flat_map(|s| &mut s.attempts)
        .enumerate()
    {
        attempt.directory = run
            .directory
            .join("01-implement")
            .join(format!("attempt-{n:03}"));
        attempt.handoff_file = attempt.directory.join("handoff.json");
        for (c, check) in attempt.checks.iter_mut().enumerate() {
            check.directory = attempt
                .directory
                .join("checks")
                .join(format!("{:02}", c + 1));
        }
    }
    run.workflow.budget.max_usd = Some(0.1);
    run.cursor = 2;
    run.stages[2].status = Status::Blocked;
    run.status = Status::Blocked;
    run.error = crate::cost::check_budget(&run);
    assert!(crate::cost::usd_budget_blocked(&run), "{:?}", run.error);
    std::fs::create_dir_all(&run.directory).unwrap();
    run.persist().unwrap();
    app.session.adopt(run);
    app
}

#[test]
fn raise_budget_is_offered_only_on_a_usd_cap_and_checks_the_amount() {
    let project = tempfile::tempdir().unwrap();
    let mut app = budget_blocked_app(project.path());
    let id = app.slot().run.id.clone();
    let saved_cap = |app: &App| {
        crate::engine::load(&app.project, &id)
            .unwrap()
            .workflow
            .budget
            .max_usd
    };
    let screen = rows(&mut app, 80, 24).join("\n");
    let footer = screen.lines().last().unwrap();
    assert!(footer.contains("[B] raise budget"), "{footer}");
    // A retry would stop on the same cap.
    assert!(!footer.contains("[y]"), "{footer}");
    assert!(screen.contains("[B] raise budget   [d] diff"), "{screen}");
    assert!(!screen.contains("[R]"), "{screen}");

    // The prompt names the cap and the spend.
    let type_amount = |app: &mut App, amount: &str| {
        app.key(shift('B')).unwrap();
        let title = dialog(app).unwrap();
        assert!(title.contains("cap $0.10, spent $0.25"), "{title}");
        for c in amount.chars() {
            app.key(key(Key::Char(c))).unwrap();
        }
        app.key(ctrl('s'))
    };
    // Not a number, or not above the cap and the spend: rejected, and the
    // prompt stays.
    for bad in ["ten", "0.05", "0.2", "-3"] {
        let error = type_amount(&mut app, bad).unwrap_err();
        assert!(format!("{error}").contains("Enter "), "{bad}: {error}");
        assert!(
            matches!(app.ui.dialog(), Some(Modal::Input { .. })),
            "{bad}"
        );
        app.key(key(Key::Esc)).unwrap();
        assert!(app.ui.dialog().is_none());
    }
    assert_eq!(saved_cap(&app), Some(0.1));

    // A valid amount asks first; declining changes nothing.
    type_amount(&mut app, "$5").unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Ask(ask)) if matches!(**ask, Ask::RaiseBudget { usd, cap, .. } if usd == 5.0 && cap == 0.1)
    ));
    let screen = rows(&mut app, 100, 30).join("\n");
    assert!(screen.contains("from $0.10 to $5.00"), "{screen}");
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.dialog().is_none());
    assert_eq!(saved_cap(&app), Some(0.1));

    // Confirming raises the cap and continues the run.
    type_amount(&mut app, "5").unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.dialog().is_none(), "{}", app.ui.message);
    assert!(
        app.ui.message.starts_with("Budget raised to $5.00"),
        "{}",
        app.ui.message
    );
    assert_eq!(saved_cap(&app), Some(5.0));
    settle_driver(&mut app);
    assert!(!crate::cost::budget_blocked(&app.slot().run));

    // Token caps can also be raised directly in the TUI.
    let mut app = budget_blocked_app(project.path());
    let id = app.slot().run.id.clone();
    let run = &mut app.slot_mut().run;
    run.workflow.budget.max_usd = None;
    run.workflow.budget.max_tokens = Some(10);
    run.error = crate::cost::check_budget(run);
    run.persist().unwrap();
    assert!(crate::cost::budget_blocked(run) && !crate::cost::usd_budget_blocked(run));
    let screen = rows(&mut app, 120, 36).join("\n");
    let footer = screen.lines().last().unwrap();
    assert!(footer.contains("[B] raise budget"), "{footer}");
    assert!(screen.contains("[B] raise budget   [d] diff"), "{screen}");
    let type_tokens = |app: &mut App, amount: &str| {
        app.key(shift('B')).unwrap();
        let title = dialog(app).unwrap();
        assert!(title.contains("New token budget"), "{title}");
        for c in amount.chars() {
            app.key(key(Key::Char(c))).unwrap();
        }
        app.key(ctrl('s'))
    };
    for bad in ["abc", "5", "100", "-5"] {
        let error = type_tokens(&mut app, bad).unwrap_err();
        assert!(format!("{error}").contains("Enter "), "{bad}: {error}");
        app.key(key(Key::Esc)).unwrap();
    }
    type_tokens(&mut app, "50k").unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Ask(ask)) if matches!(**ask, Ask::RaiseBudgetTokens { tokens: 50_000, cap: 10, .. })
    ));
    app.key(key(Key::Esc)).unwrap();
    assert_eq!(
        crate::engine::load(&app.project, &id)
            .unwrap()
            .workflow
            .budget
            .max_tokens,
        Some(10)
    );
    type_tokens(&mut app, "100000").unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(app.ui.dialog().is_none(), "{}", app.ui.message);
    assert!(
        app.ui.message.contains("Token budget raised"),
        "{}",
        app.ui.message
    );
    assert_eq!(
        crate::engine::load(&app.project, &id)
            .unwrap()
            .workflow
            .budget
            .max_tokens,
        Some(100_000)
    );
    settle_driver(&mut app);
    assert!(!crate::cost::budget_blocked(&app.slot().run));

    // A run that stopped for a non-budget reason still refuses RaiseBudget.
    let mut app = budget_blocked_app(project.path());
    app.slot_mut().run.error = Some("Harness exited".into());
    let error = app.perform(ActionId::RaiseBudget).unwrap_err();
    assert!(format!("{error}").contains("budget"), "{error}");
    assert!(app.ui.dialog().is_none());
}

#[test]
fn saved_runs_say_none_yet_when_only_this_sessions_runs_exist() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.ui.page = Page::Runs;
    // A run of this session is saved too, but listed above, not under Saved.
    let mut run = sample_run("live");
    run.project = project.path().into();
    run.directory = crate::storage::run_dir(project.path(), &run.id).unwrap();
    std::fs::create_dir_all(&run.directory).unwrap();
    run.persist().unwrap();
    app.session.adopt(run);
    let saved = crate::history::list(project.path()).unwrap();
    assert_eq!(saved.len(), 1);
    app.runs.show(saved, project.path());
    app.sync_runs();
    let screen = rows(&mut app, 120, 36).join("\n");
    assert!(
        screen.contains("none yet"),
        "no filter hides anything\n{screen}"
    );
    // Nor does the pane beside it say a filter hides any.
    app.runs.on_saved = true;
    let screen = rows(&mut app, 120, 36).join("\n");
    assert!(screen.contains("No runs yet"), "{screen}");
}

#[test]
fn every_fan_out_candidate_shows_its_status_at_80_columns() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    app.slot_mut().run.stages[0].fan_out = Some(
        serde_json::from_value(serde_json::json!({
            "base": "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            "base_commit": "0123456789abcdef0123456789abcdef01234567",
            "candidates": [
                {"id": "c1", "harness": "claude", "model": "sonnet", "status": "complete",
                 "attempts": [], "handoff": null},
                {"id": "c2", "harness": "codex", "model": "gpt-6-astra", "status": "running",
                 "attempts": [], "handoff": null},
                {"id": "c3", "harness": "opencode", "model": "openrouter/some-long-model",
                 "status": "failed", "attempts": [], "handoff": null}
            ]
        }))
        .unwrap(),
    );
    let screen = rows(&mut app, 80, 24);
    let line = screen.iter().find(|row| row.contains("fan-out")).unwrap();
    // Too wide with models, so they go and each status shows.
    assert!(line.contains("c3 · opencode ✗"), "{line}");
    assert!(!line.contains("sonnet"), "{line}");
    let screen = rows(&mut app, 200, 24);
    let line = screen.iter().find(|row| row.contains("fan-out")).unwrap();
    assert!(line.contains("c1 · claude sonnet ✓"), "{line}");
}

#[test]
fn a_run_started_in_another_project_is_not_rerun_from_this_one() {
    let project = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.ui.page = Page::Runs;
    let mut run = sample_run("elsewhere");
    run.project = other.path().into();
    app.session.adopt(run);
    for action in [ActionId::Rerun, ActionId::CompareAttempts] {
        let error = app.perform(action).unwrap_err().to_string();
        assert!(error.contains("open that project with [p]"), "{error}");
        assert!(app.ui.dialog().is_none());
    }
}

#[test]
fn the_empty_task_hint_is_shown_whole_at_80_columns() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.editor.task.clear();
    let screen = rows(&mut app, 80, 24).join("\n");
    assert!(screen.contains("GitHub issue."), "{screen}");
}

#[test]
fn filtering_shows_the_sidebar_even_beside_a_narrow_session_run() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    assert!(!rows(&mut app, 80, 24).join("\n").contains("This session"));
    app.key(key(Key::Char('F'))).unwrap();
    app.key(key(Key::Char('z'))).unwrap();
    assert!(app.runs_filtering());
    let screen = rows(&mut app, 80, 24).join("\n");
    assert!(screen.contains("F z"), "the filter field shows\n{screen}");
    assert!(screen.contains("[Enter] keep"), "{screen}");
    // The footer names the filter's keys; letters type into it.
    let footer = screen.lines().last().unwrap();
    assert!(footer.contains("[Esc] clear"), "{footer}");
    assert!(
        !footer.contains("[s]") && !footer.contains("[Space]"),
        "{footer}"
    );
}

#[test]
fn a_space_group_too_tall_for_the_sheet_still_lists_every_action() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    app.slot_mut().worker = None;
    app.session.adopt(sample_run("done"));
    let view = super::actions::menu(super::actions::Scope::MonitorIdle)
        .into_iter()
        .find(|(group, _)| *group == super::actions::Group::View)
        .map(|(_, items)| items)
        .unwrap();
    let last = view.last().unwrap().1.label;
    app.key(key(Key::Char(' '))).unwrap();
    let screen = rows(&mut app, 65, 20).join("\n");
    assert!(screen.contains("more: press v"), "{screen}");
    // Once another group is chosen, v is not a group key any more.
    app.key(key(Key::Char('r'))).unwrap();
    assert!(!rows(&mut app, 65, 20).join("\n").contains("more: press"));
    app.key(key(Key::Backspace)).unwrap();
    app.key(key(Key::Char('v'))).unwrap();
    let screen = rows(&mut app, 65, 20).join("\n");
    assert!(
        screen.contains(&format!(" {last}")),
        "lacks {last:?}\n{screen}"
    );
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
    // Idle, `n` reruns the run, except while a search is shown.
    app.slot_mut().worker = None;
    app.key(key(Key::Char('n'))).unwrap();
    assert!(app.ui.dialog().is_none());
    app.key(key(Key::Esc)).unwrap();
    assert!(app.slot().log_search.is_none());
    app.key(key(Key::Char('n'))).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Confirm(_))));
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
    assert!(matches!(app.ui.dialog(), Some(Modal::Handoff(_))));
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
    assert!(matches!(app.ui.dialog(), Some(Modal::Handoff(_))));
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.dialog().is_none());
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

#[test]
fn open_project_resets_route_stats_collecting_and_runs_sizes() {
    let project = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.route_stats.collecting = true;
    app.runs
        .sizes
        .lock()
        .unwrap()
        .insert("stale-run".into(), 42);
    app.switch_project(other.path().into()).unwrap();
    assert!(!app.route_stats.collecting);
    assert!(app.runs.sizes.lock().unwrap().is_empty());
    app.refresh_stats();
    assert!(app.route_stats.collecting);
}

#[test]
fn unchanged_repo_msg_does_not_clobber_pending_redraw_and_stale_msgs_stay_quiet() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let old = app.tasks.generation();
    app.tasks.next_generation();
    app.ui.redraw = false;
    app.update(old, Msg::Checks(Ok(vec![])));
    assert!(!app.ui.redraw);
    let cur = app.tasks.generation();
    app.update(cur, Msg::Checks(Ok(vec![])));
    assert!(app.ui.redraw);
    let unchanged = app.repo.info.clone();
    app.update(cur, Msg::Repo(Ok(unchanged)));
    assert!(app.ui.redraw);
    app.ui.redraw = false;
    app.update(
        cur,
        Msg::Repo(Ok(Some(crate::storage::RepoInfo {
            name: "changed".into(),
            head: "main".into(),
            detached: false,
            changes: 1,
            status_error: None,
        }))),
    );
    assert!(app.ui.redraw);
}

#[test]
fn picker_paste_flattens_newlines_and_resets_selection() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.key(key(Key::Char('T'))).unwrap();
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Down)).unwrap();
    app.paste("Catppuccin\nLatte\r\n");
    let Some(Modal::Picker {
        selected, filter, ..
    }) = app.ui.dialog()
    else {
        panic!("expected picker");
    };
    assert_eq!(*selected, 0);
    assert!(!super::widgets::content(filter).contains(['\r', '\n']));
}

#[test]
fn ask_apply_worktree_renders_multiline_preview_with_pinned_keys() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let preview = (1..=15)
        .map(|i| format!("M src/file_{i}.rs"))
        .collect::<Vec<_>>()
        .join("\n");
    app.ui.open(Modal::Ask(Box::new(Ask::ApplyWorktree {
        run: "run-1".into(),
        files: 15,
        preview,
    })));
    let screen = rows(&mut app, 80, 24).join("\n");
    assert!(screen.contains("M src/file_1.rs"), "{screen}");
    assert!(screen.contains("M src/file_15.rs"), "{screen}");
    assert!(
        screen.contains("[y] / [Enter] apply uncommitted"),
        "{screen}"
    );
    assert!(screen.contains("[n] / [Esc] cancel"), "{screen}");
}

#[test]
fn output_search_status_shows_on_first_frame_without_lag() {
    let project = tempfile::tempdir().unwrap();
    let mut app = running_app(project.path());
    app.slot_mut().logs.clear();
    app.slot_mut().push_log("alpha one".into());
    app.slot_mut().push_log("beta two".into());
    app.slot_mut().push_log("alpha three".into());
    app.slot_mut().log_search = Some(super::output_search::LogSearch {
        query: "alpha".into(),
        typing: false,
        current: 0,
    });
    assert!(app.slot().cache.borrow().output.is_none());
    let text = rows(&mut app, 80, 24).join("\n");
    assert!(text.contains("/alpha · 1/2"), "{text}");
}

#[test]
fn apply_worktree_plans_in_background_and_surfaces_errors_without_blocking() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let mut run = sample_run("wt");
    run.project = project.path().into();
    run.workspace = project.path().into();
    run.worktree = true;
    app.session.adopt(run);
    app.ui.page = Page::Runs;
    app.perform(ActionId::ApplyWorktree).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Loading(_))));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while matches!(app.ui.dialog(), Some(Modal::Loading(_))) && std::time::Instant::now() < deadline
    {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(app.ui.dialog().is_none());
    assert!(!app.ui.alert.is_empty());
}

#[cfg(unix)]
#[test]
fn any_disconnected_detects_peer_hangup_without_blocking() {
    use std::os::unix::{io::AsRawFd, net::UnixStream};
    let (a, b) = UnixStream::pair().unwrap();
    assert!(!super::any_disconnected([a.as_raw_fd()]));
    drop(b);
    // A child another test forks holds a copy of `b` until it execs, so the
    // hangup can arrive a moment after the drop.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !super::any_disconnected([a.as_raw_fd()]) {
        assert!(std::time::Instant::now() < deadline, "no hangup after 5 s");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A harness check result: found on PATH or not.
fn harness_report(harness: &str, installed: bool) -> crate::doctor::Report {
    crate::doctor::Report {
        harness: harness.into(),
        builtin: true,
        program: harness.into(),
        path: installed.then(|| PathBuf::from(format!("/usr/bin/{harness}"))),
        version: None,
        tested_versions: vec![],
        version_tested: None,
        version_captured: false,
        version_pin: None,
        version_pin_ok: None,
        auth_hint: None,
        capabilities: None,
        summary: "ok".into(),
        notes: vec![],
    }
}

#[test]
fn the_tour_follows_the_user_from_harnesses_to_a_completed_run() {
    use super::tour::Step;
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let tour_file = app.tour.path.clone();
    assert!(!rows(&mut app, 100, 30).join("\n").contains("Tour ·"));
    app.key(key(Key::F(1))).unwrap();
    let screen = rows(&mut app, 100, 30).join("\n");
    assert!(screen.contains("Tour · 1 of 6 · Harnesses"), "{screen}");
    assert!(screen.contains("Press [3]"), "{screen}");

    // Harnesses shows its checks: codex is missing, the rest are installed.
    app.key(key(Key::Char('3'))).unwrap();
    app.harnesses.checks.checking = false;
    app.harnesses.checks.reports = crate::config::BUILTINS
        .iter()
        .map(|name| harness_report(name, *name != "codex"))
        .collect();
    let screen = rows(&mut app, 100, 30).join("\n");
    assert_eq!(app.tour_step(), Step::Workflow);
    assert!(screen.contains("Press [1]"), "{screen}");

    // A stage on a missing harness comes first.
    app.key(key(Key::Char('1'))).unwrap();
    let missing = app.editor.settings.workflows[0]
        .stages
        .iter()
        .find(|s| s.enabled && s.harness == "codex")
        .expect("the starter has a codex stage")
        .name
        .clone();
    assert_eq!(
        app.tour_step(),
        Step::Stage {
            name: missing.clone(),
            harness: "codex".into(),
            candidate: false,
        }
    );
    assert!(rows(&mut app, 100, 30).join("\n").contains(&missing));
    for stage in &mut app.editor.settings.workflows[0].stages {
        if stage.harness == "codex" {
            stage.harness = "claude".into();
        }
    }

    // Then the task, then starting.
    assert_eq!(app.tour_step(), Step::Task);
    app.key(key(Key::Char('t'))).unwrap();
    app.paste("Add a --version flag");
    app.key(ctrl('s')).unwrap();
    assert_eq!(app.tour_step(), Step::Start);
    assert!(rows(&mut app, 100, 30).join("\n").contains("Press [r]"));

    // A run started while the tour is open is followed through its states.
    let mut run = sample_run("tour-run");
    run.status = Status::Running;
    app.tour_started(&run.id);
    app.session.adopt(run);
    assert!(rows(&mut app, 100, 30).join("\n").contains("Press [2]"));
    app.ui.page = Page::Runs;
    app.slot_mut().run.status = Status::AwaitingConfirmation;
    let screen = rows(&mut app, 100, 30).join("\n");
    assert!(screen.contains("Tour · 5 of 6"), "{screen}");
    assert!(screen.contains("[a] accepts"), "{screen}");
    assert!(!tour_file.exists(), "Nothing is saved before the end");

    app.slot_mut().run.status = Status::Complete;
    let screen = rows(&mut app, 100, 30).join("\n");
    assert!(screen.contains("Tour · 6 of 6 · Done"), "{screen}");
    assert!(
        std::fs::read_to_string(&tour_file)
            .unwrap()
            .contains("finished")
    );
    // Hiding a finished tour keeps it finished.
    app.key(key(Key::F(1))).unwrap();
    assert!(!app.tour.open);
    assert!(
        std::fs::read_to_string(&tour_file)
            .unwrap()
            .contains("finished")
    );
    assert!(!rows(&mut app, 100, 30).join("\n").contains("Tour ·"));
}

#[test]
fn hiding_the_tour_early_saves_it_and_small_screens_keep_the_page() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.key(key(Key::F(1))).unwrap();
    // At the minimum size the page keeps its rows, and the panel keeps the
    // line that says what to press.
    let small = rows(&mut app, 65, 20).join("\n");
    assert!(small.contains("Stages"), "{small}");
    assert!(small.contains("Press [3]"), "{small}");
    assert!(!small.contains("Model Router runs"), "{small}");
    assert!(rows(&mut app, 80, 30).join("\n").contains("Tour · 1 of 6"));
    app.key(key(Key::F(1))).unwrap();
    assert!(
        std::fs::read_to_string(&app.tour.path)
            .unwrap()
            .contains("hidden")
    );
    assert!(app.ui.message.contains("[F1] shows it again"));
}

/// The tour on Workflow, past Harnesses: only `missing` is not installed.
fn toured_app(project: &std::path::Path, missing: &str) -> App {
    let mut app = theme_app(project);
    app.key(key(Key::F(1))).unwrap();
    app.ui.page = Page::Harnesses;
    app.harnesses.checks.reports = crate::config::BUILTINS
        .iter()
        .map(|name| harness_report(name, *name != missing))
        .collect();
    app.sync_tour();
    app.ui.page = Page::Workflow;
    app
}

#[test]
fn the_tour_checks_the_harnesses_a_run_needs_and_at_least_one_stage() {
    use super::tour::Step;
    let project = tempfile::tempdir().unwrap();
    let mut app = toured_app(project.path(), "antigravity");
    let stages = &mut app.editor.settings.workflows[0].stages;
    for stage in stages.iter_mut() {
        stage.harness = "claude".into();
    }
    // A ship stage runs in the router, so its harness does not matter.
    stages[0].role = Role::Ship;
    stages[0].harness = "antigravity".into();
    assert_eq!(app.tour_step(), Step::Task);
    // A fan-out stage needs its candidates' harnesses.
    let stages = &mut app.editor.settings.workflows[0].stages;
    stages[1].fan_out = Some(crate::config::FanOut {
        candidates: vec![crate::config::Candidate {
            harness: "antigravity".into(),
            ..Default::default()
        }],
        judge: String::new(),
    });
    let name = stages[1].name.clone();
    assert_eq!(
        app.tour_step(),
        Step::Stage {
            name,
            harness: "antigravity".into(),
            candidate: true,
        }
    );
    // [h] would change the stage, not the candidate: the panel says [e].
    assert!(
        rows(&mut app, 100, 30)
            .join("\n")
            .contains("Fan-out candidates")
    );
    // Turning every stage off, as the advice allows, is no way to start.
    for stage in &mut app.editor.settings.workflows[0].stages {
        stage.enabled = false;
    }
    assert_eq!(app.tour_step(), Step::NoStage);
    assert!(
        rows(&mut app, 100, 30)
            .join("\n")
            .contains("[o] to turn it on")
    );
    // A new project's harnesses are unknown until Harnesses checks them.
    app.harnesses.checks.clear();
    assert_eq!(app.tour_step(), Step::Harnesses);
}

#[test]
fn the_tour_names_the_followed_run_and_how_it_recovers() {
    use super::tour::{Step, Watch};
    let project = tempfile::tempdir().unwrap();
    let mut app = toured_app(project.path(), "antigravity");
    let mut run = sample_run("tour-run");
    run.status = Status::Running;
    app.tour_started(&run.id);
    app.session.adopt(run);
    app.ui.page = Page::Runs;
    assert_eq!(app.tour_step(), Step::Watch(Watch::Running));
    // Another run selected: its keys would act on that one.
    app.session.adopt(sample_run("other"));
    assert_eq!(app.tour_step(), Step::Watch(Watch::Unselected));
    app.session.focus_run("tour-run");
    // A USD budget stop recovers with [B], not a retry.
    let run = app.slot_mut();
    run.run.status = Status::Blocked;
    run.run.error = Some(format!("{}: $2.10 of $2.00", crate::cost::EXCEEDED));
    assert_eq!(app.tour_step(), Step::Watch(Watch::Budget));
    assert!(
        rows(&mut app, 100, 30)
            .join("\n")
            .contains("[B] raises the cap")
    );
    app.slot_mut().run.error = Some("Harness exited 1".into());
    assert_eq!(app.tour_step(), Step::Watch(Watch::Stopped));
}

fn git_cmd(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn init_git_repo(dir: &std::path::Path) {
    git_cmd(dir, &["init", "-q"]);
    git_cmd(dir, &["config", "user.name", "Test"]);
    git_cmd(dir, &["config", "user.email", "test@example.invalid"]);
    git_cmd(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join(".gitignore"), ".model-router/\n").unwrap();
    std::fs::write(dir.join("hello.txt"), "initial\n").unwrap();
    git_cmd(dir, &["add", "."]);
    git_cmd(dir, &["commit", "-q", "-m", "initial"]);
}

#[test]
fn workflow_settings_editor_validates_and_marks_dirty() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    app.ui.page = Page::Workflow;
    assert!(!app.editor.dirty);

    // Accessible via `W` and via `Space w W`.
    app.key(key(Key::Char(' '))).unwrap();
    app.key(key(Key::Char('w'))).unwrap();
    app.key(shift('W')).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Workflow(_))));
    let screen = rows(&mut app, 80, 24).join("\n");
    assert!(screen.contains("Workflow settings"), "{screen}");
    // Unchanged Esc closes without DiscardDraft.
    app.key(key(Key::Esc)).unwrap();
    assert!(app.ui.dialog().is_none());

    app.key(shift('W')).unwrap();
    if let Some(Modal::Workflow(editor)) = app.ui.dialog_mut() {
        // Reject invalid max_parallel = 0.
        editor.fields[1] = textarea("0");
    }
    app.key(ctrl('s')).unwrap();
    // Stays open with inline validation error and settings unchanged.
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Workflow(editor)) if !editor.error.is_empty()
    ));
    assert!(!app.editor.dirty);

    // Edit all workflow-level fields cleanly.
    if let Some(Modal::Workflow(editor)) = app.ui.dialog_mut() {
        editor.fields[0] = textarea("4");
        editor.fields[1] = textarea("6");
        editor.fields[2] = textarea("by-role");
        editor.fields[3] = textarea("on");
        editor.fields[4] = textarea("$12.50");
        editor.fields[5] = textarea("250k");
        editor.fields[6] = textarea("warn");
        editor.fields[7] = textarea(".github/workflows/*\n**/Cargo.lock");
    }
    // Esc on dirty editor prompts DiscardDraft.
    app.key(key(Key::Esc)).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Ask(ask)) if matches!(**ask, Ask::DiscardDraft)
    ));
    app.key(key(Key::Esc)).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Workflow(_))));

    // Ctrl+S validates, applies, and marks dirty.
    app.key(ctrl('s')).unwrap();
    assert!(app.ui.dialog().is_none());
    assert!(app.editor.dirty);
    let wf = &app.editor.settings.workflows[app.editor.workflow];
    assert_eq!(wf.max_repair_passes, 4);
    assert_eq!(wf.max_parallel, 6);
    assert_eq!(wf.sandbox_default, crate::config::Sandbox::ByRole);
    assert!(wf.handoff_repair);
    assert_eq!(wf.budget.max_usd, Some(12.5));
    assert_eq!(wf.budget.max_tokens, Some(250_000));
    assert_eq!(wf.budget.on_exceed, crate::config::BudgetAction::Warn);
    assert_eq!(
        wf.protected_paths,
        vec![".github/workflows/*".to_owned(), "**/Cargo.lock".to_owned()]
    );
}

#[test]
fn stage_editor_edits_all_new_fields_and_scrolls_on_narrow_terminal() {
    use crate::config::{Billing, Budget, BudgetAction, Sandbox, SandboxFallback};
    use crate::delegation::Delegation;
    let stage = Stage {
        role: Role::Test,
        harness: "codex".into(),
        billing: Some(Billing::Api),
        budget: Budget {
            max_usd: Some(3.25),
            max_tokens: Some(80_000),
            on_exceed: BudgetAction::Warn,
        },
        require_router_checks: true,
        sandbox: Some(Sandbox::ReadOnly),
        sandbox_writable: vec!["~/cache".into(), "/tmp/build".into()],
        sandbox_network: false,
        sandbox_fallback: SandboxFallback::Warn,
        delegation: Some(Delegation {
            harnesses: vec!["claude".into()],
            models: vec!["sonnet*".into()],
            runs: true,
            schedules: true,
            ..Delegation::default()
        }),
        ..Stage::default()
    };
    let mut editor = StageEditor::new(stage.clone(), vec!["codex".into(), "claude".into()]);
    assert!(!editor.changed());
    let roundtrip = editor.finish().unwrap();
    assert_eq!(roundtrip.billing, Some(Billing::Api));
    assert_eq!(roundtrip.budget.max_usd, Some(3.25));
    assert_eq!(roundtrip.budget.max_tokens, Some(80_000));
    assert_eq!(roundtrip.budget.on_exceed, BudgetAction::Warn);
    assert!(roundtrip.require_router_checks);
    assert_eq!(roundtrip.sandbox_writable, vec!["~/cache", "/tmp/build"]);
    assert!(!roundtrip.sandbox_network);
    assert_eq!(roundtrip.sandbox_fallback, SandboxFallback::Warn);
    let del = roundtrip.delegation.unwrap();
    assert_eq!(del.harnesses, vec!["claude"]);
    assert_eq!(del.models, vec!["sonnet*"]);
    assert!(del.runs && del.schedules);

    // Narrow 65x20 terminal renders cleanly while stepping through every field.
    let palette = super::theme::Theme::default().palette();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(65, 20)).unwrap();
    for _ in 0..editor.fields.len() {
        terminal
            .draw(|frame| editor.draw(frame, frame.area(), palette))
            .unwrap();
        editor.key(key(Key::Tab));
    }

    // Invalid delegation TOML fails cleanly.
    editor.fields[22] = textarea("unknown_delegation_key = true");
    let err = editor.finish().unwrap_err();
    assert!(format!("{err:#}").contains("Delegation:"), "{err:#}");
}

#[test]
fn handoffs_budget_view_and_transitions_timeline_open_from_runs() {
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let mut run = sample_run("inspect-run");
    run.project = project.path().into();
    run.workspace = project.path().into();
    run.directory = crate::storage::run_dir(project.path(), &run.id).unwrap();
    std::fs::create_dir_all(&run.directory).unwrap();
    // Put a huge summary on stage 0 so stage 1's handoff inspection reports truncation.
    run.stages[0].status = Status::Complete;
    run.stages[0].handoff = Some(crate::handoff::Handoff::unstructured("x".repeat(7000)));
    run.cursor = 1;
    run.persist().unwrap();
    crate::transitions::append(
        &run.directory,
        &[crate::transitions::Record {
            at: chrono::Utc::now(),
            run_revision: 1,
            level: crate::transitions::Level::Run,
            stage_index: None,
            attempt: None,
            candidate: None,
            from: Status::Pending,
            to: Status::Running,
            reason: "started stage one".into(),
            actor: crate::transitions::Actor::Engine,
        }],
    )
    .unwrap();
    app.session.adopt(run);
    app.ui.page = Page::Runs;

    // Handoffs budget/loss view via `H`.
    app.key(shift('H')).unwrap();
    match app.ui.dialog() {
        Some(Modal::Text { title, body, .. }) => {
            assert!(title.contains("Handoffs budget & loss"), "{title}");
            assert!(body.contains("warning") || body.contains("cut"), "{body}");
        }
        _ => panic!("Expected Handoffs Text modal"),
    }
    app.key(key(Key::Esc)).unwrap();

    // Transitions timeline view via `l`.
    app.key(key(Key::Char('l'))).unwrap();
    match app.ui.dialog() {
        Some(Modal::Text { title, body, .. }) => {
            assert!(title.contains("Transitions"), "{title}");
            assert!(body.contains("started stage one"), "{body}");
            assert!(body.contains("pending → running"), "{body}");
        }
        _ => panic!("Expected Transitions Text modal"),
    }
}

#[test]
fn fork_run_from_stage_checkpoint_launches_worktree_fork_into_session() {
    use super::modal::InputTarget;
    use crate::config::{CustomAdapter, Mode};
    let project = tempfile::tempdir().unwrap();
    init_git_repo(project.path());
    // Runs record the canonical project path (macOS temp dirs are symlinked).
    let mut app = theme_app(&std::fs::canonicalize(project.path()).unwrap());
    let mut settings = Settings::default();
    settings.adapters.insert(
        "fake".into(),
        CustomAdapter {
            command: "true".into(),
            ..CustomAdapter::default()
        },
    );
    let mut wf = settings.workflows[0].clone();
    wf.stages = vec![
        Stage {
            id: "s1".into(),
            name: "First".into(),
            role: Role::Research,
            harness: "fake".into(),
            mode: Mode::Unattended,
            ..Stage::default()
        },
        Stage {
            id: "s2".into(),
            name: "Second".into(),
            role: Role::Implementation,
            harness: "fake".into(),
            model: "orig-model".into(),
            mode: Mode::Unattended,
            ..Stage::default()
        },
    ];
    settings.workflows = vec![wf.clone()];
    app.editor.settings = settings.clone();

    let mut run =
        crate::engine::Run::create(project.path(), &settings, &wf, "Task".into(), false).unwrap();
    let tree = crate::snapshot::tree(project.path(), &run.directory).unwrap();
    let now = chrono::Utc::now();
    run.stages[0].status = Status::Complete;
    run.stages[0].handoff = Some(crate::handoff::Handoff::unstructured("stage 1 done".into()));
    run.stages[0].attempts.push(crate::engine::Attempt {
        number: 1,
        started_at: now - chrono::Duration::seconds(10),
        finished_at: Some(now - chrono::Duration::seconds(5)),
        mode: Mode::Unattended,
        model: String::new(),
        status: Status::Complete,
        exit_code: Some(0),
        directory: run.directory.join("01-s1/attempt-001"),
        handoff_file: run.directory.join("01-s1/attempt-001/handoff.json"),
        error: None,
        checks: vec![],
        checks_tree: None,
        ignored_protected_before: None,
        usage: None,
        session: None,
        process_group: None,
        process_identity: Default::default(),
        terminal: None,
        handoff_source: None,
        worktree: None,
        tree_before: Some(tree.clone()),
        tree_after: Some(tree.clone()),
        turns: vec![],
        tasks: vec![],
        launched_runs: vec![],
    });
    run.stages[1].status = Status::Complete;
    run.stages[1].attempts.push(crate::engine::Attempt {
        number: 1,
        started_at: now,
        finished_at: Some(now + chrono::Duration::seconds(5)),
        mode: Mode::Unattended,
        model: "orig-model".into(),
        status: Status::Complete,
        exit_code: Some(0),
        directory: run.directory.join("02-s2/attempt-001"),
        handoff_file: run.directory.join("02-s2/attempt-001/handoff.json"),
        error: None,
        checks: vec![],
        checks_tree: None,
        ignored_protected_before: None,
        usage: None,
        session: None,
        process_group: None,
        process_identity: Default::default(),
        terminal: None,
        handoff_source: None,
        worktree: None,
        tree_before: Some(tree),
        tree_after: None,
        turns: vec![],
        tasks: vec![],
        launched_runs: vec![],
    });
    run.status = Status::Complete;
    run.cursor = 2;
    run.persist().unwrap();
    let source_id = run.id.clone();

    app.session.adopt(run);
    app.ui.page = Page::Runs;

    // `K` opens stage checkpoint picker.
    app.key(shift('K')).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Picker { .. })));
    // Move to second stage (`s2`) and pick it.
    app.key(key(Key::Down)).unwrap();
    app.key(key(Key::Enter)).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Input {
            target: InputTarget::ForkModel,
            ..
        })
    ));
    for c in "fork-model".chars() {
        app.key(key(Key::Char(c))).unwrap();
    }
    app.key(ctrl('s')).unwrap();
    assert!(app.starting);

    // Wait for background fork creation to finish and join session runs.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.starting && std::time::Instant::now() < deadline {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!app.starting, "{}", app.ui.message);
    assert_eq!(app.session.runs.len(), 2, "{}", app.ui.message);
    let forked = app.session.run().unwrap();
    assert!(forked.worktree);
    let fork_meta = forked.fork_of.as_ref().expect("fork_of recorded");
    assert_eq!(fork_meta.run, source_id);
    assert_eq!(fork_meta.stage, "s2");
    assert_eq!(forked.workflow.stages[1].model, "fork-model");
    settle_driver(&mut app);
}

#[test]
fn ad_hoc_ship_shows_preview_and_never_commits_without_confirmation() {
    use crate::config::{CustomAdapter, Mode};
    let project = tempfile::tempdir().unwrap();
    init_git_repo(project.path());
    // Runs record the canonical project path (macOS temp dirs are symlinked).
    let mut app = theme_app(&std::fs::canonicalize(project.path()).unwrap());
    let mut settings = Settings::default();
    settings.adapters.insert(
        "fake".into(),
        CustomAdapter {
            command: "true".into(),
            ..CustomAdapter::default()
        },
    );
    let mut wf = settings.workflows[0].clone();
    wf.stages = vec![Stage {
        id: "implement".into(),
        name: "Implement".into(),
        role: Role::Implementation,
        harness: "fake".into(),
        mode: Mode::Unattended,
        ..Stage::default()
    }];
    settings.workflows = vec![wf.clone()];
    let mut run =
        crate::engine::Run::create(project.path(), &settings, &wf, "Add greeting".into(), false)
            .unwrap();
    // Simulate a completed run that modified a tracked file.
    let before = crate::snapshot::tree(project.path(), &run.directory).unwrap();
    std::fs::write(project.path().join("hello.txt"), "updated\n").unwrap();
    let after = crate::snapshot::tree(project.path(), &run.directory).unwrap();
    run.stages[0].status = Status::Complete;
    run.stages[0].handoff = Some(crate::handoff::Handoff::unstructured(
        "Updated greeting".into(),
    ));
    run.stages[0].attempts.push(crate::engine::Attempt {
        number: 1,
        started_at: chrono::Utc::now(),
        finished_at: Some(chrono::Utc::now()),
        mode: Mode::Unattended,
        model: String::new(),
        status: Status::Complete,
        exit_code: Some(0),
        directory: run.directory.join("01-implement/attempt-001"),
        handoff_file: run.directory.join("01-implement/attempt-001/handoff.json"),
        error: None,
        checks: vec![],
        checks_tree: None,
        ignored_protected_before: None,
        usage: None,
        session: None,
        process_group: None,
        process_identity: Default::default(),
        terminal: None,
        handoff_source: None,
        worktree: None,
        tree_before: Some(before),
        tree_after: Some(after),
        turns: vec![],
        tasks: vec![],
        launched_runs: vec![],
    });
    run.status = Status::Complete;
    run.cursor = run.stages.len();
    run.persist().unwrap();

    app.session.adopt(run);
    app.ui.page = Page::Runs;

    let commits_before = git_cmd(project.path(), &["rev-list", "--count", "HEAD"]);
    let branch_before = git_cmd(project.path(), &["branch", "--show-current"]);

    // `S` plans ship in background and opens ShipConfirm preview.
    app.key(shift('S')).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Loading(_))));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(app.ui.dialog(), Some(Modal::Loading(_))) && std::time::Instant::now() < deadline
    {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        matches!(app.ui.dialog(), Some(Modal::ShipConfirm(_))),
        "{}",
        app.ui.message
    );
    let screen = rows(&mut app, 100, 32).join("\n");
    assert!(screen.contains("Ship preview"), "{screen}");
    assert!(screen.contains("hello.txt"), "{screen}");
    assert!(screen.contains("ship as previewed"), "{screen}");

    // Declining with `n` NEVER commits.
    app.key(key(Key::Char('n'))).unwrap();
    assert!(app.ui.dialog().is_none());
    assert_eq!(
        git_cmd(project.path(), &["rev-list", "--count", "HEAD"]),
        commits_before
    );
    assert_eq!(
        git_cmd(project.path(), &["branch", "--show-current"]),
        branch_before
    );

    // Re-open preview and explicitly confirm with `y` -> commits on a new branch.
    app.key(shift('S')).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(app.ui.dialog(), Some(Modal::Loading(_))) && std::time::Instant::now() < deadline
    {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(matches!(app.ui.dialog(), Some(Modal::ShipConfirm(_))));
    app.key(key(Key::Char('y'))).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Loading(_))));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(app.ui.dialog(), Some(Modal::Loading(_))) && std::time::Instant::now() < deadline
    {
        app.poll().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(app.ui.dialog().is_none(), "{}", app.ui.message);
    assert!(app.ui.message.contains("Shipped"), "{}", app.ui.message);
    assert_ne!(
        git_cmd(project.path(), &["branch", "--show-current"]),
        branch_before
    );
    assert_ne!(
        git_cmd(project.path(), &["rev-list", "--count", "HEAD"]),
        commits_before
    );
}

#[test]
fn delegated_tasks_inspector_shows_handoff_questions_patch_and_cancel_prompt() {
    use crate::config::Mode;
    use crate::delegation::{
        Isolation,
        policy::stage_ceiling,
        records::{ResolvedTarget, SealedPatch, TaskState, TaskStatus},
    };
    let project = tempfile::tempdir().unwrap();
    let mut app = theme_app(project.path());
    let mut run = sample_run("del-run");
    run.project = project.path().into();
    run.workspace = project.path().into();
    run.directory = crate::storage::run_dir(project.path(), &run.id).unwrap();
    let patch_rel = std::path::PathBuf::from("patches/t1-rev1.diff");
    let patch_abs = run.directory.join(&patch_rel);
    std::fs::create_dir_all(patch_abs.parent().unwrap()).unwrap();
    std::fs::write(
        &patch_abs,
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
    )
    .unwrap();
    let ceiling = stage_ceiling(&run.workflow, &run.workflow.stages[0], false);
    let task1 = TaskState {
        id: "t1".into(),
        client_request_id: "req-1".into(),
        fingerprint: "fp-1".into(),
        title: "Audit parser".into(),
        role: Role::Review,
        target: ResolvedTarget {
            delegate: Some("reviewer".into()),
            harness: "codex".into(),
            model: "gpt-5".into(),
            provider: String::new(),
            effort: String::new(),
        },
        isolation: Isolation::Worktree,
        ceiling,
        status: TaskStatus::Waiting,
        task: "Check edge cases".into(),
        turns: vec![],
        handoff: Some(crate::handoff::Handoff::unstructured(
            "Found edge case in parser".into(),
        )),
        latest_handoff: None,
        questions: vec!["Should empty input error or return default?".into()],
        worktree: None,
        base: None,
        patches: vec![SealedPatch {
            revision: 1,
            turn: 1,
            path: patch_abs,
            sha256: "abc".into(),
            files: vec!["a.rs".into()],
            added: 1,
            removed: 1,
            apply: None,
        }],
        error: None,
        delivered: false,
        announced: false,
        cancelled_by_parent: false,
        queued_messages: vec![],
        sends: vec![],
        created_at: chrono::Utc::now(),
    };
    run.stages[0].attempts.push(crate::engine::Attempt {
        number: 1,
        started_at: chrono::Utc::now(),
        finished_at: None,
        mode: Mode::Unattended,
        model: String::new(),
        status: Status::Running,
        exit_code: None,
        directory: run.directory.join("01-stage/attempt-001"),
        handoff_file: run.directory.join("01-stage/attempt-001/handoff.json"),
        error: None,
        checks: vec![],
        checks_tree: None,
        ignored_protected_before: None,
        usage: None,
        session: None,
        process_group: None,
        process_identity: Default::default(),
        terminal: None,
        handoff_source: None,
        worktree: None,
        tree_before: None,
        tree_after: None,
        turns: vec![],
        tasks: vec![task1],
        launched_runs: vec![],
    });
    run.cursor = 0;
    run.persist().unwrap();

    app.session.adopt(run);
    app.ui.page = Page::Runs;

    // `g` opens delegated tasks inspector.
    app.key(key(Key::Char('g'))).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::DelegatedTasks(_))));
    let screen = rows(&mut app, 100, 32).join("\n");
    assert!(screen.contains("t1 · reviewer"), "{screen}");
    assert!(screen.contains("Should empty input error"), "{screen}");
    assert!(screen.contains("Found edge case in parser"), "{screen}");

    // `h` opens the task's handoff view.
    app.key(key(Key::Char('h'))).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Handoff(_))));
    app.key(key(Key::Esc)).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::DelegatedTasks(_))));

    // `d` opens the task's sealed patch diff.
    app.key(key(Key::Char('d'))).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Diff(_))));
    app.key(key(Key::Esc)).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::DelegatedTasks(_))));

    // `C` when live prompts CancelDelegatedTask without cancelling the parent stage.
    if let Some(Modal::DelegatedTasks(view)) = app.ui.dialog_mut() {
        view.live = true;
    }
    app.key(shift('C')).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Ask(ask)) if matches!(**ask, Ask::CancelDelegatedTask { ref task, .. } if task == "t1")
    ));
    assert!(!crate::engine::cancel_delegated_task("del-run", "t1"));

    // Path traversal outside run_dir is refused.
    let outside = project.path().join("outside.diff");
    std::fs::write(&outside, "secret").unwrap();
    let bad_patch = SealedPatch {
        revision: 1,
        turn: 1,
        path: outside,
        sha256: "abc".into(),
        files: vec![],
        added: 0,
        removed: 0,
        apply: None,
    };
    assert!(
        super::delegation_view::read_sealed_patch(&app.slot().run.directory, &bad_patch).is_err()
    );
}

#[test]
fn schedules_dialog_respects_trust_and_supports_approve_pause_enable_delete() {
    use crate::delegation::{
        policy::Ceiling,
        schedules::{self, Schedule, Trigger},
    };
    let project = tempfile::tempdir().unwrap();
    // Write a project config with an unattended workflow and a dangerous custom adapter
    // so trust is required before approving schedules.
    std::fs::write(
        project.path().join(".model-router.toml"),
        r#"version = 2
[adapters.mytool]
command = "echo"
args = ["hi"]

[[workflows]]
name = "Nightly"
[[workflows.stages]]
id = "check"
name = "Check"
role = "research"
harness = "codex"
mode = "unattended"
"#,
    )
    .unwrap();
    let now = chrono::Utc::now();
    schedules::update(project.path(), |store| {
        store.schedules.push(Schedule {
            id: "nightly-check".into(),
            workflow: Some("Nightly".into()),
            task: "Run nightly audit".into(),
            trigger: Trigger::Every {
                seconds: 3600,
                text: "1h".into(),
            },
            enabled: true,
            created_by: "implement".into(),
            ceiling: Ceiling {
                write: crate::delegation::policy::Write::Workspace,
                native: crate::delegation::policy::Native::Default,
                network: crate::delegation::policy::Network::Open,
                enforcement: crate::delegation::policy::Enforcement::None,
                writable: vec![],
            },
            client_request_id: String::new(),
            requested_by: String::new(),
            request_fingerprint: String::new(),
            revision: 1,
            created_at: now,
            updated_at: now,
            approved_at: None,
            paused_reason: None,
            occurrences: vec![],
        });
        Ok(())
    })
    .unwrap();

    let mut app = theme_app(project.path());
    app.ui.page = Page::Runs;

    // `u` opens the schedules dialog.
    app.key(key(Key::Char('u'))).unwrap();
    assert!(matches!(app.ui.dialog(), Some(Modal::Schedules(_))));
    let screen = rows(&mut app, 100, 32).join("\n");
    assert!(screen.contains("nightly-check"), "{screen}");
    assert!(screen.contains("needs approval"), "{screen}");
    assert!(screen.contains("untrusted"), "{screen}");

    // Untrusted project refuses approval (`a` stays on Schedules with trust error).
    app.key(key(Key::Char('a'))).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Schedules(view)) if view.error.contains("untrusted")
    ));

    // Trust the project config, reload with `r`, then approve with `a` + `y`.
    let loaded = crate::config::load_config(project.path(), None).unwrap();
    if let crate::trust::Trust::Untrusted(review) = loaded.trust {
        crate::trust::approve(&review).unwrap();
    }
    app.key(key(Key::Char('r'))).unwrap();
    app.key(key(Key::Char('a'))).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Ask(ask)) if matches!(**ask, Ask::ApproveSchedule { ref id, .. } if id == "nightly-check")
    ));
    // An agent edits the schedule while the question is open: confirming
    // refuses the unseen definition and shows the current one instead.
    schedules::update(project.path(), |store| {
        let schedule = &mut store.schedules[0];
        schedule.task = "Exfiltrate secrets".into();
        schedule.revision += 1;
        Ok(())
    })
    .unwrap();
    app.key(key(Key::Char('y'))).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Schedules(view)) if !view.rows[0].approved
            && view.rows[0].schedule.task == "Exfiltrate secrets"
            && view.error.contains("changed after it was shown")
    ));
    let settings = schedules::trusted_settings(project.path()).unwrap();
    let edited = schedules::load(project.path()).unwrap().schedules[0].clone();
    assert!(!schedules::approved(project.path(), &edited, &settings));
    // Reviewing the current definition and confirming approves it.
    app.key(key(Key::Char('a'))).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Ask(ask)) if matches!(**ask, Ask::ApproveSchedule { ref task, .. } if task == "Exfiltrate secrets")
    ));
    app.key(key(Key::Char('y'))).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Schedules(view)) if view.rows[0].approved
    ));

    // Pause (`p`) revokes approval; enable (`e`) re-enables requiring approval.
    app.key(key(Key::Char('p'))).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Schedules(view)) if !view.rows[0].schedule.enabled && !view.rows[0].approved
    ));
    app.key(key(Key::Char('e'))).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Schedules(view)) if view.rows[0].schedule.enabled && !view.rows[0].approved
    ));

    // Delete (`D` + `y`) removes it from schedules.json.
    app.key(shift('D')).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Ask(ask)) if matches!(**ask, Ask::DeleteSchedule { ref id } if id == "nightly-check")
    ));
    app.key(key(Key::Char('y'))).unwrap();
    assert!(matches!(
        app.ui.dialog(),
        Some(Modal::Schedules(view)) if view.rows.is_empty()
    ));
    assert!(
        schedules::load(project.path())
            .unwrap()
            .schedules
            .is_empty()
    );
}
