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
//! PERSON shares, or none — an assistant declares none and is not asked. A slot with no wiki in hand resolves the principal
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

/// The BCP-47 tag a rendered directive names, or `None` when it names none.
///
/// The directive is what the page was actually told to do, and reading the tag
/// back out of it is how a check downstream asks the same question the writer
/// was asked — rather than asking a different one off the instance's settings
/// and disagreeing with the instruction the page was given. The mirror
/// fallback names no language, and a page written against it can be in any.
#[must_use]
pub fn tag_of_directive(directive: &str) -> Option<&str> {
    let rest = directive.strip_prefix("User locale: ")?;
    let tag = rest.split('.').next()?.trim();
    (!tag.is_empty()).then_some(tag)
}

/// The words that carry grammar in a language this engine can recognise —
/// enough of them, in a page of prose, to say which language it is.
///
/// **Two languages, and it abstains for the rest.** These are the ones the
/// product is written and read in, and a list I can write and defend; a
/// hand-made list for a language nobody here reads is a guess that would send
/// good pages back to be rewritten. For the other eleven the check does not
/// run, and a page in the wrong language there is caught the way it always
/// was: by somebody reading it. Languages written without spaces between
/// words — Japanese, Chinese, Korean — cannot be measured this way at all.
const STOP_WORDS: &[(&str, &[&str])] = &[
    (
        "en",
        &[
            "the", "and", "of", "to", "in", "is", "that", "it", "for", "with", "was", "on", "as",
            "at", "by", "from", "this", "are", "be", "has", "have", "had", "not", "but", "they",
            "their", "she", "he", "will", "would", "which", "been", "were", "there", "when", "who",
            "what", "an", "or", "if", "into", "after", "before", "than", "then", "its", "his",
            "her", "them", "also", "any", "all", "how", "out", "up",
        ],
    ),
    (
        "it",
        &[
            "il", "lo", "la", "i", "gli", "le", "un", "una", "uno", "di", "del", "della", "dei",
            "delle", "dello", "che", "non", "per", "con", "sono", "essere", "stato", "stata",
            "come", "dove", "quando", "anche", "più", "già", "ma", "se", "nel", "nella", "nei",
            "sul", "sulla", "alla", "allo", "agli", "alle", "da", "dal", "dalla", "questo",
            "questa", "quello", "quella", "suo", "sua", "loro", "hanno", "ha", "è", "ed", "o",
            "al", "ai", "si", "ci", "tra", "fra", "una", "delle",
        ],
    ),
];

/// A page shorter than this says nothing about its own language.
const TOO_SHORT_TO_JUDGE: usize = 20;

/// The share of a page's words that must be one language's grammar before the
/// page counts as written in it, as a percentage — counted in whole words, so
/// the arithmetic is exact and the same on every machine.
const FLOOR_PERCENT: usize = 8;

/// How far ahead of the runner-up the winner has to be, as a fraction: three
/// halves.
const MARGIN: (usize, usize) = (3, 2);

/// **Which language this prose is written in**, or `None` when it cannot be
/// told.
///
/// Grammar words per language, counted: the winner has to carry at least
/// [`FLOOR_PERCENT`] of the words and stand [`MARGIN`] clear of the second. No
/// model, no network, the same answer every time — which is what lets it
/// decide whether to spend a call rather than being one.
///
/// **It abstains far more readily than it answers**, and every way of
/// abstaining is deliberate. Under [`TOO_SHORT_TO_JUDGE`] words there is not
/// enough grammar in a page to count. A page in two languages splits the
/// score and neither clears the margin, so nothing is sent back for being
/// half-translated — which is a judgement about content, not about language.
/// A page that is mostly names and figures clears no floor, because a proper
/// noun is nobody's grammar word.
#[must_use]
pub fn language_of_prose(text: &str) -> Option<&'static str> {
    let words: Vec<String> = text
        .split(|c: char| c.is_whitespace())
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect();
    if words.len() < TOO_SHORT_TO_JUDGE {
        return None;
    }
    let total = words.len();
    let mut scored: Vec<(&'static str, usize)> = STOP_WORDS
        .iter()
        .map(|(tag, stop)| {
            (
                *tag,
                words.iter().filter(|w| stop.contains(&w.as_str())).count(),
            )
        })
        .collect();
    scored.sort_by_key(|(_, hits)| std::cmp::Reverse(*hits));
    let (best, hits) = *scored.first()?;
    let runner_up = scored.get(1).map_or(0, |(_, h)| *h);
    let clears_the_floor = hits * 100 >= total * FLOOR_PERCENT;
    let stands_clear = hits * MARGIN.1 >= runner_up * MARGIN.0;
    (clears_the_floor && stands_clear).then_some(best)
}

