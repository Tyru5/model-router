//! Cost estimates and budget caps.
//!
//! A harness's own reported cost wins; otherwise the user's pricing
//! overrides, then a router provider's `/models` prices (cached daily), then
//! the built-in table (`cost/prices.toml`, estimates). Every amount carries
//! its source. Tokens without a price stay unknown and count only toward
//! token caps. Nothing here makes a network request except
//! `refresh_provider_prices`.
use crate::{
    config::Budget,
    engine::{Attempt, Run, Status},
    handoff::Usage,
    providers::{self, ModelInfo, Provider},
    report, storage,
};
use anyhow::{Result, bail, ensure};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

const PRICES: &str = include_str!("cost/prices.toml");

/// USD per million tokens. Cache prices default to 0.1× (read) and 1.25×
/// (write) the input price when unset.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}
impl Price {
    pub fn cost(&self, usage: &Usage) -> f64 {
        let part = |tokens: Option<u64>, price: f64| tokens.unwrap_or(0) as f64 * price / 1e6;
        part(usage.input_tokens, self.input)
            + part(usage.output_tokens, self.output)
            + part(
                usage.cache_read_tokens,
                self.cache_read.unwrap_or(self.input * 0.1),
            )
            + part(
                usage.cache_write_tokens,
                self.cache_write.unwrap_or(self.input * 1.25),
            )
    }
    pub fn is_valid(&self) -> bool {
        [
            Some(self.input),
            Some(self.output),
            self.cache_read,
            self.cache_write,
        ]
        .into_iter()
        .flatten()
        .all(|p| p.is_finite() && p >= 0.0)
    }
}

/// Where an amount came from, most trusted first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The harness reported the cost itself.
    Reported,
    /// The user's `[pricing]` or pricing.toml.
    Config,
    /// The router provider's `/models` listing.
    Provider,
    /// The built-in price table.
    Table,
    Unknown,
}
impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Config => "config",
            Self::Provider => "provider",
            Self::Table => "table",
            Self::Unknown => "unknown",
        }
    }
}

/// One attempt's cost.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Estimate {
    pub usd: Option<f64>,
    pub source: Source,
    /// Tokens budgets count; see `counted_tokens`.
    pub tokens: u64,
}

/// Input, output, and cache-write tokens. Cache reads repeat context already
/// counted and would dwarf the rest, so token caps leave them out.
pub fn counted_tokens(usage: &Usage) -> u64 {
    [
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_write_tokens,
    ]
    .into_iter()
    .flatten()
    .sum()
}

#[derive(Deserialize)]
struct Table {
    #[serde(default)]
    as_of: String,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    models: BTreeMap<String, Price>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| toml::from_str(PRICES).expect("built-in price table parses"))
}

/// The date the built-in prices were last checked.
pub fn table_as_of() -> &'static str {
    &table().as_of
}

/// Lowercase, without provider prefixes (`anthropic/`, Bedrock's
/// `us.anthropic.`), variant tags (`[1m]`, `:free`), version or date
/// suffixes, and with `.`/`@`/`_` as `-`. Harness aliases resolve to the
/// model they select.
pub fn normalize(id: &str) -> String {
    let mut id = id.trim().to_ascii_lowercase();
    if let Some(start) = id.find('[') {
        id.truncate(start);
    }
    if let Some(last) = id.rsplit('/').next() {
        id = last.to_owned();
    }
    if let Some(at) = id.find("anthropic.") {
        id = id[at + "anthropic.".len()..].to_owned();
    }
    if let Some(colon) = id.find(':') {
        id.truncate(colon);
    }
    let id = id.replace(['.', '@', '_'], "-");
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    let mut parts: Vec<&str> = id.split('-').filter(|p| !p.is_empty()).collect();
    loop {
        let n = parts.len();
        if n < 2 {
            break;
        }
        let last = parts[n - 1];
        // Bedrock's "-20250219-v1" revision; "deepseek-v3" keeps its "v3".
        let revision = last.len() > 1
            && last.starts_with('v')
            && last[1..].bytes().all(|b| b.is_ascii_digit())
            && digits(parts[n - 2], 8);
        if matches!(last, "latest" | "preview" | "exp") || digits(last, 8) || revision {
            parts.pop();
        } else if n > 3 && digits(parts[n - 3], 4) && digits(parts[n - 2], 2) && digits(last, 2) {
            parts.truncate(n - 3);
        } else if n > 2 && digits(parts[n - 2], 2) && digits(last, 2) {
            parts.truncate(n - 2);
        } else {
            break;
        }
    }
    let id = parts.join("-");
    match table().aliases.get(&id) {
        Some(model) => normalize(model),
        None => id,
    }
}

