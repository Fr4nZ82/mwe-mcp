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
//! record (subject, allow, sender — the inline attributes are ignored, and
//! the subject is always explicit there). The inline marker attributes
//! remain the fallback for regions the DB does not know — a file not yet
//! indexed, or a marker without `f=` — with the region's own `sender` as
//! the subject of last resort (see "Subject-of-last-resort semantics" below).
//! This keeps `render_for_sender` itself a pure, synchronous function:
//! all I/O stays with the caller.
//!
//! ## What gets filtered and what does not
//!
//! - **Region body** is included verbatim when visible, otherwise
//!   replaced by the inline marker `[redacted]`. The marker sits exactly
//!   where the region body was, so an inline region inside a sentence
//!   leaves the sentence flowing ("Alice pesa [redacted] al 10 maggio.")
//!   and a block-level region between paragraphs becomes a `[redacted]`
//!   paragraph on its own. This intentionally diverges from the literal
//!   "callout" wording — the inline form preserves sentence flow,
//!   which was the original intent (.
//! - **Free prose outside markers is filtered too, by how much of the page
//!   this reader is served** ([`PageForReader`], decided once by
//!   [`page_for_reader`]). Headings, the connective tissue between regions,
//!   a `See also:` line: none of it was said by a person, all of it was
//!   written by the engine ABOUT somebody, and it restates what the facts on
//!   the page say. On a PERSON's own page it goes whole to that person and
//!   not at all to anybody else; on a group's or a subject's page a heading
//!   and its paragraph go with the facts under them; on a markerless page
//!   there is nothing to gate and it passes.
//! - **`[[links]]` are filtered with it.** A qualified `[[wiki/page]]` is an
//!   address, and an address names a page, whose memory it is in, and what it
//!   is about — so it survives only where the reader reads a fact of the page
//!   it points at. A bare `[[name]]` is not an address: it says the one word
//!   flattening it would leave, so it is left alone.
//! - **Standalone embeds in prose go where that prose goes** — an embed
//!   sitting between two paragraphs is part of the file context, not a
//!   fact-region in the redaction sense. Embeds *inside* a region inherit
//!   that region's fate (the whole body — embed included — gets kept or
//!   replaced wholesale).
//! - **A page with no ACTIVE fact serves nothing at all.** What is left on it
//!   is prose written around facts that are retired, forgotten, or were never
//!   there. The night reads the file itself and the operator's reveal is its
//!   own render, so neither is affected.
//!
//! ## Subject-of-last-resort semantics
//!
//! A region whose marker carries no explicit `subject=` (and which `db_acl`
//! does not cover) falls back to its own captured **`sender`** — its
//! provenance, never the wiki's scope principal: a fact's ACL is the fact's,
//! not the category's. A region with neither an inline subject nor a sender is
//! left **unreadable** (visible only via a matching `allow`) rather than
//! inheriting a wiki-wide audience it was never granted. This subject-of-last-
//! resort does **not** apply to prose or to standalone embeds (those always
//! pass through). **The wiki's scope principal is not an input here at all**:
//! it would be a wiki-wide audience, and a region's audience is the region's.
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
//! and no prose SURVIVED the filter above with non-whitespace content to
//! anchor the output, the result collapses internally to the single callout
//! "This entire page is private." What the collapse withholds is the
//! **shape** of the page: a body of bare `[redacted]` markers would show
//! the sender how many private fragments there are and where each sits,
//! and the callout shows neither. The number itself is not a secret —
//! `blocks_redacted` carries it, and `wiki_read` ships it to the caller
//! as `redacted_count`.
//!
//! There is no `fully_redacted: bool` field on [`RenderOutput`] — the
//! detection lives entirely inside `render_for_sender` and is observable
//! to consumers only via the returned `text` (collapsed callout) +
//! `blocks_redacted > 0`.

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
// Admin-reveal wrappers (dashboard-only).
//
// These literal tags are emitted by [`render_admin_reveal_segments`] around a
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
/// counts how many *regions* were replaced by a callout — `wiki_read`
/// ships it as `redacted_count`, and a UI can badge it. The
/// total-redaction collapse described in the module docs is folded into
/// `text` itself: when no region survived and there is no anchoring
/// prose, `text` contains the single
/// `> [!redacted] This entire page is private.` callout, so the body
/// reveals neither how the page is laid out nor where its private
/// fragments sit. There is no `fully_redacted: bool` field — the
/// detection lives internally inside `render_for_sender` and is
/// observable only through `text` (the collapsed callout) plus
/// `blocks_redacted > 0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutput {
    /// Declassified markdown for the sender.
    pub text: String,
    /// Number of regions that were replaced by a redaction callout.
    /// Prose chunks and standalone embeds never count here — they are
    /// not "blocks" in the redaction sense and they always pass through anyway.
    pub blocks_redacted: usize,
    /// Number of regions the sender could NOT read but that were
    /// *revealed* anyway — non-zero only for
    /// [`render_admin_reveal_segments`], the dashboard operator override.
    /// Always `0` for [`render_for_sender`]
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

/// How much of a page, beside the facts they may read, one reader is served.
///
/// The question is whose memory the page is in, and it is answered once by
/// [`page_for_reader`] so the four surfaces that serve a page cannot drift.
/// Everything a page carries and is not a fact — its headings, the prose
/// between the facts, a `See also:` line — was written by the engine ABOUT
/// somebody, and it restates what the facts say. Serving it to a reader who
/// may not read those facts hands them the thing the ACL was holding back
/// (founder, 2026-09-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageForReader {
    /// Every word of the page, with the facts on it gated one by one and
    /// nothing else withheld. A markerless page — a smart wiki's, where the
    /// gate was the wiki and has already been passed — and any text gated
    /// from its own inline markers rather than from the index.
    AsWritten,
    /// The whole page, and nothing at all once its last active fact is gone:
    /// what is left then is the prose the engine wrote around facts that are
    /// not there. The person whose memory it is, reading their own.
    Whole,
    /// The facts this reader may read, one per line, and nothing else.
    /// Somebody ELSE's memory.
    FactsAlone,
    /// Section by section: a heading and the prose under it are served when
    /// the reader may read a fact of that section, and vanish with it when
    /// they may not. A group's memory, or a subject's.
    SectionBySection,
}

/// Which of the four [`PageForReader`] a page in `meta` is for `sender_id`.
///
/// A PERSON's own memory is the one wiki whose prose is about one person: the
/// person reads it whole, and anybody else reads its facts alone. Everything
/// else a standard wiki can be — a group's, a subject's, an assistant's — is
/// written about many, and there a SECTION is the unit: whoever reads a fact
/// of it reads the words around it.
#[must_use]
pub fn page_for_reader(meta: &crate::wiki::WikiMeta, sender_id: &str) -> PageForReader {
    if meta.smart {
        return PageForReader::AsWritten;
    }
    let a_persons_own = meta.wiki_type == crate::wiki::IDENTITY_WIKI_TYPE
        && meta.parent_wiki_id.is_none()
        && !meta.is_agent;
    if a_persons_own {
        if meta.wiki_id.as_str() == sender_id {
            return PageForReader::Whole;
        }
        return PageForReader::FactsAlone;
    }
    PageForReader::SectionBySection
}

/// Everything the render needs about the reader and the page they asked for.
///
/// The permissions are worked out by the caller, which is the half that may
/// touch the database; the render itself stays pure.
#[derive(Clone, Copy)]
pub struct ReaderView<'a> {
    /// Who is reading.
    pub sender_id: &'a str,
    /// The groups they belong to, for [`can_read`].
    pub sender_groups: &'a [String],
    /// How much of the page they are served.
    pub page: PageForReader,
    /// The wiki this page lives in, so a bare `[[page]]` resolves against it.
    pub home_wiki: &'a str,
    /// Where this reader may be sent. `None` leaves every link as written —
    /// for a markerless page, and for an injected copy that has nothing to
    /// navigate from anyway.
    pub may_go: Option<Destinations<'a>>,
}

