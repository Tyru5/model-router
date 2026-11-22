//! Cost estimates and budget caps.
//!
//! A harness's own reported cost wins; otherwise the user's pricing
//! overrides, then a router provider's `/models` prices (cached daily), then
//! the built-in table (`cost/prices.toml`, estimates). Every amount carries
//! its source. Tokens without a price stay unknown and count only toward
//! token caps. Nothing here makes a network request except
//! `refresh_provider_prices`.
use crate::{
    config::{Billing, Budget, BudgetAction, CustomAdapter},
    engine::{Attempt, Run, Status},
    handoff::Usage,
    providers::{self, ModelInfo, Provider},
    report, storage,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
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
    /// Whether this spend is metered API billing or covered by a harness
    /// subscription (estimated API-equivalent value).
    pub billing: Billing,
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
struct RawTable {
    #[serde(default)]
    as_of: String,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    models: BTreeMap<String, Price>,
}

struct Table {
    as_of: String,
    aliases: BTreeMap<String, String>,
    #[cfg_attr(not(test), allow(dead_code))]
    models: BTreeMap<String, Price>,
    index: PriceIndex,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let raw: RawTable = toml::from_str(PRICES).expect("built-in price table parses");
        let index = PriceIndex::from_map(&raw.models, &raw.aliases);
        Table {
            as_of: raw.as_of,
            aliases: raw.aliases,
            models: raw.models,
            index,
        }
    })
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
    normalize_with_aliases(id, &table().aliases, 0)
}

fn normalize_with_aliases(id: &str, aliases: &BTreeMap<String, String>, depth: u8) -> String {
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
    if depth < 8 {
        if let Some(model) = aliases.get(&id) {
            return normalize_with_aliases(model, aliases, depth + 1);
        }
    }
    id
}

#[derive(Debug, Clone, Default)]
struct PriceIndex {
    exact: HashMap<String, Price>,
    normalized: HashMap<String, Price>,
}

impl PriceIndex {
    fn from_map(prices: &BTreeMap<String, Price>, aliases: &BTreeMap<String, String>) -> Self {
        let mut index = Self::default();
        for (id, price) in prices {
            index.exact.entry(id.to_ascii_lowercase()).or_insert(*price);
            index
                .normalized
                .entry(normalize_with_aliases(id, aliases, 0))
                .or_insert(*price);
        }
        index
    }

    fn insert(&mut self, id: &str, price: Price) {
        let raw = id.to_ascii_lowercase();
        let norm = normalize(id);
        if !norm.is_empty() {
            self.exact.retain(|k, _| k == &raw || normalize(k) != norm);
        }
        self.exact.insert(raw, price);
        self.normalized.insert(norm, price);
    }

    /// Exact ID (case-insensitive), then normalized ID, then (with `family`) the
    /// longest entry that prefixes the normalized ID at a `-` boundary.
    fn find(&self, raw: &str, wanted: &str, family: bool) -> Option<Price> {
        if let Some(&price) = self.exact.get(raw) {
            return Some(price);
        }
        if wanted.is_empty() {
            return None;
        }
        if let Some(&price) = self.normalized.get(wanted) {
            return Some(price);
        }
        if !family {
            return None;
        }
        for (idx, _) in wanted.rmatch_indices('-') {
            if idx > 0 {
                if let Some(&price) = self.normalized.get(&wanted[..idx]) {
                    return Some(price);
                }
            }
        }
        None
    }
}

/// Prices beyond the built-in table: user overrides and cached provider
/// listings. `Pricing::default()` is the table alone.
#[derive(Debug, Clone, Default)]
pub struct Pricing {
    overrides: PriceIndex,
    providers: BTreeMap<String, PriceIndex>,
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
        let aliases = &table().aliases;
        pricing.providers = read_cache(project)
            .providers
            .into_iter()
            .map(|(name, cached)| (name, PriceIndex::from_map(&cached.models, aliases)))
            .collect();
        pricing
    }

    fn extend(&mut self, table: Option<&toml::Value>) {
        for (id, value) in table.and_then(toml::Value::as_table).into_iter().flatten() {
            if let Ok(price) = value.clone().try_into::<Price>() {
                if price.is_valid() {
                    self.overrides.insert(id, price);
                }
            }
        }
    }

    pub fn with_override(mut self, model: &str, price: Price) -> Self {
        self.overrides.insert(model, price);
        self
    }

    pub fn with_provider(mut self, provider: &str, prices: BTreeMap<String, Price>) -> Self {
        self.providers.insert(
            provider.into(),
            PriceIndex::from_map(&prices, &table().aliases),
        );
        self
    }

    /// The price for `model`, routed through router `provider` if any.
    /// Provider listings match exactly or normalized, never by family.
    pub fn lookup(&self, model: &str, provider: Option<&str>) -> Option<(Price, Source)> {
        if model.trim().is_empty() {
            return None;
        }
        let raw = model.trim().to_ascii_lowercase();
        let wanted = normalize(model);
        if let Some(price) = self.overrides.find(&raw, &wanted, true) {
            return Some((price, Source::Config));
        }
        if let Some(price) = provider
            .and_then(|name| self.providers.get(name))
            .and_then(|prices| prices.find(&raw, &wanted, false))
        {
            return Some((price, Source::Provider));
        }
        table()
            .index
            .find(&raw, &wanted, true)
            .map(|price| (price, Source::Table))
    }
}

