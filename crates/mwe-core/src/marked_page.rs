// SPDX-License-Identifier: AGPL-3.0-or-later
//! A page of the memory, rendered so a model can name the facts on it.
//!
//! **The prose is the thing that ties a page's facts together, and it is what
//! a person would read to judge them.** Every stage that asks a model about
//! stored facts used to hand it a list of lines — one fact per row, in
//! whatever order a ranking put them — which is the memory with the joins
//! taken out. This is the other rendering: the page as the Cronista wrote it,
//! with each fact wrapped in a marker the model answers by.
//!
//! **Markers, not identifiers.** A model asked to copy a `fact_id` copies it
//! wrongly often enough to matter; asked for `f3` it says `f3`. The caller
//! holds the list that maps back, so an answer naming a marker nobody handed
//! out is refused by name rather than acted on.
//!
//! One walk, two readers. The nightly page judge shows its pages to the engine
//! itself, so a region it cannot identify keeps its words as the scaffolding
//! they are. The turn's reconciler shows them to a model acting for one
//! PERSON, and there a region that person may not read must not appear at all
//! — which is the same rule the read path applies when it serves a page
//! ([`UnknownRegions`]).

use crate::parser::{ParseEvent, parse};

/// What to do with a region whose fact the caller did not name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownRegions {
    /// Keep the words, without a marker: they are prose the reader is entitled
    /// to, and nothing can be said about them because they carry no marker.
    ///
    /// For a reader who is the engine, and for regions the caller left out for
    /// reasons of its own (a retired fact whose bytes are still on the page).
    Keep,
    /// Drop the words entirely.
    ///
    /// For a page served on behalf of a person: the caller's list is what that
    /// person may read, so anything else on the page is somebody else's and
    /// does not exist as far as this reading goes.
    Redact,
}

/// One page, marked up.
#[derive(Debug, Clone, Default)]
pub struct MarkedPage {
    /// The page's prose, with the facts the caller named wrapped in
    /// `<fN>…</fN>`.
    pub prose: String,
    /// Marker order: `order[n]` is the caller's index for the `n`-th marker
    /// this page handed out, counting from `first_marker`.
    ///
    /// The facts woven into the prose come first, in the order the PAGE has
    /// them — which is the order the model reads — and the caller's remaining
    /// facts follow in its own order.
    pub order: Vec<usize>,
    /// How many of `order` are woven into [`Self::prose`]; the rest have no
    /// place on the page yet and the caller lists them itself.
    pub woven: usize,
}

/// Walk a page and hand out the markers.
///
/// `ids` are the caller's facts, in the caller's order; the returned
/// [`MarkedPage::order`] indexes back into it. `ceiling_bytes` bounds the
/// prose: a page is normally a few paragraphs, and this is the guard against
/// the one that is not, because the whole of it goes into a prompt. What falls
/// past the cut simply has no marker, so nothing can be said about it.
///
/// `first_marker` is where this page's numbering starts. One page to a call
/// starts at 1; a caller showing several pages in one prompt carries the count
/// forward, because a marker has to name one fact across the whole of what the
/// model was shown.
#[must_use]
pub fn mark_up(
    body: &str,
    ids: &[&str],
    unknown: UnknownRegions,
    ceiling_bytes: usize,
    first_marker: usize,
) -> MarkedPage {
    use std::fmt::Write as _;

    let mut by_id: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (i, id) in ids.iter().enumerate() {
        by_id.insert(*id, i);
    }
    let mut taken = vec![false; ids.len()];
    let mut order: Vec<usize> = Vec::with_capacity(ids.len());
    let mut prose = String::with_capacity(body.len() + 64);

    for event in parse(body).events {
        match event {
            ParseEvent::Prose { text, .. } => prose.push_str(&text),
            ParseEvent::Region {
                attrs, body: span, ..
            } => {
                let mine = attrs
                    .fact_id
                    .as_ref()
                    .and_then(|id| by_id.get(id.as_str()).copied())
                    .filter(|i| !taken[*i]);
                match (mine, unknown) {
                    (Some(i), _) => {
                        taken[i] = true;
                        order.push(i);
                        let marker = first_marker + order.len() - 1;
                        let _ = write!(prose, "<f{marker}>{}</f{marker}>", span.trim());
                    },
                    // Not one of the caller's, and the caller keeps such words.
                    (None, UnknownRegions::Keep) => prose.push_str(&span),
                    // Not one of the caller's, and the caller may not show it.
                    (None, UnknownRegions::Redact) => {},
                }
            },
            // A media embed is a pointer, not a claim: nothing can be said
            // about it and it costs prompt to carry.
            ParseEvent::Embed { .. } => {},
        }
        if prose.len() > ceiling_bytes {
            break;
        }
    }

    let woven = order.len();
    for (i, _) in ids.iter().enumerate() {
        if !taken[i] {
            order.push(i);
        }
    }
    MarkedPage {
        prose: prose.trim().to_owned(),
        order,
        woven,
    }
}

