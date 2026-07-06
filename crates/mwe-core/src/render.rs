// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-sender rendering with redaction.
//!
//! Implements [`render_for_sender`]: parse the input, gate each
//! marker-delimited region by its ACL, and emit a declassified output
//! where invisible regions are replaced by the redaction callout.
//!
//! ## Where the ACL comes from — DB first, inline fallback
//!
//! The engine DB (`fact_index`) is the authoritative source for a
//! region's ACL. The caller pre-loads the page's fact-key → ACL map
//! ([`crate::fact_index::page_acl_map`]) and passes it in; a region
//! whose `f=<uuid>` key is in the map is gated **entirely** by the DB
//! record (owner, allow, sender — the inline attributes are ignored, and
//! the owner is always explicit there). The inline marker attributes
//! remain the fallback for regions the DB does not know — a file not yet
//! indexed, or a marker without `f=` — with the region's own `sender` as
//! the owner of last resort (see "Owner-of-last-resort semantics" below).
//! This keeps `render_for_sender` itself a pure, synchronous function:
//! all I/O stays with the caller.
//!
//! ## What gets filtered and what does not
//!
//! - **Free prose outside markers passes through verbatim, always.** It
//!   is the file's narrative scaffolding — headings, paragraph
//!   separators, the connective tissue between fact regions. The LLM
//!   that ingests this file needs that context to understand a region it
//!   later extracts or supersedes; the human reader needs it to
//!   understand the sentence around a redacted block. The owner-of-last-
//!   resort never filters prose. This was the source of an early
//!   misimplementation — see
//!   the redaction-policy design note.
//! - **Standalone embeds in prose pass through with the prose** for the
//!   same reason — an embed sitting between two paragraphs is part of
//!   the file context, not a fact-region in the redaction sense. Embeds
//!   *inside* a region inherit that region's fate (the whole body —
//!   embed included — gets kept or replaced wholesale).
//! - **Region body** is included verbatim when visible, otherwise
//!   replaced by the inline marker `[redacted]`. The marker sits exactly
//!   where the region body was, so an inline region inside a sentence
//!   leaves the sentence flowing ("Alice pesa [redacted] al 10 maggio.")
//!   and a block-level region between paragraphs becomes a `[redacted]`
//!   paragraph on its own. This intentionally diverges from the literal
//!   "callout" wording — the inline form preserves sentence flow,
//!   which was the original intent (see
//!   the redaction-policy design note
//!   for the full discussion).
//!
//! ## Owner-of-last-resort semantics
//!
//! A region whose marker carries no explicit `owner=` (and which `db_acl`
//! does not cover) falls back to its own captured **`sender`** — its
//! provenance, never the wiki's scope principal: a fact's ACL is the fact's,
//! not the category's. A region with neither an inline owner nor a sender is
//! left **unreadable** (visible only via a matching `allow`) rather than
//! inheriting a wiki-wide audience it was never granted. This owner-of-last-
//! resort does **not** apply to prose or to standalone embeds (those always
//! pass through). `meta_acl_default` is kept on the call signatures for
//! stability but no longer consulted.
//!
//! ## Lists vs continuous text
//!
//! The redaction policy distinguishes two output modes. `render_for_sender`
//! implements the **continuous-text** mode (`wiki_read` & friends). The
//! list-results mode (`wiki_search`, `wiki_recall`, …) is "omit
//! invisible entries, add a `redacted_count`" and lives inside each of
//! those tools, not here.
//!
//! ## Total redaction
//!
//! When the file has at least one region, every region was redacted,
//! and there is no prose with non-whitespace content to anchor the
//! output, the result collapses internally to the single callout
//! "This entire page is private." The collapse keeps the privacy
//! guarantee ("avoid leaking the exact count of hidden
//! regions") without forcing the caller to special-case the response:
//! `text` simply contains the callout and `blocks_redacted` still
//! reflects the per-region count for telemetry. There is no
//! `fully_redacted: bool` field on [`RenderOutput`]
//! — the detection lives entirely inside
//! `render_for_sender` and is observable to consumers only via the
//! returned `text` (collapsed callout) + `blocks_redacted > 0`.

use crate::acl::{FactAclMap, can_read};
use crate::parser::{ParseEvent, parse};
use crate::types::{Acl, FactId, Principal, RegionAttrs};

/// Inline placeholder for a non-visible region. Sits exactly where the
/// region body was, so the surrounding sentence keeps flowing:
/// `"Alice pesa [redacted] al 10 maggio."`. For block-level regions
/// (surrounded by `\n\n`) it naturally lands on its own line.
const REDACTED_INLINE_MARKER: &str = "[redacted]";

/// Total-redaction callout. Used as the *entire* output when nothing
/// meaningful in the file is visible to the sender. Block-level here is
/// fine because there is no surrounding sentence to keep flowing.
const FULLY_PRIVATE_CALLOUT: &str = "> [!redacted] This entire page is private.\n";

// ---------------------------------------------------------------------
// Admin-reveal wrappers (dashboard-only, see `render_admin_reveal`).
//
// These literal tags are emitted by [`render_admin_reveal`] around a
// region the sender could NOT read but the operator chose to reveal, so
// the dashboard markdown renderer can colour them differently. They are
// `pub const` because the dashboard's `md_render` matches on these exact
// strings to pass them through its raw-HTML filter (and drops every
// other tag) — one source of truth so the two surfaces cannot drift.
// The block form wraps a region that occupies whole lines; the inline
// form wraps a fragment sitting inside a sentence.
// ---------------------------------------------------------------------

/// Opening tag of a block-level revealed region.
pub const ACL_REVEAL_BLOCK_OPEN: &str = "<div class=\"acl-revealed\">";
/// Closing tag of a block-level revealed region.
pub const ACL_REVEAL_BLOCK_CLOSE: &str = "</div>";
/// Opening tag of an inline revealed region (sits inside a sentence).
pub const ACL_REVEAL_INLINE_OPEN: &str = "<span class=\"acl-revealed\">";
/// Closing tag of an inline revealed region.
pub const ACL_REVEAL_INLINE_CLOSE: &str = "</span>";

