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
//! - **asymmetric link** — a link `a → b` in the graph with no `b → a`
//!   back-edge (the Architetto makes the graph symmetric, so any asymmetry is
//!   a regression).
//! - **duplicate prose** — two leaf pages whose stripped bodies share a
//!   char-6-gram Jaccard above [`PROSE_DUP_THRESHOLD`] (the starvation invariant
//!   should make cross-page prose duplication near-zero; hubs are excluded).
//! - **missing ACL marker** — an owned fact (owner ≠ `global`) assigned to a
//!   page whose compiled body carries no `{{… f=<fact_id>}}` marker for it, or
//!   whose marker is public. This is the ACL-leak guard the old engine lacked:
//!   an owned claim rendered as unmarked prose would be readable by everyone.
//! - **cross-subject bloat** — an identity index (a `wiki-user`'s `index.md`;
//!   the agent wiki included) whose plan carries a **foreign-subject** fact:
//!   owner is a different user, or a group the page's user is not a member of
//!   (a group they belong to is their own shared context, never foreign).
//!   Observability for the identity-page discipline the Cartografo prompt
//!   carries — a count in the report/log, never a gate.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::SqlitePool;
use thiserror::Error;

use crate::enrollment;
use crate::parser::{self, ParseEvent};
use crate::planner::{CompilationPlan, PagePlan, PageType};
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
/// missing redistribution leg of two shipped designs — the refile sweep
/// deliberately lands cross-wiki moves on the destination `index.md`
/// expecting "that wiki's own dream re-files them", and a grown-but-clean
/// page otherwise never re-enters the Cartografo at all.
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
/// Carries which wikis are `wiki-user` **identity wikis** (their `index.md`
/// is an identity index — the agent wiki included, it is a normal
/// `wiki-user`) and which groups each of those users belongs to. Group
/// wikis and emergent sub-wikis carry other `wiki_type`s and never qualify.
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
        for d in tree.walk()? {
            if d.meta.wiki_type == IDENTITY_WIKI_TYPE {
                let user = d.meta.wiki_id.as_str().to_owned();
                let groups = enrollment::groups_for(pool, &user).await?;
                ctx.memberships
                    .insert(user.clone(), groups.into_iter().collect());
                ctx.user_wikis.insert(user);
            }
        }
        Ok(ctx)
    }

    /// The mechanical foreign-subject test: a fact is foreign to an identity
    /// page iff its owner is a **different user**, or a **group the page's
    /// user is not a member of**. A group the user belongs to is their own
    /// shared context — never foreign — and the builtin global group has
    /// universal membership.
    fn is_foreign(&self, page_user: &str, owner: &Principal) -> bool {
        match owner {
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
    /// `(from, to)` links missing their back-edge.
    pub asymmetric_links: Vec<(String, String)>,
    /// `(slug_a, slug_b, jaccard)` leaf pairs over the prose-dup threshold.
    pub duplicate_prose: Vec<(String, String, f32)>,
    /// `(slug, fact_id)` owned facts with no matching non-public marker on the
    /// compiled page.
    pub missing_acl_markers: Vec<(String, String)>,
    /// `(slug, fact_id, owner)` foreign-subject facts the plan places on an
    /// identity index (a `wiki-user`'s `index.md`) — the identity-page
    /// discipline violated. Observability only, never a gate.
    pub cross_subject_bloat: Vec<(String, String, String)>,
    /// `(slug, children)` — `concept_leaf` pages other pages parent under.
    /// A leaf functioning as a container is the two-rank topology violated;
    /// parked as a placement re-open so the Cartografo re-homes its facts
    /// (once emptied, the assembly normalises its type to hub).
    pub leaf_with_children: Vec<(String, usize)>,
    /// `(slug, facts)` — hub pages (`concept_hub` / `group_theme`) carrying
    /// facts. Hubs hold no facts; parked as a placement re-open.
    pub hub_with_facts: Vec<(String, usize)>,
    /// `(slug, facts)` — fact-bearing pages at/over
    /// [`OVERSIZED_PAGE_THRESHOLD`]; parked as a placement re-open so
    /// split-by-mass becomes reachable for a clean grown page.
    pub oversized_pages: Vec<(String, usize)>,
}

impl ReviewReport {
    /// Total number of findings across all categories.
    #[must_use]
    pub const fn finding_count(&self) -> usize {
        self.empty_leaves.len()
            + self.duplicate_fact_homes.len()
            + self.asymmetric_links.len()
            + self.duplicate_prose.len()
            + self.missing_acl_markers.len()
            + self.cross_subject_bloat.len()
            + self.leaf_with_children.len()
            + self.hub_with_facts.len()
            + self.oversized_pages.len()
    }

    /// True when there is nothing to report.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.finding_count() == 0
    }
}

