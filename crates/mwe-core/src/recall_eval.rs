// SPDX-License-Identifier: AGPL-3.0-or-later
//! Recall eval harness — gold queries replayed against a workdir,
//! measuring recall-as-navigation against the flat-RAG baseline.
//!
//! The harness answers one question with numbers instead of anecdotes:
//! *does navigation surface what flat similarity search misses?* Each
//! [`GoldQuery`] carries the turn text and the **expectations** — text
//! snippets that a good recall must put in front of the consumer model.
//! The runner executes both paths exactly as the ingest turn does
//! (flat = `wiki_search`; navigation = `gather_entry_points` →
//! `navigate` with the flat hits as RAG seeds) and scores them by
//! case-insensitive substring containment:
//!
//! - **hit\@1 / hit\@3** — the flat baseline's classic IR rates: is at
//!   least one expectation present in the top-1 / top-3 hit texts?
//! - **coverage** — per path, the fraction of expectations found
//!   anywhere in its output (flat hit texts / navigated prose).
//! - **deviating** — the navigation-only catch: expectations the
//!   navigator surfaced that flat similarity missed. This is the metric
//!   the whole navigation design exists for (the
//!   deviating-but-relevant fact, dissimilar in wording).
//!
//! Measurement discipline: the flat pass uses
//! [`recall::wiki_search_unrecorded`] so an eval sweep never bumps the
//! corpus's recall counters — synthetic usage must not inflate the
//! recency signal. The harness holds no lockfile: it is read-only and
//! may run next to a live `mwe-mcp serve`.
//!
//! The gold set is YAML ([`GoldSet::parse`]); the dogfood run fills it
//! with real queries. CLI entry point: `mwe-mcp recall eval`.

use std::sync::Arc;

use anyhow::Context as _;
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::embedder::Embedder;
use crate::enrollment;
use crate::fact_index;
use crate::ingest::IngestPolicy;
use crate::llm::LlmBackend;
use crate::recall::{self, RecallHit, SenderContext};
use crate::recall_nav;
use crate::types::Principal;
use crate::wiki::WikiTree;

// ---------- Gold set ----------

/// The whole gold file: a list of queries.
#[derive(Debug, Deserialize)]
pub struct GoldSet {
    /// The gold queries, replayed in order.
    pub queries: Vec<GoldQuery>,
}

/// One gold query: a turn to replay and what recall must surface.
#[derive(Debug, Deserialize)]
pub struct GoldQuery {
    /// Display label; defaults to the query text.
    #[serde(default)]
    pub id: Option<String>,
    /// The turn text, exactly as a consumer would send it.
    pub query: String,
    /// Acting sender (ACL projection + principal seeds). Groups are
    /// looked up from enrollment like the production turn does.
    pub sender_id: String,
    /// Optional classifier-style topic seeds. Production derives these
    /// from the plan's capture units; a gold entry pins them when the
    /// query under test depends on them.
    #[serde(default)]
    pub topics: Vec<String>,
    /// Optional classifier-style owner seeds (`user:x` / `group:y`).
    #[serde(default)]
    pub owners: Vec<String>,
    /// Ground truth: text snippets a good recall must surface, matched
    /// case-insensitively against the hit texts / navigated prose.
    pub expect: Vec<String>,
}

impl GoldSet {
    /// Parse the YAML gold file.
    ///
    /// # Errors
    ///
    /// `serde_yaml` parse failures (malformed YAML, missing fields).
    pub fn parse(yaml: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }
}

// ---------- Reports ----------

/// Navigation-side result of one query.
#[derive(Debug)]
pub struct NavReport {
    /// Expectations found in the navigated prose.
    pub covered: usize,
    /// Expectations navigation surfaced that the flat pass missed.
    pub deviating: usize,
    /// Pages the navigator collected.
    pub fragments: usize,
    /// Navigator decisions spent.
    pub hops: usize,
}

/// Scores of one gold query.
#[derive(Debug)]
pub struct QueryReport {
    /// `id` when given, else the query text.
    pub label: String,
    /// Number of expectations.
    pub expectations: usize,
    /// At least one expectation in the top-1 flat hit.
    pub flat_hit_at_1: bool,
    /// At least one expectation in the top-3 flat hits.
    pub flat_hit_at_3: bool,
    /// Expectations found anywhere in the flat hits.
    pub flat_covered: usize,
    /// `None` when navigation was skipped (no navigator backend).
    pub nav: Option<NavReport>,
    /// Expectations found by either path.
    pub combined_covered: usize,
    /// Expectations neither path surfaced.
    pub missing: Vec<String>,
}

