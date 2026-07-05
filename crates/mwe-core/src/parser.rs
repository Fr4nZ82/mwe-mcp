// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inline marker parser. The canonical grammar lives in
//! [marker grammar](../../../wiki/design-notes/marker-grammar.md); this
//! hand-written scanner is the authoritative implementation.
//!
//! Pure function: takes a markdown string, returns a [`ParseOutput`] with an
//! ordered list of [`ParseEvent`]s (prose / region / embed) and a list of
//! recoverable [`ParseWarning`]s. Never panics, never returns a hard error
//! — malformed markers turn into warnings and the offending bytes either
//! become prose or are skipped, so the parser always makes forward progress.
//!
//! ## Grammar (informal — see the wiki page for the EBNF)
//!
//! - `{{` … `}}` — marker open or self-closing. Must stay on a single line
//!   (newlines between `{{` and `}}` produce an `UnclosedMarker` warning).
//! - `{{/}}` — region terminator.
//! - `{{embed=<catalog_id>}}` — self-closing media embed. Legal both in
//!   free prose and *inside* a region body; in-region embeds are collected
//!   into [`ParseEvent::Region::embeds`] while the body bytes stay verbatim.
//! - Otherwise the marker open declares a region; the parser then scans
//!   forward for the next `{{/}}` (region nesting is not supported).
//!
//! Attributes inside the marker open: `owner=`, `allow=` (comma-separated
//! list), `sender=`, `f=` (`UUIDv7`, see [`crate::types::FactId`]). Unknown
//! attributes and malformed values trigger warnings but do not abort the
//! region.
//!
//! ## Scanner strategy
//!
//! Single-pass byte scan over `&str` using `str::find` (Rust's two-way
//! search). No regex, no allocation other than the event payload strings.
//! All boundary offsets used for slicing come from finding ASCII sequences
//! (`{{`, `}}`, `{{/}}`, `\n`), so they always land on UTF-8 character
//! boundaries.

use crate::types::{CatalogId, CatalogIdParseError, FactId, Principal, RegionAttrs};

/// One thing the parser produced from the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseEvent {
    /// Free prose outside of any marker. Inherits `acl_default` for ACL
    /// purposes at render time.
    Prose {
        /// Byte offset of the first prose char in the input.
        start: usize,
        /// Byte offset one past the last prose char in the input.
        end: usize,
        /// Copy of the prose slice.
        text: String,
    },
    /// A marker-delimited region `{{…}}body{{/}}` with optional ACL,
    /// `sender`, and `fact_id`.
    Region {
        /// Byte offset of the opening `{{`.
        start: usize,
        /// Byte offset one past the closing `{{/}}`.
        end: usize,
        /// Parsed attributes from the opening marker.
        attrs: RegionAttrs,
        /// Byte offset of the body (one past the opening `}}`).
        body_start: usize,
        /// Byte offset one past the body (start of `{{/}}`).
        body_end: usize,
        /// Copy of the body slice.
        body: String,
        /// Valid `{{embed=…}}` markers found inside the body, in order.
        /// The body string keeps the marker bytes verbatim — this is the
        /// event-level view export bundling and lint need to *find* them.
        embeds: Vec<CatalogId>,
    },
    /// A self-closing media embed `{{embed=<catalog_id>}}`.
    Embed {
        /// Byte offset of the opening `{{`.
        start: usize,
        /// Byte offset one past the closing `}}`.
        end: usize,
        /// Parsed catalog id.
        catalog_id: CatalogId,
    },
}

/// A recoverable problem the parser encountered while scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseWarning {
    /// Byte offset in the input where the problem originates.
    pub offset: usize,
    /// Coarse classification — useful for tests and for the `wiki_lint`
    /// tool family.
    pub kind: ParseWarningKind,
    /// Human-readable explanation (English, no leading capital).
    pub detail: String,
}

