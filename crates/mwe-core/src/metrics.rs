// SPDX-License-Identifier: AGPL-3.0-or-later
//! The numbers an operator's scraper reads, and the Prometheus text they
//! are written as.
//!
//! [`collect`] answers a whole scrape from the tables the engine already
//! keeps — the audit trail, the usage ledger, the recall traces, the
//! dream journal — plus three facts about the process itself. Nothing
//! here is counted twice: there is no in-memory counter to drift from
//! the row that pays for it, and adding a metric is adding a query, not
//! a hook in a hot path.
//!
//! # The windows are in the names
//!
//! Every table this reads is pruned on a retention window, so a running
//! total taken from one of them **goes down** when the sweep runs — and
//! a Prometheus counter that goes down is read as a process restart,
//! which turns one prune into a false spike. So no metric here pretends
//! to be an all-time total:
//!
//! - The volume metrics are scoped to the **current UTC day** and say so
//!   in their names (`…_today`). Within the day they only rise, which is
//!   exactly a counter whose reset is midnight — `rate()` and
//!   `increase()` handle the rollover the way they handle a restart.
//!   UTC, not local, because the ledger and the daily budget are UTC and
//!   three clocks would be two too many.
//! - The dream journal keeps its newest
//!   [`MAX_HISTORY`](crate::dream_journal::MAX_HISTORY) runs and no
//!   longer, so its outcomes are gauges over that window, and the
//!   question an operator actually alerts on — *did Full REM run last
//!   night, and did it work* — is answered by the timestamp of the last
//!   run of each kind.
//!
//! # What is deliberately not here
//!
//! **Model-slot reachability.** The Health console probes it with a real
//! call per slot ([`crate::diagnostics::probe_llm_slots`]), which costs
//! a round trip and, on a metered backend, money. A scrape happens every
//! few seconds and unattended; it may not spend. What the ledger knows
//! about the slots — how many calls, how many errors, how much latency —
//! is here instead, and it is a record of calls that actually happened.
//!
//! # The exposition
//!
//! [`render`] writes the Prometheus text exposition format, version
//! 0.0.4: a `# HELP` and a `# TYPE` line per family, then one line per
//! sample. Hand-written rather than taken from a metrics crate because
//! every value here is read from SQL at scrape time — there is no
//! registry to hold, no counter to increment, and nothing a registry
//! would do for this that a `String` does not.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use sqlx::{Row, SqlitePool};

use crate::budget::BudgetState;

/// How many recall traces the latency figures are taken over.
///
/// The newest ones: a percentile over the whole retained journal would
/// answer "how fast was recall this quarter", and the operator watching
/// a dashboard is asking "how fast is it now".
pub const RECALL_SAMPLE: i64 = 100;

/// Every family [`collect`] can publish, in the order it writes them.
///
/// The roster is public because two things have to agree with it and
/// neither can be checked by reading the code: the operator's guide page
/// documents one line per name, and a name that changes breaks the
/// alerts an operator wrote against it. A test in this module holds
/// [`collect`] to this list, and one in `mwe-dashboard` holds the guide
/// page to it — so a family added here without a sentence for the
/// operator fails the build.
pub const FAMILIES: &[&str] = &[
    "mwe_build_info",
    "mwe_uptime_seconds",
    "mwe_database_bytes",
    "mwe_turns_today",
    "mwe_tool_calls_today",
    "mwe_tool_errors_today",
    "mwe_llm_calls_today",
    "mwe_llm_call_errors_today",
    "mwe_llm_latency_ms_today",
    "mwe_llm_tokens_today",
    "mwe_spend_today",
    "mwe_spend_limit",
    "mwe_spend_stopped",
    "mwe_spend_unpriced_calls_today",
    "mwe_recall_ms_p50",
    "mwe_recall_ms_p90",
    "mwe_recall_ms_max",
    "mwe_dream_last_run_timestamp_seconds",
    "mwe_dream_last_run_ok",
    "mwe_dream_runs",
];

/// Prometheus type of a metric family. The two shapes this exposition
/// uses; a histogram or a summary would need bucket bookkeeping the
/// engine does not keep.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// A value that goes up and down freely.
    Gauge,
    /// A value that only rises until it resets. Every counter here
    /// resets at 00:00 UTC — see the module header.
    Counter,
}

impl Kind {
    /// The token the `# TYPE` line carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gauge => "gauge",
            Self::Counter => "counter",
        }
    }
}

/// One measurement: its labels and its value.
#[derive(Debug)]
pub struct Sample {
    /// Label pairs, written in the order given.
    pub labels: Vec<(String, String)>,
    /// The value.
    pub value: f64,
}

