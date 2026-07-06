// SPDX-License-Identifier: AGPL-3.0-or-later
//! Live diagnostics shared by the CLI `doctor` and the dashboard health
//! page (roadmap [group 19](../../../docs/development/build-run.md)).
//!
//! These are the **read-only** health checks that can run while
//! `mwe-mcp serve` holds the workdir lockfile: DB reachability +
//! migration/table counts, WAL recovery backlog, the token blacklist
//! size, the workdir permission audit, and per-slot LLM reachability.
//! They take no lockfile and mutate nothing, so the dashboard can surface
//! them against the live pool without contending with the running server.
//!
//! The lockfile-taking and env-only checks the offline CLI `doctor`
//! performs for "serve won't start" triage (lockfile acquisition, the
//! `MWE_TOKEN_SECRET`-from-env probe, the JWT self-test) deliberately
//! stay in the CLI — an in-server endpoint cannot serve them.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::config::{LlmConfig, LlmFunction};
use crate::llm::LlmBackend;
use crate::workdir_security::{self, PermFinding};
use crate::{oauth, wal};

/// Counts + findings collected from the engine DB and the workdir. All
/// fields are snapshots taken at call time; nothing here is cached.
#[derive(Debug)]
pub struct DbDiagnostics {
    /// Application tables (excludes `sqlite_*` and `_sqlx_*`).
    pub app_tables: i64,
    /// Rows in `_sqlx_migrations` — every embedded migration applied.
    pub applied_migrations: i64,
    /// Stale proposal ops awaiting WAL recovery (older than
    /// [`wal::DEFAULT_STALE_AFTER`]).
    pub stale_proposal_ops: usize,
    /// Stale REM ops awaiting WAL recovery.
    pub stale_rem_ops: usize,
    /// Entries in `token_blacklist`.
    pub token_blacklist_entries: i64,
    /// Workdir paths reachable by non-owner principals. Empty is healthy
    /// (the per-reader ACL holds only while the cleartext bytes are
    /// owner-only).
    pub perm_findings: Vec<PermFinding>,
}

impl DbDiagnostics {
    /// Remediation hint for a non-empty [`Self::perm_findings`].
    #[must_use]
    pub fn perm_remediation(workdir: &Path) -> String {
        workdir_security::remediation(workdir)
    }
}

/// Collect the DB + workdir checks against a live pool.
///
/// Read-only: counts rows and audits filesystem permissions. Safe to call
/// while `serve` holds the workdir lockfile.
///
/// # Errors
///
/// Propagates any DB error from the underlying count / scan queries.
pub async fn collect_db(pool: &SqlitePool, workdir: &Path) -> Result<DbDiagnostics> {
    let app_tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' \
         AND name NOT LIKE '_sqlx_%'",
    )
    .fetch_one(pool)
    .await
    .context("counting application tables")?;

    let applied_migrations: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .context("counting applied migrations")?;

    let stale_proposal_ops = wal::scan_stale_proposal_ops(pool, wal::DEFAULT_STALE_AFTER)
        .await
        .context("scanning stale proposal ops")?
        .len();
    let stale_rem_ops = wal::scan_stale_rem_ops(pool, wal::DEFAULT_STALE_AFTER)
        .await
        .context("scanning stale REM ops")?
        .len();

    let token_blacklist_entries: i64 = sqlx::query_scalar("SELECT count(*) FROM token_blacklist")
        .fetch_one(pool)
        .await
        .context("counting token_blacklist entries")?;

    let perm_findings = workdir_security::audit(workdir);

    Ok(DbDiagnostics {
        app_tables,
        applied_migrations,
        stale_proposal_ops,
        stale_rem_ops,
        token_blacklist_entries,
        perm_findings,
    })
}

/// Reachability verdict for one LLM slot.
#[derive(Debug)]
pub enum SlotStatus {
    /// The slot's backend answered its health check.
    Reachable {
        /// Backend tag (`ollama` | `anthropic` | …).
        backend: String,
        /// Configured model id.
        model: String,
    },
    /// The slot is absent from `mwe-mcp.config.yaml > llm` — that feature
    /// is intentionally off, not broken.
    Unconfigured,
    /// A Claude Code login slot that has not been authenticated yet. Boot
    /// continues; the slot's feature is unavailable until the operator
    /// logs in from the dashboard. Test/personal use only (see [`oauth`]).
    LoginPending,
    /// The backend could not be built or did not pass its health check.
    Failed(String),
}