/// One attempt's cost: reported, else priced from its tokens, else unknown.
pub fn estimate(
    usage: Option<&Usage>,
    model: &str,
    provider: Option<&str>,
    pricing: &Pricing,
) -> Estimate {
    let billing = if provider.is_some() {
        Billing::Api
    } else {
        Billing::Subscription
    };
    estimate_with_billing(usage, model, provider, billing, pricing)
}

pub fn estimate_with_billing(
    usage: Option<&Usage>,
    model: &str,
    provider: Option<&str>,
    billing: Billing,
    pricing: &Pricing,
) -> Estimate {
    let Some(usage) = usage else {
        return Estimate {
            usd: None,
            source: Source::Unknown,
            billing,
            tokens: 0,
        };
    };
    let tokens = counted_tokens(usage);
    if let Some(usd) = usage.cost_usd {
        return Estimate {
            usd: Some(usd),
            source: Source::Reported,
            billing,
            tokens,
        };
    }
    match pricing.lookup(model, provider) {
        Some((price, source)) => Estimate {
            usd: Some(price.cost(usage)),
            source,
            billing,
            tokens,
        },
        None => Estimate {
            usd: None,
            source: Source::Unknown,
            billing,
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

/// Effective billing mode for stage `index`: explicit stage `billing` wins;
/// otherwise a router provider implies metered API spend, and a harness's
/// own account uses the harness's usual billing (`HarnessSpec::billing`).
/// A custom adapter counts as metered: the router knows nothing about its
/// account, and calling real spend plan-covered would understate it.
pub fn stage_billing(run: &Run, index: usize) -> Billing {
    let Some(stage) = run.workflow.stages.get(index) else {
        return Billing::Api;
    };
    billing_for(
        stage.billing,
        routed_provider(run, index).is_some(),
        &stage.harness,
        &run.adapters,
    )
}

/// `stage_billing` from its parts: the stage's own `billing`, whether it is
/// routed to a router provider, and its harness.
pub fn billing_for(
    explicit: Option<Billing>,
    routed: bool,
    harness: &str,
    custom: &BTreeMap<String, CustomAdapter>,
) -> Billing {
    match explicit {
        Some(billing) => billing,
        None if routed => Billing::Api,
        None => harness_billing(harness, custom),
    }
}

/// The usual billing of a harness's own account.
pub fn harness_billing(harness: &str, custom: &BTreeMap<String, CustomAdapter>) -> Billing {
    if custom.contains_key(harness) {
        return Billing::Api;
    }
    crate::harness::get(harness).map_or(Billing::Api, |spec| spec.billing)
}

/// Routed provider (if any) and effective billing mode for stage `index`.
pub(crate) fn stage_estimate_params(run: &Run, index: usize) -> (Option<&str>, Billing) {
    let routed = routed_provider(run, index);
    let billing = match run.workflow.stages.get(index) {
        Some(stage) => billing_for(
            stage.billing,
            routed.is_some(),
            &stage.harness,
            &run.adapters,
        ),
        None => Billing::Api,
    };
    (routed, billing)
}

/// Routed provider and billing for a delegated task: its own target's, not
/// the stage's.
pub(crate) fn task_estimate_params<'a>(
    run: &'a Run,
    task: &'a crate::delegation::records::TaskState,
) -> (Option<&'a str>, Billing) {
    let target = &task.target;
    let routed = providers::route(&target.harness, &target.provider, &run.providers)
        .is_some()
        .then_some(target.provider.as_str());
    let explicit = target
        .delegate
        .as_ref()
        .and_then(|name| run.delegates.get(name))
        .and_then(|d| d.billing);
    (
        routed,
        billing_for(explicit, routed.is_some(), &target.harness, &run.adapters),
    )
}

/// A delegated task turn's estimate, priced under the task's target.
pub fn task_turn_estimate(
    run: &Run,
    task: &crate::delegation::records::TaskState,
    turn: &crate::delegation::records::TaskTurn,
    pricing: &Pricing,
) -> Estimate {
    let (provider, billing) = task_estimate_params(run, task);
    let model = if turn.model.is_empty() {
        &task.target.model
    } else {
        &turn.model
    };
    estimate_with_billing(turn.usage.as_ref(), model, provider, billing, pricing)
}

pub fn attempt_estimate(run: &Run, index: usize, attempt: &Attempt, pricing: &Pricing) -> Estimate {
    let (provider, billing) = stage_estimate_params(run, index);
    estimate_with_billing(
        attempt.usage.as_ref(),
        &attempt.model,
        provider,
        billing,
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
/// attempts with no usage or no price. `api` and `subscription` split known
/// spend between metered API usage and subscription-covered usage (API
/// equivalent).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct CostTotal {
    pub usd: f64,
    pub reported: Part,
    pub config: Part,
    pub provider: Part,
    pub table: Part,
    pub api: Part,
    pub subscription: Part,
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
        let bucket = match estimate.billing {
            Billing::Api => &mut self.api,
            Billing::Subscription => &mut self.subscription,
        };
        bucket.usd += usd;
        bucket.attempts += 1;
        self.usd += usd;
    }

    pub fn merge(&mut self, other: &Self) {
        self.usd += other.usd;
        for (mine, theirs) in [
            (&mut self.reported, other.reported),
            (&mut self.config, other.config),
            (&mut self.provider, other.provider),
            (&mut self.table, other.table),
            (&mut self.api, other.api),
            (&mut self.subscription, other.subscription),
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

    /// "$0.25" when every known amount was reported, else "~$0.31".
    pub fn amount(&self) -> String {
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
            let (provider, billing) = stage_estimate_params(run, index);
            let mut total = CostTotal::default();
            // Every candidate's spend counts, as in `report`.
            let candidates = state.candidate_attempts().map(|(_, a)| a);
            for attempt in state.attempts.iter().chain(candidates) {
                total.add(&estimate_with_billing(
                    attempt.usage.as_ref(),
                    &attempt.model,
                    provider,
                    billing,
                    pricing,
                ));
            }
            // Delegated tasks spend under their own targets.
            for (task, turn) in state.task_turns() {
                total.add(&task_turn_estimate(run, task, turn, pricing));
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

/// Result of checking a run's budget caps before launching the next stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetCheck {
    Ok,
    Warn(String),
    Stop(String),
}

/// Evaluates budget caps honoring `on_exceed` (`stop` vs `warn`).
pub fn check_budget_action(run: &Run) -> BudgetCheck {
    // A launched run answers to its launcher's caps too (which count every
    // run it launched); a cleaned launcher leaves only the run's own.
    let launcher = run
        .launched_by
        .as_ref()
        .and_then(|by| crate::engine::load(&run.project, &by.run).ok())
        .filter(has_budget);
    if !has_budget(run) && launcher.is_none() {
        return BudgetCheck::Ok;
    }
    let pricing = Pricing::load(&run.project);
    let own = if has_budget(run) {
        check_budget_action_with(run, &pricing)
    } else {
        BudgetCheck::Ok
    };
    if matches!(own, BudgetCheck::Stop(_)) {
        return own;
    }
    match launcher.map(|l| (check_budget_action_with(&l, &pricing), l.id)) {
        Some((BudgetCheck::Stop(reason), id)) => BudgetCheck::Stop(format!(
            "{reason} (launcher run {id}, counting the runs it launched)"
        )),
        Some((BudgetCheck::Warn(warning), _)) if own == BudgetCheck::Ok => {
            BudgetCheck::Warn(warning)
        }
        _ => own,
    }
}

/// Per-stage spend including the runs each stage's agent launched.
fn totals_with_launched(run: &Run, pricing: &Pricing) -> Vec<CostTotal> {
    let mut stages = stage_totals(run, pricing);
    for (index, state) in run.stages.iter().enumerate() {
        for launched in state.attempts.iter().flat_map(|a| &a.launched_runs) {
            if let Ok(child) = crate::engine::load(&run.project, &launched.id) {
                stages[index].merge(&run_total(&child, pricing));
            }
        }
    }
    stages
}

pub fn check_budget_action_with(run: &Run, pricing: &Pricing) -> BudgetCheck {
    let stages = totals_with_launched(run, pricing);
    let mut total = CostTotal::default();
    let mut stop_reason = None;
    let mut warn_reason = None;
    for (stage, spent) in run.workflow.stages.iter().zip(&stages) {
        total.merge(spent);
        if let Some(reason) = exceeded(&stage.budget, spent, &format!(" in stage {}", stage.name)) {
            match stage.budget.on_exceed {
                BudgetAction::Stop => stop_reason = stop_reason.or(Some(reason)),
                BudgetAction::Warn => warn_reason = warn_reason.or(Some(reason)),
            }
        }
    }
    if let Some(reason) = exceeded(&run.workflow.budget, &total, "") {
        match run.workflow.budget.on_exceed {
            BudgetAction::Stop => stop_reason = stop_reason.or(Some(reason)),
            BudgetAction::Warn => warn_reason = warn_reason.or(Some(reason)),
        }
    }
    if let Some(reason) = stop_reason {
        return BudgetCheck::Stop(format!(
            "{reason}. Inspect the run, then raise the cap to continue: model-router -C {} resume {} --budget-usd N (or --budget-tokens N)",
            run.project.display(),
            run.id
        ));
    }
    if let Some(reason) = warn_reason {
        return BudgetCheck::Warn(format!("Warning: {reason} (on_exceed = warn; continuing)"));
    }
    BudgetCheck::Ok
}

/// Why the run must stop before its next stage: a stage's or the whole
/// run's known cost or counted tokens exceed its cap. Unknown costs count
/// toward token caps only. Reads pricing files; never the network.
pub fn check_budget(run: &Run) -> Option<String> {
    match check_budget_action(run) {
        BudgetCheck::Stop(reason) => Some(reason),
        BudgetCheck::Ok | BudgetCheck::Warn(_) => None,
    }
}

pub fn check_budget_with(run: &Run, pricing: &Pricing) -> Option<String> {
    match check_budget_action_with(run, pricing) {
        BudgetCheck::Stop(reason) => Some(reason),
        BudgetCheck::Ok | BudgetCheck::Warn(_) => None,
    }
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

/// Spend that the live monitors of one scheduler share: the run's recorded
/// per-stage totals plus the latest streamed estimate of every process still
/// running, so a run or stage cap counts all concurrent workers, not only the
/// one checking.
///
/// The owner of the run calls [`LiveSpend::settle`] after it records a
/// finished process's usage on the run: that re-reads the recorded totals
/// and drops the process's live estimate in one step, so nothing is counted
/// twice or missed.
#[derive(Debug, Clone, Default)]
pub struct LiveSpend(Arc<Mutex<LiveSpendState>>);

#[derive(Debug, Default)]
struct LiveSpendState {
    pricing: Option<Pricing>,
    recorded: Vec<CostTotal>,
    /// Running process key -> its stage, latest estimate, and stop signal.
    live: HashMap<String, LiveEntry>,
}

#[derive(Debug)]
struct LiveEntry {
    stage: usize,
    spent: CostTotal,
    stop: Arc<StopSignal>,
}

/// Set when a cap a process answers to was exceeded, by it or by another
/// process counted with it.
#[derive(Debug, Default)]
struct StopSignal {
    flag: AtomicBool,
    reason: Mutex<Option<String>>,
}

impl StopSignal {
    fn raise(&self, reason: &str) {
        let mut slot = self.reason.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert_with(|| reason.to_owned());
        self.flag.store(true, AtomicOrdering::SeqCst);
    }
}

impl LiveSpend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prices estimates with `pricing` instead of the project's files.
    pub fn with_pricing(pricing: Pricing) -> Self {
        let spend = Self::default();
        spend.state().pricing = Some(pricing);
        spend
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LiveSpendState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pricing(&self, run: &Run) -> Pricing {
        let mut state = self.state();
        state
            .pricing
            .get_or_insert_with(|| Pricing::load(&run.project))
            .clone()
    }

    /// A monitor for an attempt of stage `index`, or `None` without an active
    /// `on_exceed = "stop"` cap. `key` names the process until it settles.
    pub fn monitor(&self, run: &Run, index: usize, key: &str) -> Option<LiveBudgetMonitor> {
        let model = run.workflow.stages.get(index)?.model.clone();
        let (provider, billing) = stage_estimate_params(run, index);
        let provider = provider.map(str::to_owned);
        self.register(run, index, key, model, provider, billing)
    }

    /// A monitor for a fan-out candidate of stage `index` running `model`,
    /// priced as `stage_totals` prices candidate attempts.
    pub fn candidate_monitor(
        &self,
        run: &Run,
        index: usize,
        model: &str,
        key: &str,
    ) -> Option<LiveBudgetMonitor> {
        let (provider, billing) = stage_estimate_params(run, index);
        let provider = provider.map(str::to_owned);
        self.register(run, index, key, model.to_owned(), provider, billing)
    }

    /// A monitor for a delegated task turn of stage `index` running `model`,
    /// priced under the task's own target as `task_turn_estimate` does.
    pub fn task_monitor(
        &self,
        run: &Run,
        index: usize,
        task: &crate::delegation::records::TaskState,
        model: &str,
        key: &str,
    ) -> Option<LiveBudgetMonitor> {
        let (provider, billing) = task_estimate_params(run, task);
        let provider = provider.map(str::to_owned);
        let model = if model.is_empty() {
            task.target.model.clone()
        } else {
            model.to_owned()
        };
        self.register(run, index, key, model, provider, billing)
    }

    fn register(
        &self,
        run: &Run,
        index: usize,
        key: &str,
        model: String,
        provider: Option<String>,
        billing: Billing,
    ) -> Option<LiveBudgetMonitor> {
        let stage = run.workflow.stages.get(index)?;
        let run_budget = (!run.workflow.budget.is_empty()
            && run.workflow.budget.on_exceed == BudgetAction::Stop)
            .then_some(run.workflow.budget);
        let stage_budget = (!stage.budget.is_empty()
            && stage.budget.on_exceed == BudgetAction::Stop)
            .then_some(stage.budget);
        if run_budget.is_none() && stage_budget.is_none() {
            return None;
        }
        let pricing = self.pricing(run);
        let recorded = totals_with_launched(run, &pricing);
        let stop = Arc::new(StopSignal::default());
        {
            let mut state = self.state();
            state.recorded = recorded;
            state.live.insert(
                key.to_owned(),
                LiveEntry {
                    stage: index,
                    spent: CostTotal::default(),
                    stop: stop.clone(),
                },
            );
        }
        Some(LiveBudgetMonitor {
            spend: self.clone(),
            stop,
            key: key.to_owned(),
            stage: index,
            run_budget,
            stage_budget,
            stage_name: stage.name.clone(),
            model,
            provider,
            billing,
            pricing,
            project: run.project.clone(),
            run_id: run.id.clone(),
        })
    }

    /// Process `key` ended and its usage is now recorded on `run`: re-reads
    /// the recorded totals and drops its live estimate together. A no-op for
    /// a process without a monitor.
    pub fn settle(&self, run: &Run, key: &str) {
        if !self.state().live.contains_key(key) {
            return;
        }
        let pricing = self.pricing(run);
        let recorded = totals_with_launched(run, &pricing);
        let mut state = self.state();
        state.recorded = recorded;
        state.live.remove(key);
    }

    /// Records `monitor`'s latest estimate and checks its caps against
    /// spend that counts every live process. An exceeded stage cap stops
    /// every live process of that stage; an exceeded run cap stops them all.
    fn update(&self, monitor: &LiveBudgetMonitor, estimate: CostTotal) -> Option<String> {
        let mut state = self.state();
        if let Some(entry) = state.live.get_mut(&monitor.key) {
            entry.spent = estimate;
        }
        let stage = monitor.stage;
        let mut stage_spent = state.recorded.get(stage).copied().unwrap_or_default();
        let mut run_spent = CostTotal::default();
        for total in &state.recorded {
            run_spent.merge(total);
        }
        for entry in state.live.values() {
            if entry.stage == stage {
                stage_spent.merge(&entry.spent);
            }
            run_spent.merge(&entry.spent);
        }
        let (reason, whole_run) = match monitor.stage_budget.as_ref().and_then(|budget| {
            exceeded(
                budget,
                &stage_spent,
                &format!(" in stage {}", monitor.stage_name),
            )
        }) {
            Some(reason) => (reason, false),
            None => (
                exceeded(monitor.run_budget.as_ref()?, &run_spent, "")?,
                true,
            ),
        };
        let reason = format!(
            "{reason}. Inspect the run, then raise the cap to continue: model-router -C {} resume {} --budget-usd N (or --budget-tokens N)",
            monitor.project.display(),
            monitor.run_id
        );
        for entry in state.live.values() {
            if whole_run || entry.stage == stage {
                entry.stop.raise(&reason);
            }
        }
        monitor.stop.raise(&reason);
        Some(reason)
    }
}

/// Checks streamed mid-attempt usage against configured `on_exceed = "stop"`
/// run and stage budget caps so an unattended harness can be terminated cleanly
/// the moment cumulative spend or tokens exceed the cap. Spend of other
/// processes running at the same time counts through the shared [`LiveSpend`].
#[derive(Debug, Clone)]
pub struct LiveBudgetMonitor {
    spend: LiveSpend,
    stop: Arc<StopSignal>,
    key: String,
    stage: usize,
    run_budget: Option<Budget>,
    stage_budget: Option<Budget>,
    stage_name: String,
    model: String,
    provider: Option<String>,
    billing: Billing,
    pricing: Pricing,
    project: PathBuf,
    run_id: String,
}

impl LiveBudgetMonitor {
    /// Builds a live monitor for stage `index` if the run or stage has an active
    /// `on_exceed = "stop"` cap. Returns `None` otherwise (zero overhead). It
    /// shares spend with no other process.
    pub fn for_stage(run: &Run, index: usize) -> Option<Self> {
        LiveSpend::new().monitor(run, index, "stage")
    }

    pub fn for_stage_with(run: &Run, index: usize, pricing: Pricing) -> Option<Self> {
        LiveSpend::with_pricing(pricing).monitor(run, index, "stage")
    }

    /// Evaluates cumulative spend/tokens with the running attempt's latest
    /// streamed `usage`. Returns the user-facing stop reason if a cap is exceeded.
    pub fn check(&self, live_usage: Usage) -> Option<String> {
        let est = estimate_with_billing(
            Some(&live_usage),
            &self.model,
            self.provider.as_deref(),
            self.billing,
            &self.pricing,
        );
        let mut own = CostTotal::default();
        own.add(&est);
        self.spend.update(self, own)
    }

    /// Raised once a cap this process answers to is exceeded, by its own
    /// usage or another live process's: pass it to the capture as an abort
    /// flag so a quiet sibling stops too.
    pub fn stop_flag(&self) -> &AtomicBool {
        &self.stop.flag
    }

    /// The budget reason that stopped this process, if any.
    pub fn stopped(&self) -> Option<String> {
        self.stop
            .reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Whether the run stopped on a budget cap.
pub fn budget_blocked(run: &Run) -> bool {
    run.status == Status::Blocked
        && run
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with(EXCEEDED))
}

/// Whether the run stopped on a USD cap, not a token cap: the amount in
/// its error is in dollars. Reads no pricing files.
pub fn usd_budget_blocked(run: &Run) -> bool {
    budget_blocked(run)
        && run.error.as_deref().is_some_and(|e| {
            e[EXCEEDED.len()..]
                .trim_start_matches(": ")
                .trim_start_matches('~')
                .starts_with('$')
        })
}

/// For a run its budget stopped: the USD cap a raise must exceed, and the
/// run's spend so far. Errors when the run is not stopped by a budget, or
/// only a token cap stopped it.
pub fn raise_floor(run: &Run) -> Result<(f64, CostTotal)> {
    ensure!(
        budget_blocked(run),
        "Only a run stopped by its budget can raise it"
    );
    let pricing = Pricing::load(&run.project);
    let stages = stage_totals(run, &pricing);
    let (cap, total) = exceeded_usd_cap_from(run, &stages).with_context(|| {
        format!(
            "A token cap stopped this run; raise it with: model-router -C {} resume {} --budget-tokens N",
            run.project.display(),
            run.id
        )
    })?;
    Ok((cap, total))
}

/// The USD cap a raise must exceed (the run's cap, or the highest exceeded
/// stage cap when larger) together with the summed spend. None unless a USD
/// cap is exceeded.
fn exceeded_usd_cap_from(run: &Run, stages: &[CostTotal]) -> Option<(f64, CostTotal)> {
    let mut total = CostTotal::default();
    let mut exceeded = None::<f64>;
    for (stage, spent) in run.workflow.stages.iter().zip(stages) {
        total.merge(spent);
        if let Some(cap) = stage.budget.max_usd.filter(|&cap| spent.usd > cap) {
            exceeded = Some(exceeded.map_or(cap, |e| e.max(cap)));
        }
    }
    let run_cap = run.workflow.budget.max_usd;
    if let Some(cap) = run_cap.filter(|&cap| total.usd > cap) {
        exceeded = Some(exceeded.map_or(cap, |e| e.max(cap)));
    }
    exceeded.map(|cap| (run_cap.map_or(cap, |run_cap| run_cap.max(cap)), total))
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

fn is_fresh(cache: &ProviderCache, name: &str, provider: &Provider, now: DateTime<Utc>) -> bool {
    cache.providers.get(name).is_some_and(|cached| {
        cached.base_url == provider.base_url && now - cached.fetched_at < Duration::days(1)
    })
}

fn insert_cached(
    cache: &mut ProviderCache,
    name: &str,
    provider: &Provider,
    models: &[ModelInfo],
    now: DateTime<Utc>,
) -> bool {
    let models = listed_prices(models);
    if models.is_empty() {
        return false;
    }
    cache.providers.insert(
        name.to_owned(),
        CachedPrices {
            fetched_at: now,
            base_url: provider.base_url.clone(),
            models,
        },
    );
    true
}

fn save_cache(project: &Path, cache: &ProviderCache) {
    if let Ok(dir) = storage::metadata_dir(project) {
        let _ = storage::write_json_fast(&dir.join(CACHE_FILE), cache);
    }
}

/// Refreshes the cached `/models` prices of `providers` older than a day
/// (or whose endpoint changed), fetching stale providers in parallel. Failures,
/// e.g. offline, keep the old cache and are otherwise ignored.
pub fn refresh_provider_prices(project: &Path, providers: &BTreeMap<String, Provider>) {
    if providers.is_empty() {
        return;
    }
    let now = Utc::now();
    let mut cache = read_cache(project);
    let stale: Vec<(&String, &Provider)> = providers
        .iter()
        .filter(|(name, provider)| !is_fresh(&cache, name, provider, now))
        .collect();
    if stale.is_empty() {
        return;
    }
    let fetched: Vec<(&String, &Provider, Result<Vec<ModelInfo>>)> = if stale.len() == 1 {
        let (name, provider) = stale[0];
        vec![(
            name,
            provider,
            providers::models_within(provider, std::time::Duration::from_secs(5)),
        )]
    } else {
        std::thread::scope(|scope| {
            let jobs: Vec<_> = stale
                .into_iter()
                .map(|(name, provider)| {
                    scope.spawn(move || {
                        (
                            name,
                            provider,
                            providers::models_within(provider, std::time::Duration::from_secs(5)),
                        )
                    })
                })
                .collect();
            jobs.into_iter().filter_map(|job| job.join().ok()).collect()
        })
    };
    let mut changed = false;
    for (name, provider, result) in fetched {
        if let Ok(models) = result {
            changed |= insert_cached(&mut cache, name, provider, &models, now);
        }
    }
    if changed {
        save_cache(project, &cache);
    }
}

#[cfg(test)]
fn refresh_with(
    project: &Path,
    providers: &BTreeMap<String, Provider>,
    now: DateTime<Utc>,
    fetch: impl Fn(&Provider) -> Result<Vec<ModelInfo>>,
) {
    let mut cache = read_cache(project);
    let mut changed = false;
    for (name, provider) in providers {
        if is_fresh(&cache, name, provider, now) {
            continue;
        }
        if let Ok(models) = fetch(provider) {
            changed |= insert_cached(&mut cache, name, provider, &models, now);
        }
    }
    if changed {
        save_cache(project, &cache);
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projects: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_projects: Option<usize>,
    pub rows: Vec<Row>,
    pub usage: Usage,
    pub total: CostTotal,
}

type RowEntry = (Row, BTreeSet<(usize, String)>);

#[derive(Default)]
struct Accumulator {
    rows: BTreeMap<(String, String), RowEntry>,
    usage: Usage,
    total: CostTotal,
}

impl Accumulator {
    fn fold_runs<'a>(
        &mut self,
        project_idx: usize,
        runs: impl IntoIterator<Item = &'a Run>,
        since: Option<DateTime<Utc>>,
        pricing: &Pricing,
    ) {
        let mut add =
            |run: &Run, harness: &str, model: &str, usage: Option<Usage>, estimate: Estimate| {
                let model = if model.is_empty() {
                    "default".to_owned()
                } else {
                    model.to_owned()
                };
                let (row, ids) = self
                    .rows
                    .entry((harness.to_owned(), model.clone()))
                    .or_insert_with(|| {
                        (
                            Row {
                                harness: harness.to_owned(),
                                model,
                                runs: 0,
                                usage: Usage::default(),
                                cost: CostTotal::default(),
                            },
                            BTreeSet::new(),
                        )
                    });
                ids.insert((project_idx, run.id.clone()));
                row.cost.add(&estimate);
                self.total.add(&estimate);
                if let Some(usage) = usage {
                    row.usage = row.usage + usage;
                    self.usage = self.usage + usage;
                }
            };
        for run in runs {
            for (index, (stage, state)) in run.workflow.stages.iter().zip(&run.stages).enumerate() {
                let (provider, billing) = stage_estimate_params(run, index);
                // Candidates count under their own harness.
                let candidates = state
                    .candidate_attempts()
                    .map(|(c, a)| (c.harness.as_str(), a));
                let own = state.attempts.iter().map(|a| (stage.harness.as_str(), a));
                for (harness, attempt) in own.chain(candidates) {
                    if since.is_some_and(|since| attempt.started_at < since) {
                        continue;
                    }
                    let estimate = estimate_with_billing(
                        attempt.usage.as_ref(),
                        &attempt.model,
                        provider,
                        billing,
                        pricing,
                    );
                    add(run, harness, &attempt.model, attempt.usage, estimate);
                }
                // Delegated tasks count under their own targets.
                for (task, turn) in state.task_turns() {
                    if since.is_some_and(|since| turn.started_at < since) {
                        continue;
                    }
                    let estimate = task_turn_estimate(run, task, turn, pricing);
                    let model = if turn.model.is_empty() {
                        &task.target.model
                    } else {
                        &turn.model
                    };
                    add(run, &task.target.harness, model, turn.usage, estimate);
                }
            }
        }
    }

    fn finish(
        self,
        since: Option<DateTime<Utc>>,
        projects: Option<usize>,
        skipped_projects: Option<usize>,
    ) -> Summary {
        let rows = self
            .rows
            .into_values()
            .map(|(mut row, ids)| {
                row.runs = ids.len();
                row
            })
            .collect();
        Summary {
            since,
            prices_as_of: table_as_of().into(),
            projects,
            skipped_projects,
            rows,
            usage: self.usage,
            total: self.total,
        }
    }
}

/// Spend of attempts started at or after `since`, by harness and model.
pub fn aggregate<'a>(
    runs: impl IntoIterator<Item = &'a Run>,
    since: Option<DateTime<Utc>>,
    pricing: &Pricing,
) -> Summary {
    let mut acc = Accumulator::default();
    acc.fold_runs(0, runs, since, pricing);
    acc.finish(since, None, None)
}

/// Spend across multiple projects (`cost --all-projects`), using each project's
/// own pricing overrides and cached provider prices. Missing or unreadable
/// projects are skipped gracefully and counted in `skipped_projects`.
pub fn aggregate_projects(
    projects: &[PathBuf],
    since: Option<DateTime<Utc>>,
    refresh_prices: bool,
) -> Summary {
    let mut acc = Accumulator::default();
    let mut included = 0usize;
    let mut skipped = 0usize;
    for (idx, project) in projects.iter().enumerate() {
        if !project.is_dir() {
            skipped += 1;
            continue;
        }
        let Ok(runs) = crate::engine::history(project) else {
            skipped += 1;
            continue;
        };
        included += 1;
        if refresh_prices {
            let mut routed = BTreeMap::new();
            for run in &runs {
                routed.extend(routed_providers(run));
            }
            refresh_provider_prices(project, &routed);
        }
        let pricing = Pricing::load(project);
        acc.fold_runs(idx, &runs, since, &pricing);
    }
    acc.finish(since, Some(included), (skipped > 0).then_some(skipped))
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
    let scope = match (summary.projects, summary.skipped_projects) {
        (Some(n), Some(skipped)) => format!(" across {n} project(s) ({skipped} skipped)"),
        (Some(n), None) => format!(" across {n} project(s)"),
        (None, _) => String::new(),
    };
    let _ = writeln!(
        text,
        "Spend {}{scope} · prices are estimates (table as of {}); verify with your provider",
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
    if summary.total.known() {
        let mut split = vec![];
        if summary.total.api.attempts > 0 {
            split.push(format!(
                "billed API {}",
                report::dollars(summary.total.api.usd)
            ));
        }
        if summary.total.subscription.attempts > 0 {
            split.push(format!(
                "subscription API-equivalent {}",
                report::dollars(summary.total.subscription.usd)
            ));
        }
        if !split.is_empty() {
            let _ = writeln!(text, "Billing: {}", split.join(" · "));
        }
    }
    if summary.total.unknown > 0 {
        let _ = writeln!(
            text,
            "Note: {} of {} attempt(s) had unknown cost (missing usage or unpriced model; not counted as $0).",
            summary.total.unknown, summary.total.attempts
        );
    }
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
        assert_eq!(table_price("gpt-6-astra"), Some((2.0, 12.0, Source::Table)));
        assert_eq!(table_price("gpt-6.2-sol"), Some((2.5, 15.0, Source::Table)));
        assert_eq!(table_price("haiku").map(|p| p.1), Some(0.5));
        // Family matching respects segment boundaries and never invents a price.
        assert_eq!(table_price("o30"), None);
        assert_eq!(table_price("nonexistent-model-xyz"), None);
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
            billing: Billing::Api,
            tokens: 5,
        });
        assert_eq!(
            (total.text(), total.short()),
            ("cost unknown".into(), "?".into())
        );
        total.add(&Estimate {
            usd: Some(0.25),
            source: Source::Reported,
            billing: Billing::Api,
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
            billing: Billing::Subscription,
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

    #[test]
    fn later_overrides_replace_normalized_keys_and_family_picks_longest_boundary() {
        let chained = Pricing::default()
            .with_override("claude-sonnet-5.5", price(1.0, 2.0))
            .with_override("claude-sonnet-5-5", price(3.0, 4.0));
        let (p, _) = chained.lookup("claude-sonnet-5.5", None).unwrap();
        assert_eq!(p.input, 3.0);

        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".model-router.toml"),
            "[pricing]\n\"gpt\" = { input = 1.0, output = 1.0 }\n\"gpt-5-mini\" = { input = 2.0, output = 2.0 }\n",
        )
        .unwrap();
        let pricing = Pricing::load(dir.path());
        let (family, _) = pricing.lookup("gpt-5-mini-20260101", None).unwrap();
        assert_eq!(family.input, 2.0);
        assert_eq!(pricing.lookup("o-missing", None), None);
    }

    #[test]
    fn live_budget_monitor_triggers_on_stop_caps_and_ignores_warn_caps() {
        let mut run = crate::report::tests::sample();
        run.workflow.budget = Budget {
            max_tokens: Some(500),
            on_exceed: BudgetAction::Warn,
            ..Budget::default()
        };
        assert!(
            LiveBudgetMonitor::for_stage_with(&run, 0, Pricing::default()).is_none(),
            "warn caps never abort mid-stream"
        );
        run.workflow.budget.on_exceed = BudgetAction::Stop;
        let monitor = LiveBudgetMonitor::for_stage_with(&run, 0, Pricing::default()).unwrap();
        // sample() already has prior token spend on stage 0; adding 500 tokens exceeds 500.
        let reason = monitor
            .check(Usage {
                input_tokens: Some(400),
                output_tokens: Some(200),
                ..Usage::default()
            })
            .unwrap();
        assert!(reason.starts_with(EXCEEDED), "{reason}");
    }

    fn tokens(input: u64) -> Usage {
        Usage {
            input_tokens: Some(input),
            ..Usage::default()
        }
    }

    #[test]
    fn live_monitors_count_every_concurrent_process_toward_the_run_cap() {
        // sample() records 2,400 tokens; 1,000 more are allowed.
        let mut run = crate::report::tests::sample();
        run.workflow.budget = Budget {
            max_tokens: Some(3_400),
            on_exceed: BudgetAction::Stop,
            ..Budget::default()
        };
        let spend = LiveSpend::with_pricing(Pricing::default());
        let a = spend.monitor(&run, 1, "a").unwrap();
        let b = spend.monitor(&run, 2, "b").unwrap();
        assert_eq!(a.check(tokens(600)), None);
        assert!(a.stopped().is_none());
        // Alone each is under the cap; together they exceed it.
        let reason = b.check(tokens(600)).unwrap();
        assert!(reason.starts_with(EXCEEDED), "{reason}");
        // The quiet sibling is told to stop too, with the same reason.
        assert!(a.stop_flag().load(AtomicOrdering::SeqCst));
        assert_eq!(a.stopped(), Some(reason));
    }

    #[test]
    fn settling_a_finished_process_counts_its_usage_once() {
        let mut run = crate::report::tests::sample();
        run.workflow.budget = Budget {
            max_tokens: Some(3_900),
            on_exceed: BudgetAction::Stop,
            ..Budget::default()
        };
        let spend = LiveSpend::with_pricing(Pricing::default());
        let a = spend.monitor(&run, 1, "a").unwrap();
        let b = spend.monitor(&run, 2, "b").unwrap();
        assert_eq!(a.check(tokens(600)), None);
        // `a` ends: its 600 tokens are recorded on the run, then settled.
        run.stages[1].attempts[0].usage = Some(Usage {
            input_tokens: Some(1_400),
            output_tokens: Some(100),
            ..Usage::default()
        });
        spend.settle(&run, "a");
        // 3,000 recorded + 600 live: under 3,900 unless `a` counted twice.
        assert_eq!(b.check(tokens(600)), None);
        assert!(b.check(tokens(1_000)).is_some());
    }

    #[test]
    fn a_stage_cap_counts_every_live_process_of_that_stage_only() {
        let mut run = crate::report::tests::sample();
        run.workflow.stages[2].budget = Budget {
            max_tokens: Some(1_000),
            on_exceed: BudgetAction::Stop,
            ..Budget::default()
        };
        let spend = LiveSpend::with_pricing(Pricing::default());
        let one = spend.candidate_monitor(&run, 2, "m1", "one").unwrap();
        let two = spend.candidate_monitor(&run, 2, "m2", "two").unwrap();
        assert!(
            spend.monitor(&run, 1, "other").is_none(),
            "no cap on stage 1"
        );
        assert_eq!(one.check(tokens(600)), None);
        let reason = two.check(tokens(600)).unwrap();
        assert!(reason.contains("in stage Review"), "{reason}");
        assert!(one.stopped().is_some());
    }
}
