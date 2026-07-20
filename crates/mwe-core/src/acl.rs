// SPDX-License-Identifier: AGPL-3.0-or-later
//! Region-level access control.
//!
//! Pure function: takes a region's [`Acl`], the sender's identity, the
//! sender's groups, and (optionally) the principal that originally
//! captured the region, and decides whether the sender can read it.
//! Symmetrical for every user — `isAdmin` does **not** bypass,
//! that flag is a UI hint only.
//!
//! The algorithm reduces to a single rule: build the effective principal
//! set `acl.owner ∪ acl.allow ∪ {sender_of_region}` and check whether
//! any principal in it matches the current reader. The cross-user
//! attribution shortcut ("the principal that captured the region always
//! rereads it") is just the third element of that union.
//!
//! The three are independent axes, not synonyms: `owner` is the fact's
//! **subject** (who it is *about*), `allow` is the **audience** (who else
//! may read), and `sender_of_region` is the **provenance** (who captured
//! it). `owner` is named for the data-subject-governs-their-fact model
//! (see the engineering wiki `concepts/identity-and-acl.md`), never for
//! authorship or visibility — both of which live on the other two axes.
//!
//! `sender_of_region` is a [`Principal`], not just a user id — it can be
//! [`Principal::User`] (Galadriel captured a fact about Gollum on
//! behalf of herself), a specific [`Principal::Group`] (the family
//! microphone captured a household-attributed fact), or the builtin
//! `global` group (a public capture device — edge case, but the data
//! model is deliberately symmetric).

use std::collections::HashMap;

use crate::types::{Acl, FactId, Principal};

/// Authoritative ACL of one region, as stored in the engine DB
/// (`fact_index.owner_id` / `allow_ids` / `sender_id`).
///
/// Unlike the inline-marker [`Acl`], the owner is never optional here —
/// every `fact_index` row carries an explicit owner, so a DB-resolved
/// region never inherits `acl_default`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionAcl {
    /// The fact's **subject** — who/what it is *about* (always explicit in
    /// the DB; never the author `sender` or the audience `allow`).
    pub owner: Principal,
    /// `allow=` extension list (possibly empty).
    pub allow: Vec<Principal>,
    /// Cross-user attribution; `None` ⇒ sender equals owner.
    pub sender: Option<Principal>,
}

/// Fact-key → ACL map for the regions of one page, loaded from the
/// engine DB by [`crate::fact_index::page_acl_map`].
///
/// [`crate::render::render_for_sender`] resolves a region's ACL from
/// this map **first**; the inline marker attributes are only the
/// fallback for regions the DB does not know (a file not yet indexed,
/// or a marker without `f=`). An empty map therefore reproduces the
/// pure inline-attribute behaviour.
pub type FactAclMap = HashMap<FactId, RegionAcl>;

/// Decide whether `sender_id` (member of `sender_groups`) can read a region
/// with the given `region_acl`.
///
/// `sender_of_region` is the principal that originally captured the
/// region. It is `None` when the caller has no attribution info to pass;
/// when `Some(principal)`, the principal is added to the effective ACL
/// for this check — so a region captured by `group:famiglia` is
/// readable by every member of `famiglia` even if `famiglia` is not in
/// `region_acl.allow`.
///
/// `region_acl.owner` may be `None` — that means "no explicit owner in the
/// marker, region inherits `acl_default` from `_meta.md`". In that case the
/// caller (typically [`crate::render::render_for_sender`]) is responsible
/// for substituting the inherited owner *before* calling this function;
/// passing an `Acl { owner: None, allow: [] }` here will deny everyone
/// except a matching `sender_of_region`.
#[must_use]
pub fn can_read(
    region_acl: &Acl,
    sender_id: &str,
    sender_groups: &[String],
    sender_of_region: Option<&Principal>,
) -> bool {
    // Effective principal set: owner ∪ allow ∪ {sender_of_region}.
    let owner_iter = region_acl.owner.iter();
    let allow_iter = region_acl.allow.iter();
    let sender_iter = sender_of_region.into_iter();
    for principal in owner_iter.chain(allow_iter).chain(sender_iter) {
        if principal_matches(principal, sender_id, sender_groups) {
            return true;
        }
    }
    false
}

