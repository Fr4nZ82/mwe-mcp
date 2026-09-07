// SPDX-License-Identifier: AGPL-3.0-or-later
//! Deterministic **reviewer** — zero-LLM post-compilation QA.
//!
//! After the Cronista ([`crate::compiler`]) compiles the dirty pages, the
//! reviewer runs a set of cheap, deterministic invariant checks over the
//! [`CompilationPlan`] and the compiled page bodies and reports any findings.
//! It is **non-blocking**: it never mutates the corpus — it surfaces problems
//! (logged / available to the dashboard) so a maintainer or a later cycle can
//! act. No LLM is involved.
//!
//! Checks:
//! - **empty leaf** — a `concept_leaf` with zero facts (should have been
//!   garbage-collected by the Architetto; a non-empty list signals a planner
//!   bug).
//! - **duplicate fact home** — a `fact_id` appearing on two or more pages
//!   (a one-fact-one-page violation).
//! - **duplicate prose** — two leaf pages whose stripped bodies share a
//!   char-6-gram Jaccard above [`PROSE_DUP_THRESHOLD`] (the starvation invariant
//!   should make cross-page prose duplication near-zero; fact-less pages are
//!   excluded).
//! - **missing ACL marker** — an owned fact (subject ≠ `global`) assigned to a
//!   page whose compiled body carries no `{{… f=<fact_id>}}` marker for it, or
//!   whose marker is public. This is the ACL-leak guard the old engine lacked:
//!   an owned claim rendered as unmarked prose would be readable by everyone.
//! - **cross-subject bloat** — an identity card (a `wiki-user`'s `@profile.md`;
//!   the agent wiki included) whose plan carries a **foreign-subject** fact:
//!   subject is a different user, or a group the page's user is not a member of
//!   (a group they belong to is their own shared context, never foreign).
//!   Observability for the identity-page discipline the Cartografo prompt
//!   carries — a count in the report/log, never a gate.
//! - **spent card fact** — an identity card still carrying a fact that has
//!   stopped holding: the engine closed it (`decay_reason`), or its
//!   `valid_to` is already past. The card is the always-on base context, so
//!   a spent claim there is served as true on every turn.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::SqlitePool;
use thiserror::Error;

use crate::enrollment;
use crate::parser::{self, ParseEvent};
use crate::planner::{CompilationPlan, PagePlan};
use crate::recall;
use crate::types::{Principal, WikiId};
use crate::wiki::{IDENTITY_WIKI_TYPE, WikiError, WikiTree};

/// Cross-page prose Jaccard (char-6-gram) at or above which two leaf pages are
/// flagged as duplicating content.
pub const PROSE_DUP_THRESHOLD: f32 = 0.20;

/// Fact mass at or above which a fact-bearing page is flagged `oversized`.
///
/// A **nomination** for a placement re-open (the Cartografo re-judges the
/// page's placements with the split-by-mass lever live), never a gate: the
/// LLM alone decides whether the page still reads as one page. This is the
/// missing redistribution leg: a grown-but-clean page otherwise never
/// re-enters the Cartografo at all.
pub const OVERSIZED_PAGE_THRESHOLD: usize = 30;

