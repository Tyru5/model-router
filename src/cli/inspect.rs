use anyhow::{Context, Result, bail, ensure};
use model_router::{
    adapters, catalog,
    config::{Role, Settings},
    cost, doctor,
    engine::{self, Run},
    harness,
    history::{self, Entry, StatusFilter},
    providers, report,
};
use std::{
    io::Write,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

/// The run's verification receipts, after a blank line; nothing without any.
pub fn print_receipts(run: &Run) {
    let text = report::render_receipts(&report::receipts(run));
    if !text.is_empty() {
        print!("\n{text}");
    }
}

pub fn report_command(
    project: &Path,
    id: &str,
    json: bool,
    diff: bool,
    transitions: bool,
) -> Result<()> {
    let run = engine::load(project, id)?;
    cost::refresh_provider_prices(project, &cost::routed_providers(&run));
    let timeline = transitions.then(|| report::transitions(&run)).transpose()?;
    if diff {
        print!("{}", report::diff(&run)?);
    } else if json {
        let mut value = serde_json::to_value(report::build(&run))?;
        value["changes"] = serde_json::to_value(report::changes(&run))?;
        value["receipts"] = serde_json::to_value(report::receipts(&run))?;
        if let Some(timeline) = &timeline {
            value["transitions"] = serde_json::to_value(timeline)?;
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if let Some(timeline) = &timeline {
        print!("{}", report::render_transitions(&run, timeline));
    } else {
        println!(
            "Run {} · {}\nArtifacts: {}\n",
            run.id,
            run.workflow.name,
            run.directory.display()
        );
        print!(
            "{}",
            report::render(&report::build(&run), Some(&report::changes(&run)))
        );
        print_receipts(&run);
    }
    Ok(())
}

pub fn doctor_command(project: &Path, settings: &Settings, json: bool, strict: bool) -> Result<()> {
    let reports = doctor::inspect_all(settings, project);
    if json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        print!("{}", doctor::table(&reports));
        println!("\n{}", doctor::sandbox_line());
        println!("{}", doctor::tools_line(&doctor::inspect_tools(project)));
        println!("\n{}", doctor::AUTH_NOTE);
    }
    ensure!(
        !strict || !reports.iter().any(doctor::Report::missing_flags),
        "An installed harness lacks a flag the router passes"
    );
    Ok(())
}

pub fn stats_command(
    project: &Path,
    role: Option<Role>,
    json: bool,
    all: bool,
    min_pass: Option<f64>,
) -> Result<()> {
    use model_router::stats::{self, Stats};
    let stats = Stats::collect(&stats::projects(project, all));
    let shortlist = min_pass.map(|threshold| {
        let rows: Vec<stats::Tradeoff> = stats
            .metrics
            .iter()
            .filter(|m| role.is_none_or(|r| m.role == r))
            .map(stats::Tradeoff::from)
            .collect();
        stats::shortlist(&rows, threshold)
    });
    if json {
        let mut value = serde_json::to_value(&stats)?;
        if let Some(role) = role {
            value["metrics"] = serde_json::to_value(
                stats
                    .metrics
                    .iter()
                    .filter(|m| m.role == role)
                    .collect::<Vec<_>>(),
            )?;
        }
        if let Some(shortlist) = &shortlist {
            value["shortlist"] = serde_json::to_value(shortlist)?;
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", stats.table(role));
        if let Some(shortlist) = &shortlist {
            print!("\n{}", shortlist.text());
        }
    }
    Ok(())
}

pub fn bench_command(
    project: &Path,
    settings: &Settings,
    file: &Path,
    options: &model_router::bench::Options,
    dry_run: bool,
    min_pass: Option<f64>,
    cancel: &AtomicBool,
) -> Result<()> {
    use model_router::bench;
    let plan = bench::Plan::new(file, &bench::load(file)?, settings, options)?;
    print!("{}", plan.preview());
    if dry_run {
        println!("\nDry run: nothing started.");
        return Ok(());
    }
    println!();
    let results = bench::run(project, settings, &plan, options, cancel, &|line| {
        eprintln!("{line}")
    })?;
    let (json, markdown) = bench::save(project, &results)?;
    println!("{}", bench::markdown(&results));
    if let Some(threshold) = min_pass {
        let text = bench::shortlist(&results, threshold).text();
        println!("{text}");
        let mut file = std::fs::OpenOptions::new().append(true).open(&markdown)?;
        write!(file, "\n```text\n{text}```\n")?;
    }
    println!("Saved {}\nSaved {}", json.display(), markdown.display());
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "Bench cancelled; unfinished cells are marked skipped"
    );
    Ok(())
}

/// `harnesses`: a table, or with `--spec` the resolved specs as JSON.
pub fn harnesses_command(settings: &Settings, spec: Option<&str>) -> Result<()> {
    let names = catalog::known_harnesses(settings);
    let resolved = |name: &str| -> Result<serde_json::Value> {
        if let Some(spec) = harness::get(name) {
            let mut value = serde_json::to_value(spec)?;
            value["builtin"] = true.into();
            value["program"] = adapters::program(name, &settings.adapters).into();
            return Ok(value);
        }
        let adapter = settings
            .adapters
            .get(name)
            .with_context(|| format!("Unknown harness {name}; see `model-router harnesses`"))?;
        let mut value = serde_json::to_value(adapter)?;
        value["name"] = name.into();
        value["builtin"] = false.into();
        value["output"] = adapters::output_kind(name, &settings.adapters)
            .name()
            .into();
        value["resumable"] = adapters::resumable(name, &settings.adapters).into();
        Ok(value)
    };
    match spec {
        Some("") => {
            let all = names
                .iter()
                .map(|name| resolved(name))
                .collect::<Result<Vec<_>>>()?;
            println!("{}", serde_json::to_string_pretty(&all)?);
        }
        Some(name) => println!("{}", serde_json::to_string_pretty(&resolved(name)?)?),
        None => {
            println!("HARNESS\tKIND\tPROGRAM\tOUTPUT\tRESUME");
            for name in &names {
                println!(
                    "{name}\t{}\t{}\t{}\t{}",
                    if harness::get(name).is_some() {
                        "built-in"
                    } else {
                        "custom"
                    },
                    adapters::program(name, &settings.adapters),
                    adapters::output_kind(name, &settings.adapters).name(),
                    if adapters::resumable(name, &settings.adapters) {
                        "yes"
                    } else {
                        "no"
                    }
                );
            }
        }
    }
    Ok(())
}

pub fn models_command(project: &Path, settings: &Settings, harness: &str) -> Result<()> {
    let catalog = catalog::discover(harness, settings, project)?;
    for model in catalog.models {
        println!(
            "{}{}{}\t{}\t{}\t{}",
            if model.provider.is_empty() {
                ""
            } else {
                &model.provider
            },
            if model.provider.is_empty() { "" } else { "/" },
            if model.id.is_empty() {
                "(default)"
            } else {
                &model.id
            },
            model.source,
            model.availability,
            model.efforts.join("/")
        );
    }
    if !catalog.efforts.is_empty() {
        println!(
            "\nEffort levels ({}): {}",
            catalog.effort_source,
            catalog.efforts.join(", ")
        );
    }
    println!("\n{}", catalog.note);
    Ok(())
}

pub fn providers_command(settings: &Settings, name: Option<&str>) -> Result<()> {
    match name {
        Some(name) => {
            let provider = providers::resolve(name, &settings.providers).with_context(|| {
                format!("Unknown provider {name}; see `model-router providers`")
            })?;
            for model in providers::models(&provider)? {
                let mut line = model.id.clone();
                if let Some(context) = model.context_length {
                    line.push_str(&format!("\tcontext {context}"));
                }
                if let (Some(prompt), Some(completion)) =
                    (model.prompt_price, model.completion_price)
                {
                    line.push_str(&format!(
                        "\t${:.2}/${:.2} per M tokens",
                        prompt * 1e6,
                        completion * 1e6
                    ));
                }
                println!("{line}");
            }
        }
        None => {
            println!("PROVIDER\tSOURCE\tKEY\tBASE URL");
            for (name, provider) in providers::all(&settings.providers) {
                let key = if provider.api_key_env.is_empty() {
                    "none needed".into()
                } else {
                    let set =
                        std::env::var_os(&provider.api_key_env).is_some_and(|v| !v.is_empty());
                    format!(
                        "{} {}",
                        provider.api_key_env,
                        if set { "set" } else { "unset" }
                    )
                };
                println!(
                    "{name}\t{}\t{key}\t{}",
                    if settings.providers.contains_key(&name) {
                        "configured"
                    } else {
                        "built-in"
                    },
                    provider.base_url
                );
            }
            println!("\n{}", providers::note());
        }
    }
    Ok(())
}

pub fn history_command(
    project: &Path,
    status: Vec<StatusFilter>,
    search: Option<&str>,
    limit: Option<usize>,
    json: bool,
    sizes: bool,
) -> Result<()> {
    let entries = history::list(project)?;
    let mut shown = history::filter(&entries, &status, search.unwrap_or(""));
    shown.truncate(limit.unwrap_or(usize::MAX));
    let size = |entry: &Entry| sizes.then(|| history::run_size(project, entry));
    if json {
        let mut values = vec![];
        for entry in &shown {
            let mut value = match entry {
                Entry::Summary(summary) => serde_json::to_value(summary)?,
                Entry::Damaged { id, path, error } => serde_json::json!({
                    "id": id, "status": "damaged", "path": path, "error": error
                }),
            };
            if let Some(bytes) = size(entry) {
                value["size"] = bytes.into();
            }
            values.push(value);
        }
        println!("{}", serde_json::to_string_pretty(&values)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("No saved runs in {}", project.display());
    } else if shown.is_empty() {
        println!("No runs match ({} saved)", entries.len());
    }
    let mut total = 0;
    for entry in &shown {
        let bytes = size(entry).map_or_else(String::new, |bytes| {
            total += bytes;
            format!("{:>8}  ", history::human(bytes))
        });
        match entry {
            Entry::Summary(run) => println!(
                "{}  {:18} {bytes}{}  {}",
                run.id,
                run.status.label(),
                run.workflow,
                run.task
            ),
            Entry::Damaged { id, error, .. } => println!("DAMAGED {id}  {bytes}{error}"),
        }
    }
    if sizes && !shown.is_empty() {
        println!("Total {} in {} runs", history::human(total), shown.len());
    }
    Ok(())
}

pub fn cost_command(project: &Path, since: Option<&str>, json: bool) -> Result<()> {
    let since = since
        .map(cost::parse_since)
        .transpose()?
        .map(|age| chrono::Utc::now() - age);
    let runs = engine::history(project)?;
    let mut routed = std::collections::BTreeMap::new();
    for run in &runs {
        routed.extend(cost::routed_providers(run));
    }
    cost::refresh_provider_prices(project, &routed);
    let summary = cost::aggregate(&runs, since, &cost::Pricing::load(project));
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print!("{}", cost::render_summary(&summary));
    }
    Ok(())
}

pub fn compare_command(
    project: &Path,
    id: &str,
    stage: Option<&str>,
    attempts: &[usize],
    source: bool,
) -> Result<()> {
    let run = engine::load(project, id)?;
    let index = match stage {
        Some(stage) => run
            .workflow
            .stages
            .iter()
            .position(|s| s.id == stage)
            .with_context(|| format!("Run {id} has no stage {stage}"))?,
        None if source => run
            .stages
            .iter()
            .rposition(|s| !s.attempts.is_empty())
            .context("This run has no attempts yet")?,
        None => history::retried_stage(&run)
            .context("No stage in this run has two attempts to compare")?,
    };
    let comparison = if source {
        let origin = history::source_of(&run).context("This run is not a rerun or fork")?;
        let origin = engine::load(project, origin)?;
        history::compare_rerun(&origin, &run, &run.workflow.stages[index].id)?
    } else {
        let numbers: Vec<_> = run.stages[index]
            .attempts
            .iter()
            .map(|a| a.number)
            .collect();
        let (a, b) = match (attempts, numbers.as_slice()) {
            ([a, b], _) | ([], [.., a, b]) => (*a, *b),
            _ => bail!(
                "Stage {} has {} attempt(s); give two with --attempts A B",
                run.workflow.stages[index].id,
                numbers.len()
            ),
        };
        history::compare(&run, index, a, b)?
    };
    print!("{}", comparison.text());
    Ok(())
}

pub fn handoffs_command(
    project: &Path,
    id: &str,
    stage: Option<&str>,
    prompt: bool,
    json: bool,
) -> Result<()> {
    let run = engine::load(project, id)?;
    let inspection = engine::inspect_handoffs(&run, stage)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&inspection)?);
    } else {
        print!("{}", inspection.text());
        if prompt {
            println!("\n----- prompt -----\n{}", inspection.prompt);
        }
    }
    Ok(())
}
