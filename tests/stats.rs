mod common;

use common::{drive, fixture, stage};
use model_router::{
    config::{Role, Settings},
    engine::{Action, Run},
    stats::{self, Stats},
};
use std::{fs, path::Path, process::Command};

fn implement_and_review(fail_until: &str) -> Vec<model_router::config::Stage> {
    let mut review = stage("review", Role::Review);
    review.extra_args = vec!["--review-fail-until".into(), fail_until.into()];
    vec![stage("implement", Role::Implementation), review]
}

fn run_with_model(dir: &Path, settings: &Settings, model: &str) -> Run {
    let mut workflow = settings.workflows[0].clone();
    workflow.stages[0].model = model.into();
    let mut run = Run::create(dir, settings, &workflow, "Task".into(), false).unwrap();
    drive(&mut run);
    run
}

fn find<'a>(samples: &'a [stats::Sample], run: &str, stage: &str) -> &'a stats::Sample {
    samples
        .iter()
        .find(|s| s.run == run && s.stage == stage)
        .unwrap_or_else(|| panic!("no sample for {run} {stage}: {samples:#?}"))
}

#[test]
fn samples_attribute_repairs_retries_and_first_pass() {
    let (dir, settings, mut run) = fixture(implement_and_review("1"));
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(run.repair_passes, 1);
    let samples = stats::samples(&run);
    let implement = find(&samples, &run.id, "implement");
    assert_eq!(
        (
            implement.attempts,
            implement.repairs_caused,
            implement.first_pass
        ),
        (2, 1, Some(false))
    );
    assert!(implement.clean, "{implement:?}");
    assert_eq!(implement.model, "test-model");
    let review = find(&samples, &run.id, "review");
    assert_eq!(
        (review.attempts, review.repair_requests, review.first_pass),
        (2, 1, Some(false))
    );
    // A review that passes at once, after an implementation that needed no repair.
    let mut settings = settings;
    settings.workflows[0].stages[1].extra_args.clear();
    let passing = run_with_model(dir.path(), &settings, "test-model");
    let samples = stats::samples(&passing);
    assert_eq!(
        find(&samples, &passing.id, "implement").first_pass,
        Some(true)
    );
    assert_eq!(find(&samples, &passing.id, "review").first_pass, Some(true));
    // A failed then retried implementation counts the retry and the failure.
    fs::remove_dir_all(dir.path().join(".model-router/mock")).unwrap();
    let mut flaky = run_with_model(dir.path(), &settings, "fail-once");
    flaky.retry(false, None).unwrap();
    drive(&mut flaky);
    let samples = stats::samples(&flaky);
    let implement = find(&samples, &flaky.id, "implement");
    assert_eq!((implement.retries, implement.failed), (1, 1));
    assert!(!implement.clean);
}