/// Whether `caller` may **delete or edit** the fact directly.
///
/// The write-authority model for `delete` / `edit` / `validity_edit`
/// ([identity and ACL](../../../docs/concepts/identity-and-acl.md)):
/// only the fact's **sender** (its author / provenance) acts on their own
/// contribution directly; an admin may act on any fact. A non-sender (even a
/// non-sender `owner`) is refused here — their path is a request → vote (a
/// later step).
///
/// The `sender` is always a human author in practice, so only
/// [`Principal::User`] grants the direct right; a `Group` (or `global`)
/// sender, or no recorded sender at all, leaves the admin as the sole actor.
#[must_use]
pub fn can_delete(sender_of_fact: Option<&Principal>, caller: &str, is_admin: bool) -> bool {
    is_admin || matches!(sender_of_fact, Some(Principal::User(s)) if s == caller)
}

/// Enumerate the **read audience** of a fact as a sorted, deduplicated list of
/// bare **user ids**.
///
/// This is the finite electorate a non-sender owner's forget request is put to
/// ([the write-authority model](../../../docs/concepts/identity-and-acl.md)).
/// The audience is the same effective read-set [`can_read`] checks,
/// `owner ∪ allow ∪ {sender}`, but resolved to concrete humans: each
/// [`Principal::Group`] is expanded to its members via
/// [`crate::enrollment::members_for`], and the builtin `global` group is
/// **dropped** (a public fact has no finite electorate — there is nobody to
/// poll). A `None` sender contributes nothing. The result is sorted and
/// deduped so the caller can compare / store it deterministically.
///
/// # Errors
///
/// Propagates the underlying `sqlx` error from the group-member lookups.
pub async fn audience(
    pool: &sqlx::SqlitePool,
    owner: &Principal,
    allow: &[Principal],
    sender: Option<&Principal>,
) -> Result<Vec<String>, sqlx::Error> {
    use std::collections::BTreeSet;
    let mut users: BTreeSet<String> = BTreeSet::new();
    for principal in std::iter::once(owner).chain(allow.iter()).chain(sender) {
        match principal {
            Principal::User(id) => {
                users.insert(id.clone());
            },
            // `global` is the group of everyone: an infinite electorate, so a
            // public fact has nobody finite to vote — drop it entirely.
            Principal::Group(id) if id == "global" => {},
            Principal::Group(id) => {
                for member in crate::enrollment::members_for(pool, id).await? {
                    users.insert(member);
                }
            },
        }
    }
    Ok(users.into_iter().collect())
}

pub(crate) fn principal_matches(p: &Principal, sender_id: &str, sender_groups: &[String]) -> bool {
    match p {
        Principal::User(id) => id == sender_id,
        // The builtin `global` group has universal membership: everyone is
        // a member, so naming it grants read to anyone. This is a property
        // of the model (global = the group of all), not a semantic gate —
        // it holds regardless of what `sender_groups` the caller resolved.
        Principal::Group(id) if id == "global" => true,
        Principal::Group(id) => sender_groups.iter().any(|g| g == id),
    }
}

/// Whether a region is public — readable by anyone.
///
/// True when the builtin `global` group appears anywhere in the region's
/// effective principal set (`owner ∪ allow ∪ sender`). Centralises the "is
/// this public?" question so call sites stop pattern-matching a single
/// position (the old `owner == global` shortcut, which broke once `owner`
/// became the subject and visibility moved to the `allow`/`sender` axes).
#[must_use]
pub fn is_public(owner: &Principal, allow: &[Principal], sender: Option<&Principal>) -> bool {
    owner.is_global()
        || allow.iter().any(Principal::is_global)
        || sender.is_some_and(Principal::is_global)
}

/// Whether `sender` has OWNER authority over a region (ACL change / chat edit).
///
/// Distinct from `principal_matches` (read authority): the builtin `global`
/// group grants read to everyone but ownership to no one, so a world fact
/// (`owner=global`) is not editable from chat. Ownership is the sender being the
/// owning user, or a member of a non-global owning group.
#[must_use]
pub fn sender_owns(owner: &Principal, sender_id: &str, sender_groups: &[String]) -> bool {
    match owner {
        Principal::User(id) => id == sender_id,
        Principal::Group(id) if id == "global" => false,
        Principal::Group(id) => sender_groups.iter().any(|g| g == id),
    }
}

