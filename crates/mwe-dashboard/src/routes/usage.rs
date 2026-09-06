// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Usage & spend** — the page for what this deployment consumes, what
//! it costs against the rates the operator declared, and the daily budget
//! that stops it.
//!
//! # Tokens are the measurement; money is an opinion
//!
//! The page answers four questions, in the order they matter:
//!
//! 1. **Where today stands against the budget** — the only figure on the
//!    page that can change what happens in the next minute.
//! 2. **What is being consumed, and by what** — tokens per slot and per
//!    model, because "which part of the machine spends" is the question
//!    whose answer changes a decision.
//! 3. **How it moved over time** — per day and per month, so a clean
//!    month can be read at a glance.
//! 4. **How much of it the cache absorbed** — reads are the discount,
//!    writes are the deposit paid for it, and both are subsets of the
//!    prompt total, so a page that showed only "prompt tokens" would
//!    make a well-cached workload look several times more expensive
//!    than it is.
//!
//! Money is layered **on top** and only where a rate exists. With no
//! `llm_pricing:` rates the money columns are not rendered at all — not
//! as zeros, not as dashes. That is not a demo mode, it is the default:
//! published rates move, contracts differ, and the currency is not ours
//! to assume, so a deployment that invented a price would be confidently
//! wrong about somebody else's money. A freshly installed server and the
//! public demo behave identically for exactly this reason, with no
//! branch anywhere that names either of them.
//!
//! # Three editors, because the numbers are useless read-only
//!
//! The price list is what turns tokens into money and the budget is what
//! acts on the result, so both are set here, on the page that shows what
//! they do, saved into `mwe-mcp.config.yaml` the same way every other
//! operator setting is (backup `.bak`, serialise, atomic write) and
//! hot-reloaded into the running guard. The third editor is one button:
//! unlocking today, which is how an operator who meant to spend more
//! keeps working without editing anything.
//!
//! # Admin-only
//!
//! The ledger is deployment-wide: it is the sum over every user's
//! turns. It carries no content and no sender, so it is not a
//! confidentiality problem in the way the facts table is — but it is
//! operator telemetry about the machine, which is the same class as
//! Health, and it sits behind the same gate.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use maud::{Markup, html};
use mwe_core::budget::{BudgetGuard, BudgetState};
use mwe_core::config::{BudgetConfig, CONFIG_FILENAME, Config, LlmPricingConfig, ModelPrice};
use mwe_core::usage::{self, EMBEDDING_SLOT, UsageBucket};
use mwe_core::wiki::atomic_write;
use serde::Deserialize;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::layout;

/// Sub-router for `/admin/usage`.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/admin/usage", get(page))
        .route("/admin/usage/budget", post(save_budget))
        .route("/admin/usage/pricing", post(save_pricing))
        .route("/admin/usage/unlock", post(set_unlocked))
}

/// Selectable trailing windows, in days. `0` means the whole ledger.
const WINDOWS: &[(i64, &str)] = &[
    (1, "Today"),
    (7, "7 days"),
    (30, "30 days"),
    (90, "90 days"),
    (0, "All"),
];

/// Default window. A month is the unit a provider bills in, and the
/// unit the question "was this month clean" is asked in.
const DEFAULT_DAYS: i64 = 30;

/// Blank price rows offered below the ones already configured, so adding
/// a model does not need a second round-trip.
const SPARE_PRICE_ROWS: usize = 3;

#[derive(Debug, Deserialize)]
struct Params {
    /// Trailing window in days; `0` = the whole ledger.
    days: Option<i64>,
    /// `1` ⇒ count only the running server's untagged traffic, i.e.
    /// drop hand-run cycles and anything a `MWE_USAGE_TAG` process
    /// produced.
    clean: Option<u8>,
    /// One-shot banner after a save, carried in the redirect so a
    /// refresh does not re-post the form.
    saved: Option<String>,
}

/// Everything one render of the page needs, gathered once.
struct View<'a> {
    buckets: &'a [UsageBucket],
    pricing: &'a LlmPricingConfig,
    budget_config: &'a BudgetConfig,
    budget: &'a BudgetState,
    days: i64,
    clean: bool,
    dropped_rows: usize,
    first_day: Option<&'a str>,
    read_only: bool,
    saved: Option<&'a str>,
}

async fn page(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Query(params): Query<Params>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let days = params
        .days
        .filter(|d| WINDOWS.iter().any(|(w, _)| w == d))
        .unwrap_or(DEFAULT_DAYS);
    let clean = params.clean == Some(1);

    // The price list and the budget are read from disk on every load rather
    // than cached in `DashboardState`: they are edited here and in the
    // YAML both, and an operator who corrects a rate should see the
    // totals move on the next refresh instead of after a restart.
    //
    // No workdir (identity-only / test builds) is not an error here — it
    // only means no price list and no budget, and the tokens are the point.
    let config = load_config(&state)?;
    let pricing = config.llm_pricing.clone();
    let budget_config = config.budget.clone();
    let budget = guard(&state, &budget_config, &pricing)
        .state()
        .await
        .map_err(|e| DashboardError::Internal(format!("budget state: {e}")))?;

    let since = since_day(days);
    let mut buckets = usage::buckets(&state.pool, since.as_deref())
        .await
        .map_err(|e| DashboardError::Internal(format!("usage::buckets: {e}")))?;
    // Both filters are about *whose* traffic it is, so they belong
    // together and in Rust: the counts of what was dropped are printed
    // beside the switch, and a SQL `WHERE` could not report them
    // without a second query.
    let total_rows = buckets.len();
    if clean {
        buckets.retain(|b| b.source == "serve" && b.tag.is_none());
    }
    let dropped_rows = total_rows - buckets.len();

    let first_day = usage::first_day(&state.pool)
        .await
        .map_err(|e| DashboardError::Internal(format!("usage::first_day: {e}")))?;

    let body = render(&View {
        buckets: &buckets,
        pricing: &pricing,
        budget_config: &budget_config,
        budget: &budget,
        days,
        clean,
        dropped_rows,
        first_day: first_day.as_deref(),
        read_only: chrome.read_only,
        saved: params.saved.as_deref(),
    });
    Ok(Html(layout::authenticated_page(
        chrome,
        "Usage & spend",
        admin.session(),
        &body,
    )))
}

/// First day of the trailing window (`YYYY-MM-DD`, UTC), or `None` for
/// the whole ledger.
///
/// `days = 1` means "today", so the window starts today rather than
/// yesterday: subtracting `days` would make every label off by one.
fn since_day(days: i64) -> Option<String> {
    (days > 0).then(|| {
        (chrono::Utc::now() - chrono::Duration::days(days - 1))
            .format("%Y-%m-%d")
            .to_string()
    })
}

// ---------- config on disk ----------

fn workdir_of(state: &DashboardState) -> Option<PathBuf> {
    state.memory.as_ref().map(|m| m.workdir.clone())
}

fn load_config(state: &DashboardState) -> Result<Config> {
    workdir_of(state).map_or_else(
        || Ok(Config::default()),
        |workdir| {
            Config::load_raw(&workdir)
                .map_err(|e| DashboardError::Internal(format!("config load: {e}")))
        },
    )
}

