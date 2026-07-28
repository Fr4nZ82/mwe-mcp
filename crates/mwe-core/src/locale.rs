// SPDX-License-Identifier: AGPL-3.0-or-later
//! Locale plumbing for the v2.0 prompts (`ingest`, `agentic-chat-panel`).
//!
//! ## Why this module exists
//!
//! Both v2.0 prompts ship a single-line `LANGUAGE` directive at the
//! end of their body. Before the P.14 plumbing landed, that directive
//! was a hand-written paragraph telling the LLM to "mirror the
//! language of the user's message". The mirror works when the
//! message is unambiguous but is fragile in two regimes: short
//! messages with a single foreign loanword (a one-token English
//! interjection in an Italian sentence has flipped Qwen 3.5 to
//! English replies in the field), and tool-call replies where the
//! LLM has no fresh user prose to mirror.
//!
//! The fix the prompt locale plumbing
//! calls for an **explicit injection** of `User locale: <code>.
//! Respond in <Language>.` derived from one of three sources, in
//! order:
//!
//! 1. `metadata.locale` on the `wiki_ingest_message` MCP request,
//! 2. `enrollment::locale_for(sender_id)` — the per-user default the
//!    admin configured,
//! 3. the dashboard `SessionUser` for the chat panel.
//!
//! [`render_language_directive`] takes the resolved locale (or `None`
//! when every source came up empty) and produces the single-line
//! body the prompt's `{locale}` placeholder is substituted with. The
//! `None` branch is the legacy mirror clause, kept so a deployment
//! without populated locales does not regress.
//!
//! ## Answering a turn versus writing memory
//!
//! The mirror clause only makes sense for a slot that can see the
//! user's own words. The slots that **write memory** — page prose,
//! page names, the date normaliser's rewrites — are handed extracted
//! facts, not a live turn, and their prompt bodies carry Italian
//! few-shot examples that a "mirror what you see" instruction loses
//! against in practice. For those,
//! [`render_memory_language_directive`] resolves an undeclared locale
//! to English instead: the declared language wins, and the fallback
//! is a fixed language rather than a guess. Setting the locale on
//! every user is what an existing deployment does to keep its pages
//! in their own language.
//!
//! [`memory_directive_for_wiki`] and [`memory_directive_for_user`] are
//! the two ways a memory-writing slot gets there — a compiled page
//! belongs to a wiki, an ingested document belongs to whoever
//! uploaded it. Both are best-effort: any lookup failure logs and
//! degrades to the English fallback rather than failing the job.

/// Render the body the prompt's `{locale}` placeholder is replaced with.
///
/// The shape is a single line so a hand-edited override at
/// `<workdir>/prompts/<name>.md` can keep the `LANGUAGE` heading
/// stable and only re-flow the directive itself.
///
/// `locale` is interpreted as a BCP-47 tag (`it-IT`, `en-US`,
/// `pt-BR`, ...). The renderer is tolerant of:
///
/// - `None` or `Some("")` — returns the mirror fallback,
/// - a region-only tag (`it`) — primary subtag only is fine,
/// - an unknown primary subtag — the directive cites the tag itself
///   instead of dropping it (the LLM understands BCP-47 well enough
///   in practice; this stops us silently degrading to the mirror
///   clause when the operator typed a tag we don't have in the
///   table).
///
/// The output never contains the `{locale}` placeholder text itself,
/// so a degenerate prompt body with stacked placeholders cannot
/// loop. See [`crate::prompts::substitute`] for the substitution
/// rules.
#[must_use]
pub fn render_language_directive(locale: Option<&str>) -> String {
    let Some(tag) = locale.map(str::trim).filter(|s| !s.is_empty()) else {
        return MIRROR_FALLBACK.to_owned();
    };
    let language = language_name_for(tag);
    format!(
        "User locale: {tag}. Respond in {language}. \
         Never mix languages in a single response. \
         Never use non-Latin alphabets unless the user's text \
         explicitly uses them. The tool names, JSON keys and \
         argument enums above stay in English; only the \
         natural-language replies follow the user's locale."
    )
}

/// BCP-47 tag a memory-writing slot falls back to when nobody
/// declared a locale for the memory it is about to write.
///
/// English, deliberately: a fixed language the operator can recognise
/// and correct from the users page beats a per-call guess at what the
/// facts look like. See the module docs.
pub const MEMORY_FALLBACK_TAG: &str = "en";