impl Sample {
    /// A sample with no labels.
    #[must_use]
    pub const fn bare(value: f64) -> Self {
        Self {
            labels: Vec::new(),
            value,
        }
    }

    /// A sample with labels, from anything that borrows as `str`.
    #[must_use]
    pub fn labelled(labels: &[(&str, &str)], value: f64) -> Self {
        Self {
            labels: labels
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            value,
        }
    }
}

/// A named metric with its documentation and its samples.
///
/// A family with no samples is dropped by [`render`]: two `# HELP` lines
/// and nothing under them tell a reader less than their absence does.
#[derive(Debug)]
pub struct Family {
    /// Metric name, `mwe_`-prefixed.
    pub name: &'static str,
    /// One sentence saying what the number is and over what window.
    pub help: String,
    /// Gauge or counter.
    pub kind: Kind,
    /// The measurements.
    pub samples: Vec<Sample>,
}

impl Family {
    /// Build a family from its parts.
    #[must_use]
    pub fn new(
        name: &'static str,
        kind: Kind,
        help: impl Into<String>,
        samples: Vec<Sample>,
    ) -> Self {
        Self {
            name,
            help: help.into(),
            kind,
            samples,
        }
    }
}

/// Everything [`collect`] needs that is not in the database.
#[derive(Debug)]
pub struct Reading<'a> {
    /// The engine database, for its size on disk.
    pub db_path: &'a Path,
    /// How long this process has been serving.
    pub uptime: Duration,
    /// Version of the running binary.
    pub version: &'a str,
    /// Today's spend against today's budget.
    ///
    /// The server always has one: the budget guard is installed beside
    /// the usage ledger before the first backend is built, so every
    /// command that opens the database has both. `None` is the library
    /// caller who installed neither, and drops the spend families rather
    /// than reporting a budget nobody set as zero.
    pub spend: Option<&'a BudgetState>,
}

/// Read one scrape.
///
/// Four queries and a look at the database on disk, none of them
/// touching a model or the network. Ordered as the exposition reads:
/// what the process is, what came in, what the models cost, how fast
/// recall answered, and what the Dream console recorded.
///
/// # Errors
///
/// Propagates any sqlx failure — a scrape that cannot read the database
/// is an error, not a page of zeroes.
pub async fn collect(pool: &SqlitePool, reading: Reading<'_>) -> crate::Result<Vec<Family>> {
    let today = today_utc();
    let mut out = vec![
        Family::new(
            "mwe_build_info",
            Kind::Gauge,
            "Always 1; the version of the running binary is the label.",
            vec![Sample::labelled(&[("version", reading.version)], 1.0)],
        ),
        Family::new(
            "mwe_uptime_seconds",
            Kind::Gauge,
            "Seconds since this server process started serving.",
            vec![Sample::bare(reading.uptime.as_secs_f64())],
        ),
        Family::new(
            "mwe_database_bytes",
            Kind::Gauge,
            "Size on disk of engine.db together with its write-ahead sidecars.",
            vec![Sample::bare(database_bytes(reading.db_path))],
        ),
    ];
    out.extend(traffic_today(pool, &today).await?);
    out.extend(models_today(pool, &today).await?);
    out.extend(spend(reading.spend));
    out.extend(recall_latency(pool).await?);
    out.extend(dream_history(pool).await?);
    Ok(out)
}

/// Render families as the Prometheus text exposition format, version
/// 0.0.4.
///
/// Empty families are skipped; the rest are written in the order given,
/// each with its `# HELP` and `# TYPE` header, and every line — headers
/// and samples alike — is newline-terminated.
#[must_use]
pub fn render(families: &[Family]) -> String {
    let mut out = String::new();
    for family in families.iter().filter(|f| !f.samples.is_empty()) {
        let _ = writeln!(out, "# HELP {} {}", family.name, escape_help(&family.help));
        let _ = writeln!(out, "# TYPE {} {}", family.name, family.kind.as_str());
        for sample in &family.samples {
            out.push_str(family.name);
            if !sample.labels.is_empty() {
                out.push('{');
                for (i, (key, value)) in sample.labels.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "{key}=\"{}\"", escape_label(value));
                }
                out.push('}');
            }
            let _ = writeln!(out, " {}", sample_value(sample.value));
        }
    }
    out
}

// ---------- the families ----------

