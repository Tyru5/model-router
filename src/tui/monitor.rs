use super::{
    actions::ActionId,
    app::App,
    completion::{draw_completion, draw_next, finished, next_height},
    handoff_view::HandoffView,
    modal::{Ask, Choice, InputTarget, Loading, Modal, PickValue},
    output_search::{Wrapped, format_search_status, visible_lines},
    run_slot::{RunSlot, SETTLE},
    theme::{Palette, Theme},
    widgets::{StripStage, elapsed, empty_state, fit, key_chips, rows_for, stage_strip, textarea},
};
use crate::{
    adapters,
    config::Mode,
    engine::{self, Cmd, Status},
    storage,
};
use anyhow::{Context, Result, bail, ensure};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use std::{collections::VecDeque, rc::Rc};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

impl App {
    pub(super) fn perform_monitor(&mut self, id: ActionId) -> Result<()> {
        match id {
            ActionId::CancelStage => {
                let run = self.session.run().context("No run selected")?;
                self.ui.open(Modal::Ask(Box::new(Ask::CancelStage {
                    run: run.id.clone(),
                })));
            }
            ActionId::TakeOver => {
                let slot = self.session.focused().context("No run selected")?;
                if slot.terminal().is_some() {
                    self.session.launch_interactive = true;
                    return Ok(());
                }
                let run = &slot.run;
                // A stage waiting for its terminal opens as it is.
                if slot.worker.is_none() && run.status == Status::Interactive {
                    self.session.launch_interactive = true;
                    return Ok(());
                }
                let stage = run
                    .workflow
                    .stages
                    .get(run.cursor)
                    .context("Run is complete")?;
                ensure!(
                    adapters::resumable(&stage.harness, &run.adapters),
                    "{} sessions cannot be resumed; [i] retries the stage interactively",
                    stage.harness
                );
                if slot.worker.is_some() {
                    ensure!(
                        stage.mode == Mode::Unattended,
                        "This stage is already interactive"
                    );
                    self.ui.open(Modal::Ask(Box::new(Ask::TakeOver {
                        run: run.id.clone(),
                    })));
                } else {
                    self.session
                        .run_mut()
                        .context("No run")?
                        .command(Cmd::TakeOver)?;
                    self.session.launch_interactive = true;
                }
            }
            ActionId::CloseRun => {
                self.session.dismiss()?;
                self.runs.stale = true;
                self.ui.message =
                    "Run closed here; it moves to Saved, where [Enter] reopens it.".into();
            }
            ActionId::ScrollUp if self.results_shown() => {
                self.view(|slot| slot.results_scroll = slot.results_scroll.saturating_sub(5))
            }
            ActionId::ScrollDown if self.results_shown() => self.view(|slot| {
                slot.results_scroll = slot
                    .results_scroll
                    .saturating_add(5)
                    .min(slot.results_limit.get())
            }),
            // Scrolling up pauses the output; reaching the end follows again.
            ActionId::ScrollUp => self.view(|slot| slot.scroll_output(-10)),
            ActionId::ScrollDown => self.view(|slot| slot.scroll_output(10)),
            ActionId::OutputTop if self.results_shown() => {
                self.view(|slot| slot.results_scroll = 0)
            }
            ActionId::OutputBottom if self.results_shown() => {
                self.view(|slot| slot.results_scroll = slot.results_limit.get())
            }
            ActionId::OutputTop => self.view(RunSlot::output_home),
            ActionId::OutputBottom => self.view(RunSlot::output_end),
            ActionId::FollowOutput if self.results_shown() => self.view(|slot| {
                slot.show_output = true;
                slot.output_end();
            }),
            ActionId::FollowOutput => {
                let message = self.session.focused_mut().and_then(RunSlot::toggle_follow);
                if let Some(message) = message {
                    self.ui.message = message;
                }
            }
            ActionId::FindOutput => self.view(RunSlot::start_output_search),
            ActionId::OpenLog => {
                let path = self.focused_log().context("No attempt has output yet")?;
                self.ui.external = Some(super::external::page_file(&path)?);
            }
            ActionId::Diff => self.show_diff()?,
            ActionId::ApplyWorktree => {
                let run = self.session.run().context("No run selected")?.clone();
                let loading = Loading::new("Planning worktree apply…");
                let ticket = loading.ticket;
                let id = run.id.clone();
                self.tasks.spawn(
                    "Planning worktree apply stopped unexpectedly",
                    move || engine::apply_worktree_plan(&run).map_err(|e| format!("{e:#}")),
                    move |result| super::tasks::Msg::ApplyPlan {
                        ticket,
                        run: id,
                        result,
                    },
                );
                self.ui.open(Modal::Loading(loading));
            }
            ActionId::ToggleResults => self.toggle_results()?,
            ActionId::Handoff => {
                let run = self.session.run().context("No run selected")?;
                let index = run.cursor.min(run.stages.len().saturating_sub(1));
                let found = run
                    .stages
                    .get(index)
                    .and_then(|s| s.handoff.as_ref().map(|h| (index, h)))
                    .or_else(|| {
                        run.stages
                            .iter()
                            .enumerate()
                            .rev()
                            .find_map(|(i, s)| s.handoff.as_ref().map(|h| (i, h)))
                    });
                match found {
                    Some((index, handoff)) => {
                        let stage = run
                            .workflow
                            .stages
                            .get(index)
                            .map_or_else(String::new, |s| s.name.clone());
                        self.ui.open(Modal::Handoff(Box::new(HandoffView::new(
                            handoff.clone(),
                            stage,
                        ))));
                    }
                    None => {
                        let body = format!(
                            "No completed handoff yet.\n\n{}\n\nArtifacts: {}",
                            run.error.as_deref().unwrap_or(""),
                            run.directory.display()
                        );
                        self.text("Handoff · [↑↓] scroll · [Esc] closes", body);
                    }
                }
            }
            ActionId::Continue => {
                // `c` pressed as a stage stops was likely meant for the
                // running stage, so it must not start the next one.
                ensure!(
                    self.session
                        .focused()
                        .and_then(|slot| slot.stopped_at)
                        .is_none_or(|at| at.elapsed() >= SETTLE),
                    "The stage just stopped; press [c] again to continue"
                );
                self.start_driver()?
            }
            ActionId::Advance => {
                let explicit = self.editor.settings.explicit_config.clone();
                self.session
                    .focused_mut()
                    .context("No run")?
                    .authorize_replay(explicit.as_deref())?;
                self.session
                    .run_mut()
                    .context("No run")?
                    .command(Cmd::Confirm)?;
                self.start_driver()?;
            }
            ActionId::Retry => {
                self.session
                    .run_mut()
                    .context("No run")?
                    .command(Cmd::Retry {
                        interactive: false,
                        model: None,
                    })?;
                self.start_driver()?;
            }
            ActionId::RetryInteractive => {
                let run = self.session.run_mut().context("No run")?;
                // A stage already waiting for the terminal opens as it is.
                if run.status == Status::Interactive {
                    self.session.launch_interactive = true;
                } else {
                    run.command(Cmd::Retry {
                        interactive: true,
                        model: None,
                    })?;
                    self.start_driver()?;
                }
            }
            ActionId::RetryCandidate => {
                let run = self.session.run().context("No run selected")?;
                let items: Vec<Choice> = retryable_candidates(run)
                    .into_iter()
                    .map(|(index, candidate)| Choice {
                        label: format!(
                            "{} · {} · {} · {}",
                            run.workflow.stages[index].name,
                            candidate.id,
                            if candidate.model.is_empty() {
                                candidate.harness.clone()
                            } else {
                                format!("{} {}", candidate.harness, candidate.model)
                            },
                            candidate.status.label()
                        ),
                        value: PickValue::Candidate {
                            stage: run.workflow.stages[index].id.clone(),
                            candidate: candidate.id.clone(),
                        },
                    })
                    .collect();
                ensure!(
                    !items.is_empty(),
                    "No failed fan-out candidate to retry; [y] retries the whole stage"
                );
                self.ui.open(Modal::Picker {
                    title: "Retry a fan-out candidate".into(),
                    items,
                    selected: 0,
                    filter: textarea(""),
                    note: "Reruns the candidate from the fan-out base; the others keep their results and the judge chooses again.".into(),
                });
            }
            ActionId::AllowRepair => {
                let run = self.session.run().context("No run selected")?;
                ensure!(
                    run.repair_blocked(),
                    "Only a run stopped by its repair limit can get another repair pass"
                );
                self.ui.open(Modal::Ask(Box::new(Ask::AllowRepair {
                    run: run.id.clone(),
                    passes: run.repair_passes,
                    limit: run.repair_limit(),
                })));
            }
            ActionId::RaiseBudget => {
                let run = self.session.run().context("No run selected")?;
                match budget_raise_floor(run)? {
                    BudgetRaiseFloor::Usd { cap, spent } => self.input(
                        &format!(
                            "New USD budget (cap {}, spent {})",
                            crate::report::dollars(cap),
                            spent.amount()
                        ),
                        InputTarget::Budget,
                        "",
                    ),
                    BudgetRaiseFloor::Tokens { cap, spent } => self.input(
                        &format!(
                            "New token budget (cap {}, spent {} tokens)",
                            crate::report::tokens(cap),
                            crate::report::tokens(spent.tokens)
                        ),
                        InputTarget::BudgetTokens,
                        "",
                    ),
                }
            }
            ActionId::ReplaceModel => {
                let run = self.session.run().context("No run")?;
                let stage = run
                    .workflow
                    .stages
                    .get(run.cursor)
                    .context("Run is complete")?;
                self.input(
                    "Replace model / mode and retry this stage",
                    InputTarget::RetryModel,
                    &stage.model.clone(),
                );
            }
            ActionId::EditSummary => {
                let run = self.session.run().context("No run")?;
                ensure!(
                    run.status == Status::AwaitingConfirmation,
                    "Only interactive handoffs can be edited"
                );
                let summary = run
                    .stages
                    .get(run.cursor)
                    .and_then(|s| s.handoff.as_ref())
                    .map(|h| h.summary.clone())
                    .unwrap_or_default();
                self.input(
                    "Edit interactive handoff summary",
                    InputTarget::Handoff,
                    &summary,
                );
            }
            other => bail!("{other:?} is not a monitor action"),
        }
        Ok(())
    }
}

