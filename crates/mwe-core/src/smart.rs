// SPDX-License-Identifier: AGPL-3.0-or-later
//! Smart-consumer bootstrap helpers — family K of the MCP surface.
//!
//! These are *thin orchestration primitives* over read-only APIs that
//! already exist in [`crate::recall`], [`crate::briefing`], and
//! [`crate::wiki`]. They exist as standalone tools so the Claude Code
//! hook bundle (and equivalents for other smart consumers) can fire
//! them deterministically from the agent harness without each consumer
//! re-implementing the call shape documented in the bundled skills.
//!
//! Two entry points:
//!
//! - [`bootstrap`] — used by the `SessionStart` hook. Surfaces every
//!   smart-family wiki the caller owns plus its pending briefing
//!   inbox, sorted so the most-recently-touched wiki floats up. Caller
//!   may pass `project_hint` to bias the ranking toward a particular
//!   `project_id` / slug / title.
//! - [`recall_core_global`] — used by the `UserPromptSubmit` hook.
//!   Wraps [`crate::recall::wiki_search`] with the canonical
//!   "transversal recall" filter documented in the bundled skill
//!   `core-globalmemory.md`: scope to the caller's own
//!   `acl_default = user:<sender>` wikis **and** exclude the
//!   `the smart family` set so project-bound memory does not leak into
//!   unrelated work.
//!
//! Both gated on `consumer_class=smart` — these tools have no business
//! on a standard/conversational token.
//!
//! See [`tool-reference.md`](../../../docs/protocol/tool-reference.md)
//! for the wire shape + error mapping.

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::SqlitePool;

use crate::briefing::{self, BriefingError, BriefingKind, BriefingKindCounts, ListItemsFilter};
use crate::embedder::Embedder;
use crate::fact_index::FactFilters;
use crate::recall::{self, RecallError, SenderContext};
use crate::types::{Principal, WikiId};
use crate::wiki::{WikiError, WikiTree};
use crate::wiki_admin::AdminCaller;

/// Default cap on briefing rows surfaced per wiki by [`bootstrap`].
pub const DEFAULT_BOOTSTRAP_BRIEFING_LIMIT: usize = 5;
/// Hard cap on briefing rows surfaced per wiki by [`bootstrap`].
pub const MAX_BOOTSTRAP_BRIEFING_LIMIT: usize = 50;
/// Default `top_k` for [`recall_core_global`]. Matches the skill body
/// recommendation "tight result limit 5-8 hits".
pub const DEFAULT_RECALL_LIMIT: usize = 8;
/// Hard cap on `top_k` for [`recall_core_global`]. Above this, the
/// distillate the smart consumer renders becomes too long for a
/// pre-prompt context budget — the skill is explicit about this.
pub const MAX_RECALL_LIMIT: usize = 20;