/// What came in today, by the credential that sent it.
///
/// One query over the audit trail, folded three ways. The label is the
/// **device label** the credential carries, which is what the audit
/// trail records: the Tokens page fills it with the consumer's name, so
/// in practice it reads as the consumer, but an operator who labelled
/// two credentials of one consumer differently gets two series, and
/// calling the label `consumer` would have made that a lie.
async fn traffic_today(pool: &SqlitePool, today: &str) -> crate::Result<Vec<Family>> {
    let rows = sqlx::query(
        "SELECT device_label, tool_name,
                COUNT(*)                                      AS calls,
                SUM(CASE WHEN error IS NULL THEN 0 ELSE 1 END) AS failed
         FROM tool_executions
         WHERE timestamp >= ?1
         GROUP BY device_label, tool_name",
    )
    .bind(today)
    .fetch_all(pool)
    .await
    .map_err(|e| crate::Error::Other(format!("metrics traffic: {e}")))?;

    let mut turns: Vec<(String, f64)> = Vec::new();
    let mut calls: Vec<(String, f64)> = Vec::new();
    let mut errors: Vec<(String, f64)> = Vec::new();
    for row in rows {
        let device: String = row.get("device_label");
        let tool: String = row.get("tool_name");
        let n: i64 = row.get("calls");
        let failed: i64 = row.get("failed");
        #[allow(clippy::cast_precision_loss, reason = "call counts are far below 2^53")]
        let (n, failed) = (n as f64, failed as f64);
        if tool == TURN_TOOL {
            add(&mut turns, &device, n);
        }
        add(&mut calls, &device, n);
        add(&mut errors, &device, failed);
    }

    Ok(vec![
        Family::new(
            "mwe_turns_today",
            Kind::Counter,
            "Turns ingested today (UTC), by the device label on the calling credential. \
             Resets at 00:00 UTC.",
            by_device(turns),
        ),
        Family::new(
            "mwe_tool_calls_today",
            Kind::Counter,
            "Tool calls of every kind today (UTC), by the device label on the calling \
             credential. Resets at 00:00 UTC.",
            by_device(calls),
        ),
        Family::new(
            "mwe_tool_errors_today",
            Kind::Counter,
            "Tool calls that failed today (UTC), by the device label on the calling \
             credential. A subset of mwe_tool_calls_today. Resets at 00:00 UTC.",
            by_device(errors),
        ),
    ])
}

/// The MCP tool a turn arrives through — the audit row that counts as
/// one exchange between a person and their consumer.
const TURN_TOOL: &str = "wiki_ingest_message";

/// What the models did today, from the usage ledger.
///
/// The ledger's own rollup, re-grouped: the grain it stores is finer
/// than a scrape wants (it keeps the day, the source and the experiment
/// tag as well), and folding those away here keeps the series count
/// down without losing what carries the price.
async fn models_today(pool: &SqlitePool, today: &str) -> crate::Result<Vec<Family>> {
    let rows = sqlx::query(
        "SELECT slot, backend, model,
                COUNT(*)                                      AS calls,
                SUM(CASE WHEN error IS NULL THEN 0 ELSE 1 END) AS failed,
                COALESCE(SUM(latency_ms), 0)                  AS latency_ms,
                COALESCE(SUM(prompt_tokens), 0)               AS prompt_tokens,
                COALESCE(SUM(completion_tokens), 0)           AS completion_tokens
         FROM llm_usage
         WHERE ts >= ?1
         GROUP BY slot, backend, model
         ORDER BY slot, backend, model",
    )
    .bind(today)
    .fetch_all(pool)
    .await
    .map_err(|e| crate::Error::Other(format!("metrics models: {e}")))?;

    let mut calls = Vec::new();
    let mut errors = Vec::new();
    let mut latency = Vec::new();
    let mut tokens = Vec::new();
    for row in rows {
        let slot: String = row.get("slot");
        let backend: String = row.get("backend");
        let model: String = row.get("model");
        let dims: &[(&str, &str)] = &[("slot", &slot), ("backend", &backend), ("model", &model)];
        let with_direction = |direction: &str| {
            let mut v = dims.to_vec();
            v.push(("direction", direction));
            v.iter()
                .map(|&(k, val)| (k.to_owned(), val.to_owned()))
                .collect::<Vec<_>>()
        };
        #[allow(
            clippy::cast_precision_loss,
            reason = "counts and millisecond sums are far below 2^53"
        )]
        {
            calls.push(Sample::labelled(dims, row.get::<i64, _>("calls") as f64));
            errors.push(Sample::labelled(dims, row.get::<i64, _>("failed") as f64));
            latency.push(Sample::labelled(
                dims,
                row.get::<i64, _>("latency_ms") as f64,
            ));
            tokens.push(Sample {
                labels: with_direction("prompt"),
                value: row.get::<i64, _>("prompt_tokens") as f64,
            });
            tokens.push(Sample {
                labels: with_direction("completion"),
                value: row.get::<i64, _>("completion_tokens") as f64,
            });
        }
    }

    Ok(vec![
        Family::new(
            "mwe_llm_calls_today",
            Kind::Counter,
            "Model calls today (UTC), by slot, backend and model. The embedder is recorded \
             under the slot name `embedding`. Resets at 00:00 UTC.",
            calls,
        ),
        Family::new(
            "mwe_llm_call_errors_today",
            Kind::Counter,
            "Model calls today (UTC) the backend refused or could not answer. A subset of \
             mwe_llm_calls_today. Resets at 00:00 UTC.",
            errors,
        ),
        Family::new(
            "mwe_llm_latency_ms_today",
            Kind::Counter,
            "Milliseconds spent waiting on model calls today (UTC); divide by \
             mwe_llm_calls_today for the mean. Resets at 00:00 UTC.",
            latency,
        ),
        Family::new(
            "mwe_llm_tokens_today",
            Kind::Counter,
            "Tokens moved today (UTC), by direction: `prompt` counts what was sent, cached \
             and cache-written prompt included, and `completion` what the model emitted. \
             Resets at 00:00 UTC.",
            tokens,
        ),
    ])
}

