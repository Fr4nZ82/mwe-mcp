//! Which pages a model is shown, once the memory has more of them than fit
//! one list.
//!
//! Two stages ask the same question. The Cartografo decides where a fact goes;
//! the Cronista decides which pages this page should link to. Past their
//! ceilings the obvious answer is **the pages nearest theirs**, ranked by card
//! similarity — and for the second question that is precisely the wrong list.
//! The link that carries a reader onward often goes to a page resembling this
//! one in nothing — the page that constrains a fact here while sharing none of
//! its words — and a nearest-ranked list is exactly where such a page never
//! appears. Serving only the nearest is hiding the answers this module exists
//! to offer.
//!
//! ## The measurement
//!
//! Rebuilding a corpus's structure from text and sender alone, one model call
//! per fact, the external lab reached **40 %** of the old engine's retrieval
//! when the call was shown the fifteen most similar candidates — and **zero**
//! on the targets similarity could never reach. Shown twenty candidates drawn
//! from four different pools it reached **89 %**, with *fewer* links written.
//! The lab's own summary: *«per obbligare davvero bisogna mettere davanti al
//! modello cose che non si assomigliano già, altrimenti la domanda risponde da
//! sola»*. The numbers come from one corpus, in one language, measured with a
//! different embedder — they are a direction, never a tuning.
//!
//! ## The four sources
//!
//! Each candidate line tells the model where it came from. That label is half
//! the mechanism: without it the model cannot tell that the last quarter of
//! the list is there *because* it does not resemble the page.
//!
//! - [`CandidateSource::Near`] — nearest by card. Answers "what exists", so a
//!   destination is never invented.
//! - [`CandidateSource::People`] — pages carrying facts about, or told by, the
//!   same principals. An axis the words do not carry.
//! - [`CandidateSource::Turn`] — pages holding facts extracted from the same
//!   conversational turns. Two facts of one turn sit at 0.256 textual
//!   similarity: the association is real and no embedder will find it.
//! - [`CandidateSource::Far`] — sampled across the far half. A page that
//!   constrains a fact here while sharing none of its words appears in no
//!   other source.
//!
//! A fifth, [`CandidateSource::Home`], is not a search: it is the caller's own
//! ordering, used only when the asking page has no card vector to rank with.
//! That is not the rare case — a page created in the run being compiled has no
//! stored card yet — so the vectorless asker must still be told what exists.
//!
//! ## What this module is not
//!
//! It does not rank, cut or render anything **below** a ceiling. While the
//! whole page list fits one call it is both cheaper (byte-identical for every
//! call of the run, so it rides the cached prefix) and complete, and a
//! selection would be strictly worse on both counts.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::SqlitePool;

use crate::recall::cosine_similarity;

/// How many candidates a selection carries.
///
/// Twenty is what the lab measured and 89 % is where it landed; the four
/// quotas below sum past it (founder, 2026-08-27) so a source that comes up
/// empty still leaves a usable list, and so the widest source is not the only
/// one with room. Above the measured point is an extrapolation, not a
/// setting — what the measurement fixes is the SHAPE, four pools rather than
/// one ranking. It stays well under a tenth of the ceilings that switch a
/// stage into selection mode
/// ([`crate::compiler::CARD_INDEX_CACHE_CEILING_PAGES`],
/// `planner::FOREST_PAGE_CEILING`), which is the arithmetic that makes those
/// ceilings a ceiling rather than a replacement.
pub const SELECTION_PAGES: usize = 32;

/// Per-source quotas, in fill order. They sum to [`SELECTION_PAGES`].
///
/// Order is fill order, not importance: a page reached by two sources is
/// credited to the first that claims it, so the cheapest and most redundant
/// source goes first and the scarce ones keep their seats.
const QUOTA_NEAR: usize = 12;
const QUOTA_PEOPLE: usize = 8;
const QUOTA_TURN: usize = 6;
const QUOTA_FAR: usize = 6;

/// A principal carried by more than this share of the pool is not a
/// discriminator, so it is ignored when matching [`CandidateSource::People`].
///
/// In a one-person memory the owner is the subject of nearly every fact, and
/// "shares a principal with me" would then be true of every page — a source
/// that matches everything selects nothing, and would spend six of the
/// the whole selection on an arbitrary slice. Turn hashes need no such guard:
/// one turn touches a handful of pages by construction.
const PRINCIPAL_UBIQUITY: f32 = 0.5;