/// Errors raised by the reviewer (only the tree walk can fail hard).
#[derive(Debug, Error)]
pub enum ReviewerError {
    /// Locating a wiki to read a compiled page failed.
    #[error("reviewer wiki: {0}")]
    Wiki(#[from] WikiError),
    /// Enrollment lookup for the identity context failed.
    #[error("reviewer db: {0}")]
    Db(#[from] sqlx::Error),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, ReviewerError>;

/// Enrollment context for the cross-subject check.
///
/// Carries which wikis are `wiki-user` **identity wikis** (their `@profile.md`
/// is an identity card — the agent wiki included, it is a normal
/// `wiki-user`) and which groups each of those users belongs to. Group
/// wikis and topic wikis carry other `wiki_type`s and never qualify.
///
/// Build via [`IdentityContext::load`]; a `Default` (empty) context disables
/// the check — used by callers that consume only the plan-shape findings.
#[derive(Debug, Default, Clone)]
pub struct IdentityContext {
    /// Wiki ids whose `_meta.wiki_type` is `wiki-user` (the wiki id **is**
    /// the user id for an identity wiki).
    pub user_wikis: BTreeSet<String>,
    /// User id → the groups the user belongs to (enrollment membership).
    pub memberships: BTreeMap<String, BTreeSet<String>>,
    /// User id → every name that user answers to: the id itself plus the
    /// aliases the operator declared. Read by the card check to tell a fact
    /// about the card's person from a fact about somebody else.
    pub aliases: BTreeMap<String, BTreeSet<String>>,
}

impl IdentityContext {
    /// Read the `wiki-user` wikis from the tree's `_meta.md` files and each
    /// such user's group memberships from enrollment.
    ///
    /// # Errors
    ///
    /// Tree walk or enrollment lookup failures.
    pub async fn load(pool: &SqlitePool, tree: &WikiTree) -> Result<Self> {
        let mut ctx = Self::default();
        let roster = enrollment::list_users(pool).await?;
        for d in tree.walk()? {
            if d.meta.wiki_type == IDENTITY_WIKI_TYPE {
                let user = d.meta.wiki_id.as_str().to_owned();
                let groups = enrollment::groups_for(pool, &user).await?;
                ctx.memberships
                    .insert(user.clone(), groups.into_iter().collect());
                let mut names: BTreeSet<String> = BTreeSet::new();
                names.insert(user.to_lowercase());
                if let Some(row) = roster.iter().find(|r| r.user_id == user) {
                    names.extend(row.aliases.iter().map(|a| a.to_lowercase()));
                }
                ctx.aliases.insert(user.clone(), names);
                ctx.user_wikis.insert(user);
            }
        }
        Ok(ctx)
    }

    /// The mechanical foreign-subject test: a fact is foreign to an identity
    /// page iff its subject is a **different user**, or a **group the page's
    /// user is not a member of**. A group the user belongs to is their own
    /// shared context — never foreign — and the builtin global group has
    /// universal membership.
    fn is_foreign(&self, page_user: &str, subject: &Principal) -> bool {
        match subject {
            p if p.is_global() => false,
            Principal::User(u) => u != page_user,
            Principal::Group(g) => !self
                .memberships
                .get(page_user)
                .is_some_and(|groups| groups.contains(g)),
        }
    }
}

/// The QA findings. All empty ⇒ the compiled wiki is clean.
#[derive(Debug, Default, Clone)]
pub struct ReviewReport {
    /// `concept_leaf` slugs with zero facts.
    pub empty_leaves: Vec<String>,
    /// `(fact_id, slugs)` for facts homed on more than one page.
    pub duplicate_fact_homes: Vec<(String, Vec<String>)>,
    /// `(slug_a, slug_b, jaccard)` leaf pairs over the prose-dup threshold.
    pub duplicate_prose: Vec<(String, String, f32)>,
    /// `(slug, fact_id)` owned facts with no matching non-public marker on the
    /// compiled page.
    pub missing_acl_markers: Vec<(String, String)>,
    /// `(slug, fact_id, about)` facts the plan places on an identity card (a
    /// `wiki-user`'s `@profile.md`) that are not about the card's person —
    /// the identity-page discipline violated. `about` is whoever they ARE
    /// about: the foreign subject, or the person the sentence names.
    ///
    /// The bridge parks each one as a refile plus a placement re-open, so the
    /// Cartografo judges the card again with its one criterion. Never a silent
    /// drop: the cost of a wrong reading here is one re-judgment.
    pub cross_subject_bloat: Vec<(String, String, String)>,
    /// `(slug, facts)` — fact-bearing pages at/over
    /// [`OVERSIZED_PAGE_THRESHOLD`]; parked as a placement re-open so
    /// split-by-mass becomes reachable for a clean grown page.
    pub oversized_pages: Vec<(String, usize)>,
    /// `(slug, fact_id, why)` facts the plan keeps on an identity card whose
    /// validity has run out — the engine closed them (`decay_reason`), or
    /// their `valid_to` is already behind `now`.
    ///
    /// The card is the always-on base context: what a consumer is handed in
    /// every exchange whatever the topic. A claim that has stopped holding
    /// does not belong in it — «she has a pessary» after it comes out is read
    /// as true on every turn. The bridge parks each one as a placement
    /// re-open, so the Cartografo re-homes it onto a topic page where it
    /// stays readable as history. Never a drop: a spent fact is still what
    /// happened.
    pub spent_card_facts: Vec<(String, String, String)>,
}

impl ReviewReport {
    /// Total number of findings across all categories.
    #[must_use]
    pub const fn finding_count(&self) -> usize {
        self.empty_leaves.len()
            + self.duplicate_fact_homes.len()
            + self.duplicate_prose.len()
            + self.missing_acl_markers.len()
            + self.cross_subject_bloat.len()
            + self.oversized_pages.len()
            + self.spent_card_facts.len()
    }

    /// True when there is nothing to report.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.finding_count() == 0
    }
}

/// Why a card fact has stopped holding, or `None` while it still holds.
///
/// `now` is an ISO-8601 UTC instant and `valid_to` is compared to it
/// lexically, the way every timestamp in this engine is compared.
fn spent_reason(f: &crate::planner::FactForPage, now: &str) -> Option<String> {
    if let Some(reason) = f
        .decay_reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        return Some(reason.to_owned());
    }
    let ends = f
        .valid_to
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())?;
    (ends < now).then(|| format!("valid_to {ends}"))
}