impl App {
    /// Changes the focused run's view; nothing without a run.
    fn view(&mut self, change: impl FnOnce(&mut RunSlot)) {
        if let Some(slot) = self.session.focused_mut() {
            change(slot);
        }
    }

    /// Keys for a search in the focused run's output; false passes the key on.
    pub(super) fn output_search_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let handled = self
            .session
            .focused_mut()
            .and_then(|slot| slot.output_search_key(key));
        if let Some(Some(message)) = &handled {
            self.ui.message = message.clone();
        }
        handled.is_some()
    }

    /// stdout.log of the focused attempt: the current stage's latest, else
    /// the latest of any stage.
    pub(super) fn focused_log(&self) -> Option<std::path::PathBuf> {
        let run = self.session.run()?;
        let index = run.cursor.min(run.stages.len().saturating_sub(1));
        let attempt = run
            .stages
            .get(index)
            .and_then(|s| s.attempts.last())
            .or_else(|| {
                run.stages
                    .iter()
                    .flat_map(|s| &s.attempts)
                    .max_by_key(|a| a.started_at)
            })?;
        Some(attempt.directory.join("stdout.log"))
    }
}

/// Floor that a raised budget cap must exceed for a budget-blocked run.
pub(super) enum BudgetRaiseFloor {
    Usd {
        cap: f64,
        spent: crate::cost::CostTotal,
    },
    Tokens {
        cap: u64,
        spent: crate::cost::CostTotal,
    },
}

