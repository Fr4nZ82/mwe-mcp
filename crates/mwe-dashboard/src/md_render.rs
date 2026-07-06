// SPDX-License-Identifier: AGPL-3.0-or-later
//! Markdown → HTML rendering for the dashboard wiki page preview.
//!
//! Built on top of [`pulldown_cmark`] with four opinionated tweaks:
//!
//! 1. **Heading anchors match the comment grammar.** Every
//!    rendered `<hN>` element gets an `id` attribute computed from
//!    the heading text via [`mwe_core::briefing::slug_from_heading`]
//!    — the same function the comment write path feeds into
//!    `?anchor=<slug>`. This is the load-bearing invariant: a
//!    comment recorded with `target_cite=wiki://<id>/<path>#<slug>`
//!    can reach its heading on the rendered preview.
//!
//! 2. **Raw HTML in markdown is dropped.** `pulldown_cmark` by
//!    default surfaces source `<script>`, `<style>`, `<div>` etc.
//!    as `Event::Html` / `Event::InlineHtml`. We filter both —
//!    defense in depth against a smart consumer or an operator
//!    pasting unsafe markup into a page. Inline markdown (bold,
//!    italic, code spans, links, lists, tables, strikethrough,
//!    task lists, smart quotes) is rendered normally.
//!
//! 3. **`{{embed=<catalog_id>}}` markers render as media elements.**
//!    A valid embed marker in prose becomes an `<img>` / `<video>` /
//!    `<audio>` / download link pointed at the dashboard's
//!    cookie-authenticated media alias (`/dashboard/media/<id>` — the
//!    session cookie is path-scoped to `/dashboard`, so the root
//!    `GET /media` mount would arrive unauthenticated from the
//!    browser). The page-level redaction already ran before this
//!    renderer; per-blob denial happens at `GET` time (a denied byte
//!    fetch shows as a broken image — the marker may be visible while
//!    the bytes are not, by design of the
//!    media pipeline).
//!    Markers inside code blocks and code spans stay literal so
//!    documentation about the marker grammar renders as text.
//!
//! 4. **Canonical wikilinks click through (page surfaces only).** With a
//!    [`PageRenderContext`], `[[wiki_id]]` / `[[wiki_id/page-slug]]` /
//!    `[[target|display]]` become in-dashboard `<a>` navigation per the
//!    link grammar of
//!    recall-pipeline.md:
//!    the context's resolver maps the target (alias already stripped) to
//!    an href, an unresolved target stays literal text (never a broken
//!    link), the label is emitted as a text event so pulldown escapes it
//!    (no HTML injection through an alias). Linkification runs **after
//!    redaction** by construction — this renderer only ever sees the
//!    declassified text. `{{factref=<fact_id>}}` markers (inserted by the
//!    page view from the segmented render, one per fact-backed region)
//!    become small superscript anchors to the fact's record. Both stay
//!    literal inside code, and both are inert without a context (the
//!    chat-reply render never linkifies).
//!
//! The exported entry points:
//!
//! - [`render`] — straight markdown → safe HTML, no per-heading
//!   post-processing. Use this for "just show me the markdown
//!   pretty" surfaces (e.g. the operator chat reply).
//! - [`render_with_heading_injections`] — same, but takes a
//!   `FnMut(&str) -> Option<String>` callback that fires once per
//!   heading with the computed slug. Whatever the callback returns
//!   is emitted as raw HTML right after the heading's closing tag.
//!   The comment viewer uses this to interleave the
//!   "+ Comment on #slug" CTA and the inline comment blocks.
//! - [`render_page`] — the memory-explorer page surface: reveal switch,
//!   heading injections, and a [`PageRenderContext`] for wikilink
//!   click-through + fact-ref anchors.

use mwe_core::briefing::slug_from_heading;
use mwe_core::types::{CatalogId, FactId};
use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Per-render context for the memory-explorer page surfaces
/// (`/dashboard/wiki/:id` + `/dashboard/wiki/:id/view/*path`).
pub struct PageRenderContext<'a> {
    /// Resolve one raw wikilink target (the part before any `|` alias,
    /// e.g. `famiglia` or `famiglia/albero_genealogico`) to an
    /// in-dashboard href. Return `None` for a target that does not
    /// resolve against the wiki tree — the literal text then stays in
    /// the prose instead of a dead link. The returned href is emitted
    /// into an attribute (it is attribute-escaped here, but the
    /// resolver is expected to build it from validated, URL-encoded
    /// components).
    pub resolve_wikilink: &'a dyn Fn(&str) -> Option<String>,
    /// Rewrite `{{factref=<fact_id>}}` markers into superscript anchors
    /// to `/dashboard/facts/<id>/edit` (the fact's record). Enabled only
    /// on renders whose input was annotated from the segmented render.
    pub fact_refs: bool,
}