/// Exact ID (case-insensitive), then normalized ID, then (with `family`) the
/// longest entry that prefixes the normalized ID at a `-` boundary.
fn find(prices: &BTreeMap<String, Price>, model: &str, family: bool) -> Option<Price> {
    let raw = model.trim().to_ascii_lowercase();
    if let Some((_, price)) = prices.iter().find(|(id, _)| id.to_ascii_lowercase() == raw) {
        return Some(*price);
    }
    let wanted = normalize(model);
    if wanted.is_empty() {
        return None;
    }
    let keyed: Vec<(String, Price)> = prices.iter().map(|(id, p)| (normalize(id), *p)).collect();
    if let Some((_, price)) = keyed.iter().find(|(id, _)| *id == wanted) {
        return Some(*price);
    }
    if !family {
        return None;
    }
    keyed
        .into_iter()
        .filter(|(id, _)| {
            !id.is_empty() && wanted.starts_with(id.as_str()) && wanted[id.len()..].starts_with('-')
        })
        .max_by_key(|(id, _)| id.len())
        .map(|(_, price)| price)
}

/// Prices beyond the built-in table: user overrides and cached provider
/// listings. `Pricing::default()` is the table alone.
#[derive(Debug, Clone, Default)]
pub struct Pricing {
    overrides: BTreeMap<String, Price>,
    providers: BTreeMap<String, BTreeMap<String, Price>>,
}
impl Pricing {
    /// The user's pricing.toml, then `[pricing]` from the global and project
    /// configs (later wins), and the project's cached provider prices.
    /// Reads files only; unreadable or invalid entries are skipped.
    pub fn load(project: &Path) -> Self {
        let read = |path: PathBuf| -> Option<toml::Value> {
            toml::from_str(&fs::read_to_string(path).ok()?).ok()
        };
        let global = crate::config::config_dir();
        let mut pricing = Self::default();
        if let Some(file) = read(global.join("pricing.toml")) {
            pricing.extend(file.get("models"));
        }
        for path in [
            global.join("config.toml"),
            project.join(".model-router.toml"),
        ] {
            if let Some(config) = read(path) {
                pricing.extend(config.get("pricing"));
            }
        }
        pricing.providers = read_cache(project)
            .providers
            .into_iter()
            .map(|(name, cached)| (name, cached.models))
            .collect();
        pricing
    }

    fn extend(&mut self, table: Option<&toml::Value>) {
        for (id, value) in table.and_then(toml::Value::as_table).into_iter().flatten() {
            if let Ok(price) = value.clone().try_into::<Price>() {
                if price.is_valid() {
                    self.overrides.insert(id.clone(), price);
                }
            }
        }
    }

    pub fn with_override(mut self, model: &str, price: Price) -> Self {
        self.overrides.insert(model.into(), price);
        self
    }

    pub fn with_provider(mut self, provider: &str, prices: BTreeMap<String, Price>) -> Self {
        self.providers.insert(provider.into(), prices);
        self
    }

    /// The price for `model`, routed through router `provider` if any.
    /// Provider listings match exactly or normalized, never by family.
    pub fn lookup(&self, model: &str, provider: Option<&str>) -> Option<(Price, Source)> {
        if model.trim().is_empty() {
            return None;
        }
        if let Some(price) = find(&self.overrides, model, true) {
            return Some((price, Source::Config));
        }
        if let Some(price) = provider
            .and_then(|name| self.providers.get(name))
            .and_then(|prices| find(prices, model, false))
        {
            return Some((price, Source::Provider));
        }
        find(&table().models, model, true).map(|price| (price, Source::Table))
    }
}

/// One attempt's cost: reported, else priced from its tokens, else unknown.
pub fn estimate(
    usage: Option<&Usage>,
    model: &str,
    provider: Option<&str>,
    pricing: &Pricing,
) -> Estimate {
    let Some(usage) = usage else {
        return Estimate {
            usd: None,
            source: Source::Unknown,
            tokens: 0,
        };
    };
    let tokens = counted_tokens(usage);
    if let Some(usd) = usage.cost_usd {
        return Estimate {
            usd: Some(usd),
            source: Source::Reported,
            tokens,
        };
    }
    match pricing.lookup(model, provider) {
        Some((price, source)) => Estimate {
            usd: Some(price.cost(usage)),
            source,
            tokens,
        },
        None => Estimate {
            usd: None,
            source: Source::Unknown,
            tokens,
        },
    }
}