/// Today's spend against today's budget.
///
/// `mwe_spend_limit` is absent when no budget is set, which is the
/// difference between "the ceiling is zero" and "there is no ceiling" —
/// the one difference an alert on this metric turns on.
fn spend(state: Option<&BudgetState>) -> Vec<Family> {
    let Some(state) = state else {
        return Vec::new();
    };
    #[allow(clippy::cast_precision_loss, reason = "a call count, far below 2^53")]
    let unpriced = state.unpriced_calls as f64;
    vec![
        Family::new(
            "mwe_spend_today",
            Kind::Gauge,
            "Estimated money spent today (UTC) on metered model calls, in the price list's \
             currency. Resets at 00:00 UTC.",
            vec![Sample::bare(state.spent)],
        ),
        Family::new(
            "mwe_spend_limit",
            Kind::Gauge,
            "The daily budget, in the same currency. Absent when no budget is set.",
            state.limit.map(Sample::bare).into_iter().collect(),
        ),
        Family::new(
            "mwe_spend_stopped",
            Kind::Gauge,
            "1 while metered model calls are refused because today's budget is spent, \
             0 otherwise.",
            vec![Sample::bare(f64::from(u8::from(state.stopped())))],
        ),
        Family::new(
            "mwe_spend_unpriced_calls_today",
            Kind::Gauge,
            "Model calls today (UTC) on a model with no rate in the price list: their tokens \
             are counted and their cost is not, so the budget above is looser than it looks \
             while this is above zero.",
            vec![Sample::bare(unpriced)],
        ),
    ]
}

/// How long recall took, over the newest [`RECALL_SAMPLE`] traces.
///
/// The percentile is computed here rather than in SQL because `SQLite` has
/// no percentile function, and the sample is small enough that reading
/// it is cheaper than the window function would be.
async fn recall_latency(pool: &SqlitePool) -> crate::Result<Vec<Family>> {
    let rows = sqlx::query(
        "SELECT json_extract(payload, '$.recall_ms') AS ms
         FROM recall_traces ORDER BY id DESC LIMIT ?1",
    )
    .bind(RECALL_SAMPLE)
    .fetch_all(pool)
    .await
    .map_err(|e| crate::Error::Other(format!("metrics recall: {e}")))?;

    let mut ms: Vec<i64> = rows
        .into_iter()
        .filter_map(|r| r.get::<Option<i64>, _>("ms"))
        .collect();
    if ms.is_empty() {
        return Ok(Vec::new());
    }
    ms.sort_unstable();
    let window = format!("over the newest {RECALL_SAMPLE} recorded recalls");

    Ok(vec![
        Family::new(
            "mwe_recall_ms_p50",
            Kind::Gauge,
            format!("Median milliseconds a recall took, {window}."),
            vec![Sample::bare(percentile(&ms, 0.50))],
        ),
        Family::new(
            "mwe_recall_ms_p90",
            Kind::Gauge,
            format!("Milliseconds the slowest tenth of recalls exceeded, {window}."),
            vec![Sample::bare(percentile(&ms, 0.90))],
        ),
        Family::new(
            "mwe_recall_ms_max",
            Kind::Gauge,
            format!("The slowest recall, in milliseconds, {window}."),
            vec![Sample::bare(percentile(&ms, 1.0))],
        ),
    ])
}