/// Classification of a [`ParseWarning`]. Stable enough that the `wiki_lint`
/// tool can group by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParseWarningKind {
    /// `{{` found but no `}}` on the same line.
    UnclosedMarker,
    /// Region open found but no `{{/}}` terminator before EOF.
    UnclosedRegion,
    /// `{{/}}` encountered with no matching open.
    StrayCloseMarker,
    /// Attribute clause missing `=` or with empty key.
    InvalidAttr,
    /// Attribute value did not parse as a [`Principal`].
    InvalidPrincipal,
    /// Attribute value did not parse as a [`FactId`].
    InvalidFactId,
    /// Embed `catalog_id` did not parse as a [`CatalogId`].
    InvalidCatalogId,
    /// Attribute key not in `{owner, allow, sender, f}`.
    UnknownAttr,
    /// A `{{` that does not open a valid `{{embed=…}}` was found inside an
    /// open region — region nesting is not supported. The outer region is
    /// accepted as-is.
    NestedRegion,
}

/// Result of parsing a single input string.
///
/// `events` is ordered by input position and covers the input without
/// overlap: concatenating the `text` / `body` of all events in order
/// reconstructs the parts of the input the parser preserved (some malformed
/// fragments may have been turned into [`ParseEvent::Prose`] instead of
/// dropped).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParseOutput {
    /// Ordered list of parser events.
    pub events: Vec<ParseEvent>,
    /// Recoverable problems encountered while scanning.
    pub warnings: Vec<ParseWarning>,
}

/// Parse the input string into events + warnings.
#[must_use]
#[allow(clippy::too_many_lines)] // one sequential scanner loop; splitting hides the dispatch order
pub fn parse(text: &str) -> ParseOutput {
    let mut out = ParseOutput::default();
    let mut pos = 0;

    while pos < text.len() {
        let Some(open) = find_from(text, "{{", pos) else {
            push_prose(&mut out.events, text, pos, text.len());
            break;
        };

        if open > pos {
            push_prose(&mut out.events, text, pos, open);
        }

        // The opening `{{` exists. Find the next `}}`. Then enforce the
        // single-line constraint of §3.2 by checking that no `\n` appears
        // between `{{` and `}}`. Doing the line-end scan *before* the `}}`
        // search is what makes that earlier draft accidentally O(n²) on
        // newline-poor inputs — `unwrap_or(text.len())` falls back to a
        // full-tail scan on every marker open.
        let after_open = open + 2;
        let Some(close_attr) = find_from(text, "}}", after_open) else {
            out.warnings.push(ParseWarning {
                offset: open,
                kind: ParseWarningKind::UnclosedMarker,
                detail: "missing `}}` for opening `{{`".into(),
            });
            push_prose(&mut out.events, text, open, text.len());
            break;
        };

        if text.as_bytes()[after_open..close_attr].contains(&b'\n') {
            out.warnings.push(ParseWarning {
                offset: open,
                kind: ParseWarningKind::UnclosedMarker,
                detail: "marker open spans a newline (§3.2 forbids multi-line markers)".into(),
            });
            // Recovery: keep `{{` as prose and advance past it so the
            // scanner can pick up later markers naturally. We do NOT
            // consume the `}}` we just found, because it is not a valid
            // marker close.
            push_prose(&mut out.events, text, open, open + 2);
            pos = open + 2;
            continue;
        }

        let attr_str = &text[after_open..close_attr];
        let marker_end = close_attr + 2; // past `}}`

        // Dispatch on attr_str shape.
        if attr_str == "/" {
            out.warnings.push(ParseWarning {
                offset: open,
                kind: ParseWarningKind::StrayCloseMarker,
                detail: "stray `{{/}}` without a matching open marker".into(),
            });
            push_prose(&mut out.events, text, open, marker_end);
            pos = marker_end;
            continue;
        }

        if let Some(catalog_raw) = attr_str.strip_prefix("embed=") {
            match CatalogId::parse(catalog_raw) {
                Ok(cid) => out.events.push(ParseEvent::Embed {
                    start: open,
                    end: marker_end,
                    catalog_id: cid,
                }),
                Err(CatalogIdParseError::InvalidFormat(_)) => {
                    out.warnings.push(ParseWarning {
                        offset: open,
                        kind: ParseWarningKind::InvalidCatalogId,
                        detail: format!("invalid catalog_id {catalog_raw:?}"),
                    });
                    push_prose(&mut out.events, text, open, marker_end);
                },
            }
            pos = marker_end;
            continue;
        }

        // Region open — find the next `{{/}}` terminator.
        let Some(term) = find_from(text, "{{/}}", marker_end) else {
            out.warnings.push(ParseWarning {
                offset: open,
                kind: ParseWarningKind::UnclosedRegion,
                detail: "missing `{{/}}` terminator for the open region".into(),
            });
            push_prose(&mut out.events, text, open, marker_end);
            pos = marker_end;
            continue;
        };

        // Scan the body for inner `{{`. A valid self-closing
        // `{{embed=<catalog_id>}}` is legal inside a region (the embed
        // rides with its fact) and is collected; any other `{{` is an
        // unsupported nested marker and warns once (the outer region is
        // still accepted — the body bytes are preserved verbatim).
        let mut embeds = Vec::new();
        let mut nested_warned = false;
        let mut scan = marker_end;
        while let Some(inner) = find_from(&text[..term], "{{", scan) {
            let inner_attr_start = inner + 2;
            if text[inner_attr_start..term].starts_with("embed=")
                && let Some(inner_close) = find_from(&text[..term], "}}", inner_attr_start)
                && !text.as_bytes()[inner_attr_start..inner_close].contains(&b'\n')
            {
                let catalog_raw = &text[inner_attr_start + "embed=".len()..inner_close];
                match CatalogId::parse(catalog_raw) {
                    Ok(cid) => embeds.push(cid),
                    Err(CatalogIdParseError::InvalidFormat(_)) => {
                        out.warnings.push(ParseWarning {
                            offset: inner,
                            kind: ParseWarningKind::InvalidCatalogId,
                            detail: format!("invalid catalog_id {catalog_raw:?}"),
                        });
                    },
                }
                scan = inner_close + 2;
                continue;
            }
            if !nested_warned {
                out.warnings.push(ParseWarning {
                    offset: inner,
                    kind: ParseWarningKind::NestedRegion,
                    detail: "nested markers are not supported; outer region kept verbatim".into(),
                });
                nested_warned = true;
            }
            scan = inner + 2;
        }

        let attrs = parse_attrs(attr_str, open, &mut out.warnings);
        let body_start = marker_end;
        let body_end = term;
        let region_end = term + 5; // past `{{/}}`

        out.events.push(ParseEvent::Region {
            start: open,
            end: region_end,
            attrs,
            body_start,
            body_end,
            body: text[body_start..body_end].to_owned(),
            embeds,
        });
        pos = region_end;
    }

    out
}