fn backup_path_for(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    target.with_file_name(name)
}

/// The guard whose numbers this page shows and whose budget its buttons
/// move.
///
/// The process-wide one when the server installed it — which is every
/// running deployment, and the only guard whose in-memory budget the save
/// can hot-reload. A dashboard built without one (test harness) gets a
/// guard over the same engine DB, so the figures and the unlock still
/// read and write the rows they should.
fn guard(
    state: &DashboardState,
    budget_config: &BudgetConfig,
    pricing: &LlmPricingConfig,
) -> Arc<BudgetGuard> {
    mwe_core::budget::global().unwrap_or_else(|| {
        Arc::new(BudgetGuard::new(
            state.pool.clone(),
            budget_config.clone(),
            pricing.clone(),
        ))
    })
}

/// Write `cfg` back over `mwe-mcp.config.yaml`, keeping a `.bak` of what
/// was there.
///
/// The same shape as every other YAML editor on the dashboard: re-load
/// from disk first (raw — env overrides are runtime-only and must never
/// be baked into the file by a save), replace one section, back up,
/// atomic-write. Operator comments in the YAML are flattened;
/// `serde_yaml` keeps none.
fn persist(state: &DashboardState, cfg: &Config) -> Result<()> {
    let Some(workdir) = workdir_of(state) else {
        return Err(DashboardError::Internal(
            "this dashboard has no workdir, so there is no config file to save into".to_owned(),
        ));
    };
    let path = workdir.join(CONFIG_FILENAME);
    let backup = backup_path_for(&path);
    match fs::read(&path) {
        Ok(bytes) => atomic_write(&backup, &bytes)
            .map_err(|e| DashboardError::Internal(format!("backup: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(DashboardError::Internal(format!("read for backup: {e}"))),
    }
    let yaml = serde_yaml::to_string(cfg)
        .map_err(|e| DashboardError::Internal(format!("serialize config: {e}")))?;
    atomic_write(&path, yaml.as_bytes())
        .map_err(|e| DashboardError::Internal(format!("write config: {e}")))?;
    Ok(())
}

/// Push the saved budget and price list into the running guard, so the next
/// model call is judged against what the operator just typed rather than
/// against what the process booted with.
async fn hot_reload(budget_config: BudgetConfig, pricing: LlmPricingConfig) {
    if let Some(guard) = mwe_core::budget::global() {
        guard.replace_config(budget_config, pricing).await;
    }
}

/// Back to the page with a one-shot banner, so a refresh does not
/// re-post the form.
fn back_to_page(saved: &str) -> Response {
    Redirect::to(&format!("/dashboard/admin/usage?saved={saved}")).into_response()
}

// ---------- POST ----------

async fn save_budget(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let mut cfg = load_config(&state)?;
    cfg.budget = parse_budget_form(&form, &cfg.budget)?;
    persist(&state, &cfg)?;
    hot_reload(cfg.budget.clone(), cfg.llm_pricing.clone()).await;
    tracing::info!(
        admin = %admin.session().sender_id,
        daily_limit = ?cfg.budget.daily_limit,
        warn_at_percent = cfg.budget.warn_at_percent,
        "usage: daily budget saved from the dashboard (hot-reloaded)"
    );
    Ok(back_to_page("budget"))
}

async fn save_pricing(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let mut cfg = load_config(&state)?;
    cfg.llm_pricing = parse_pricing_form(&form, &cfg.llm_pricing)?;
    persist(&state, &cfg)?;
    hot_reload(cfg.budget.clone(), cfg.llm_pricing.clone()).await;
    tracing::info!(
        admin = %admin.session().sender_id,
        models = cfg.llm_pricing.models.len(),
        "usage: price list saved from the dashboard (hot-reloaded)"
    );
    Ok(back_to_page("pricing"))
}

async fn set_unlocked(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let cfg = load_config(&state)?;
    let guard = guard(&state, &cfg.budget, &cfg.llm_pricing);
    let unlock = form.get("unlock").map(String::as_str) == Some("1");
    if unlock {
        guard.unlock_today().await
    } else {
        guard.relock_today().await
    }
    .map_err(|e| DashboardError::Internal(format!("budget unlock: {e}")))?;
    tracing::info!(
        admin = %admin.session().sender_id,
        unlock,
        "usage: the daily budget stop was changed from the dashboard"
    );
    Ok(back_to_page(if unlock { "unlocked" } else { "relocked" }))
}

/// Decode the budget form. An empty limit means **no budget**, which is how an
/// operator turns the feature off from the page they turned it on from.
fn parse_budget_form(form: &HashMap<String, String>, prior: &BudgetConfig) -> Result<BudgetConfig> {
    let raw_limit = form
        .get("daily_limit")
        .map(|v| v.trim().to_owned())
        .unwrap_or_default();
    let daily_limit = if raw_limit.is_empty() {
        None
    } else {
        let parsed = raw_limit.replace(',', ".").parse::<f64>().map_err(|_| {
            DashboardError::Validation(format!(
                "the daily budget must be a number (got `{raw_limit}`)"
            ))
        })?;
        if parsed < 0.0 {
            return Err(DashboardError::Validation(
                "the daily budget cannot be negative".to_owned(),
            ));
        }
        Some(parsed)
    };

    let raw_warn = form
        .get("warn_at_percent")
        .map(|v| v.trim().to_owned())
        .unwrap_or_default();
    let warn_at_percent = if raw_warn.is_empty() {
        mwe_core::budget::DEFAULT_WARN_AT_PERCENT
    } else {
        let parsed = raw_warn.parse::<u8>().map_err(|_| {
            DashboardError::Validation(format!(
                "the warning threshold must be a whole percentage (got `{raw_warn}`)"
            ))
        })?;
        if !(1..=100).contains(&parsed) {
            return Err(DashboardError::Validation(
                "the warning threshold must be between 1 and 100 percent".to_owned(),
            ));
        }
        parsed
    };

    Ok(BudgetConfig {
        daily_limit,
        warn_at_percent,
        extra: prior.extra.clone(),
    })
}

/// Decode the price-list form.
///
/// A row whose model id is blank is dropped, which is how a rate is
/// removed — the same rule the slot editor uses for a slot with no
/// provider. `input` and `output` are required on a row that has a
/// model; the two cache rates are optional and fall back to the input
/// rate, so a half-filled row is an upper bound rather than a discount
/// nobody promised.
fn parse_pricing_form(
    form: &HashMap<String, String>,
    prior: &LlmPricingConfig,
) -> Result<LlmPricingConfig> {
    let field = |i: usize, name: &str| {
        form.get(&format!("price__{i}__{name}"))
            .map(|v| v.trim().to_owned())
            .unwrap_or_default()
    };
    let rate = |i: usize, name: &str, model: &str| -> Result<Option<f64>> {
        let raw = field(i, name);
        if raw.is_empty() {
            return Ok(None);
        }
        raw.replace(',', ".")
            .parse::<f64>()
            .map(Some)
            .map_err(|_| {
                DashboardError::Validation(format!(
                    "`{model}`: {name} must be a number (got `{raw}`)"
                ))
            })
            .and_then(|v| match v {
                Some(v) if v < 0.0 => Err(DashboardError::Validation(format!(
                    "`{model}`: {name} cannot be negative"
                ))),
                other => Ok(other),
            })
    };

    let mut models = Vec::new();
    for i in 0..prior.models.len() + SPARE_PRICE_ROWS {
        let model = field(i, "model");
        if model.is_empty() {
            continue;
        }
        let input = rate(i, "input", &model)?.ok_or_else(|| {
            DashboardError::Validation(format!("`{model}`: an input rate is required"))
        })?;
        let output = rate(i, "output", &model)?.ok_or_else(|| {
            DashboardError::Validation(format!("`{model}`: an output rate is required"))
        })?;
        let cached_input = rate(i, "cached_input", &model)?;
        let cache_write = rate(i, "cache_write", &model)?;
        models.push(ModelPrice {
            model,
            input,
            cached_input,
            cache_write,
            output,
        });
    }

    let currency = form
        .get("currency")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());

    Ok(LlmPricingConfig {
        currency,
        models,
        extra: prior.extra.clone(),
    })
}

// ---------- rendering ----------

/// Group buckets by a key extracted from each, preserving a stable
/// order for the rendered table.
fn group_by<K: Ord, F: Fn(&UsageBucket) -> K>(
    buckets: &[UsageBucket],
    key: F,
) -> BTreeMap<K, Vec<UsageBucket>> {
    let mut out: BTreeMap<K, Vec<UsageBucket>> = BTreeMap::new();
    for b in buckets {
        out.entry(key(b)).or_default().push(b.clone());
    }
    out
}

/// `1234567` → `1 234 567`. A thin space, because a token count is
/// read, not parsed, and seven unbroken digits are read wrongly.
fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// Money, with enough decimals to be non-zero.
///
/// A per-day cost of `0.004` printed as `0.00` reads as "free", which
/// is the one thing this page must never say by accident.
fn money(v: f64) -> String {
    if v != 0.0 && v.abs() < 0.01 {
        format!("{v:.4}")
    } else {
        format!("{v:.2}")
    }
}

/// Percentage of `part` in `whole`, blank when there is no whole.
fn share(part: i64, whole: i64) -> String {
    if whole <= 0 {
        return "—".to_owned();
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "display rounding of counts far below 2^53"
    )]
    let pct = (part as f64) * 100.0 / (whole as f64);
    format!("{pct:.0}%")
}

/// A proportion bar, drawn with design tokens inline so no new Tailwind
/// utility has to be compiled for this page.
///
/// `accent` is the fill colour token, so the budget bar can go from the
/// ordinary accent to a warning to a stop without a second component;
/// `width` lets the budget's bar fill its tile while the per-day bars
/// stay a narrow column in a table.
fn bar_with(part: f64, whole: f64, accent: &str, width: &str) -> Markup {
    let pct = if whole > 0.0 {
        (part * 100.0 / whole).clamp(0.0, 100.0)
    } else {
        0.0
    };
    html! {
        span style=(format!("display:inline-block;width:{width};height:.5rem;background:var(--bg-3);border-radius:9999px;overflow:hidden;vertical-align:middle")) {
            span style=(format!("display:block;height:100%;width:{pct:.1}%;background:{accent}")) {}
        }
    }
}

/// The same bar over two counts.
fn bar(part: i64, whole: i64) -> Markup {
    #[allow(
        clippy::cast_precision_loss,
        reason = "display rounding of counts far below 2^53"
    )]
    bar_with(part as f64, whole as f64, "var(--p)", "6rem")
}