#[test]
fn collect_caches_by_revision_and_skips_damaged_records() {
    let (dir, settings, mut first) = fixture(implement_and_review("0"));
    drive(&mut first);
    let failed = run_with_model(dir.path(), &settings, "fail");
    fs::create_dir_all(
        dir.path()
            .join(".model-router/runs/20250101-000000-deadbeef"),
    )
    .unwrap();
    fs::write(
        dir.path()
            .join(".model-router/runs/20250101-000000-deadbeef/run.json"),
        "{",
    )
    .unwrap();
    let found = stats::collect(dir.path());
    assert_eq!((found.runs, found.damaged, found.loaded), (2, 1, 2));
    let failure = find(&found.samples, &failed.id, "implement");
    assert_eq!((failure.clean, failure.failed), (false, 1));
    // The review never ran in the failed run, so it gives no sample.
    assert!(
        !found
            .samples
            .iter()
            .any(|s| s.run == failed.id && s.stage == "review")
    );
    let cache = stats::cache_path(dir.path());
    assert!(cache.is_file());
    // Unchanged runs come from the cache.
    let again = stats::collect(dir.path());
    assert_eq!(again.loaded, 0);
    assert_eq!(again.samples, found.samples);
    let mut text = fs::read_to_string(&cache).unwrap();
    text = text.replace("\"test-model\"", "\"cached-model\"");
    fs::write(&cache, text).unwrap();
    let cached = stats::collect(dir.path());
    assert_eq!(
        find(&cached.samples, &first.id, "implement").model,
        "cached-model"
    );
    assert_eq!(
        find(&cached.samples, &first.id, "review").model,
        "cached-model"
    );
    // A saved change reloads that run only.
    first.persist().unwrap();
    let after = stats::collect(dir.path());
    assert_eq!(after.loaded, 1);
    assert_eq!(
        find(&after.samples, &first.id, "implement").model,
        "test-model"
    );
    // A corrupt cache is rebuilt.
    fs::write(&cache, "not json").unwrap();
    assert_eq!(stats::collect(dir.path()).loaded, 2);

    let stats = Stats::collect(&[dir.path().to_path_buf()]);
    let metrics = stats
        .find(Role::Implementation, "mock", "test-model")
        .unwrap();
    assert_eq!((metrics.samples, metrics.clean), (1, 1));
    let failing = stats.find(Role::Implementation, "mock", "fail").unwrap();
    assert_eq!((failing.samples, failing.clean, failing.failed), (1, 0, 1));
    // Too few samples for any suggestion.
    assert!(stats::suggest(&stats, Role::Implementation, "mock", "fail").is_none());
}

#[test]
fn suggestion_needs_enough_history() {
    let (dir, settings, mut run) = fixture(vec![stage("implement", Role::Implementation)]);
    drive(&mut run);
    for _ in 0..5 {
        run_with_model(dir.path(), &settings, "fail");
    }
    for _ in 0..4 {
        run_with_model(dir.path(), &settings, "test-model");
    }
    let stats = Stats::collect(&[dir.path().to_path_buf()]);
    let suggestion = stats::suggest(&stats, Role::Implementation, "mock", "fail").unwrap();
    assert_eq!(
        suggestion.text(),
        "History: mock/test-model succeeded 5/5 here vs 0/5"
    );
    assert!(stats::suggest(&stats, Role::Implementation, "mock", "test-model").is_none());
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
fn stats_command_prints_ranked_tables_and_json() {
    let (dir, settings, mut run) = fixture(implement_and_review("0"));
    drive(&mut run);
    run_with_model(dir.path(), &settings, "fail");
    let (ok, text) = cli(dir.path(), &settings, &["stats"]);
    assert!(ok, "{text}");
    assert!(text.contains("2 run(s) in 1 project(s)"), "{text}");
    assert!(text.contains("IMPLEMENTATION"), "{text}");
    assert!(text.contains("REVIEW"), "{text}");
    assert!(text.contains("mock/test-model *"), "{text}");
    assert!(text.contains("fewer than 5 samples"), "{text}");
    let implementation = text.find("IMPLEMENTATION").unwrap();
    let first = text[implementation..].find("mock/test-model").unwrap();
    let second = text[implementation..].find("mock/fail").unwrap();
    assert!(first < second, "best first: {text}");

    let (ok, text) = cli(dir.path(), &settings, &["stats", "--role", "review"]);
    assert!(ok, "{text}");
    assert!(!text.contains("IMPLEMENTATION"), "{text}");
    let (ok, text) = cli(
        dir.path(),
        &settings,
        &["stats", "--json", "--role", "implementation"],
    );
    assert!(ok, "{text}");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["runs"], 2);
    let metrics = value["metrics"].as_array().unwrap();
    assert_eq!(metrics.len(), 2);
    assert!(metrics.iter().all(|m| m["role"] == "implementation"));
    let (ok, text) = cli(dir.path(), &settings, &["stats", "--role", "nonsense"]);
    assert!(!ok && text.contains("expected one of"), "{text}");
}