/// What a `[[link]]` is asked, and what it takes to ask it.
///
/// The two travel together because the question needs both: the card says
/// which pages this reader reads a fact of, and the tree says what a page of a
/// given wiki is actually CALLED — a smart wiki lives under its person's
/// (`wikis/<user>/<slug>/`) while its id is `<user>-<slug>`, so a path glued
/// together from the id misses every nested wiki and the answer comes back no
/// by accident instead of by rule.
#[derive(Clone, Copy)]
pub struct Destinations<'a> {
    /// Which pages this reader reads a fact of.
    pub card: &'a crate::meta_annotate::ReaderCard,
    /// The tree the addresses are resolved against.
    pub tree: &'a crate::wiki::WikiTree,
}

impl<'a> ReaderView<'a> {
    /// The view of a page whose only gate is its facts: every word of it is
    /// served, each region asked of [`can_read`], and every `[[link]]` left as
    /// written. What a markerless page gets, and what a fragment gated from
    /// its own inline markers gets.
    #[must_use]
    pub const fn as_written(sender_id: &'a str, sender_groups: &'a [String]) -> Self {
        Self {
            sender_id,
            sender_groups,
            page: PageForReader::AsWritten,
            home_wiki: "",
            may_go: None,
        }
    }
}

/// Does this line of prose give a redacted page something to stand on?
///
/// The total-redaction collapse asks whether anything outside the fact regions
/// would still be readable, and the answer is no for everything the engine
/// appends on its own account — [`crate::wiki::is_engine_furniture`] is that
/// rule, shared with the reader of the sender's standing policy so the two
/// cannot drift. Scaffolding says nothing of its own, and on a page whose
/// every fact is withheld it says the one thing the collapse exists to
/// withhold: that the page has parts, and roughly where they sit.
///
/// Everything a person wrote does count, a lone heading included: a heading
/// names something, and a reader shown a heading over `[redacted]` has been
/// told what they may not read, which is the author's decision to make.
fn anchors_a_redacted_page(line: &str) -> bool {
    !line.trim().is_empty() && !crate::wiki::is_engine_furniture(line)
}

/// Did this render put anything in the reader's hands?
///
/// Two shapes say no and they are the same answer: an empty page — nothing on
/// it was theirs and nothing was left to serve — and the total-redaction
/// callout, which says the page exists and is all somebody else's. A caller
/// that answers differently in the two cases tells the reader which of the two
/// it was, and that is itself something about the page.
#[must_use]
pub fn serves_nothing(text: &str) -> bool {
    text.trim().is_empty() || text.trim() == FULLY_PRIVATE_CALLOUT.trim()
}

/// The `See also:` line the compiler writes under a page served as its bare
/// facts — a list of addresses and nothing else. Its entries are removed
/// rather than flattened when the reader may not follow them, because a name
/// with no sentence around it is not holding any sentence together.
const SEE_ALSO_PREFIX: &str = "See also:";

/// May this reader be sent where this link points?
///
/// **A qualified `[[wiki/page]]` is an ADDRESS**, and an address the reader
/// may not use tells them a page exists, in whose memory, and what it is
/// about. So it survives only where they demonstrably read a fact of the page
/// it names.
///
/// **A bare `[[name]]` is not an address and is left alone.** Which page it
/// resolves to is a question only the tree can answer, and the answer is asked
/// again, against this same reader, before any page is opened.
///
/// The page's address is asked OF ITS WIKI, never glued together from the id:
/// a smart wiki lives under its person's directory while its id is
/// `<user>-<slug>`, so a hand-built path misses every nested wiki. And a
/// **markerless wiki is refused outright, by the rule and not by accident**:
/// it holds no facts at all, so there is no fact of its pages for anybody to
/// read and no address into one can survive this question.
fn may_follow(inner: &str, view: &ReaderView<'_>) -> bool {
    let Some(dest) = view.may_go else {
        return true;
    };
    let target = inner.split('|').next().unwrap_or(inner).trim();
    let Some((wiki, page)) = target.split_once('/') else {
        return true;
    };
    let (wiki, page) = (wiki.trim(), page.trim());
    let Ok(id) = crate::types::WikiId::parse(wiki) else {
        return false;
    };
    let Ok(handle) = dest.tree.locate(&id) else {
        return false;
    };
    if handle.meta().smart {
        return false;
    }
    // A rail is written without the extension; a hand-typed one may carry it.
    let rel = format!("{}.md", page.strip_suffix(".md").unwrap_or(page));
    dest.card
        .reader_can_read_page(wiki, &handle.source_path(std::path::Path::new(&rel)))
}

/// The words the author chose for a reader to see, when they chose any.
///
/// An alias is presentation: it is already in the sentence, in the sentence's
/// own grammar. What sits on the other side of the `|` is an ADDRESS, and the
/// last segment of an address is the file's SLUG — `acquisti_conad_neonata` —
/// which is the page's name written out, in no grammar at all. So a link with
/// no alias has nothing a reader may be given in its place.
fn wikilink_alias(inner: &str) -> Option<&str> {
    inner
        .split_once('|')
        .map(|(_, alias)| alias.trim())
        .filter(|alias| !alias.is_empty())
}

/// Split one line into the clauses a withheld address can be taken out of.
///
/// A full stop, a question or an exclamation mark ends one; so does a
/// SEMICOLON or a COLON, because what follows either is a clause that stands
/// on its own and there is no reason to take the rest of the line with it.
/// Each keeps its own terminator, and an abbreviation («Sig. Rossi») splits a
/// sentence in two — which errs towards keeping prose: the address goes either
/// way, and only less of the line goes with it.
fn sentences(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut start = 0usize;
    for (i, ch) in line.char_indices() {
        if !matches!(ch, '.' | '!' | '?' | ';' | ':' | '\u{2026}') {
            continue;
        }
        let after = i + ch.len_utf8();
        if after >= bytes.len() {
            continue;
        }
        // Run on through a cluster of terminators («?!», «...»), then cut
        // when whitespace follows.
        if matches!(line[after..].chars().next(), Some('.' | '!' | '?')) {
            continue;
        }
        if line[after..].starts_with(char::is_whitespace) {
            let cut = after + line[after..].len() - line[after..].trim_start().len();
            out.push(&line[start..cut]);
            start = cut;
        }
    }
    if start < line.len() || out.is_empty() {
        out.push(&line[start..]);
    }
    out
}

/// One sentence, as this reader is served it — or `None` when it goes.
///
/// **A sentence that points where the reader may not go is a sentence about
/// what they may not read.** Where the author wrote an alias, the alias stays
/// and the sentence stands: those words were chosen for a reader to see and
/// they are already part of it. Where they wrote none, there is nothing to put
/// in the link's place that is not the page's own name — its slug, or its
/// title, which says as much — so the sentence goes with the address.
fn sentence_for_reader(sentence: &str, view: &ReaderView<'_>) -> Option<String> {
    let mut out = String::with_capacity(sentence.len());
    let mut rest = sentence;
    while let Some(open) = rest.find("[[") {
        let (before, from_open) = rest.split_at(open);
        let Some(close) = from_open.find("]]") else {
            break;
        };
        let inner = &from_open[2..close];
        out.push_str(before);
        if may_follow(inner, view) {
            out.push_str(&from_open[..=close + 1]);
        } else {
            out.push_str(wikilink_alias(inner)?);
        }
        rest = &from_open[close + 2..];
    }
    out.push_str(rest);
    Some(out)
}

