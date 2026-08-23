//! The clause around a `[[wikilink]]`, as a second way to find the facts
//! beside it.
//!
//! A link is written inside a sentence, and that sentence says **why** the two
//! pages belong together: *«le abitudini e ricette che ne derivano sono
//! raccolte in [[famiglia/cucina]]»*. Without this, those words are prose and
//! nothing else — the parser reads the link out of them and throws the rest
//! away — so a fact standing beside the link can only be found by its own
//! words, and a question phrased in none of them never reaches it.
//!
//! On the external lab's corpus, indexing that span and letting it stand in
//! for the facts next to it moved a coeliac diagnosis from ranks 21/49/83/286
//! to 7/7/14/18 on four cooking questions saying neither *coeliac* nor
//! *gluten*, with retrieval precision unchanged. One corpus, one language, a
//! different embedder: a direction, never a setting.
//!
//! ## Three constraints, each measured, each counter-intuitive
//!
//! 1. **The clause, not the sentence** ([`clauses_of`]). A whole-sentence key
//!    is a blur — the same fact reaches rank 21–49 instead of 7.
//! 2. **Only the facts beside it** ([`covered_facts`]). Widening a key to also
//!    cover the *target page's* facts puts ~19 keys on every covered fact; the
//!    max over nineteen stops discriminating and the gain disappears.
//! 3. **Max across keys, never a summed bonus** ([`best_scores`]). The
//!    additive form cost 5 points of precision for the same reach.
//!
//! **One hop and no further.** At two steps every page of that corpus had 21
//! neighbours out of 456 and "near" stopped meaning anything. There is nothing
//! transitive here and there must not be.
//!
//! ## What it cannot do
//!
//! It never widens what a reader may see. A key changes the **score** of facts
//! the reader is already allowed to read — `recall`'s visibility filter runs
//! before scoring — and the clause text is never returned to anyone. A key
//! written on a page a reader cannot open still shows them nothing.

use std::collections::{BTreeMap, HashMap};

use sqlx::{Row, SqlitePool};

use crate::fact_index::{decode_embedding, encode_embedding};
use crate::types::FactId;

/// Errors raised by the link-key store.
#[derive(Debug, thiserror::Error)]
pub enum LinkKeyError {
    /// Underlying SQL failure.
    #[error("link_key db: {0}")]
    Db(#[from] sqlx::Error),
    /// `covers` could not be encoded or decoded.
    #[error("link_key json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, LinkKeyError>;

/// The longest clause worth keeping, in characters.
///
/// The point of a clause key is that it is *narrow* — the span the link sits
/// in, not the paragraph around it. A page written without any of the marks
/// [`clauses_of`] splits on would otherwise hand its whole body to one key,
/// which is the blur the measurement warns about, at the cost of the whole
/// gain.
const MAX_CLAUSE_CHARS: usize = 400;

/// The shortest clause worth keeping, in characters.
///
/// A span of a few words is either the link's own address or a fragment with
/// no content of its own; embedding it produces a vector that matches
/// everything a little and nothing in particular.
const MIN_CLAUSE_CHARS: usize = 20;

/// One key on its way into the store: the clause, the facts it covers, and
/// its vector when the embedder was up.
pub type PendingKey = (Clause, Vec<String>, Option<Vec<f32>>);

/// One clause and what it points at, before it reaches the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clause {
    /// The canonical link target, `wiki_id/page-slug`.
    pub target: String,
    /// The span of prose the link sits in, markers stripped.
    pub text: String,
    /// Byte offset of the clause in the page body.
    pub at: usize,
}

/// A stored key, as the read path sees it.
#[derive(Debug, Clone)]
pub struct LinkKeyRow {
    /// The page carrying the link.
    pub source_path: String,
    /// The canonical link target.
    pub target: String,
    /// The `fact_id`s beside the clause.
    pub covers: Vec<String>,
    /// The clause's vector; `None` when the embedder was down when it was
    /// written.
    pub embedding: Option<Vec<f32>>,
}

/// Cut a page body into the clauses that carry a `[[wikilink]]`.
///
/// A clause is the span between sentence boundaries **and** the marks that
/// separate clauses inside a sentence — dashes, semicolons, colons, brackets.
/// Splitting further than the sentence is constraint 1, and it is the whole
/// difference between rank 7 and rank 21.
///
/// The link's own `[[…]]` bytes are removed from the stored text: the address
/// is not part of what the clause means, and leaving it in embeds a slug.
/// A clause with no link, one shorter than [`MIN_CLAUSE_CHARS`], or one longer
/// than [`MAX_CLAUSE_CHARS`] is dropped.
#[must_use]
pub fn clauses_of(body: &str) -> Vec<Clause> {
    let mut out: Vec<Clause> = Vec::new();
    let mut start = 0usize;
    let bytes = body.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        let is_break = matches!(b, b'.' | b'!' | b'?' | b';' | b':' | b'\n' | b'(' | b')');
        // An em/en dash is multi-byte; catch it on its lead byte.
        let is_dash = body.is_char_boundary(i) && body[i..].starts_with(['—', '–']);
        if !(is_break || is_dash) {
            continue;
        }
        push_clause(body, start, i, &mut out);
        start = i + 1;
        while start < body.len() && !body.is_char_boundary(start) {
            start += 1;
        }
    }
    push_clause(body, start, body.len(), &mut out);
    out
}