/// Render `body` as safe HTML. No per-heading injection, no link
/// context.
#[must_use]
pub fn render(body: &str) -> String {
    render_with_heading_injections(body, |_| None)
}

/// The memory-explorer page render.
///
/// `reveal` picks the admin ACL-reveal wrapper pass-through (see
/// [`render_reveal`]), `ctx` enables wikilink click-through + fact-ref
/// anchors, and `inject_after_heading` is the per-heading comment/CTA
/// hook of [`render_with_heading_injections`].
pub fn render_page<F>(
    body: &str,
    reveal: bool,
    ctx: &PageRenderContext<'_>,
    inject_after_heading: F,
) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    render_inner(body, reveal, Some(ctx), inject_after_heading)
}

/// Like [`render`], but for the dashboard's **admin ACL-reveal** view.
///
/// The four [`mwe_core::render`] reveal wrapper tags
/// ([`mwe_core::render::ACL_REVEAL_BLOCK_OPEN`] et al.) are passed
/// through so revealed-private fragments can be coloured, while every
/// other raw HTML tag is still dropped. Use only on the operator's
/// reveal-mode page; the default [`render`] keeps stripping all HTML.
#[must_use]
pub fn render_reveal(body: &str) -> String {
    render_inner(body, true, None, |_| None)
}

/// [`render_with_heading_injections`] for the admin ACL-reveal view —
/// see [`render_reveal`] for what "reveal" relaxes.
pub fn render_reveal_with_heading_injections<F>(body: &str, inject_after_heading: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    render_inner(body, true, None, inject_after_heading)
}

/// Trimmed-equality membership test for the reveal wrapper tags. pulldown
/// hands us the source HTML verbatim (often with a trailing newline for a
/// block tag), so we trim before comparing against the canonical literals.
fn reveal_block_open(s: &str) -> bool {
    s.trim() == mwe_core::render::ACL_REVEAL_BLOCK_OPEN
}
fn reveal_block_close(s: &str) -> bool {
    s.trim() == mwe_core::render::ACL_REVEAL_BLOCK_CLOSE
}
fn reveal_inline_open(s: &str) -> bool {
    s.trim() == mwe_core::render::ACL_REVEAL_INLINE_OPEN
}
fn reveal_inline_close(s: &str) -> bool {
    s.trim() == mwe_core::render::ACL_REVEAL_INLINE_CLOSE
}

/// Render `body` as safe HTML, firing `inject_after_heading` once per heading.
///
/// The callback receives the computed slug for each heading; its
/// `Some(html)` return value is emitted as raw HTML directly after the
/// heading's closing tag, while `None` skips the injection.
///
/// Slugs are computed with [`slug_from_heading`] over the
/// concatenation of the heading's text + inline-code segments — no
/// `**bold**` markers, no link URLs. For the typical headings used
/// by the engine (`## Boundary tokens`, `### resolve_scope_principal`,
/// `# Parser`) the result matches what
/// [`mwe_core::briefing::extract_anchors_from_markdown`] would
/// produce when fed the same markdown line, so existing comment
/// `target_cite` rows continue to anchor cleanly. Pathological
/// headings with links or images inside diverge — corner case,
/// accepted.
pub fn render_with_heading_injections<F>(body: &str, inject_after_heading: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    render_inner(body, false, None, inject_after_heading)
}