/// Rewrite one line of prose so it names only what this reader may open.
///
/// Sentence by sentence ([`sentence_for_reader`]), so what goes with an
/// address is the sentence that carried it and not the paragraph around it. A
/// line left with nothing but whitespace goes entirely.
///
/// A `See also:` line is not prose and is handled apart: it is a list of
/// addresses and nothing else, so an address the reader may not follow is
/// REMOVED from it rather than replaced, and a line left with none goes.
fn line_for_reader(line: &str, view: &ReaderView<'_>) -> Option<String> {
    if view.may_go.is_none() || !line.contains("[[") {
        return Some(line.to_owned());
    }
    if line.trim_start().starts_with(SEE_ALSO_PREFIX) {
        let mut kept: Vec<String> = Vec::new();
        let mut rest = line;
        while let Some(open) = rest.find("[[") {
            let from_open = &rest[open..];
            let Some(close) = from_open.find("]]") else {
                break;
            };
            let inner = &from_open[2..close];
            if may_follow(inner, view) {
                kept.push(format!("[[{inner}]]"));
            }
            rest = &from_open[close + 2..];
        }
        if kept.is_empty() {
            return None;
        }
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        return Some(format!("{indent}{SEE_ALSO_PREFIX} {}", kept.join(", ")));
    }
    let clauses = sentences(line);
    let kept: String = clauses
        .iter()
        .filter_map(|c| sentence_for_reader(c, view))
        .collect();
    if kept.trim().is_empty() {
        return None;
    }
    // A list item whose opening clause went takes its bullet with it, and the
    // clause that survives is left hanging off the item above. Put the marker
    // back on what is left: it is the line's shape, not its content.
    let marker = list_marker(line);
    if !marker.is_empty() && !kept.starts_with(marker) {
        return Some(format!("{marker}{}", kept.trim_start()));
    }
    Some(kept)
}

/// Rewrite a run of prose for this reader, line by line.
fn prose_for_reader(prose: &str, view: &ReaderView<'_>) -> String {
    if view.may_go.is_none() || !prose.contains("[[") {
        return prose.to_owned();
    }
    let mut out = String::with_capacity(prose.len());
    let mut rest = prose;
    while let Some(nl) = rest.find('\n') {
        if let Some(line) = line_for_reader(&rest[..nl], view) {
            out.push_str(&line);
            out.push('\n');
        }
        rest = &rest[nl + 1..];
    }
    if !rest.is_empty()
        && let Some(line) = line_for_reader(rest, view)
    {
        out.push_str(&line);
    }
    out
}

/// The bullet or number a list item opens with, indent included — empty when
/// the line is not a list item.
///
/// What a reader sees of a list is its shape, and the shape is not content:
/// when the clause carrying the marker goes, the marker comes back on what is
/// left rather than leaving the rest to hang off the item above.
fn list_marker(line: &str) -> &str {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    for m in ["- ", "* ", "+ "] {
        if rest.starts_with(m) {
            return &line[..indent + m.len()];
        }
    }
    // `1. `, `12) ` and the like.
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && rest[digits..].starts_with(['.', ')']) && rest[digits + 1..].starts_with(' ') {
        return &line[..indent + digits + 2];
    }
    ""
}

/// Does this line open a new section?
///
/// An ATX heading of any level. The section it opens runs to the next one, and
/// it is the unit [`PageForReader::SectionBySection`] serves or withholds.
fn opens_a_section(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') && t.trim_start_matches('#').starts_with(' ')
}

/// One run of the page, tagged with the section it belongs to.
enum Piece {
    /// Prose, already rewritten for the reader.
    Prose(String),
    /// A region: its body, whether this reader may read it, and its fact id.
    Region {
        body: String,
        visible: bool,
        fact_id: Option<FactId>,
    },
}

/// Split one run of prose into the sections it spans.
///
/// Each item is `(opens a new section, the run)`. A heading line starts a run
/// and flags it; everything before the first heading belongs to the run that
/// was already open.
fn split_at_headings(chunk: &str) -> Vec<(bool, &str)> {
    let mut runs: Vec<(bool, &str)> = Vec::new();
    let mut run_start = 0usize;
    let mut run_is_new = false;
    let mut at = 0usize;
    for line in chunk.split_inclusive('\n') {
        if opens_a_section(line) {
            if at > run_start {
                runs.push((run_is_new, &chunk[run_start..at]));
            }
            run_start = at;
            run_is_new = true;
        }
        at += line.len();
    }
    if at > run_start || runs.is_empty() {
        runs.push((run_is_new, &chunk[run_start..]));
    }
    runs
}

/// Render a page for one reader, applying the redaction policy and serving
/// only as much of what is NOT a fact as [`ReaderView::page`] allows.
///
/// The page's own testata (its `description`, its keys) is taken off here, in
/// the one place that serves a page to a reader, so no surface has to remember
/// to do it.
///
/// `db_acl` is the page's authoritative fact-key → ACL map loaded from the
/// engine DB ([`crate::fact_index::page_acl_map_active`]); a region whose fact
/// key is in the map is gated **entirely** by the DB record (subject, allow,
/// sender — the inline attributes are ignored, and the subject is always
/// explicit there). The inline marker attributes remain the fallback for
/// regions the map does not know — a file not yet indexed, or a marker without
/// `f=` — with the region's own `sender` as the subject of last resort (see
/// "Subject-of-last-resort semantics" in the module docs).
///
/// **An EMPTY map means the page has no active fact left**, and then nothing
/// of it is served: what remains on disk is the prose the engine wrote around
/// facts that are retired, forgotten, or were never there, and that prose
/// restates what they said. The exception is [`PageForReader::AsWritten`], a
/// markerless page, which has no facts by construction and whose gate was the
/// wiki.
///
/// A region with no inline `subject=` and not covered by `db_acl` falls back to
/// its own captured `sender`, and is left unreadable when it has neither —
/// never to the wiki's scope principal, which is why that principal is not a
/// parameter.
#[must_use]
pub fn render_for_sender(text: &str, db_acl: &FactAclMap, view: &ReaderView<'_>) -> RenderOutput {
    render_for_sender_segments(text, db_acl, view).into_output()
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
    view: &ReaderView<'_>,
) -> SegmentedRenderOutput {
    let doc = crate::wiki::MarkdownDoc::parse(text);
    let body: &str = doc.as_ref().map_or(text, |d| d.body.as_str());

    // The map IS the page's active facts, so an empty one says the page has
    // none left and nothing of it is served — see the doc comment. A
    // markerless page never had any and is the one exception.
    if view.page != PageForReader::AsWritten && db_acl.is_empty() {
        return SegmentedRenderOutput::default();
    }

    let parsed = parse(body);
    let mut pieces: Vec<(usize, Piece)> = Vec::new();
    let mut section = 0usize;
    let mut n_regions = 0usize;
    for ev in &parsed.events {
        match ev {
            ParseEvent::Prose { text: chunk, .. } => {
                for (opens, run) in split_at_headings(chunk) {
                    if opens {
                        section += 1;
                    }
                    pieces.push((section, Piece::Prose(prose_for_reader(run, view))));
                }
            },
            ParseEvent::Region {
                attrs,
                body: region,
                ..
            } => {
                n_regions += 1;
                let (resolved, sender_of_region) = resolve_region_acl(attrs, db_acl);
                // Cross-user attribution: pass the full principal — it may be
                // User (Galadriel wrote about Gollum), Group (family
                // microphone), or Global (public capture device). `can_read`
                // treats it as an extra principal in the effective ACL.
                let visible = can_read(
                    &resolved,
                    view.sender_id,
                    view.sender_groups,
                    sender_of_region,
                );
                pieces.push((
                    section,
                    Piece::Region {
                        body: region.clone(),
                        visible,
                        fact_id: segment_fact_id(attrs, db_acl),
                    },
                ));
            },
            ParseEvent::Embed { start, end, .. } => {
                // A standalone embed sits in the surrounding prose: it is part
                // of the file context, not a fact-region, and it goes wherever
                // that prose goes. An embed *inside* a region is already part
                // of that region's body.
                pieces.push((section, Piece::Prose(body[*start..*end].to_owned())));
            },
        }
    }

    let sections_with_a_readable_fact: std::collections::BTreeSet<usize> = pieces
        .iter()
        .filter_map(|(sec, piece)| {
            matches!(piece, Piece::Region { visible: true, .. }).then_some(*sec)
        })
        .collect();

    let mut out = SegmentedRenderOutput::default();
    let mut visible_regions = 0usize;
    let mut kept_prose = false;
    for (sec, piece) in &pieces {
        match piece {
            Piece::Prose(run) => {
                let serve = match view.page {
                    PageForReader::AsWritten | PageForReader::Whole => true,
                    PageForReader::FactsAlone => false,
                    PageForReader::SectionBySection => sections_with_a_readable_fact.contains(sec),
                };
                if serve {
                    out.push_plain(run);
                    if run.lines().any(anchors_a_redacted_page) {
                        kept_prose = true;
                    }
                }
            },
            Piece::Region {
                body: region,
                visible,
                fact_id,
            } => {
                if *visible {
                    if view.page == PageForReader::FactsAlone {
                        out.push_region(region.trim(), fact_id.clone());
                        out.push_plain("\n");
                    } else {
                        out.push_region(region, fact_id.clone());
                    }
                    visible_regions += 1;
                } else {
                    // Served facts alone, a withheld one leaves no mark: the
                    // reader is being shown the facts that are theirs to read,
                    // not the shape of somebody else's page. The count still
                    // reaches them — `blocks_redacted` is not a secret, the
                    // layout is.
                    if view.page != PageForReader::FactsAlone {
                        out.push_plain(REDACTED_INLINE_MARKER);
                    }
                    out.blocks_redacted += 1;
                }
            },
        }
    }

    // Asked of what SURVIVED, not of what was on disk: a page whose prose was
    // withheld has nothing left to stand on, whatever the file says. An
    // internal detection, deliberately not a field on `RenderOutput` — the
    // caller observes the collapse through the text itself.
    let collapse_to_callout = n_regions > 0 && visible_regions == 0 && !kept_prose;
    if collapse_to_callout {
        out.segments = vec![RenderSegment {
            text: FULLY_PRIVATE_CALLOUT.to_owned(),
            fact_id: None,
        }];
    }
    out
}