/// Result of rendering one input for one sender.
///
/// `text` is ready to ship to the sender (markdown). `blocks_redacted`
/// counts how many *regions* were replaced by a callout — useful to
/// populate a UI badge or telemetry. The total-redaction collapse
/// described in the module docs is folded into `text` itself: when no
/// region survived and there is no anchoring prose, `text` contains
/// the single `> [!redacted] This entire page is private.` callout.
/// There is no `fully_redacted: bool` field — the detection
/// lives internally inside `render_for_sender` and is observable only
/// through `text` (the collapsed callout) plus `blocks_redacted > 0`,
/// which keeps the exact count of hidden regions private.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutput {
    /// Declassified markdown for the sender.
    pub text: String,
    /// Number of regions that were replaced by a redaction callout.
    /// Prose chunks and standalone embeds never count here — they are
    /// not "blocks" in the redaction sense and they always pass through anyway.
    pub blocks_redacted: usize,
    /// Number of regions the sender could NOT read but that were
    /// *revealed* anyway — non-zero only for [`render_admin_reveal`], the
    /// dashboard operator override. Always `0` for [`render_for_sender`]
    /// (where those regions are redacted, not revealed). The dashboard
    /// uses it to label "N fragments revealed" and to highlight them.
    pub blocks_revealed: usize,
}

/// One ordered slice of a segmented render — see [`SegmentedRenderOutput`].
///
/// `text` is the exact byte run the joined output carries at this
/// position. `fact_id` is `Some` only for a **region shown to the
/// viewer** whose fact key the page's `db_acl` map covers — so the id
/// always resolves to a `fact_index` record a caller can link to.
/// Connective prose, standalone embeds, redaction placeholders, and
/// regions gated by inline attributes alone (the map does not know
/// them — there may be no DB record behind the key) all carry `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSegment {
    /// The rendered text of this slice.
    pub text: String,
    /// The fact behind this slice, when it is a shown, DB-known region.
    pub fact_id: Option<FactId>,
}

/// Segment-emitting variant of [`RenderOutput`]: the same rendered
/// stream, split into ordered `(text, Option<fact_id>)` slices whose
/// concatenation is byte-identical to the plain output.
///
/// Built for the **dashboard page render only** (the region →
/// source-fact click-through): the dashboard walks the segments to drop
/// a per-region anchor after each fact-backed slice. The MCP/consumer
/// paths keep the plain [`RenderOutput`] — consumer prose carries no
/// fact-id clutter. Adjacent `None` slices are merged, so the vec stays
/// small on prose-heavy pages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SegmentedRenderOutput {
    /// Ordered slices; concatenating their `text` reproduces the plain
    /// render exactly.
    pub segments: Vec<RenderSegment>,
    /// As [`RenderOutput::blocks_redacted`].
    pub blocks_redacted: usize,
    /// As [`RenderOutput::blocks_revealed`].
    pub blocks_revealed: usize,
}

impl SegmentedRenderOutput {
    /// The joined plain text — byte-identical to what the corresponding
    /// non-segmented render returns.
    #[must_use]
    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect()
    }

    /// Collapse into the plain [`RenderOutput`].
    #[must_use]
    pub fn into_output(self) -> RenderOutput {
        RenderOutput {
            text: self.text(),
            blocks_redacted: self.blocks_redacted,
            blocks_revealed: self.blocks_revealed,
        }
    }

    /// Append `text` as fact-less filler, merging into a trailing
    /// fact-less segment so adjacent prose/embed/placeholder runs stay
    /// one slice.
    fn push_plain(&mut self, text: &str) {
        if let Some(last) = self.segments.last_mut()
            && last.fact_id.is_none()
        {
            last.text.push_str(text);
            return;
        }
        self.segments.push(RenderSegment {
            text: text.to_owned(),
            fact_id: None,
        });
    }

    /// Append a shown region's body with its (map-covered) fact id.
    fn push_region(&mut self, text: &str, fact_id: Option<FactId>) {
        self.segments.push(RenderSegment {
            text: text.to_owned(),
            fact_id,
        });
    }
}

/// The fact id a shown region's segment carries: the region's `f=` key,
/// but only when the page's ACL map covers it — a map-covered key is by
/// construction a real `fact_index` record, so a caller building a
/// click-through can trust the id to resolve. An inline-gated region
/// (unindexed file, no `f=`) yields `None`.
fn segment_fact_id(attrs: &RegionAttrs, db_acl: &FactAclMap) -> Option<FactId> {
    attrs
        .fact_id
        .as_ref()
        .filter(|fid| db_acl.contains_key(*fid))
        .cloned()
}

/// Render `text` for `sender_id`, applying the redaction policy.
///
/// `db_acl` is the page's authoritative fact-key → ACL map loaded from
/// the engine DB ([`crate::fact_index::page_acl_map`]); a region whose
/// fact key is in the map is gated by the DB record alone, the inline
/// attributes only gate the regions the map does not cover (see module
/// docs). Pass an empty map to render from inline attributes only —
/// e.g. for text that never went through capture.
///
/// `meta_acl_default` is retained for signature stability but is no longer
/// consulted: a region with no inline `owner=` and not covered by `db_acl`
/// now falls back to its own captured `sender` (and is left unreadable when it
/// has neither) — never the wiki's scope principal. It still does **not**
/// filter prose or standalone embeds — those always pass through (see module
/// docs).
///
/// `sender_groups` is the list of group ids the sender belongs to (used
/// by `acl::can_read` for group-membership checks).
#[must_use]
pub fn render_for_sender(
    text: &str,
    db_acl: &FactAclMap,
    meta_acl_default: &Principal,
    sender_id: &str,
    sender_groups: &[String],
) -> RenderOutput {
    render_for_sender_segments(text, db_acl, meta_acl_default, sender_id, sender_groups)
        .into_output()
}

