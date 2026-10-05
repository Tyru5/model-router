use model_router::{
    adapters::{self, ContextPaths, OutputKind},
    config::{BUILTINS, Mode, Role, Stage},
    handoff::{self, Verdict},
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/harness-output");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    fixture: Vec<Fixture>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    file: String,
    harness: String,
    kind: OutputKind,
    version: String,
    scenario: String,
    provenance: String,
    source: String,
    sanitization: String,
    #[serde(default)]
    text_contains: Vec<String>,
    #[serde(default)]
    text_excludes: Vec<String>,
    error_contains: Option<String>,
    #[serde(default)]
    permission_blocked: bool,
    handoff_summary: Option<String>,
    role: Option<Role>,
    verdict: Option<Verdict>,
}

fn manifest(root: &Path) -> Manifest {
    toml::from_str(&fs::read_to_string(root.join("manifest.toml")).unwrap()).unwrap()
}

/// Runs normalize (and parse for handoffs) and checks what the manifest declares.
fn check(root: &Path, f: &Fixture) {
    let name = &f.file;
    let raw = fs::read_to_string(root.join(name)).unwrap();
    let out = handoff::normalize(&raw, f.kind);
    match &f.error_contains {
        Some(expected) => assert!(
            out.error
                .as_deref()
                .is_some_and(|e| e.contains(expected.as_str())),
            "{name}: error {:?}, expected {expected:?}",
            out.error
        ),
        None => assert!(out.error.is_none(), "{name}: unexpected {:?}", out.error),
    }
    assert_eq!(out.permission_blocked, f.permission_blocked, "{name}");
    for text in &f.text_contains {
        assert!(out.text.contains(text.as_str()), "{name}: lacks {text:?}");
    }
    for text in &f.text_excludes {
        assert!(!out.text.contains(text.as_str()), "{name}: has {text:?}");
    }
    if let Some(summary) = &f.handoff_summary {
        let parsed = handoff::parse(&out.text, f.role.unwrap_or(Role::Implementation))
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(parsed.structured, "{name}");
        assert_eq!(&parsed.summary, summary, "{name}");
        if let Some(verdict) = f.verdict {
            assert_eq!(parsed.verdict, verdict, "{name}");
        }
    }
}

#[test]
fn every_fixture_normalizes_as_its_manifest_declares() {
    let root = Path::new(FIXTURES);
    let manifest = manifest(root);
    for fixture in &manifest.fixture {
        check(root, fixture);
    }
}

fn kind_of(harness: &str) -> OutputKind {
    let dir = tempfile::tempdir().unwrap();
    let paths = ContextPaths {
        workspace: dir.path(),
        attempt_dir: dir.path(),
        prompt_file: dir.path(),
        handoff_file: dir.path(),
    };
    let kinds: Vec<_> = [Mode::Unattended, Mode::Interactive]
        .into_iter()
        .map(|mode| {
            let stage = Stage {
                harness: harness.into(),
                model: "m".into(),
                mode,
                ..Stage::default()
            };
            adapters::build(&stage, &BTreeMap::new(), "task", &paths)
                .unwrap()
                .kind
        })
        .collect();
    assert_eq!(kinds[0], kinds[1], "{harness}");
    kinds[0]
}

#[test]
fn manifest_is_labeled_matches_adapters_and_covers_each_scenario() {
    let root = Path::new(FIXTURES);
    let manifest = manifest(root);
    let mut listed = BTreeSet::new();
    for f in &manifest.fixture {
        assert!(
            matches!(f.provenance.as_str(), "captured" | "schema-derived"),
            "{}: provenance {}",
            f.file,
            f.provenance
        );
        assert!(BUILTINS.contains(&f.harness.as_str()), "{}", f.file);
        assert!(f.file.starts_with(&format!("{}/", f.harness)), "{}", f.file);
        // *.log is ignored by Git.
        assert!(!f.file.ends_with(".log"), "{}", f.file);
        assert_eq!(
            f.kind,
            kind_of(&f.harness),
            "{}: kind differs from adapter",
            f.file
        );
        for field in [&f.version, &f.scenario, &f.source, &f.sanitization] {
            assert!(!field.trim().is_empty(), "{}: empty metadata", f.file);
        }
        assert!(listed.insert(f.file.clone()), "{} listed twice", f.file);
    }
    for harness in BUILTINS {
        let scenarios: BTreeSet<_> = manifest
            .fixture
            .iter()
            .filter(|f| f.harness == *harness)
            .map(|f| f.scenario.as_str())
            .collect();
        let required: &[&str] = match (kind_of(harness), *harness) {
            (OutputKind::Text, _) => &["transcript"],
            (_, "claude") => &["success", "error", "tool-echo", "permission-denial"],
            _ => &["success", "error", "tool-echo"],
        };
        for scenario in required {
            assert!(scenarios.contains(scenario), "{harness} lacks {scenario}");
        }
    }
    for dir in fs::read_dir(root).unwrap() {
        let dir = dir.unwrap().path();
        if dir.is_dir() {
            for file in fs::read_dir(&dir).unwrap() {
                let file = file.unwrap().path();
                let relative = file.strip_prefix(root).unwrap().to_str().unwrap();
                assert!(
                    listed.contains(relative),
                    "{relative} is not in the manifest"
                );
            }
        }
    }
}
