// SPDX-License-Identifier: AGPL-3.0-or-later
//! The gold-set gate — a candidate repair proves itself on a scratch
//! copy before it may touch the live memory.
//!
//! Self-correcting REM's safety discipline: **the judge of a fix is
//! never an LLM's opinion; it is a replayed regression.** A repair
//! (today: the recall-repair sub-job's re-file, [`crate::rem`]) is
//! applied to a throwaway snapshot of the workdir, then two checks run
//! against the snapshot:
//!
//! 1. **The target check** — did the repair make the missed fact
//!    surface for the query that missed it? Judged by **fact id** (flat
//!    top-K) or by the fact's home page appearing among the navigated
//!    fragments — deterministic, robust to prose restyling.
//! 2. **The gold-set regression** — the operator's
//!    [`recall_eval`](crate::recall_eval) gold set
//!    ([`RECALL_GOLD_FILENAME`]) replays before and after; coverage must
//!    not drop anywhere.
//!
//! Only a repair that flips the target *and* regresses nothing earns a
//! [`GateVerdict::passes`]; the caller then re-applies it on the real
//! workdir (act-first, receipt + revert as usual). An empty or absent
//! gold set degrades the gate to the target check alone — the system
//! stays honest about what it can prove, and grows the gold set from
//! its own confirmed misses ([`append_gold_candidate`], the 15f loop).
//!
//! The scratch snapshot is a `VACUUM INTO` of the live DB (consistent
//! under WAL) plus a plain copy of `wikis/` — media, prompts, and logs
//! are irrelevant to recall replay and are not copied.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use sqlx::SqlitePool;

use crate::embedder::Embedder;
use crate::ingest::IngestPolicy;
use crate::llm::LlmBackend;
use crate::recall::SenderContext;
use crate::recall_eval::{EvalReport, GoldSet};
use crate::types::FactId;
use crate::wiki::WikiTree;
use crate::{db, enrollment, fact_index, recall, recall_eval, recall_nav};

/// Canonical per-deployment gold set the gate replays
/// (`<workdir>/recall-gold.yaml`, the `recall_eval` YAML shape).
/// Absent → the gate runs on the target check alone.
pub const RECALL_GOLD_FILENAME: &str = "recall-gold.yaml";

/// Where confirmed misses land as **candidate** gold cases.
///
/// `<workdir>/recall-gold-candidates.yaml` — same YAML shape, reviewed
/// and merged into [`RECALL_GOLD_FILENAME`] by the operator, never
/// auto-promoted (a noisy case must not become the judge).
pub const RECALL_GOLD_CANDIDATES_FILENAME: &str = "recall-gold-candidates.yaml";

/// The query that missed, replayed as production would run it.
#[derive(Debug)]
pub struct TargetCase<'a> {
    /// The user's restatement (the turn text of the miss).
    pub query: &'a str,
    /// Acting sender of the missed turn.
    pub sender_id: &'a str,
    /// The turn's classifier topic seeds (entry-point gather input).
    pub topics: &'a [String],
    /// The fact recall failed to surface.
    pub fact_id: &'a str,
}

/// What the replay proved.
#[derive(Debug)]
pub struct GateVerdict {
    /// The target already surfaced on the scratch **baseline** — the
    /// corpus healed itself since the miss; no repair is needed.
    pub target_before: bool,
    /// The target surfaced after the candidate repair.
    pub target_after: bool,
    /// Some gold query lost coverage under the repair.
    pub gold_regressed: bool,
    /// Gold queries replayed (0 = target-only gate).
    pub gold_queries: usize,
}

impl GateVerdict {
    /// The repair proved itself: the miss flips and nothing regresses.
    #[must_use]
    pub const fn passes(&self) -> bool {
        !self.target_before && self.target_after && !self.gold_regressed
    }

    /// The miss no longer reproduces — nothing to repair.
    #[must_use]
    pub const fn stale(&self) -> bool {
        self.target_before
    }
}