/// Where a candidate came from — rendered on its line, for the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateSource {
    /// Nearest by card similarity.
    Near,
    /// Shares a principal with the asking page.
    People,
    /// Holds a fact from the same conversational turn.
    Turn,
    /// Sampled across the far half of the similarity ranking.
    Far,
    /// The caller's own fallback ordering, for an asker with no card vector.
    Home,
}

impl CandidateSource {
    /// The tag written on the candidate's line.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Near => "near",
            Self::People => "same-people",
            Self::Turn => "same-turn",
            Self::Far => "far",
            Self::Home => "same-wiki",
        }
    }
}

/// What one page carries that a candidate search can key on.
#[derive(Debug, Default, Clone)]
struct PageTraits {
    /// The page's card vector, when `page_card` holds one. A page whose card
    /// never embedded simply does not rank by nearness — the offer gets
    /// smaller, never wrong.
    embedding: Option<Vec<f32>>,
    /// Wire-form principals of the page's live facts: every `subject_id`, and
    /// every `sender_id` that has one.
    principals: BTreeSet<String>,
    /// `origin_message_hash` of the turns the page's facts were extracted
    /// from. Empty for a page whose facts predate the column or were written
    /// live rather than buffered.
    turns: BTreeSet<String>,
}

/// One page's side of the question: what the asker is, as traits.
#[derive(Debug, Default, Clone)]
pub struct Ask {
    /// The asker's card vectors — one for a page, several for a wiki.
    ///
    /// A candidate scores its **best** similarity against any of them rather
    /// than against their average: a wiki spans unrelated subjects, and a
    /// centroid over them is a point about none of them.
    embeddings: Vec<Vec<f32>>,
    principals: BTreeSet<String>,
    turns: BTreeSet<String>,
}

impl Ask {
    /// Whether nearness can be computed at all for this asker.
    #[must_use]
    pub const fn has_vector(&self) -> bool {
        !self.embeddings.is_empty()
    }

    /// Widen the ask with more directions to be near.
    ///
    /// Scoring takes the **best** similarity across the whole bag, so adding a
    /// vector can only raise a candidate's score and never lower one: an ask
    /// carrying a page's card plus each of its facts offers everything the card
    /// alone would have, and the pages only a single fact points at besides.
    /// That is the difference between "what is this page about" and "what does
    /// somebody standing on this fact need".
    pub fn widen_with(&mut self, vectors: impl IntoIterator<Item = Vec<f32>>) {
        self.embeddings.extend(vectors);
    }
}

/// One chosen candidate: the caller's own key, and why it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The key the caller loaded the pool with — a plan slug, a registry slug.
    pub key: String,
    /// Which source claimed it.
    pub source: CandidateSource,
}

/// Every page a stage may offer, with the traits a selection keys on.
///
/// Keyed by whatever the caller keys its pages by, so the pool never needs to
/// know whether it is serving plan slugs or registry slugs.
#[derive(Debug, Default)]
pub struct CandidatePool {
    entries: BTreeMap<String, PageTraits>,
    /// Principals over [`PRINCIPAL_UBIQUITY`] of the pool — see that constant.
    ubiquitous: BTreeSet<String>,
    /// How many pages arrived with a card vector — see [`Self::embedded`].
    embedded: usize,
}