/// Every name this memory holds for somebody who is not a principal, taken
/// from the plan itself: the `subject_external` any fact declares.
///
/// Read from the plan rather than the database because the plan is what is
/// being judged and it already carries them — no query, no ACL question, and
/// the set is exactly the people this compilation knows by name.
fn named_non_principals(plan: &CompilationPlan) -> BTreeSet<String> {
    plan.pages
        .values()
        .flat_map(|p| p.primary_facts.iter())
        .filter_map(|f| f.subject_external.as_deref())
        .map(str::trim)
        .filter(|n| n.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect()
}

/// `haystack` names `needle` as a word, not as a fragment of a longer one.
fn names(haystack: &str, needle: &str) -> bool {
    let edge = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    let mut from = 0;
    while let Some(hit) = haystack[from..].find(needle) {
        let at = from + hit;
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + needle.len()..].chars().next();
        if edge(before) && edge(after) {
            return true;
        }
        from = at + needle.len();
    }
    false
}

/// A card fact that is about SOMEBODY ELSE, said without the mark that says so.
///
/// `subject_external` is the mark, and the placement fence refuses any fact
/// that carries it — but a fact filed without it reaches a card looking like
/// an ordinary claim about the card's person, and once there nothing revisits
/// it. So the sentence is read: it names a person the memory knows by name and
/// does not name the person whose card this is.
///
/// Naming BOTH is the one crossing a card is for — «coordinates her father's
/// care» is the user's own fact and belongs there, which is why the second
/// half of the test is not optional.
fn about_someone_else(
    text: &str,
    others: &BTreeSet<String>,
    own: &BTreeSet<String>,
) -> Option<String> {
    let lower = text.to_lowercase();
    if own.iter().any(|n| names(&lower, n)) {
        return None;
    }
    others.iter().find(|n| names(&lower, n)).cloned()
}