/// Keep `body[start..end]` when it carries exactly the kind of link this is
/// about and reads as more than an address.
fn push_clause(body: &str, start: usize, end: usize, out: &mut Vec<Clause>) {
    if start >= end || !body.is_char_boundary(start) || !body.is_char_boundary(end) {
        return;
    }
    let span = &body[start..end];
    // A page hop only. A bare `[[wiki_id]]` names a wiki, which is not a
    // destination, so a clause carrying one carries no link at all.
    let Some(target) = crate::recall::extract_wikilinks(span)
        .into_iter()
        .find_map(|l| l.page.map(|p| format!("{}/{}", l.wiki_id, p)))
    else {
        return;
    };
    let text = strip_links(span);
    let text = crate::parser::strip_embed_markers(&text);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = text.chars().count();
    if !(MIN_CLAUSE_CHARS..=MAX_CLAUSE_CHARS).contains(&n) {
        return;
    }
    out.push(Clause {
        target,
        text,
        at: start,
    });
}

/// Drop every `[[…]]` from a span, keeping any `|display` alias — the alias is
/// the words a reader sees, the address is not.
fn strip_links(span: &str) -> String {
    let mut out = String::with_capacity(span.len());
    let mut rest = span;
    while let Some(open) = rest.find("[[") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("]]") else {
            break;
        };
        let inner = &after[..close];
        if let Some((_, alias)) = inner.split_once('|') {
            out.push_str(alias);
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

/// The facts **beside** `at` — constraint 2.
///
/// A clause either sits inside a fact's region (the Cronista wraps a fact's
/// prose in one) or in the connective prose between two. Inside a region it
/// covers that fact; between regions it covers the one before and the one
/// after, which is what "beside" means for a sentence that joins two things.
///
/// Never the target page's facts. That is the widening the measurement
/// refuses: nineteen keys on one fact, and a max over nineteen discriminates
/// nothing.
#[must_use]
pub fn covered_facts(regions: &[(usize, usize, FactId)], at: usize) -> Vec<String> {
    if let Some((_, _, id)) = regions.iter().find(|(s, e, _)| at >= *s && at < *e) {
        return vec![id.as_str().to_owned()];
    }
    let before = regions
        .iter()
        .filter(|(_, e, _)| *e <= at)
        .max_by_key(|(_, e, _)| *e);
    let after = regions
        .iter()
        .filter(|(s, _, _)| *s > at)
        .min_by_key(|(s, _, _)| *s);
    before
        .into_iter()
        .chain(after)
        .map(|(_, _, id)| id.as_str().to_owned())
        .collect()
}

/// The fact regions of a parsed page, as `(start, end, fact_id)`.
#[must_use]
pub fn fact_regions(parsed: &crate::parser::ParseOutput) -> Vec<(usize, usize, FactId)> {
    parsed
        .events
        .iter()
        .filter_map(|ev| match ev {
            crate::parser::ParseEvent::Region {
                start, end, attrs, ..
            } => attrs.fact_id.clone().map(|id| (*start, *end, id)),
            _ => None,
        })
        .collect()
}

/// Replace one page's keys with `keys`, in one transaction.
///
/// Wholesale, never incremental: a rewrite moves every clause on the page, so
/// there is nothing to reconcile and matching old rows to new ones would only
/// invent a way to be wrong.
///
/// # Errors
///
/// Propagates SQL and JSON failures.
pub async fn replace_for_page(
    pool: &SqlitePool,
    source_path: &str,
    wiki_id: &str,
    keys: &[PendingKey],
    now: &str,
) -> Result<u64> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM link_key WHERE source_path = ?")
        .bind(source_path)
        .execute(&mut *tx)
        .await?;
    let mut written = 0u64;
    for (clause, covers, embedding) in keys {
        if covers.is_empty() {
            continue;
        }
        let dim = embedding.as_ref().and_then(|v| i64::try_from(v.len()).ok());
        sqlx::query(
            "INSERT INTO link_key
               (source_path, wiki_id, target, clause, covers, embedding, embedding_dim, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(source_path)
        .bind(wiki_id)
        .bind(&clause.target)
        .bind(&clause.text)
        .bind(serde_json::to_string(covers)?)
        .bind(embedding.as_deref().map(encode_embedding))
        .bind(dim)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        written += 1;
    }
    tx.commit().await?;
    Ok(written)
}

/// Drop one page's keys — the page is gone.
///
/// # Errors
///
/// Propagates the SQL failure.
pub async fn drop_page(pool: &SqlitePool, source_path: &str) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM link_key WHERE source_path = ?")
        .bind(source_path)
        .execute(pool)
        .await?
        .rows_affected())
}

/// Drop a whole wiki's keys.
///
/// # Errors
///
/// Propagates the SQL failure.
pub async fn drop_wiki(pool: &SqlitePool, wiki_id: &str) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM link_key WHERE wiki_id = ?")
        .bind(wiki_id)
        .execute(pool)
        .await?
        .rows_affected())
}

