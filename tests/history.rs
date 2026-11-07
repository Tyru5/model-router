mod common;

use common::{drive, fixture, stage};
use model_router::{
    config::{Role, Settings},
    engine::{self, Action, Run, Status},
    history::{self, Criteria, Entry, StatusFilter},
    storage,
};
use std::{fs, path::Path, process::Command};

fn create(dir: &Path, settings: &Settings, task: &str) -> Run {
    Run::create(dir, settings, &settings.workflows[0], task.into(), false).unwrap()
}

fn summary(entry: &Entry) -> &history::RunSummary {
    match entry {
        Entry::Summary(summary) => summary,
        Entry::Damaged { error, .. } => panic!("damaged: {error}"),
    }
}

fn cli(dir: &Path, settings: &Settings, args: &[&str]) -> (bool, String) {
    let config = dir.join("config.toml");
    settings.save(&config).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir)
        .arg("--config")
        .arg(&config)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

#[test]
fn listing_reads_summaries_without_run_json() {
    let (dir, _, mut run) = fixture(vec![stage("research", Role::Research)]);
    assert_eq!(drive(&mut run), Action::Complete);
    let saved: history::RunSummary =
        storage::read_json(&run.directory.join("summary.json")).unwrap();
    let run_bytes = fs::metadata(run.directory.join("run.json")).unwrap().len();
    assert_eq!(saved.run_bytes, Some(run_bytes));
    assert_eq!(saved.revision, run.revision);
    let listed = history::RunSummary {
        run_bytes: None,
        ..saved.clone()
    };
    assert_eq!(listed, history::RunSummary::from(&run));
    assert_eq!(saved.status, Status::Complete);
    assert_eq!((saved.enabled_stages, saved.complete_stages), (1, 1));
    assert_eq!(saved.attempts, 1);
    assert!(saved.task.starts_with("Original task"));
    // Old runs without summary.json still list from run.json.
    fs::remove_file(run.directory.join("summary.json")).unwrap();
    let entries = history::list(dir.path()).unwrap();
    assert_eq!(summary(&entries[0]), &listed);
    assert!(
        !run.directory.join("summary.json").exists(),
        "listing wrote"
    );
    // With a summary, run.json is only checked for the size it recorded.
    run.persist().unwrap();
    let entries = history::list(dir.path()).unwrap();
    assert_eq!(summary(&entries[0]).revision, run.revision);
    // A run.json that broke after its summary was saved lists as damaged.
    fs::write(run.directory.join("run.json"), "{").unwrap();
    let entries = history::list(dir.path()).unwrap();
    assert!(matches!(&entries[0], Entry::Damaged { id, .. } if *id == run.id));
    // Loading on demand reports the broken record.
    assert!(engine::load(dir.path(), &run.id).is_err());
    assert!(engine::history(dir.path()).unwrap().is_empty());
}

#[test]
fn damaged_records_are_listed() {
    let (dir, settings, run) = fixture(vec![stage("research", Role::Research)]);
    let runs = dir.path().join(".model-router/runs");
    let json = fs::read_to_string(run.directory.join("run.json")).unwrap();
    let damage = |id: &str, run_json: Option<&str>| {
        fs::create_dir_all(runs.join(id)).unwrap();
        if let Some(text) = run_json {
            fs::write(runs.join(id).join("run.json"), text).unwrap();
        }
    };
    damage("20200101-000000-corrupt0", Some("{not json"));
    damage(
        "20200101-000001-invalid0",
        Some(&json.replacen("\"version\": 1", "\"version\": 9", 1)),
    );
    damage("20200101-000002-missing0", None);
    // A corrupt summary falls back to a valid run.json.
    let healthy = create(dir.path(), &settings, "Healthy");
    fs::write(healthy.directory.join("summary.json"), "[").unwrap();

    let entries = history::list(dir.path()).unwrap();
    assert_eq!(entries.len(), 5);
    let damaged: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Damaged { id, path, error } => Some((id.as_str(), path, error.as_str())),
            Entry::Summary(_) => None,
        })
        .collect();
    assert_eq!(
        damaged.iter().map(|d| d.0).collect::<Vec<_>>(),
        [
            "20200101-000002-missing0",
            "20200101-000001-invalid0",
            "20200101-000000-corrupt0"
        ]
    );
    assert_eq!(damaged[0].1, &runs.join("20200101-000002-missing0"));
    assert!(damaged[1].2.contains("Invalid or unsupported"));
    assert!(
        entries
            .iter()
            .any(|e| e.id() == healthy.id && e.status_label() == "pending")
    );

    let (ok, out) = cli(dir.path(), &settings, &["history"]);
    assert!(ok, "{out}");
    assert!(out.contains("DAMAGED 20200101-000000-corrupt0"), "{out}");
    assert!(out.contains(&healthy.id));
    let (ok, out) = cli(
        dir.path(),
        &settings,
        &["history", "--status", "damaged", "--json"],
    );
    assert!(ok, "{out}");
    let values: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(values.len(), 3);
    assert!(values.iter().all(|v| v["status"] == "damaged"));
}