/// Errors surfaced by the smart-helper functions in this module.
#[derive(Debug, thiserror::Error)]
pub enum SmartError {
    /// Caller's token does not have `consumer_class=smart`.
    #[error("requires consumer_class=smart")]
    RequiresSmart,
    /// Caller-side validation failure (empty query, …).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Bubbled from the wiki tree walk / locate.
    #[error(transparent)]
    Wiki(#[from] WikiError),
    /// Bubbled from [`recall::wiki_search`].
    #[error(transparent)]
    Recall(#[from] RecallError),
    /// Bubbled from briefing helpers.
    #[error(transparent)]
    Briefing(#[from] BriefingError),
    /// Bubbled from briefing list helpers / direct SQL probes.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

// ---------- bootstrap ----------

/// Input to [`bootstrap`]. All fields optional — the hook fires with
/// `{}` and the function still does useful work using the caller's
/// JWT claims alone.
#[derive(Debug, Clone, Default)]
pub struct BootstrapRequest {
    /// Substring matched (case-insensitive) against each candidate
    /// wiki's `_meta.md.extra.project_id`, slug, and title. Matches
    /// float to the top of the response; non-matches are still
    /// returned, in last-activity order. Empty or whitespace = no
    /// hint.
    pub project_hint: Option<String>,
    /// Per-wiki cap on `recent_briefing` rows. Clamped to
    /// [`MAX_BOOTSTRAP_BRIEFING_LIMIT`]. Default
    /// [`DEFAULT_BOOTSTRAP_BRIEFING_LIMIT`].
    pub briefing_limit_per_wiki: Option<usize>,
}

/// Per-wiki snippet returned by [`bootstrap`].
#[derive(Debug, Clone)]
pub struct SmartWikiSummary {
    /// Stable id of the wiki.
    pub wiki_id: WikiId,
    /// Bare `wiki_type` stem (`wiki-companion`,
    /// `wiki-companion-<slug>`, …).
    pub wiki_type: String,
    /// Display title.
    pub title: String,
    /// On-disk slug.
    pub slug: String,
    /// `_meta.md.extra.project_id` value if set (smart-wikis born
    /// from `wiki_admin_push mode=create` with a `project_id` arg).
    pub project_id: Option<String>,
    /// Bucketed counts of `wiki_briefing_items` rows.
    pub briefing_counts: BriefingKindCounts,
    /// Pending (`processed_at IS NULL`) briefing items, freshest
    /// first, capped at the request's `briefing_limit_per_wiki`.
    pub recent_briefing: Vec<BriefingItemSummary>,
    /// Latest `wiki_admin_op_log.op_id` for this wiki, or `None` when
    /// the wiki has never been pushed (just-forged via dashboard).
    pub last_op_log_id: Option<i64>,
    /// `ts` of the latest `wiki_admin_op_log` row, for caller-side
    /// "last touched" rendering.
    pub last_op_log_ts: Option<String>,
    /// `true` when [`BootstrapRequest::project_hint`] is set and
    /// matches this wiki (case-insensitive substring on `project_id`,
    /// slug, or title).
    pub matches_project_hint: bool,
    /// `true` when this wiki is the **caller's own operational wiki** — the
    /// dedicated wiki forged at consent for this connection, whose slug equals
    /// the caller's `consumer_id`. Lets a freshly-connected consumer identify
    /// "its own" wiki directly instead of deducing it, when the user owns more
    /// than one agent wiki.
    pub is_self: bool,
}

/// A row of [`SmartWikiSummary::recent_briefing`].
#[derive(Debug, Clone)]
pub struct BriefingItemSummary {
    /// Opaque `bi_<N>` id.
    pub briefing_item_id: String,
    /// Three-layer classification. `None` for legacy rows.
    pub kind: Option<BriefingKind>,
    /// Trimmed topic line.
    pub topic: String,
    /// Trimmed body.
    pub body: String,
    /// Stable cite handle (`wiki://…#…`) when present.
    pub target_cite: Option<String>,
    /// ISO-8601 wall-clock write time.
    pub ts: String,
}

/// Response of [`bootstrap`].
#[derive(Debug, Clone)]
pub struct BootstrapResponse {
    /// Echoes the caller's `sender_id` — useful for the smart consumer
    /// to log alongside the bootstrap row.
    pub caller_sender_id: String,
    /// Echoes the request hint, if any. Trimmed of leading/trailing
    /// whitespace; preserved verbatim otherwise.
    pub project_hint: Option<String>,
    /// One entry per smart-family wiki owned by the caller, sorted:
    /// (1) hint matches first, (2) most-recent `wiki_admin_op_log`
    /// activity next, (3) `wiki_id` alphabetical as stable tie-break.
    pub smart_wikis: Vec<SmartWikiSummary>,
}

/// Wrap up the smart-consumer's session-start landscape.
///
/// The hook fires once per session. The smart consumer reads the
/// returned summary, picks the wiki whose `matches_project_hint` (or
/// `last_op_log_ts`) makes it the current project, surfaces unread
/// briefing items to the user, and proceeds with the usual editing
/// loop documented in the `smart-consumer.md` skill.
///
/// # Errors
///
/// [`SmartError::RequiresSmart`] when the caller's token is not
/// `consumer_class=smart`. Other variants bubble underlying read
/// failures.
pub async fn bootstrap(
    pool: &SqlitePool,
    tree: &WikiTree,
    caller: &AdminCaller,
    req: BootstrapRequest,
) -> Result<BootstrapResponse, SmartError> {
    if !caller.consumer_class.is_smart() {
        return Err(SmartError::RequiresSmart);
    }

    let briefing_limit = req
        .briefing_limit_per_wiki
        .unwrap_or(DEFAULT_BOOTSTRAP_BRIEFING_LIMIT)
        .min(MAX_BOOTSTRAP_BRIEFING_LIMIT);

    let hint_lc = req
        .project_hint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);

    let caller_principal = Principal::User(caller.sender_id.clone());
    let mut summaries: Vec<SmartWikiSummary> = Vec::new();

    for d in tree.walk()? {
        // Smart family = the per-wiki `_meta.md` flag.
        if !d.meta.smart {
            continue;
        }
        let owner = tree.resolve_scope_principal(&d.meta)?;
        if owner != caller_principal {
            continue;
        }

        let counts = briefing::counts_by_kind(pool, &d.meta.wiki_id).await?;
        let items = briefing::list_items(
            pool,
            &d.meta.wiki_id,
            &ListItemsFilter {
                kind: None,
                pending_only: Some(true),
                limit: Some(i64::try_from(briefing_limit).unwrap_or(i64::MAX)),
            },
        )
        .await?;
        let recent_briefing: Vec<BriefingItemSummary> = items
            .into_iter()
            .map(|bi| BriefingItemSummary {
                briefing_item_id: bi.briefing_item_id,
                kind: bi.kind,
                topic: bi.topic,
                body: bi.body,
                target_cite: bi.target_cite,
                ts: bi.ts,
            })
            .collect();

        let last_op: Option<(i64, String)> = sqlx::query_as(
            "SELECT op_id, ts FROM wiki_admin_op_log WHERE wiki_id = ? \
                 ORDER BY ts DESC, op_id DESC LIMIT 1",
        )
        .bind(d.meta.wiki_id.as_str())
        .fetch_optional(pool)
        .await?;

        let project_id = d
            .meta
            .extra
            .get(serde_yaml::Value::String("project_id".to_owned()))
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let matches_project_hint = hint_lc.as_deref().is_some_and(|hint| {
            let haystack = format!(
                "{}|{}|{}",
                project_id.as_deref().unwrap_or(""),
                d.meta.slug.as_str(),
                d.meta.title,
            )
            .to_lowercase();
            haystack.contains(hint)
        });

        // Own operational wiki = its slug equals the caller's `consumer_id`.
        let is_self = caller.consumer_id.as_deref() == Some(d.meta.slug.as_str());

        summaries.push(SmartWikiSummary {
            wiki_id: d.meta.wiki_id.clone(),
            wiki_type: d.meta.wiki_type.clone(),
            title: d.meta.title.clone(),
            slug: d.meta.slug.as_str().to_owned(),
            project_id,
            briefing_counts: counts,
            recent_briefing,
            last_op_log_id: last_op.as_ref().map(|(id, _)| *id),
            last_op_log_ts: last_op.map(|(_, ts)| ts),
            matches_project_hint,
            is_self,
        });
    }

    summaries.sort_by(|a, b| {
        b.matches_project_hint
            .cmp(&a.matches_project_hint)
            .then_with(|| b.is_self.cmp(&a.is_self))
            .then_with(|| b.last_op_log_ts.cmp(&a.last_op_log_ts))
            .then_with(|| a.wiki_id.as_str().cmp(b.wiki_id.as_str()))
    });

    Ok(BootstrapResponse {
        caller_sender_id: caller.sender_id.clone(),
        project_hint: req
            .project_hint
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
        smart_wikis: summaries,
    })
}

// ---------- recall_core_global ----------

/// Input to [`recall_core_global`].
#[derive(Debug, Clone)]
pub struct RecallCoreGlobalRequest {
    /// Free-form query string. Trimmed; empty after trim → error.
    pub query: String,
    /// Cap on hits returned. `None` ⇒ [`DEFAULT_RECALL_LIMIT`].
    /// Clamped to `[1, MAX_RECALL_LIMIT]`.
    pub limit: Option<usize>,
}

/// Echoes the filter that was applied — surfaced so the smart consumer
/// can include it verbatim in audit logs / a forked-subagent distillate.
#[derive(Debug, Clone)]
pub struct RecallCoreGlobalFilter {
    /// `caller.sender_id` — every hit is scoped to wikis whose derived
    /// scope principal is this user.
    pub owner_user: String,
    /// Companion-family `wiki_type` stems excluded from this search.
    /// Returned for diagnostic clarity; collected at query time from the
    /// per-wiki `_meta.md` smart flag of every wiki on disk (derived
    /// from the on-disk flag, not the `wiki_types_registry` lookup).
    pub excluded_wiki_types: Vec<String>,
}

/// A row of [`RecallCoreGlobalResponse::hits`].
#[derive(Debug, Clone)]
pub struct RecallCoreGlobalHit {
    /// Wiki containing the fact.
    pub wiki_id: String,
    /// Bare `wiki_type` stem of the parent wiki (post-filter; always
    /// non-smart).
    pub wiki_type: String,
    /// Stable fact id.
    pub fact_id: String,
    /// Recall snippet excerpt.
    pub snippet: String,
    /// Cosine similarity score in `[0.0, 1.0]`.
    pub score: f32,
}

/// Response of [`recall_core_global`].
#[derive(Debug, Clone)]
pub struct RecallCoreGlobalResponse {
    /// Echoes the trimmed query.
    pub query: String,
    /// Applied filter, for caller-side logging.
    pub filter_applied: RecallCoreGlobalFilter,
    /// Hits in cosine-descending order, capped at `limit`.
    pub hits: Vec<RecallCoreGlobalHit>,
}

/// Canonical "transversal recall" wrapper — see module docstring + the
/// bundled skill `core-globalmemory.md`.
///
/// # Errors
///
/// [`SmartError::RequiresSmart`] when the token is not smart-class;
/// [`SmartError::InvalidInput`] when the query is empty after trim;
/// other variants bubble underlying failures.
pub async fn recall_core_global(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    caller: &AdminCaller,
    sender_groups: Vec<String>,
    req: RecallCoreGlobalRequest,
) -> Result<RecallCoreGlobalResponse, SmartError> {
    if !caller.consumer_class.is_smart() {
        return Err(SmartError::RequiresSmart);
    }
    let query = req.query.trim().to_owned();
    if query.is_empty() {
        return Err(SmartError::InvalidInput("query is empty".to_owned()));
    }
    let limit = req
        .limit
        .unwrap_or(DEFAULT_RECALL_LIMIT)
        .clamp(1, MAX_RECALL_LIMIT);

    let sender = SenderContext {
        sender_id: caller.sender_id.clone(),
        sender_groups,
    };
    let filters = FactFilters {
        owner_id: Some(Principal::User(caller.sender_id.clone())),
        ..Default::default()
    };
    // The fact corpus only. Smart-wiki documentation lives in its own
    // table and is simply not queried here — no overfetch, no post-filter
    // to shrink the caller's `limit`. This view is transversal personal
    // memory; project docs would drown it.
    let raw = recall::wiki_search(pool, embedder, &query, limit, filters, &sender).await?;

    // The smart-family `wiki_type`s this view excludes, echoed in
    // `filter_applied` regardless of how many hits there are — the policy
    // statement "smart-wiki material is not in this view".
    let mut excluded_types: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for d in tree.walk()? {
        if d.meta.smart {
            excluded_types.insert(d.meta.wiki_type.clone());
        }
    }

    let mut meta_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut hits: Vec<RecallCoreGlobalHit> = Vec::with_capacity(limit);

    for h in raw {
        if hits.len() == limit {
            break;
        }
        let resolved = if let Some(v) = meta_cache.get(&h.wiki_id) {
            v.clone()
        } else {
            let r = WikiId::parse(&h.wiki_id)
                .ok()
                .and_then(|id| tree.locate(&id).ok().map(|hh| hh.meta().wiki_type.clone()));
            meta_cache.insert(h.wiki_id.clone(), r.clone());
            r
        };
        let Some(wt) = resolved else {
            continue;
        };
        hits.push(RecallCoreGlobalHit {
            wiki_id: h.wiki_id,
            wiki_type: wt,
            fact_id: h.fact_id.to_string(),
            snippet: h.text,
            score: h.score,
        });
    }

    Ok(RecallCoreGlobalResponse {
        query,
        filter_applied: RecallCoreGlobalFilter {
            owner_user: caller.sender_id.clone(),
            // The smart-family `wiki_type`s excluded from this view, sorted
            // by the BTreeSet (derived from `_meta.md`).
            excluded_wiki_types: excluded_types.into_iter().collect(),
        },
        hits,
    })
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::tempdir;

    use crate::briefing::{BriefingKind, BriefingSourceKind, NotifyCaller, NotifyRequest};
    use crate::embedder::FakeEmbedder;
    use crate::jwt::ConsumerClass;
    use crate::wiki::{IdentityKind, create_identity_wiki};
    use crate::wiki_admin::{self, ActorKind, PushMode, PushPage, PushRequest};

    fn embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::new("smart-test", 4))
    }

    async fn make_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool
    }