/// Computes the cap and spend floor for a run blocked by either a USD or token budget.
pub(super) fn budget_raise_floor(run: &engine::Run) -> Result<BudgetRaiseFloor> {
    ensure!(
        crate::cost::budget_blocked(run),
        "Only a run stopped by its budget can raise it"
    );
    if crate::cost::usd_budget_blocked(run) {
        let (cap, spent) = crate::cost::raise_floor(run)?;
        return Ok(BudgetRaiseFloor::Usd { cap, spent });
    }
    let pricing = crate::cost::Pricing::load(&run.project);
    let stages = crate::cost::stage_totals(run, &pricing);
    let mut total = crate::cost::CostTotal::default();
    let mut exceeded = None::<u64>;
    for (stage, spent) in run.workflow.stages.iter().zip(&stages) {
        total.merge(spent);
        if let Some(cap) = stage.budget.max_tokens.filter(|&cap| spent.tokens > cap) {
            exceeded = Some(exceeded.map_or(cap, |e| e.max(cap)));
        }
    }
    let run_cap = run.workflow.budget.max_tokens;
    if let Some(cap) = run_cap.filter(|&cap| total.tokens > cap) {
        exceeded = Some(exceeded.map_or(cap, |e| e.max(cap)));
    }
    if let Some(cap) = exceeded {
        let floor = run_cap.map_or(cap, |rc| rc.max(cap));
        return Ok(BudgetRaiseFloor::Tokens {
            cap: floor,
            spent: total,
        });
    }
    // Fall back to USD raise_floor if error text didn't start with $ but USD cap is exceeded.
    let (cap, spent) = crate::cost::raise_floor(run)?;
    Ok(BudgetRaiseFloor::Usd { cap, spent })
}

/// Stopped candidates the run can retry, with their stage's index: those of
/// its stopped fan-out stage, or of any stopped fan-out stage of a graph run.
fn retryable_candidates(run: &engine::Run) -> Vec<(usize, &engine::CandidateState)> {
    let stopped = |status| matches!(status, Status::Failed | Status::Blocked | Status::Cancelled);
    if !stopped(run.status) {
        return vec![];
    }
    run.stages
        .iter()
        .enumerate()
        .filter(|(index, state)| (run.graph() || *index == run.cursor) && stopped(state.status))
        .filter_map(|(index, state)| Some((index, state.fan_out.as_ref()?)))
        .flat_map(|(index, fan)| {
            fan.candidates
                .iter()
                .filter(|candidate| stopped(candidate.status))
                .map(move |candidate| (index, candidate))
        })
        .collect()
}