/// The whole run.
#[derive(Debug)]
pub struct EvalReport {
    /// Per-query scores, gold order.
    pub queries: Vec<QueryReport>,
}

impl EvalReport {
    /// Fraction of queries with a top-1 flat hit.
    #[must_use]
    pub fn hit_at_1_rate(&self) -> f64 {
        rate(
            self.queries.iter().filter(|q| q.flat_hit_at_1).count(),
            self.queries.len(),
        )
    }

    /// Fraction of queries with a top-3 flat hit.
    #[must_use]
    pub fn hit_at_3_rate(&self) -> f64 {
        rate(
            self.queries.iter().filter(|q| q.flat_hit_at_3).count(),
            self.queries.len(),
        )
    }

    /// Expectations the flat pass covered, over all expectations.
    #[must_use]
    pub fn flat_coverage(&self) -> f64 {
        rate(
            self.queries.iter().map(|q| q.flat_covered).sum(),
            self.queries.iter().map(|q| q.expectations).sum(),
        )
    }

    /// Expectations navigation covered, over all expectations. `None`
    /// when no query ran the navigator.
    #[must_use]
    pub fn nav_coverage(&self) -> Option<f64> {
        if self.queries.iter().all(|q| q.nav.is_none()) {
            return None;
        }
        Some(rate(
            self.queries
                .iter()
                .filter_map(|q| q.nav.as_ref().map(|n| n.covered))
                .sum(),
            self.queries.iter().map(|q| q.expectations).sum(),
        ))
    }

    /// Expectations either path covered, over all expectations.
    #[must_use]
    pub fn combined_coverage(&self) -> f64 {
        rate(
            self.queries.iter().map(|q| q.combined_covered).sum(),
            self.queries.iter().map(|q| q.expectations).sum(),
        )
    }

    /// Total navigation-only catches across the run — the headline
    /// number for "navigation beats flat".
    #[must_use]
    pub fn deviating_found(&self) -> usize {
        self.queries
            .iter()
            .filter_map(|q| q.nav.as_ref().map(|n| n.deviating))
            .sum()
    }
}

#[allow(clippy::cast_precision_loss)] // counts are tiny vs f64 mantissa
fn rate(num: usize, den: usize) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

// ---------- Runner ----------

/// Replay the gold set. `navigator = None` measures the flat baseline
/// only (every `QueryReport.nav` is `None`).
///
/// # Errors
///
/// Infrastructure failures only (DB, embedder, filesystem, enrollment
/// lookups). A navigator LLM failure inside the funnel degrades to a
/// partial navigation, as in production.
pub async fn run_eval(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    navigator: Option<&dyn LlmBackend>,
    gold: &GoldSet,
    policy: &IngestPolicy,
) -> anyhow::Result<EvalReport> {
    let mut queries = Vec::with_capacity(gold.queries.len());
    for q in &gold.queries {
        let report = eval_query(pool, tree, Arc::clone(&embedder), navigator, q, policy)
            .await
            .with_context(|| format!("gold query `{}`", q.label()))?;
        queries.push(report);
    }
    Ok(EvalReport { queries })
}

impl GoldQuery {
    fn label(&self) -> &str {
        self.id.as_deref().unwrap_or(&self.query)
    }
}