/// Shared body of every public entry point. `allow_reveal`: when set, the
/// four `mwe_core::render` reveal wrapper tags are passed through
/// (depth-tracked so a stray close tag from page content cannot leak);
/// every other raw HTML tag is dropped either way. `ctx` enables the
/// wikilink / fact-ref rewrites of [`PageRenderContext`].
///
/// Consecutive `Event::Text` runs are **coalesced before rewriting**:
/// pulldown tokenizes `[[bob]]` as five separate text events
/// (`[`,`[`,`bob`,`]`,`]` — unresolved link brackets), so a per-event
/// scan would never see a whole wikilink. Coalescing is semantically
/// neutral for plain text (adjacent text events concatenate in the
/// output anyway).
fn render_inner<F>(
    body: &str,
    allow_reveal: bool,
    ctx: Option<&PageRenderContext<'_>>,
    mut inject_after_heading: F,
) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    // Deliberately NOT enabling raw HTML — Event::Html and
    // Event::InlineHtml are filtered out below.

    let parser = Parser::new_ext(body, options);
    let mut out_events: Vec<Event<'_>> = Vec::new();
    let mut iter = parser.into_iter();
    let mut in_code_block = false;
    // The pending run of consecutive text events (outside code blocks),
    // flushed through the rewriter at the next non-text event.
    let mut text_run = String::new();
    // Depth of currently-open reveal wrappers, so a matching close is
    // only kept when we actually opened one (a forged `</div>` / `</span>`
    // in page content is still dropped).
    let mut reveal_div_depth = 0usize;
    let mut reveal_span_depth = 0usize;

    while let Some(event) = iter.next() {
        // Accumulate prose text; anything else flushes the run first so
        // the rewriter sees whole wikilinks/markers.
        if let Event::Text(t) = &event
            && !in_code_block
        {
            text_run.push_str(t);
            continue;
        }
        flush_text_run(&mut text_run, &mut out_events, ctx);
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let level_num = heading_level_to_u8(level);
                // Buffer the inner events + accumulate the visible
                // text so the slug derivation sees a clean string.
                // Inner text runs go through the same rewriter (a
                // wikilink inside a heading still clicks through); the
                // slug is computed from the RAW text — stable against
                // linkification — with the synthetic `{{factref=…}}`
                // markers stripped (they are per-render annotations,
                // never part of the heading's identity).
                let mut inner: Vec<Event<'_>> = Vec::new();
                let mut inner_run = String::new();
                let mut inner_text = String::new();
                for nested in iter.by_ref() {
                    if matches!(&nested, Event::End(TagEnd::Heading(_))) {
                        break;
                    }
                    match nested {
                        Event::Text(t) => {
                            inner_text.push_str(&t);
                            inner_run.push_str(&t);
                        },
                        other => {
                            if let Event::Code(t) = &other {
                                inner_text.push_str(t);
                            }
                            flush_text_run(&mut inner_run, &mut inner, ctx);
                            inner.push(other);
                        },
                    }
                }
                flush_text_run(&mut inner_run, &mut inner, ctx);
                let slug = slug_from_heading(&strip_factref_markers(&inner_text));
                out_events.push(Event::Html(format!(r#"<h{level_num} id="{slug}">"#).into()));
                out_events.extend(inner);
                out_events.push(Event::Html(format!("</h{level_num}>").into()));
                if let Some(injection) = inject_after_heading(&slug) {
                    out_events.push(Event::Html(injection.into()));
                }
            },
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                out_events.push(event);
            },
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                out_events.push(event);
            },
            // Admin reveal: keep the (depth-balanced) block wrapper a
            // revealed region is fenced with; drop everything else.
            Event::Html(h) if allow_reveal => {
                let keep = if reveal_block_open(&h) {
                    reveal_div_depth += 1;
                    true
                } else if reveal_block_close(&h) && reveal_div_depth > 0 {
                    reveal_div_depth -= 1;
                    true
                } else {
                    false
                };
                if keep {
                    out_events.push(Event::Html(h));
                }
            },
            // Admin reveal: same, for the inline (span) wrapper.
            Event::InlineHtml(h) if allow_reveal => {
                let keep = if reveal_inline_open(&h) {
                    reveal_span_depth += 1;
                    true
                } else if reveal_inline_close(&h) && reveal_span_depth > 0 {
                    reveal_span_depth -= 1;
                    true
                } else {
                    false
                };
                if keep {
                    out_events.push(Event::InlineHtml(h));
                }
            },
            // Drop raw HTML from source — safety by construction.
            Event::Html(_) | Event::InlineHtml(_) => {},
            other => out_events.push(other),
        }
    }
    flush_text_run(&mut text_run, &mut out_events, ctx);

    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, out_events.into_iter());
    html
}

/// The next rewritable token in a text run.
#[derive(Clone, Copy)]
enum TextToken {
    Embed,
    FactRef,
    WikiLink,
}

/// Emit the pending literal chunk `from..upto` of `text` as an escaped
/// text event.
fn emit_literal(text: &str, from: usize, upto: usize, out: &mut Vec<Event<'_>>) {
    if upto > from {
        out.push(Event::Text(CowStr::from(text[from..upto].to_owned())));
    }
}