/// Segment-emitting sibling of [`render_for_sender`].
///
/// Same policy, same joined text, but each shown region arrives as its
/// own [`RenderSegment`] carrying its fact id (see
/// [`SegmentedRenderOutput`] for who this is for and what carries
/// `None`). A redacted region's `[redacted]` placeholder is fact-less
/// filler by design: the viewer may not read the fact, so no
/// click-through is offered either.
#[must_use]
pub fn render_for_sender_segments(
    text: &str,
    db_acl: &FactAclMap,
    meta_acl_default: &Principal,
    sender_id: &str,
    sender_groups: &[String],
) -> SegmentedRenderOutput {
    let parsed = parse(text);

    let mut out = SegmentedRenderOutput::default();
    let mut n_regions = 0usize;
    let mut visible_regions = 0usize;
    let mut has_meaningful_prose = false;

    for ev in &parsed.events {
        match ev {
            ParseEvent::Prose { text: body, .. } => {
                // Prose always passes through — it is the narrative
                // scaffolding of the file (headings, sentence connective
                // tissue around inline regions, paragraph separators).
                // Both the consumer LLM and the human reader need this
                // context to interpret the surrounding regions.
                out.push_plain(body);
                if body.chars().any(|c| !c.is_whitespace()) {
                    has_meaningful_prose = true;
                }
            },
            ParseEvent::Region { attrs, body, .. } => {
                n_regions += 1;
                let (resolved, sender_of_region) =
                    resolve_region_acl(attrs, db_acl, meta_acl_default);
                // Cross-user attribution: pass the full principal — it
                // may be User (Galadriel wrote about Gollum), Group
                // (family microphone), or Global (public capture
                // device). `can_read` treats it as an extra principal
                // in the effective ACL.
                if can_read(&resolved, sender_id, sender_groups, sender_of_region) {
                    out.push_region(body, segment_fact_id(attrs, db_acl));
                    visible_regions += 1;
                } else {
                    out.push_plain(REDACTED_INLINE_MARKER);
                    out.blocks_redacted += 1;
                }
            },
            ParseEvent::Embed { start, end, .. } => {
                // A standalone embed sits in the surrounding prose: it
                // is part of the file context, not a fact-region. An
                // embed *inside* a region is already part of that
                // region's body (which we either kept or replaced
                // wholesale above), so this branch only fires for the
                // standalone case.
                out.push_plain(&text[*start..*end]);
            },
        }
    }

    // The boolean used to be exposed via
    // `RenderOutput.fully_redacted`. It is now an internal detection
    // that decides whether to collapse the output to the single
    // callout — the caller observes the collapse through the text itself
    // (count-privacy is preserved either way).
    let collapse_to_callout = n_regions > 0 && visible_regions == 0 && !has_meaningful_prose;
    if collapse_to_callout {
        out.segments = vec![RenderSegment {
            text: FULLY_PRIVATE_CALLOUT.to_owned(),
            fact_id: None,
        }];
    }
    out
}

/// Render `text` for an **operator** who switched on the dashboard's
/// admin ACL-reveal toggle.
///
/// Every region body is shown, but the ones the `sender_id` could *not*
/// read are wrapped in the [`ACL_REVEAL_BLOCK_OPEN`] /
/// [`ACL_REVEAL_INLINE_OPEN`] markers so the dashboard renderer can colour
/// them differently. This is the counterpart of [`render_for_sender`] used
/// **only** on the dashboard, gated server-side on the admin role — it is
/// never reachable from the MCP tool surface, which always goes through
/// [`render_for_sender`] and honours the ACL. See
/// the redaction-policy design note.
///
/// Crucially this does **not** weaken the access predicate: it calls
/// [`can_read`] exactly as `render_for_sender` does, only to *decide
/// which regions to highlight*, then shows them regardless. The
/// authorization invariant ([`can_read`] has no admin bypass) is intact;
/// the reveal lives entirely in this presentation layer.
///
/// Arguments mirror [`render_for_sender`]. The returned
/// [`RenderOutput::blocks_revealed`] counts the highlighted regions;
/// `blocks_redacted` is always `0` (nothing is hidden) and there is no
/// total-redaction collapse — the operator always sees the whole page.
#[must_use]
pub fn render_admin_reveal(
    text: &str,
    db_acl: &FactAclMap,
    meta_acl_default: &Principal,
    sender_id: &str,
    sender_groups: &[String],
) -> RenderOutput {
    render_admin_reveal_segments(text, db_acl, meta_acl_default, sender_id, sender_groups)
        .into_output()
}

/// Segment-emitting sibling of [`render_admin_reveal`].
///
/// Same policy, same joined text (highlight wrappers included in the
/// segment bodies), but every **shown** region arrives as its own
/// [`RenderSegment`] carrying its fact id when the map covers it. Under
/// the reveal that is *all* regions — readable and highlighted alike,
/// retired residue included when the caller loaded the full ACL map —
/// so the dashboard supervision lens can offer the click-through on
/// everything it shows.
#[must_use]
pub fn render_admin_reveal_segments(
    text: &str,
    db_acl: &FactAclMap,
    meta_acl_default: &Principal,
    sender_id: &str,
    sender_groups: &[String],
) -> SegmentedRenderOutput {
    let parsed = parse(text);

    let mut out = SegmentedRenderOutput::default();

    for ev in &parsed.events {
        match ev {
            ParseEvent::Prose { text: body, .. } => out.push_plain(body),
            ParseEvent::Region {
                attrs,
                body,
                start,
                end,
                ..
            } => {
                let (resolved, sender_of_region) =
                    resolve_region_acl(attrs, db_acl, meta_acl_default);
                let fact_id = segment_fact_id(attrs, db_acl);
                if can_read(&resolved, sender_id, sender_groups, sender_of_region) {
                    // The sender could read this region anyway — show it
                    // verbatim, no highlight (it is not a "reveal").
                    out.push_region(body, fact_id);
                } else {
                    // Hidden from the sender, revealed for the operator:
                    // wrap so the dashboard can colour it. An inline
                    // fragment keeps the sentence flowing (span); a region
                    // on its own lines becomes a block (div), padded with
                    // blank lines so the body inside still renders as
                    // markdown rather than raw HTML.
                    let wrapped = if is_inline_region(text, *start, *end) {
                        format!("{ACL_REVEAL_INLINE_OPEN}{body}{ACL_REVEAL_INLINE_CLOSE}")
                    } else {
                        format!(
                            "\n\n{ACL_REVEAL_BLOCK_OPEN}\n\n{}\n\n{ACL_REVEAL_BLOCK_CLOSE}\n\n",
                            body.trim_matches('\n')
                        )
                    };
                    out.push_region(&wrapped, fact_id);
                    out.blocks_revealed += 1;
                }
            },
            ParseEvent::Embed { start, end, .. } => out.push_plain(&text[*start..*end]),
        }
    }

    out
}