/// Collect every valid `{{embed=…}}` catalog id in `text`, in document
/// order — both standalone embeds and embeds inside region bodies.
///
/// This is the one enumeration export bundling, the `embed_missing` lint
/// and the dashboard renderer share, so none of them re-implements the
/// in-region scan.
#[must_use]
pub fn collect_embeds(text: &str) -> Vec<CatalogId> {
    let mut out = Vec::new();
    for ev in parse(text).events {
        match ev {
            ParseEvent::Embed { catalog_id, .. } => out.push(catalog_id),
            ParseEvent::Region { embeds, .. } => out.extend(embeds),
            ParseEvent::Prose { .. } => {},
        }
    }
    out
}

/// Check that `text` carries **no marker syntax other than well-formed
/// self-closing `{{embed=…}}` markers**, returning the embeds when it
/// passes.
///
/// This is the relaxation gate the capture-body validators use: a fact
/// body may carry its media embeds (rendered by code, see
/// [media pipeline](../../../wiki/design-notes/media-pipeline.md)) but
/// never region markers, stray braces, or malformed marker fragments —
/// those stay the LLM-injection failure mode the validators exist to
/// block.
#[must_use]
pub fn embed_only_markers(text: &str) -> Option<Vec<CatalogId>> {
    let parsed = parse(text);
    if !parsed.warnings.is_empty() {
        return None;
    }
    let mut embeds = Vec::new();
    for ev in parsed.events {
        match ev {
            ParseEvent::Embed { catalog_id, .. } => embeds.push(catalog_id),
            ParseEvent::Region { .. } => return None,
            ParseEvent::Prose { text, .. } => {
                // A stray `}}` parses silently as prose; keep rejecting it
                // so marker syntax cannot half-survive in a body.
                if text.contains("{{") || text.contains("}}") {
                    return None;
                }
            },
        }
    }
    Some(embeds)
}