/// Run one candidate repair through the gate.
///
/// `apply` receives the **scratch** pool + tree and performs the repair
/// there (receipts/events it writes land in the snapshot and are
/// discarded with it). The live workdir is never touched.
///
/// # Errors
///
/// Snapshot I/O, scratch DB open, replay failures, or the `apply`
/// closure's own error.
pub async fn gate_repair<F>(
    pool: &SqlitePool,
    workdir: &Path,
    embedder: Arc<dyn Embedder>,
    navigator: Option<&dyn LlmBackend>,
    recall_policy: &IngestPolicy,
    gold: &GoldSet,
    target: &TargetCase<'_>,
    apply: F,
) -> anyhow::Result<GateVerdict>
where
    F: AsyncFnOnce(&SqlitePool, &WikiTree) -> anyhow::Result<()>,
{
    let scratch = tempfile::tempdir().context("create scratch dir")?;
    snapshot_workdir(pool, workdir, scratch.path()).await?;
    let s_pool = db::open_or_init(scratch.path())
        .await
        .context("open scratch db")?;
    let s_tree = WikiTree::open(scratch.path()).context("open scratch tree")?;

    let target_before = target_surfaced(
        &s_pool,
        &s_tree,
        Arc::clone(&embedder),
        navigator,
        recall_policy,
        target,
    )
    .await?;
    if target_before {
        return Ok(GateVerdict {
            target_before: true,
            target_after: true,
            gold_regressed: false,
            gold_queries: gold.queries.len(),
        });
    }
    let gold_before = replay_gold(&s_pool, &s_tree, &embedder, navigator, recall_policy, gold)
        .await
        .context("gold replay (baseline)")?;

    apply(&s_pool, &s_tree).await.context("scratch apply")?;

    let target_after = target_surfaced(
        &s_pool,
        &s_tree,
        Arc::clone(&embedder),
        navigator,
        recall_policy,
        target,
    )
    .await?;
    let gold_after = replay_gold(&s_pool, &s_tree, &embedder, navigator, recall_policy, gold)
        .await
        .context("gold replay (patched)")?;

    Ok(GateVerdict {
        target_before: false,
        target_after,
        gold_regressed: regressed(gold_before.as_ref(), gold_after.as_ref()),
        gold_queries: gold.queries.len(),
    })
}

/// Load the deployment's gold set from [`RECALL_GOLD_FILENAME`].
///
/// Absent file → empty set (target-only gate); a malformed file is an
/// error — the operator's hand-written judge deserves a loud failure,
/// not a silent skip.
///
/// # Errors
///
/// I/O or YAML parse failures.
pub fn load_gold_set(workdir: &Path) -> anyhow::Result<GoldSet> {
    let path = workdir.join(RECALL_GOLD_FILENAME);
    if !path.is_file() {
        return Ok(GoldSet {
            queries: Vec::new(),
        });
    }
    let yaml =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    GoldSet::parse(&yaml).with_context(|| format!("parse {}", path.display()))
}