/// Per-slot reachability, in display order.
#[derive(Debug)]
pub struct SlotHealth {
    /// `llm:` YAML key for the slot.
    pub slot: &'static str,
    /// The verdict.
    pub status: SlotStatus,
}

/// The canonical LLM slots, in display order — mirrors the boot
/// health-check roster.
#[allow(
    deprecated,
    reason = "Cronista is deprecated but a legacy YAML may still wire it; probe it like any other slot."
)]
pub const LLM_FUNCTIONS: [LlmFunction; 7] = [
    LlmFunction::HubWriter,
    LlmFunction::Ingest,
    LlmFunction::OperatorChat,
    LlmFunction::RemPromotions,
    LlmFunction::RemDedupSemantic,
    LlmFunction::Cronista,
    LlmFunction::Navigator,
];

/// Probe every canonical LLM slot for reachability, non-fatally.
///
/// Unconfigured slots are reported (not skipped silently); a Claude Code
/// login slot awaiting authentication is reported as [`SlotStatus::LoginPending`]
/// rather than dialled (it would fail until the dashboard login lands).
/// For every other configured slot the `build` closure materialises the
/// backend (the CLI builds from disk config; the dashboard builds via its
/// live handles so dashboard-set API keys are honoured) and its
/// `health_check` is awaited.
///
/// Unlike the boot gate this never short-circuits — every slot is probed
/// so the operator sees the full picture. Use [`slots_failed`] to decide
/// whether to refuse to boot.
pub async fn probe_llm_slots<F>(config: &LlmConfig, mut build: F) -> Vec<SlotHealth>
where
    F: FnMut(LlmFunction) -> Result<Arc<dyn LlmBackend>>,
{
    let mut out = Vec::with_capacity(LLM_FUNCTIONS.len());
    for func in LLM_FUNCTIONS {
        let Some(slot) = config.slot(func) else {
            out.push(SlotHealth {
                slot: func.yaml_key(),
                status: SlotStatus::Unconfigured,
            });
            continue;
        };

        if slot.api_key_env.as_deref() == Some(oauth::CLAUDE_CODE_LOGIN)
            && !oauth::global_store().is_some_and(|s| s.is_logged_in())
        {
            out.push(SlotHealth {
                slot: func.yaml_key(),
                status: SlotStatus::LoginPending,
            });
            continue;
        }

        let status = match build(func) {
            Ok(backend) => match backend.health_check().await {
                Ok(()) => SlotStatus::Reachable {
                    backend: slot.backend.clone(),
                    model: slot.model.clone(),
                },
                Err(e) => SlotStatus::Failed(format!("{e:#}")),
            },
            Err(e) => SlotStatus::Failed(format!("build: {e:#}")),
        };
        out.push(SlotHealth {
            slot: func.yaml_key(),
            status,
        });
    }
    out
}

/// The slots whose status is [`SlotStatus::Failed`] — the boot gate
/// refuses to bind when this is non-empty.
#[must_use]
pub fn slots_failed(report: &[SlotHealth]) -> Vec<&SlotHealth> {
    report
        .iter()
        .filter(|s| matches!(s.status, SlotStatus::Failed(_)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn make_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrate");
        pool
    }

    #[tokio::test]
    async fn collect_db_reports_a_clean_fresh_database() {
        let pool = make_pool().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let d = collect_db(&pool, dir.path()).await.expect("collect_db");

        assert!(d.app_tables > 0, "a migrated DB has application tables");
        assert!(
            d.applied_migrations > 0,
            "every embedded migration is recorded in _sqlx_migrations"
        );
        assert_eq!(d.stale_proposal_ops, 0, "fresh DB has no stale ops");
        assert_eq!(d.stale_rem_ops, 0);
        assert_eq!(d.token_blacklist_entries, 0);
    }

    #[tokio::test]
    async fn probe_reports_unconfigured_for_an_empty_config() {
        let cfg = LlmConfig::default();
        // The build closure must never fire: every slot is unconfigured.
        let report = probe_llm_slots(&cfg, |_| {
            panic!("build closure called for an unconfigured slot")
        })
        .await;

        assert_eq!(report.len(), LLM_FUNCTIONS.len());
        assert!(
            report
                .iter()
                .all(|s| matches!(s.status, SlotStatus::Unconfigured)),
            "an empty config yields all-unconfigured"
        );
        assert!(slots_failed(&report).is_empty());
    }
}