/// Render the `{locale}` directive for a slot that **writes memory**
/// rather than answering a live turn.
///
/// Same directive body as [`render_language_directive`] — one
/// mechanism, one wording — but an absent or blank locale resolves to
/// [`MEMORY_FALLBACK_TAG`] instead of the mirror clause, because these
/// slots have no user message to mirror.
#[must_use]
pub fn render_memory_language_directive(locale: Option<&str>) -> String {
    let tag = locale
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(MEMORY_FALLBACK_TAG);
    render_language_directive(Some(tag))
}

/// Resolve the language of everything written under `wiki_id`, and
/// render it as the `{locale}` directive.
///
/// The wiki's scope principal is its language of record: a
/// `wiki-user` line speaks its owner's declared language, a
/// `wiki-group` line speaks the one its members agree on (see
/// [`crate::enrollment::locale_for_principal`]). Best-effort — a
/// topology or DB failure logs and yields the English fallback, since
/// no compile should die over a missing locale.
pub async fn memory_directive_for_wiki(
    pool: &sqlx::SqlitePool,
    tree: &crate::wiki::WikiTree,
    wiki_id: &crate::types::WikiId,
) -> String {
    match tree.locate(wiki_id) {
        Ok(handle) => memory_directive_for_wiki_meta(pool, tree, handle.meta()).await,
        Err(e) => {
            tracing::warn!(
                wiki_id = %wiki_id,
                error = %e,
                "locale: wiki not found, falling back to English"
            );
            render_memory_language_directive(None)
        },
    }
}

/// [`memory_directive_for_wiki`] for a caller that already holds the
/// wiki's `_meta` — every REM sub-job walks the forest and has one, and
/// re-locating by id would re-walk the tree for nothing.
pub async fn memory_directive_for_wiki_meta(
    pool: &sqlx::SqlitePool,
    tree: &crate::wiki::WikiTree,
    meta: &crate::wiki::WikiMeta,
) -> String {
    let principal = match tree.resolve_scope_principal(meta) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                wiki_id = %meta.wiki_id,
                error = %e,
                "locale: wiki scope principal unresolved, falling back to English"
            );
            return render_memory_language_directive(None);
        },
    };
    let resolved = match crate::enrollment::locale_for_principal(pool, &principal).await {
        Ok(loc) => loc,
        Err(e) => {
            tracing::warn!(
                wiki_id = %meta.wiki_id,
                error = %e,
                "locale: principal locale lookup failed, falling back to English"
            );
            None
        },
    };
    render_memory_language_directive(resolved.as_deref())
}

/// Resolve `user_id`'s declared language and render it as the
/// `{locale}` directive for a memory-writing slot.
///
/// The document-ingest slots use this: a document has no wiki until
/// the classify phase has decided one, but it always has the person
/// who submitted it. Best-effort, same as
/// [`memory_directive_for_wiki`].
pub async fn memory_directive_for_user(pool: &sqlx::SqlitePool, user_id: &str) -> String {
    let resolved = match crate::enrollment::locale_for(pool, user_id).await {
        Ok(loc) => loc,
        Err(e) => {
            tracing::warn!(
                user_id,
                error = %e,
                "locale: user locale lookup failed, falling back to English"
            );
            None
        },
    };
    render_memory_language_directive(resolved.as_deref())
}

/// Mirror clause used when no explicit locale is known. Equivalent
/// to the pre-plumbing LANGUAGE section so the bundled prompts keep
/// working out of the box on a deployment that has not populated any
/// of the three sources.
const MIRROR_FALLBACK: &str = "Mirror the language of the user's message. Never mix languages in a single response. \
     Never use non-Latin alphabets unless the user's text explicitly uses them. The tool \
     names, JSON keys and argument enums above stay in English; only the natural-language \
     replies follow the user's locale.";