    async fn seeded_tree() -> (tempfile::TempDir, WikiTree, SqlitePool) {
        let dir = tempdir().expect("tempdir");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        // Identity wikis for alice + bob so the smart-wiki parent
        // resolution has somewhere to land.
        let alice = WikiId::parse("alice").expect("alice id");
        create_identity_wiki(&tree, &alice, "Alice", IdentityKind::User).expect("alice identity");
        let bob = WikiId::parse("bob").expect("bob id");
        create_identity_wiki(&tree, &bob, "Bob", IdentityKind::User).expect("bob identity");
        (dir, tree, pool)
    }

    fn alice_smart() -> AdminCaller {
        AdminCaller {
            sender_id: "alice".to_owned(),
            consumer_id: Some("cc-laptop".to_owned()),
            consumer_class: ConsumerClass::Smart,
        }
    }

    fn alice_standard() -> AdminCaller {
        AdminCaller {
            sender_id: "alice".to_owned(),
            consumer_id: None,
            consumer_class: ConsumerClass::Standard,
        }
    }

    fn bob_smart() -> AdminCaller {
        AdminCaller {
            sender_id: "bob".to_owned(),
            consumer_id: Some("cc-laptop".to_owned()),
            consumer_class: ConsumerClass::Smart,
        }
    }

