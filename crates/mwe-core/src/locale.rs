// SPDX-License-Identifier: AGPL-3.0-or-later
//! Locale plumbing for the v2.0 prompts (`ingest`, `agentic-chat-panel`).
//!
//! ## Why this module exists
//!
//! Both v2.0 prompts ship a single-line `LANGUAGE` directive at the
//! end of their body. A hand-written "mirror the language of the
//! user's message" paragraph works when the message is unambiguous
//! but is fragile in two regimes: short messages with a single
//! foreign loanword (a one-token English interjection in an Italian
//! sentence has flipped Qwen 3.5 to English replies in the field),
//! and tool-call replies where the LLM has no fresh user prose to
//! mirror.
//!
//! The fix is an **explicit injection** of `User locale: <code>.
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
//! [`memory_directive_for_wiki`] is how a memory-writing slot gets
//! there when it is compiling a page — the wiki's scope principal names
//! the language. **A wiki that answers to nobody** — every one the nightly
//! grouping raises, since it hangs under nothing — has no principal to name
//! it, and falls back to [`memory_wide_locale`]: the language every enrolled
//! person shares, or none. A slot with no wiki in hand resolves the principal
//! itself and calls [`render_memory_language_directive`]: document
//! ingest does that once per job, from the job's subject. All of it is
//! best-effort — any lookup failure logs and degrades to the English
//! fallback rather than failing the job.

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
         argument enums above stay in English; EVERYTHING ELSE you \
         write as natural language follows the user's locale — not \
         only whole sentences, but every short label and keyword you \
         coin as well."
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
    // A wiki that answers to nobody is the ordinary case now, not an error:
    // a wiki born out of the nightly grouping hangs under nothing, so there
    // is no principal whose language to read. The memory still has one when
    // every enrolled person shares it, and that is the language its pages are
    // written in — the alternative is what the bench showed on 2026-09-05,
    // where the two topic wikis held 15 pages of English inside a
    // memory whose five people are all `it`, among them a father's clinical
    // record.
    let owner = match tree.resolve_scope_principal(meta) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                wiki_id = %meta.wiki_id,
                error = %e,
                "locale: scope chain unresolved — no principal names a language"
            );
            None
        },
    };
    let Some(principal) = owner else {
        let shared = memory_wide_locale(pool).await;
        tracing::debug!(
            wiki_id = %meta.wiki_id,
            locale = shared.as_deref().unwrap_or("(none)"),
            "locale: wiki answers to nobody — the memory's own language is used"
        );
        return render_memory_language_directive(shared.as_deref());
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

/// The language the memory writes in when no single wiki answers for the
/// text — the one every enrolled person shares, or none.
///
/// Two callers, one question. A wiki that does not exist yet declares no
/// language, and the pages a grouping would gather come from several that need
/// not agree; a wiki born at the top level answers to nobody, so it has no
/// principal to read one from. Unanimity or nothing is the same rule
/// [`crate::enrollment::locale_for_principal`] applies to a group's members,
/// and `None` renders the ordinary mirror fallback.
///
/// # Errors
///
/// None: a lookup that fails reads as "no shared language", which is the
/// safe answer — no compile should die over a locale.
pub async fn memory_wide_locale(pool: &sqlx::SqlitePool) -> Option<String> {
    let users = crate::enrollment::list_users(pool).await.ok()?;
    let mut locales = Vec::new();
    for u in users {
        locales.push(
            crate::enrollment::locale_for(pool, &u.user_id)
                .await
                .ok()??,
        );
    }
    let first = locales.first()?.clone();
    locales.iter().all(|l| *l == first).then_some(first)
}

/// Mirror clause used when no explicit locale is known: it tells the
/// model to follow the language of the message it is answering, so a
/// deployment that has populated none of the three sources still gets
/// a complete LANGUAGE section.
const MIRROR_FALLBACK: &str = "Mirror the language of the user's message. Never mix languages in a single response. \
     Never use non-Latin alphabets unless the user's text explicitly uses them. The tool \
     names, JSON keys and argument enums above stay in English; EVERYTHING ELSE you write \
     as natural language follows the user's language — not only whole sentences, but every \
     short label and keyword you coin as well.";

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

    /// A wiki that answers to nobody writes in the memory's own language.
    ///
    /// Every wiki the nightly grouping raises is one: it hangs under nothing,
    /// so there is no principal to read a language from. Deriving it from
    /// ownership and giving up when there is no owner put 15 English pages
    /// inside an all-Italian memory on the bench, 2026-09-05 — a father's
    /// clinical record among them.
    #[tokio::test]
    async fn a_wiki_that_answers_to_nobody_writes_in_the_memorys_language() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        for u in ["alice", "bob"] {
            sqlx::query(
                "INSERT INTO enrollment_users (user_id, aliases, is_admin, locale) \
                 VALUES (?, '[]', 0, 'it')",
            )
            .bind(u)
            .execute(&pool)
            .await
            .unwrap();
        }
        let wikis = dir.path().join("wikis/giardinaggio");
        std::fs::create_dir_all(&wikis).unwrap();
        std::fs::write(
            wikis.join("_meta.md"),
            "---\nwiki_id: giardinaggio\nwiki_type: wiki-tech\nparent_wiki_id: null\n\
             slug: giardinaggio\ntitle: Giardinaggio\n---\n",
        )
        .unwrap();
        let tree = crate::wiki::WikiTree::open(dir.path()).expect("tree");
        let meta = tree
            .locate(&crate::types::WikiId::parse("giardinaggio").unwrap())
            .expect("wiki")
            .meta()
            .clone();
        // The premise: nobody answers for this wiki.
        assert_eq!(
            tree.resolve_scope_principal(&meta).expect("resolve"),
            None,
            "a wiki born at the top level has no principal"
        );

        let directive = memory_directive_for_wiki_meta(&pool, &tree, &meta).await;
        assert!(
            directive.contains("Italian"),
            "the memory's own language is the one its pages are written in: {directive}"
        );

        // And with the enrolled disagreeing there is no memory-wide language,
        // so the memory-writing fallback stands — English, a fixed language
        // the operator can recognise and correct — rather than one person's
        // locale winning a vote it was never in.
        sqlx::query("UPDATE enrollment_users SET locale = 'fr' WHERE user_id = 'bob'")
            .execute(&pool)
            .await
            .unwrap();
        let directive = memory_directive_for_wiki_meta(&pool, &tree, &meta).await;
        assert!(
            directive.contains("Respond in English"),
            "no unanimity, no language: {directive}"
        );
        drop(dir);
    }

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

    /// The directive reaches the short labels a slot coins, not only its
    /// sentences.
    ///
    /// A directive that names the machinery on one side and the reply on the
    /// other leaves in the gap everything that is neither — and the topic
    /// words live there: two lower-case words per fact, coined by the
    /// classifier and printed among a page's keywords, where a reader sees
    /// them. An all-English memory tagged in another language is the failure
    /// this sentence exists to stop, so the directive has to reach past whole
    /// sentences while still holding the JSON keys and enums in English.
    #[test]
    fn the_language_directive_covers_the_short_labels_a_slot_coins() {
        // Both halves of the only chain there is: a per-turn `metadata.locale`
        // or, failing that, the `enrollment_users.locale` an admin sets on the
        // Users page. There is no deployment-wide language to disagree with
        // them, so the rule has to hold identically whichever language the tag
        // names — an Italian memory keeps Italian labels, an English one
        // English ones, and neither borrows the other's.
        // The exit taken by a person with no language set at all — the third
        // source empty, which `ingest.rs` reaches through
        // `render_language_directive(None)`. It carries the same rule: a
        // memory mirroring its speaker still must not coin a label in
        // another language than the one it is mirroring.
        let mirrored = render_language_directive(None);
        assert!(
            mirrored.contains("every short label and keyword you coin"),
            "the mirror exit stops at whole sentences: {mirrored}"
        );
        assert!(
            mirrored.contains("Mirror the language"),
            "and it is still the mirror clause: {mirrored}"
        );

        for (tag, language) in [("en-GB", "English"), ("it", "Italian")] {
            let directive = render_language_directive(Some(tag));
            assert!(
                directive.contains(&format!("Respond in {language}")),
                "{tag} no longer resolves to {language}: {directive}"
            );
            assert!(
                directive.contains("every short label and keyword you coin"),
                "the directive stops at whole sentences again for {tag}: {directive}"
            );
            assert!(
                directive.contains("tool names, JSON keys"),
                "and it must still hold the machinery back in English: {directive}"
            );
        }
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
