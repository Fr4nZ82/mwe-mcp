// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Usage & spend** — what the internal LLM slots consumed, and what
//! it cost if the operator said what they pay.
//!
//! One route, `GET /admin/usage`, reading the per-call ledger
//! ([`mwe_core::usage`]). No writes at all: the period and the filter
//! are query parameters, so the whole page works unchanged on a frozen
//! deployment, and the price list is edited where every other operator
//! setting is, in `mwe-mcp.config.yaml`.
//!
//! # Tokens are the measurement; money is an opinion
//!
//! The page answers three questions, in the order they matter:
//!
//! 1. **What is being consumed, and by what** — tokens per slot and per
//!    model, because "which part of the machine spends" is the only
//!    question whose answer changes a decision.
//! 2. **How it moved over time** — per day and per month, so a clean
//!    month can be read at a glance.
//! 3. **How much of it the cache absorbed** — reads are the discount,
//!    writes are the deposit paid for it, and both are subsets of the
//!    prompt total, so a page that showed only "prompt tokens" would
//!    make a well-cached workload look several times more expensive
//!    than it is.
//!
//! Money is layered **on top** and only where a rate exists. With no
//! `llm_pricing:` section the money columns are not rendered at all —
//! not as zeros, not as dashes. That is not a demo mode, it is the
//! default: published rates move, contracts differ, and the currency is
//! not ours to assume, so a deployment that invented a price would be
//! confidently wrong about somebody else's money. A freshly installed
//! server and the public demo behave identically for exactly this
//! reason, with no branch anywhere that names either of them.
//!
//! # Admin-only
//!
//! The ledger is deployment-wide: it is the sum over every user's
//! turns. It carries no content and no sender, so it is not a
//! confidentiality problem in the way the facts table is — but it is
//! operator telemetry about the machine, which is the same class as
//! Health, and it sits behind the same gate.

use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use maud::{Markup, html};
use mwe_core::config::{CONFIG_FILENAME, Config, LlmPricingConfig};
use mwe_core::usage::{self, UsageBucket};
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;
use crate::ui::layout;

/// Sub-router for `/admin/usage`. Read-only, so it is merged into the
/// authenticated tree with no write path to allow-list.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/admin/usage", get(page))
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

#[derive(Debug, Deserialize)]
struct Params {
    /// Trailing window in days; `0` = the whole ledger.
    days: Option<i64>,
    /// `1` ⇒ count only the running server's untagged traffic, i.e.
    /// drop hand-run cycles and anything a `MWE_USAGE_TAG` process
    /// produced.
    clean: Option<u8>,
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

    // The price list is read from disk on every load rather than cached
    // in `DashboardState`: editing the YAML is how prices are set, and
    // an operator who corrects a rate should see the totals move on the
    // next refresh instead of after a restart.
    //
    // No workdir (identity-only / test builds) is not an error here —
    // it only means no price list, and the tokens are the point.
    let pricing = state
        .memory
        .as_ref()
        .map(|m| Config::load_raw(&m.workdir))
        .transpose()
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?
        .map(|c| c.llm_pricing)
        .unwrap_or_default();

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

    let body = render(
        &buckets,
        &pricing,
        days,
        clean,
        dropped_rows,
        first_day.as_deref(),
    );
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
fn bar(part: i64, whole: i64) -> Markup {
    #[allow(
        clippy::cast_precision_loss,
        reason = "display rounding of counts far below 2^53"
    )]
    let pct = if whole > 0 {
        ((part as f64) * 100.0 / (whole as f64)).clamp(0.0, 100.0)
    } else {
        0.0
    };
    html! {
        span style="display:inline-block;width:6rem;height:.5rem;background:var(--bg-3);border-radius:9999px;overflow:hidden;vertical-align:middle" {
            span style=(format!("display:block;height:100%;width:{pct:.1}%;background:var(--p)")) {}
        }
    }
}