async fn eval_query(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    navigator: Option<&dyn LlmBackend>,
    q: &GoldQuery,
    policy: &IngestPolicy,
) -> anyhow::Result<QueryReport> {
    let groups = enrollment::groups_for(pool, &q.sender_id)
        .await
        .context("sender groups")?;
    let sender = SenderContext {
        sender_id: q.sender_id.clone(),
        sender_groups: groups,
    };

    // Flat baseline — the same call ingest Step 1 makes, minus the
    // recall-counter bump (synthetic usage must not pollute recency).
    let flat = recall::wiki_search_unrecorded(
        pool,
        embedder,
        &q.query,
        policy.recall_top_k,
        fact_index::FactFilters::default(),
        &sender,
    )
    .await
    .context("flat recall")?;

    let expectations: Vec<String> = q.expect.iter().map(|e| e.to_lowercase()).collect();
    let flat_texts: Vec<String> = flat.iter().map(|h| h.text.to_lowercase()).collect();
    let covered_by_flat: Vec<bool> = expectations
        .iter()
        .map(|e| flat_texts.iter().any(|t| t.contains(e)))
        .collect();
    let hit_in_top = |k: usize| {
        flat_texts
            .iter()
            .take(k)
            .any(|t| expectations.iter().any(|e| t.contains(e)))
    };

    // Navigation — same seeds the post-classification step builds:
    // gold-pinned topics/owners + the flat hits as RAG seeds.
    let nav_outcome = match navigator {
        Some(llm) => Some(navigate_query(pool, tree, llm, q, &sender, &flat, policy).await?),
        None => None,
    };
    let covered_by_nav: Vec<bool> = nav_outcome.as_ref().map_or_else(
        || vec![false; expectations.len()],
        |outcome| {
            let prose: String = outcome
                .fragments
                .iter()
                .map(|f| f.text.to_lowercase())
                .collect::<Vec<_>>()
                .join("\n");
            expectations.iter().map(|e| prose.contains(e)).collect()
        },
    );

    let combined_covered = covered_by_flat
        .iter()
        .zip(&covered_by_nav)
        .filter(|(f, n)| **f || **n)
        .count();
    let missing = q
        .expect
        .iter()
        .zip(covered_by_flat.iter().zip(&covered_by_nav))
        .filter(|(_, (f, n))| !**f && !**n)
        .map(|(e, _)| e.clone())
        .collect();

    Ok(QueryReport {
        label: q.label().to_owned(),
        expectations: expectations.len(),
        flat_hit_at_1: hit_in_top(1),
        flat_hit_at_3: hit_in_top(3),
        flat_covered: covered_by_flat.iter().filter(|c| **c).count(),
        nav: nav_outcome.map(|outcome| NavReport {
            covered: covered_by_nav.iter().filter(|c| **c).count(),
            deviating: covered_by_flat
                .iter()
                .zip(&covered_by_nav)
                .filter(|(f, n)| !**f && **n)
                .count(),
            fragments: outcome.fragments.len(),
            hops: outcome.hops,
        }),
        combined_covered,
        missing,
    })
}