/// The words this page uses for a slot.
///
/// The six model slots are named exactly as the LLM config editor names
/// them, so a reader who sets a model on one page recognises it on the
/// other. The embedder is the one other spender the ledger records, and
/// it is not a model slot.
fn slot_label(slot: &str) -> &str {
    if slot == EMBEDDING_SLOT {
        return "Embedder (search vectors)";
    }
    super::llm_config::slot_title(slot).unwrap_or(slot)
}

/// The words this page uses for how a call is paid for.
fn billing_label(billing: &str) -> &'static str {
    match billing {
        "subscription" => "flat subscription",
        "local" => "on this machine",
        "api" => "metered",
        _ => "mixed",
    }
}

/// The words this page uses for which process made a call.
fn source_label(source: &str) -> &'static str {
    match source {
        "rem-cli" => "hand-run cycle",
        "eval-cli" => "recall evaluation",
        "serve" => "live server",
        _ => "other",
    }
}

/// The token columns shared by every table on the page.
fn token_headers(with_money: bool) -> Markup {
    html! {
        th { "Calls" }
        th { "Tokens in" }
        th { "of which cached" }
        th { "written to cache" }
        th { "Tokens out" }
        th { "Total tokens" }
        @if with_money { th { "Est. cost" } }
    }
}

/// The token cells for one group of buckets.
///
/// Takes the **group**, not just its fold, because the tokens and the
/// money are answered at different grains. Tokens add up at any grain;
/// a price belongs to a model, so the money for a row that spans two
/// models is the sum of two priced buckets and can never be recovered
/// from their sum. Handing this function only the fold is how the total
/// row came to disagree with the headline by a factor of three.
fn token_cells(rows: &[UsageBucket], pricing: &LlmPricingConfig, with_money: bool) -> Markup {
    let f = usage::fold(rows);
    let embedding_only = rows.iter().all(|r| r.slot == EMBEDDING_SLOT);
    html! {
        td { (thousands(f.calls)) @if f.failed > 0 { " " span.muted { "(" (f.failed) " failed)" } } }
        @if embedding_only {
            // Every token column of an embedding row is NULL: the
            // endpoint reports no counts. Printing the summed zeros
            // would read as "this cost nothing", which is a different
            // claim from "nobody counted".
            td.muted colspan="5" title="An embedding endpoint reports no token counts" {
                "not reported"
            }
        } @else {
            td { (thousands(f.prompt_tokens)) }
            td.muted { (thousands(f.cached_prompt_tokens)) " " span.muted { "(" (share(f.cached_prompt_tokens, f.prompt_tokens)) ")" } }
            td.muted { (thousands(f.cache_write_tokens)) }
            td { (thousands(f.completion_tokens)) }
            td { strong { (thousands(f.total_tokens())) } }
        }
        @if with_money { (cost_cell(rows, pricing)) }
    }
}

/// The money cell for one group: each bucket priced at its own model's
/// rate, then added.
///
/// Three outcomes, kept distinct because collapsing them is how a page
/// lies. Nothing priced ⇒ say so rather than print `0.00`. Everything
/// priced ⇒ the figure. Partly priced ⇒ the figure **plus** what it
/// leaves out, because a total that quietly omits calls is worse than
/// no total.
fn cost_cell(rows: &[UsageBucket], pricing: &LlmPricingConfig) -> Markup {
    let (cost, unpriced_calls) = usage::total_cost(rows, pricing);
    let priced_calls: i64 = rows.iter().map(|r| r.calls).sum::<i64>() - unpriced_calls;
    html! {
        td {
            @if priced_calls == 0 {
                span.muted title="No rate configured for these models" { "not priced" }
            } @else {
                (money(cost))
                @if unpriced_calls > 0 {
                    " " span.muted title="Calls on models with no configured rate are not in this figure" {
                        "+" (thousands(unpriced_calls)) " unpriced"
                    }
                }
            }
        }
    }
}