/// Every key that carries a vector.
///
/// One read per query, the same order as the active-fact scan the recall path
/// already does: one key per link, and a page carries a handful.
///
/// # Errors
///
/// Propagates the SQL failure.
pub async fn all_embedded(pool: &SqlitePool) -> Result<Vec<LinkKeyRow>> {
    let rows = sqlx::query(
        "SELECT source_path, target, covers, embedding
           FROM link_key WHERE embedding IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let blob: Option<Vec<u8>> = r.get("embedding");
            let covers: String = r.get("covers");
            LinkKeyRow {
                source_path: r.get("source_path"),
                target: r.get("target"),
                covers: serde_json::from_str(&covers).unwrap_or_default(),
                embedding: blob.as_deref().and_then(|b| decode_embedding(b).ok()),
            }
        })
        .collect())
}

/// Each fact id mapped to the best score any key covering it earns against
/// `query`.
///
/// **Max, never a sum** — constraint 3. A fact covered by three keys is worth
/// what its best key is worth, because two weak keys are not one strong one,
/// and the additive form cost 5 points of precision for the same reach.
#[must_use]
pub fn best_scores(keys: &[LinkKeyRow], query: &[f32]) -> HashMap<String, f32> {
    let mut best: HashMap<String, f32> = HashMap::new();
    for k in keys {
        let Some(v) = &k.embedding else {
            continue;
        };
        let score = crate::recall::cosine_similarity(query, v);
        if !score.is_finite() {
            continue;
        }
        for fact in &k.covers {
            let slot = best.entry(fact.clone()).or_insert(f32::MIN);
            if score > *slot {
                *slot = score;
            }
        }
    }
    best
}

/// The clause texts one page currently has stored, for the write path's
/// change test.
///
/// Comparing the texts is exact and costs one `SELECT`. Comparing anything
/// coarser — the file stamp, the page's card — would re-embed every link on
/// every page whose testata moved, which is most of them on a compile night.
///
/// # Errors
///
/// Propagates the SQL failure.
pub async fn texts_for_page(pool: &SqlitePool, source_path: &str) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT clause FROM link_key WHERE source_path = ? ORDER BY id")
        .bind(source_path)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("clause")).collect())
}