/// Render `text` for an **operator** who switched on the dashboard's
/// admin ACL-reveal toggle, one [`RenderSegment`] per ordered slice.
///
/// Every region body is shown, but the ones the `sender_id` could *not*
/// read are wrapped in the [`ACL_REVEAL_BLOCK_OPEN`] /
/// [`ACL_REVEAL_INLINE_OPEN`] markers so the dashboard renderer can colour
/// them differently. This is the counterpart of [`render_for_sender`] used
/// **only** on the dashboard, gated server-side on the admin role — it is
/// never reachable from the MCP tool surface, which always goes through
/// [`render_for_sender`] and honours the ACL.
///
/// Crucially this does **not** weaken the access predicate: it calls
/// [`can_read`] exactly as `render_for_sender` does, only to *decide
/// which regions to highlight*, then shows them regardless. The
/// authorization invariant ([`can_read`] has no admin bypass) is intact;
/// the reveal lives entirely in this presentation layer.
///
/// Every **shown** region arrives as its own segment carrying its fact id
/// when the map covers it. Under the reveal that is *all* regions —
/// readable and highlighted alike, retired residue included when the
/// caller loaded the full ACL map — so the dashboard supervision lens can
/// offer the click-through on everything it shows.
///
/// Arguments mirror [`render_for_sender`]. The
/// [`RenderOutput::blocks_revealed`] of the joined output counts the
/// highlighted regions; `blocks_redacted` is always `0` (nothing is
/// hidden) and there is no total-redaction collapse — the operator always
/// sees the whole page.
#[must_use]
pub fn render_admin_reveal_segments(
    text: &str,
    db_acl: &FactAclMap,
    sender_id: &str,
    sender_groups: &[String],
) -> SegmentedRenderOutput {
    let doc = crate::wiki::MarkdownDoc::parse(text);
    let body: &str = doc.as_ref().map_or(text, |d| d.body.as_str());
    let parsed = parse(body);

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
                let (resolved, sender_of_region) = resolve_region_acl(attrs, db_acl);
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
            ParseEvent::Embed { start, end, .. } => out.push_plain(&body[*start..*end]),
        }
    }

    out
}