/// What the Dream console recorded, from the dream journal.
///
/// Three kinds, and only one of them is nightly — the distinction the
/// console draws with its own buttons: `light` is the frequent cycle
/// that places waiting captures, `compile` is the page rewrite on its
/// own, and `full` is Full REM, the nightly reorganisation on the strong
/// models. Reading them as one thing is exactly the confusion the names
/// exist to prevent.
///
/// The journal keeps its newest
/// [`MAX_HISTORY`](crate::dream_journal::MAX_HISTORY) runs, so the
/// outcome counts are gauges over that window. The timestamp of the last
/// run of each kind is the one an operator alerts on: a Full REM that
/// has not finished for two days is the failure that shows up nowhere
/// else.
async fn dream_history(pool: &SqlitePool) -> crate::Result<Vec<Family>> {
    let rows = sqlx::query(
        "SELECT kind, ok, COUNT(*) AS runs, MAX(finished_at) AS last_finished
         FROM dream_runs GROUP BY kind, ok ORDER BY kind, ok",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| crate::Error::Other(format!("metrics dream history: {e}")))?;

    let mut runs = Vec::new();
    // Newest finish per kind, whatever its outcome, and the outcome that
    // came with it: the pair answers "did this kind run, and did it
    // work" — asked of each kind separately, because they run on
    // different cadences and a healthy `light` says nothing about
    // `full`.
    let mut latest: Vec<(String, String, bool)> = Vec::new();
    for row in rows {
        let kind: String = row.get("kind");
        let ok: i64 = row.get("ok");
        let ok = ok != 0;
        #[allow(clippy::cast_precision_loss, reason = "run counts are tiny")]
        let count = row.get::<i64, _>("runs") as f64;
        runs.push(Sample::labelled(
            &[("kind", &kind), ("ok", if ok { "true" } else { "false" })],
            count,
        ));
        let finished: Option<String> = row.get("last_finished");
        if let Some(finished) = finished {
            match latest.iter_mut().find(|(k, _, _)| *k == kind) {
                Some(entry) if entry.1 >= finished => {},
                Some(entry) => {
                    entry.1 = finished;
                    entry.2 = ok;
                },
                None => latest.push((kind, finished, ok)),
            }
        }
    }
    latest.sort_by(|a, b| a.0.cmp(&b.0));

    let last_at: Vec<Sample> = latest
        .iter()
        .filter_map(|(kind, finished, _)| {
            let ts = chrono::DateTime::parse_from_rfc3339(finished).ok()?;
            #[allow(
                clippy::cast_precision_loss,
                reason = "a unix timestamp in seconds is far below 2^53"
            )]
            Some(Sample::labelled(&[("kind", kind)], ts.timestamp() as f64))
        })
        .collect();
    let last_ok: Vec<Sample> = latest
        .iter()
        .map(|(kind, _, ok)| Sample::labelled(&[("kind", kind)], f64::from(u8::from(*ok))))
        .collect();

    Ok(vec![
        Family::new(
            "mwe_dream_last_run_timestamp_seconds",
            Kind::Gauge,
            "Unix time the most recent run of each kind finished. Alert on kind=\"full\", \
             Full REM: the nightly reorganisation stopping fails silently otherwise.",
            last_at,
        ),
        Family::new(
            "mwe_dream_last_run_ok",
            Kind::Gauge,
            "1 when the most recent run of that kind succeeded, 0 when it failed.",
            last_ok,
        ),
        Family::new(
            "mwe_dream_runs",
            Kind::Gauge,
            format!(
                "Recorded runs by kind and outcome, over the newest {} the journal keeps — \
                 a window, not a total.",
                crate::dream_journal::MAX_HISTORY
            ),
            runs,
        ),
    ])
}

// ---------- helpers ----------

/// Today's date in UTC, `YYYY-MM-DD`.
///
/// Both tables stamp RFC-3339 UTC, whose first ten characters are the
/// day, so `>=` against this string is "today or later" **and** uses the
/// timestamp index — which `substr(ts, 1, 10) >= ?` would not.
fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// `engine.db` plus the `-wal` and `-shm` sidecars `SQLite` keeps beside
/// it: the operator watching a disk fill up is watching all three, and a
/// number that left the write-ahead log out would be the one that is
/// wrong exactly when it matters.
fn database_bytes(db_path: &Path) -> f64 {
    let mut total = 0_u64;
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db_path.as_os_str().to_owned();
        path.push(suffix);
        if let Ok(meta) = std::fs::metadata(Path::new(&path)) {
            total = total.saturating_add(meta.len());
        }
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "a file size in bytes; the estimate is exact well past any real database"
    )]
    {
        total as f64
    }
}