/// The router provider stage `index` routes through, if any.
fn routed_provider(run: &Run, index: usize) -> Option<&str> {
    let stage = run.workflow.stages.get(index)?;
    providers::route(&stage.harness, &stage.provider, &run.providers)
        .is_some()
        .then_some(stage.provider.as_str())
}

pub fn attempt_estimate(run: &Run, index: usize, attempt: &Attempt, pricing: &Pricing) -> Estimate {
    estimate(
        attempt.usage.as_ref(),
        &attempt.model,
        routed_provider(run, index),
        pricing,
    )
}

/// Known amounts from one source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Part {
    pub usd: f64,
    pub attempts: usize,
}

/// Summed estimates. `usd` adds every known amount; `unknown` counts
/// attempts with no usage or no price.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct CostTotal {
    pub usd: f64,
    pub reported: Part,
    pub config: Part,
    pub provider: Part,
    pub table: Part,
    pub attempts: usize,
    pub unknown: usize,
    /// Input, output, and cache-write tokens; what token caps count.
    pub tokens: u64,
}
impl CostTotal {
    pub fn add(&mut self, estimate: &Estimate) {
        self.attempts += 1;
        self.tokens += estimate.tokens;
        let part = match estimate.source {
            Source::Reported => &mut self.reported,
            Source::Config => &mut self.config,
            Source::Provider => &mut self.provider,
            Source::Table => &mut self.table,
            Source::Unknown => {
                self.unknown += 1;
                return;
            }
        };
        let usd = estimate.usd.unwrap_or(0.0);
        part.usd += usd;
        part.attempts += 1;
        self.usd += usd;
    }

    pub fn merge(&mut self, other: &Self) {
        self.usd += other.usd;
        for (mine, theirs) in [
            (&mut self.reported, other.reported),
            (&mut self.config, other.config),
            (&mut self.provider, other.provider),
            (&mut self.table, other.table),
        ] {
            mine.usd += theirs.usd;
            mine.attempts += theirs.attempts;
        }
        self.attempts += other.attempts;
        self.unknown += other.unknown;
        self.tokens += other.tokens;
    }

    fn parts(&self) -> impl Iterator<Item = (Source, Part)> {
        [
            (Source::Reported, self.reported),
            (Source::Config, self.config),
            (Source::Provider, self.provider),
            (Source::Table, self.table),
        ]
        .into_iter()
        .filter(|(_, part)| part.attempts > 0)
    }

    /// Whether any attempt has a known cost.
    pub fn known(&self) -> bool {
        self.attempts > self.unknown
    }

    /// Whether every known amount was reported by its harness.
    pub fn exact(&self) -> bool {
        self.parts().all(|(source, _)| source == Source::Reported)
    }

    /// E.g. "reported + table".
    pub fn sources(&self) -> String {
        let labels: Vec<_> = self.parts().map(|(source, _)| source.label()).collect();
        if labels.is_empty() {
            Source::Unknown.label().into()
        } else {
            labels.join(" + ")
        }
    }

    fn amount(&self) -> String {
        format!(
            "{}{}",
            if self.exact() { "" } else { "~" },
            report::dollars(self.usd)
        )
    }

    /// E.g. "~$0.31 (reported $0.25 + table $0.06) · cost unknown for 1 of 3 attempts".
    pub fn text(&self) -> String {
        if self.attempts == 0 {
            return "no attempts".into();
        }
        if !self.known() {
            return "cost unknown".into();
        }
        let parts: Vec<_> = self.parts().collect();
        let mut text = if parts.len() == 1 {
            format!("{} ({})", self.amount(), parts[0].0.label())
        } else {
            format!(
                "{} ({})",
                self.amount(),
                parts
                    .iter()
                    .map(|(source, part)| format!(
                        "{} {}",
                        source.label(),
                        report::dollars(part.usd)
                    ))
                    .collect::<Vec<_>>()
                    .join(" + ")
            )
        };
        if self.unknown > 0 {
            let _ = write!(
                text,
                " · cost unknown for {} of {} attempt{}",
                self.unknown,
                self.attempts,
                if self.attempts == 1 { "" } else { "s" }
            );
        }
        text
    }