/// Review a compiled plan.
///
/// `tree` is used to read the compiled page bodies for the prose-duplication
/// and ACL-marker checks; the plan-level checks need only the plan.
/// `identity` feeds the cross-subject check — pass
/// `IdentityContext::default()` to skip it (callers that consume only the
/// plan-shape findings).
///
/// # Errors
///
/// Only a wiki-tree access error bubbles; missing/unreadable page files are
/// treated as "not compiled yet" and skipped per page.
pub fn review(
    tree: &WikiTree,
    plan: &CompilationPlan,
    identity: &IdentityContext,
) -> Result<ReviewReport> {
    let mut report = ReviewReport::default();

    // --- plan-level checks ---
    let mut homes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (slug, page) in &plan.pages {
        check_page_shape(slug, page, &mut report);
        // Cross-subject bloat: a foreign-subject fact planned onto an
        // identity index. Detection is `_meta`-driven (the wiki is a
        // `wiki-user` and the page is its `index.md`), so topic pages,
        // group wikis, and emergent sub-wiki indexes never qualify.
        let is_identity_index =
            page.page_path == "index.md" && identity.user_wikis.contains(&page.wiki_id);
        for f in &page.primary_facts {
            if is_identity_index && identity.is_foreign(&page.wiki_id, &f.owner) {
                report.cross_subject_bloat.push((
                    slug.clone(),
                    f.fact_id.as_str().to_owned(),
                    f.owner.to_string(),
                ));
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
    for (from, targets) in &plan.link_graph {
        for to in targets {
            let symmetric = plan
                .link_graph
                .get(to)
                .is_some_and(|back| back.contains(from));
            if !symmetric {
                report.asymmetric_links.push((from.clone(), to.clone()));
            }
        }
    }

    // --- page-content checks (read compiled bodies) ---
    // Collect (slug, stripped_body) for leaf pages that have a compiled file.
    let mut leaf_bodies: Vec<(String, String)> = Vec::new();
    for (slug, page) in &plan.pages {
        // Hubs are excluded from prose-dup (overview narrative naturally overlaps);
        // the ACL-marker check is leaf-only too (hubs carry no facts/markers).
        let is_hub = page.primary_facts.is_empty()
            && matches!(page.page_type, PageType::ConceptHub | PageType::GroupTheme);
        if is_hub {
            continue;
        }
        let Some(contents) = read_page(tree, &page.wiki_id, &page.page_path) else {
            continue; // not compiled yet
        };
        let parsed = parser::parse(&contents);

        // missing ACL marker: every owned fact on the page must appear in a
        // non-public marker region.
        let owned_marked: std::collections::BTreeSet<String> = parsed
            .events
            .iter()
            .filter_map(|ev| match ev {
                ParseEvent::Region { attrs, .. } => {
                    let fid = attrs.fact_id.as_ref()?;
                    // A region is public — and so does NOT protect an owned
                    // fact — when the builtin global group appears in its
                    // owner or allow set. (Inheritance, i.e. owner=None, is
                    // treated as non-public: it can resolve to a non-global
                    // default.)
                    let public = attrs.acl.owner.as_ref().is_some_and(Principal::is_global)
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
            if !crate::acl::is_public(&f.owner, &f.allow, f.sender.as_ref())
                && !owned_marked.contains(f.fact_id.as_str())
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
        asymmetric_links = report.asymmetric_links.len(),
        duplicate_prose = report.duplicate_prose.len(),
        missing_acl_markers = report.missing_acl_markers.len(),
        cross_subject_bloat = report.cross_subject_bloat.len(),
        leaf_with_children = report.leaf_with_children.len(),
        hub_with_facts = report.hub_with_facts.len(),
        oversized_pages = report.oversized_pages.len(),
        "reviewer: review done"
    );
    Ok(report)
}

/// The per-page shape checks of the plan-level pass: empty leaf, two-rank
/// topology (leaf-with-children / hub-with-facts), oversized nomination.
fn check_page_shape(slug: &str, page: &PagePlan, report: &mut ReviewReport) {
    if page.page_type == PageType::ConceptLeaf && page.primary_facts.is_empty() {
        report.empty_leaves.push(slug.to_owned());
    }
    // Two-rank topology: leaves hold facts and parent nothing; hubs
    // parent children and hold nothing. (An *empty* leaf with children
    // is normalised to hub by the assembly itself — what reaches this
    // check is the fact-bearing container, which needs its facts
    // re-homed first.)
    if page.page_type == PageType::ConceptLeaf && !page.child_leaves.is_empty() {
        report
            .leaf_with_children
            .push((slug.to_owned(), page.child_leaves.len()));
    }
    if matches!(page.page_type, PageType::ConceptHub | PageType::GroupTheme)
        && !page.primary_facts.is_empty()
    {
        report
            .hub_with_facts
            .push((slug.to_owned(), page.primary_facts.len()));
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
            page_type: PageType::ConceptLeaf,
            owner_scope: None,
            parent_hub: None,
            child_leaves: Vec::new(),
            primary_facts: facts,
            outgoing_links: Vec::new(),
            incoming_links: Vec::new(),
            wiki_id: "alice".to_owned(),
            page_path: format!("{slug}.md"),
        }
    }

    fn ffp(seed: u8, owner: &str) -> FactForPage {
        FactForPage {
            authored_refs: Vec::new(),
            fact_id: fid(seed),
            text: format!("fact {seed}"),
            fact_type: Some("bio".to_owned()),
            owner: owner.parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            source_wiki_id: "alice".to_owned(),
            valid_from: None,
            valid_to: None,
            decay_reason: None,
            successor_fact_id: None,
            target_page: None,
            style: None,
            page_description: None,
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
        let r = review(&tree, &plan, &IdentityContext::default()).unwrap();
        assert_eq!(r.empty_leaves, vec!["c".to_owned()]);
        assert_eq!(r.duplicate_fact_homes.len(), 1);
        assert_eq!(r.duplicate_fact_homes[0].1.len(), 2);
    }

    #[test]
    fn flags_two_rank_topology_violations_and_oversized_pages() {
        // `cucina`: a fact-bearing leaf other pages parent under (container);
        // `hub`: a concept_hub carrying a fact; `pile`: a subject-clean leaf
        // at the oversized nomination threshold. All three park as placement
        // re-opens via the findings→healing bridge.
        let mut cucina = leaf("cucina", vec![ffp(0x40, "user:alice")]);
        cucina.child_leaves = vec!["cucina_tecniche".to_owned()];
        let mut hub = leaf("hub", vec![ffp(0x41, "user:alice")]);
        hub.page_type = PageType::ConceptHub;
        hub.child_leaves = vec!["cucina".to_owned()];
        let pile_facts: Vec<FactForPage> = (0..OVERSIZED_PAGE_THRESHOLD)
            .map(|i| ffp(u8::try_from(i).unwrap(), "user:alice"))
            .collect();
        let pile = leaf("pile", pile_facts);
        let plan = plan_with(
            vec![
                cucina,
                hub,
                pile,
                leaf("cucina_tecniche", vec![ffp(0x42, "user:alice")]),
            ],
            BTreeMap::new(),
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let r = review(&tree, &plan, &IdentityContext::default()).unwrap();
        assert_eq!(
            r.leaf_with_children,
            vec![("cucina".to_owned(), 1)],
            "a fact-bearing leaf with children is the two-rank topology violated"
        );
        assert_eq!(
            r.hub_with_facts,
            vec![("hub".to_owned(), 1)],
            "hubs hold no facts"
        );
        assert_eq!(
            r.oversized_pages,
            vec![("pile".to_owned(), OVERSIZED_PAGE_THRESHOLD)],
            "mass at the threshold nominates the page for a re-open"
        );
        assert!(!r.is_clean());
    }

    #[test]
    fn flags_asymmetric_link() {
        let mut links = BTreeMap::new();
        links.insert("a".to_owned(), vec!["b".to_owned()]);
        links.insert("b".to_owned(), Vec::new()); // missing b->a
        let plan = plan_with(vec![leaf("a", vec![]), leaf("b", vec![])], links);
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let r = review(&tree, &plan, &IdentityContext::default()).unwrap();
        assert!(
            r.asymmetric_links
                .contains(&("a".to_owned(), "b".to_owned()))
        );
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
        let r = review(&tree, &plan, &IdentityContext::default()).unwrap();
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
            format!("---\ntitle: Secrets\n---\n\n{{{{owner=user:alice f={f}}}}}Alice has a private condition.{{{{/}}}}\n"),
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let mut page = leaf("secrets", vec![ffp(2, "user:alice")]);
        page.wiki_id = "alice".to_owned();
        let plan = plan_with(vec![page], BTreeMap::new());
        let r = review(&tree, &plan, &IdentityContext::default()).unwrap();
        assert!(
            r.missing_acl_markers.is_empty(),
            "marked owned fact is clean"
        );
    }

    /// The identity index of `franz` (a `wiki-user`) planned with facts of
    /// every ownership shape: only the foreign SUBJECTS are flagged —
    /// another user's fact and a fact of a group franz is NOT in. His own
    /// facts, a group he belongs to (his own shared context), and global
    /// world context are all clean.
    #[test]
    fn flags_foreign_subject_facts_on_an_identity_index() {
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
        page.page_type = PageType::Person;
        page.wiki_id = "franz".to_owned();
        page.page_path = "index.md".to_owned();
        let plan = plan_with(vec![page], BTreeMap::new());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();

        let r = review(&tree, &plan, &identity).unwrap();
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

    /// The same foreign-subject fact on a TOPIC page (a concept leaf) of the
    /// same wiki is fine — multi-subject detail legitimately lives on topic
    /// pages; only the identity index carries one subject. A `wiki-group`
    /// index never qualifies either (out of the 32a scope).
    #[test]
    fn foreign_fact_on_a_topic_page_or_group_index_is_not_flagged() {
        let mut identity = IdentityContext::default();
        identity.user_wikis.insert("franz".to_owned());
        identity
            .memberships
            .insert("franz".to_owned(), BTreeSet::new());

        // A concept leaf in franz's wiki holding bruno's fact: not an index.
        let mut topic = leaf("dossier", vec![ffp(2, "user:bruno")]);
        topic.wiki_id = "franz".to_owned();
        // The famiglia GROUP wiki's index holding bruno's fact: not a
        // wiki-user wiki (absent from `user_wikis`).
        let mut group_index = leaf("famiglia", vec![ffp(3, "user:bruno")]);
        group_index.page_type = PageType::GroupTheme;
        group_index.wiki_id = "famiglia".to_owned();
        group_index.page_path = "index.md".to_owned();

        let plan = plan_with(vec![topic, group_index], BTreeMap::new());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();

        let r = review(&tree, &plan, &identity).unwrap();
        assert!(
            r.cross_subject_bloat.is_empty(),
            "topic pages and group-wiki indexes are outside the identity discipline"
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
        page.page_type = PageType::Person;
        page.wiki_id = "hermes1".to_owned();
        page.page_path = "index.md".to_owned();
        let plan = plan_with(vec![page], BTreeMap::new());
        let r = review(&tree, &plan, &identity).unwrap();
        assert_eq!(
            r.cross_subject_bloat,
            vec![(
                "hermes1".to_owned(),
                fid(6).as_str().to_owned(),
                "user:franz".to_owned()
            )],
            "a human's fact on the agent's identity index is a foreign subject"
        );
        drop(dir);
    }
}