    /// Push a fresh smart wiki under the caller's identity tree and
    /// return its `wiki_id`. Uses the canonical `wiki_admin::push`
    /// `Create` flow so the on-disk shape + `op_log` row are exactly
    /// what production writes.
    async fn create_smart_wiki_for(
        pool: &SqlitePool,
        tree: &WikiTree,
        caller: &AdminCaller,
        slug: &str,
        title: &str,
        project_id: Option<&str>,
    ) -> WikiId {
        let parent = WikiId::parse(&caller.sender_id).expect("parent id");
        let resp = wiki_admin::push(
            pool,
            tree,
            caller,
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Create,
                wiki_id: None,
                parent_wiki_id: Some(parent),
                slug: Some(slug.to_owned()),
                title: Some(title.to_owned()),
                wiki_type: Some("wiki-companion".to_owned()),
                smart: true,
                project_id: project_id.map(str::to_owned),
                pages: vec![PushPage {
                    path: "index.md".to_owned(),
                    content: format!("# {title}\n"),
                }],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("create smart wiki");
        resp.wiki_id
    }

    /// Bump the latest `op_log` row for `wiki_id` to `ts`. The smart-wiki
    /// creation above already stamps one row; this helper rewrites its
    /// timestamp so the test can pin ordering deterministically.
    async fn rewrite_last_op_log_ts(pool: &SqlitePool, wiki_id: &WikiId, ts: &str) {
        sqlx::query(
            "UPDATE wiki_admin_op_log SET ts = ? \
             WHERE op_id = (SELECT MAX(op_id) FROM wiki_admin_op_log WHERE wiki_id = ?)",
        )
        .bind(ts)
        .bind(wiki_id.as_str())
        .execute(pool)
        .await
        .expect("update op_log ts");
    }

    #[tokio::test]
    async fn bootstrap_rejects_standard_consumer() {
        let (_dir, tree, pool) = seeded_tree().await;
        let err = bootstrap(&pool, &tree, &alice_standard(), BootstrapRequest::default())
            .await
            .expect_err("standard rejected");
        assert!(matches!(err, SmartError::RequiresSmart));
    }

    #[tokio::test]
    async fn bootstrap_returns_only_caller_owned_smart_wikis() {
        let (_dir, tree, pool) = seeded_tree().await;
        let alice_a = create_smart_wiki_for(
            &pool,
            &tree,
            &alice_smart(),
            "acme",
            "Acme",
            Some("acme-monorepo"),
        )
        .await;
        let alice_b =
            create_smart_wiki_for(&pool, &tree, &alice_smart(), "widget", "Widget", None).await;
        let bob_x = create_smart_wiki_for(&pool, &tree, &bob_smart(), "side", "Side", None).await;
        let resp = bootstrap(&pool, &tree, &alice_smart(), BootstrapRequest::default())
            .await
            .expect("bootstrap");
        let ids: Vec<String> = resp
            .smart_wikis
            .iter()
            .map(|c| c.wiki_id.as_str().to_owned())
            .collect();
        assert!(ids.contains(&alice_a.as_str().to_owned()));
        assert!(ids.contains(&alice_b.as_str().to_owned()));
        assert!(!ids.contains(&bob_x.as_str().to_owned()));
    }

    #[tokio::test]
    async fn bootstrap_sorts_by_hint_match_then_last_op_log() {
        let (_dir, tree, pool) = seeded_tree().await;
        let acme = create_smart_wiki_for(
            &pool,
            &tree,
            &alice_smart(),
            "acme",
            "Acme",
            Some("acme-monorepo"),
        )
        .await;
        let widget =
            create_smart_wiki_for(&pool, &tree, &alice_smart(), "widget", "Widget Pro", None).await;
        rewrite_last_op_log_ts(&pool, &acme, "2026-05-25T09:00:00Z").await;
        rewrite_last_op_log_ts(&pool, &widget, "2026-05-26T09:00:00Z").await;
        let resp = bootstrap(&pool, &tree, &alice_smart(), BootstrapRequest::default())
            .await
            .expect("bootstrap");
        assert_eq!(resp.smart_wikis[0].wiki_id, widget);
        let resp_hint = bootstrap(
            &pool,
            &tree,
            &alice_smart(),
            BootstrapRequest {
                project_hint: Some("acme".to_owned()),
                briefing_limit_per_wiki: None,
            },
        )
        .await
        .expect("bootstrap hint");
        assert_eq!(resp_hint.smart_wikis[0].wiki_id, acme);
        assert!(resp_hint.smart_wikis[0].matches_project_hint);
    }

    #[tokio::test]
    async fn bootstrap_marks_caller_operational_wiki_is_self() {
        let (_dir, tree, pool) = seeded_tree().await;
        // alice_smart()'s consumer_id is "cc-laptop"; its operational wiki has that slug.
        let own =
            create_smart_wiki_for(&pool, &tree, &alice_smart(), "cc-laptop", "CC Laptop", None)
                .await;
        let other =
            create_smart_wiki_for(&pool, &tree, &alice_smart(), "acme", "Acme", Some("acme")).await;
        let resp = bootstrap(&pool, &tree, &alice_smart(), BootstrapRequest::default())
            .await
            .expect("bootstrap");
        let own_summary = resp
            .smart_wikis
            .iter()
            .find(|c| c.wiki_id == own)
            .expect("own wiki present");
        let other_summary = resp
            .smart_wikis
            .iter()
            .find(|c| c.wiki_id == other)
            .expect("other wiki present");
        assert!(
            own_summary.is_self,
            "slug == consumer_id ⇒ the caller's own operational wiki"
        );
        assert!(!other_summary.is_self, "a different wiki is not is_self");
        // No project hint ⇒ the caller's own operational wiki floats to the top.
        assert_eq!(resp.smart_wikis[0].wiki_id, own);
    }

    #[tokio::test]
    async fn bootstrap_surfaces_pending_briefing_items() {
        let (_dir, tree, pool) = seeded_tree().await;
        let wiki = create_smart_wiki_for(&pool, &tree, &alice_smart(), "acme", "Acme", None).await;
        // Standard consumer relaying onto a smart-wiki — the
        // canonical openclaw-style use case, and the only matrix cell
        // that writes both the DB row and `_briefing.md`.
        let notify_caller = NotifyCaller {
            sender_id: "alice".to_owned(),
            consumer_class: crate::jwt::ConsumerClass::Standard,
        };
        briefing::notify(
            &pool,
            &tree,
            &notify_caller,
            NotifyRequest {
                wiki_id: wiki.clone(),
                topic: "first".to_owned(),
                body: "first body".to_owned(),
                source_kind: BriefingSourceKind::Consumer,
                source_ref: "test:notify".to_owned(),
                kind: Some(BriefingKind::Observation.as_str().to_owned()),
                target_cite: None,
                ts: None,
            },
        )
        .await
        .expect("notify 1");
        briefing::notify(
            &pool,
            &tree,
            &notify_caller,
            NotifyRequest {
                wiki_id: wiki.clone(),
                topic: "second".to_owned(),
                body: "second body".to_owned(),
                source_kind: BriefingSourceKind::Consumer,
                source_ref: "test:notify".to_owned(),
                kind: Some(BriefingKind::Reasoning.as_str().to_owned()),
                target_cite: None,
                ts: None,
            },
        )
        .await
        .expect("notify 2");
        let resp = bootstrap(&pool, &tree, &alice_smart(), BootstrapRequest::default())
            .await
            .expect("bootstrap");
        let acme_row = resp
            .smart_wikis
            .into_iter()
            .find(|c| c.wiki_id == wiki)
            .expect("acme present");
        assert_eq!(acme_row.recent_briefing.len(), 2);
        assert!(acme_row.briefing_counts.total >= 2);
        assert!(
            acme_row
                .recent_briefing
                .iter()
                .any(|i| i.topic == "first" || i.topic == "second")
        );
    }

    #[tokio::test]
    async fn bootstrap_returns_empty_when_owner_has_no_smart_wikis() {
        let (_dir, tree, pool) = seeded_tree().await;
        let resp = bootstrap(&pool, &tree, &alice_smart(), BootstrapRequest::default())
            .await
            .expect("bootstrap");
        assert!(resp.smart_wikis.is_empty());
        assert_eq!(resp.caller_sender_id, "alice");
    }

    #[tokio::test]
    async fn recall_core_global_rejects_standard_consumer() {
        let (_dir, tree, pool) = seeded_tree().await;
        let err = recall_core_global(
            &pool,
            &tree,
            embedder(),
            &alice_standard(),
            Vec::new(),
            RecallCoreGlobalRequest {
                query: "anything".to_owned(),
                limit: None,
            },
        )
        .await
        .expect_err("rejected");
        assert!(matches!(err, SmartError::RequiresSmart));
    }

    #[tokio::test]
    async fn recall_core_global_rejects_empty_query() {
        let (_dir, tree, pool) = seeded_tree().await;
        let err = recall_core_global(
            &pool,
            &tree,
            embedder(),
            &alice_smart(),
            Vec::new(),
            RecallCoreGlobalRequest {
                query: "   ".to_owned(),
                limit: None,
            },
        )
        .await
        .expect_err("rejected");
        assert!(matches!(err, SmartError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn recall_core_global_clamps_limit_and_returns_empty_on_no_facts() {
        let (_dir, tree, pool) = seeded_tree().await;
        let resp = recall_core_global(
            &pool,
            &tree,
            embedder(),
            &alice_smart(),
            Vec::new(),
            RecallCoreGlobalRequest {
                query: "anything".to_owned(),
                limit: Some(9_999),
            },
        )
        .await
        .expect("recall");
        // No fact_index rows yet, so empty hits — but the response
        // shape (filter echo + smart-family stems list) is populated.
        assert!(resp.hits.is_empty());
        assert_eq!(resp.filter_applied.owner_user, "alice");
        assert_eq!(resp.query, "anything");
    }

    #[tokio::test]
    async fn recall_core_global_excludes_smart_family_stems_in_filter_echo() {
        let (_dir, tree, pool) = seeded_tree().await;
        // Create a smart wiki (`_meta.md` smart flag, stamped at
        // `wiki_admin` create) — the excluded family is derived from
        // the on-disk flag, not the registry.
        create_smart_wiki_for(&pool, &tree, &alice_smart(), "acme", "Acme", None).await;
        let resp = recall_core_global(
            &pool,
            &tree,
            embedder(),
            &alice_smart(),
            Vec::new(),
            RecallCoreGlobalRequest {
                query: "anything".to_owned(),
                limit: None,
            },
        )
        .await
        .expect("recall");
        assert!(
            resp.filter_applied
                .excluded_wiki_types
                .contains(&"wiki-companion".to_owned()),
            "filter echo must mention the smart-family stem the caller can't see, got {:?}",
            resp.filter_applied.excluded_wiki_types
        );
    }
}