/// The `f7` a model answers with, as the number 7.
///
/// Generous about the spelling — `f7`, `F7`, `<f7>`, `7` — because the marker
/// is the model's only way to name a fact, and a bracket it copied off the
/// page is not a reason to drop a true verdict.
#[must_use]
pub fn marker_number(raw: &str) -> Option<usize> {
    let cleaned: String = raw
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_start_matches(['f', 'F'])
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    cleaned.parse().ok().filter(|n| *n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "018f1234-5678-7abc-9def-00000000000a";
    const B: &str = "018f1234-5678-7abc-9def-00000000000b";
    const HIDDEN: &str = "018f1234-5678-7abc-9def-00000000000c";

    fn page() -> String {
        format!(
            "Pepper is the household cat. {{{{f={B}}}}}She has been fed.{{{{/}}}} \
             {{{{f={HIDDEN}}}}}The vet bill was 90 euros.{{{{/}}}} \
             {{{{f={A}}}}}Alice asked somebody to feed the cat.{{{{/}}}}\n"
        )
    }

    /// The markers follow the PAGE, not the caller's list, because the page is
    /// what the model reads.
    #[test]
    fn the_markers_are_handed_out_in_reading_order() {
        let marked = mark_up(&page(), &[A, B], UnknownRegions::Keep, 8_000, 1);
        assert!(
            marked.prose.contains("<f1>She has been fed.</f1>"),
            "{}",
            marked.prose
        );
        assert!(
            marked
                .prose
                .contains("<f2>Alice asked somebody to feed the cat.</f2>"),
            "{}",
            marked.prose
        );
        assert_eq!(marked.order, vec![1, 0], "f1 is B, f2 is A");
        assert_eq!(marked.woven, 2);
        assert!(
            !marked.prose.contains("{{f="),
            "the runtime markers never reach a model: {}",
            marked.prose
        );
    }

    /// **A fact the caller did not name is not nameable — and where the caller
    /// is acting for a person, it is not even there.**
    ///
    /// This is the read path's rule applied to a prompt: the caller's list is
    /// what that person may read, so the rest of the page is somebody else's.
    #[test]
    fn a_region_the_caller_did_not_name_is_kept_or_redacted() {
        let kept = mark_up(&page(), &[A, B], UnknownRegions::Keep, 8_000, 1);
        assert!(
            kept.prose.contains("The vet bill was 90 euros."),
            "kept as scaffolding, with no marker: {}",
            kept.prose
        );

        let redacted = mark_up(&page(), &[A, B], UnknownRegions::Redact, 8_000, 1);
        assert!(
            !redacted.prose.contains("vet bill"),
            "a person who cannot read it must not be shown it: {}",
            redacted.prose
        );
        assert!(
            redacted.prose.contains("Pepper is the household cat."),
            "the prose between the facts stays — it carries no ACL of its own: {}",
            redacted.prose
        );
        assert_eq!(redacted.order, kept.order, "and the markers are the same");
    }

    /// A fact of the caller's that the prose has not taken yet keeps a marker
    /// all the same: the caller lists it, and it is nameable like the rest.
    #[test]
    fn a_fact_the_prose_has_not_taken_still_gets_a_marker() {
        let marked = mark_up(
            &page(),
            &[A, B, HIDDEN, "018f1234-5678-7abc-9def-00000000000d"],
            UnknownRegions::Keep,
            8_000,
            1,
        );
        assert_eq!(marked.woven, 3, "three are on the page");
        assert_eq!(marked.order.len(), 4, "and the fourth is still nameable");
        assert_eq!(marked.order[3], 3);
    }

    /// **A prompt that shows several pages numbers them once, across the
    /// lot.** A marker names one fact in what the model was shown, and two
    /// pages each starting at `f1` would give it two facts with one name.
    #[test]
    fn the_numbering_carries_across_pages() {
        let first = mark_up(&page(), &[A, B], UnknownRegions::Redact, 8_000, 1);
        assert_eq!(first.woven, 2);
        let second = mark_up(
            &format!("{{{{f={HIDDEN}}}}}The vet bill was 90 euros.{{{{/}}}}\n"),
            &[HIDDEN],
            UnknownRegions::Redact,
            8_000,
            1 + first.woven,
        );
        assert!(
            second.prose.contains("<f3>The vet bill was 90 euros.</f3>"),
            "the second page carries on from the first: {}",
            second.prose
        );
    }

    #[test]
    fn a_marker_is_read_however_the_model_spells_it() {
        assert_eq!(marker_number("f7"), Some(7));
        assert_eq!(marker_number("F7"), Some(7));
        assert_eq!(marker_number("<f7>"), Some(7));
        assert_eq!(marker_number(" f7 "), Some(7));
        assert_eq!(marker_number("7"), Some(7));
        assert_eq!(marker_number("f0"), None, "the markers start at one");
        assert_eq!(marker_number("the cat"), None);
        assert_eq!(marker_number(""), None);
    }
}