/// Add `n` to `device`'s running total, appending it if it is new.
fn add(into: &mut Vec<(String, f64)>, device: &str, n: f64) {
    match into.iter_mut().find(|(d, _)| d == device) {
        Some(entry) => entry.1 += n,
        None => into.push((device.to_owned(), n)),
    }
}

/// Turn per-device totals into samples, ordered by device.
///
/// Every family in this module comes out in a fixed order — by `ORDER
/// BY` where the rows are used as they arrive, here by sorting after the
/// fold. A scrape diffed against the previous one should differ in its
/// numbers, not in where its lines sit.
fn by_device(mut totals: Vec<(String, f64)>) -> Vec<Sample> {
    totals.sort_by(|a, b| a.0.cmp(&b.0));
    totals
        .into_iter()
        .map(|(device, value)| Sample::labelled(&[("device", &device)], value))
        .collect()
}

/// The `q`-th percentile of an already-sorted, non-empty slice, by
/// nearest rank — the reading a scraper expects of a value taken from a
/// finite sample rather than interpolated out of one.
fn percentile(sorted: &[i64], q: f64) -> f64 {
    debug_assert!(!sorted.is_empty(), "percentile of an empty sample");
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "an index into a sample of at most RECALL_SAMPLE elements"
    )]
    let rank = ((sorted.len() as f64) * q).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    #[allow(
        clippy::cast_precision_loss,
        reason = "a millisecond duration, far below 2^53"
    )]
    {
        sorted[idx] as f64
    }
}

/// A sample's value as the exposition spells it.
///
/// Prometheus writes the three non-finite floats its own way, and a
/// scraper that meets Rust's spelling of one fails the parse of the
/// **whole document** — every other metric on this page goes with it. So
/// the spelling is translated here rather than trusted not to arise: a
/// price list an operator typed is the one number on this page that
/// arrives from outside.
fn sample_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_owned()
    } else if v.is_infinite() {
        if v.is_sign_positive() { "+Inf" } else { "-Inf" }.to_owned()
    } else {
        v.to_string()
    }
}

/// Escape a `# HELP` line: a backslash and a newline are the two
/// characters that would end or continue the line somewhere else.
fn escape_help(help: &str) -> String {
    help.replace('\\', "\\\\").replace('\n', "\\n")
}