/// How many keys each page carries — for the dashboard and the tests.
///
/// # Errors
///
/// Propagates the SQL failure.
pub async fn count_by_page(pool: &SqlitePool) -> Result<BTreeMap<String, i64>> {
    let rows = sqlx::query("SELECT source_path, count(*) AS n FROM link_key GROUP BY source_path")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("source_path"), r.get::<i64, _>("n")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fid(tail: u8) -> FactId {
        FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{tail:02x}")).unwrap()
    }

    /// **The clause, not the sentence** — constraint 1, and the whole
    /// difference between rank 7 and rank 21.
    #[test]
    fn a_clause_is_cut_at_the_marks_inside_a_sentence_too() {
        let body = "Cucina spesso la sera — le abitudini e le ricette che ne derivano \
                    sono raccolte in [[famiglia/cucina]], e il resto è altrove.";
        let cut = clauses_of(body);
        assert_eq!(cut.len(), 1, "{cut:?}");
        let c = &cut[0];
        assert_eq!(c.target, "famiglia/cucina");
        assert!(
            !c.text.contains("Cucina spesso la sera"),
            "the dash ends the previous clause: {c:?}"
        );
        assert!(c.text.starts_with("le abitudini"), "{c:?}");
        assert!(
            !c.text.contains("[[") && !c.text.contains("famiglia/cucina"),
            "the address is not part of what the clause means: {c:?}"
        );
    }

    /// A display alias is the words a reader sees, so it stays; the address
    /// never does.
    #[test]
    fn an_alias_survives_and_the_address_does_not() {
        let body = "Le ricette di casa stanno tutte sulla [[famiglia/cucina|pagina della cucina]].";
        let cut = clauses_of(body);
        assert_eq!(cut.len(), 1, "{cut:?}");
        assert!(cut[0].text.contains("pagina della cucina"), "{cut:?}");
        assert!(!cut[0].text.contains("famiglia/cucina"), "{cut:?}");
    }

    /// A bare `[[wiki]]` names a wiki, and recall opens pages: no destination,
    /// so no key.
    #[test]
    fn a_bare_wiki_link_is_not_a_key() {
        assert!(clauses_of("Ne abbiamo parlato a lungo con [[bob]] durante la cena.").is_empty());
    }

    /// A span too short says nothing of its own; one too long is the blur the
    /// measurement warns about.
    #[test]
    fn a_clause_too_short_or_too_long_is_dropped() {
        assert!(clauses_of("Vedi [[a/b]].").is_empty(), "too short");
        let long = format!(
            "{} e quindi vedi [[a/b]] per il resto.",
            "parola ".repeat(MAX_CLAUSE_CHARS)
        );
        assert!(clauses_of(&long).is_empty(), "too long");
    }

    /// **Only the facts beside it** — constraint 2.
    #[test]
    fn a_clause_covers_the_facts_next_to_it_and_nothing_else() {
        let regions = vec![(0, 10, fid(1)), (30, 40, fid(2)), (80, 90, fid(3))];
        assert_eq!(
            covered_facts(&regions, 5),
            vec![fid(1).as_str().to_owned()],
            "inside a region it covers that fact alone"
        );
        assert_eq!(
            covered_facts(&regions, 50),
            vec![fid(2).as_str().to_owned(), fid(3).as_str().to_owned()],
            "between two, it covers the one before and the one after"
        );
        assert_eq!(
            covered_facts(&regions, 95),
            vec![fid(3).as_str().to_owned()],
            "past the last region there is only a before"
        );
        assert!(covered_facts(&[], 5).is_empty());
    }

    /// **Max, never a sum** — constraint 3. Three weak keys must not add up to
    /// a strong one.
    #[test]
    fn a_fact_is_worth_its_best_key_not_the_sum_of_them() {
        let key = |score_axis: Vec<f32>, covers: &[&str]| LinkKeyRow {
            source_path: "wikis/alice/x.md".to_owned(),
            target: "alice/y".to_owned(),
            covers: covers.iter().map(|s| (*s).to_owned()).collect(),
            embedding: Some(score_axis),
        };
        let keys = vec![
            key(vec![0.6, 0.8], &["f1"]),
            key(vec![1.0, 0.0], &["f1"]),
            key(vec![0.0, 1.0], &["f2"]),
        ];
        let scores = best_scores(&keys, &[1.0, 0.0]);
        assert!(
            (scores["f1"] - 1.0).abs() < 1e-6,
            "the best of the two, not their sum: {scores:?}"
        );
        assert!(scores["f2"].abs() < 1e-6, "{scores:?}");
    }

    /// A key with no vector — the embedder was down when the page was
    /// written — simply does not score. Smaller, never wrong.
    #[test]
    fn a_key_with_no_vector_scores_nothing() {
        let keys = vec![LinkKeyRow {
            source_path: "p".to_owned(),
            target: "t".to_owned(),
            covers: vec!["f1".to_owned()],
            embedding: None,
        }];
        assert!(best_scores(&keys, &[1.0, 0.0]).is_empty());
    }

    #[tokio::test]
    async fn a_pages_keys_are_replaced_wholesale_and_dropped_with_the_page() {
        let (_wd, pool) = crate::test_db::TestWorkdir::with_db().await;
        let clause = |t: &str| Clause {
            target: "alice/cucina".to_owned(),
            text: t.to_owned(),
            at: 0,
        };
        let keys = vec![
            (
                clause("le ricette che ne derivano sono raccolte altrove"),
                vec!["f1".to_owned()],
                Some(vec![1.0, 0.0]),
            ),
            (
                clause("e le abitudini che ne seguono stanno con quelle"),
                vec!["f2".to_owned()],
                Some(vec![0.0, 1.0]),
            ),
            // A key covering nothing is not stored: it could only ever score
            // for facts that are not there.
            (
                clause("un pezzo senza fatti accanto a se"),
                Vec::new(),
                None,
            ),
        ];
        let n = replace_for_page(&pool, "wikis/alice/x.md", "alice", &keys, "t")
            .await
            .expect("write");
        assert_eq!(n, 2);

        let stored = all_embedded(&pool).await.expect("read");
        assert_eq!(stored.len(), 2);

        // A rewrite replaces the lot rather than reconciling.
        let n = replace_for_page(&pool, "wikis/alice/x.md", "alice", &keys[..1], "t2")
            .await
            .expect("rewrite");
        assert_eq!(n, 1);
        assert_eq!(all_embedded(&pool).await.expect("read").len(), 1);

        drop_page(&pool, "wikis/alice/x.md").await.expect("drop");
        assert!(all_embedded(&pool).await.expect("read").is_empty());
    }
}