impl CandidatePool {
    /// Read every page in `key_by_source_path`: its people, its turns, and its
    /// card vector.
    ///
    /// **Two queries for the whole run, not two per page.** The caller asks
    /// about every page it may offer at once, and the per-page work is set
    /// intersection and cosine in memory afterwards.
    ///
    /// The vector is **read** here, never made: a card is embedded by the
    /// reindex pipeline, which is the one place that has both the page's bytes
    /// and an embedder. A page whose card was never embedded simply does not
    /// rank by nearness, which is a smaller offer and never a wrong one.
    ///
    /// Failure is **not** an error. A pool that could not read its traits
    /// still ranks by nearness, and one that could not read its vectors still
    /// has the two sources that read none — so a database hiccup costs reach,
    /// never correctness.
    #[must_use]
    pub async fn load(pool: &SqlitePool, key_by_source_path: &BTreeMap<String, String>) -> Self {
        let mut entries: BTreeMap<String, PageTraits> = key_by_source_path
            .values()
            .map(|k| (k.clone(), PageTraits::default()))
            .collect();

        // The card vectors, in one read. A row for a page this pool does not
        // serve is simply not ours.
        let mut embedded = 0usize;
        match crate::page_card::list_all(pool).await {
            Ok(rows) => {
                for row in rows {
                    let Some(key) = key_by_source_path.get(&row.source_path) else {
                        continue;
                    };
                    let Some(t) = entries.get_mut(key) else {
                        continue;
                    };
                    if let Some(v) = row.embedding {
                        t.embedding = Some(v);
                        embedded += 1;
                    }
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "candidates: card vectors unread — no nearness");
            },
        }

        // LEFT JOIN: a fact written live (a list item, a container the user
        // asked for this turn) never passed through the buffer, so it has no
        // turn to contribute and must still contribute its principals.
        let rows: Vec<(String, String, Option<String>, Option<String>)> = match sqlx::query_as(
            "SELECT f.source_path, f.subject_id, f.sender_id, c.origin_message_hash
               FROM fact_index f
               LEFT JOIN capture_buffer c ON c.capture_id = f.fact_id
              WHERE f.deleted_at IS NULL AND f.superseded_at IS NULL",
        )
        .fetch_all(pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "candidates: traits unread — nearness only");
                return Self {
                    entries,
                    ubiquitous: BTreeSet::new(),
                    embedded,
                };
            },
        };

        for (source_path, subject, sender, turn) in rows {
            let Some(key) = key_by_source_path.get(&source_path) else {
                continue;
            };
            let Some(t) = entries.get_mut(key) else {
                continue;
            };
            t.principals.insert(subject);
            if let Some(s) = sender {
                t.principals.insert(s);
            }
            if let Some(h) = turn {
                t.turns.insert(h);
            }
        }

        let mut carriers: BTreeMap<&str, usize> = BTreeMap::new();
        for t in entries.values() {
            for p in &t.principals {
                *carriers.entry(p.as_str()).or_default() += 1;
            }
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a page count compared against a share; the float is the comparison, not a stored value"
        )]
        let bar = entries.len() as f32 * PRINCIPAL_UBIQUITY;
        let ubiquitous: BTreeSet<String> = carriers
            .into_iter()
            .filter(|&(_, n)| {
                #[expect(clippy::cast_precision_loss, reason = "see above")]
                let n = n as f32;
                n > bar
            })
            .map(|(p, _)| p.to_owned())
            .collect();

        Self {
            entries,
            ubiquitous,
            embedded,
        }
    }

    /// How many of this pool's pages arrived carrying a card vector.
    ///
    /// The rest cannot be ranked by nearness at all, so a small number against
    /// a large pool says the selection is running on the two sources that read
    /// no vector, plus the caller's own ordering.
    #[must_use]
    pub const fn embedded(&self) -> usize {
        self.embedded
    }

    /// Put a vector on one page directly, for a test that needs the cosine to
    /// come out somewhere known. [`Self::load`] is how a real pool gets them.
    pub fn set_embedding(&mut self, key: &str, embedding: Vec<f32>) {
        let Some(t) = self.entries.get_mut(key) else {
            return;
        };
        let was_empty = t.embedding.is_none();
        t.embedding = Some(embedding);
        if was_empty {
            self.embedded += 1;
        }
    }

    /// The asking side, unioned over `keys` — one page, or a wiki's pages.
    #[must_use]
    pub fn ask_for<'k>(&self, keys: impl IntoIterator<Item = &'k str>) -> Ask {
        let mut ask = Ask::default();
        for k in keys {
            let Some(t) = self.entries.get(k) else {
                continue;
            };
            if let Some(v) = &t.embedding {
                ask.embeddings.push(v.clone());
            }
            ask.principals.extend(t.principals.iter().cloned());
            ask.turns.extend(t.turns.iter().cloned());
        }
        ask
    }

    /// Compose up to `budget` candidates from the four sources, excluding
    /// `exclude` (the asker's own pages).
    ///
    /// Deterministic for a given pool: every source ranks by a total order and
    /// breaks its ties on the pool's own key order, so two runs over the same
    /// plan return the same list.
    ///
    /// De-duplicated across sources, **first claim wins**: a page that is both
    /// near and from the same turn takes one seat, and it takes it as `near`.
    /// Quotas a source cannot fill roll forward, so a memory whose facts
    /// predate the turn column still gets a full list.
    /// `home` is the caller's own ordering of what to show an asker that
    /// **cannot be ranked** — it is consulted only when `ask` carries no card
    /// vector, and ignored entirely otherwise.
    #[must_use]
    pub fn pick(
        &self,
        ask: &Ask,
        exclude: &BTreeSet<String>,
        budget: usize,
        home: &[String],
    ) -> Vec<Candidate> {
        let ranked = self.ranked_by_nearness(ask, exclude);

        let mut taken: BTreeSet<String> = BTreeSet::new();
        let mut out: Vec<Candidate> = Vec::with_capacity(budget);
        let push = |keys: Vec<String>,
                    source: CandidateSource,
                    quota: usize,
                    taken: &mut BTreeSet<String>,
                    out: &mut Vec<Candidate>| {
            let mut n = 0;
            for key in keys {
                if n == quota || out.len() == budget {
                    break;
                }
                if !taken.insert(key.clone()) {
                    continue;
                }
                out.push(Candidate { key, source });
                n += 1;
            }
        };

        push(
            ranked.iter().map(|(_, k)| k.clone()).collect(),
            CandidateSource::Near,
            QUOTA_NEAR,
            &mut taken,
            &mut out,
        );
        push(
            self.sharing(&ask.principals, &self.ubiquitous, exclude, |t| {
                &t.principals
            }),
            CandidateSource::People,
            QUOTA_PEOPLE,
            &mut taken,
            &mut out,
        );
        push(
            self.sharing(&ask.turns, &BTreeSet::new(), exclude, |t| &t.turns),
            CandidateSource::Turn,
            QUOTA_TURN,
            &mut taken,
            &mut out,
        );
        push(
            far_sample(&ranked, QUOTA_FAR),
            CandidateSource::Far,
            QUOTA_FAR,
            &mut taken,
            &mut out,
        );

        // Roll the unfilled quotas forward. Near first: its job is telling the
        // model what exists, which is the half that must not go short. Far
        // second, because a longer tail of unreachable candidates is the other
        // thing this list is for.
        push(
            ranked.iter().map(|(_, k)| k.clone()).collect(),
            CandidateSource::Near,
            budget,
            &mut taken,
            &mut out,
        );
        push(
            far_sample(&ranked, budget),
            CandidateSource::Far,
            budget,
            &mut taken,
            &mut out,
        );
        // Nothing could be scored, so the caller's own ordering is the only
        // thing left that can say what exists. Two ways to get here and the
        // fallback owes both: an asker with no vector to measure from, and a
        // pool whose destinations carry none to measure against — a memory
        // whose pages were written before anything embedded their cards, which
        // is the memory that most needs its first rails. The test is the
        // RANKING, never the ask: an ask can be rich in vectors and still rank
        // nothing, and keying on the ask left this pass offering zero
        // candidates on exactly that memory. Where the ranking did produce
        // something, `near` has already said what exists, and better.
        if ranked.is_empty() {
            push(
                home.iter()
                    .filter(|k| !exclude.contains(*k))
                    .cloned()
                    .collect(),
                CandidateSource::Home,
                budget,
                &mut taken,
                &mut out,
            );
        }
        out
    }

    /// Every candidate that can be scored, best similarity first.
    fn ranked_by_nearness(&self, ask: &Ask, exclude: &BTreeSet<String>) -> Vec<(f32, String)> {
        let mut scored: Vec<(f32, String)> = self
            .entries
            .iter()
            .filter(|(k, _)| !exclude.contains(*k))
            .filter_map(|(k, t)| {
                let v = t.embedding.as_ref()?;
                let best = ask
                    .embeddings
                    .iter()
                    .map(|m| cosine_similarity(m, v))
                    .fold(f32::NEG_INFINITY, f32::max);
                best.is_finite().then(|| (best, k.clone()))
            })
            .collect();
        // Descending by similarity; the pool's key order breaks ties, so the
        // same pool always yields the same ranking.
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored
    }

    /// Pages whose `field` intersects `mine`, most shared first.
    ///
    /// `ignore` drops the values that are true of nearly everything — see
    /// [`PRINCIPAL_UBIQUITY`]. The pool's key order breaks ties.
    fn sharing(
        &self,
        mine: &BTreeSet<String>,
        ignore: &BTreeSet<String>,
        exclude: &BTreeSet<String>,
        field: impl Fn(&PageTraits) -> &BTreeSet<String>,
    ) -> Vec<String> {
        let mut scored: Vec<(usize, &str)> = self
            .entries
            .iter()
            .filter(|(k, _)| !exclude.contains(*k))
            .filter_map(|(k, t)| {
                let shared = field(t)
                    .iter()
                    .filter(|v| mine.contains(*v) && !ignore.contains(*v))
                    .count();
                (shared > 0).then_some((shared, k.as_str()))
            })
            .collect();
        scored.sort_by_key(|(shared, _)| std::cmp::Reverse(*shared));
        scored.into_iter().map(|(_, k)| k.to_owned()).collect()
    }
}