/// Append a candidate gold case derived from a confirmed miss.
///
/// The 15f loop: the restated turn is the query, the missed fact's text
/// is the expectation. Appended to [`RECALL_GOLD_CANDIDATES_FILENAME`],
/// deduplicated by fact id; the operator reviews, distils the
/// expectation snippets, and merges cases into the real gold file.
///
/// Returns `false` when a candidate for this fact already exists.
///
/// # Errors
///
/// I/O or YAML codec failures.
pub fn append_gold_candidate(
    workdir: &Path,
    miss: &crate::recall_log::MissRow,
    fact_text: &str,
) -> anyhow::Result<bool> {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct Candidates {
        #[serde(default)]
        queries: Vec<Candidate>,
    }
    #[derive(Debug, Serialize, Deserialize)]
    struct Candidate {
        id: String,
        query: String,
        sender_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        topics: Vec<String>,
        expect: Vec<String>,
        /// Extra key the gold parser ignores — the provenance anchor.
        fact_id: String,
    }

    let path = workdir.join(RECALL_GOLD_CANDIDATES_FILENAME);
    let mut current: Candidates = if path.is_file() {
        let yaml =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_yaml::from_str(&yaml).with_context(|| format!("parse {}", path.display()))?
    } else {
        Candidates::default()
    };
    if current.queries.iter().any(|c| c.fact_id == miss.fact_id) {
        return Ok(false);
    }
    current.queries.push(Candidate {
        id: format!("miss-{}", miss.miss_id),
        query: miss.restated_text.clone(),
        sender_id: miss.sender_id.clone(),
        topics: miss.seed_topics.clone(),
        expect: vec![
            crate::parser::strip_embed_markers(fact_text)
                .trim()
                .to_owned(),
        ],
        fact_id: miss.fact_id.clone(),
    });
    let yaml = serde_yaml::to_string(&current).context("serialize candidates")?;
    crate::wiki::atomic_write(&path, yaml.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Consistent scratch snapshot: `VACUUM INTO` for the DB (safe against
/// a live WAL pool), plain recursive copy for `wikis/`.
async fn snapshot_workdir(pool: &SqlitePool, workdir: &Path, scratch: &Path) -> anyhow::Result<()> {
    let db_dest = scratch.join("engine.db");
    let quoted = db_dest
        .to_str()
        .context("scratch path not utf-8")?
        .replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{quoted}'"))
        .execute(pool)
        .await
        .context("VACUUM INTO scratch")?;
    copy_dir(&workdir.join("wikis"), &scratch.join("wikis")).context("copy wikis")?;
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Does the query surface the fact on THIS corpus — as a flat top-K hit
/// (by id), or as a navigated fragment of its home page?
async fn target_surfaced(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    navigator: Option<&dyn LlmBackend>,
    policy: &IngestPolicy,
    target: &TargetCase<'_>,
) -> anyhow::Result<bool> {
    let sender_groups = enrollment::groups_for(pool, target.sender_id)
        .await
        .context("sender groups")?;
    let sender = SenderContext {
        sender_id: target.sender_id.to_owned(),
        sender_groups,
    };
    let hits = recall::wiki_search_unrecorded(
        pool,
        Arc::clone(&embedder),
        target.query,
        policy.recall_top_k,
        fact_index::FactFilters::default(),
        &sender,
    )
    .await
    .context("flat replay")?;
    if hits.iter().any(|h| h.fact_id.as_str() == target.fact_id) {
        return Ok(true);
    }
    let Some(nav) = navigator else {
        return Ok(false);
    };
    // The fact's CURRENT home on this corpus (post-repair it has moved).
    let fact_id = FactId::parse(target.fact_id).context("target fact id")?;
    let Some(row) = fact_index::find_by_id(pool, &fact_id).await? else {
        return Ok(false);
    };
    let entries =
        recall_nav::gather_entry_points(pool, tree, &sender, target.topics, &[], &hits, &[])
            .await
            .context("gather replay")?;
    if entries.is_empty() {
        return Ok(false);
    }
    let outcome = recall_nav::navigate(
        pool,
        tree,
        nav,
        &sender,
        target.query,
        &entries,
        &policy.nav,
    )
    .await
    .context("navigate replay")?;
    for f in &outcome.fragments {
        let Ok(wid) = crate::types::WikiId::parse(&f.wiki_id) else {
            continue;
        };
        let Ok(handle) = tree.locate(&wid) else {
            continue;
        };
        let fragment_path = handle.rel_dir().join(&f.page);
        if fragment_path.to_str() == Some(row.source_path.as_str()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Replay the gold set; `None` when it is empty (nothing to regress).
async fn replay_gold(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    navigator: Option<&dyn LlmBackend>,
    policy: &IngestPolicy,
    gold: &GoldSet,
) -> anyhow::Result<Option<EvalReport>> {
    if gold.queries.is_empty() {
        return Ok(None);
    }
    let report = recall_eval::run_eval(pool, tree, Arc::clone(embedder), navigator, gold, policy)
        .await
        .context("gold replay")?;
    Ok(Some(report))
}

/// Coverage must not drop anywhere: per-query `combined_covered` and
/// `flat_covered` are compared pairwise (gold order is stable).
fn regressed(before: Option<&EvalReport>, after: Option<&EvalReport>) -> bool {
    let (Some(before), Some(after)) = (before, after) else {
        return false;
    };
    before
        .queries
        .iter()
        .zip(after.queries.iter())
        .any(|(b, a)| a.combined_covered < b.combined_covered || a.flat_covered < b.flat_covered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use crate::types::Principal;

    fn write_wiki(workdir: &Path, slug: &str) {
        let dir = workdir.join("wikis").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("_meta.md"),
            format!(
                "---\nwiki_id: {slug}\nwiki_type: wiki-user\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{slug}'\n---\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("index.md"), "# index\n").unwrap();
    }

    async fn seed_fact(pool: &SqlitePool, id: &str, wiki: &str, text: &str) {
        let fact = fact_index::NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(id).unwrap(),
            wiki_id: wiki.to_owned(),
            source_path: format!("wikis/{wiki}/index.md"),
            region_start: None,
            region_end: None,
            text: text.to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
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
        };
        fact_index::insert(pool, &fact).await.expect("seed fact");
    }

    /// The harness mechanics: the scratch is isolated (the live DB is
    /// untouched by the apply), the target check flips when the repair
    /// works, and a regressing gold case fails the verdict.
    #[tokio::test]
    async fn gate_isolates_the_scratch_and_judges_flip_and_regression() {
        const TARGET: &str = "018f1234-5678-7abc-9def-00000000aa01";
        const DECOY: &str = "018f1234-5678-7abc-9def-00000000aa02";
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db");
        write_wiki(dir.path(), "alice");
        seed_fact(&pool, TARGET, "alice", "la crostata si fa con le renette").await;
        seed_fact(&pool, DECOY, "alice", "il gatto dorme sul divano").await;

        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::with_fixed_embedding(
            "fake",
            vec![0.1, 0.2, 0.3, 0.4],
        ));
        // top-1 flat: everything ties at cosine 1 — deleting the decoy on
        // the scratch makes the target the only (hence top-1) hit. A
        // synthetic repair, chosen to exercise the harness deterministically.
        let policy = IngestPolicy {
            recall_top_k: 1,
            ..IngestPolicy::default()
        };
        let target = TargetCase {
            query: "come si fa la crostata?",
            sender_id: "alice",
            topics: &[],
            fact_id: TARGET,
        };
        // Gold case that the repair REGRESSES (it expects the decoy).
        let gold: GoldSet = GoldSet::parse(
            "queries:\n  - query: il gatto\n    sender_id: alice\n    expect: [\"gatto dorme\"]\n",
        )
        .unwrap();

        let delete_decoy = async |s_pool: &SqlitePool, _tree: &WikiTree| -> anyhow::Result<()> {
            sqlx::query("DELETE FROM fact_index WHERE fact_id = ?")
                .bind(DECOY)
                .execute(s_pool)
                .await?;
            Ok(())
        };

        // The decoy occupies top-1 on the baseline only if it wins the tie;
        // force determinism by checking both possible baselines: when the
        // target already wins top-1 the verdict is STALE, otherwise the
        // delete-repair must flip the target and regress the gold case.
        let verdict = gate_repair(
            &pool,
            dir.path(),
            Arc::clone(&embedder),
            None,
            &policy,
            &gold,
            &target,
            delete_decoy,
        )
        .await
        .expect("gate runs");
        if verdict.stale() {
            assert!(!verdict.passes(), "a stale target never commits");
        } else {
            assert!(verdict.target_after, "deleting the decoy flips the target");
            assert!(verdict.gold_regressed, "the gold case lost its decoy");
            assert!(!verdict.passes(), "a regressing repair never passes");
        }

        // Isolation: the live DB still holds the decoy either way.
        let live: i64 = sqlx::query_scalar("SELECT count(*) FROM fact_index WHERE fact_id = ?")
            .bind(DECOY)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(live, 1, "the apply ran on the scratch only");

        // Same repair with an EMPTY gold set: nothing can regress — the
        // verdict rides the target flip alone.
        let verdict = gate_repair(
            &pool,
            dir.path(),
            Arc::clone(&embedder),
            None,
            &policy,
            &GoldSet {
                queries: Vec::new(),
            },
            &target,
            async |s_pool: &SqlitePool, _tree: &WikiTree| -> anyhow::Result<()> {
                sqlx::query("DELETE FROM fact_index WHERE fact_id = ?")
                    .bind(DECOY)
                    .execute(s_pool)
                    .await?;
                Ok(())
            },
        )
        .await
        .expect("gate runs");
        if !verdict.stale() {
            assert!(verdict.passes(), "target flip + empty gold = pass");
        }
    }

    #[test]
    fn gold_candidates_append_and_dedup_by_fact() {
        let dir = tempfile::tempdir().unwrap();
        let miss = crate::recall_log::MissRow {
            miss_id: 7,
            created_at: "2026-07-05T10:00:00+00:00".into(),
            sender_id: "alice".into(),
            fact_id: "018f1234-5678-7abc-9def-00000000aa01".into(),
            wiki_id: "alice".into(),
            source_path: "wikis/alice/index.md".into(),
            surface: "direct".into(),
            similarity: Some(0.9),
            restated_text: "la crostata si fa con le renette".into(),
            log_id: None,
            status: "new".into(),
            resolution: None,
            seed_topics: vec!["cucina".into()],
        };
        assert!(
            append_gold_candidate(dir.path(), &miss, "la crostata si fa con le renette").unwrap()
        );
        assert!(
            !append_gold_candidate(dir.path(), &miss, "la crostata si fa con le renette").unwrap(),
            "second append for the same fact is a dedup no-op"
        );
        let yaml =
            std::fs::read_to_string(dir.path().join(RECALL_GOLD_CANDIDATES_FILENAME)).unwrap();
        assert!(yaml.contains("miss-7"), "{yaml}");
        assert!(yaml.contains("cucina"), "{yaml}");
        // The candidates file parses as a regular gold set (extra keys
        // tolerated), so merging is copy-paste.
        let parsed = GoldSet::parse(&yaml).expect("gold-compatible");
        assert_eq!(parsed.queries.len(), 1);
    }
}