/// Remove every valid `{{embed=…}}` marker span from `text`, leaving all
/// other bytes verbatim.
///
/// Used by the similarity surfaces (embedding, n-gram dedup): the marker
/// is a key, not prose — two different photos with near-identical
/// captions must compare by their words, not by their catalog ids.
#[must_use]
pub fn strip_embed_markers(text: &str) -> String {
    let parsed = parse(text);
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for ev in &parsed.events {
        if let ParseEvent::Embed { start, end, .. } = ev {
            out.push_str(&text[last..*start]);
            last = *end;
        }
    }
    out.push_str(&text[last..]);
    // In-region embeds are body bytes, not Embed events — strip them with
    // a second pass over the already-marker-free output only when regions
    // are present.
    if out.contains("{{embed=") {
        let mut cleaned = String::with_capacity(out.len());
        let mut pos = 0;
        while let Some(open) = find_from(&out, "{{embed=", pos) {
            let Some(close) = find_from(&out, "}}", open + 2) else {
                break;
            };
            let raw = &out[open + "{{embed=".len()..close];
            if !raw.contains('\n') && CatalogId::parse(raw).is_ok() {
                cleaned.push_str(&out[pos..open]);
            } else {
                cleaned.push_str(&out[pos..close + 2]);
            }
            pos = close + 2;
        }
        cleaned.push_str(&out[pos..]);
        return cleaned;
    }
    out
}

// ---------- helpers ----------

fn push_prose(events: &mut Vec<ParseEvent>, text: &str, start: usize, end: usize) {
    if start == end {
        return;
    }
    events.push(ParseEvent::Prose {
        start,
        end,
        text: text[start..end].to_owned(),
    });
}

fn find_from(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    haystack[from..].find(needle).map(|p| p + from)
}