/// Output lines wrapped to `width` columns and coloured by what they show:
/// assistant text, tool calls, results, and errors, indented under a
/// heading for each stage attempt. `first` is the absolute number of the
/// first log line.
pub(super) fn output_lines(
    logs: &VecDeque<String>,
    first: u64,
    width: usize,
    palette: Palette,
) -> Wrapped {
    let width = width.max(10);
    let mut lines = Vec::new();
    let mut starts = Vec::with_capacity(logs.len());
    let mut style = Style::default().fg(palette.muted);
    for log in logs {
        starts.push(lines.len());
        if let Some(heading) = log.strip_prefix("── ") {
            // A blank row sets each stage apart from the one before.
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(stage_heading(heading, width, palette));
            style = Style::default().fg(palette.muted);
            continue;
        }
        style = if log.starts_with("● ") {
            Style::default().fg(palette.fg)
        } else if log.starts_with("⚙ ") || log.starts_with("$ ") || log.starts_with("⌕ ") {
            Style::default().fg(palette.blue)
        } else if log.starts_with("✎ ") {
            Style::default().fg(palette.warn)
        } else if log.starts_with("✓ ") {
            Style::default().fg(palette.success)
        } else if log.starts_with("✗ ") || log.starts_with("[stderr]") || log.contains("↳ ✗")
        {
            Style::default().fg(palette.red)
        } else if log.starts_with("∴ ") {
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::ITALIC)
        } else if log.starts_with("  ") && !log.starts_with("  ↳") {
            // A message's later lines keep its style.
            style
        } else {
            Style::default().fg(palette.muted)
        };
        for row in wrap(log, width - 2) {
            lines.push(Line::styled(format!("  {row}"), style));
        }
    }
    Wrapped {
        lines,
        starts,
        first,
    }
}

/// A `── Implement · attempt 2 · claude sonnet ──` log line as a feed
/// heading: the stage's name, then the rest faint.
fn stage_heading(text: &str, width: usize, palette: Palette) -> Line<'static> {
    let text = text.trim_end().trim_end_matches('─').trim_end();
    let (name, meta) = text.split_once(" · ").unwrap_or((text, ""));
    let name = fit(name, width);
    let room = width.saturating_sub(name.width() + 2);
    let mut spans = vec![Span::styled(
        name,
        Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
    )];
    if !meta.is_empty() && room >= 3 {
        spans.push(Span::styled(
            format!("  {}", fit(&meta.replace(" · ", "  "), room)),
            Style::default().fg(palette.muted),
        ));
    }
    Line::from(spans)
}

/// `text` in rows of at most `width` display columns; wide characters
/// (CJK, emoji) take two. An empty line is one empty row.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = vec![];
    let mut row = String::new();
    let mut used = 0;
    for c in text.chars() {
        let size = c.width().unwrap_or(0);
        if used + size > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(c);
        used += size;
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

/// "  ·  cost ~$0.42 table of $5.00": spend so far, updated as each attempt
/// records its usage; empty before any attempt without a budget.
fn running_cost(slot: &RunSlot) -> String {
    let report = super::completion::cached_report(slot);
    let (cost, budget) = (report.usage.cost, report.budget);
    if cost.attempts == 0 && budget.is_empty() {
        return String::new();
    }
    let mut text = format!("  ·  cost {}", cost.short());
    if cost.known() {
        text.push_str(&format!(" {}", cost.sources()));
    }
    if let Some(cap) = budget.max_usd {
        text.push_str(&format!(" of {}", crate::report::dollars(cap)));
    }
    if let Some(cap) = budget.max_tokens {
        text.push_str(&format!(
            " · {} of {} tokens",
            crate::report::tokens(cost.tokens),
            crate::report::tokens(cap)
        ));
    }
    text
}

/// Seconds from the run's start to its latest attempt's end, counting an
/// attempt still running up to now.
fn wall_seconds(run: &engine::Run) -> i64 {
    let now = chrono::Utc::now();
    let end = run
        .stages
        .iter()
        .flat_map(|s| &s.attempts)
        .map(|a| a.finished_at.unwrap_or(now))
        .max()
        .unwrap_or(run.updated_at);
    (end - run.created_at).num_seconds().max(0)
}

/// The run's title left; its status, time, and spend right. A narrow pane
/// cuts the title, not the status.
fn header(slot: &RunSlot, width: u16, palette: Palette) -> Line<'static> {
    let run = &slot.run;
    let muted = Style::default().fg(palette.muted);
    let mut right = vec![
        palette.status_span(run.status),
        Span::styled(
            format!("  {}{}", elapsed(wall_seconds(run)), running_cost(slot)),
            muted,
        ),
    ];
    if run.repair_passes > 0 {
        right.push(Span::styled(
            format!(
                "  ·  repairs {}/{}",
                run.repair_passes, run.workflow.max_repair_passes
            ),
            muted,
        ));
    }
    if slot.terminal().is_some() {
        right.push(Span::styled(
            "  ·  detached [t]",
            Style::default().fg(palette.accent),
        ));
    }
    let right = Line::from(right);
    let width = usize::from(width);
    let title = fit(
        &super::history::title(&run.task, &run.workflow.name),
        width.saturating_sub(right.width() + 2),
    );
    let gap = width.saturating_sub(title.width() + right.width()).max(1);
    let mut spans = vec![
        Span::styled(
            title,
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap)),
    ];
    spans.extend(right.spans);
    Line::from(spans)
}