    /// Table width: "$0.25", "~$0.31", "~$0.31+?" when some are unknown,
    /// "?" when none are known, "–" without attempts.
    pub fn short(&self) -> String {
        match (self.attempts, self.known()) {
            (0, _) => "–".into(),
            (_, false) => "?".into(),
            _ if self.unknown > 0 => format!("{}+?", self.amount()),
            _ => self.amount(),
        }
    }
}

/// Per-stage spend of a run, in workflow order.
pub fn stage_totals(run: &Run, pricing: &Pricing) -> Vec<CostTotal> {
    run.stages
        .iter()
        .enumerate()
        .map(|(index, state)| {
            let mut total = CostTotal::default();
            for attempt in &state.attempts {
                total.add(&attempt_estimate(run, index, attempt, pricing));
            }
            total
        })
        .collect()
}

pub fn run_total(run: &Run, pricing: &Pricing) -> CostTotal {
    let mut total = CostTotal::default();
    for stage in stage_totals(run, pricing) {
        total.merge(&stage);
    }
    total
}

/// Whether the workflow or any of its stages sets a cap.
pub fn has_budget(run: &Run) -> bool {
    !run.workflow.budget.is_empty() || run.workflow.stages.iter().any(|s| !s.budget.is_empty())
}

pub const EXCEEDED: &str = "Budget exceeded";

/// Why the run must stop before its next stage: a stage's or the whole
/// run's known cost or counted tokens exceed its cap. Unknown costs count
/// toward token caps only. Reads pricing files; never the network.
pub fn check_budget(run: &Run) -> Option<String> {
    if !has_budget(run) {
        return None;
    }
    check_budget_with(run, &Pricing::load(&run.project))
}

pub fn check_budget_with(run: &Run, pricing: &Pricing) -> Option<String> {
    let stages = stage_totals(run, pricing);
    let mut total = CostTotal::default();
    let mut reason = None;
    for (stage, spent) in run.workflow.stages.iter().zip(&stages) {
        total.merge(spent);
        reason =
            reason.or_else(|| exceeded(&stage.budget, spent, &format!(" in stage {}", stage.name)));
    }
    let reason = reason.or_else(|| exceeded(&run.workflow.budget, &total, ""))?;
    Some(format!(
        "{reason}. Inspect the run, then raise the cap to continue: model-router -C {} resume {} --budget-usd N (or --budget-tokens N)",
        run.project.display(),
        run.id
    ))
}

fn exceeded(budget: &Budget, spent: &CostTotal, scope: &str) -> Option<String> {
    if let Some(cap) = budget.max_usd {
        if spent.usd > cap {
            return Some(format!(
                "{EXCEEDED}: {} of {} ({}){scope}",
                spent.amount(),
                report::dollars(cap),
                spent.sources()
            ));
        }
    }
    if let Some(cap) = budget.max_tokens {
        if spent.tokens > cap {
            return Some(format!(
                "{EXCEEDED}: {} of {} tokens (reported){scope}",
                report::tokens(spent.tokens),
                report::tokens(cap)
            ));
        }
    }
    None
}

/// Whether the run stopped on a budget cap.
pub fn budget_blocked(run: &Run) -> bool {
    run.status == Status::Blocked
        && run
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with(EXCEEDED))
}

/// `resume --budget-usd/--budget-tokens`: sets the run's cap, and any stage
/// cap already exceeded, to the new value, and unblocks a run its budget
/// stopped. The next drive checks again.
pub fn raise_budget(run: &mut Run, usd: Option<f64>, tokens: Option<u64>) -> Result<()> {
    if let Some(usd) = usd {
        ensure!(
            usd.is_finite() && usd > 0.0,
            "--budget-usd must be a positive amount"
        );
    }
    ensure!(tokens != Some(0), "--budget-tokens must be positive");
    if usd.is_none() && tokens.is_none() {
        bail!("Give --budget-usd or --budget-tokens");
    }
    let spent = stage_totals(run, &Pricing::load(&run.project));
    for (stage, spent) in run.workflow.stages.iter_mut().zip(&spent) {
        if let (Some(new), Some(cap)) = (usd, stage.budget.max_usd) {
            if spent.usd > cap {
                stage.budget.max_usd = Some(new);
            }
        }
        if let (Some(new), Some(cap)) = (tokens, stage.budget.max_tokens) {
            if spent.tokens > cap {
                stage.budget.max_tokens = Some(new);
            }
        }
    }
    if usd.is_some() {
        run.workflow.budget.max_usd = usd;
    }
    if tokens.is_some() {
        run.workflow.budget.max_tokens = tokens;
    }
    if budget_blocked(run) {
        run.status = Status::Pending;
        run.error = None;
        if let Some(state) = run.stages.get_mut(run.cursor) {
            state.status = Status::Pending;
        }
    }
    Ok(())
}