/// Whether an ACL change WIDENS a fact's effective read-set: it introduces
/// at least one principal that was not already in the old set.
///
/// The disclosure signal for the audit row written by the chat
/// `acl_changes` verb (ingest pipeline).
/// The effective read-set is `{owner} ∪ allow` on each side; a change is a
/// widening when any principal in the new set is absent from the old set.
/// `Global` is treated like any other principal here — adding it newly is
/// a widening because it admits everyone; a change that only NARROWS (a
/// pure subset of the old set) is not.
#[must_use]
pub fn widens(
    old_owner: &Principal,
    old_allow: &[Principal],
    new_owner: &Principal,
    new_allow: &[Principal],
) -> bool {
    use std::collections::HashSet;
    let old_set: HashSet<&Principal> = std::iter::once(old_owner).chain(old_allow.iter()).collect();
    std::iter::once(new_owner)
        .chain(new_allow.iter())
        .any(|p| !old_set.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::collection::vec;
    use proptest::prelude::*;

    fn acl_owner(owner: Principal) -> Acl {
        Acl {
            owner: Some(owner),
            allow: vec![],
        }
    }

    fn acl_owner_allow(owner: Principal, allow: Vec<Principal>) -> Acl {
        Acl {
            owner: Some(owner),
            allow,
        }
    }

    fn groups(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    // ---------- widens ----------

    #[test]
    fn widens_true_when_global_added() {
        let alice = Principal::User("alice".into());
        assert!(widens(&alice, &[], &alice, &[Principal::global()]));
    }

    #[test]
    fn widens_true_when_a_new_user_added() {
        let alice = Principal::User("alice".into());
        let bob = Principal::User("bob".into());
        assert!(widens(&alice, &[], &alice, &[bob]));
    }

    #[test]
    fn widens_false_when_narrowing_to_subset() {
        let alice = Principal::User("alice".into());
        let bob = Principal::User("bob".into());
        // old read-set {alice, bob}; new {alice} — pure narrowing.
        assert!(!widens(&alice, &[bob], &alice, &[]));
    }

    #[test]
    fn widens_false_when_unchanged() {
        let alice = Principal::User("alice".into());
        let fam = Principal::Group("famiglia".into());
        assert!(!widens(
            &alice,
            std::slice::from_ref(&fam),
            &alice,
            std::slice::from_ref(&fam)
        ));
    }

    #[test]
    fn widens_counts_owner_swap_that_admits_new_reader() {
        // owner moves alice → bob with empty allow on both sides: bob is a
        // reader who was not in the old set, so this widens.
        let alice = Principal::User("alice".into());
        let bob = Principal::User("bob".into());
        assert!(widens(&alice, &[], &bob, &[]));
    }

    // ---------- owner-only ----------

    #[test]
    fn owner_user_matches_self() {
        let acl = acl_owner(Principal::User("alice".into()));
        assert!(can_read(&acl, "alice", &[], None));
    }

    #[test]
    fn owner_user_rejects_other() {
        let acl = acl_owner(Principal::User("alice".into()));
        assert!(!can_read(&acl, "bob", &[], None));
    }

    #[test]
    fn owner_group_matches_member() {
        let acl = acl_owner(Principal::Group("team".into()));
        assert!(can_read(&acl, "bob", &groups(&["team", "alpha"]), None));
    }

    #[test]
    fn owner_group_rejects_non_member() {
        let acl = acl_owner(Principal::Group("team".into()));
        assert!(!can_read(&acl, "carol", &groups(&["alpha"]), None));
    }

    #[test]
    fn owner_global_admits_anyone() {
        let acl = acl_owner(Principal::global());
        assert!(can_read(&acl, "alice", &[], None));
        assert!(can_read(&acl, "bob", &groups(&["whatever"]), None));
        assert!(can_read(&acl, "carol", &[], None));
    }

    // ---------- owner=None (region inherits acl_default) ----------

    #[test]
    fn empty_acl_denies_unless_sender_attribution_matches() {
        // owner=None and no allow → caller should have resolved
        // acl_default beforehand. If they didn't, the only escape is the
        // sender_of_region rule.
        let empty = Acl::default();
        let alice = Principal::User("alice".into());
        let bob = Principal::User("bob".into());
        assert!(!can_read(&empty, "alice", &[], None));
        assert!(can_read(&empty, "alice", &[], Some(&alice)));
        assert!(!can_read(&empty, "alice", &[], Some(&bob)));
    }

    // ---------- allow extends ----------

    #[test]
    fn allow_user_extends_visibility() {
        let acl = acl_owner_allow(
            Principal::User("alice".into()),
            vec![Principal::User("bob".into())],
        );
        assert!(can_read(&acl, "alice", &[], None));
        assert!(can_read(&acl, "bob", &[], None));
        assert!(!can_read(&acl, "carol", &[], None));
    }

    #[test]
    fn allow_group_extends_visibility() {
        let acl = acl_owner_allow(
            Principal::User("alice".into()),
            vec![Principal::Group("team".into())],
        );
        assert!(can_read(&acl, "alice", &[], None));
        assert!(can_read(&acl, "carol", &groups(&["team"]), None));
        assert!(!can_read(&acl, "dave", &groups(&["other"]), None));
    }

    #[test]
    fn allow_global_makes_region_public_even_with_owner() {
        let acl = acl_owner_allow(Principal::User("alice".into()), vec![Principal::global()]);
        assert!(can_read(&acl, "anyone", &[], None));
    }

    #[test]
    fn allow_combination_user_and_group() {
        let acl = acl_owner_allow(
            Principal::User("gollum".into()),
            vec![
                Principal::User("frodo".into()),
                Principal::Group("famiglia".into()),
            ],
        );
        // owner
        assert!(can_read(&acl, "gollum", &[], None));
        // allow user
        assert!(can_read(&acl, "frodo", &[], None));
        // allow group
        assert!(can_read(&acl, "galadriel", &groups(&["famiglia"]), None));
        // outsider
        assert!(!can_read(&acl, "bilbo", &groups(&["amici"]), None));
    }

    // ---------- cross-user attribution ----------

    #[test]
    fn user_sender_overrides_owner_for_that_user() {
        // Galadriel captured a region whose owner is gollum. Galadriel
        // must still be able to reread it.
        let acl = acl_owner_allow(
            Principal::User("gollum".into()),
            vec![Principal::Group("famiglia".into())],
        );
        let galadriel = Principal::User("galadriel".into());
        assert!(can_read(&acl, "galadriel", &[], Some(&galadriel)));
        // But Galadriel only gets through *because* she's the sender —
        // a different user with no group membership stays out.
        assert!(!can_read(&acl, "bilbo", &[], Some(&galadriel)));
    }

    #[test]
    fn user_sender_does_not_help_other_users() {
        let acl = acl_owner(Principal::User("alice".into()));
        let carol = Principal::User("carol".into());
        // bob is reading, region was captured by carol — bob has no
        // shortcut.
        assert!(!can_read(&acl, "bob", &[], Some(&carol)));
    }

    /// "Family microphone" case: an ambient capture device attributed to
    /// the family group. Every family member should reread, even when the
    /// region's allow-list does not name the group.
    #[test]
    fn group_sender_admits_group_members() {
        // Region owner = gollum (the person the fact is about). Sender =
        // group:famiglia (the device that captured). No allow=.
        let acl = acl_owner(Principal::User("gollum".into()));
        let famiglia = Principal::Group("famiglia".into());
        // Galadriel ∈ famiglia → reads via sender shortcut.
        assert!(can_read(
            &acl,
            "galadriel",
            &groups(&["famiglia"]),
            Some(&famiglia)
        ));
        // Frodo ∈ famiglia → also reads.
        assert!(can_read(
            &acl,
            "frodo",
            &groups(&["famiglia"]),
            Some(&famiglia)
        ));
        // Bilbo ∈ amici only → does NOT read.
        assert!(!can_read(
            &acl,
            "bilbo",
            &groups(&["amici"]),
            Some(&famiglia)
        ));
        // Stranger with no groups → does NOT read.
        assert!(!can_read(&acl, "outsider", &[], Some(&famiglia)));
    }

    /// Edge case: a "public capture device" with `sender=global`. Data
    /// model allows it; the resulting region is readable by anyone via
    /// the sender shortcut. (Functionally equivalent to making the
    /// region public.)
    #[test]
    fn global_sender_admits_anyone() {
        let acl = acl_owner(Principal::User("alice".into()));
        let global = Principal::global();
        assert!(can_read(&acl, "bob", &[], Some(&global)));
        assert!(can_read(&acl, "carol", &groups(&["team"]), Some(&global)));
    }

    #[test]
    fn admin_does_not_bypass() {
        // `isAdmin` is a UI hint only, not a server-side bypass.
        // can_read has no `is_admin` parameter — by construction the API
        // cannot grant blanket access. This test documents the invariant
        // rather than the absence of an opt-out flag.
        let acl = acl_owner(Principal::User("alice".into()));
        let admin_id = "admin";
        let admin_groups = groups(&["admins", "root"]);
        assert!(!can_read(&acl, admin_id, &admin_groups, None));
    }

    // ---------- is_public / sender_owns ----------

    #[test]
    fn is_public_detects_global_on_any_axis() {
        let alice = Principal::User("alice".into());
        let g = Principal::global();
        assert!(is_public(&g, &[], None), "owner global");
        assert!(
            is_public(&alice, &[Principal::global()], None),
            "allow global"
        );
        assert!(is_public(&alice, &[], Some(&g)), "sender global");
        assert!(
            !is_public(&alice, &[Principal::Group("team".into())], Some(&alice)),
            "no global anywhere → not public"
        );
    }

    #[test]
    fn sender_owns_user_and_group_member_but_never_global() {
        let alice = Principal::User("alice".into());
        let team = Principal::Group("team".into());
        let g = Principal::global();
        // The owning user.
        assert!(sender_owns(&alice, "alice", &[]));
        assert!(!sender_owns(&alice, "bob", &[]));
        // A member of the owning group; a non-member is refused.
        assert!(sender_owns(&team, "bob", &groups(&["team"])));
        assert!(!sender_owns(&team, "carol", &groups(&["other"])));
        // A world fact (owner=global) is editable from chat by no one,
        // even though everyone can READ it.
        assert!(!sender_owns(&g, "alice", &groups(&["global"])));
        assert!(can_read(&acl_owner(g), "alice", &[], None));
    }

    // ---------- can_delete (sender-direct authority) ----------

    #[test]
    fn can_delete_admin_always_may() {
        // Admin acts on any fact regardless of who its sender is.
        let alice = Principal::User("alice".into());
        let fam = Principal::Group("famiglia".into());
        assert!(can_delete(Some(&alice), "bob", true), "admin, user sender");
        assert!(can_delete(Some(&fam), "bob", true), "admin, group sender");
        assert!(can_delete(None, "bob", true), "admin, no sender");
    }

    #[test]
    fn can_delete_sender_may_act_on_own_contribution() {
        // The fact's sender (as `user:<caller>`) acts directly, no admin.
        let alice = Principal::User("alice".into());
        assert!(can_delete(Some(&alice), "alice", false));
    }

    #[test]
    fn can_delete_non_sender_user_refused() {
        // A different non-admin user — even if they were the owner/subject —
        // is refused the direct act; their path is a request → vote.
        let alice = Principal::User("alice".into());
        assert!(!can_delete(Some(&alice), "bob", false));
    }

    #[test]
    fn can_delete_no_sender_only_admin() {
        // A fact with no recorded sender has no direct actor but the admin.
        assert!(!can_delete(None, "alice", false));
        assert!(can_delete(None, "alice", true));
    }

    #[test]
    fn can_delete_group_sender_only_admin() {
        // The sender is always a human author in practice; a `Group` sender
        // grants the direct right to no one — only the admin acts.
        let fam = Principal::Group("famiglia".into());
        assert!(!can_delete(Some(&fam), "alice", false), "non-admin member");
        assert!(can_delete(Some(&fam), "alice", true), "admin still may");
    }

    // ---------- audience (DB-backed electorate) ----------

    #[tokio::test]
    async fn audience_expands_groups_drops_global_includes_sender() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        // famiglia = {franz, morgana, bilbo}.
        crate::enrollment::mirror_to_db(
            &pool,
            &crate::enrollment::EnrollmentFile {
                version: 1,
                users: ["franz", "morgana", "bilbo"]
                    .iter()
                    .map(|id| crate::enrollment::UserEntry {
                        id: (*id).to_owned(),
                        aliases: Vec::new(),
                        is_admin: false,
                        locale: None,
                        timezone: None,
                    })
                    .collect(),
                groups: vec![crate::enrollment::GroupEntry {
                    id: "famiglia".into(),
                    members: vec!["franz".into(), "morgana".into(), "bilbo".into()],
                    scope: None,
                }],
            },
        )
        .await
        .expect("mirror enrollment");

        // owner = user:franz, allow = [group:famiglia, global], sender = user:nina.
        let aud = audience(
            &pool,
            &Principal::User("franz".into()),
            &[Principal::Group("famiglia".into()), Principal::global()],
            Some(&Principal::User("nina".into())),
        )
        .await
        .expect("audience");
        // Group expanded; global dropped; sender included; sorted + deduped
        // (franz appears as both owner and a famiglia member → once).
        assert_eq!(aud, vec!["bilbo", "franz", "morgana", "nina"]);
    }

    #[tokio::test]
    async fn audience_global_owner_is_empty_electorate() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        // A purely public fact (owner=global, no allow, no sender) has nobody
        // finite to poll.
        let aud = audience(&pool, &Principal::global(), &[], None)
            .await
            .expect("audience");
        assert!(aud.is_empty(), "global owner yields no finite electorate");
    }

    // ---------- proptest invariants ----------

    /// Strategy producing a small `Principal`.
    fn principal_strategy() -> impl Strategy<Value = Principal> {
        prop_oneof![
            Just(Principal::global()),
            "[a-z]{1,8}".prop_map(Principal::User),
            "[a-z]{1,8}".prop_map(Principal::Group),
        ]
    }

    fn acl_strategy() -> impl Strategy<Value = Acl> {
        (
            proptest::option::of(principal_strategy()),
            vec(principal_strategy(), 0..4),
        )
            .prop_map(|(owner, allow)| Acl { owner, allow })
    }

    fn sender_groups_strategy() -> impl Strategy<Value = Vec<String>> {
        vec("[a-z]{1,8}", 0..4)
    }

    proptest! {
        /// Any region whose principal set contains `Global` is visible to
        /// everyone.
        #[test]
        fn global_admits_anyone(
            mut acl in acl_strategy(),
            sender in "[a-z]{1,8}",
            groups in sender_groups_strategy(),
        ) {
            acl.allow.push(Principal::global());
            prop_assert!(can_read(&acl, &sender, &groups, None));
        }

        /// `User(sender)` as owner always lets the named user in.
        #[test]
        fn owner_self_always_reads(
            sender in "[a-z]{1,8}",
            allow in vec(principal_strategy(), 0..4),
            groups in sender_groups_strategy(),
        ) {
            let acl = Acl { owner: Some(Principal::User(sender.clone())), allow };
            prop_assert!(can_read(&acl, &sender, &groups, None));
        }

        /// Monotonicity in `allow`: adding a principal can only grant
        /// visibility, never revoke it. `result_after ⇒ result_before` is
        /// the implication that fails monotonicity; we check the
        /// contrapositive.
        #[test]
        fn allow_is_monotonic(
            acl in acl_strategy(),
            extra in principal_strategy(),
            sender in "[a-z]{1,8}",
            groups in sender_groups_strategy(),
        ) {
            let before = can_read(&acl, &sender, &groups, None);
            let mut bigger = acl;
            bigger.allow.push(extra);
            let after = can_read(&bigger, &sender, &groups, None);
            // `before ⇒ after` (never revoke).
            prop_assert!(!before || after);
        }

        /// The user that captured a region always rereads it,
        /// regardless of the rest of the ACL.
        #[test]
        fn user_sender_always_rereads(
            acl in acl_strategy(),
            sender in "[a-z]{1,8}",
            groups in sender_groups_strategy(),
        ) {
            let sender_principal = Principal::User(sender.clone());
            prop_assert!(can_read(&acl, &sender, &groups, Some(&sender_principal)));
        }

        /// A group sender admits every member of that group, regardless
        /// of `acl.owner` / `acl.allow`. "Family microphone" property.
        #[test]
        fn group_sender_admits_members(
            acl in acl_strategy(),
            group_id in "[a-z]{1,8}",
            reader in "[a-z]{1,8}",
            mut other_groups in sender_groups_strategy(),
        ) {
            // Make sure the reader belongs to `group_id`.
            other_groups.push(group_id.clone());
            let sender_principal = Principal::Group(group_id);
            prop_assert!(can_read(&acl, &reader, &other_groups, Some(&sender_principal)));
        }

        /// A global sender opens the region to everybody.
        #[test]
        fn global_sender_admits_anyone_proptest(
            acl in acl_strategy(),
            reader in "[a-z]{1,8}",
            groups in sender_groups_strategy(),
        ) {
            let sender_principal = Principal::global();
            prop_assert!(can_read(&acl, &reader, &groups, Some(&sender_principal)));
        }
    }
}