/// Escape a label value: as [`escape_help`], plus the quote that would
/// close the value early.
fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;

    use super::*;
    use crate::test_db::TestWorkdir;

    /// A database holding one of everything the collector reads, stamped
    /// today so the day-scoped families see it.
    async fn populated() -> (TestWorkdir, SqlitePool) {
        let (workdir, pool) = TestWorkdir::with_db().await;
        let now = chrono::Utc::now().to_rfc3339();

        for (tool, error) in [
            ("wiki_ingest_message", None),
            ("wiki_search", Some("timeout")),
        ] {
            sqlx::query(
                "INSERT INTO tool_executions
                     (timestamp, tool_name, sender_id, device_label, latency_ms, error)
                 VALUES (?, ?, 'alice', 'nanoclaw', 12, ?)",
            )
            .bind(&now)
            .bind(tool)
            .bind(error)
            .execute(&pool)
            .await
            .expect("audit row");
        }

        for (slot, error) in [("ingest", None), ("cronista", Some("refused"))] {
            sqlx::query(
                "INSERT INTO llm_usage
                     (ts, slot, backend, model, kind, billing, source,
                      prompt_tokens, completion_tokens, latency_ms, error)
                 VALUES (?, ?, 'ollama', 'qwen3.5:9b-q8_0', 'chat', 'local', 'serve',
                         100, 40, 250, ?)",
            )
            .bind(&now)
            .bind(slot)
            .bind(error)
            .execute(&pool)
            .await
            .expect("usage row");
        }

        for ms in [10, 30, 90] {
            sqlx::query(
                "INSERT INTO recall_traces (created_at, source, sender_id, payload)
                 VALUES (?, 'ingest', 'alice', ?)",
            )
            .bind(&now)
            .bind(format!(r#"{{"version":1,"recall_ms":{ms}}}"#))
            .execute(&pool)
            .await
            .expect("trace row");
        }

        for (kind, ok, finished) in [
            ("full", 1, "2026-09-07T02:00:00+00:00"),
            ("full", 0, "2026-09-06T02:00:00+00:00"),
            ("light", 1, "2026-09-07T09:00:00+00:00"),
        ] {
            sqlx::query(
                "INSERT INTO dream_runs
                     (kind, trigger_source, ok, summary, log_text, started_at, finished_at)
                 VALUES (?, 'scheduled', ?, 'done', '', ?, ?)",
            )
            .bind(kind)
            .bind(ok)
            .bind(finished)
            .bind(finished)
            .execute(&pool)
            .await
            .expect("dream row");
        }

        (workdir, pool)
    }

    fn reading<'a>(db_path: &'a Path, spend: &'a BudgetState) -> Reading<'a> {
        Reading {
            db_path,
            uptime: Duration::from_secs(3600),
            version: "2.0.0",
            spend: Some(spend),
        }
    }

    fn budget() -> BudgetState {
        BudgetState {
            day: "2026-09-08".to_owned(),
            spent: 2.0,
            unpriced_calls: 0,
            limit: Some(8.0),
            warn_fraction: 0.8,
            unlocked: false,
            currency: Some("EUR".to_owned()),
        }
    }

    /// The roster is what the guide documents and what an operator's
    /// alerts name, so it is not allowed to drift from what the
    /// collector actually emits — in either direction.
    #[tokio::test]
    async fn a_populated_database_publishes_exactly_the_documented_roster() {
        let (workdir, pool) = populated().await;
        let spend = budget();

        let db_path = crate::db::engine_db_path(workdir.path());
        let families = collect(&pool, reading(&db_path, &spend))
            .await
            .expect("collect");
        let emitted: Vec<&str> = families
            .iter()
            .filter(|f| !f.samples.is_empty())
            .map(|f| f.name)
            .collect();

        assert_eq!(emitted, FAMILIES);
    }

    /// The queries run against the real schema, and the numbers are the
    /// rows. `json_extract` on the trace payload is the one that would
    /// fail silently — an empty recall family rather than an error.
    #[tokio::test]
    async fn the_numbers_are_the_rows() {
        let (workdir, pool) = populated().await;
        let spend = budget();

        let db_path = crate::db::engine_db_path(workdir.path());
        let out = render(
            &collect(&pool, reading(&db_path, &spend))
                .await
                .expect("collect"),
        );

        // One turn, two calls, one of them failed — all on one credential.
        assert!(
            out.contains(r#"mwe_turns_today{device="nanoclaw"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"mwe_tool_calls_today{device="nanoclaw"} 2"#),
            "{out}"
        );
        assert!(
            out.contains(r#"mwe_tool_errors_today{device="nanoclaw"} 1"#),
            "{out}"
        );
        // Per slot, with the model that carries the price.
        assert!(
            out.contains(
                r#"mwe_llm_calls_today{slot="ingest",backend="ollama",model="qwen3.5:9b-q8_0"} 1"#
            ),
            "{out}"
        );
        assert!(
            out.contains(r#"mwe_llm_tokens_today{slot="ingest",backend="ollama",model="qwen3.5:9b-q8_0",direction="completion"} 40"#),
            "{out}"
        );
        // Percentiles over the three recorded recalls.
        assert!(out.contains("mwe_recall_ms_p50 30"), "{out}");
        assert!(out.contains("mwe_recall_ms_max 90"), "{out}");
        // The newest full run succeeded, and the older failed one is
        // still in the outcome counts.
        assert!(
            out.contains(r#"mwe_dream_last_run_ok{kind="full"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"mwe_dream_runs{kind="full",ok="false"} 1"#),
            "{out}"
        );
        // The size includes the write-ahead log, so it is never zero on
        // a database that has been written to.
        assert!(
            out.lines()
                .any(|l| l.starts_with("mwe_database_bytes ") && l != "mwe_database_bytes 0"),
            "{out}"
        );
    }

    /// An installation nobody has used yet still answers: the process
    /// families and the budget are there, and the families with nothing
    /// behind them are absent rather than a page of zeroes.
    #[tokio::test]
    async fn an_empty_database_answers_without_the_day_families() {
        let (workdir, pool) = TestWorkdir::with_db().await;
        let spend = budget();

        let db_path = crate::db::engine_db_path(workdir.path());
        let families = collect(&pool, reading(&db_path, &spend))
            .await
            .expect("collect");
        let out = render(&families);

        assert!(out.contains("mwe_uptime_seconds 3600"), "{out}");
        assert!(
            out.contains(r#"mwe_build_info{version="2.0.0"} 1"#),
            "{out}"
        );
        // Nothing has happened yet, so the families with nothing to say
        // are absent rather than a page of zeroes claiming otherwise.
        assert!(!out.contains("mwe_turns_today"), "{out}");
        assert!(!out.contains("mwe_recall_ms_p50"), "{out}");
        assert!(!out.contains("mwe_dream_runs"), "{out}");
        // The budget is set whether or not anything has been spent.
        assert!(out.contains("mwe_spend_limit 8"), "{out}");
    }

    #[test]
    fn render_writes_one_header_pair_per_family() {
        let out = render(&[Family::new(
            "mwe_thing",
            Kind::Counter,
            "A thing.",
            vec![
                Sample::labelled(&[("a", "1")], 2.0),
                Sample::labelled(&[("a", "2")], 3.0),
            ],
        )]);

        assert_eq!(
            out,
            "# HELP mwe_thing A thing.\n# TYPE mwe_thing counter\nmwe_thing{a=\"1\"} 2\nmwe_thing{a=\"2\"} 3\n"
        );
    }

    #[test]
    fn render_drops_a_family_with_no_samples() {
        let out = render(&[Family::new("mwe_empty", Kind::Gauge, "Nothing.", vec![])]);
        assert!(out.is_empty(), "{out}");
    }

    #[test]
    fn a_label_value_cannot_break_out_of_its_quotes() {
        let out = render(&[Family::new(
            "mwe_thing",
            Kind::Gauge,
            "A thing.",
            vec![Sample::labelled(&[("device", "a\"b\\c\nd")], 1.0)],
        )]);

        assert!(
            out.contains(r#"device="a\"b\\c\nd""#),
            "quote, backslash and newline are all escaped: {out}"
        );
        assert_eq!(out.lines().count(), 3, "the value stays on one line: {out}");
    }

    /// A budget an operator typed can be a number Rust prints as `inf`,
    /// and Prometheus fails the parse of the whole document on one line
    /// it cannot read — taking every other metric with it.
    #[test]
    fn a_non_finite_value_is_spelled_the_way_prometheus_reads_it() {
        let out = render(&[Family::new(
            "mwe_thing",
            Kind::Gauge,
            "A thing.",
            vec![
                Sample::bare(f64::INFINITY),
                Sample::bare(f64::NEG_INFINITY),
                Sample::bare(f64::NAN),
            ],
        )]);

        assert!(out.contains("mwe_thing +Inf"), "{out}");
        assert!(out.contains("mwe_thing -Inf"), "{out}");
        assert!(out.contains("mwe_thing NaN"), "{out}");
        assert!(!out.contains("inf\n"), "Rust's spelling escaped: {out}");
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let sample: Vec<i64> = (1..=10).collect();

        assert!((percentile(&sample, 0.50) - 5.0).abs() < f64::EPSILON);
        assert!((percentile(&sample, 0.90) - 9.0).abs() < f64::EPSILON);
        assert!((percentile(&sample, 1.0) - 10.0).abs() < f64::EPSILON);
        // A single-value sample answers that value at every rank rather
        // than indexing past its end.
        assert!((percentile(&[7], 0.50) - 7.0).abs() < f64::EPSILON);
    }

    /// No budget set is not a budget of zero: the ceiling series is
    /// absent, so an alert on "spend against limit" has nothing to fire
    /// against instead of firing on every call.
    #[test]
    fn a_deployment_with_no_budget_publishes_no_limit() {
        let state = BudgetState {
            day: "2026-09-08".to_owned(),
            spent: 1.5,
            unpriced_calls: 0,
            limit: None,
            warn_fraction: 0.8,
            unlocked: false,
            currency: Some("EUR".to_owned()),
        };
        let out = render(&spend(Some(&state)));

        assert!(out.contains("mwe_spend_today 1.5"), "{out}");
        assert!(!out.contains("mwe_spend_limit"), "{out}");
        assert!(out.contains("mwe_spend_stopped 0"), "{out}");
    }

    #[test]
    fn a_spent_budget_reads_as_stopped() {
        let state = BudgetState {
            day: "2026-09-08".to_owned(),
            spent: 4.0,
            unpriced_calls: 3,
            limit: Some(4.0),
            warn_fraction: 0.8,
            unlocked: false,
            currency: Some("EUR".to_owned()),
        };
        let out = render(&spend(Some(&state)));

        assert!(out.contains("mwe_spend_limit 4"), "{out}");
        assert!(out.contains("mwe_spend_stopped 1"), "{out}");
        assert!(out.contains("mwe_spend_unpriced_calls_today 3"), "{out}");
    }
}