/// The run's stages in a strip, the current one highlighted; ◆ marks a
/// stage waiting on the person.
fn strip(slot: &RunSlot, width: u16, palette: Palette) -> Line<'static> {
    let run = &slot.run;
    let stages: Vec<StripStage> = run
        .stages
        .iter()
        .zip(&run.workflow.stages)
        .enumerate()
        .map(|(index, (state, stage))| {
            let waiting = index == run.cursor && slot.needs_input();
            StripStage {
                glyph: if waiting {
                    "◆"
                } else {
                    Palette::status_glyph(state.status)
                },
                color: if waiting {
                    palette.warn
                } else {
                    palette.status_color(state.status)
                },
                name: stage.name.clone(),
                on: stage.enabled && state.status != Status::Skipped,
            }
        })
        .collect();
    stage_strip(&stages, run.cursor, width.into(), palette)
}

/// One line per fan-out stage: its candidates, ★ marking the judge's choice.
/// Models are left out when the line is wider than `width`, so every
/// candidate's harness and status show.
fn candidate_lines(run: &engine::Run, width: u16, palette: Palette) -> Vec<Line<'static>> {
    let muted = Style::default().fg(palette.muted);
    run.stages
        .iter()
        .zip(&run.workflow.stages)
        .filter_map(|(state, stage)| Some((state.fan_out.as_ref()?, stage)))
        .map(|(fan, stage)| {
            let full = candidate_line(fan, stage, true, muted, palette);
            if full.width() <= usize::from(width) {
                full
            } else {
                candidate_line(fan, stage, false, muted, palette)
            }
        })
        .collect()
}

/// One line per delegating stage with tasks: each task's target and status.
fn task_lines(run: &engine::Run, width: u16, palette: Palette) -> Vec<Line<'static>> {
    use crate::delegation::records::TaskStatus;
    let muted = Style::default().fg(palette.muted);
    run.stages
        .iter()
        .zip(&run.workflow.stages)
        .filter_map(|(state, stage)| Some((state.attempts.last()?, stage)))
        .filter(|(attempt, _)| !attempt.tasks.is_empty())
        .map(|(attempt, stage)| {
            let mut spans = vec![Span::styled(format!("  {} tasks ", stage.name), muted)];
            for task in &attempt.tasks {
                let status = match task.status {
                    TaskStatus::Queued | TaskStatus::Starting => engine::Status::Pending,
                    TaskStatus::Running => engine::Status::Running,
                    TaskStatus::Waiting => engine::Status::AwaitingConfirmation,
                    TaskStatus::Completed => engine::Status::Complete,
                    TaskStatus::Blocked => engine::Status::Blocked,
                    TaskStatus::Cancelled => engine::Status::Cancelled,
                    TaskStatus::Failed | TaskStatus::Interrupted => engine::Status::Failed,
                };
                let target = task
                    .target
                    .delegate
                    .clone()
                    .unwrap_or_else(|| task.target.harness.clone());
                spans.push(Span::styled(
                    format!("  ↳ {} · {target} ", task.id),
                    Style::default().fg(palette.fg),
                ));
                spans.push(Span::styled(
                    Palette::status_glyph(status),
                    Style::default().fg(palette.status_color(status)),
                ));
            }
            if !attempt.turns.is_empty() {
                spans.push(Span::styled(
                    format!("  · {} wake(s)", attempt.turns.len()),
                    muted,
                ));
            }
            let line = Line::from(spans);
            if line.width() > usize::from(width) {
                Line::from(format!(
                    "  {} tasks: {} ({} done)",
                    stage.name,
                    attempt.tasks.len(),
                    attempt
                        .tasks
                        .iter()
                        .filter(|t| t.status.is_terminal())
                        .count()
                ))
                .style(muted)
            } else {
                line
            }
        })
        .collect()
}