/// **The language this page is in, when it is not the one the page was told to
/// write in** — and `None` whenever the question cannot be answered.
///
/// It compares against the DIRECTIVE the page received, not the instance's
/// settings: a page obeying the instruction it was given is not a page in the
/// wrong language, whatever a setting elsewhere says, and disagreeing with the
/// directive would send the same page back every night for a rewrite that
/// would come out the same.
///
/// Silent unless all of it holds: the directive names a language, that
/// language is one of the two with a list, and the prose says clearly that it
/// is in the other one.
#[must_use]
pub fn written_in_another_language(directive: &str, prose: &str) -> Option<&'static str> {
    let asked = tag_of_directive(directive)?
        .split(['-', '_'])
        .next()?
        .to_ascii_lowercase();
    if !STOP_WORDS.iter().any(|(tag, _)| *tag == asked) {
        return None;
    }
    let found = language_of_prose(prose)?;
    (found != asked).then_some(found)
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
/// text — the one every enrolled PERSON shares, or none.
///
/// Two callers, one question. A wiki that does not exist yet declares no
/// language, and the pages a grouping would gather come from several that need
/// not agree; a wiki born at the top level answers to nobody, so it has no
/// principal to read one from. Unanimity or nothing is the same rule
/// [`crate::enrollment::locale_for_principal`] applies to a group's members,
/// and `None` renders the ordinary mirror fallback.
///
/// **Only those who said so are counted, and an assistant is never asked.**
/// A language is a fact about the people a memory is for, not about the
/// programs that talk to them: an assistant is enrolled so it can be addressed
/// and answered for, and nobody declares a language for it. Neither does a
/// person who left the field empty — they have not disagreed with anything,
/// they have said nothing, and treating silence as disagreement is what made
/// a single empty row mean «this household agrees on nothing». Measured on the
/// live memory: four people all `it` and one agent with an empty row, and the
/// four wikis answering to nobody held 23 pages of English inside an Italian
/// memory, a pregnancy record and a father's clinical notes among them.
///
/// So the rule is unanimity **among those who declared one**. Nobody declared:
/// no language, and the English fallback stands — which is a fixed language an
/// operator can recognise and correct, not a guess. Two declared and different:
/// no language either, because the memory genuinely has no one language and
/// picking a side would be one person's locale winning a vote it was never in.
///
/// # Errors
///
/// None: a lookup that fails reads as "no shared language", which is the
/// safe answer — no compile should die over a locale.
pub async fn memory_wide_locale(pool: &sqlx::SqlitePool) -> Option<String> {
    let users = crate::enrollment::list_users(pool).await.ok()?;
    let mut declared = Vec::new();
    for u in users.iter().filter(|u| !u.is_agent) {
        if let Ok(Some(locale)) = crate::enrollment::locale_for(pool, &u.user_id).await {
            declared.push(locale);
        }
    }
    let first = declared.first()?.clone();
    declared.iter().all(|l| *l == first).then_some(first)
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

    /// **A page in the wrong language is sent back; a page that cannot be
    /// judged is left alone.**
    ///
    /// The four answers the check has to get right, and the three of them that
    /// are ways of saying nothing. Being quiet is what makes this affordable:
    /// every time it speaks costs one call to the page writer.
    #[test]
    fn a_page_is_read_against_the_directive_it_was_given() {
        let italian = "Non tutti i pasti in giardino restano documentati qui in dettaglio, \
             ma un'occasione compare nei ricordi condivisi della famiglia: una sera hanno \
             mangiato fuori e il gatto è rimasto seduto sul tavolo per tutto il pasto.";
        let english_directive = render_memory_language_directive(Some("en-GB"));
        let italian_directive = render_memory_language_directive(Some("it"));

        assert_eq!(
            written_in_another_language(&english_directive, italian),
            Some("it"),
            "Italian prose under an English directive goes back"
        );
        assert_eq!(
            written_in_another_language(&italian_directive, italian),
            None,
            "the same page under the directive it obeys is right, and costs nothing"
        );

        // Too short to judge: a page of a dozen words carries no grammar to
        // count, and a guess there would spend a call on a page that may be
        // perfectly correct.
        assert_eq!(
            written_in_another_language(&english_directive, "Il gatto dorme sul tavolo."),
            None,
            "under twenty words nothing is said"
        );

        // A language with no list: the check abstains rather than guessing,
        // and a wrong page there is caught the way it always was.
        let japanese_directive = render_memory_language_directive(Some("ja"));
        assert_eq!(
            written_in_another_language(&japanese_directive, italian),
            None,
            "no list, no verdict — and a language written without spaces cannot be \
             measured this way at all"
        );

        // The mirror fallback names no language, so no page can disobey it.
        assert_eq!(
            written_in_another_language(&render_language_directive(None), italian),
            None,
            "a directive that names no language asks nothing of the page"
        );
    }

    /// **An assistant has no language of its own to break the household's.**
    ///
    /// A language is a fact about the people a memory is for. An assistant is
    /// enrolled so it can be addressed and answered for, and nobody declares a
    /// language for it — so counting its empty row said the household agreed
    /// on nothing. On the live memory that was four people all `it` and one
    /// agent with an empty row, and the four wikis answering to nobody wrote
    /// 23 pages of English inside an Italian memory.
    #[tokio::test]
    async fn an_assistant_does_not_break_the_language_the_people_share() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        for (u, locale, is_agent) in [("alice", "it", 0), ("bob", "it", 0), ("frodo", "", 1)] {
            sqlx::query(
                "INSERT INTO enrollment_users (user_id, aliases, is_admin, locale, is_agent) \
                 VALUES (?, '[]', 0, ?, ?)",
            )
            .bind(u)
            .bind(locale)
            .bind(is_agent)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(
            memory_wide_locale(&pool).await.as_deref(),
            Some("it"),
            "the people agree, and the assistant was not asked"
        );

        // A PERSON who left the field empty has not disagreed — they have said
        // nothing — so the language the others declared still stands. Silence
        // read as disagreement is what sent a whole memory to English.
        sqlx::query("UPDATE enrollment_users SET locale = '' WHERE user_id = 'bob'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            memory_wide_locale(&pool).await.as_deref(),
            Some("it"),
            "one empty field does not overrule the people who did say"
        );

        // Two people who DID declare, and differently: the memory genuinely
        // has no one language, and it says so rather than picking a side.
        sqlx::query("UPDATE enrollment_users SET locale = 'fr' WHERE user_id = 'bob'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            memory_wide_locale(&pool).await,
            None,
            "two declared and different is no agreement"
        );

        // And nobody having said anything is no language either.
        sqlx::query("UPDATE enrollment_users SET locale = '' WHERE is_agent = 0")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            memory_wide_locale(&pool).await,
            None,
            "nobody declared: the English fallback stands"
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