// Provider price cache --------------------------------------------------------

const CACHE_FILE: &str = "pricing-cache.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProviderCache {
    #[serde(default)]
    providers: BTreeMap<String, CachedPrices>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedPrices {
    fetched_at: DateTime<Utc>,
    base_url: String,
    models: BTreeMap<String, Price>,
}

fn cache_path(project: &Path) -> PathBuf {
    project.join(".model-router").join(CACHE_FILE)
}

fn read_cache(project: &Path) -> ProviderCache {
    fs::read(cache_path(project))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Per-million prices from a `/models` listing's per-token prices. Models
/// without both prices, or with negative ones (routers), are left out.
pub fn listed_prices(models: &[ModelInfo]) -> BTreeMap<String, Price> {
    models
        .iter()
        .filter_map(|model| {
            let price = Price {
                input: model.prompt_price? * 1e6,
                output: model.completion_price? * 1e6,
                cache_read: None,
                cache_write: None,
            };
            price.is_valid().then(|| (model.id.clone(), price))
        })
        .collect()
}

/// Router providers the run's stages route through.
pub fn routed_providers(run: &Run) -> BTreeMap<String, Provider> {
    (0..run.workflow.stages.len())
        .filter_map(|index| {
            let stage = &run.workflow.stages[index];
            routed_provider(run, index)?;
            let provider = providers::route(&stage.harness, &stage.provider, &run.providers)?;
            Some((stage.provider.clone(), provider.into_owned()))
        })
        .collect()
}

/// Refreshes the cached `/models` prices of `providers` older than a day
/// (or whose endpoint changed), one short request each. Failures, e.g.
/// offline, keep the old cache and are otherwise ignored.
pub fn refresh_provider_prices(project: &Path, providers: &BTreeMap<String, Provider>) {
    refresh_with(project, providers, Utc::now(), |provider| {
        providers::models_within(provider, std::time::Duration::from_secs(5))
    });
}

fn refresh_with(
    project: &Path,
    providers: &BTreeMap<String, Provider>,
    now: DateTime<Utc>,
    fetch: impl Fn(&Provider) -> Result<Vec<ModelInfo>>,
) {
    let mut cache = read_cache(project);
    let mut changed = false;
    for (name, provider) in providers {
        let fresh = cache.providers.get(name).is_some_and(|cached| {
            cached.base_url == provider.base_url && now - cached.fetched_at < Duration::days(1)
        });
        if fresh {
            continue;
        }
        if let Ok(models) = fetch(provider) {
            let models = listed_prices(&models);
            if !models.is_empty() {
                cache.providers.insert(
                    name.clone(),
                    CachedPrices {
                        fetched_at: now,
                        base_url: provider.base_url.clone(),
                        models,
                    },
                );
                changed = true;
            }
        }
    }
    if changed {
        if let Ok(dir) = storage::metadata_dir(project) {
            let _ = storage::write_json(&dir.join(CACHE_FILE), &cache);
        }
    }
}

// History aggregation -------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub harness: String,
    /// The attempt's model; "default" for the harness default.
    pub model: String,
    pub runs: usize,
    pub usage: Usage,
    pub cost: CostTotal,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub since: Option<DateTime<Utc>>,
    pub prices_as_of: String,
    pub rows: Vec<Row>,
    pub usage: Usage,
    pub total: CostTotal,
}