#[test]
fn filters_match_status_and_words() {
    let mut fail = stage("research", Role::Research);
    fail.model = "fail".into();
    let (dir, settings, mut failed) = fixture(vec![fail]);
    assert_eq!(drive(&mut failed), Action::Stopped);
    let pending = create(dir.path(), &settings, "Write the docs\nsecond line");
    fs::create_dir_all(dir.path().join(".model-router/runs/broken")).unwrap();
    let entries = history::list(dir.path()).unwrap();
    let ids = |statuses: &[StatusFilter], search: &str| -> Vec<String> {
        history::filter(&entries, statuses, search)
            .iter()
            .map(|e| e.id().to_owned())
            .collect()
    };
    let parse = |name: &str| name.parse::<StatusFilter>().unwrap();
    assert_eq!(ids(&[], "").len(), 3);
    assert_eq!(ids(&[parse("failed")], ""), [failed.id.as_str()]);
    assert_eq!(ids(&[parse("active")], ""), [pending.id.as_str()]);
    assert_eq!(ids(&[parse("damaged")], ""), ["broken"]);
    assert_eq!(ids(&[parse("Failed"), parse("damaged")], "").len(), 2);
    assert_eq!(ids(&[], "DOCS write"), [pending.id.as_str()]);
    assert!(
        ids(&[], "second").is_empty(),
        "only the first line is indexed"
    );
    assert_eq!(ids(&[], "test failed"), [failed.id.as_str()]);
    assert_eq!(ids(&[], &failed.id[16..]), [failed.id.as_str()]);
    assert!("unknown".parse::<StatusFilter>().is_err());

    let (ok, out) = cli(
        dir.path(),
        &settings,
        &[
            "history",
            "--status",
            "pending,failed",
            "--search",
            "docs",
            "--sizes",
        ],
    );
    assert!(ok, "{out}");
    assert!(
        out.contains(&pending.id) && !out.contains(&failed.id),
        "{out}"
    );
    assert!(out.contains("Total"), "{out}");
    let (_, out) = cli(dir.path(), &settings, &["history", "--limit", "1"]);
    assert_eq!(out.lines().count(), 1, "{out}");
}

#[test]
fn rerun_creates_a_new_run_from_the_snapshot() {
    let dir = common::tempdir();
    let workflow = common::workflow(vec![stage("research", Role::Research)]);
    let mut settings = common::settings(&workflow);
    // Provider definitions also come from the snapshot, not current settings.
    settings.providers.insert(
        "snapshot-proxy".into(),
        model_router::providers::Provider {
            base_url: "http://localhost:4000/v1".into(),
            ..Default::default()
        },
    );
    let mut source = common::create(dir.path(), &settings, &workflow);
    assert_eq!(drive(&mut source), Action::Complete);
    let before = fs::read(source.directory.join("run.json")).unwrap();
    // The saved adapter snapshot supplies the mock harness.
    let bare = Settings {
        adapters: Default::default(),
        ..settings.clone()
    };
    let mut rerun = history::rerun(dir.path(), &bare, &source.id, None).unwrap();
    assert_ne!(rerun.id, source.id);
    assert_eq!(rerun.providers, source.providers);
    assert_eq!(rerun.task, source.task);
    assert_eq!(rerun.rerun_of.as_deref(), Some(source.id.as_str()));
    assert_eq!(
        serde_json::to_value(&rerun.workflow).unwrap(),
        serde_json::to_value(&source.workflow).unwrap()
    );
    assert_eq!(rerun.status, Status::Pending);
    let loaded = engine::load(dir.path(), &rerun.id).unwrap();
    assert_eq!(loaded.rerun_of, rerun.rerun_of);
    assert_eq!(drive(&mut rerun), Action::Complete);
    assert_eq!(fs::read(source.directory.join("run.json")).unwrap(), before);
    let entries = history::list(dir.path()).unwrap();
    assert_eq!(
        summary(&entries[0]).rerun_of.as_deref(),
        Some(source.id.as_str())
    );
    assert!(history::rerun(dir.path(), &bare, "missing", None).is_err());

    let (ok, out) = cli(
        dir.path(),
        &settings,
        &["rerun", &source.id, "--unattended"],
    );
    assert!(ok, "{out}");
    assert!(out.contains(&format!("Rerun of: {}", source.id)), "{out}");
    assert!(out.contains("Completed"), "{out}");
    assert_eq!(history::list(dir.path()).unwrap().len(), 3);
}