/// The banner for a save that just happened.
fn saved_banner(saved: &str) -> Markup {
    let msg = match saved {
        "budget" => "Daily budget saved — the next model call is judged against it.",
        "pricing" => "Price list saved — the figures below use it from now on.",
        "unlocked" => {
            "Unlocked for today. Paid model calls run again until midnight UTC, and the budget \
             applies again tomorrow."
        },
        "relocked" => "The budget is back in force for today.",
        _ => return html! {},
    };
    html! { p.flash.flash-success { (msg) } }
}

/// Today against the budget: the block the page opens with, because it is
/// the only figure here that changes what happens next.
fn budget_block(v: &View<'_>) -> Markup {
    let currency = v.budget.currency.as_deref().unwrap_or("");
    let stopped = v.budget.stopped();
    let crossed_warn = matches!(
        v.budget.crossed(),
        Some(mwe_core::budget::Threshold::Warn | mwe_core::budget::Threshold::Stop)
    );
    // The palette's own accents, not hex literals: this dashboard is a
    // dark phosphor theme, and a red borrowed from somewhere else does
    // not read as "stop" on it, it reads as a bug.
    let accent = if stopped {
        "var(--rose)"
    } else if crossed_warn {
        "var(--amber)"
    } else {
        "var(--p)"
    };
    html! {
        h3 { "Today" }
        @match v.budget.limit {
            None => {
                p.muted {
                    strong { "No budget set." }
                    " Today's metered spend is " strong { (money(v.budget.spent)) }
                    @if !currency.is_empty() { " " (currency) }
                    " and nothing stops it. Set a daily budget below and this deployment "
                    "warns you on the way there, then stops paid model calls when it "
                    "arrives — user turns keep answering, degraded, and say so."
                }
            },
            Some(limit) => {
                div.kpi-grid {
                    div.kpi {
                        strong { (money(v.budget.spent)) " " (currency) }
                        span { "spent today of " (money(limit)) " " (currency) }
                    }
                    div.kpi {
                        strong { (v.budget.percent_used().unwrap_or(0)) "%" }
                        span { "of the daily budget" }
                    }
                    div.kpi {
                        strong { (bar_with(v.budget.spent, limit, accent, "100%")) }
                        span {
                            @if stopped { "stopped" }
                            @else if v.budget.unlocked { "unlocked for today" }
                            @else if crossed_warn { "past the warning line" }
                            @else { "spending" }
                        }
                    }
                }
                @if stopped {
                    p.flash.flash-error {
                        strong { "Paid model calls are stopped." }
                        " " (v.budget.stop_message())
                        " User turns still answer: they degrade and say nothing was saved. "
                        "The nightly cycle skips its round and says why. Raise the budget "
                        "below, or unlock today."
                    }
                } @else if v.budget.unlocked {
                    p.flash.flash-info {
                        "You unlocked today, so spending continues past the budget. The budget "
                        "applies again at 00:00 UTC."
                    }
                } @else if crossed_warn {
                    p.flash.flash-info {
                        "Past " (v.budget.percent_used().unwrap_or(0)) "% of the daily budget. "
                        "Paid model calls stop at " (money(limit)) " " (currency)
                        " — a notice went out on the reverse channel too."
                    }
                }
                @if v.budget.unpriced_calls > 0 {
                    p.muted {
                        "The budget is read against the price list, and "
                        strong { (thousands(v.budget.unpriced_calls)) }
                        " of today's calls are on models with no rate — they spend nothing as "
                        "far as this budget is concerned. Price them below to make it bind."
                    }
                }
            },
        }
        @if !v.read_only {
            (budget_form(v))
        }
    }
}

/// The budget editor and the unlock button.
fn budget_form(v: &View<'_>) -> Markup {
    let currency = v.budget.currency.as_deref().unwrap_or("");
    html! {
        @if v.budget.limit.is_some() && (v.budget.stopped() || v.budget.unlocked) {
            form method="post" action="/dashboard/admin/usage/unlock" {
                @if v.budget.unlocked {
                    input type="hidden" name="unlock" value="0";
                    button type="submit" { "Re-apply the budget for today" }
                } @else {
                    input type="hidden" name="unlock" value="1";
                    button type="submit" { "Unlock for today" }
                }
            }
        }
        form method="post" action="/dashboard/admin/usage/budget" {
            div.field-grid {
                p {
                    label for="daily_limit" {
                        "Daily budget"
                        @if !currency.is_empty() { " (" (currency) ")" }
                    }
                    input #daily_limit type="text" inputmode="decimal" name="daily_limit"
                          value=(v.budget_config.daily_limit.map(|l| format!("{l}")).unwrap_or_default())
                          placeholder="e.g. 5.00";
                }
                p {
                    label for="warn_at_percent" { "Warn at (% of the budget)" }
                    input #warn_at_percent type="text" inputmode="numeric"
                          name="warn_at_percent"
                          value=(v.budget_config.warn_at_percent.to_string());
                }
                p.field-wide { button type="submit" { "Save budget" } }
            }
            p.muted {
                "Leave the budget empty to remove it. The figure is in the currency of your "
                "price list, per UTC day, and it covers metered calls only — a model on a "
                "flat subscription or running on this machine never moves it."
            }
        }
    }
}