/// Spend of attempts started at or after `since`, by harness and model.
pub fn aggregate<'a>(
    runs: impl IntoIterator<Item = &'a Run>,
    since: Option<DateTime<Utc>>,
    pricing: &Pricing,
) -> Summary {
    let mut rows: BTreeMap<(String, String), (Row, BTreeSet<String>)> = BTreeMap::new();
    let mut summary = Summary {
        since,
        prices_as_of: table_as_of().into(),
        rows: vec![],
        usage: Usage::default(),
        total: CostTotal::default(),
    };
    for run in runs {
        for (index, (stage, state)) in run.workflow.stages.iter().zip(&run.stages).enumerate() {
            for attempt in &state.attempts {
                if since.is_some_and(|since| attempt.started_at < since) {
                    continue;
                }
                let estimate = attempt_estimate(run, index, attempt, pricing);
                let model = if attempt.model.is_empty() {
                    "default".to_owned()
                } else {
                    attempt.model.clone()
                };
                let (row, ids) = rows
                    .entry((stage.harness.clone(), model.clone()))
                    .or_insert_with(|| {
                        (
                            Row {
                                harness: stage.harness.clone(),
                                model,
                                runs: 0,
                                usage: Usage::default(),
                                cost: CostTotal::default(),
                            },
                            BTreeSet::new(),
                        )
                    });
                ids.insert(run.id.clone());
                row.cost.add(&estimate);
                summary.total.add(&estimate);
                if let Some(usage) = attempt.usage {
                    row.usage = row.usage + usage;
                    summary.usage = summary.usage + usage;
                }
            }
        }
    }
    summary.rows = rows
        .into_values()
        .map(|(mut row, ids)| {
            row.runs = ids.len();
            row
        })
        .collect();
    summary
}

/// "7d", "12h", "30m", or "2w".
pub fn parse_since(text: &str) -> Result<Duration> {
    let text = text.trim();
    let split = text.len().saturating_sub(1);
    let (number, unit) = text.split_at(split);
    let number: i64 = number
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow::anyhow!("Use a duration like 7d, 12h, 30m, or 2w"))?;
    Ok(match unit {
        "m" => Duration::minutes(number),
        "h" => Duration::hours(number),
        "d" => Duration::days(number),
        "w" => Duration::weeks(number),
        _ => bail!("Use a duration like 7d, 12h, 30m, or 2w"),
    })
}