/// Is the region at `start..end` *inline* — i.e. does it sit inside a
/// line of prose rather than occupying whole lines on its own? True when
/// there is non-whitespace content on the same source line before the
/// opening marker or after the closing marker. Drives the block-vs-inline
/// choice of reveal wrapper in [`render_admin_reveal_segments`]. The byte offsets
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
/// key, the DB record gates it alone — subject is always explicit there,
/// and the inline attributes are ignored even when present (they are a
/// derived projection, not the source of truth). Otherwise the inline
/// marker attributes gate the region.
///
/// The subject of last resort for a region with **no inline `subject=`** is the
/// region's own `sender` (its captured provenance), NOT the wiki's scope
/// principal — a fact's ACL is the fact's, never the category's. When the
/// region has neither an inline subject nor a sender, `subject` stays `None` and
/// the region is left unreadable (invisible to everyone but a matching `allow`)
/// rather than inheriting a wiki-wide audience it was never granted.
fn resolve_region_acl<'a>(
    attrs: &'a RegionAttrs,
    db_acl: &'a FactAclMap,
) -> (Acl, Option<&'a Principal>) {
    attrs
        .fact_id
        .as_ref()
        .and_then(|fid| db_acl.get(fid))
        .map_or_else(
            || {
                (
                    Acl {
                        // Inline subject, else the region's sender (its captured
                        // provenance) — never the wiki principal. No subject AND
                        // no sender ⇒ `None` ⇒ unreadable, rather than inventing
                        // a category-wide audience.
                        subject: attrs.acl.subject.clone().or_else(|| attrs.sender.clone()),
                        allow: attrs.acl.allow.clone(),
                        // A region the DB does not know is read off its inline
                        // attributes, and the marker language has no way to
                        // write an exclusion: there is none to carry.
                        excluded: Vec::new(),
                    },
                    attrs.sender.as_ref(),
                )
            },
            |rec| {
                (
                    Acl {
                        subject: Some(rec.subject.clone()),
                        allow: rec.allow.clone(),
                        excluded: rec.excluded.clone(),
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
    fn db_acl(subject: &str, allow: &[&str], sender: Option<&str>) -> FactAclMap {
        let mut map = FactAclMap::default();
        map.insert(
            FactId::parse(SAMPLE_UUID_V7).unwrap(),
            RegionAcl {
                excluded: Vec::new(),
                subject: subject.parse().unwrap(),
                allow: allow.iter().map(|p| p.parse().unwrap()).collect(),
                sender: sender.map(|p| p.parse().unwrap()),
            },
        );
        map
    }

    /// Two facts on one page, with prose between them and a heading over them.
    fn two_facts_and_prose() -> String {
        format!(
            "# Health\n\nThe doctor was seen in May.\n\n\
             {{{{subject=global f={SAMPLE_UUID_V7}}}}}anybody may read this{{{{/}}}}\n\n\
             ## Private\n\nAnd this paragraph explains the next one.\n\n\
             {{{{subject=user:alice f=018f1234-5678-7abc-9def-0123456789cd}}}}\
             hers alone{{{{/}}}}\n"
        )
    }

    /// The map both facts of [`two_facts_and_prose`] are known by.
    fn two_facts_map() -> FactAclMap {
        let mut map = db_acl("global", &[], None);
        map.insert(
            FactId::parse("018f1234-5678-7abc-9def-0123456789cd").unwrap(),
            RegionAcl {
                excluded: Vec::new(),
                subject: "user:alice".parse().unwrap(),
                allow: Vec::new(),
                sender: None,
            },
        );
        map
    }

    fn view(
        page: PageForReader,
        sender: &'static str,
        groups: &'static [String],
    ) -> ReaderView<'static> {
        ReaderView {
            sender_id: sender,
            sender_groups: groups,
            page,
            home_wiki: "alice",
            may_go: None,
        }
    }

    /// **Somebody else's memory is served as the facts you may read, and the
    /// words around them stay where they were written.**
    ///
    /// The prose on a person's page was written ABOUT that person, and it
    /// restates what their facts say — so handing it to another reader hands
    /// them the thing the ACL was holding back. Nor is the SHAPE of the page
    /// theirs to see: no `[redacted]` marks the spot, because knowing there
    /// was something there, and where, is knowing something.
    #[test]
    fn somebody_elses_memory_is_served_as_the_facts_they_may_read() {
        let input = two_facts_and_prose();
        let out = render_for_sender(
            &input,
            &two_facts_map(),
            &view(PageForReader::FactsAlone, "bob", &[]),
        );
        assert!(out.text.contains("anybody may read this"));
        assert!(!out.text.contains("hers alone"));
        assert!(!out.text.contains("The doctor was seen in May."));
        assert!(!out.text.contains("# Health"));
        assert!(!out.text.contains("## Private"));
        assert!(!out.text.contains("[redacted]"), "{}", out.text);
        assert_eq!(out.blocks_redacted, 1, "the count is still hers to have");
    }

    /// The same page read by the person whose memory it is: whole.
    #[test]
    fn your_own_memory_comes_back_as_it_was_written() {
        let input = two_facts_and_prose();
        let out = render_for_sender(
            &input,
            &two_facts_map(),
            &view(PageForReader::Whole, "alice", &[]),
        );
        assert!(out.text.contains("The doctor was seen in May."));
        assert!(out.text.contains("## Private"));
        assert!(out.text.contains("hers alone"));
        assert_eq!(out.blocks_redacted, 0);
    }

    /// **On a page nobody owns alone, a heading and its prose go with the
    /// facts under them.**
    ///
    /// A group's page, or a subject's, is written about many people, so the
    /// unit is the SECTION: whoever reads a fact of it reads the words around
    /// it, and whoever reads none of them gets neither the heading nor the
    /// paragraph — only the mark that says a fact stood here.
    #[test]
    fn a_section_goes_with_the_facts_under_it() {
        let input = two_facts_and_prose();
        let out = render_for_sender(
            &input,
            &two_facts_map(),
            &view(PageForReader::SectionBySection, "bob", &[]),
        );
        assert!(out.text.contains("# Health"));
        assert!(out.text.contains("The doctor was seen in May."));
        assert!(out.text.contains("anybody may read this"));
        assert!(
            !out.text.contains("## Private")
                && !out
                    .text
                    .contains("And this paragraph explains the next one."),
            "the section he reads no fact of goes with it: {}",
            out.text
        );
        assert!(out.text.contains("[redacted]"), "{}", out.text);
    }

    /// **A page whose last active fact is gone serves nothing to anybody.**
    ///
    /// What is left on it is the prose the engine wrote around facts that are
    /// not there — retired, forgotten, or never written — and that prose still
    /// says what they said. The page's own reader gets it no more than a
    /// stranger does; the night reads the file itself, and the operator's
    /// reveal is its own render.
    #[test]
    fn a_page_with_no_active_fact_serves_nothing() {
        let input = two_facts_and_prose();
        for page in [
            PageForReader::Whole,
            PageForReader::FactsAlone,
            PageForReader::SectionBySection,
        ] {
            let out = render_for_sender(&input, &no_db(), &view(page, "alice", &[]));
            assert_eq!(out.text, "", "{page:?} served something: {}", out.text);
        }
        // A markerless page is the exception, and it is not one: it never had
        // facts to lose, and the gate it passed was the wiki.
        let out = render_for_sender(
            &input,
            &no_db(),
            &view(PageForReader::AsWritten, "alice", &[]),
        );
        assert!(out.text.contains("The doctor was seen in May."));
    }

    /// A reader card built over two pages: one the reader may read a fact of,
    /// one they may not.
    async fn card_over_two_pages(
        reader: &str,
    ) -> (
        tempfile::TempDir,
        crate::wiki::WikiTree,
        crate::meta_annotate::ReaderCard,
    ) {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrations");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = crate::wiki::WikiTree::open(dir.path()).unwrap();
        for who in ["alice", "zoe"] {
            crate::wiki::create_identity_wiki(
                &tree,
                &crate::types::WikiId::parse(who).unwrap(),
                who,
                crate::wiki::IdentityKind::User,
            )
            .unwrap();
        }
        let tree = crate::wiki::WikiTree::open(dir.path()).unwrap();
        for (n, wiki, page, subject) in [
            (1u8, "alice", "shopping.md", "global"),
            (2, "zoe", "private.md", "user:zoe"),
        ] {
            crate::fact_index::insert(
                &pool,
                &crate::fact_index::NewFact {
                    excluded_ids: Vec::new(),
                    fact_id: FactId::parse(&format!("018f1234-5678-7abc-9def-01234567{n:02x}00"))
                        .unwrap(),
                    wiki_id: wiki.to_owned(),
                    source_path: format!("wikis/{wiki}/{page}"),
                    region_start: None,
                    region_end: None,
                    text: "body".to_owned(),
                    embedding: vec![0.0; 4],
                    subject_id: subject.parse().unwrap(),
                    allow_ids: Vec::new(),
                    sender_id: None,
                    subject_external: None,
                    slot: None,
                    slot_value: None,
                    authored_refs: Vec::new(),
                    fact_type: None,
                    topics: Vec::new(),
                    valid_from: None,
                    valid_to: None,
                    salience: None,
                    target_page: None,
                    style: None,
                    source_ref: None,
                },
            )
            .await
            .unwrap();
        }
        let card = crate::meta_annotate::build_reader_card(&pool, &tree, reader, &[])
            .await
            .unwrap();
        (dir, tree, card)
    }

    /// **An address the reader cannot use is not served as an address — and
    /// what takes its place is either the author's own words or nothing.**
    ///
    /// `[[zoe/private]]` says three things at once: that a page exists, whose
    /// memory it is in, and what it is about. Writing out the last segment
    /// instead would say the same thing in a worse grammar — it is the file's
    /// SLUG — so a link with no alias takes its SENTENCE with it: a sentence
    /// pointing where the reader may not go is a sentence about what they may
    /// not read. An alias is different: those words were chosen for a reader
    /// to see and are already part of the sentence, so they stay and the
    /// sentence stands.
    ///
    /// Zoe herself follows both, on the very same page — the rule is about the
    /// reader, not about the link.
    #[tokio::test]
    async fn an_address_the_reader_may_not_use_is_not_served_as_one() {
        let input = format!(
            "The list is in [[alice/shopping]]. The rest is in [[zoe/private]]. \
             She keeps [[zoe/private|the quiet things]] apart. Both live in \
             [[alice/shopping]] and [[zoe/private]].\n\n\
             {{{{subject=global f={SAMPLE_UUID_V7}}}}}a fact{{{{/}}}}\n"
        );
        let map = db_acl("global", &[], None);

        let (_d, tree, alices) = card_over_two_pages("alice").await;
        let out = render_for_sender(
            &input,
            &map,
            &ReaderView {
                sender_id: "alice",
                sender_groups: &[],
                page: PageForReader::SectionBySection,
                home_wiki: "famiglia",
                may_go: Some(Destinations {
                    card: &alices,
                    tree: &tree,
                }),
            },
        );
        assert!(out.text.contains("The list is in [[alice/shopping]]."));
        assert!(
            !out.text.contains("The rest is in"),
            "the sentence goes with the address it carried: {}",
            out.text
        );
        assert!(
            out.text.contains("She keeps the quiet things apart."),
            "the author's own words stay, and the sentence with them: {}",
            out.text
        );
        assert!(
            !out.text.contains("Both live in"),
            "one address she may not use takes the sentence, however many \
             others it holds: {}",
            out.text
        );
        assert!(
            !out.text.contains("zoe") && !out.text.contains("private"),
            "neither the slug nor the wiki it names may survive: {}",
            out.text
        );

        let (_d, ztree, zoes) = card_over_two_pages("zoe").await;
        let out = render_for_sender(
            &input,
            &map,
            &ReaderView {
                sender_id: "zoe",
                sender_groups: &[],
                page: PageForReader::SectionBySection,
                home_wiki: "famiglia",
                may_go: Some(Destinations {
                    card: &zoes,
                    tree: &ztree,
                }),
            },
        );
        assert!(out.text.contains("The rest is in [[zoe/private]]."));
        assert!(out.text.contains("Both live in"));
    }

    /// **A clause is the unit, and a list keeps its bullet.**
    ///
    /// A semicolon and a colon end a clause as surely as a full stop does:
    /// what follows either stands on its own, so an address on one side of it
    /// does not take the other side with it. And when the clause that goes was
    /// the one carrying a list item's bullet, the bullet comes back on what is
    /// left — the shape of a list is not its content, and a clause left
    /// hanging off the item above says something neither of them said.
    #[tokio::test]
    async fn a_clause_is_the_unit_and_a_list_keeps_its_bullet() {
        let map = db_acl("global", &[], None);
        let (_d, tree, alices) = card_over_two_pages("alice").await;
        let view = ReaderView {
            sender_id: "alice",
            sender_groups: &[],
            page: PageForReader::SectionBySection,
            home_wiki: "famiglia",
            may_go: Some(Destinations {
                card: &alices,
                tree: &tree,
            }),
        };

        let input = format!(
            "- The rest is in [[zoe/private]]; the list is in [[alice/shopping]].\n\
             - Two: [[zoe/private]]: and then some.\n\n\
             {{{{subject=global f={SAMPLE_UUID_V7}}}}}a fact{{{{/}}}}\n"
        );
        let out = render_for_sender(&input, &map, &view);
        assert!(
            out.text.contains("- the list is in [[alice/shopping]]."),
            "the clause that survives keeps the bullet: {}",
            out.text
        );
        assert!(
            !out.text.contains("The rest is in") && !out.text.contains("zoe"),
            "and the clause that carried the address goes: {}",
            out.text
        );
        assert!(
            out.text.contains("- Two: and then some."),
            "a colon ends a clause too, so only the clause between the colons \
             goes: {}",
            out.text
        );
    }

    /// **A `See also:` line loses the entry, not its brackets — and an empty
    /// one goes.**
    ///
    /// That line is a list of addresses and nothing else, so a name with no
    /// sentence around it is holding nothing together: flattening it would
    /// leave a bare word pretending to be a destination.
    #[tokio::test]
    async fn a_see_also_line_loses_what_the_reader_may_not_follow() {
        let map = db_acl("global", &[], None);
        let (_d, tree, alices) = card_over_two_pages("alice").await;
        let view = ReaderView {
            sender_id: "alice",
            sender_groups: &[],
            page: PageForReader::SectionBySection,
            home_wiki: "famiglia",
            may_go: Some(Destinations {
                card: &alices,
                tree: &tree,
            }),
        };

        let input = format!(
            "{{{{subject=global f={SAMPLE_UUID_V7}}}}}a fact{{{{/}}}}\n\n\
             See also: [[alice/shopping]], [[zoe/private]]\n"
        );
        let out = render_for_sender(&input, &map, &view);
        assert!(
            out.text.contains("See also: [[alice/shopping]]"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("zoe"), "{}", out.text);

        let only_hers = format!(
            "{{{{subject=global f={SAMPLE_UUID_V7}}}}}a fact{{{{/}}}}\n\n\
             See also: [[zoe/private]]\n"
        );
        let out = render_for_sender(&only_hers, &map, &view);
        assert!(
            !out.text.contains("See also"),
            "an empty list is not a line: {}",
            out.text
        );
    }

    /// Canonical 3-region scenario (adapted
    /// to `UUIDv7`-format `fact_id`s).
    ///
    /// Region 3 carries `sender=user:alice` (its captured provenance — alice
    /// authored it) but no explicit `subject=`: with the subject-of-last-resort now
    /// being the region's sender, alice reads it as its subject while a team
    /// member reads it via `allow=group:team`. An outsider sees neither.
    fn modello_memoria_5_input() -> String {
        format!(
            "# Controller API edit-composition\n\n\
{{{{subject=global f={SAMPLE_UUID_V7}}}}}\nEndpoint del Widget Pro che gestisce \
l'edit di una composition utente.\n{{{{/}}}}\n\n\
{{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}\n## Implementazione\n[codice + design \
decision storico]\n{{{{/}}}}\n\n\
{{{{allow=group:team sender=user:alice f={SAMPLE_UUID_V7}}}}}\nQuando alice dice \"sto sistemando \
l'edit composition\" parla di questa cosa.\n{{{{/}}}}\n"
        )
    }

    // ---------- basic visibility ----------

    #[test]
    fn alice_sees_everything_in_her_own_file() {
        let input = modello_memoria_5_input();
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("alice", &[]));
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
            &ReaderView::as_written("bob", &groups(&["team"])),
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
            &ReaderView::as_written("carol", &groups(&["sales"])),
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
            "Alice pesa {{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}72 kg{{{{/}}}} \
al 10 maggio, ha {{{{subject=global f={SAMPLE_UUID_V7}}}}}tagliato i capelli{{{{/}}}} ieri."
        );
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("bob", &[]));
        assert!(out.text.contains("Alice pesa"));
        assert!(out.text.contains("al 10 maggio, ha"));
        assert!(out.text.contains("tagliato i capelli"));
        assert!(out.text.contains("ieri."));
        assert!(!out.text.contains("72 kg"));
        assert!(out.text.contains("[redacted]"));
        assert_eq!(out.blocks_redacted, 1);
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
    }

    // ---------- subject-of-last-resort: the region's SENDER, never the wiki ----------

    #[test]
    fn region_without_subject_falls_back_to_its_sender() {
        // Region has a fact_id and a `sender=user:alice` but NO explicit
        // `subject=`. The subject-of-last-resort is the region's own sender (its
        // captured provenance), NOT the wiki principal — so alice reads it as
        // its subject, bob does not. Surrounding prose always passes.
        let input = format!(
            "before {{{{sender=user:alice f={SAMPLE_UUID_V7}}}}}private body{{{{/}}}} after"
        );
        // Alice (the sender) sees the body.
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("alice", &[]));
        assert!(out.text.contains("private body"));
        assert!(out.text.contains("before "));
        assert!(out.text.contains(" after"));
        assert_eq!(out.blocks_redacted, 0);
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
        // Bob does not see the body, but he still sees the surrounding prose —
        // and the callout in the body's place.
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("bob", &[]));
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
    fn region_without_subject_or_sender_is_unreadable_not_wiki_default() {
        // A region with neither an inline `subject=` nor a `sender` is left
        // UNREADABLE — it is never rescued by the wiki's scope principal, and
        // alice here is that principal: a fact's ACL is the fact's, not the
        // category's.
        let input = format!("before {{{{f={SAMPLE_UUID_V7}}}}}orphan body{{{{/}}}} after");
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("alice", &[]));
        assert!(
            !out.text.contains("orphan body"),
            "no subject, no sender ⇒ invisible"
        );
        assert!(out.text.contains("before "));
        assert!(out.text.contains(" after"));
        assert!(out.text.contains("[redacted]"));
        assert_eq!(out.blocks_redacted, 1);
    }

    #[test]
    fn a_wikis_own_principal_does_not_read_a_fact_about_someone_else() {
        // The two axes this codebase spells with the same word are
        // independent, and this is the case that proves it: a wiki's scope
        // principal is its proprietor, the authority for wiki-level acts,
        // while the region attribute is the FACT's subject.
        // A fact about bob filed inside alice's wiki stays bob's, and alice
        // opening her own wiki does not thereby read it.
        //
        // The sibling test above covers the orphan region (no subject at all);
        // this one covers the case that actually carries someone else's datum.
        // Deliberately the LEGACY spelling: this is the redaction engine's only
        // fixture that still feeds `owner=`, so the permanent read alias is
        // exercised through the whole render path and not only in the parser.
        let input =
            format!("before {{{{owner=user:bob f={SAMPLE_UUID_V7}}}}}bob's weight{{{{/}}}} after");
        let for_alice = render_for_sender(&input, &no_db(), &ReaderView::as_written("alice", &[]));
        assert!(
            !for_alice.text.contains("bob's weight"),
            "the wiki's proprietor is not a reader of every fact filed in it"
        );
        assert_eq!(for_alice.blocks_redacted, 1);

        // …and the fact is not lost, only withheld: its own subject reads it.
        let for_bob = render_for_sender(&input, &no_db(), &ReaderView::as_written("bob", &[]));
        assert!(for_bob.text.contains("bob's weight"));
        assert_eq!(for_bob.blocks_redacted, 0);
    }

    // ---------- cross-user attribution ----------

    #[test]
    fn group_sender_microphone_case() {
        // Family microphone: subject = user:gollum (the person the fact
        // describes), sender = group:famiglia (the device that captured
        // it). Family members reread via the sender shortcut even when
        // `famiglia` is NOT in `allow=`. Outsiders stay out.
        let input = format!(
            "{{{{subject=user:gollum sender=group:famiglia f={SAMPLE_UUID_V7}}}}}\
Sméagol stamattina ha brontolato a colazione.{{{{/}}}}"
        );
        // Galadriel ∈ famiglia → reads via sender shortcut.
        let out = render_for_sender(
            &input,
            &no_db(),
            &ReaderView::as_written("galadriel", &groups(&["famiglia"])),
        );
        assert!(out.text.contains("Sméagol stamattina"));
        assert_eq!(out.blocks_redacted, 0);

        // Frodo ∈ famiglia → also reads.
        let out = render_for_sender(
            &input,
            &no_db(),
            &ReaderView::as_written("frodo", &groups(&["famiglia"])),
        );
        assert!(out.text.contains("Sméagol stamattina"));

        // Bilbo ∈ amici only → does NOT read (still hits the inline
        // `[redacted]` marker but the file collapses to total-redaction
        // because there is no prose to anchor the output).
        let out = render_for_sender(
            &input,
            &no_db(),
            &ReaderView::as_written("bilbo", &groups(&["amici"])),
        );
        assert!(!out.text.contains("Sméagol stamattina"));
        assert_eq!(out.blocks_redacted, 1);
        // The total-redaction signal is observable as the
        // collapsed callout in `text`, not as a separate boolean.
        assert_eq!(out.text, FULLY_PRIVATE_CALLOUT);
    }

    /// A page of facts and a separator still collapses, so the separator
    /// cannot report the page's shape to a reader who may read none of it.
    ///
    /// The compiler puts a thematic break above the facts it appends when the
    /// writer left them untagged, and a page that is nothing but facts is the
    /// ordinary case. Counting that break as prose would hand the one reader
    /// the collapse exists for — the one who may read nothing here — a body of
    /// `[redacted]`, a rule, `[redacted]`: how many parts, and where the seam
    /// between them falls.
    #[test]
    fn a_separator_is_not_the_prose_that_keeps_a_private_page_from_collapsing() {
        let private = format!(
            "{{{{subject=user:frodo allow=group:famiglia f={SAMPLE_UUID_V7}}}}}\
Il primo fatto.{{{{/}}}}\n\n---\n\n\
{{{{subject=user:frodo allow=group:famiglia f=0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d02}}}}\
Il secondo fatto.{{{{/}}}}"
        );

        // The reader who may read none of it is told one thing and no more.
        let out = render_for_sender(
            &private,
            &no_db(),
            &ReaderView::as_written("bilbo", &groups(&["amici"])),
        );
        assert_eq!(
            out.text, FULLY_PRIVATE_CALLOUT,
            "a separator must not keep the page from collapsing: {}",
            out.text
        );
        assert_eq!(out.blocks_redacted, 2, "and the count is still carried");

        // The reader who may read it gets the page, separator included.
        let out = render_for_sender(
            &private,
            &no_db(),
            &ReaderView::as_written("frodo", &groups(&["famiglia"])),
        );
        assert!(out.text.contains("Il primo fatto."));
        assert!(out.text.contains("Il secondo fatto."));
        assert!(
            out.text.contains("---"),
            "the break is still written for whoever can read the page: {}",
            out.text
        );

        // The other thing the engine appends on its own account: the rail
        // floor's bare line of addresses, written when the prose declined to
        // carry them. It names no claim either, so it holds nothing up.
        let with_rails = format!("{private}\n\n[[alice/spesa]] · [[bob/hobbies]]\n");
        let out = render_for_sender(
            &with_rails,
            &no_db(),
            &ReaderView::as_written("bilbo", &groups(&["amici"])),
        );
        assert_eq!(
            out.text, FULLY_PRIVATE_CALLOUT,
            "a line of bare links must not keep the page from collapsing: {}",
            out.text
        );

        // The third shape the engine writes on its own account: the note it
        // leaves on a rules page when a directive is withdrawn.
        let with_a_note = format!(
            "{private}\n\n{}\n",
            crate::wiki::withdrawn_note("10 September 2026 (UTC)")
        );
        let out = render_for_sender(
            &with_a_note,
            &no_db(),
            &ReaderView::as_written("bilbo", &groups(&["amici"])),
        );
        assert_eq!(
            out.text, FULLY_PRIVATE_CALLOUT,
            "a withdrawal note must not keep the page from collapsing: {}",
            out.text
        );

        // Real prose still anchors: one heading is enough, and it should be —
        // a heading names what is withheld, which is the author's decision.
        let with_heading = format!("# Il titolo\n\n{private}");
        let out = render_for_sender(
            &with_heading,
            &no_db(),
            &ReaderView::as_written("bilbo", &groups(&["amici"])),
        );
        assert_ne!(
            out.text, FULLY_PRIVATE_CALLOUT,
            "a page with real prose does not collapse"
        );
        assert!(out.text.contains("Il titolo"));

        // And a sentence that merely BEGINS with a link is prose, not a rail
        // line: the discriminator is that the line says nothing of its own.
        let prose_with_a_link = format!("{private}\n\n[[alice/spesa]] è dove teniamo la lista.\n");
        let out = render_for_sender(
            &prose_with_a_link,
            &no_db(),
            &ReaderView::as_written("bilbo", &groups(&["amici"])),
        );
        assert_ne!(
            out.text, FULLY_PRIVATE_CALLOUT,
            "a sentence carrying a link is still a sentence"
        );
    }

    #[test]
    fn sender_attribution_lets_capturer_reread() {
        // Region subject = user:gollum, sender = user:galadriel.
        // Galadriel must be able to reread even with no other access.
        let input = format!(
            "{{{{subject=user:gollum sender=user:galadriel allow=group:famiglia f={SAMPLE_UUID_V7}}}}}\
Sméagol oggi era stanco.{{{{/}}}}"
        );
        let out = render_for_sender(
            &input,
            &no_db(),
            &ReaderView::as_written("galadriel", &groups(&["amici"])),
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
            "{{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}body 1{{{{/}}}}\n\n\
{{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}body 2{{{{/}}}}\n"
        );
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("bob", &[]));
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
            "# Heading\n\n{{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}body 1{{{{/}}}}\n\n\
{{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}body 2{{{{/}}}}\n"
        );
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("bob", &[]));
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
        assert!(out.text.contains("# Heading"));
        assert!(out.text.contains("[redacted]"));
        assert!(!out.text.contains("body 1"));
        assert!(!out.text.contains("body 2"));
        assert_eq!(out.blocks_redacted, 2);
    }

    #[test]
    fn empty_input_is_empty_output_not_fully_redacted() {
        let out = render_for_sender("", &no_db(), &ReaderView::as_written("anyone", &[]));
        assert_eq!(out.text, "");
        assert_eq!(out.blocks_redacted, 0);
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
    }

    #[test]
    fn pure_visible_prose_passes_through_byte_for_byte() {
        let input = "just some prose without any markers.\nSecond line.\n";
        let out = render_for_sender(input, &no_db(), &ReaderView::as_written("anyone", &[]));
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
        for sender in ["anyone", "bob", "carol"] {
            let out = render_for_sender(input, &no_db(), &ReaderView::as_written(sender, &[]));
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
            "prose before {{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}\
caption {{{{embed=c-2026-05-10-foto-001.jpg}}}}{{{{/}}}} prose after"
        );
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("bob", &[]));
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
            format!("anchor {{{{subject=global f={SAMPLE_UUID_V7}}}}}the body{{{{/}}}} prose");
        let map = db_acl("user:alice", &[], None);
        let out = render_for_sender(&input, &map, &ReaderView::as_written("bob", &[]));
        assert!(!out.text.contains("the body"));
        assert!(out.text.contains("[redacted]"));
        assert_eq!(out.blocks_redacted, 1);

        // Loosening: the marker says owner=user:alice but the DB says
        // global (e.g. the ACL was widened via the dashboard and the
        // file rewrite has not landed yet) → bob reads.
        let input =
            format!("anchor {{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}the body{{{{/}}}} prose");
        let map = db_acl("global", &[], None);
        let out = render_for_sender(&input, &map, &ReaderView::as_written("bob", &[]));
        assert!(out.text.contains("the body"));
        assert_eq!(out.blocks_redacted, 0);
    }

    #[test]
    fn bare_marker_resolves_from_the_db_record() {
        // Forward-compat with the bare runtime marker (`{{f=uuid}}`, no
        // inline attributes): the DB record is what gates it.
        let input = format!("anchor {{{{f={SAMPLE_UUID_V7}}}}}private body{{{{/}}}} prose");
        let map = db_acl("user:alice", &["group:team"], None);

        let out = render_for_sender(&input, &map, &ReaderView::as_written("alice", &[]));
        assert!(out.text.contains("private body"));

        let out = render_for_sender(
            &input,
            &map,
            &ReaderView::as_written("bob", &groups(&["team"])),
        );
        assert!(out.text.contains("private body"), "allow= from the DB");

        let out = render_for_sender(&input, &map, &ReaderView::as_written("carol", &[]));
        assert!(
            !out.text.contains("private body"),
            "a reader outside the DB record's audience is refused"
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
            &ReaderView::as_written("galadriel", &groups(&["famiglia"])),
        );
        assert!(out.text.contains("Sméagol brontola"));

        let out = render_for_sender(
            &input,
            &map,
            &ReaderView::as_written("bilbo", &groups(&["amici"])),
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
            format!("anchor {{{{subject=user:alice f={other_key}}}}}inline body{{{{/}}}} prose");
        let map = db_acl("global", &[], None); // keyed on SAMPLE_UUID_V7, not other_key
        let out = render_for_sender(&input, &map, &ReaderView::as_written("bob", &[]));
        assert!(
            !out.text.contains("inline body"),
            "an inline subject must still gate an unindexed region"
        );
        let out = render_for_sender(&input, &map, &ReaderView::as_written("alice", &[]));
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
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("alice", &[]));
        insta::assert_snapshot!(out.text);
    }

    #[test]
    fn snapshot_bob_team_member() {
        let input = modello_memoria_5_input();
        let out = render_for_sender(
            &input,
            &no_db(),
            &ReaderView::as_written("bob", &groups(&["team"])),
        );
        insta::assert_snapshot!(out.text);
    }

    #[test]
    fn snapshot_carol_outsider() {
        let input = modello_memoria_5_input();
        let out = render_for_sender(
            &input,
            &no_db(),
            &ReaderView::as_written("carol", &groups(&["sales"])),
        );
        insta::assert_snapshot!(out.text);
    }

    // ---------- admin reveal (dashboard operator override) ----------

    /// The joined output of [`render_admin_reveal_segments`] — what the
    /// dashboard shows once it has stitched the segments back together.
    fn reveal(
        text: &str,
        db_acl: &FactAclMap,
        sender_id: &str,
        sender_groups: &[String],
    ) -> RenderOutput {
        render_admin_reveal_segments(text, db_acl, sender_id, sender_groups).into_output()
    }

    #[test]
    fn admin_reveal_shows_every_region_and_counts_the_hidden_ones() {
        // Carol (an outsider) would normally see only the global region;
        // the operator reveal shows all three, highlighting the two she
        // could not read.
        let input = modello_memoria_5_input();
        let out = reveal(&input, &no_db(), "carol", &groups(&["sales"]));
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
        let out = reveal(&input, &no_db(), "alice", &[]);
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
            "Alice pesa {{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}72 kg{{{{/}}}} \
al 10 maggio, ha {{{{subject=global f={SAMPLE_UUID_V7}}}}}tagliato i capelli{{{{/}}}} ieri."
        );
        let out = reveal(&input, &no_db(), "bob", &[]);
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
            "# Heading\n\n{{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}\n## Secret\nbody\n{{{{/}}}}\n"
        );
        let out = reveal(&input, &no_db(), "bob", &[]);
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
            "{{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}body 1{{{{/}}}}\n\n\
{{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}body 2{{{{/}}}}\n"
        );
        let out = reveal(&input, &no_db(), "bob", &[]);
        assert_ne!(out.text, FULLY_PRIVATE_CALLOUT);
        assert!(out.text.contains("body 1"));
        assert!(out.text.contains("body 2"));
        assert_eq!(out.blocks_revealed, 2);
    }

    // ---------- segments variant (dashboard region → fact click-through) ----------

    #[test]
    fn segments_readable_region_carries_fact_id_and_connective_prose_none() {
        let input = format!(
            "Alice pesa {{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}72 kg{{{{/}}}} al 10 maggio."
        );
        let map = db_acl("user:alice", &[], None);
        let seg = render_for_sender_segments(&input, &map, &ReaderView::as_written("alice", &[]));
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
        let plain = render_for_sender(&input, &map, &ReaderView::as_written("alice", &[]));
        assert_eq!(seg.text(), plain.text);
        assert_eq!(seg.blocks_redacted, plain.blocks_redacted);
    }

    #[test]
    fn segments_redacted_region_is_factless_filler() {
        let input = format!(
            "Alice pesa {{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}72 kg{{{{/}}}} al 10 maggio."
        );
        let map = db_acl("user:alice", &[], None);
        let seg = render_for_sender_segments(&input, &map, &ReaderView::as_written("bob", &[]));
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
        let plain = render_for_sender(&input, &map, &ReaderView::as_written("bob", &[]));
        assert_eq!(seg.text(), plain.text);
    }

    #[test]
    fn segments_map_uncovered_region_is_visible_but_factless() {
        // The region is gated (and readable) via its inline attributes —
        // the DB map does not know the key, so there is no record to link
        // to and the segment stays fact-less.
        let input =
            format!("anchor {{{{subject=global f={SAMPLE_UUID_V7}}}}}public body{{{{/}}}} prose");
        let seg =
            render_for_sender_segments(&input, &no_db(), &ReaderView::as_written("carol", &[]));
        assert_eq!(seg.text(), "anchor public body prose");
        assert!(
            seg.segments.iter().all(|s| s.fact_id.is_none()),
            "an inline-gated region must not advertise a fact record: {:?}",
            seg.segments
        );
    }

    #[test]
    fn segments_total_redaction_collapses_to_one_factless_callout() {
        let input = format!("{{{{subject=user:alice f={SAMPLE_UUID_V7}}}}}body{{{{/}}}}\n");
        let map = db_acl("user:alice", &[], None);
        let seg = render_for_sender_segments(&input, &map, &ReaderView::as_written("bob", &[]));
        assert_eq!(
            seg.segments,
            vec![RenderSegment {
                text: FULLY_PRIVATE_CALLOUT.to_owned(),
                fact_id: None,
            }]
        );
        let plain = render_for_sender(&input, &map, &ReaderView::as_written("bob", &[]));
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
        // Two DB records: the private one (subject alice) reveals for bob,
        // the public one (subject global) is plainly readable.
        let mut map = db_acl("user:alice", &[], None);
        map.insert(
            FactId::parse(public_key).unwrap(),
            RegionAcl {
                excluded: Vec::new(),
                subject: "global".parse().unwrap(),
                allow: vec![],
                sender: None,
            },
        );
        let seg = render_admin_reveal_segments(&input, &map, "bob", &[]);
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
        let plain = reveal(&input, &map, "bob", &[]);
        assert_eq!(seg.text(), plain.text);
        assert_eq!(seg.blocks_revealed, plain.blocks_revealed);
        assert_eq!(seg.blocks_revealed, 1);
    }

    #[test]
    fn snapshot_dave_global_only() {
        let input = modello_memoria_5_input();
        // Dave (an outsider, in `global` only) sees the scaffolding prose and
        // the `owner=global` region. Region 2 (`owner=user:alice`) and region 3
        // (`sender=user:alice`, no subject) are both redacted: the subject-of-last-
        // resort is the region's sender, not a wiki-wide `global` default, so a
        // sender-owned region is not globally readable.
        let out = render_for_sender(&input, &no_db(), &ReaderView::as_written("dave", &[]));
        insta::assert_snapshot!(out.text);
    }
}