fn candidate_line(
    fan: &engine::FanOutState,
    stage: &crate::config::Stage,
    models: bool,
    muted: Style,
    palette: Palette,
) -> Line<'static> {
    let chosen = fan.selection.as_ref().map(|s| s.candidate.as_str());
    let mut spans = vec![Span::styled(format!("  {} fan-out ", stage.name), muted)];
    for candidate in &fan.candidates {
        let mark = if chosen == Some(candidate.id.as_str()) {
            "★"
        } else {
            "↳"
        };
        let harness = if candidate.model.is_empty() || !models {
            candidate.harness.clone()
        } else {
            format!("{} {}", candidate.harness, candidate.model)
        };
        spans.push(Span::styled(
            format!("  {mark} {} · {harness} ", candidate.id),
            Style::default().fg(palette.fg),
        ));
        spans.push(Span::styled(
            Palette::status_glyph(candidate.status),
            Style::default().fg(palette.status_color(candidate.status)),
        ));
    }
    Line::from(spans)
}

/// A rule under the strip, ending in the run's ID.
fn rule(id: &str, width: u16, palette: Palette) -> Line<'static> {
    let label = format!(" {id} ");
    let rest = usize::from(width).saturating_sub(label.width() + 1);
    Line::from(vec![
        Span::styled("─".repeat(rest), Style::default().fg(palette.line)),
        Span::styled(label, Style::default().fg(palette.muted)),
        Span::styled("─", Style::default().fg(palette.line)),
    ])
}

/// The selected run of this session: its title, stage strip, and then the
/// feed of its output (with a card when it waits on the person), or the
/// brief once it has finished.
pub(super) fn draw_run(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    theme: Theme,
    palette: Palette,
) {
    let Some(slot) = app.session.focused() else {
        empty_state(
            frame,
            palette,
            area,
            palette.panel(" Run "),
            "No run selected",
            "Start one from Workflow [r], or open a saved run from the sidebar.",
        );
        return;
    };
    let results = app.results_shown();
    // While it runs; the brief lists candidates once it has finished.
    let candidates = if results {
        vec![]
    } else {
        let mut lines = candidate_lines(&slot.run, area.width, palette);
        lines.extend(task_lines(&slot.run, area.width, palette));
        lines
    };
    let [head, strip_row, candidate_rows, rule_row, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(candidates.len().min(4) as u16),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(candidates), candidate_rows);
    frame.render_widget(Paragraph::new(header(slot, head.width, palette)), head);
    frame.render_widget(
        Paragraph::new(strip(slot, strip_row.width, palette)),
        strip_row,
    );
    frame.render_widget(
        Paragraph::new(rule(&slot.run.id, rule_row.width, palette)),
        rule_row,
    );
    if results {
        draw_completion(frame, slot, body, theme, palette);
    } else {
        draw_feed(frame, slot, body, theme, palette);
    }
}

/// The output of every stage in order, a paused or search line under it,
/// and the needs-you card at the bottom; a finished run's output keeps its
/// Next bar instead.
fn draw_feed(frame: &mut Frame<'_>, slot: &RunSlot, area: Rect, theme: Theme, palette: Palette) {
    let done = (slot.worker.is_none() && finished(&slot.run)).then(|| {
        key_chips(
            palette,
            &[("o", "brief"), ("d", "diff"), ("/", "search")],
            area.width.into(),
        )
    });
    let card = needs_card(slot, area.width.saturating_sub(4).into(), palette);
    let inner = area.width.saturating_sub(4);
    let rows = |lines: &[Line<'_>]| -> u16 {
        lines
            .iter()
            .map(|line| rows_for(line, inner, u16::MAX))
            .fold(0, u16::saturating_add)
    };
    let card_height = card.as_ref().map_or(0, |card| {
        // A blank row between the text and the keys; the output keeps at
        // least three rows.
        rows(&card.body)
            .saturating_add(1)
            .saturating_add(rows(&card.keys))
            .saturating_add(2)
            .min(area.height.saturating_sub(3))
    });
    let paused = !slot.log_view.follow;
    let search = slot.log_search.as_ref();
    let has_search_notice = search.is_some_and(|s| s.typing || !slot.logs.is_empty());
    let notice = paused || has_search_notice;
    let bar_height = done
        .as_ref()
        .map_or(0, |keys| next_height(keys, area.width));
    let [output, notice_row, card_row] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(u16::from(notice)),
        Constraint::Length(card_height.max(bar_height)),
    ])
    .areas(area);
    if let Some(keys) = done {
        draw_next(frame, keys, false, card_row, palette);
    }
    let visible = usize::from(output.height);
    slot.log_visible.set(visible);
    let search_status = if slot.logs.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Harness output appears here. Some harnesses emit their final response only when the stage finishes. Full logs are saved on disk.",
                Style::default().fg(palette.muted).add_modifier(Modifier::ITALIC),
            ))
            .wrap(Wrap { trim: false }),
            output,
        );
        search
            .filter(|s| s.typing)
            .map(|s| format_search_status(s, 0))
    } else {
        let width = usize::from(output.width);
        // Wrapped lines are kept until the log, width, or theme changes.
        let key = (width, slot.log_version, theme);
        let mut cache = slot.cache.borrow_mut();
        let lines = match &cache.output {
            Some((cached, lines)) if *cached == key => lines.clone(),
            _ => {
                let first = slot.log_version.saturating_sub(slot.logs.len() as u64);
                let lines = Rc::new(output_lines(&slot.logs, first, width, palette));
                cache.output = Some((key, lines.clone()));
                lines
            }
        };
        drop(cache);
        let matches = search
            .filter(|s| !s.query.is_empty())
            .map(|s| slot.cached_matches(&lines, width, &s.query));
        // Paused output keeps its top line as new output arrives.
        let top = lines.top(slot.log_view, visible);
        frame.render_widget(
            Paragraph::new(visible_lines(
                &lines,
                top,
                visible,
                search,
                matches.as_deref().map(Vec::as_slice),
                palette,
            )),
            output,
        );
        search.map(|s| {
            let count = matches.as_ref().map_or(0, |m| m.len());
            format_search_status(s, count)
        })
    };
    if notice {
        let mut spans = vec![];
        if paused {
            spans.push(Span::styled(
                "▲ paused — End to follow   ",
                Style::default().fg(palette.warn),
            ));
        }
        if let Some(search_status) = search_status {
            spans.push(Span::styled(
                search_status,
                Style::default().fg(palette.accent),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), notice_row);
    }
    if let Some(card) = card {
        let block = palette
            .panel(Span::styled(
                format!(" {} ", card.title),
                Style::default().fg(card.color).add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(card.color));
        let inside = block.inner(card_row);
        frame.render_widget(block, card_row);
        // The keys keep their rows; a long summary or error is cut instead.
        let key_rows = rows(&card.keys).min(inside.height);
        let [text, keys] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(key_rows)]).areas(inside);
        frame.render_widget(
            Paragraph::new(card.body).wrap(Wrap { trim: false }),
            Rect {
                height: text.height.saturating_sub(1),
                ..text
            },
        );
        frame.render_widget(Paragraph::new(card.keys).wrap(Wrap { trim: false }), keys);
    }
}