/// Parse one wikilink body (the text between `[[` and `]]`) and resolve
/// it: `Some((href, label))` when the target resolves — the label is the
/// `|display` alias when present, the full target otherwise — `None` for
/// an empty or unresolved target.
fn resolve_wikilink_parts(
    inner: &str,
    resolve: &dyn Fn(&str) -> Option<String>,
) -> Option<(String, String)> {
    let target = inner.split('|').next().unwrap_or(inner).trim();
    if target.is_empty() {
        return None;
    }
    let href = resolve(target)?;
    let label = inner
        .split_once('|')
        .map(|(_, l)| l.trim())
        .filter(|l| !l.is_empty())
        .unwrap_or(target);
    Some((href, label.to_owned()))
}

/// Flush one coalesced text run through the rewriters: `{{embed=…}}`
/// markers become trusted media elements (always), `{{factref=…}}`
/// markers become fact-record anchors and `[[wikilinks]]` become
/// in-dashboard navigation (only with a [`PageRenderContext`]). Prose
/// between tokens is emitted as text events (escaped by pulldown);
/// invalid/unresolved tokens stay literal. Clears `run`.
fn flush_text_run(run: &mut String, out: &mut Vec<Event<'_>>, ctx: Option<&PageRenderContext<'_>>) {
    if run.is_empty() {
        return;
    }
    let text = std::mem::take(run);
    let fact_refs = ctx.is_some_and(|c| c.fact_refs);
    let links = ctx.map(|c| c.resolve_wikilink);
    let mut pos = 0usize;
    let mut literal_from = 0usize;

    while pos < text.len() {
        // Earliest candidate token from the current position.
        let mut next: Option<(usize, TextToken)> = None;
        let mut consider = |found: Option<usize>, tok: TextToken| {
            if let Some(at) = found
                && next.is_none_or(|(best, _)| at < best)
            {
                next = Some((at, tok));
            }
        };
        consider(
            text[pos..].find("{{embed=").map(|r| pos + r),
            TextToken::Embed,
        );
        if fact_refs {
            consider(
                text[pos..].find("{{factref=").map(|r| pos + r),
                TextToken::FactRef,
            );
        }
        if links.is_some() {
            consider(text[pos..].find("[[").map(|r| pos + r), TextToken::WikiLink);
        }
        let Some((open, token)) = next else {
            break;
        };
        match token {
            TextToken::Embed => {
                let Some(crel) = text[open..].find("}}") else {
                    break;
                };
                let close = open + crel;
                let raw = &text[open + "{{embed=".len()..close];
                if let Ok(cid) = CatalogId::parse(raw) {
                    emit_literal(&text, literal_from, open, out);
                    out.push(Event::Html(embed_element(&cid).into()));
                    pos = close + 2;
                    literal_from = pos;
                } else {
                    // Invalid id: keep the marker literal, move past the
                    // opening token so the scan cannot loop on it.
                    pos = open + "{{embed=".len();
                }
            },
            TextToken::FactRef => {
                let Some(crel) = text[open..].find("}}") else {
                    break;
                };
                let close = open + crel;
                let raw = &text[open + "{{factref=".len()..close];
                if let Ok(fid) = FactId::parse(raw) {
                    emit_literal(&text, literal_from, open, out);
                    out.push(Event::Html(fact_ref_element(&fid).into()));
                    pos = close + 2;
                    literal_from = pos;
                } else {
                    pos = open + "{{factref=".len();
                }
            },
            TextToken::WikiLink => {
                let Some(crel) = text[open + 2..].find("]]") else {
                    break;
                };
                let close = open + 2 + crel;
                let inner = &text[open + 2..close];
                let resolved = links.and_then(|resolve| resolve_wikilink_parts(inner, resolve));
                if let Some((href, label)) = resolved {
                    // The label is emitted as a TEXT event so pulldown
                    // escapes it — an alias cannot inject HTML.
                    emit_literal(&text, literal_from, open, out);
                    out.push(Event::Html(
                        format!(r#"<a class="wikilink" href="{}">"#, escape_attr(&href)).into(),
                    ));
                    out.push(Event::Text(CowStr::from(label)));
                    out.push(Event::Html("</a>".into()));
                    pos = close + 2;
                    literal_from = pos;
                } else {
                    // Unresolved target (unknown wiki / missing page /
                    // mutant grammar): the literal text stays — plain
                    // prose, never a broken link.
                    pos = close + 2;
                }
            },
        }
    }
    emit_literal(&text, literal_from, text.len(), out);
}

/// Strip `{{factref=…}}` markers from a heading's raw text before slug
/// derivation — they are per-render annotations, not heading identity.
fn strip_factref_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while let Some(rel) = text[pos..].find("{{factref=") {
        let open = pos + rel;
        let Some(crel) = text[open..].find("}}") else {
            break;
        };
        out.push_str(&text[pos..open]);
        pos = open + crel + 2;
    }
    out.push_str(&text[pos..]);
    out
}