/// Is the region at `start..end` *inline* — i.e. does it sit inside a
/// line of prose rather than occupying whole lines on its own? True when
/// there is non-whitespace content on the same source line before the
/// opening marker or after the closing marker. Drives the block-vs-inline
/// choice of reveal wrapper in [`render_admin_reveal`]. The byte offsets
/// are marker boundaries (`{{` / `}}`), always on char boundaries.
fn is_inline_region(text: &str, start: usize, end: usize) -> bool {
    let prefix_has_content = text[..start]
        .rsplit('\n')
        .next()
        .is_some_and(|line| line.chars().any(|c| !c.is_whitespace()));
    let suffix_has_content = text[end..]
        .split('\n')
        .next()
        .is_some_and(|line| line.chars().any(|c| !c.is_whitespace()));
    prefix_has_content || suffix_has_content
}

/// Resolve the ACL gating one region, plus its cross-user attribution.
///
/// DB first: when the page's [`FactAclMap`] covers the region's fact
/// key, the DB record gates it alone — owner is always explicit there,
/// and the inline attributes are ignored even when present (they are a
/// derived projection, not the source of truth). Otherwise the inline
/// marker attributes gate the region.
///
/// The owner of last resort for a region with **no inline `owner=`** is the
/// region's own `sender` (its captured provenance), NOT the wiki's scope
/// principal — a fact's ACL is the fact's, never the category's. When the
/// region has neither an inline owner nor a sender, `owner` stays `None` and
/// the region is left unreadable (invisible to everyone but a matching `allow`)
/// rather than inheriting a wiki-wide audience it was never granted.
/// `meta_acl_default` is accepted for signature stability but no longer
/// consulted here.
fn resolve_region_acl<'a>(
    attrs: &'a RegionAttrs,
    db_acl: &'a FactAclMap,
    _meta_acl_default: &Principal,
) -> (Acl, Option<&'a Principal>) {
    attrs
        .fact_id
        .as_ref()
        .and_then(|fid| db_acl.get(fid))
        .map_or_else(
            || {
                (
                    Acl {
                        // Inline owner, else the region's sender (its captured
                        // provenance) — never the wiki principal. No owner AND
                        // no sender ⇒ `None` ⇒ unreadable, rather than inventing
                        // a category-wide audience.
                        owner: attrs.acl.owner.clone().or_else(|| attrs.sender.clone()),
                        allow: attrs.acl.allow.clone(),
                    },
                    attrs.sender.as_ref(),
                )
            },
            |rec| {
                (
                    Acl {
                        owner: Some(rec.owner.clone()),
                        allow: rec.allow.clone(),
                    },
                    rec.sender.as_ref(),
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::RegionAcl;
    use crate::types::{FactId, SAMPLE_UUID_V7};

    fn groups(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Empty DB map — inline-attribute behaviour, the pre-existing test
    /// baseline.
    fn no_db() -> FactAclMap {
        FactAclMap::default()
    }

    /// One-entry DB map for the shared `SAMPLE_UUID_V7` fact key.
    fn db_acl(owner: &str, allow: &[&str], sender: Option<&str>) -> FactAclMap {
        let mut map = FactAclMap::default();
        map.insert(
            FactId::parse(SAMPLE_UUID_V7).unwrap(),
            RegionAcl {
                owner: owner.parse().unwrap(),
                allow: allow.iter().map(|p| p.parse().unwrap()).collect(),
                sender: sender.map(|p| p.parse().unwrap()),
            },
        );
        map
    }

    /// Canonical 3-region scenario (adapted
    /// to `UUIDv7`-format `fact_id`s).
    ///
    /// Region 3 carries `sender=user:alice` (its captured provenance — alice
    /// authored it) but no explicit `owner=`: with the owner-of-last-resort now
    /// being the region's sender, alice reads it as its owner while a team
    /// member reads it via `allow=group:team`. An outsider sees neither.
    fn modello_memoria_5_input() -> String {
        format!(
            "# Controller API edit-composition\n\n\
{{{{owner=global f={SAMPLE_UUID_V7}}}}}\nEndpoint del Widget Pro che gestisce \
l'edit di una composition utente.\n{{{{/}}}}\n\n\
{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}\n## Implementazione\n[codice + design \
decision storico]\n{{{{/}}}}\n\n\
{{{{allow=group:team sender=user:alice f={SAMPLE_UUID_V7}}}}}\nQuando alice dice \"sto sistemando \
l'edit composition\" parla di questa cosa.\n{{{{/}}}}\n"
        )
    }

    // ---------- basic visibility ----------

    #[test]
    fn alice_sees_everything_in_her_own_file() {
        let input = modello_memoria_5_input();
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "alice",
            &[],
        );
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
        assert_eq!(out.blocks_redacted, 0);
        assert!(out.text.contains("Endpoint del Widget Pro"));
        assert!(out.text.contains("[codice + design decision storico]"));
        assert!(out.text.contains("parla di questa cosa"));
    }

    #[test]
    fn bob_in_team_sees_global_and_team_with_callout_for_alice_region() {
        let input = modello_memoria_5_input();
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "bob",
            &groups(&["team"]),
        );
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
        // Region with owner=user:alice is redacted → 1 block redacted.
        assert_eq!(out.blocks_redacted, 1);
        // Scaffolding prose (heading) is always visible.
        assert!(out.text.contains("# Controller API edit-composition"));
        // Global region body survives.
        assert!(out.text.contains("Endpoint del Widget Pro"));
        // Team-allow region body survives.
        assert!(out.text.contains("parla di questa cosa"));
        // Alice's implementation is hidden.
        assert!(!out.text.contains("[codice + design decision storico]"));
        // ...and the inline `[redacted]` marker is in its place.
        assert!(out.text.contains("[redacted]"));
    }

    #[test]
    fn carol_outsider_sees_scaffolding_and_global_with_two_callouts() {
        let input = modello_memoria_5_input();
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "carol",
            &groups(&["sales"]),
        );
        assert_ne!(
            out.text, FULLY_PRIVATE_CALLOUT,
            "scaffolding prose keeps it from total"
        );
        assert_eq!(out.blocks_redacted, 2);
        // Scaffolding prose always passes through — the heading anchors
        // the reader even when most regions are hidden.
        assert!(out.text.contains("# Controller API edit-composition"));
        // The only visible region body for carol is the global one.
        assert!(out.text.contains("Endpoint del Widget Pro"));
        // The two redacted region bodies are gone.
        assert!(!out.text.contains("[codice"));
        assert!(!out.text.contains("parla di questa cosa"));
        // Two callouts in their place.
        assert!(out.text.contains("[redacted]"));
    }

    // ---------- inline granularity ----------

    #[test]
    fn inline_granularity_preserves_surrounding_words() {
        // The canonical "Alice pesa {{...}}72 kg{{/}} al 10 maggio, ha
        // {{...}}tagliato i capelli{{/}} ieri." example. Bob is not
        // alice and not global, so
        // region 1 (owner=user:alice) is redacted; region 2 (owner=
        // global) survives. The narrative around the regions ("Alice
        // pesa", "al 10 maggio, ha", "ieri.") always passes through —
        // that is the whole point of the inline granularity (and the
        // reason a prose-filtering acl_default would be wrong here).
        let input = format!(
            "Alice pesa {{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}72 kg{{{{/}}}} \
al 10 maggio, ha {{{{owner=global f={SAMPLE_UUID_V7}}}}}tagliato i capelli{{{{/}}}} ieri."
        );
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "bob",
            &[],
        );
        assert!(out.text.contains("Alice pesa"));
        assert!(out.text.contains("al 10 maggio, ha"));
        assert!(out.text.contains("tagliato i capelli"));
        assert!(out.text.contains("ieri."));
        assert!(!out.text.contains("72 kg"));
        assert!(out.text.contains("[redacted]"));
        assert_eq!(out.blocks_redacted, 1);
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
    }

    // ---------- owner-of-last-resort: the region's SENDER, never the wiki ----------

    #[test]
    fn region_without_owner_falls_back_to_its_sender() {
        // Region has a fact_id and a `sender=user:alice` but NO explicit
        // `owner=`. The owner-of-last-resort is the region's own sender (its
        // captured provenance), NOT the wiki principal — so alice reads it as
        // its owner, bob does not. `meta_acl_default` (here a contrasting
        // `global`) is no longer consulted. Surrounding prose always passes.
        let input = format!(
            "before {{{{sender=user:alice f={SAMPLE_UUID_V7}}}}}private body{{{{/}}}} after"
        );
        // Alice (the sender) sees the body.
        let out = render_for_sender(&input, &no_db(), &Principal::global(), "alice", &[]);
        assert!(out.text.contains("private body"));
        assert!(out.text.contains("before "));
        assert!(out.text.contains(" after"));
        assert_eq!(out.blocks_redacted, 0);
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
        // Bob does not see the body, but he still sees the surrounding prose —
        // and the callout in the body's place.
        let out = render_for_sender(&input, &no_db(), &Principal::global(), "bob", &[]);
        assert!(!out.text.contains("private body"));
        assert!(out.text.contains("before "));
        assert!(out.text.contains(" after"));
        assert!(out.text.contains("[redacted]"));
        assert_eq!(out.blocks_redacted, 1);
        assert_ne!(
            out.text, FULLY_PRIVATE_CALLOUT,
            "scaffolding 'before'/'after' prose anchors output"
        );
    }

    #[test]
    fn region_without_owner_or_sender_is_unreadable_not_wiki_default() {
        // A region with neither an inline `owner=` nor a `sender` is left
        // UNREADABLE — it is never rescued by the wiki's scope principal. Even
        // the wiki principal passed as `meta_acl_default` (here `user:alice`)
        // cannot read it: a fact's ACL is the fact's, not the category's.
        let input = format!("before {{{{f={SAMPLE_UUID_V7}}}}}orphan body{{{{/}}}} after");
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "alice",
            &[],
        );
        assert!(
            !out.text.contains("orphan body"),
            "no owner, no sender ⇒ invisible"
        );
        assert!(out.text.contains("before "));
        assert!(out.text.contains(" after"));
        assert!(out.text.contains("[redacted]"));
        assert_eq!(out.blocks_redacted, 1);
    }

    // ---------- cross-user attribution ----------

    #[test]
    fn group_sender_microphone_case() {
        // Family microphone: owner = user:gollum (the person the fact
        // describes), sender = group:famiglia (the device that captured
        // it). Family members reread via the sender shortcut even when
        // `famiglia` is NOT in `allow=`. Outsiders stay out.
        let input = format!(
            "{{{{owner=user:gollum sender=group:famiglia f={SAMPLE_UUID_V7}}}}}\
Sméagol stamattina ha brontolato a colazione.{{{{/}}}}"
        );
        let acl_default = Principal::global();

        // Galadriel ∈ famiglia → reads via sender shortcut.
        let out = render_for_sender(
            &input,
            &no_db(),
            &acl_default,
            "galadriel",
            &groups(&["famiglia"]),
        );
        assert!(out.text.contains("Sméagol stamattina"));
        assert_eq!(out.blocks_redacted, 0);

        // Frodo ∈ famiglia → also reads.
        let out = render_for_sender(
            &input,
            &no_db(),
            &acl_default,
            "frodo",
            &groups(&["famiglia"]),
        );
        assert!(out.text.contains("Sméagol stamattina"));

        // Bilbo ∈ amici only → does NOT read (still hits the inline
        // `[redacted]` marker but the file collapses to total-redaction
        // because there is no prose to anchor the output).
        let out = render_for_sender(&input, &no_db(), &acl_default, "bilbo", &groups(&["amici"]));
        assert!(!out.text.contains("Sméagol stamattina"));
        assert_eq!(out.blocks_redacted, 1);
        // The total-redaction signal is observable as the
        // collapsed callout in `text`, not as a separate boolean.
        assert_eq!(out.text, FULLY_PRIVATE_CALLOUT);
    }

    #[test]
    fn sender_attribution_lets_capturer_reread() {
        // Region owner = user:gollum, sender = user:galadriel.
        // Galadriel must be able to reread even with no other access.
        let input = format!(
            "{{{{owner=user:gollum sender=user:galadriel allow=group:famiglia f={SAMPLE_UUID_V7}}}}}\
Sméagol oggi era stanco.{{{{/}}}}"
        );
        let acl_default = Principal::global();
        let out = render_for_sender(
            &input,
            &no_db(),
            &acl_default,
            "galadriel",
            &groups(&["amici"]),
        );
        assert!(out.text.contains("Sméagol oggi era stanco"));
        assert_eq!(out.blocks_redacted, 0);
    }

    // ---------- total redaction ----------

    #[test]
    fn total_redaction_when_no_scaffolding_prose() {
        // File with only regions and whitespace-only prose between them.
        // Bob can read none of them → the body collapses to a single
        // callout ("non leakare il count esatto"). The
        // collapse is observable only via `text` itself.
        let input = format!(
            "{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}body 1{{{{/}}}}\n\n\
{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}body 2{{{{/}}}}\n"
        );
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "bob",
            &[],
        );
        assert_eq!(out.text, FULLY_PRIVATE_CALLOUT);
        assert_eq!(out.text, "> [!redacted] This entire page is private.\n");
        // blocks_redacted still reflects how many regions were
        // individually invisible — caller may use this for telemetry.
        assert_eq!(out.blocks_redacted, 2);
    }

    #[test]
    fn scaffolding_prose_prevents_total_redaction() {
        // Same situation as above but with a heading. The heading is
        // narrative scaffolding and always passes — so the page is NOT
        // collapsed to the callout, the user sees the heading plus two
        // inline `[redacted]` markers.
        let input = format!(
            "# Heading\n\n{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}body 1{{{{/}}}}\n\n\
{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}body 2{{{{/}}}}\n"
        );
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "bob",
            &[],
        );
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
        assert!(out.text.contains("# Heading"));
        assert!(out.text.contains("[redacted]"));
        assert!(!out.text.contains("body 1"));
        assert!(!out.text.contains("body 2"));
        assert_eq!(out.blocks_redacted, 2);
    }

    #[test]
    fn empty_input_is_empty_output_not_fully_redacted() {
        let out = render_for_sender("", &no_db(), &Principal::global(), "anyone", &[]);
        assert_eq!(out.text, "");
        assert_eq!(out.blocks_redacted, 0);
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
    }

    #[test]
    fn pure_visible_prose_passes_through_byte_for_byte() {
        let input = "just some prose without any markers.\nSecond line.\n";
        let out = render_for_sender(input, &no_db(), &Principal::global(), "anyone", &[]);
        assert_eq!(out.text, input);
        assert_eq!(out.blocks_redacted, 0);
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
    }

    // ---------- embed ----------

    #[test]
    fn standalone_embed_always_passes_through() {
        // Standalone embeds are file scaffolding (same rationale as
        // prose) — they pass through regardless of the sender, byte-for-
        // byte. To hide an embed it must be wrapped in a region whose
        // ACL excludes the sender.
        let input = "see this: {{embed=c-2026-05-10-foto-001.jpg}}";
        for (acl_default, sender) in [
            (Principal::global(), "anyone"),
            (Principal::User("alice".into()), "bob"),
            (Principal::Group("team".into()), "carol"),
        ] {
            let out = render_for_sender(input, &no_db(), &acl_default, sender, &[]);
            assert!(
                out.text.contains("{{embed=c-2026-05-10-foto-001.jpg}}"),
                "embed missing for sender={sender}: {:?}",
                out.text
            );
            assert!(!out.text.contains("[redacted]"));
        }
    }

    #[test]
    fn embed_inside_redacted_region_disappears_with_the_region() {
        // Surrounding prose keeps the test focused on the region-embed
        // behavior — without it the file would collapse to the
        // total-redaction callout instead.
        let input = format!(
            "prose before {{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}\
caption {{{{embed=c-2026-05-10-foto-001.jpg}}}}{{{{/}}}} prose after"
        );
        let out = render_for_sender(&input, &no_db(), &Principal::global(), "bob", &[]);
        assert!(!out.text.contains("c-2026-05-10-foto-001.jpg"));
        assert!(!out.text.contains("caption"));
        assert!(out.text.contains("[redacted]"));
        assert!(out.text.contains("prose before "));
        assert!(out.text.contains(" prose after"));
        assert_eq!(out.blocks_redacted, 1);
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
    }

    // ---------- DB-first ACL resolution ----------

    #[test]
    fn db_record_wins_over_inline_attributes_both_directions() {
        // Tightening: the marker still says global (stale inline copy)
        // but the DB says owner=user:alice → bob must NOT read.
        let input =
            format!("anchor {{{{owner=global f={SAMPLE_UUID_V7}}}}}the body{{{{/}}}} prose");
        let map = db_acl("user:alice", &[], None);
        let out = render_for_sender(&input, &map, &Principal::global(), "bob", &[]);
        assert!(!out.text.contains("the body"));
        assert!(out.text.contains("[redacted]"));
        assert_eq!(out.blocks_redacted, 1);

        // Loosening: the marker says owner=user:alice but the DB says
        // global (e.g. the ACL was widened via the dashboard and the
        // file rewrite has not landed yet) → bob reads.
        let input =
            format!("anchor {{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}the body{{{{/}}}} prose");
        let map = db_acl("global", &[], None);
        let out = render_for_sender(&input, &map, &Principal::User("alice".into()), "bob", &[]);
        assert!(out.text.contains("the body"));
        assert_eq!(out.blocks_redacted, 0);
    }

    #[test]
    fn bare_marker_resolves_from_db_not_acl_default() {
        // Forward-compat with the bare runtime marker (`{{f=uuid}}`,
        // no inline attributes): the DB record gates it even when the
        // page default would have let everyone in.
        let input = format!("anchor {{{{f={SAMPLE_UUID_V7}}}}}private body{{{{/}}}} prose");
        let map = db_acl("user:alice", &["group:team"], None);

        let out = render_for_sender(&input, &map, &Principal::global(), "alice", &[]);
        assert!(out.text.contains("private body"));

        let out = render_for_sender(
            &input,
            &map,
            &Principal::global(),
            "bob",
            &groups(&["team"]),
        );
        assert!(out.text.contains("private body"), "allow= from the DB");

        let out = render_for_sender(&input, &map, &Principal::global(), "carol", &[]);
        assert!(
            !out.text.contains("private body"),
            "acl_default=global must NOT rescue a DB-gated region"
        );
        assert_eq!(out.blocks_redacted, 1);
    }

    #[test]
    fn db_sender_attribution_lets_capturer_reread() {
        // The cross-user shortcut works from the DB record too: the
        // family microphone's capture stays readable to the family.
        let input = format!("anchor {{{{f={SAMPLE_UUID_V7}}}}}Sméagol brontola{{{{/}}}} prose");
        let map = db_acl("user:gollum", &[], Some("group:famiglia"));
        let out = render_for_sender(
            &input,
            &map,
            &Principal::global(),
            "galadriel",
            &groups(&["famiglia"]),
        );
        assert!(out.text.contains("Sméagol brontola"));

        let out = render_for_sender(
            &input,
            &map,
            &Principal::global(),
            "bilbo",
            &groups(&["amici"]),
        );
        assert!(!out.text.contains("Sméagol brontola"));
    }

    #[test]
    fn map_miss_falls_back_to_inline_attributes() {
        // The map covers a DIFFERENT fact key — this region is unknown
        // to the DB (file not yet indexed), so the inline attributes
        // keep gating it during the transition.
        let other_key = "018f1234-5678-7abc-9def-9999999999aa";
        let input =
            format!("anchor {{{{owner=user:alice f={other_key}}}}}inline body{{{{/}}}} prose");
        let map = db_acl("global", &[], None); // keyed on SAMPLE_UUID_V7, not other_key
        let out = render_for_sender(&input, &map, &Principal::global(), "bob", &[]);
        assert!(
            !out.text.contains("inline body"),
            "inline owner=user:alice must still gate an unindexed region"
        );
        let out = render_for_sender(&input, &map, &Principal::global(), "alice", &[]);
        assert!(out.text.contains("inline body"));
    }

    // ---------- snapshots ----------
    //
    // Snapshot tests for the canonical scenario.
    // Snapshots live in `tests/snapshots/` for the integration tests; here
    // we use `insta::assert_snapshot!` which stores them alongside this
    // file under `src/snapshots/render__tests__*.snap`. Run
    // `cargo insta review` to inspect/accept after intentional changes.

    #[test]
    fn snapshot_alice_full() {
        let input = modello_memoria_5_input();
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "alice",
            &[],
        );
        insta::assert_snapshot!(out.text);
    }

    #[test]
    fn snapshot_bob_team_member() {
        let input = modello_memoria_5_input();
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "bob",
            &groups(&["team"]),
        );
        insta::assert_snapshot!(out.text);
    }

    #[test]
    fn snapshot_carol_outsider() {
        let input = modello_memoria_5_input();
        let out = render_for_sender(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "carol",
            &groups(&["sales"]),
        );
        insta::assert_snapshot!(out.text);
    }

    // ---------- admin reveal (dashboard operator override) ----------

    #[test]
    fn admin_reveal_shows_every_region_and_counts_the_hidden_ones() {
        // Carol (an outsider) would normally see only the global region;
        // the operator reveal shows all three, highlighting the two she
        // could not read.
        let input = modello_memoria_5_input();
        let out = render_admin_reveal(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "carol",
            &groups(&["sales"]),
        );
        assert!(out.text.contains("Endpoint del Widget Pro"));
        assert!(out.text.contains("[codice + design decision storico]"));
        assert!(out.text.contains("parla di questa cosa"));
        // Nothing is hidden, two regions are highlighted.
        assert_eq!(out.blocks_redacted, 0);
        assert_eq!(out.blocks_revealed, 2);
        assert!(!out.text.contains("[redacted]"));
    }

    #[test]
    fn admin_reveal_does_not_wrap_regions_the_sender_can_read() {
        // Alice owns everything — the operator viewing as Alice sees no
        // highlights because nothing was hidden from her.
        let input = modello_memoria_5_input();
        let out = render_admin_reveal(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "alice",
            &[],
        );
        assert_eq!(out.blocks_revealed, 0);
        assert!(!out.text.contains(ACL_REVEAL_BLOCK_OPEN));
        assert!(!out.text.contains(ACL_REVEAL_INLINE_OPEN));
    }

    #[test]
    fn admin_reveal_wraps_inline_fragment_with_a_span() {
        // The canonical inline case: "Alice pesa {{alice}}72 kg{{/}} al
        // 10 maggio …" — bob cannot read the alice-owned fragment, so the
        // reveal wraps it inline (span), keeping the sentence flowing.
        let input = format!(
            "Alice pesa {{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}72 kg{{{{/}}}} \
al 10 maggio, ha {{{{owner=global f={SAMPLE_UUID_V7}}}}}tagliato i capelli{{{{/}}}} ieri."
        );
        let out = render_admin_reveal(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "bob",
            &[],
        );
        assert!(out.text.contains(&format!(
            "{ACL_REVEAL_INLINE_OPEN}72 kg{ACL_REVEAL_INLINE_CLOSE}"
        )));
        // The global fragment stays unwrapped (bob could read it).
        assert!(out.text.contains("tagliato i capelli"));
        assert!(!out.text.contains(ACL_REVEAL_BLOCK_OPEN));
        assert_eq!(out.blocks_revealed, 1);
    }

    #[test]
    fn admin_reveal_wraps_block_region_with_a_div_and_blank_lines() {
        // A region on its own lines is revealed as a block so its inner
        // markdown still renders. The div is blank-line padded.
        let input = format!(
            "# Heading\n\n{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}\n## Secret\nbody\n{{{{/}}}}\n"
        );
        let out = render_admin_reveal(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "bob",
            &[],
        );
        assert!(
            out.text
                .contains(&format!("\n\n{ACL_REVEAL_BLOCK_OPEN}\n\n"))
        );
        assert!(
            out.text
                .contains(&format!("\n\n{ACL_REVEAL_BLOCK_CLOSE}\n\n"))
        );
        assert!(out.text.contains("## Secret"));
        assert!(!out.text.contains(ACL_REVEAL_INLINE_OPEN));
        assert_eq!(out.blocks_revealed, 1);
    }

    #[test]
    fn admin_reveal_never_collapses_to_total_redaction() {
        // The fully-private file (no anchoring prose) collapses to the
        // callout for a normal sender, but the operator always sees the
        // bodies.
        let input = format!(
            "{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}body 1{{{{/}}}}\n\n\
{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}body 2{{{{/}}}}\n"
        );
        let out = render_admin_reveal(
            &input,
            &no_db(),
            &Principal::User("alice".into()),
            "bob",
            &[],
        );
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
        assert!(out.text.contains("body 1"));
        assert!(out.text.contains("body 2"));
        assert_eq!(out.blocks_revealed, 2);
    }

    // ---------- segments variant (dashboard region → fact click-through) ----------

    #[test]
    fn segments_readable_region_carries_fact_id_and_connective_prose_none() {
        let input = format!(
            "Alice pesa {{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}72 kg{{{{/}}}} al 10 maggio."
        );
        let map = db_acl("user:alice", &[], None);
        let seg = render_for_sender_segments(&input, &map, &Principal::global(), "alice", &[]);
        let fid = FactId::parse(SAMPLE_UUID_V7).unwrap();
        assert_eq!(
            seg.segments,
            vec![
                RenderSegment {
                    text: "Alice pesa ".to_owned(),
                    fact_id: None,
                },
                RenderSegment {
                    text: "72 kg".to_owned(),
                    fact_id: Some(fid),
                },
                RenderSegment {
                    text: " al 10 maggio.".to_owned(),
                    fact_id: None,
                },
            ]
        );
        // The joined text is byte-identical to the plain render.
        let plain = render_for_sender(&input, &map, &Principal::global(), "alice", &[]);
        assert_eq!(seg.text(), plain.text);
        assert_eq!(seg.blocks_redacted, plain.blocks_redacted);
    }

    #[test]
    fn segments_redacted_region_is_factless_filler() {
        let input = format!(
            "Alice pesa {{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}72 kg{{{{/}}}} al 10 maggio."
        );
        let map = db_acl("user:alice", &[], None);
        let seg = render_for_sender_segments(&input, &map, &Principal::global(), "bob", &[]);
        // Prose + placeholder + prose merge into one fact-less slice: no
        // segment carries a fact id, so no click-through can be offered
        // on a region the viewer cannot read.
        assert_eq!(
            seg.segments,
            vec![RenderSegment {
                text: "Alice pesa [redacted] al 10 maggio.".to_owned(),
                fact_id: None,
            }]
        );
        assert_eq!(seg.blocks_redacted, 1);
        let plain = render_for_sender(&input, &map, &Principal::global(), "bob", &[]);
        assert_eq!(seg.text(), plain.text);
    }

    #[test]
    fn segments_map_uncovered_region_is_visible_but_factless() {
        // The region is gated (and readable) via its inline attributes —
        // the DB map does not know the key, so there is no record to link
        // to and the segment stays fact-less.
        let input =
            format!("anchor {{{{owner=global f={SAMPLE_UUID_V7}}}}}public body{{{{/}}}} prose");
        let seg = render_for_sender_segments(&input, &no_db(), &Principal::global(), "carol", &[]);
        assert_eq!(seg.text(), "anchor public body prose");
        assert!(
            seg.segments.iter().all(|s| s.fact_id.is_none()),
            "an inline-gated region must not advertise a fact record: {:?}",
            seg.segments
        );
    }

    #[test]
    fn segments_total_redaction_collapses_to_one_factless_callout() {
        let input = format!("{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}body{{{{/}}}}\n");
        let map = db_acl("user:alice", &[], None);
        let seg = render_for_sender_segments(&input, &map, &Principal::global(), "bob", &[]);
        assert_eq!(
            seg.segments,
            vec![RenderSegment {
                text: FULLY_PRIVATE_CALLOUT.to_owned(),
                fact_id: None,
            }]
        );
        let plain = render_for_sender(&input, &map, &Principal::global(), "bob", &[]);
        assert_eq!(seg.text(), plain.text);
    }

    #[test]
    fn reveal_segments_carry_ids_for_readable_and_revealed_regions() {
        // Reveal shows everything — the readable region AND the
        // highlighted one both carry their fact id (map-covered), so the
        // supervision lens can click through on either.
        let private_key = SAMPLE_UUID_V7;
        let public_key = "018f1234-5678-7abc-9def-9999999999aa";
        let input = format!(
            "Alice pesa {{{{f={private_key}}}}}72 kg{{{{/}}}} \
al 10 maggio, ha {{{{f={public_key}}}}}tagliato i capelli{{{{/}}}} ieri."
        );
        // Two DB records: the private one (owner alice) reveals for bob,
        // the public one (owner global) is plainly readable.
        let mut map = db_acl("user:alice", &[], None);
        map.insert(
            FactId::parse(public_key).unwrap(),
            RegionAcl {
                owner: "global".parse().unwrap(),
                allow: vec![],
                sender: None,
            },
        );
        let seg = render_admin_reveal_segments(&input, &map, &Principal::global(), "bob", &[]);
        let private_fid = FactId::parse(private_key).unwrap();
        let public_fid = FactId::parse(public_key).unwrap();
        let revealed = seg
            .segments
            .iter()
            .find(|s| s.fact_id.as_ref() == Some(&private_fid))
            .expect("revealed region segment");
        assert!(
            revealed.text.contains(ACL_REVEAL_INLINE_OPEN) && revealed.text.contains("72 kg"),
            "the highlighted region keeps its wrapper inside the segment: {:?}",
            seg.segments
        );
        let readable = seg
            .segments
            .iter()
            .find(|s| s.fact_id.as_ref() == Some(&public_fid))
            .expect("readable region segment");
        assert_eq!(readable.text, "tagliato i capelli");
        let plain = render_admin_reveal(&input, &map, &Principal::global(), "bob", &[]);
        assert_eq!(seg.text(), plain.text);
        assert_eq!(seg.blocks_revealed, plain.blocks_revealed);
        assert_eq!(seg.blocks_revealed, 1);
    }

    #[test]
    fn snapshot_dave_global_only() {
        let input = modello_memoria_5_input();
        // Dave (an outsider, in `global` only) sees the scaffolding prose and
        // the `owner=global` region. Region 2 (`owner=user:alice`) and region 3
        // (`sender=user:alice`, no owner) are both redacted: the owner-of-last-
        // resort is the region's sender, not a wiki-wide `global` default, so a
        // sender-owned region is not globally readable.
        let out = render_for_sender(&input, &no_db(), &Principal::global(), "dave", &[]);
        insta::assert_snapshot!(out.text);
    }
}