pub fn render_summary(summary: &Summary) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Spend {} · prices are estimates (table as of {}); verify with your provider",
        summary.since.map_or_else(
            || "over all history".into(),
            |since| format!("since {}", since.format("%Y-%m-%d %H:%M UTC"))
        ),
        summary.prices_as_of
    );
    if summary.rows.is_empty() {
        let _ = writeln!(text, "No attempts.");
        return text;
    }
    let width = |f: fn(&Row) -> usize| summary.rows.iter().map(f).max().unwrap_or(0);
    let (harness, model) = (
        width(|r| r.harness.chars().count()).max(7),
        width(|r| r.model.chars().count()).max(5),
    );
    let _ = writeln!(
        text,
        "{:<harness$}  {:<model$}  {:>4}  {:>8}  {:>8}  {:>10}  COST",
        "HARNESS", "MODEL", "RUNS", "ATTEMPTS", "TOKENS", "CACHE READ"
    );
    let count = |n: Option<u64>| n.map_or_else(|| "–".into(), report::tokens);
    for row in &summary.rows {
        let _ = writeln!(
            text,
            "{:<harness$}  {:<model$}  {:>4}  {:>8}  {:>8}  {:>10}  {}",
            row.harness,
            row.model,
            row.runs,
            row.cost.attempts,
            report::tokens(row.cost.tokens),
            count(row.usage.cache_read_tokens),
            row.cost.text()
        );
    }
    let _ = writeln!(text, "Total: {}", summary.total.text());
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, read: u64, write: u64) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(read),
            cache_write_tokens: Some(write),
            cost_usd: None,
        }
    }

    fn price(input: f64, output: f64) -> Price {
        Price {
            input,
            output,
            cache_read: None,
            cache_write: None,
        }
    }

    #[test]
    fn table_parses_with_a_date() {
        assert!(!table_as_of().is_empty());
        assert!(table().models.values().all(Price::is_valid));
        for target in table().aliases.values() {
            assert!(table().models.contains_key(target), "{target}");
        }
    }

    #[test]
    fn ids_normalize_across_provider_spellings() {
        for (id, expected) in [
            ("anthropic/claude-sonnet-4.5", "claude-sonnet-4-5"),
            (
                "us.anthropic.claude-3-7-sonnet-20250219-v1:0",
                "claude-3-7-sonnet",
            ),
            ("claude-opus-4-5@20251101", "claude-opus-4-5"),
            ("claude-haiku-4-5-20251001", "claude-haiku-4-5"),
            ("openrouter/openai/gpt-4o-2024-08-06", "gpt-4o"),
            ("gemini-2.5-pro-preview-06-05", "gemini-2-5-pro"),
            ("sonnet[1m]", "claude-sonnet-5-5"),
            ("Opus", "claude-opus-5-5"),
            ("gpt-5.1-codex", "gpt-5-1-codex"),
            ("deepseek/deepseek-chat:free", "deepseek-chat"),
        ] {
            assert_eq!(normalize(id), expected, "{id}");
        }
    }

    #[test]
    fn prices_match_exact_then_normalized_then_family() {
        let pricing = Pricing::default();
        let table_price = |id: &str| {
            pricing
                .lookup(id, None)
                .map(|(p, s)| (p.input, p.output, s))
        };
        assert_eq!(
            table_price("claude-opus-5-5"),
            Some((4.0, 20.0, Source::Table))
        );
        assert_eq!(
            table_price("anthropic/claude-opus-5.5"),
            Some((4.0, 20.0, Source::Table))
        );
        // A dated or unlisted point release falls back to its family.
        assert_eq!(
            table_price("claude-opus-4-1-20250805").map(|p| p.0),
            Some(15.0)
        );
        assert_eq!(table_price("claude-sonnet-4-5").map(|p| p.0), Some(3.0));
        assert_eq!(table_price("gpt-5-mini").map(|p| p.0), Some(0.25));
        assert_eq!(table_price("gpt-5.1-codex-max").map(|p| p.0), Some(1.25));
        assert_eq!(table_price("haiku").map(|p| p.1), Some(0.5));
        // Family matching respects segment boundaries and never invents a price.
        assert_eq!(table_price("o30"), None);
        assert_eq!(table_price("gpt-6-astra"), None);
        assert_eq!(table_price(""), None);
    }

    #[test]
    fn overrides_beat_providers_and_providers_beat_the_table() {
        let listed = BTreeMap::from([("anthropic/claude-sonnet-5.5".to_owned(), price(1.0, 2.0))]);
        let pricing = Pricing::default().with_provider("openrouter", listed);
        let (p, source) = pricing
            .lookup("anthropic/claude-sonnet-5.5", Some("openrouter"))
            .unwrap();
        assert_eq!((p.input, source), (1.0, Source::Provider));
        // Not routed through the provider: the table.
        assert_eq!(
            pricing
                .lookup("anthropic/claude-sonnet-5.5", None)
                .unwrap()
                .1,
            Source::Table
        );
        // Providers match normalized IDs but never by family.
        assert_eq!(
            pricing
                .lookup("claude-sonnet-5-5", Some("openrouter"))
                .unwrap()
                .1,
            Source::Provider
        );
        assert_eq!(
            pricing
                .lookup("claude-sonnet-5-5-new", Some("openrouter"))
                .unwrap()
                .1,
            Source::Table
        );
        let pricing = pricing.with_override("claude-sonnet-5", price(9.0, 9.0));
        let (p, source) = pricing
            .lookup("anthropic/claude-sonnet-5.5", Some("openrouter"))
            .unwrap();
        assert_eq!((p.input, source), (9.0, Source::Config));
        let custom = Pricing::default().with_override("gpt-6", price(3.0, 30.0));
        assert_eq!(
            custom.lookup("gpt-6-astra", None).unwrap().1,
            Source::Config
        );
    }

    #[test]
    fn reported_cost_wins_and_unpriced_tokens_stay_unknown() {
        let pricing = Pricing::default();
        let mut reported = usage(1_000, 100, 0, 0);
        reported.cost_usd = Some(0.5);
        let e = estimate(Some(&reported), "claude-opus-5-5", None, &pricing);
        assert_eq!(
            (e.usd, e.source, e.tokens),
            (Some(0.5), Source::Reported, 1_100)
        );
        // 1M in at $4, 100k out at $20, 1M cache read at $0.20, 100k write at $5.
        let e = estimate(
            Some(&usage(1_000_000, 100_000, 1_000_000, 100_000)),
            "opus",
            None,
            &pricing,
        );
        assert_eq!(e.source, Source::Table);
        assert!((e.usd.unwrap() - (4.0 + 2.0 + 0.2 + 0.5)).abs() < 1e-9);
        assert_eq!(e.tokens, 1_200_000, "cache reads are not counted");
        let e = estimate(Some(&usage(10, 10, 0, 0)), "mystery-model", None, &pricing);
        assert_eq!((e.usd, e.source, e.tokens), (None, Source::Unknown, 20));
        let e = estimate(None, "opus", None, &pricing);
        assert_eq!((e.usd, e.source, e.tokens), (None, Source::Unknown, 0));
    }

    #[test]
    fn totals_label_sources_and_unknowns() {
        let mut total = CostTotal::default();
        assert_eq!(
            (total.text(), total.short()),
            ("no attempts".into(), "–".into())
        );
        total.add(&Estimate {
            usd: None,
            source: Source::Unknown,
            tokens: 5,
        });
        assert_eq!(
            (total.text(), total.short()),
            ("cost unknown".into(), "?".into())
        );
        total.add(&Estimate {
            usd: Some(0.25),
            source: Source::Reported,
            tokens: 5,
        });
        assert_eq!(
            total.text(),
            "$0.25 (reported) · cost unknown for 1 of 2 attempts"
        );
        assert_eq!(total.short(), "$0.25+?");
        total.add(&Estimate {
            usd: Some(0.06),
            source: Source::Table,
            tokens: 5,
        });
        assert_eq!(
            total.text(),
            "~$0.31 (reported $0.25 + table $0.06) · cost unknown for 1 of 3 attempts"
        );
        assert_eq!(total.sources(), "reported + table");
        let mut sum = CostTotal::default();
        sum.merge(&total);
        sum.merge(&total);
        assert_eq!(
            (sum.attempts, sum.unknown, sum.tokens, sum.table.attempts),
            (6, 2, 30, 2)
        );
        assert!((sum.usd - 0.62).abs() < 1e-9);
    }

    #[test]
    fn since_parses_units() {
        assert_eq!(parse_since("7d").unwrap(), Duration::days(7));
        assert_eq!(parse_since("12h").unwrap(), Duration::hours(12));
        assert_eq!(parse_since("2w").unwrap(), Duration::weeks(2));
        assert_eq!(parse_since("30m").unwrap(), Duration::minutes(30));
        for bad in ["", "d", "0d", "-1d", "7y", "7"] {
            assert!(parse_since(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn provider_cache_refreshes_daily_and_survives_failures() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Provider {
            base_url: "http://localhost:1/v1".into(),
            ..Provider::default()
        };
        let providers = BTreeMap::from([("gw".to_owned(), provider)]);
        let listing = || {
            Ok(vec![
                ModelInfo {
                    id: "vendor/model-a".into(),
                    prompt_price: Some(0.000002),
                    completion_price: Some(0.00001),
                    ..ModelInfo::default()
                },
                ModelInfo {
                    id: "router/auto".into(),
                    prompt_price: Some(-1.0),
                    completion_price: Some(-1.0),
                    ..ModelInfo::default()
                },
            ])
        };
        let now = Utc::now();
        let calls = std::cell::Cell::new(0);
        let fetch = |_: &Provider| {
            calls.set(calls.get() + 1);
            listing()
        };
        refresh_with(dir.path(), &providers, now, fetch);
        let pricing = Pricing::load(dir.path());
        let (p, source) = pricing.lookup("vendor/model-a", Some("gw")).unwrap();
        assert_eq!(source, Source::Provider);
        assert!((p.input - 2.0).abs() < 1e-9 && (p.output - 10.0).abs() < 1e-9);
        assert!(pricing.lookup("router/auto", Some("gw")).is_none());
        // Fresh: no request. A day later: refreshed. Offline: the cache stays.
        refresh_with(dir.path(), &providers, now + Duration::hours(23), fetch);
        assert_eq!(calls.get(), 1);
        refresh_with(dir.path(), &providers, now + Duration::hours(25), fetch);
        assert_eq!(calls.get(), 2);
        refresh_with(dir.path(), &providers, now + Duration::days(3), |_| {
            bail!("offline")
        });
        assert!(
            Pricing::load(dir.path())
                .lookup("vendor/model-a", Some("gw"))
                .is_some()
        );
    }

    #[test]
    fn config_pricing_overrides_load_from_the_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".model-router.toml"),
            "[pricing.\"my-model\"]\ninput = 1.0\noutput = 2.0\n[pricing.bad]\ninput = -1.0\noutput = 1.0\n",
        )
        .unwrap();
        let pricing = Pricing::load(dir.path());
        assert_eq!(pricing.lookup("my-model", None).unwrap().1, Source::Config);
        assert!(pricing.lookup("bad", None).is_none());
    }
}