/// Minimal HTML attribute escaping for code-built hrefs.
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The small superscript anchor for one fact-backed region: links to the
/// fact's record (`/dashboard/facts/:id/edit` — canonical text,
/// owner/sender, validity, provenance and the structured actions). The
/// `FactId` charset (UUID: `[0-9a-f-]`) is HTML-attribute safe by
/// construction. The target route gates on the same readability
/// predicate that made this region visible, so the anchor never leads a
/// reader somewhere they cannot go.
fn fact_ref_element(fid: &FactId) -> String {
    let id = fid.as_str();
    format!(
        r#"<sup class="fact-ref"><a href="/dashboard/facts/{id}/edit" title="View this fact's record">§</a></sup>"#
    )
}

/// The media element for one embed, dispatched on the kind segment of
/// the catalog id (`c-YYYY-MM-DD-<kind>-NNN.<ext>`). Unknown/legacy
/// kinds degrade to a download link. The catalog-id charset
/// (`[a-z0-9.-]`) is HTML-attribute safe by construction.
fn embed_element(cid: &CatalogId) -> String {
    let id = cid.as_str();
    let url = format!("/dashboard/media/{id}");
    match id.split('-').nth(4) {
        Some("photo") => {
            format!(r#"<img class="wiki-embed" src="{url}" alt="{id}" loading="lazy">"#)
        },
        Some("video") => {
            format!(r#"<video class="wiki-embed" controls src="{url}"></video>"#)
        },
        Some("audio") => {
            format!(r#"<audio class="wiki-embed" controls src="{url}"></audio>"#)
        },
        _ => format!(r#"<a class="wiki-embed-link" href="{url}">{id}</a>"#),
    }
}

const fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headings_with_briefing_slug_ids() {
        let html = render("# Parser\n\n## Boundary tokens\n\nBody.\n");
        assert!(html.contains(r#"<h1 id="parser">"#), "{html}");
        assert!(html.contains(r#"<h2 id="boundary-tokens">"#), "{html}");
        assert!(html.contains("Body."), "{html}");
    }

    #[test]
    fn renders_inline_markdown_in_headings() {
        let html = render("## Boundary **tokens**\n");
        // Slug strips the `**` per slug_from_heading's punctuation
        // rules; the rendered text keeps the <strong>.
        assert!(html.contains(r#"<h2 id="boundary-tokens">"#), "{html}");
        assert!(html.contains("<strong>tokens</strong>"), "{html}");
    }

    #[test]
    fn drops_raw_html_from_source() {
        let html = render("Paragraph.\n\n<script>alert(1)</script>\n\nMore.\n");
        assert!(!html.contains("<script>"), "raw HTML survived: {html}");
        assert!(!html.contains("alert(1)"), "raw HTML survived: {html}");
        assert!(html.contains("Paragraph."), "{html}");
        assert!(html.contains("More."), "{html}");
    }

    #[test]
    fn reveal_passes_through_block_and_inline_wrappers_but_renders_inner_markdown() {
        use mwe_core::render::{
            ACL_REVEAL_BLOCK_CLOSE, ACL_REVEAL_BLOCK_OPEN, ACL_REVEAL_INLINE_CLOSE,
            ACL_REVEAL_INLINE_OPEN,
        };
        // Shape mirrors what `render_admin_reveal` emits: a blank-line
        // padded div around a block region, and a span inline.
        let input = format!(
            "Alice pesa {ACL_REVEAL_INLINE_OPEN}72 kg{ACL_REVEAL_INLINE_CLOSE} oggi.\n\n\
             {ACL_REVEAL_BLOCK_OPEN}\n\n## Secret\nbody\n\n{ACL_REVEAL_BLOCK_CLOSE}\n"
        );
        let html = render_reveal(&input);
        assert!(
            html.contains(r#"<div class="acl-revealed">"#),
            "block wrapper dropped: {html}"
        );
        assert!(html.contains("</div>"), "block close dropped: {html}");
        assert!(
            html.contains(r#"<span class="acl-revealed">"#),
            "inline wrapper dropped: {html}"
        );
        assert!(html.contains("</span>"), "inline close dropped: {html}");
        // Inner markdown of the block still renders as a heading.
        assert!(html.contains(r#"<h2 id="secret">"#), "{html}");
        // The inline fragment stays in the flowing sentence.
        assert!(html.contains("72 kg"), "{html}");
        assert!(html.contains("Alice pesa"), "{html}");
    }

    #[test]
    fn reveal_still_drops_other_html_and_unbalanced_close_tags() {
        use mwe_core::render::ACL_REVEAL_BLOCK_CLOSE;
        // A forged `</div>` with no opening wrapper, plus a script: both
        // dropped even in reveal mode.
        let input =
            format!("Para.\n\n<script>alert(1)</script>\n\n{ACL_REVEAL_BLOCK_CLOSE}\n\nEnd.\n");
        let html = render_reveal(&input);
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("</div>"), "stray close leaked: {html}");
        assert!(html.contains("Para."), "{html}");
        assert!(html.contains("End."), "{html}");
    }

    #[test]
    fn default_render_still_strips_reveal_wrappers() {
        use mwe_core::render::{ACL_REVEAL_INLINE_CLOSE, ACL_REVEAL_INLINE_OPEN};
        let input = format!("x {ACL_REVEAL_INLINE_OPEN}secret{ACL_REVEAL_INLINE_CLOSE} y\n");
        let html = render(&input);
        assert!(
            !html.contains(r#"<span class="acl-revealed">"#),
            "reveal wrapper must not survive the default renderer: {html}"
        );
        // The body text itself still passes through (it is plain text).
        assert!(html.contains("secret"), "{html}");
    }

    #[test]
    fn renders_tables_strikethrough_tasklists() {
        let html = render(
            "| a | b |\n|---|---|\n| 1 | 2 |\n\
             \n~~strike~~\n\n- [x] done\n- [ ] todo\n",
        );
        assert!(html.contains("<table>"), "{html}");
        assert!(html.contains("<del>strike</del>"), "{html}");
        // pulldown-cmark renders tasklists as `<input disabled
        // checked type="checkbox">` (or unchecked); attribute order
        // varies between versions, just check the input is there.
        assert!(html.contains(r#"type="checkbox""#), "{html}");
        assert!(html.contains("disabled"), "{html}");
        assert!(html.contains("checked"), "{html}");
    }

    #[test]
    fn inject_after_heading_fires_once_per_heading() {
        let mut calls = Vec::new();
        let html = render_with_heading_injections("## Section A\n\n## Section B\n", |slug| {
            calls.push(slug.to_owned());
            Some(format!(r#"<aside data-anchor="{slug}">cmt</aside>"#))
        });
        assert_eq!(calls, vec!["section-a".to_owned(), "section-b".to_owned()]);
        assert!(
            html.contains(r#"<aside data-anchor="section-a">cmt</aside>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<aside data-anchor="section-b">cmt</aside>"#),
            "{html}"
        );
    }

    #[test]
    fn inject_none_skips_emission_after_heading() {
        let html = render_with_heading_injections("## Foo\n\nBody.\n", |_| None);
        assert!(html.contains(r#"<h2 id="foo">Foo</h2>"#), "{html}");
        assert!(!html.contains("<aside"), "no injection expected: {html}");
    }

    // ---------- embed rendering ----------

    #[test]
    fn embed_markers_render_as_media_elements_by_kind() {
        let html = render(
            "Una foto {{embed=c-2026-06-12-photo-001.jpg}} e un vocale \
{{embed=c-2026-06-12-audio-001.m4a}} e un video {{embed=c-2026-06-12-video-001.mp4}} \
e un documento {{embed=c-2026-06-12-doc-001.pdf}}.\n",
        );
        assert!(
            html.contains(
                r#"<img class="wiki-embed" src="/dashboard/media/c-2026-06-12-photo-001.jpg""#
            ),
            "{html}"
        );
        assert!(
            html.contains(r#"<audio class="wiki-embed" controls"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<video class="wiki-embed" controls"#),
            "{html}"
        );
        assert!(
            html.contains(
                r#"<a class="wiki-embed-link" href="/dashboard/media/c-2026-06-12-doc-001.pdf""#
            ),
            "{html}"
        );
        // The surrounding prose survives around the elements.
        assert!(html.contains("Una foto"), "{html}");
        assert!(html.contains("e un vocale"), "{html}");
    }

    #[test]
    fn legacy_kind_degrades_to_link_and_invalid_marker_stays_literal() {
        let html =
            render("vecchia {{embed=c-2026-05-10-foto-001.jpg}} e rotta {{embed=garbage}}.\n");
        // Legacy Italian kind → download link, never a panic.
        assert!(
            html.contains(
                r#"<a class="wiki-embed-link" href="/dashboard/media/c-2026-05-10-foto-001.jpg""#
            ),
            "{html}"
        );
        // Invalid id stays literal prose (escaped by the renderer).
        assert!(html.contains("{{embed=garbage}}"), "{html}");
    }

    #[test]
    fn embeds_in_code_blocks_and_code_spans_stay_literal() {
        let html = render(
            "```\n{{embed=c-2026-06-12-photo-001.jpg}}\n```\n\nE `{{embed=c-2026-06-12-photo-002.jpg}}` inline.\n",
        );
        assert!(
            !html.contains("<img"),
            "code-fenced markers must stay literal: {html}"
        );
        assert!(
            html.contains("{{embed=c-2026-06-12-photo-001.jpg}}"),
            "{html}"
        );
        assert!(
            html.contains("{{embed=c-2026-06-12-photo-002.jpg}}"),
            "{html}"
        );
    }

    // ---------- wikilink click-through + fact-ref anchors ----------

    /// A fixed resolver for the linkifier tests: `alice` and the sub-wiki
    /// `famiglia-bruno-battaglia` are known wikis; only `alice/notes` and
    /// `famiglia-bruno-battaglia/referto` exist as pages.
    fn test_resolver(target: &str) -> Option<String> {
        match target {
            "alice" => Some("/dashboard/wiki/alice".to_owned()),
            "famiglia-bruno-battaglia" => {
                Some("/dashboard/wiki/famiglia-bruno-battaglia".to_owned())
            },
            "alice/notes" => Some("/dashboard/wiki/alice/view/notes.md".to_owned()),
            "famiglia-bruno-battaglia/referto" => {
                Some("/dashboard/wiki/famiglia-bruno-battaglia/view/referto.md".to_owned())
            },
            _ => None,
        }
    }

    fn render_linkified(body: &str) -> String {
        let ctx = PageRenderContext {
            resolve_wikilink: &test_resolver,
            fact_refs: true,
        };
        render_page(body, false, &ctx, |_| None)
    }

    #[test]
    fn wikilinks_linkify_both_canonical_forms() {
        let html = render_linkified("See [[alice]] and [[alice/notes]] today.\n");
        assert!(
            html.contains(r#"<a class="wikilink" href="/dashboard/wiki/alice">alice</a>"#),
            "wiki hop: {html}"
        );
        assert!(
            html.contains(
                r#"<a class="wikilink" href="/dashboard/wiki/alice/view/notes.md">alice/notes</a>"#
            ),
            "page hop: {html}"
        );
        assert!(!html.contains("[["), "brackets must be consumed: {html}");
    }

    #[test]
    fn wikilink_alias_renders_as_the_label() {
        let html = render_linkified("Read [[alice/notes|My Notes]].\n");
        assert!(
            html.contains(
                r#"<a class="wikilink" href="/dashboard/wiki/alice/view/notes.md">My Notes</a>"#
            ),
            "{html}"
        );
        assert!(!html.contains("alice/notes|"), "{html}");
    }

    #[test]
    fn dangling_wikilink_stays_literal_text_not_a_broken_link() {
        // Unknown wiki, missing page, and the underscored-mutant grammar:
        // all render as plain text — never an <a>.
        let html = render_linkified(
            "Ghost [[ghost]] and [[alice/missing]] and \
             [[famiglia_bruno_battaglia/referto_oculistica]] stay.\n",
        );
        assert!(!html.contains("<a "), "{html}");
        assert!(html.contains("[[ghost]]"), "{html}");
        assert!(html.contains("[[alice/missing]]"), "{html}");
        assert!(
            html.contains("[[famiglia_bruno_battaglia/referto_oculistica]]"),
            "{html}"
        );
    }

    #[test]
    fn sub_wiki_id_linkifies_as_flat_id() {
        let html = render_linkified(
            "Dossier at [[famiglia-bruno-battaglia/referto]] in [[famiglia-bruno-battaglia]].\n",
        );
        assert!(
            html.contains(
                r#"href="/dashboard/wiki/famiglia-bruno-battaglia/view/referto.md">famiglia-bruno-battaglia/referto</a>"#
            ),
            "{html}"
        );
        assert!(
            html.contains(
                r#"href="/dashboard/wiki/famiglia-bruno-battaglia">famiglia-bruno-battaglia</a>"#
            ),
            "{html}"
        );
    }

    #[test]
    fn wikilink_alias_cannot_inject_html() {
        // Raw HTML inside an alias hits the raw-HTML filter first: the
        // tag events are dropped, the split text run no longer forms a
        // wikilink, and no anchor (let alone a script) reaches the output.
        let html = render_linkified("x [[alice|<script>alert(1)</script>]] y\n");
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<a "), "{html}");
        // A plain-text alias is emitted as a text event, so pulldown
        // escapes HTML-special characters in the label.
        let html = render_linkified("x [[alice|Tom & Jerry]] y\n");
        assert!(
            html.contains(
                r#"<a class="wikilink" href="/dashboard/wiki/alice">Tom &amp; Jerry</a>"#
            ),
            "{html}"
        );
    }

    #[test]
    fn wikilinks_in_code_stay_literal_and_render_without_ctx_is_inert() {
        let html = render_linkified("```\n[[alice]]\n```\n\nAnd `[[alice/notes]]` inline.\n");
        assert!(!html.contains("<a "), "code stays literal: {html}");
        assert!(html.contains("[[alice]]"), "{html}");
        // The plain `render` (no ctx — e.g. the chat reply) never linkifies.
        let html = render("See [[alice]].\n");
        assert!(!html.contains("<a "), "{html}");
        assert!(html.contains("[[alice]]"), "{html}");
    }

    #[test]
    fn wikilinks_linkify_inside_headings_with_stable_slug() {
        let html = render_linkified("## About [[alice]] stuff\n\nBody.\n");
        // The slug derives from the RAW heading text (brackets stripped by
        // the slug rules), unchanged by linkification.
        assert!(html.contains(r#"<h2 id="about-alice-stuff">"#), "{html}");
        assert!(
            html.contains(r#"<a class="wikilink" href="/dashboard/wiki/alice">alice</a>"#),
            "{html}"
        );
    }

    #[test]
    fn factref_marker_renders_as_superscript_anchor_to_the_fact_record() {
        let html = render_linkified(
            "Alice pesa 72 kg{{factref=018f1234-5678-7abc-9def-0123456789ab}} oggi.\n",
        );
        assert!(
            html.contains(
                r#"<sup class="fact-ref"><a href="/dashboard/facts/018f1234-5678-7abc-9def-0123456789ab/edit""#
            ),
            "{html}"
        );
        assert!(html.contains("Alice pesa 72 kg"), "{html}");
        assert!(!html.contains("{{factref="), "marker consumed: {html}");
    }

    #[test]
    fn factref_invalid_or_without_ctx_stays_literal() {
        let html = render_linkified("bad {{factref=garbage}} marker.\n");
        assert!(html.contains("{{factref=garbage}}"), "{html}");
        assert!(!html.contains("fact-ref"), "{html}");
        // Without a ctx (fact_refs off) even a valid marker stays literal.
        let html = render("x {{factref=018f1234-5678-7abc-9def-0123456789ab}} y\n");
        assert!(!html.contains("fact-ref"), "{html}");
        assert!(
            html.contains("{{factref=018f1234-5678-7abc-9def-0123456789ab}}"),
            "{html}"
        );
    }

    #[test]
    fn factref_in_heading_is_rewritten_and_kept_out_of_the_slug() {
        let html =
            render_linkified("## Peso{{factref=018f1234-5678-7abc-9def-0123456789ab}}\n\nBody.\n");
        assert!(html.contains(r#"<h2 id="peso">"#), "{html}");
        assert!(html.contains(r#"<sup class="fact-ref">"#), "{html}");
    }

    #[test]
    fn reveal_page_render_keeps_wrappers_and_linkifies() {
        use mwe_core::render::{ACL_REVEAL_INLINE_CLOSE, ACL_REVEAL_INLINE_OPEN};
        let ctx = PageRenderContext {
            resolve_wikilink: &test_resolver,
            fact_refs: true,
        };
        let input = format!(
            "See [[alice]] and {ACL_REVEAL_INLINE_OPEN}secret 72 kg\
{{{{factref=018f1234-5678-7abc-9def-0123456789ab}}}}{ACL_REVEAL_INLINE_CLOSE} here.\n"
        );
        let html = render_page(&input, true, &ctx, |_| None);
        assert!(
            html.contains(r#"<span class="acl-revealed">"#),
            "reveal wrapper survives: {html}"
        );
        assert!(
            html.contains(r#"<a class="wikilink" href="/dashboard/wiki/alice">alice</a>"#),
            "{html}"
        );
        assert!(html.contains(r#"<sup class="fact-ref">"#), "{html}");
    }
}