async fn navigate_query(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    q: &GoldQuery,
    sender: &SenderContext,
    flat: &[RecallHit],
    policy: &IngestPolicy,
) -> anyhow::Result<recall_nav::NavigationOutcome> {
    let owners: Vec<Principal> = q
        .owners
        .iter()
        .map(|s| s.parse().with_context(|| format!("owner `{s}`")))
        .collect::<anyhow::Result<_>>()?;
    let entries =
        recall_nav::gather_entry_points(pool, tree, sender, &q.topics, &owners, flat, &[])
            .await
            .context("entry-point gather")?;
    recall_nav::navigate(pool, tree, llm, sender, &q.query, &entries, &policy.nav)
        .await
        .context("navigate")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::embedder::FakeEmbedder;
    use crate::llm::FakeLlmBackend;
    use crate::types::FactId;

    fn gold_yaml() -> &'static str {
        "queries:\n\
         \x20 - id: coffee\n\
         \x20   query: how does alice take her coffee?\n\
         \x20   sender_id: alice\n\
         \x20   expect:\n\
         \x20     - coffee black\n\
         \x20     - celiac\n"
    }

    #[test]
    fn gold_set_parses_yaml_with_defaults() {
        let gold = GoldSet::parse(gold_yaml()).expect("parse");
        assert_eq!(gold.queries.len(), 1);
        let q = &gold.queries[0];
        assert_eq!(q.label(), "coffee");
        assert_eq!(q.expect.len(), 2);
        assert!(q.topics.is_empty() && q.owners.is_empty());
    }

    async fn setup_workdir() -> (tempfile::TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db open");
        let wikis = dir.path().join("wikis").join("alice");
        std::fs::create_dir_all(&wikis).unwrap();
        std::fs::write(
            wikis.join("_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\n\
             acl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        // The deviating fact lives in the prose, dissimilar to the query.
        std::fs::write(
            wikis.join("index.md"),
            "# Alice\n\nAlice is celiac and avoids gluten everywhere.\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        (dir, tree, pool)
    }

    fn fact(id: &str, text: &str, embedding: Vec<f32>) -> fact_index::NewFact {
        fact_index::NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(id).unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/index.md".to_owned(),
            region_start: None,
            region_end: None,
            text: text.to_owned(),
            embedding,
            owner_id: Principal::User("alice".into()),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: None,
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            salience: None,
            target_page: None,
            style: None,
            page_description: None,
            source_ref: None,
        }
    }

    #[tokio::test]
    async fn eval_scores_flat_baseline_and_navigation_deviating_catch() {
        let (dir, tree, pool) = setup_workdir().await;
        // FakeEmbedder embeds every text to the same vector, so the
        // similar fact scores 1.0; the orthogonal one scores 0.0 and
        // still enters top-5 but its text holds neither expectation.
        fact_index::insert(
            &pool,
            &fact(
                "018f1234-5678-7abc-9def-00000000e001",
                "alice drinks her coffee black",
                vec![0.1, 0.2, 0.3, 0.4],
            ),
        )
        .await
        .expect("insert similar");
        fact_index::insert(
            &pool,
            &fact(
                "018f1234-5678-7abc-9def-00000000e002",
                "alice has a red bicycle",
                vec![0.4, -0.1, -0.2, 0.1],
            ),
        )
        .await
        .expect("insert noise");

        let gold = GoldSet::parse(gold_yaml()).expect("parse");
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::with_fixed_embedding(
            "fake-bge",
            vec![0.1, 0.2, 0.3, 0.4],
        ));
        let nav = FakeLlmBackend::new(
            "fake-nav",
            "{\"open\":[{\"wiki_id\":\"alice\"}],\"done\":true}",
        );
        let policy = IngestPolicy::default();

        let report = run_eval(&pool, &tree, embedder, Some(&nav), &gold, &policy)
            .await
            .expect("eval");
        assert_eq!(report.queries.len(), 1);
        let q = &report.queries[0];
        // Flat finds the coffee fact at top-1; "celiac" lives only in
        // the prose, invisible to flat similarity over fact_index.
        assert!(q.flat_hit_at_1 && q.flat_hit_at_3);
        assert_eq!(q.flat_covered, 1);
        // The navigator opened alice's index.md, whose prose carries
        // the deviating fact — the catch the harness exists to count.
        let nav_report = q.nav.as_ref().expect("navigation ran");
        assert_eq!(nav_report.deviating, 1);
        assert_eq!(q.combined_covered, 2);
        assert!(q.missing.is_empty(), "{:?}", q.missing);

        assert!((report.hit_at_1_rate() - 1.0).abs() < f64::EPSILON);
        assert!((report.flat_coverage() - 0.5).abs() < f64::EPSILON);
        assert_eq!(report.deviating_found(), 1);
        assert!((report.combined_coverage() - 1.0).abs() < f64::EPSILON);

        // Measurement discipline: the unrecorded search left the recall
        // counters untouched.
        let row = fact_index::find_by_id(
            &pool,
            &FactId::parse("018f1234-5678-7abc-9def-00000000e001").unwrap(),
        )
        .await
        .expect("find")
        .expect("row");
        assert!(row.last_recall_at.is_none());
        assert_eq!(row.recall_count_30d, 0);

        drop(dir);
    }

    #[tokio::test]
    async fn eval_without_navigator_reports_flat_only() {
        let (dir, tree, pool) = setup_workdir().await;
        fact_index::insert(
            &pool,
            &fact(
                "018f1234-5678-7abc-9def-00000000e003",
                "alice drinks her coffee black",
                vec![0.1, 0.2, 0.3, 0.4],
            ),
        )
        .await
        .expect("insert");
        let gold = GoldSet::parse(gold_yaml()).expect("parse");
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::with_fixed_embedding(
            "fake-bge",
            vec![0.1, 0.2, 0.3, 0.4],
        ));
        let report = run_eval(
            &pool,
            &tree,
            embedder,
            None,
            &gold,
            &IngestPolicy::default(),
        )
        .await
        .expect("eval");
        let q = &report.queries[0];
        assert!(q.nav.is_none());
        assert_eq!(report.nav_coverage(), None);
        assert_eq!(q.combined_covered, 1);
        assert_eq!(q.missing, vec!["celiac".to_owned()]);
        drop(dir);
    }
}