/// Translate a BCP-47 tag's **primary subtag** to its English
/// language name. Unknown subtags surface as the tag itself wrapped
/// in a hint clause so the LLM still gets a usable signal; the table
/// is intentionally small (the languages we expect on day-1
/// deployments) and grows when a real consumer needs it.
fn language_name_for(tag: &str) -> String {
    let primary = tag
        .split(['-', '_'])
        .next()
        .unwrap_or(tag)
        .to_ascii_lowercase();
    match primary.as_str() {
        "it" => "Italian".to_owned(),
        "en" => "English".to_owned(),
        "es" => "Spanish".to_owned(),
        "fr" => "French".to_owned(),
        "de" => "German".to_owned(),
        "pt" => "Portuguese".to_owned(),
        "nl" => "Dutch".to_owned(),
        "pl" => "Polish".to_owned(),
        "ja" => "Japanese".to_owned(),
        "zh" => "Chinese".to_owned(),
        "ko" => "Korean".to_owned(),
        "ru" => "Russian".to_owned(),
        "ar" => "Arabic".to_owned(),
        // Unknown primary subtag — surface the BCP-47 tag itself.
        // The LLM understands the standard well enough that this
        // beats silently degrading to the mirror clause.
        _ => format!("the language indicated by BCP-47 tag {tag}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_fallback_when_locale_is_none() {
        let directive = render_language_directive(None);
        assert!(directive.starts_with("Mirror the language"));
    }

    #[test]
    fn mirror_fallback_when_locale_is_empty() {
        let directive = render_language_directive(Some(""));
        assert!(directive.starts_with("Mirror the language"));
        let directive = render_language_directive(Some("   "));
        assert!(directive.starts_with("Mirror the language"));
    }

    #[test]
    fn renders_known_primary_subtags() {
        let cases: &[(&str, &str)] = &[
            ("it-IT", "Italian"),
            ("en-US", "English"),
            ("en", "English"),
            ("es-ES", "Spanish"),
            ("fr-FR", "French"),
            ("de-DE", "German"),
            ("pt-BR", "Portuguese"),
            ("ja-JP", "Japanese"),
            ("zh-CN", "Chinese"),
        ];
        for (tag, language) in cases {
            let directive = render_language_directive(Some(tag));
            assert!(
                directive.contains(&format!("User locale: {tag}")),
                "missing locale `{tag}` in directive: {directive}"
            );
            assert!(
                directive.contains(&format!("Respond in {language}")),
                "missing language `{language}` for `{tag}`: {directive}"
            );
        }
    }

    /// Underscore separator (POSIX-style locales like `it_IT.UTF-8`)
    /// is accepted: we split on either `-` or `_` for the primary
    /// subtag lookup.
    #[test]
    fn renders_locale_with_underscore_separator() {
        let directive = render_language_directive(Some("it_IT"));
        assert!(directive.contains("Respond in Italian"));
    }

    /// Case-insensitive primary subtag (`IT-IT` vs `it-IT`).
    #[test]
    fn renders_locale_case_insensitive_primary_subtag() {
        let directive = render_language_directive(Some("IT-IT"));
        assert!(directive.contains("Respond in Italian"));
    }

    /// The memory-writing fallback is English, **not** the mirror
    /// clause. Both were plausible; the mirror is what the compiled
    /// slots had implicitly and what lost against their Italian
    /// few-shot examples, so the absence of the mirror wording is as
    /// much the point as the presence of English.
    #[test]
    fn memory_directive_falls_back_to_english_never_to_the_mirror() {
        for undeclared in [None, Some(""), Some("   ")] {
            let directive = render_memory_language_directive(undeclared);
            assert!(
                directive.contains("Respond in English"),
                "undeclared locale must resolve to English: {directive}"
            );
            assert!(
                !directive.contains("Mirror the language"),
                "the memory slots must never get the mirror clause: {directive}"
            );
        }
    }

    /// A declared locale wins over the fallback — the founder's rule
    /// in one assertion.
    #[test]
    fn memory_directive_honours_a_declared_locale() {
        let directive = render_memory_language_directive(Some("it-IT"));
        assert!(directive.contains("User locale: it-IT"));
        assert!(directive.contains("Respond in Italian"));
        // Not "does not mention English" — the directive always names
        // English once, in the clause exempting tool names and JSON
        // keys from translation. What must be absent is the fallback
        // instruction itself.
        assert!(
            !directive.contains("Respond in English"),
            "a declared locale must not be overridden by the fallback: {directive}"
        );
    }

    /// Unknown primary subtag surfaces the BCP-47 tag itself instead
    /// of dropping to the mirror clause.
    #[test]
    fn unknown_primary_subtag_surfaces_the_tag() {
        let directive = render_language_directive(Some("xq-XQ"));
        assert!(
            directive.contains("BCP-47 tag xq-XQ"),
            "expected unknown-subtag fallback, got: {directive}"
        );
        assert!(!directive.starts_with("Mirror the language"));
    }
}