fn parse_attrs(spec: &str, marker_offset: usize, warnings: &mut Vec<ParseWarning>) -> RegionAttrs {
    let mut attrs = RegionAttrs::default();
    for clause in spec.split_ascii_whitespace() {
        let Some((key, val)) = clause.split_once('=') else {
            warnings.push(ParseWarning {
                offset: marker_offset,
                kind: ParseWarningKind::InvalidAttr,
                detail: format!("attribute missing `=`: {clause:?}"),
            });
            continue;
        };
        if key.is_empty() {
            warnings.push(ParseWarning {
                offset: marker_offset,
                kind: ParseWarningKind::InvalidAttr,
                detail: format!("empty attribute key in {clause:?}"),
            });
            continue;
        }
        match key {
            "owner" => match val.parse::<Principal>() {
                Ok(p) => attrs.acl.owner = Some(p),
                Err(e) => warnings.push(ParseWarning {
                    offset: marker_offset,
                    kind: ParseWarningKind::InvalidPrincipal,
                    detail: format!("owner={val:?}: {e}"),
                }),
            },
            "allow" => {
                for raw in val.split(',') {
                    match raw.parse::<Principal>() {
                        Ok(p) => attrs.acl.allow.push(p),
                        Err(e) => warnings.push(ParseWarning {
                            offset: marker_offset,
                            kind: ParseWarningKind::InvalidPrincipal,
                            detail: format!("allow has invalid principal {raw:?}: {e}"),
                        }),
                    }
                }
            },
            "sender" => match val.parse::<Principal>() {
                Ok(p) => attrs.sender = Some(p),
                Err(e) => warnings.push(ParseWarning {
                    offset: marker_offset,
                    kind: ParseWarningKind::InvalidPrincipal,
                    detail: format!("sender={val:?}: {e}"),
                }),
            },
            "f" => match FactId::parse(val) {
                Ok(fid) => attrs.fact_id = Some(fid),
                Err(e) => warnings.push(ParseWarning {
                    offset: marker_offset,
                    kind: ParseWarningKind::InvalidFactId,
                    detail: e.to_string(),
                }),
            },
            other => warnings.push(ParseWarning {
                offset: marker_offset,
                kind: ParseWarningKind::UnknownAttr,
                detail: format!("unknown attribute {other:?}"),
            }),
        }
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SAMPLE_UUID_V7;

    // ---------- basic shape ----------

    #[test]
    fn empty_input_produces_no_events() {
        let out = parse("");
        assert!(out.events.is_empty());
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn pure_prose_input_produces_one_prose_event() {
        let out = parse("hello world");
        assert_eq!(out.events.len(), 1);
        assert!(out.warnings.is_empty());
        match &out.events[0] {
            ParseEvent::Prose { start, end, text } => {
                assert_eq!(*start, 0);
                assert_eq!(*end, 11);
                assert_eq!(text, "hello world");
            },
            other => panic!("expected Prose, got {other:?}"),
        }
    }

    // ---------- region happy paths ----------

    #[test]
    fn single_region_with_owner_and_fact_id() {
        let input = format!("{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}body{{{{/}}}}");
        let out = parse(&input);
        assert!(out.warnings.is_empty(), "warnings: {:?}", out.warnings);
        assert_eq!(out.events.len(), 1);
        match &out.events[0] {
            ParseEvent::Region { attrs, body, .. } => {
                assert_eq!(attrs.acl.owner, Some(Principal::User("alice".into())));
                assert!(attrs.acl.allow.is_empty());
                assert_eq!(attrs.sender, None);
                assert_eq!(attrs.fact_id.as_ref().unwrap().as_str(), SAMPLE_UUID_V7);
                assert_eq!(body, "body");
            },
            other => panic!("expected Region, got {other:?}"),
        }
    }

    #[test]
    fn allow_comma_separated_list() {
        let input = "{{owner=user:alice allow=user:bob,group:team}}x{{/}}";
        let out = parse(input);
        assert!(out.warnings.is_empty());
        match &out.events[0] {
            ParseEvent::Region { attrs, .. } => {
                assert_eq!(
                    attrs.acl.allow,
                    vec![
                        Principal::User("bob".into()),
                        Principal::Group("team".into())
                    ]
                );
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn sender_attribute_records_cross_user_attribution() {
        let input = "{{owner=user:gollum sender=user:galadriel allow=group:famiglia}}x{{/}}";
        let out = parse(input);
        assert!(out.warnings.is_empty());
        match &out.events[0] {
            ParseEvent::Region { attrs, .. } => {
                assert_eq!(attrs.acl.owner, Some(Principal::User("gollum".into())));
                assert_eq!(attrs.sender, Some(Principal::User("galadriel".into())));
                assert_eq!(attrs.acl.allow, vec![Principal::Group("famiglia".into())]);
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn region_without_owner_leaves_owner_unset_for_render_fallback() {
        // Region with only fact_id, no owner/allow. The parser leaves
        // acl.owner None; at render time the owner-of-last-resort is the
        // region's own sender (never the wiki principal), and a region with no
        // sender either stays unreadable.
        let input = format!("{{{{f={SAMPLE_UUID_V7}}}}}body{{{{/}}}}");
        let out = parse(&input);
        assert!(out.warnings.is_empty());
        match &out.events[0] {
            ParseEvent::Region { attrs, .. } => {
                assert_eq!(attrs.acl.owner, None);
                assert!(attrs.acl.allow.is_empty());
                assert!(attrs.fact_id.is_some());
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn region_pure_acl_no_fact_id() {
        // A region without `f=` applies its ACL but creates no fact_index
        // entry (see the marker-grammar wiki page).
        let input = "{{allow=group:famiglia}}content{{/}}";
        let out = parse(input);
        assert!(out.warnings.is_empty());
        match &out.events[0] {
            ParseEvent::Region { attrs, .. } => {
                assert!(attrs.fact_id.is_none());
                assert_eq!(attrs.acl.allow, vec![Principal::Group("famiglia".into())]);
            },
            _ => unreachable!(),
        }
    }

    // ---------- inline granularity ----------

    #[test]
    fn multiple_regions_interleaved_with_prose() {
        let input = format!(
            "Alice pesa {{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}72 kg{{{{/}}}} al 10 maggio, \
ha {{{{owner=global f={SAMPLE_UUID_V7}}}}}tagliato i capelli{{{{/}}}} ieri."
        );
        let out = parse(&input);
        assert!(out.warnings.is_empty(), "warnings: {:?}", out.warnings);
        // Expect 5 events: prose, region, prose, region, prose.
        assert_eq!(out.events.len(), 5);
        assert!(matches!(out.events[0], ParseEvent::Prose { .. }));
        assert!(matches!(out.events[1], ParseEvent::Region { .. }));
        assert!(matches!(out.events[2], ParseEvent::Prose { .. }));
        assert!(matches!(out.events[3], ParseEvent::Region { .. }));
        assert!(matches!(out.events[4], ParseEvent::Prose { .. }));
        // Regions have the right owners.
        if let ParseEvent::Region { attrs, .. } = &out.events[1] {
            assert_eq!(attrs.acl.owner, Some(Principal::User("alice".into())));
        }
        if let ParseEvent::Region { attrs, .. } = &out.events[3] {
            assert_eq!(attrs.acl.owner, Some(Principal::global()));
        }
    }

    #[test]
    fn back_to_back_regions_without_prose_between() {
        let input = format!(
            "{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}a{{{{/}}}}{{{{owner=global f={SAMPLE_UUID_V7}}}}}b{{{{/}}}}"
        );
        let out = parse(&input);
        assert!(out.warnings.is_empty());
        assert_eq!(out.events.len(), 2);
        assert!(matches!(out.events[0], ParseEvent::Region { .. }));
        assert!(matches!(out.events[1], ParseEvent::Region { .. }));
    }

    // ---------- embed ----------

    #[test]
    fn embed_self_closing_in_prose() {
        let input = "before {{embed=c-2026-05-10-foto-001.jpg}} after";
        let out = parse(input);
        assert!(out.warnings.is_empty());
        assert_eq!(out.events.len(), 3);
        assert!(matches!(out.events[0], ParseEvent::Prose { .. }));
        match &out.events[1] {
            ParseEvent::Embed { catalog_id, .. } => {
                assert_eq!(catalog_id.as_str(), "c-2026-05-10-foto-001.jpg");
            },
            other => panic!("expected Embed, got {other:?}"),
        }
        assert!(matches!(out.events[2], ParseEvent::Prose { .. }));
    }

    #[test]
    fn embed_with_invalid_catalog_id_warns_and_becomes_prose() {
        let input = "before {{embed=garbage}} after";
        let out = parse(input);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::InvalidCatalogId);
        // The malformed embed survives as prose so we don't lose bytes.
        assert_eq!(out.events.len(), 3);
        assert!(matches!(out.events[1], ParseEvent::Prose { .. }));
    }

    // ---------- invalid attribute recovery ----------

    #[test]
    fn owner_without_prefix_warns_but_keeps_region() {
        let input = "{{owner=alice}}body{{/}}";
        let out = parse(input);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::InvalidPrincipal);
        // Region still emitted, just without an owner set.
        assert_eq!(out.events.len(), 1);
        if let ParseEvent::Region { attrs, body, .. } = &out.events[0] {
            assert_eq!(attrs.acl.owner, None);
            assert_eq!(body, "body");
        }
    }

    #[test]
    fn unknown_attribute_warns_but_keeps_region() {
        let input = "{{owner=user:alice nonsense=foo}}body{{/}}";
        let out = parse(input);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::UnknownAttr);
        assert_eq!(out.events.len(), 1);
        if let ParseEvent::Region { attrs, .. } = &out.events[0] {
            assert_eq!(attrs.acl.owner, Some(Principal::User("alice".into())));
        }
    }

    #[test]
    fn legacy_fact_id_format_warns_but_keeps_region() {
        let input = "{{owner=user:alice f=f-2026-05-11-001}}body{{/}}";
        let out = parse(input);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::InvalidFactId);
        if let ParseEvent::Region { attrs, .. } = &out.events[0] {
            // Owner survives, fact_id is None because parse failed.
            assert_eq!(attrs.acl.owner, Some(Principal::User("alice".into())));
            assert!(attrs.fact_id.is_none());
        }
    }

    #[test]
    fn attribute_without_equals_warns_and_skips_clause() {
        let input = "{{owner=user:alice noequals}}body{{/}}";
        let out = parse(input);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::InvalidAttr);
    }

    // ---------- malformed markers ----------

    #[test]
    fn unclosed_open_marker_at_eof_warns() {
        let input = "prose {{owner=user:alice";
        let out = parse(input);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::UnclosedMarker);
        // The bytes from `{{` to EOF survive as prose.
        let joined: String = out
            .events
            .iter()
            .filter_map(|e| match e {
                ParseEvent::Prose { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined, input);
    }

    #[test]
    fn marker_open_spanning_newline_is_unclosed() {
        // Marker open/close must be on a single line.
        let input = "prose {{owner=\nuser:alice}}body{{/}}";
        let out = parse(input);
        assert!(
            out.warnings
                .iter()
                .any(|w| w.kind == ParseWarningKind::UnclosedMarker)
        );
    }

    #[test]
    fn unclosed_region_warns() {
        let input = "{{owner=user:alice}}body without terminator";
        let out = parse(input);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::UnclosedRegion);
    }

    #[test]
    fn stray_close_marker_warns_and_becomes_prose() {
        let input = "before {{/}} after";
        let out = parse(input);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::StrayCloseMarker);
        assert!(
            out.events
                .iter()
                .all(|e| matches!(e, ParseEvent::Prose { .. }))
        );
    }

    #[test]
    fn nested_region_warns_outer_kept_verbatim() {
        let input = "{{owner=user:alice}}outer {{owner=user:bob}}inner{{/}} rest{{/}}";
        let out = parse(input);
        assert!(
            out.warnings
                .iter()
                .any(|w| w.kind == ParseWarningKind::NestedRegion)
        );
        // The outer region accepts the body verbatim, terminating at the
        // FIRST `{{/}}` — so its body is `outer {{owner=user:bob}}inner`
        // (no inner termination logic — nesting is unsupported).
        match &out.events[0] {
            ParseEvent::Region { body, .. } => {
                assert!(body.starts_with("outer "));
            },
            _ => unreachable!(),
        }
    }

    // ---------- in-region embeds ----------

    #[test]
    fn embed_inside_region_is_legal_and_collected() {
        let input = format!(
            "{{{{owner=user:gollum f={SAMPLE_UUID_V7}}}}}an 8 in maths \
{{{{embed=c-2026-05-11-photo-001.jpg}}}}{{{{/}}}}"
        );
        let out = parse(&input);
        assert!(out.warnings.is_empty(), "warnings: {:?}", out.warnings);
        assert_eq!(out.events.len(), 1);
        match &out.events[0] {
            ParseEvent::Region { body, embeds, .. } => {
                // Body bytes keep the marker verbatim.
                assert!(body.contains("{{embed=c-2026-05-11-photo-001.jpg}}"));
                assert_eq!(embeds.len(), 1);
                assert_eq!(embeds[0].as_str(), "c-2026-05-11-photo-001.jpg");
            },
            other => panic!("expected Region, got {other:?}"),
        }
    }

    #[test]
    fn multiple_embeds_inside_region_collected_in_order() {
        let input = "{{owner=user:alice}}a {{embed=c-2026-05-10-photo-001.jpg}} b \
{{embed=c-2026-05-10-photo-002.png}} c{{/}}";
        let out = parse(input);
        assert!(out.warnings.is_empty(), "warnings: {:?}", out.warnings);
        match &out.events[0] {
            ParseEvent::Region { embeds, .. } => {
                let ids: Vec<&str> = embeds.iter().map(CatalogId::as_str).collect();
                assert_eq!(
                    ids,
                    vec!["c-2026-05-10-photo-001.jpg", "c-2026-05-10-photo-002.png"]
                );
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn invalid_embed_inside_region_warns_invalid_catalog_id() {
        let input = "{{owner=user:alice}}x {{embed=garbage}} y{{/}}";
        let out = parse(input);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::InvalidCatalogId);
        // Region survives with empty embeds; body keeps the bytes.
        match &out.events[0] {
            ParseEvent::Region { body, embeds, .. } => {
                assert!(embeds.is_empty());
                assert!(body.contains("{{embed=garbage}}"));
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn embed_and_nested_marker_inside_region_each_classified() {
        let input = "{{owner=user:alice}}x {{embed=c-2026-05-10-doc-001.pdf}} \
{{owner=user:bob}}nested{{/}}";
        let out = parse(input);
        // One NestedRegion for the `{{owner=user:bob}}`, no warning for
        // the valid embed.
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].kind, ParseWarningKind::NestedRegion);
        match &out.events[0] {
            ParseEvent::Region { embeds, .. } => {
                assert_eq!(embeds.len(), 1);
            },
            _ => unreachable!(),
        }
    }

    // ---------- embed helpers ----------

    #[test]
    fn collect_embeds_finds_standalone_and_in_region() {
        let input = format!(
            "{{{{embed=c-2026-05-10-photo-001.jpg}}}} prose \
{{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}body {{{{embed=c-2026-05-11-audio-001.m4a}}}}{{{{/}}}}"
        );
        let ids: Vec<String> = collect_embeds(&input)
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect();
        assert_eq!(
            ids,
            vec!["c-2026-05-10-photo-001.jpg", "c-2026-05-11-audio-001.m4a"]
        );
    }

    #[test]
    fn embed_only_markers_accepts_prose_plus_embeds() {
        let body = "Sunset over the harbour {{embed=c-2026-06-12-photo-001.jpg}}";
        let embeds = embed_only_markers(body).expect("valid embed-only body");
        assert_eq!(embeds.len(), 1);
    }

    #[test]
    fn embed_only_markers_rejects_regions_braces_and_malformed() {
        assert!(embed_only_markers("{{owner=user:alice}}x{{/}}").is_none());
        assert!(embed_only_markers("stray }} braces").is_none());
        assert!(embed_only_markers("open {{ brace").is_none());
        assert!(embed_only_markers("{{embed=garbage}}").is_none());
        assert_eq!(embed_only_markers("plain prose").map(|v| v.len()), Some(0));
    }

    #[test]
    fn strip_embed_markers_removes_standalone_and_in_region_spans() {
        let standalone = "sunset {{embed=c-2026-06-12-photo-001.jpg}} harbour";
        assert_eq!(strip_embed_markers(standalone), "sunset  harbour");

        let in_region = "{{owner=user:alice}}body {{embed=c-2026-06-12-photo-001.jpg}} tail{{/}}";
        assert_eq!(
            strip_embed_markers(in_region),
            "{{owner=user:alice}}body  tail{{/}}"
        );

        // Invalid embeds and plain text are untouched.
        assert_eq!(
            strip_embed_markers("{{embed=garbage}}"),
            "{{embed=garbage}}"
        );
        assert_eq!(strip_embed_markers("plain"), "plain");
    }

    // ---------- offset invariants ----------

    #[test]
    fn event_offsets_are_monotonic_and_in_bounds() {
        let input = format!(
            "lead {{{{owner=user:alice f={SAMPLE_UUID_V7}}}}}mid{{{{/}}}} trail \
{{{{embed=c-2026-05-10-foto-001.jpg}}}} tail"
        );
        let out = parse(&input);
        let mut last_end = 0;
        for ev in &out.events {
            let (s, e) = event_span(ev);
            assert!(s >= last_end, "events overlap at {s}");
            assert!(e <= input.len());
            assert!(s < e || matches!(ev, ParseEvent::Prose { .. }) && s == e);
            last_end = e;
        }
    }

    fn event_span(ev: &ParseEvent) -> (usize, usize) {
        match ev {
            ParseEvent::Prose { start, end, .. }
            | ParseEvent::Region { start, end, .. }
            | ParseEvent::Embed { start, end, .. } => (*start, *end),
        }
    }

    // ---------- proptest invariants ----------

    use proptest::prelude::*;

    proptest! {
        /// On any input the parser must terminate, never panic, and produce
        /// events whose spans are monotonic and contained in `0..input.len()`.
        #[test]
        fn parser_terminates_with_sane_offsets(input in ".{0,512}") {
            let out = parse(&input);
            let mut last_end = 0usize;
            for ev in &out.events {
                let (s, e) = event_span(ev);
                prop_assert!(s >= last_end, "overlap at {} (input {:?})", s, input);
                prop_assert!(e <= input.len(), "event end {} > input len {}", e, input.len());
                prop_assert!(s <= e, "event start {} > end {}", s, e);
                last_end = e;
            }
        }

        /// Inputs without `{{` always parse to a single Prose event covering
        /// the whole input.
        #[test]
        fn input_without_open_marker_is_single_prose(s in "[^{]{1,256}") {
            let out = parse(&s);
            prop_assert_eq!(out.events.len(), 1);
            prop_assert!(out.warnings.is_empty());
            if let ParseEvent::Prose { text, .. } = &out.events[0] {
                prop_assert_eq!(text, &s);
            } else {
                prop_assert!(false, "expected Prose, got {:?}", out.events[0]);
            }
        }
    }
}