/// The token columns shared by every table on the page.
fn token_headers(with_money: bool) -> Markup {
    html! {
        th { "Calls" }
        th { "Prompt" }
        th { "cached" }
        th { "written" }
        th { "Completion" }
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
    html! {
        td { (thousands(f.calls)) @if f.failed > 0 { " " span.muted { "(" (f.failed) " failed)" } } }
        td { (thousands(f.prompt_tokens)) }
        td.muted { (thousands(f.cached_prompt_tokens)) " " span.muted { "(" (share(f.cached_prompt_tokens, f.prompt_tokens)) ")" } }
        td.muted { (thousands(f.cache_write_tokens)) }
        td { (thousands(f.completion_tokens)) }
        td { strong { (thousands(f.total_tokens())) } }
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

#[allow(
    clippy::too_many_lines,
    reason = "one page, one flat template — splitting the sections into helpers \
              hides the reading order that IS the design"
)]
fn render(
    buckets: &[UsageBucket],
    pricing: &LlmPricingConfig,
    days: i64,
    clean: bool,
    dropped_rows: usize,
    first_day: Option<&str>,
) -> Markup {
    let with_money = !pricing.is_empty();
    let currency = pricing.currency.as_deref().unwrap_or("");
    let total = usage::fold(buckets);
    let (cost, unpriced_calls) = usage::total_cost(buckets, pricing);

    // The by-model view is also the "what would pricing this buy me"
    // view, so it is rendered even when nothing is priced.
    let by_slot = group_by(buckets, |b| b.slot.clone());
    let by_model = group_by(buckets, |b| (b.backend.clone(), b.model.clone()));
    let by_day = group_by(buckets, |b| b.day.clone());
    let by_month = group_by(buckets, |b| b.day.chars().take(7).collect::<String>());
    let by_traffic = group_by(buckets, |b| (b.source.clone(), b.tag.clone()));
    let day_peak = by_day
        .values()
        .map(|v| usage::fold(v).total_tokens())
        .max()
        .unwrap_or(0);

    let query = |d: i64| {
        let c = if clean { "&clean=1" } else { "" };
        format!("/dashboard/admin/usage?days={d}{c}")
    };

    html! {
        h2 { "Usage & spend" }
        p.muted {
            "Every call the internal LLM slots made, counted per call. "
            "Tokens are the measurement; money is an estimate against the "
            "rates you configured, and is only shown for the models you priced."
        }

        // ---------- period + filter ----------
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
                @if dropped_rows > 0 { " (" (dropped_rows) " group" @if dropped_rows != 1 { "s" } " hidden)" }
                "."
            } @else {
                a href=(format!("/dashboard/admin/usage?days={days}&clean=1")) { "Production traffic only" }
                " — hide hand-run cycles (" code { "mwe-mcp rem run-cycle" } ", "
                code { "recall eval" } ") and anything a "
                code { "MWE_USAGE_TAG" } " process produced."
            }
        }

        @if buckets.is_empty() {
            p.flash.flash-info {
                "Nothing recorded in this window. "
                @match first_day {
                    Some(d) => { "The ledger starts on " strong { (d) } "." },
                    None => {
                        "The ledger is empty: recording begins with the first model "
                        "call this server makes after the upgrade that introduced it. "
                        "A month measured from today is a clean month."
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
                    " on models with no rate in " code { "llm_pricing:" }
                    " — their tokens are counted above, their cost is not."
                }
            }
            @if !with_money {
                p.flash.flash-info {
                    "No price list is configured, so this page shows tokens only. "
                    "Add an " code { "llm_pricing:" } " section to " code { (CONFIG_FILENAME) }
                    " — rates per 1M tokens, in your own currency — and the cost "
                    "columns appear. Nothing is assumed on your behalf: published "
                    "rates change and your contract may not be the published one."
                }
            }

            // ---------- by slot ----------
            h3 { "By slot" }
            p.muted { "Which part of the engine is spending." }
            div.table-wrap {
                table.config-table {
                    thead { tr { th { "Slot" } (token_headers(with_money)) th { "Mean latency" } } }
                    tbody {
                        @for (slot, rows) in &by_slot {
                            @let f = usage::fold(rows);
                            tr {
                                td { code { (slot) } }
                                (token_cells(rows, pricing, with_money))
                                td.muted {
                                    @if f.calls > 0 { (thousands(f.latency_ms_total / f.calls)) " ms" }
                                    @else { "—" }
                                }
                            }
                        }
                        tr {
                            td { strong { "Total" } }
                            (token_cells(buckets, pricing, with_money))
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
                    thead { tr { th { "Model" } th { "Paid" } (token_headers(with_money)) } }
                    tbody {
                        @for ((backend, model), rows) in &by_model {
                            @let f = usage::fold(rows);
                            tr {
                                td { code { (model) } " " span.muted { (backend) } }
                                // Blank when one model was reached two ways
                                // (a key rotated onto a subscription, say);
                                // `fold` refuses to claim either.
                                td.muted { @if f.billing.is_empty() { "mixed" } @else { (f.billing) } }
                                (token_cells(rows, pricing, with_money))
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
                                tr { td { (month) } (token_cells(rows, pricing, with_money)) }
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
                                (token_cells(rows, pricing, with_money))
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
                        thead { tr { th { "Source" } th { "Tag" } (token_headers(with_money)) } }
                        tbody {
                            @for ((source, tag), rows) in &by_traffic {
                                tr {
                                    td { code { (source) } }
                                    td { @match tag {
                                        Some(t) => code { (t) },
                                        None => span.muted { "—" },
                                    } }
                                    (token_cells(rows, pricing, with_money))
                                }
                            }
                        }
                    }
                }
            }
        }

        // ---------- the price list ----------
        h3 { "Price list" }
        @if pricing.is_empty() {
            p.muted {
                "None configured. Rates go in " code { (CONFIG_FILENAME) }
                ", per 1M tokens, in whatever currency you are billed in:"
            }
            // The block scrolls inside itself. A `pre` holds lines that
            // must not be re-wrapped, so on a narrow screen it is the one
            // element on this page wide enough to push the *document*
            // sideways — measured at 675px against a 390px viewport, and
            // the only uncontained overflow on any dashboard page. Every
            // table here is already inside `.table-wrap`, which does the
            // same job; this is the same rule for the one block that is
            // not a table. Inline design tokens, so no Tailwind utility
            // has to be recompiled for one element.
            pre style="overflow-x:auto;max-width:100%" { code {
"llm_pricing:
  currency: EUR
  models:
    # exact model id, or a prefix wildcard
    - model: \"gemini-3-flash-*\"
      input: 0.30
      cached_input: 0.075   # cache read
      cache_write: 0.375    # cache write
      output: 2.50"
            } }
            p.muted {
                "An omitted " code { "cached_input" } " or " code { "cache_write" }
                " falls back to the input rate, so a half-filled entry gives you an "
                "upper bound rather than a discount nobody promised. The longest "
                "matching wildcard wins, so a specific entry always beats a "
                "catch-all whatever order they are written in."
            }
        } @else {
            p.muted {
                "Per 1M tokens" @if !currency.is_empty() { ", in " code { (currency) } }
                ". Edited in " code { (CONFIG_FILENAME) } "; no restart needed."
            }
            div.table-wrap {
                table.config-table {
                    thead { tr {
                        th { "Model" } th { "Input" } th { "Cache read" }
                        th { "Cache write" } th { "Output" }
                    } }
                    tbody {
                        @for m in &pricing.models {
                            tr {
                                td { code { (m.model) } }
                                td { (money(m.input)) }
                                td {
                                    (money(m.cached_input_rate()))
                                    @if m.cached_input.is_none() { " " span.muted { "(= input)" } }
                                }
                                td {
                                    (money(m.cache_write_rate()))
                                    @if m.cache_write.is_none() { " " span.muted { "(= input)" } }
                                }
                                td { (money(m.output)) }
                            }
                        }
                    }
                }
            }
        }

        p.muted {
            "Related: which model serves each slot is the "
            a href="/dashboard/admin/llm-config" { "LLM config editor" }
            "; the full prompt/completion recorder is the "
            a href="/dashboard/admin/training-spool" { "training spool" }
            ", which is a distillation dataset and not this. Embedding calls "
            "are not counted here — the bundled embedder runs locally."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mwe_core::config::ModelPrice;

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

        let html = render(&rows, &pricing, 30, false, 0, None).into_string();
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
        let html = render(
            &rows,
            &LlmPricingConfig::default(),
            30,
            false,
            0,
            Some("2026-07-01"),
        )
        .into_string();
        assert!(!html.contains("Est. cost"), "no cost column without rates");
        assert!(
            !html.contains("not priced"),
            "and no per-row apology either"
        );
        // The measurement is still all there.
        assert!(html.contains("By slot") && html.contains("ingest"));
        assert!(html.contains("1\u{202f}000"), "prompt tokens are rendered");
        // …and the page says how to turn money on.
        assert!(html.contains("llm_pricing:"));
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
        let html = render(&rows, &priced(), 30, false, 0, Some("2026-07-01")).into_string();
        assert!(html.contains("Est. cost"));
        assert!(html.contains("EUR"));
    }

    #[test]
    fn an_empty_ledger_says_so_instead_of_rendering_empty_tables() {
        let html = render(&[], &LlmPricingConfig::default(), 30, false, 0, None).into_string();
        assert!(html.contains("The ledger is empty"));
        assert!(!html.contains("By slot"));
    }

    /// Every wide block on this page carries its own horizontal scroll,
    /// so the document never scrolls sideways on a phone.
    ///
    /// The tables get it from `.table-wrap`; the YAML example is the one
    /// block that is not a table, and it was the only uncontained
    /// overflow on any dashboard page when this was measured in a
    /// browser (675px of `<code>` against a 390px viewport). A markup
    /// test cannot measure pixels — what it can do is refuse to let the
    /// declaration be deleted by someone editing the example text.
    #[test]
    fn the_wide_blocks_all_declare_their_own_scroll() {
        let html = render(&[], &LlmPricingConfig::default(), 30, false, 0, None).into_string();
        assert!(
            html.contains("<pre style=\"overflow-x:auto;max-width:100%\">"),
            "the config example must scroll inside itself"
        );
        let rows = vec![bucket(
            "2026-07-29",
            "ingest",
            "claude-haiku-4-5",
            1_000,
            800,
        )];
        let with_tables = render(&rows, &priced(), 30, false, 0, None).into_string();
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
}