/// Review a compiled plan.
///
/// `tree` is used to read the compiled page bodies for the prose-duplication
/// and ACL-marker checks; the plan-level checks need only the plan.
/// `identity` feeds the cross-subject check — pass
/// `IdentityContext::default()` to skip it (callers that consume only the
/// plan-shape findings). `now` is the ISO-8601 UTC instant a card fact's
/// `valid_to` is judged against.
///
/// # Errors
///
/// Only a wiki-tree access error bubbles; missing/unreadable page files are
/// treated as "not compiled yet" and skipped per page.
pub fn review(
    tree: &WikiTree,
    plan: &CompilationPlan,
    identity: &IdentityContext,
    now: &str,
) -> Result<ReviewReport> {
    let mut report = ReviewReport::default();
    let named_non_principals_here = named_non_principals(plan);

    // --- plan-level checks ---
    let mut homes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (slug, page) in &plan.pages {
        check_page_shape(slug, page, &mut report);
        // Cross-subject bloat: a foreign-subject fact planned onto an
        // identity **card**. Detection is `_meta`-driven (the wiki is a
        // `wiki-user` and the page is its card), so topic pages, group wikis
        // and buffers never qualify.
        //
        // ⚠️ It must key on the card's real file name. A guard keyed on a
        // name no plan node can claim matches nothing and reports nothing, and
        // an empty guard looks exactly like a clean corpus.
        let is_identity_card = page.page_path == crate::wiki::PROFILE_FILENAME
            && identity.user_wikis.contains(&page.wiki_id);
        let own_names = identity.aliases.get(&page.wiki_id);
        for f in &page.primary_facts {
            if is_identity_card {
                // Two ways a fact turns out not to be about the card's person,
                // and the subject line only catches the first.
                let foreign = identity
                    .is_foreign(&page.wiki_id, &f.subject)
                    .then(|| f.subject.to_string())
                    .or_else(|| {
                        own_names.and_then(|own| {
                            about_someone_else(&f.text, &named_non_principals_here, own)
                        })
                    });
                if let Some(about) = foreign {
                    report.cross_subject_bloat.push((
                        slug.clone(),
                        f.fact_id.as_str().to_owned(),
                        about,
                    ));
                }
                // A card fact whose validity has run out. Two ways it runs
                // out and the `valid_to` column only records the second: the
                // engine closes a fact by writing `decay_reason` (superseded,
                // retracted, contradicted, completed) without necessarily
                // stamping an end, and a fact given a horizon at capture
                // simply reaches it.
                if let Some(why) = spent_reason(f, now) {
                    report.spent_card_facts.push((
                        slug.clone(),
                        f.fact_id.as_str().to_owned(),
                        why,
                    ));
                }
            }
            homes
                .entry(f.fact_id.as_str().to_owned())
                .or_default()
                .push(slug.clone());
        }
    }
    for (fid, slugs) in homes {
        if slugs.len() > 1 {
            report.duplicate_fact_homes.push((fid, slugs));
        }
    }
    // --- page-content checks (read compiled bodies) ---
    // Collect (slug, stripped_body) for leaf pages that have a compiled file.
    let mut leaf_bodies: Vec<(String, String)> = Vec::new();
    for (slug, page) in &plan.pages {
        let Some(contents) = read_page(tree, &page.wiki_id, &page.page_path) else {
            continue; // not compiled yet
        };
        let parsed = parser::parse(&contents);

        // missing ACL marker: every owned fact on the page must appear in a
        // non-public marker region.
        let subject_marked: std::collections::BTreeSet<String> = parsed
            .events
            .iter()
            .filter_map(|ev| match ev {
                ParseEvent::Region { attrs, .. } => {
                    let fid = attrs.fact_id.as_ref()?;
                    // A region is public — and so does NOT protect an owned
                    // fact — when the builtin global group appears in its
                    // subject or allow set. (Inheritance, i.e. subject=None, is
                    // treated as non-public: it can resolve to a non-global
                    // default.)
                    let public = attrs.acl.subject.as_ref().is_some_and(Principal::is_global)
                        || attrs.acl.allow.iter().any(Principal::is_global);
                    if public {
                        None
                    } else {
                        Some(fid.as_str().to_owned())
                    }
                },
                _ => None,
            })
            .collect();
        for f in &page.primary_facts {
            if !crate::acl::is_public(&f.subject, &f.allow, f.sender.as_ref())
                && !subject_marked.contains(f.fact_id.as_str())
            {
                report
                    .missing_acl_markers
                    .push((slug.clone(), f.fact_id.as_str().to_owned()));
            }
        }

        leaf_bodies.push((slug.clone(), stripped_body(&parsed)));
    }

    // duplicate prose: pairwise char-6-gram Jaccard over leaf bodies.
    for i in 0..leaf_bodies.len() {
        for j in (i + 1)..leaf_bodies.len() {
            let score = recall::jaccard_6gram(&leaf_bodies[i].1, &leaf_bodies[j].1);
            if score >= PROSE_DUP_THRESHOLD {
                report.duplicate_prose.push((
                    leaf_bodies[i].0.clone(),
                    leaf_bodies[j].0.clone(),
                    score,
                ));
            }
        }
    }

    tracing::info!(
        empty_leaves = report.empty_leaves.len(),
        duplicate_fact_homes = report.duplicate_fact_homes.len(),
        duplicate_prose = report.duplicate_prose.len(),
        missing_acl_markers = report.missing_acl_markers.len(),
        cross_subject_bloat = report.cross_subject_bloat.len(),
        oversized_pages = report.oversized_pages.len(),
        "reviewer: review done"
    );
    Ok(report)
}

/// The per-page shape checks of the plan-level pass: empty page, oversized
/// nomination.
fn check_page_shape(slug: &str, page: &PagePlan, report: &mut ReviewReport) {
    if !page.is_identity_card() && page.primary_facts.is_empty() {
        report.empty_leaves.push(slug.to_owned());
    }
    // Oversized nomination: mass alone re-opens nothing today, so a
    // clean grown page could never split (see the const's doc).
    if page.primary_facts.len() >= OVERSIZED_PAGE_THRESHOLD {
        report
            .oversized_pages
            .push((slug.to_owned(), page.primary_facts.len()));
    }
}