/// `n` candidates spread evenly across the **far half** of `ranked`.
///
/// Not the strict tail: the very last entries of a similarity ranking are
/// whatever happens to be most orthogonal — an empty page, a page in another
/// language — which is noise rather than distance. Sampling the half gives the
/// model pages that are genuinely far from it and still about something.
fn far_sample(ranked: &[(f32, String)], n: usize) -> Vec<String> {
    if ranked.is_empty() || n == 0 {
        return Vec::new();
    }
    let far = &ranked[ranked.len() / 2..];
    let stride = (far.len() / n).max(1);
    far.iter()
        .rev()
        .step_by(stride)
        .take(n)
        .map(|(_, k)| k.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traits(embedding: Option<&[f32]>, principals: &[&str], turns: &[&str]) -> PageTraits {
        PageTraits {
            embedding: embedding.map(<[f32]>::to_vec),
            principals: principals.iter().map(|s| (*s).to_owned()).collect(),
            turns: turns.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn pool(entries: Vec<(&str, PageTraits)>) -> CandidatePool {
        let entries: BTreeMap<String, PageTraits> = entries
            .into_iter()
            .map(|(k, t)| (k.to_owned(), t))
            .collect();
        let mut carriers: BTreeMap<&str, usize> = BTreeMap::new();
        for t in entries.values() {
            for p in &t.principals {
                *carriers.entry(p.as_str()).or_default() += 1;
            }
        }
        #[expect(clippy::cast_precision_loss, reason = "test mirror of `load`")]
        let bar = entries.len() as f32 * PRINCIPAL_UBIQUITY;
        let ubiquitous = carriers
            .into_iter()
            .filter(|&(_, n)| {
                #[expect(clippy::cast_precision_loss, reason = "test mirror of `load`")]
                let n = n as f32;
                n > bar
            })
            .map(|(p, _)| p.to_owned())
            .collect();
        CandidatePool {
            entries,
            ubiquitous,
            embedded: 0,
        }
    }

    fn excl(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|s| (*s).to_owned()).collect()
    }

    fn sources(picked: &[Candidate]) -> BTreeSet<CandidateSource> {
        picked.iter().map(|c| c.source).collect()
    }

    /// A page one FACT points at, which the page itself points nowhere near.
    ///
    /// This is the whole of asking per fact. The asker's card sits at one end
    /// of the space and the destination at the other, so on the card alone it
    /// ranks below every filler and never reaches the model as `near`. Widen
    /// the ask with the vector of a single fact that DOES sit beside it, and it
    /// ranks first — without displacing anything the card was already offering,
    /// because scoring takes the best across the bag rather than an average.
    #[test]
    fn a_fact_reaches_the_page_its_own_page_is_nowhere_near() {
        let mut entries = vec![
            ("me", traits(Some(&[1.0, 0.0]), &[], &[])),
            // Diametrically opposite the asker's card.
            ("courses", traits(Some(&[-1.0, 0.0]), &[], &[])),
        ];
        let mut owned: Vec<String> = Vec::new();
        for i in 0_u8..30 {
            let step = f32::from(i) / 60.0;
            owned.push(format!("filler{i}"));
            entries.push(("", traits(Some(&[1.0 - step, step]), &[], &[])));
        }
        for (i, name) in owned.iter().enumerate() {
            entries[i + 2].0 = name.as_str();
        }
        let p = pool(entries);
        let exclude = excl(&["me"]);

        let on_the_card = p.pick(&p.ask_for(["me"]), &exclude, SELECTION_PAGES, &[]);
        assert!(
            !on_the_card
                .iter()
                .any(|c| c.key == "courses" && c.source == CandidateSource::Near),
            "asked on the page's own card it is never a near candidate: {on_the_card:?}"
        );

        let mut widened = p.ask_for(["me"]);
        widened.widen_with([vec![-0.99_f32, 0.05]]);
        let with_the_fact = p.pick(&widened, &exclude, SELECTION_PAGES, &[]);
        assert_eq!(
            with_the_fact
                .iter()
                .find(|c| c.key == "courses")
                .map(|c| c.source),
            Some(CandidateSource::Near),
            "the fact's own neighbourhood reaches the model: {with_the_fact:?}"
        );
        assert!(
            with_the_fact.iter().any(|c| c.key == "filler0"),
            "and widening takes nothing away: the card's own nearest is still there"
        );
    }

    /// The whole point: a page nothing resembles still reaches the model,
    /// because two of the four sources never look at a vector and a third
    /// looks for distance.
    #[test]
    fn all_four_sources_are_represented_when_each_has_something_to_give() {
        // 30 filler pages so the far half has room and no principal is
        // ubiquitous. Vectors walk away from the asker one step at a time.
        let mut entries = vec![(
            "me",
            traits(Some(&[1.0, 0.0]), &["user:alice"], &["turn-a"]),
        )];
        let mut owned: Vec<String> = Vec::new();
        for i in 0..30 {
            owned.push(format!("filler{i:02}"));
        }
        #[expect(clippy::cast_precision_loss, reason = "small loop counter")]
        for (i, name) in owned.iter().enumerate() {
            let t = i as f32 / 30.0;
            entries.push((
                name.as_str(),
                traits(Some(&[1.0 - t, t]), &[&format!("user:u{i}")], &[]),
            ));
        }
        // One page shares the asker's principal, one shares its turn, and
        // neither is anywhere near it: without their own sources they could
        // not be picked at all.
        entries.push((
            "same_person",
            traits(Some(&[0.0, 1.0]), &["user:alice"], &[]),
        ));
        entries.push(("same_turn", traits(Some(&[0.0, 1.0]), &[], &["turn-a"])));

        let p = pool(entries);
        let picked = p.pick(&p.ask_for(["me"]), &excl(&["me"]), SELECTION_PAGES, &[]);

        assert_eq!(picked.len(), SELECTION_PAGES, "the budget is filled");
        assert_eq!(
            sources(&picked),
            [
                CandidateSource::Near,
                CandidateSource::People,
                CandidateSource::Turn,
                CandidateSource::Far
            ]
            .into_iter()
            .collect::<BTreeSet<_>>(),
            "every source that had something to give is represented: {picked:?}"
        );
        let by_source = |s: CandidateSource| -> Vec<&str> {
            picked
                .iter()
                .filter(|c| c.source == s)
                .map(|c| c.key.as_str())
                .collect()
        };
        assert!(
            by_source(CandidateSource::People).contains(&"same_person"),
            "the page sharing a principal is offered: {picked:?}"
        );
        assert!(
            by_source(CandidateSource::Turn).contains(&"same_turn"),
            "the page sharing a turn is offered: {picked:?}"
        );
    }

    #[test]
    fn a_page_takes_one_seat_and_the_first_source_claims_it() {
        let p = pool(vec![
            ("me", traits(Some(&[1.0, 0.0]), &["user:alice"], &["t1"])),
            // Nearest AND same turn AND same principal.
            ("both", traits(Some(&[1.0, 0.0]), &["user:alice"], &["t1"])),
            ("other", traits(Some(&[0.0, 1.0]), &["user:bob"], &[])),
        ]);
        let picked = p.pick(&p.ask_for(["me"]), &excl(&["me"]), SELECTION_PAGES, &[]);
        let seats = picked.iter().filter(|c| c.key == "both").count();
        assert_eq!(seats, 1, "one page, one seat: {picked:?}");
        assert_eq!(
            picked
                .iter()
                .find(|c| c.key == "both")
                .map(|c| c.source)
                .unwrap(),
            CandidateSource::Near,
            "the first source to claim it keeps it",
        );
    }

    #[test]
    fn the_same_pool_always_returns_the_same_list() {
        let p = pool(vec![
            ("me", traits(Some(&[1.0, 0.0]), &["user:alice"], &[])),
            ("a", traits(Some(&[0.9, 0.1]), &["user:bob"], &["t"])),
            ("b", traits(Some(&[0.9, 0.1]), &["user:carol"], &["t"])),
            ("c", traits(Some(&[0.1, 0.9]), &["user:dave"], &[])),
        ]);
        let ask = p.ask_for(["me"]);
        let one = p.pick(&ask, &excl(&["me"]), SELECTION_PAGES, &[]);
        let two = p.pick(&ask, &excl(&["me"]), SELECTION_PAGES, &[]);
        assert_eq!(one, two, "two runs over one pool agree");
    }

    /// A principal on nearly every page says nothing about any of them. In a
    /// one-person memory that is the owner, and without this the `same-people`
    /// seats would go to whatever the pool's key order happened to put first.
    #[test]
    fn a_principal_on_nearly_every_page_is_not_a_signal() {
        let p = pool(vec![
            ("me", traits(None, &["user:alice"], &[])),
            ("a", traits(None, &["user:alice"], &[])),
            ("b", traits(None, &["user:alice"], &[])),
            ("c", traits(None, &["user:alice", "user:bob"], &[])),
        ]);
        let picked = p.pick(&p.ask_for(["me"]), &excl(&["me"]), SELECTION_PAGES, &[]);
        assert!(
            picked.is_empty(),
            "`user:alice` is on every page, so it discriminates nothing: {picked:?}"
        );

        // The same pool with one more owner, so nobody is on more than half
        // of it: now the source selects.
        let p = pool(vec![
            ("me", traits(None, &["user:alice"], &[])),
            ("a", traits(None, &["user:alice"], &[])),
            ("b", traits(None, &["user:bob"], &[])),
            ("c", traits(None, &["user:carol"], &[])),
            ("d", traits(None, &["user:dave"], &[])),
        ]);
        let picked = p.pick(&p.ask_for(["me"]), &excl(&["me"]), SELECTION_PAGES, &[]);
        assert_eq!(
            picked,
            vec![Candidate {
                key: "a".to_owned(),
                source: CandidateSource::People
            }],
        );
    }

    /// A wiki asks with every card it owns, and a candidate answers to its
    /// **best** match rather than to their average — a wiki spans unrelated
    /// subjects, and a centroid over them is a point about none of them.
    #[test]
    fn a_multi_page_asker_scores_a_candidate_on_its_best_match() {
        let p = pool(vec![
            ("mine_a", traits(Some(&[1.0, 0.0]), &[], &[])),
            ("mine_b", traits(Some(&[0.0, 1.0]), &[], &[])),
            // Sits on `mine_b`; a centroid of the two would put it at 45°
            // from the asker and rank it below a page near neither.
            ("theirs", traits(Some(&[0.0, 1.0]), &[], &[])),
            ("middling", traits(Some(&[0.7, 0.7]), &[], &[])),
        ]);
        let ask = p.ask_for(["mine_a", "mine_b"]);
        assert!(ask.has_vector());
        let picked = p.pick(&ask, &excl(&["mine_a", "mine_b"]), SELECTION_PAGES, &[]);
        assert_eq!(picked.first().map(|c| c.key.as_str()), Some("theirs"));
    }

    #[test]
    fn an_asker_with_no_card_vector_still_gets_the_two_sources_that_need_none() {
        let p = pool(vec![
            ("me", traits(None, &["user:alice"], &["t1"])),
            ("person", traits(Some(&[1.0, 0.0]), &["user:alice"], &[])),
            ("turn", traits(Some(&[1.0, 0.0]), &[], &["t1"])),
            ("nothing", traits(Some(&[1.0, 0.0]), &["user:zoe"], &[])),
            ("nothing2", traits(Some(&[1.0, 0.0]), &["user:yan"], &[])),
        ]);
        let ask = p.ask_for(["me"]);
        assert!(!ask.has_vector(), "no card vector to rank with");
        let picked = p.pick(&ask, &excl(&["me"]), SELECTION_PAGES, &[]);
        assert_eq!(
            sources(&picked),
            [CandidateSource::People, CandidateSource::Turn]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            "nearness and distance both need a vector; the other two do not: {picked:?}"
        );
    }

    /// The asker having vectors says nothing about whether anything can be
    /// RANKED: scoring needs a vector at both ends, and a memory written
    /// before anything embedded its cards has none at the far end. Keying the
    /// fallback on the ask left the rail writer offering zero candidates on
    /// exactly that memory — every nominated page returned before it reached
    /// the model, silently, because an empty offer is a legal answer.
    #[test]
    fn an_asker_rich_in_vectors_still_falls_back_when_nothing_can_be_ranked() {
        let p = pool(vec![
            ("me", traits(None, &[], &[])),
            ("a", traits(None, &[], &[])),
            ("b", traits(None, &[], &[])),
        ]);
        let mut ask = p.ask_for(["me"]);
        // What `rem::rail_candidates` does: the page's own facts widen the ask.
        ask.widen_with([vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert!(ask.has_vector(), "the ask carries the facts' vectors");
        let home: Vec<String> = ["b", "a"].iter().map(|s| (*s).to_owned()).collect();
        let picked = p.pick(&ask, &excl(&["me"]), SELECTION_PAGES, &home);
        assert_eq!(
            picked.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"],
            "nothing ranks, so the caller's own ordering is offered: {picked:?}"
        );
    }

    /// The vectorless asker is not the rare one — a page created in the run
    /// being compiled has no stored card — so it must still be told what
    /// exists, and the caller's own ordering is the only thing that can.
    #[test]
    fn a_vectorless_asker_is_topped_up_from_the_callers_own_ordering() {
        let p = pool(vec![
            ("me", traits(None, &["user:alice"], &[])),
            ("person", traits(None, &["user:alice"], &[])),
            ("a", traits(None, &["user:bob"], &[])),
            ("b", traits(None, &["user:carol"], &[])),
            ("c", traits(None, &["user:dave"], &[])),
        ]);
        let home: Vec<String> = ["c", "b", "a", "me"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let picked = p.pick(&p.ask_for(["me"]), &excl(&["me"]), SELECTION_PAGES, &home);
        assert_eq!(
            picked,
            vec![
                Candidate {
                    key: "person".to_owned(),
                    source: CandidateSource::People
                },
                Candidate {
                    key: "c".to_owned(),
                    source: CandidateSource::Home
                },
                Candidate {
                    key: "b".to_owned(),
                    source: CandidateSource::Home
                },
                Candidate {
                    key: "a".to_owned(),
                    source: CandidateSource::Home
                },
            ],
            "the caller's order is kept, the asker itself is never offered, \
             and a page a search already found does not take a second seat",
        );
    }

    /// With a vector, the caller's ordering is never consulted: `near` has
    /// already answered the question it exists to answer.
    #[test]
    fn a_ranked_asker_never_falls_back_to_the_callers_ordering() {
        let p = pool(vec![
            ("me", traits(Some(&[1.0, 0.0]), &[], &[])),
            ("a", traits(Some(&[0.9, 0.1]), &[], &[])),
            ("b", traits(Some(&[0.1, 0.9]), &[], &[])),
        ]);
        let home: Vec<String> = vec!["b".to_owned(), "a".to_owned()];
        let picked = p.pick(&p.ask_for(["me"]), &excl(&["me"]), SELECTION_PAGES, &home);
        assert!(
            picked.iter().all(|c| c.source != CandidateSource::Home),
            "{picked:?}"
        );
    }

    #[test]
    fn far_sampling_spreads_across_the_far_half_instead_of_taking_the_tail() {
        let ranked: Vec<(f32, String)> = (0..20)
            .map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "small loop counter")]
                let s = 1.0 - i as f32 / 20.0;
                (s, format!("p{i:02}"))
            })
            .collect();
        let far = far_sample(&ranked, 4);
        assert_eq!(far.len(), 4);
        assert_eq!(far[0], "p19", "the farthest is always offered");
        assert!(
            far.iter().any(|k| k.as_str() < "p17"),
            "and the sample reaches back into the half, not just its tail: {far:?}"
        );
        assert!(
            far.iter().all(|k| k.as_str() >= "p10"),
            "never out of the near half: {far:?}"
        );
    }

    #[test]
    fn an_empty_pool_offers_nothing_rather_than_failing() {
        let p = pool(vec![]);
        assert!(
            p.pick(&Ask::default(), &BTreeSet::new(), SELECTION_PAGES, &[])
                .is_empty()
        );
        assert!(far_sample(&[], 4).is_empty());
    }
}