/// What the run waits on: a title, its colour, what to know, and the keys
/// that answer it, which stay in view when the text does not fit.
struct Card {
    title: String,
    color: Color,
    body: Vec<Line<'static>>,
    keys: Vec<Line<'static>>,
}

/// The card under the feed while the run waits on the person: its title,
/// colour, and lines, ending in the keys that answer it. None while the run
/// drives itself or once it has finished. Keys past `width` are left to the
/// Space menu.
fn needs_card(slot: &RunSlot, width: usize, palette: Palette) -> Option<Card> {
    let run = &slot.run;
    if slot.worker.is_some() || finished(run) {
        return None;
    }
    let stage = run.current_name().to_owned();
    let fg = Style::default().fg(palette.fg);
    let muted = Style::default().fg(palette.muted);
    let more = Line::from(vec![
        Span::styled("More: ", muted),
        Span::styled(
            "[Space]",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let mut lines = vec![];
    let mut keys = vec![];
    // Keys the engine refuses at this stage: fan-out stages have no session
    // to reopen and run unattended; a review's verdict cannot be rewritten;
    // a ship plan has no session and its preview is not what ships.
    let current = run.workflow.stages.get(run.cursor);
    let role = current.map(|s| s.role);
    let fan_out = current.is_some_and(|s| s.fan_out.is_some());
    let ship = role == Some(crate::config::Role::Ship);
    let review = role == Some(crate::config::Role::Review);
    // Harnesses without resume support have no session to reopen.
    let resumable = current.is_some_and(|s| crate::adapters::resumable(&s.harness, &run.adapters));
    let offered = |pairs: &[(&'static str, &'static str)]| -> Vec<(&'static str, &'static str)> {
        pairs
            .iter()
            .copied()
            .filter(|(key, _)| !((fan_out || ship) && matches!(*key, "t" | "i")))
            .filter(|(key, _)| !((review || ship) && *key == "e"))
            .filter(|(key, _)| resumable || *key != "t")
            .collect()
    };
    if let Some(error) = &run.error {
        lines.push(Line::styled(
            storage::clean_terminal(error),
            Style::default().fg(palette.red),
        ));
        lines.push(Line::raw(""));
    }
    if let Some(reason) = run.pending_waiver() {
        lines.push(Line::styled(
            "The test stage reported that no check applies:",
            fg,
        ));
        lines.push(Line::styled(
            storage::clean_terminal(reason),
            fg.add_modifier(Modifier::BOLD),
        ));
        keys.push(key_chips(
            palette,
            &[
                ("a", "accept without checks"),
                ("y", "retry"),
                ("h", "handoff"),
            ],
            width,
        ));
        return Some(Card {
            title: format!("◆ {stage}: verification not applicable"),
            color: palette.warn,
            body: lines,
            keys,
        });
    }
    let title = match run.status {
        Status::AwaitingConfirmation => {
            let next = run
                .workflow
                .stages
                .iter()
                .zip(&run.stages)
                .skip(run.cursor + 1)
                .find(|(stage, state)| {
                    stage.enabled && !matches!(state.status, Status::Skipped | Status::Complete)
                })
                .map(|(stage, _)| stage.name.clone());
            lines.push(Line::styled(
                match next {
                    // A graph's stages start as their inputs finish, so the
                    // next one is not simply the one after.
                    _ if run.graph() => {
                        "The stage handed off. Accept it to continue the run.".into()
                    }
                    Some(next) => format!("The stage handed off. Accept it to continue to {next}."),
                    None => "The stage handed off. Accept it to finish the run.".into(),
                },
                fg,
            ));
            let handoff = run.stages.get(run.cursor).and_then(|s| s.handoff.as_ref());
            if let Some(handoff) = handoff {
                lines.extend(handoff_lines(handoff, palette));
            }
            keys.push(key_chips(
                palette,
                &offered(&[
                    ("a", "accept"),
                    ("t", "reopen session"),
                    ("y", "retry"),
                    ("e", "edit summary"),
                    ("h", "handoff"),
                ]),
                width,
            ));
            keys.push(more);
            format!("◆ {stage} needs you")
        }
        Status::Interactive => {
            let harness = run
                .workflow
                .stages
                .get(run.cursor)
                .map_or("its harness", |s| s.harness.as_str());
            lines.push(Line::styled(
                format!(
                    "This stage runs in {harness}'s own terminal. You drive it, then exit to hand off."
                ),
                fg,
            ));
            keys.push(key_chips(
                palette,
                &[("i", "open the terminal"), ("h", "handoff")],
                width,
            ));
            format!("◆ {stage} is ready for you")
        }
        // Left running by an earlier execution: continuing would fail it, so
        // retrying leads.
        Status::Running => {
            lines.push(Line::styled(
                "An earlier execution of this stage was interrupted. Retry it to pick up here.",
                fg,
            ));
            keys.push(key_chips(
                palette,
                &offered(&[("y", "retry"), ("i", "interactive"), ("m", "model")]),
                width,
            ));
            keys.push(more);
            format!("◆ {stage} was interrupted")
        }
        // Stopped between stages before they ran.
        _ => {
            lines.push(Line::styled(
                "The run stopped here; continuing picks up at this stage.",
                fg,
            ));
            keys.push(key_chips(
                palette,
                &offered(&[
                    ("c", "continue"),
                    ("y", "retry"),
                    ("i", "interactive"),
                    ("m", "model"),
                ]),
                width,
            ));
            keys.push(more);
            if run.cursor < run.workflow.stages.len() {
                format!("◆ {stage} is paused")
            } else {
                "◆ Run is paused".into()
            }
        }
    };
    Some(Card {
        title,
        color: palette.warn,
        body: lines,
        keys,
    })
}

/// What a handoff claims, in a line, then the start of its summary.
fn handoff_lines(handoff: &crate::handoff::Handoff, palette: Palette) -> Vec<Line<'static>> {
    use crate::handoff::TestStatus;
    let mut lines = vec![];
    if !handoff.structured {
        lines.push(Line::styled(
            "Text handoff, not structured: inspect it before accepting.",
            Style::default().fg(palette.warn),
        ));
    }
    let mut facts: Vec<Span<'static>> = vec![];
    let mut fact = |text: String, color| {
        if !facts.is_empty() {
            facts.push(Span::raw("   "));
        }
        facts.push(Span::styled(text, Style::default().fg(color)));
    };
    match handoff.files_changed.len() {
        0 => {}
        1 => fact("1 file changed".into(), palette.fg),
        n => fact(format!("{n} files changed"), palette.fg),
    }
    let count = |status| handoff.tests.iter().filter(|t| t.status == status).count();
    let (passed, failed) = (count(TestStatus::Passed), count(TestStatus::Failed));
    if passed > 0 {
        fact(format!("✓ {passed} passed"), palette.success);
    }
    if failed > 0 {
        fact(format!("✗ {failed} failed"), palette.red);
    }
    if !handoff.unresolved.is_empty() {
        fact(
            format!("{} unresolved", handoff.unresolved.len()),
            palette.warn,
        );
    }
    if !facts.is_empty() {
        lines.push(Line::from(facts));
    }
    let summary = storage::clean_terminal(&handoff.summary);
    let mut text = summary.lines().filter(|line| !line.trim().is_empty());
    let shown: Vec<_> = text.by_ref().take(3).collect();
    if !shown.is_empty() {
        lines.push(Line::raw(""));
    }
    for line in shown {
        lines.push(Line::styled(
            line.to_owned(),
            Style::default().fg(palette.fg),
        ));
    }
    if text.next().is_some() {
        lines.push(Line::styled(
            "… [h] shows the rest",
            Style::default().fg(palette.muted),
        ));
    }
    lines
}