#[test]
fn compare_two_attempts_of_a_stage() {
    let mut research = stage("research", Role::Research);
    research.model = "fail-once".into();
    let (dir, settings, mut run) = fixture(vec![research]);
    assert_eq!(drive(&mut run), Action::Stopped);
    run.retry(false, None).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(history::retried_stage(&run), Some(0));

    let comparison = history::compare(&run, 0, 1, 2).unwrap();
    assert_eq!(comparison.stage, "research");
    assert_eq!(comparison.a.attempt.status, Status::Failed);
    assert_eq!(comparison.b.attempt.status, Status::Complete);
    assert!(comparison.a.handoff.is_none());
    assert!(comparison.b.handoff.is_some());
    let text = comparison.text();
    assert!(text.contains("* status        failed → complete"), "{text}");
    assert!(text.contains("* exit code     17 → 0"), "{text}");
    assert!(text.contains("  model         fail-once"), "{text}");
    assert!(
        text.contains("* handoff       missing → structured"),
        "{text}"
    );
    assert!(history::compare(&run, 0, 1, 3).is_err());
    assert!(history::compare(&run, 4, 1, 2).is_err());

    let (ok, out) = cli(dir.path(), &settings, &["compare", &run.id]);
    assert!(ok, "{out}");
    assert!(out.contains("research · attempt 1 vs attempt 2"), "{out}");
    let (ok, out) = cli(
        dir.path(),
        &settings,
        &[
            "compare",
            &run.id,
            "--stage",
            "research",
            "--attempts",
            "2",
            "1",
        ],
    );
    assert!(ok, "{out}");
    assert!(out.contains("complete → failed"), "{out}");
    let (ok, out) = cli(
        dir.path(),
        &settings,
        &["compare", &run.id, "--stage", "nope"],
    );
    assert!(!ok && out.contains("no stage nope"), "{out}");
}

#[test]
fn compare_across_a_rerun() {
    let mut research = stage("research", Role::Research);
    research.model = "fail-once".into();
    let (dir, settings, mut source) = fixture(vec![research]);
    assert_eq!(drive(&mut source), Action::Stopped);
    let mut rerun = history::rerun(dir.path(), &settings, &source.id, None).unwrap();
    assert_eq!(drive(&mut rerun), Action::Complete);
    let comparison = history::compare_rerun(&source, &rerun, "research").unwrap();
    assert_eq!(comparison.a.label, "source attempt 1");
    assert_eq!(comparison.b.run, rerun.id);
    assert!(history::compare_rerun(&rerun, &source, "research").is_err());
}

#[test]
fn size_does_not_follow_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("a"), "12345").unwrap();
    fs::write(root.join("nested/b"), "1234567").unwrap();
    fs::write(dir.path().join("big"), vec![0; 100_000]).unwrap();
    let mut expected = 12;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("big"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.path(), root.join("loop")).unwrap();
        expected += fs::symlink_metadata(root.join("link")).unwrap().len()
            + fs::symlink_metadata(root.join("loop")).unwrap().len();
    }
    assert_eq!(history::size(&root), expected);
    assert_eq!(history::size(&root.join("a")), 5);
    assert_eq!(history::size(&dir.path().join("missing")), 0);
    assert_eq!(history::human(512), "512 B");
    assert_eq!(history::human(4300), "4.2 KiB");
    assert_eq!(history::human(13 * 1024 * 1024), "13 MiB");
}