/// The price list, editable in place.
fn pricing_block(v: &View<'_>) -> Markup {
    let currency = v.pricing.currency.as_deref().unwrap_or("");
    let rows = v.pricing.models.len() + SPARE_PRICE_ROWS;
    html! {
        h3 { "Price list" }
        p.muted {
            "Rates per 1M tokens, in whatever currency you are billed in. Nothing is "
            "assumed on your behalf: published rates change and your contract may not be "
            "the published one, so money is shown only for the models you price here. "
            "A model id may be a " code { "prefix*" } " wildcard, and the longest match "
            "wins whatever order the rows are in. An omitted cache rate falls back to the "
            "input rate, which gives you an upper bound rather than a discount nobody "
            "promised."
        }
        @if v.read_only {
            @if v.pricing.is_empty() {
                p.muted { "None configured." }
            } @else {
                div.table-wrap {
                    table.config-table {
                        thead { tr {
                            th { "Model" } th { "Input" } th { "Cache read" }
                            th { "Cache write" } th { "Output" }
                        } }
                        tbody {
                            @for m in &v.pricing.models {
                                tr {
                                    td { code { (m.model) } }
                                    td { (money(m.input)) }
                                    td { (money(m.cached_input_rate())) }
                                    td { (money(m.cache_write_rate())) }
                                    td { (money(m.output)) }
                                }
                            }
                        }
                    }
                }
            }
        } @else {
            form method="post" action="/dashboard/admin/usage/pricing" {
                p {
                    label {
                        "Currency "
                        input type="text" name="currency" value=(currency)
                              placeholder="EUR" size="6";
                    }
                    " " span.muted { "printed beside every figure; no rate is ever converted" }
                }
                div.table-wrap {
                    table.config-table {
                        thead { tr {
                            th { "Model" } th { "Input" } th { "Cache read" }
                            th { "Cache write" } th { "Output" }
                        } }
                        tbody {
                            @for i in 0..rows {
                                @let m = v.pricing.models.get(i);
                                tr {
                                    td {
                                        input type="text" name=(format!("price__{i}__model"))
                                              value=(m.map(|m| m.model.clone()).unwrap_or_default())
                                              placeholder="gemini-3-flash-*" size="24";
                                    }
                                    td {
                                        input type="text" inputmode="decimal"
                                              name=(format!("price__{i}__input"))
                                              value=(m.map(|m| m.input.to_string()).unwrap_or_default())
                                              size="8";
                                    }
                                    td {
                                        input type="text" inputmode="decimal"
                                              name=(format!("price__{i}__cached_input"))
                                              value=(m.and_then(|m| m.cached_input).map(|v| v.to_string()).unwrap_or_default())
                                              placeholder="= input" size="8";
                                    }
                                    td {
                                        input type="text" inputmode="decimal"
                                              name=(format!("price__{i}__cache_write"))
                                              value=(m.and_then(|m| m.cache_write).map(|v| v.to_string()).unwrap_or_default())
                                              placeholder="= input" size="8";
                                    }
                                    td {
                                        input type="text" inputmode="decimal"
                                              name=(format!("price__{i}__output"))
                                              value=(m.map(|m| m.output.to_string()).unwrap_or_default())
                                              size="8";
                                    }
                                }
                            }
                        }
                    }
                }
                p {
                    button type="submit" { "Save price list" }
                    " " span.muted { "clear a model id to drop its rate" }
                }
            }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one page, one flat template — splitting the sections into helpers \
              hides the reading order that IS the design"
)]
fn render(v: &View<'_>) -> Markup {
    let with_money = !v.pricing.is_empty();
    let currency = v.pricing.currency.as_deref().unwrap_or("");
    let total = usage::fold(v.buckets);
    let (cost, unpriced_calls) = usage::total_cost(v.buckets, v.pricing);

    // The by-model view is also the "what would pricing this buy me"
    // view, so it is rendered even when nothing is priced.
    let by_slot = group_by(v.buckets, |b| b.slot.clone());
    let by_model = group_by(v.buckets, |b| (b.backend.clone(), b.model.clone()));
    let by_day = group_by(v.buckets, |b| b.day.clone());
    let by_month = group_by(v.buckets, |b| b.day.chars().take(7).collect::<String>());
    let by_traffic = group_by(v.buckets, |b| (b.source.clone(), b.tag.clone()));
    let day_peak = by_day
        .values()
        .map(|d| usage::fold(d).total_tokens())
        .max()
        .unwrap_or(0);

    let days = v.days;
    let clean = v.clean;
    let query = |d: i64| {
        let c = if clean { "&clean=1" } else { "" };
        format!("/dashboard/admin/usage?days={d}{c}")
    };

    html! {
        h2 { "Usage & spend" }
        p.muted {
            "Every call this deployment made to a model or to the embedder, counted per "
            "call. Tokens are the measurement; money is an estimate against the rates you "
            "set below, and is only shown for the models you priced."
        }

        @if let Some(saved) = v.saved { (saved_banner(saved)) }

        // ---------- today against the budget ----------
        (budget_block(v))

        // ---------- period + filter ----------
        h3 { "History" }
        p {
            @for (d, label) in WINDOWS {
                @if *d == days {
                    strong { (label) }
                } @else {
                    a href=(query(*d)) { (label) }
                }
                @if *d != WINDOWS[WINDOWS.len() - 1].0 { " · " }
            }
        }
        p.muted {
            @if clean {
                a href=(format!("/dashboard/admin/usage?days={days}")) { "Show everything" }
                " — currently hiding hand-run cycles and tagged experiments"
                @if v.dropped_rows > 0 { " (" (v.dropped_rows) " group" @if v.dropped_rows != 1 { "s" } " hidden)" }
                "."
            } @else {
                a href=(format!("/dashboard/admin/usage?days={days}&clean=1")) { "Production traffic only" }
                " — hide hand-run cycles (" code { "mwe-mcp rem run-cycle" } ", "
                code { "recall eval" } ") and anything a "
                code { "MWE_USAGE_TAG" } " process produced."
            }
        }

        @if v.buckets.is_empty() {
            p.flash.flash-info {
                "Nothing recorded in this window. "
                @match v.first_day {
                    Some(d) => { "The ledger starts on " strong { (d) } "." },
                    None => {
                        "The ledger is empty: it starts at the first model call this "
                        "server makes, and every call after that is in it."
                    },
                }
            }
        } @else {
            // ---------- headline ----------
            div.kpi-grid {
                div.kpi {
                    strong { (thousands(total.total_tokens())) }
                    span { "tokens" }
                }
                div.kpi {
                    strong { (thousands(total.calls)) }
                    span { "calls" @if total.failed > 0 { ", " (total.failed) " failed" } }
                }
                div.kpi {
                    strong { (share(total.cached_prompt_tokens, total.prompt_tokens)) }
                    span { "of prompt served from cache" }
                }
                @if with_money {
                    div.kpi {
                        strong { (money(cost)) " " (currency) }
                        span { "estimated" }
                    }
                }
            }

            @if with_money && unpriced_calls > 0 {
                p.flash.flash-info {
                    "The estimate leaves out " strong { (thousands(unpriced_calls)) }
                    " call" @if unpriced_calls != 1 { "s" }
                    " on models with no rate in the price list"
                    " — their tokens are counted above, their cost is not."
                }
            }
            @if !with_money {
                p.flash.flash-info {
                    "No price list is configured, so this page shows tokens only. Fill in "
                    "the price list below — rates per 1M tokens, in your own currency — and "
                    "the cost columns appear."
                }
            }

            // ---------- by slot ----------
            h3 { "By slot" }
            p.muted {
                "Which part of the engine is spending. The embedder is the one row here "
                "that is not a model slot: it turns text into the vectors search runs on."
            }
            div.table-wrap {
                table.config-table {
                    thead { tr { th { "Slot" } (token_headers(with_money)) th { "Mean latency" } } }
                    tbody {
                        @for (slot, rows) in &by_slot {
                            @let f = usage::fold(rows);
                            tr {
                                td { (slot_label(slot)) " " span.muted { code { (slot) } } }
                                (token_cells(rows, v.pricing, with_money))
                                td.muted {
                                    @if f.calls > 0 { (thousands(f.latency_ms_total / f.calls)) " ms" }
                                    @else { "—" }
                                }
                            }
                        }
                        tr {
                            td { strong { "Total" } }
                            (token_cells(v.buckets, v.pricing, with_money))
                            td {}
                        }
                    }
                }
            }

            // ---------- by model ----------
            h3 { "By model" }
            p.muted {
                "The model carries the price, so this is the table to read "
                "next to your provider's invoice. "
                strong { "How it is paid for" }
                " is not the same question as which provider answered: a slot on a "
                "flat subscription, or a model running on this machine, moves tokens "
                "without moving money."
            }
            div.table-wrap {
                table.config-table {
                    thead { tr { th { "Model" } th { "Paid for" } (token_headers(with_money)) } }
                    tbody {
                        @for ((backend, model), rows) in &by_model {
                            @let f = usage::fold(rows);
                            tr {
                                td { code { (model) } " " span.muted { (backend) } }
                                // Blank when one model was reached two ways
                                // (a key rotated onto a subscription, say);
                                // `fold` refuses to claim either.
                                td.muted { (billing_label(&f.billing)) }
                                (token_cells(rows, v.pricing, with_money))
                            }
                        }
                    }
                }
            }

            // ---------- by month ----------
            @if by_month.len() > 1 || days == 0 {
                h3 { "By month" }
                div.table-wrap {
                    table.config-table {
                        thead { tr { th { "Month" } (token_headers(with_money)) } }
                        tbody {
                            @for (month, rows) in by_month.iter().rev() {
                                tr { td { (month) } (token_cells(rows, v.pricing, with_money)) }
                            }
                        }
                    }
                }
            }

            // ---------- by day ----------
            h3 { "By day" }
            p.muted { "Days are UTC, like the ledger and unlike your evening." }
            div.table-wrap {
                table.config-table {
                    thead { tr { th { "Day" } th {} (token_headers(with_money)) } }
                    tbody {
                        @for (day, rows) in by_day.iter().rev() {
                            @let f = usage::fold(rows);
                            tr {
                                td { (day) }
                                td { (bar(f.total_tokens(), day_peak)) }
                                (token_cells(rows, v.pricing, with_money))
                            }
                        }
                    }
                }
            }

            // ---------- traffic provenance ----------
            @if by_traffic.len() > 1 {
                h3 { "Whose traffic" }
                p.muted {
                    "A month is only clean if you know what is in it. This is "
                    "recorded when the call is made, never guessed afterwards."
                }
                div.table-wrap {
                    table.config-table {
                        thead { tr { th { "Made by" } th { "Experiment tag" } (token_headers(with_money)) } }
                        tbody {
                            @for ((source, tag), rows) in &by_traffic {
                                tr {
                                    td { (source_label(source)) }
                                    td { @match tag {
                                        Some(t) => code { (t) },
                                        None => span.muted { "—" },
                                    } }
                                    (token_cells(rows, v.pricing, with_money))
                                }
                            }
                        }
                    }
                }
            }
        }

        // ---------- the price list ----------
        (pricing_block(v))

        p.muted {
            "Related: which model serves each slot is the "
            a href="/dashboard/admin/llm-config" { "LLM config editor" }
            "; the full prompt/completion recorder is the "
            a href="/dashboard/admin/training-spool" { "training spool" }
            ", which records the prompts themselves and not what they cost. "
            "The bundled embedder is not in this ledger: it runs inside this process on "
            "this machine, so there is no request to count and no bill to explain."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(day: &str, slot: &str, model: &str, prompt: i64, cached: i64) -> UsageBucket {
        UsageBucket {
            day: day.to_owned(),
            slot: slot.to_owned(),
            backend: "anthropic".to_owned(),
            model: model.to_owned(),
            billing: "api".to_owned(),
            source: "serve".to_owned(),
            tag: None,
            calls: 1,
            failed: 0,
            prompt_tokens: prompt,
            cached_prompt_tokens: cached,
            cache_write_tokens: 0,
            completion_tokens: 100,
            latency_ms_total: 500,
        }
    }

    fn priced() -> LlmPricingConfig {
        LlmPricingConfig {
            currency: Some("EUR".to_owned()),
            models: vec![ModelPrice {
                model: "claude-*".to_owned(),
                input: 3.0,
                cached_input: Some(0.3),
                cache_write: None,
                output: 15.0,
            }],
            extra: serde_yaml::Mapping::new(),
        }
    }

    fn no_budget() -> BudgetState {
        BudgetState {
            day: "2026-09-06".to_owned(),
            spent: 0.0,
            unpriced_calls: 0,
            limit: None,
            warn_fraction: 0.8,
            unlocked: false,
            currency: Some("EUR".to_owned()),
        }
    }

    fn capped(spent: f64, limit: f64) -> BudgetState {
        BudgetState {
            spent,
            limit: Some(limit),
            ..no_budget()
        }
    }

    fn view<'a>(
        buckets: &'a [UsageBucket],
        pricing: &'a LlmPricingConfig,
        budget_config: &'a BudgetConfig,
        budget: &'a BudgetState,
    ) -> View<'a> {
        View {
            buckets,
            pricing,
            budget_config,
            budget,
            days: 30,
            clean: false,
            dropped_rows: 0,
            first_day: None,
            read_only: false,
            saved: None,
        }
    }

    /// The defect that only a screen could show: the headline and the
    /// total row are the same quantity computed two ways, and they must
    /// agree.
    ///
    /// They did not. The headline priced each bucket at its own model's
    /// rate; the total row priced the *sum* of every bucket at whichever
    /// model sorted first, and on a seeded month printed 34.05 against
    /// the correct 11.54, four lines apart, both in the same currency
    /// and both plausible. Every per-day, per-month and per-traffic
    /// figure was wrong the same way; only the per-slot rows happened to
    /// be right, because those groups held one model each.
    ///
    /// The data here is built so the two answers cannot coincide: a
    /// cheap model and a dear one, equal token counts, so pricing the
    /// pair at either single rate is visibly not the sum.
    #[test]
    fn the_total_row_agrees_with_the_headline_across_two_models() {
        let pricing = LlmPricingConfig {
            currency: Some("EUR".to_owned()),
            models: vec![
                ModelPrice {
                    model: "cheap-1".to_owned(),
                    input: 1.0,
                    cached_input: None,
                    cache_write: None,
                    output: 1.0,
                },
                ModelPrice {
                    model: "dear-1".to_owned(),
                    input: 100.0,
                    cached_input: None,
                    cache_write: None,
                    output: 100.0,
                },
            ],
            extra: serde_yaml::Mapping::new(),
        };
        // 1M prompt each, no completion: 1.00 + 100.00 = 101.00.
        let mut cheap = bucket("2026-07-29", "ingest", "cheap-1", 1_000_000, 0);
        cheap.completion_tokens = 0;
        let mut dear = bucket("2026-07-29", "cronista", "dear-1", 1_000_000, 0);
        dear.completion_tokens = 0;
        let rows = vec![cheap, dear];

        let cfg = BudgetConfig::default();
        let state = no_budget();
        let html = render(&view(&rows, &pricing, &cfg, &state)).into_string();
        assert!(html.contains("101.00"), "the honest total must appear");
        // The two single-rate answers a fold could have produced.
        assert!(
            !html.contains("2.00") && !html.contains("200.00"),
            "neither model's rate may be applied to the whole group"
        );
        // Headline + By-slot total + By-day row + By-month is absent
        // (one month) — the figure recurs, and never a different one.
        assert!(
            html.matches("101.00").count() >= 3,
            "headline, total row and day row are the same number"
        );
    }

    /// The founder's rule, and the demo's behaviour, are the same rule:
    /// with no price list the page renders tokens and says nothing at
    /// all about money — no zeros, no currency, no "not priced" column
    /// to misread.
    #[test]
    fn without_a_price_list_no_money_is_rendered() {
        let rows = vec![bucket(
            "2026-07-29",
            "ingest",
            "claude-haiku-4-5",
            1_000,
            800,
        )];
        let pricing = LlmPricingConfig::default();
        let cfg = BudgetConfig::default();
        let mut state = no_budget();
        state.currency = None;
        let mut v = view(&rows, &pricing, &cfg, &state);
        v.first_day = Some("2026-07-01");
        let html = render(&v).into_string();
        assert!(!html.contains("Est. cost"), "no cost column without rates");
        assert!(
            !html.contains("not priced"),
            "and no per-row apology either"
        );
        // The measurement is still all there.
        assert!(html.contains("By slot") && html.contains("Conversation &amp; capture"));
        assert!(html.contains("1\u{202f}000"), "prompt tokens are rendered");
        // …and the page says how to turn money on, on the page itself.
        assert!(html.contains("Save price list"));
    }

    #[test]
    fn with_a_price_list_the_cost_columns_appear() {
        let rows = vec![bucket(
            "2026-07-29",
            "ingest",
            "claude-haiku-4-5",
            1_000,
            800,
        )];
        let pricing = priced();
        let cfg = BudgetConfig::default();
        let state = no_budget();
        let html = render(&view(&rows, &pricing, &cfg, &state)).into_string();
        assert!(html.contains("Est. cost"));
        assert!(html.contains("EUR"));
    }

    #[test]
    fn an_empty_ledger_says_so_instead_of_rendering_empty_tables() {
        let pricing = LlmPricingConfig::default();
        let cfg = BudgetConfig::default();
        let state = no_budget();
        let html = render(&view(&[], &pricing, &cfg, &state)).into_string();
        assert!(html.contains("The ledger is empty"));
        assert!(!html.contains("By slot"));
    }

    /// The page the founder asked for opens with the one number that can
    /// change what happens next: today against the budget, with the line
    /// and the percentage.
    #[test]
    fn the_page_shows_todays_spend_against_the_budget_line() {
        let rows = vec![bucket("2026-09-06", "ingest", "claude-x", 1_000, 0)];
        let pricing = priced();
        let cfg = BudgetConfig {
            daily_limit: Some(4.0),
            warn_at_percent: 80,
            extra: serde_yaml::Mapping::new(),
        };
        let state = capped(1.0, 4.0);
        let html = render(&view(&rows, &pricing, &cfg, &state)).into_string();
        assert!(html.contains("spent today of"), "the budget line is named");
        assert!(html.contains("4.00"), "the budget itself is printed");
        assert!(html.contains("25%"), "and how far along today is");
    }

    /// Without a budget the page says so in those words, rather than
    /// printing a zero budget that reads as "you may spend nothing".
    #[test]
    fn with_no_budget_the_page_says_no_budget_set() {
        let pricing = priced();
        let cfg = BudgetConfig::default();
        let state = no_budget();
        let html = render(&view(&[], &pricing, &cfg, &state)).into_string();
        assert!(html.contains("No budget set"));
        assert!(
            !html.contains("stopped"),
            "no budget means nothing was ever stopped"
        );
    }

    /// A stop must read as a stop, and must name both ways out of it —
    /// the button and the budget itself. The alternative this denies is the quiet
    /// one: a page that shows 105% of the budget and says nothing about
    /// what that did.
    #[test]
    fn a_stopped_day_says_so_and_offers_both_ways_out() {
        let pricing = priced();
        let cfg = BudgetConfig {
            daily_limit: Some(4.0),
            warn_at_percent: 80,
            extra: serde_yaml::Mapping::new(),
        };
        let state = capped(4.5, 4.0);
        let html = render(&view(&[], &pricing, &cfg, &state)).into_string();
        assert!(html.contains("Paid model calls are stopped"));
        assert!(
            html.contains("Unlock for today"),
            "the button that lifts it for the day"
        );
        assert!(
            html.contains("Save budget"),
            "and the field that raises the budget"
        );
        // A stop is a spending decision, not a way of running without a
        // model: the page must never offer it as one.
        assert!(!html.contains("without a model"));
    }

    /// Crossing the warning line is news while there is still something
    /// to do about it, and it must not read as a stop.
    #[test]
    fn past_the_warning_line_the_page_warns_without_claiming_a_stop() {
        let pricing = priced();
        let cfg = BudgetConfig {
            daily_limit: Some(4.0),
            warn_at_percent: 80,
            extra: serde_yaml::Mapping::new(),
        };
        let state = capped(3.4, 4.0);
        let html = render(&view(&[], &pricing, &cfg, &state)).into_string();
        assert!(html.contains("Past 85% of the daily budget"));
        assert!(!html.contains("Paid model calls are stopped"));
    }

    /// The embedder is the seventh spender and gets its own row — with
    /// "not reported" where the tokens would be, because an embedding
    /// endpoint reports none and a summed zero would read as free.
    #[test]
    fn the_embedder_gets_its_own_row_and_claims_no_token_counts() {
        let mut e = bucket("2026-09-06", EMBEDDING_SLOT, "bge-m3", 0, 0);
        e.backend = "ollama".to_owned();
        e.billing = "local".to_owned();
        e.completion_tokens = 0;
        e.calls = 41;
        let rows = vec![e, bucket("2026-09-06", "ingest", "claude-x", 1_000, 0)];
        let pricing = priced();
        let cfg = BudgetConfig::default();
        let state = no_budget();
        let html = render(&view(&rows, &pricing, &cfg, &state)).into_string();
        assert!(html.contains("Embedder (search vectors)"));
        assert!(html.contains("not reported"));
        assert!(html.contains("on this machine"), "and how it is paid for");
    }

    /// A table header that repeats an internal field name teaches the
    /// reader the schema instead of the answer.
    #[test]
    fn the_tables_are_headed_in_words_not_in_field_names() {
        let mut hand_run = bucket("2026-09-06", "cronista", "claude-x", 1_000, 0);
        hand_run.source = "rem-cli".to_owned();
        let rows = vec![
            bucket("2026-09-06", "ingest", "claude-x", 1_000, 0),
            hand_run,
        ];
        let pricing = priced();
        let cfg = BudgetConfig::default();
        let state = no_budget();
        let html = render(&view(&rows, &pricing, &cfg, &state)).into_string();
        assert!(html.contains("Tokens in") && html.contains("Tokens out"));
        assert!(html.contains("hand-run cycle") && html.contains("live server"));
        assert!(html.contains("metered"), "`api` is not a word for a person");
        assert!(
            html.contains("Conversation &amp; capture"),
            "the slot's name"
        );
    }

    /// The budget bar is drawn in this dashboard's own accents. A hex
    /// literal borrowed from another palette does not read as "stop" on a
    /// dark phosphor theme — it reads as a rendering bug, which is the
    /// one thing a page about money must not look like.
    #[test]
    fn the_budget_bar_is_drawn_in_the_palette_not_in_hex() {
        let pricing = priced();
        let cfg = BudgetConfig {
            daily_limit: Some(4.0),
            warn_at_percent: 80,
            extra: serde_yaml::Mapping::new(),
        };
        for (state, token) in [
            (capped(1.0, 4.0), "var(--p)"),
            (capped(3.4, 4.0), "var(--amber)"),
            (capped(4.5, 4.0), "var(--rose)"),
        ] {
            let html = render(&view(&[], &pricing, &cfg, &state)).into_string();
            assert!(
                html.contains(&format!("background:{token}")),
                "expected {token} at {} of {:?}: {html}",
                state.spent,
                state.limit
            );
            assert!(!html.contains('#'), "no hex colour belongs in this page");
        }
    }

    /// A frozen instance shows the numbers and none of the controls —
    /// the guard refuses the POSTs anyway, and a button that errors is
    /// worse than no button.
    #[test]
    fn a_frozen_instance_shows_the_figures_and_hides_the_editors() {
        let pricing = priced();
        let cfg = BudgetConfig {
            daily_limit: Some(4.0),
            warn_at_percent: 80,
            extra: serde_yaml::Mapping::new(),
        };
        let state = capped(4.5, 4.0);
        let mut v = view(&[], &pricing, &cfg, &state);
        v.read_only = true;
        let html = render(&v).into_string();
        assert!(html.contains("Paid model calls are stopped"), "still shown");
        assert!(!html.contains("Save budget"));
        assert!(!html.contains("Save price list"));
        assert!(!html.contains("Unlock for today"));
    }

    /// Every wide block on this page carries its own horizontal scroll,
    /// so the document never scrolls sideways on a phone.
    #[test]
    fn the_wide_blocks_all_declare_their_own_scroll() {
        let rows = vec![bucket(
            "2026-07-29",
            "ingest",
            "claude-haiku-4-5",
            1_000,
            800,
        )];
        let pricing = priced();
        let cfg = BudgetConfig::default();
        let state = no_budget();
        let with_tables = render(&view(&rows, &pricing, &cfg, &state)).into_string();
        assert_eq!(
            with_tables.matches("<table").count(),
            with_tables.matches("table-wrap").count(),
            "every table sits in a scrolling wrapper"
        );
    }

    #[test]
    fn thousands_groups_from_the_right() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1\u{202f}000");
        assert_eq!(thousands(19_100_000), "19\u{202f}100\u{202f}000");
        assert_eq!(thousands(-1_234), "-1\u{202f}234");
    }

    /// A cost under a cent must not print as `0.00`: "free" is the one
    /// thing this page must never say by accident.
    #[test]
    fn a_sub_cent_cost_keeps_its_digits() {
        assert_eq!(money(0.004), "0.0040");
        assert_eq!(money(0.0), "0.00");
        assert_eq!(money(12.5), "12.50");
    }

    #[test]
    fn the_window_start_includes_today() {
        assert_eq!(
            since_day(1),
            Some(chrono::Utc::now().format("%Y-%m-%d").to_string())
        );
        assert_eq!(since_day(0), None);
    }

    // ---------- the forms ----------

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// Clearing the budget field removes it, rather than setting
    /// it to zero — which would stop the deployment outright.
    #[test]
    fn an_empty_budget_field_means_no_cap() {
        let prior = BudgetConfig::default();
        let parsed = parse_budget_form(&form(&[("daily_limit", "  ")]), &prior).expect("parse");
        assert_eq!(parsed.daily_limit, None);
        assert_eq!(parsed.warn_at_percent, 80, "the default warning line");
    }

    #[test]
    fn a_budget_that_is_not_a_number_is_refused_by_name() {
        let prior = BudgetConfig::default();
        let err = parse_budget_form(&form(&[("daily_limit", "five euros")]), &prior)
            .expect_err("must refuse");
        assert!(format!("{err:?}").contains("five euros"));
    }

    #[test]
    fn a_warning_threshold_outside_one_to_a_hundred_is_refused() {
        let prior = BudgetConfig::default();
        assert!(
            parse_budget_form(
                &form(&[("daily_limit", "5"), ("warn_at_percent", "0")]),
                &prior
            )
            .is_err()
        );
        assert!(
            parse_budget_form(
                &form(&[("daily_limit", "5"), ("warn_at_percent", "120")]),
                &prior
            )
            .is_err()
        );
        let ok = parse_budget_form(
            &form(&[("daily_limit", "5,50"), ("warn_at_percent", "90")]),
            &prior,
        )
        .expect("parse");
        assert_eq!(ok.daily_limit, Some(5.5), "a decimal comma is a number too");
        assert_eq!(ok.warn_at_percent, 90);
    }

    /// The price list is edited through this form, so its round-trip must
    /// not quietly lose a rate or invent one.
    #[test]
    fn the_price_form_round_trips_a_row_and_drops_a_cleared_one() {
        let prior = LlmPricingConfig {
            currency: Some("EUR".to_owned()),
            models: vec![
                ModelPrice {
                    model: "keep-me".to_owned(),
                    input: 3.0,
                    cached_input: Some(0.3),
                    cache_write: None,
                    output: 15.0,
                },
                ModelPrice {
                    model: "drop-me".to_owned(),
                    input: 1.0,
                    cached_input: None,
                    cache_write: None,
                    output: 1.0,
                },
            ],
            extra: serde_yaml::Mapping::new(),
        };
        let parsed = parse_pricing_form(
            &form(&[
                ("currency", "USD"),
                ("price__0__model", "keep-me"),
                ("price__0__input", "3"),
                ("price__0__cached_input", "0.3"),
                ("price__0__output", "15"),
                ("price__1__model", ""),
                ("price__2__model", "new-one"),
                ("price__2__input", "2"),
                ("price__2__output", "8"),
            ]),
            &prior,
        )
        .expect("parse");
        assert_eq!(parsed.currency.as_deref(), Some("USD"));
        let names: Vec<&str> = parsed.models.iter().map(|m| m.model.as_str()).collect();
        assert_eq!(names, vec!["keep-me", "new-one"]);
        assert_eq!(parsed.models[0].cached_input, Some(0.3));
        assert_eq!(
            parsed.models[1].cached_input, None,
            "an unstated cache rate stays unstated, and falls back to input at read time"
        );
    }

    /// A row with a model and no rates is a half-typed row, and saving
    /// it as a zero would tell the operator that model is free.
    #[test]
    fn a_price_row_without_its_rates_is_refused_by_model_name() {
        let prior = LlmPricingConfig::default();
        let err = parse_pricing_form(
            &form(&[("price__0__model", "half-typed"), ("price__0__input", "3")]),
            &prior,
        )
        .expect_err("must refuse");
        assert!(format!("{err:?}").contains("half-typed"));
    }
}