fn read_page(tree: &WikiTree, wiki_id: &str, page_path: &str) -> Option<String> {
    let id = WikiId::parse(wiki_id).ok()?;
    let handle = tree.locate(&id).ok()?;
    handle.read_page(std::path::Path::new(page_path)).ok()
}

/// The readable text of a page: prose + region bodies, marker syntax removed —
/// the surface the cross-page duplicate check compares.
fn stripped_body(parsed: &parser::ParseOutput) -> String {
    let mut out = String::new();
    for ev in &parsed.events {
        match ev {
            ParseEvent::Prose { text, .. } => out.push_str(text),
            // Region bodies may carry in-body `{{embed=…}}` markers —
            // strip them like the standalone Embed events below, so the
            // duplicate-prose comparison sees only the words.
            ParseEvent::Region { body, .. } => {
                out.push_str(&parser::strip_embed_markers(body));
            },
            ParseEvent::Embed { .. } => {},
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// The instant the reviewer's tests judge a fact's `valid_to` against.
    /// Later than every fixture window so a closed one reads as closed.
    const REVIEW_NOW: &str = "2026-09-01T00:00:00Z";

    use super::*;
    use crate::planner::{FactForPage, PagePlan};
    use crate::types::FactId;

    fn fid(seed: u8) -> FactId {
        FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{seed:02x}")).unwrap()
    }

    fn leaf(slug: &str, facts: Vec<FactForPage>) -> PagePlan {
        PagePlan {
            slug: slug.to_owned(),
            title: slug.to_owned(),
            description: slug.to_owned(),
            style: None,
            primary_facts: facts,
            outgoing_links: Vec::new(),
            pending_links: Vec::new(),
            wiki_id: "alice".to_owned(),
            page_path: format!("{slug}.md"),
        }
    }

    fn ffp(seed: u8, subject: &str) -> FactForPage {
        FactForPage {
            topics: Vec::new(),
            subject_external: None,
            authored_refs: Vec::new(),
            fact_id: fid(seed),
            text: format!("fact {seed}"),
            fact_type: Some("bio".to_owned()),
            subject: subject.parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            source_wiki_id: "alice".to_owned(),
            valid_from: None,
            valid_to: None,
            decay_reason: None,
            successor_fact_id: None,
            target_page: None,
            style: None,
            salience: None,
        }
    }

    fn plan_with(pages: Vec<PagePlan>, links: BTreeMap<String, Vec<String>>) -> CompilationPlan {
        let map = pages.into_iter().map(|p| (p.slug.clone(), p)).collect();
        CompilationPlan {
            pages: map,
            merged_pages: Vec::new(),
            link_graph: links,
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        }
    }

    #[test]
    fn flags_empty_leaf_and_duplicate_fact_home() {
        // fact 1 homed on BOTH a and b (violation); page c empty.
        let plan = plan_with(
            vec![
                leaf("a", vec![ffp(1, "user:alice")]),
                leaf("b", vec![ffp(1, "user:alice")]),
                leaf("c", vec![]),
            ],
            BTreeMap::new(),
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let r = review(&tree, &plan, &IdentityContext::default(), REVIEW_NOW).unwrap();
        assert_eq!(r.empty_leaves, vec!["c".to_owned()]);
        assert_eq!(r.duplicate_fact_homes.len(), 1);
        assert_eq!(r.duplicate_fact_homes[0].1.len(), 2);
    }

    #[test]
    fn flags_two_rank_topology_violations_and_oversized_pages() {
        // `cucina`: a fact-bearing page other pages hang under (container);
        // `pile`: a subject-clean page at the oversized nomination threshold.
        // Both park as placement re-opens via the findings→healing bridge.
        //
        let cucina = leaf("cucina", vec![ffp(0x40, "user:alice")]);
        let pile_facts: Vec<FactForPage> = (0..OVERSIZED_PAGE_THRESHOLD)
            .map(|i| ffp(u8::try_from(i).unwrap(), "user:alice"))
            .collect();
        let pile = leaf("pile", pile_facts);
        let plan = plan_with(
            vec![
                cucina,
                pile,
                leaf("cucina_tecniche", vec![ffp(0x42, "user:alice")]),
            ],
            BTreeMap::new(),
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let r = review(&tree, &plan, &IdentityContext::default(), REVIEW_NOW).unwrap();
        assert_eq!(
            r.oversized_pages,
            vec![("pile".to_owned(), OVERSIZED_PAGE_THRESHOLD)],
            "mass at the threshold nominates the page for a re-open"
        );
        assert!(!r.is_clean());
    }

    /// **A one-way link is not a defect** (founder, 2026-08-23: *«la
    /// reciprocità non serve … può capitare ma non è obbligatoria»*). The
    /// review must not report one, because a page that links somewhere puts
    /// no obligation on the page it links to.
    #[test]
    fn a_one_way_link_is_not_a_finding() {
        let mut links = BTreeMap::new();
        links.insert("a".to_owned(), vec!["b".to_owned()]);
        links.insert("b".to_owned(), Vec::new());
        let plan = plan_with(
            vec![
                leaf("a", vec![ffp(1, "user:alice")]),
                leaf("b", vec![ffp(2, "user:alice")]),
            ],
            links,
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let r = review(&tree, &plan, &IdentityContext::default(), REVIEW_NOW).unwrap();
        assert!(r.is_clean(), "{r:?}");
    }

    #[test]
    fn flags_owned_fact_without_marker_on_compiled_page() {
        let dir = tempfile::tempdir().unwrap();
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("alice")).unwrap();
        std::fs::write(
            wikis.join("alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        // Compiled page: the owned fact's claim is UNMARKED prose (a leak).
        std::fs::write(
            wikis.join("alice/secrets.md"),
            "---\ntitle: Secrets\n---\n\nAlice has a private medical condition.\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let mut page = leaf("secrets", vec![ffp(2, "user:alice")]);
        page.wiki_id = "alice".to_owned();
        let plan = plan_with(vec![page], BTreeMap::new());
        let r = review(&tree, &plan, &IdentityContext::default(), REVIEW_NOW).unwrap();
        assert_eq!(
            r.missing_acl_markers,
            vec![("secrets".to_owned(), fid(2).as_str().to_owned())],
            "an owned fact with no marker on the page is an ACL leak"
        );
    }

    #[test]
    fn owned_fact_with_marker_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("alice")).unwrap();
        std::fs::write(
            wikis.join("alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        let f = fid(2);
        std::fs::write(
            wikis.join("alice/secrets.md"),
            format!("---\ntitle: Secrets\n---\n\n{{{{subject=user:alice f={f}}}}}Alice has a private condition.{{{{/}}}}\n"),
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let mut page = leaf("secrets", vec![ffp(2, "user:alice")]);
        page.wiki_id = "alice".to_owned();
        let plan = plan_with(vec![page], BTreeMap::new());
        let r = review(&tree, &plan, &IdentityContext::default(), REVIEW_NOW).unwrap();
        assert!(
            r.missing_acl_markers.is_empty(),
            "marked owned fact is clean"
        );
    }

    /// The identity card of `franz` (a `wiki-user`) planned with facts of
    /// every subject shape: only the foreign SUBJECTS are flagged —
    /// another user's fact and a fact of a group franz is NOT in. His own
    /// facts, a group he belongs to (his own shared context), and global
    /// world context are all clean.
    #[test]
    fn flags_foreign_subject_facts_on_an_identity_card() {
        let mut identity = IdentityContext::default();
        identity.user_wikis.insert("franz".to_owned());
        identity.memberships.insert(
            "franz".to_owned(),
            std::iter::once("famiglia".to_owned()).collect(),
        );

        let mut page = leaf(
            "franz",
            vec![
                ffp(1, "user:franz"),       // own fact — clean.
                ffp(2, "user:bruno"),       // another user — foreign.
                ffp(3, "group:famiglia"),   // franz IS a member — clean.
                ffp(4, "group:condominio"), // franz is NOT a member — foreign.
                ffp(5, "global"),           // world context — clean.
            ],
        );
        page.wiki_id = "franz".to_owned();
        page.page_path = crate::wiki::PROFILE_FILENAME.to_owned();
        let plan = plan_with(vec![page], BTreeMap::new());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();

        let r = review(&tree, &plan, &identity, REVIEW_NOW).unwrap();
        let flagged: Vec<String> = r
            .cross_subject_bloat
            .iter()
            .map(|(_, id, _)| id.clone())
            .collect();
        assert_eq!(
            flagged,
            vec![fid(2).as_str().to_owned(), fid(4).as_str().to_owned()],
            "exactly the foreign-user and foreign-group facts are flagged"
        );
        assert!(
            r.cross_subject_bloat
                .iter()
                .all(|(slug, _, _)| slug == "franz")
        );
        assert_eq!(r.cross_subject_bloat[0].2, "user:bruno");
        assert_eq!(r.cross_subject_bloat[1].2, "group:condominio");
    }

    /// A card fact that has stopped holding must be nominated to leave the
    /// card, whichever way it stopped.
    ///
    /// The card is what a consumer is handed on EVERY turn, whatever the
    /// topic. Nothing else re-opens a card that is neither too long nor
    /// carrying somebody else's fact — so «she has a pessary», once it comes
    /// out, is served as current until something moves it. Both closures
    /// count: the engine writing `decay_reason` when a later turn supersedes
    /// the claim, and a horizon set at capture simply being reached.
    ///
    /// A fact still in force stays, `valid_to` or not: a window that has not
    /// closed is not a closed window.
    #[test]
    fn flags_a_card_fact_that_has_stopped_holding() {
        let mut identity = IdentityContext::default();
        identity.user_wikis.insert("frodo".to_owned());

        let mut removed = ffp(1, "user:frodo");
        removed.text = "Frodo wears a wrist brace.".to_owned();
        removed.decay_reason = Some("superseded".to_owned());

        let mut expired = ffp(2, "user:frodo");
        expired.text = "Side effects of the 24 June jab: headache, aching joints.".to_owned();
        expired.valid_to = Some("2026-06-27T23:59:59Z".to_owned());

        let mut standing = ffp(3, "user:frodo");
        standing.text = "Coeliac: cannot eat gluten.".to_owned();

        // In force, and dated: the horizon is ahead of REVIEW_NOW.
        let mut ahead = ffp(4, "user:frodo");
        ahead.text = "On call until December.".to_owned();
        ahead.valid_to = Some("2026-12-31T23:59:59Z".to_owned());

        let mut card = leaf("frodo", vec![removed, expired, standing, ahead]);
        card.wiki_id = "frodo".to_owned();
        card.page_path = crate::wiki::PROFILE_FILENAME.to_owned();
        let plan = plan_with(vec![card], BTreeMap::new());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();

        let r = review(&tree, &plan, &identity, REVIEW_NOW).unwrap();
        let flagged: Vec<(String, String)> = r
            .spent_card_facts
            .iter()
            .map(|(_, id, why)| (id.clone(), why.clone()))
            .collect();
        assert_eq!(
            flagged,
            vec![
                (fid(1).as_str().to_owned(), "superseded".to_owned()),
                (
                    fid(2).as_str().to_owned(),
                    "valid_to 2026-06-27T23:59:59Z".to_owned()
                ),
            ],
            "the closed one and the expired one, and neither of the two in force"
        );
    }

    /// A fact whose SUBJECT says the card's person but whose SENTENCE is about
    /// somebody else — the shape that reaches a card and stays there.
    ///
    /// `subject_external` is the mark for "this is about a person the system
    /// has no principal for", and the placement fence refuses anything that
    /// carries it. A fact filed WITHOUT it looks ordinary at every door: the
    /// fence sees no mark, the subject line says the card's own user, and no
    /// nightly pass re-opens a card that is not too long. So an uncle's
    /// potassium sits on his nephew's always-on card, in every conversation.
    ///
    /// Naming BOTH people is the crossing a card is FOR — the user's own fact
    /// about their tie to somebody — and stays clean.
    #[test]
    fn flags_a_card_fact_whose_sentence_is_about_somebody_else() {
        let mut identity = IdentityContext::default();
        identity.user_wikis.insert("frodo".to_owned());
        identity.aliases.insert(
            "frodo".to_owned(),
            ["frodo", "frodo baggins"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );

        let mut uncle = ffp(1, "user:frodo");
        uncle.text = "Bilbo Baggins has a critically high potassium level.".to_owned();
        let mut tie = ffp(2, "user:frodo");
        tie.text = "Frodo coordinates the care of his uncle Bilbo Baggins.".to_owned();
        let mut own = ffp(3, "user:frodo");
        own.text = "Back surgery: cannot lift weights.".to_owned();

        // Somewhere in the same plan, a fact that DOES carry the mark: it is
        // what tells the memory this name belongs to a person of its own.
        let mut marked = ffp(4, "user:frodo");
        marked.text = "Bilbo Baggins is in hospital.".to_owned();
        marked.subject_external = Some("Bilbo Baggins".to_owned());
        let elsewhere = leaf("frodo/bilbo_health", vec![marked]);

        let mut card = leaf("frodo", vec![uncle, tie, own]);
        card.wiki_id = "frodo".to_owned();
        card.page_path = crate::wiki::PROFILE_FILENAME.to_owned();
        let plan = plan_with(vec![card, elsewhere], BTreeMap::new());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();

        let r = review(&tree, &plan, &identity, REVIEW_NOW).unwrap();
        let flagged: Vec<(String, String)> = r
            .cross_subject_bloat
            .iter()
            .map(|(_, id, about)| (id.clone(), about.clone()))
            .collect();
        assert_eq!(
            flagged,
            vec![(fid(1).as_str().to_owned(), "bilbo baggins".to_owned())],
            "the uncle's condition is about the uncle; the tie and the \
             user's own limitation are about the user"
        );
    }

    /// The same foreign-subject fact on a TOPIC page (a concept leaf) of the
    /// same wiki is fine — multi-subject detail legitimately lives on topic
    /// pages; only an identity card carries one subject.
    ///
    /// And the discipline keys on WHOSE wiki it is, not on a page path. The
    /// second fixture proves it by parking the fact at `@profile.md` inside a
    /// GROUP's wiki — a page nothing in the engine mints, since a group has
    /// no card, and one the reviewer must still leave alone if it meets it.
    #[test]
    fn foreign_fact_on_a_topic_page_or_in_a_group_wiki_is_not_flagged() {
        let mut identity = IdentityContext::default();
        identity.user_wikis.insert("franz".to_owned());
        identity
            .memberships
            .insert("franz".to_owned(), BTreeSet::new());

        // A concept leaf in franz's wiki holding bruno's fact: not an index.
        let mut topic = leaf("dossier", vec![ffp(2, "user:bruno")]);
        topic.wiki_id = "franz".to_owned();
        // A page of the famiglia GROUP wiki holding bruno's fact, at the
        // card's own path: not a wiki-user wiki (absent from `user_wikis`).
        let mut group_page = leaf("famiglia", vec![ffp(3, "user:bruno")]);
        group_page.wiki_id = "famiglia".to_owned();
        group_page.page_path = crate::wiki::PROFILE_FILENAME.to_owned();

        let plan = plan_with(vec![topic, group_page], BTreeMap::new());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();

        let r = review(&tree, &plan, &identity, REVIEW_NOW).unwrap();
        assert!(
            r.cross_subject_bloat.is_empty(),
            "a topic page and a group's wiki are both outside the identity discipline"
        );
    }

    /// `IdentityContext::load` reads the `wiki-user` set from `_meta.md` and
    /// the memberships from enrollment — and the agent wiki (a normal
    /// `wiki-user` with `is_agent: true`) behaves as an identity page: a
    /// human user's fact planned onto the agent's index is flagged.
    #[tokio::test]
    async fn agent_wiki_index_behaves_as_an_identity_page() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        // The agent's own identity wiki — wiki-user, stamped is_agent.
        std::fs::create_dir_all(wikis.join("hermes1")).unwrap();
        std::fs::write(
            wikis.join("hermes1/_meta.md"),
            "---\nwiki_id: hermes1\nwiki_type: wiki-user\nslug: hermes1\ntitle: Hermes\nacl_default: 'user:hermes1'\nis_agent: true\n---\n",
        )
        .unwrap();
        // A group wiki, to pin that only wiki-user wikis enter the context.
        std::fs::create_dir_all(wikis.join("famiglia")).unwrap();
        std::fs::write(
            wikis.join("famiglia/_meta.md"),
            "---\nwiki_id: famiglia\nwiki_type: wiki-group\nslug: famiglia\ntitle: Famiglia\nacl_default: 'group:famiglia'\n---\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();

        let identity = IdentityContext::load(&pool, &tree).await.expect("load");
        assert!(identity.user_wikis.contains("hermes1"));
        assert!(
            !identity.user_wikis.contains("famiglia"),
            "a wiki-group wiki is never an identity wiki"
        );

        let mut page = leaf("hermes1", vec![ffp(6, "user:franz")]);
        page.wiki_id = "hermes1".to_owned();
        page.page_path = crate::wiki::PROFILE_FILENAME.to_owned();
        let plan = plan_with(vec![page], BTreeMap::new());
        let r = review(&tree, &plan, &identity, REVIEW_NOW).unwrap();
        assert_eq!(
            r.cross_subject_bloat,
            vec![(
                "hermes1".to_owned(),
                fid(6).as_str().to_owned(),
                "user:franz".to_owned()
            )],
            "a human's fact on the agent's identity card is a foreign subject"
        );
        drop(dir);
    }
}