#[test]
fn cleanup_selects_and_removes_only_the_plan() {
    let mut fail = stage("research", Role::Research);
    fail.model = "fail".into();
    let (dir, settings, mut failed) = fixture(vec![fail]);
    assert_eq!(drive(&mut failed), Action::Stopped);
    let locked = create(dir.path(), &settings, "Locked");
    let newest = create(dir.path(), &settings, "Newest");
    let handoffs = dir.path().join(".model-router/handoffs").join(&failed.id);
    assert!(handoffs.is_dir());

    let everything = history::cleanup_plan(dir.path(), &Criteria::default()).unwrap();
    assert_eq!(everything.items.len(), 3);
    let kept = history::cleanup_plan(
        dir.path(),
        &Criteria {
            keep: 1,
            ..Criteria::default()
        },
    )
    .unwrap();
    assert!(kept.items.iter().all(|item| item.id != newest.id));
    let only_failed = Criteria {
        statuses: vec![StatusFilter::Is(Status::Failed)],
        ..Criteria::default()
    };
    let plan = history::cleanup_plan(dir.path(), &only_failed).unwrap();
    assert_eq!(plan.items.len(), 1);
    // Run creation canonicalizes the project; cleanup preserves its supplied path.
    // On macOS, the temporary directory can be under the /var -> /private/var symlink.
    assert_eq!(
        fs::canonicalize(&plan.items[0].paths[0].0).unwrap(),
        failed.directory
    );
    assert_eq!(plan.items[0].paths[1].0, handoffs);
    assert!(plan.bytes() > 0);
    let recent = Criteria {
        older_than_days: Some(1),
        ..Criteria::default()
    };
    assert!(
        history::cleanup_plan(dir.path(), &recent)
            .unwrap()
            .items
            .is_empty()
    );

    // A held run.lock skips the run, at planning and at removal.
    let lock = storage::lock(&locked.directory.join("run.lock")).unwrap();
    let plan = history::cleanup_plan(
        dir.path(),
        &Criteria {
            keep: 1,
            ..Criteria::default()
        },
    )
    .unwrap();
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.skipped[0].0, locked.id);
    drop(lock);
    let plan = history::cleanup_plan(
        dir.path(),
        &Criteria {
            keep: 1,
            ..Criteria::default()
        },
    )
    .unwrap();
    assert_eq!(plan.items.len(), 2);
    let lock = storage::lock(&locked.directory.join("run.lock")).unwrap();
    let done = history::apply(&plan);
    drop(lock);
    assert_eq!(done.items.len(), 1);
    assert_eq!(done.items[0].id, failed.id);
    assert_eq!(done.skipped[0].0, locked.id);
    assert!(!failed.directory.exists() && !handoffs.exists());
    assert!(locked.directory.exists() && newest.directory.exists());
    assert!(done.text(true).starts_with("Removed 1 run,"));

    let protected = history::cleanup_plan(
        dir.path(),
        &Criteria {
            protect: vec![newest.id.clone()],
            ids: vec![newest.id.clone()],
            ..Criteria::default()
        },
    )
    .unwrap();
    assert!(protected.items.is_empty());
    assert_eq!(protected.skipped[0].0, newest.id);
}

#[test]
fn cli_clean_previews_then_removes() {
    let (dir, settings, run) = fixture(vec![stage("research", Role::Research)]);
    let newest = create(dir.path(), &settings, "Newest");
    let (ok, out) = cli(dir.path(), &settings, &["clean", "--keep", "1"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("Would remove 1 run") && out.contains(&run.id),
        "{out}"
    );
    assert!(out.contains("Add --yes"), "{out}");
    assert!(run.directory.exists());
    let (ok, out) = cli(dir.path(), &settings, &["clean", "--status", "complete"]);
    assert!(ok && out.contains("Nothing to clean."), "{out}");
    let (ok, out) = cli(dir.path(), &settings, &["clean", "--keep", "1", "--yes"]);
    assert!(ok, "{out}");
    assert!(out.contains("Removed 1 run"), "{out}");
    assert!(!run.directory.exists() && newest.directory.exists());
}
