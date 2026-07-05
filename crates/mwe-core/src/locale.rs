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
//! The fix [the prompt locale plumbing](../../../wiki/design-notes/llm-functions.md)
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
