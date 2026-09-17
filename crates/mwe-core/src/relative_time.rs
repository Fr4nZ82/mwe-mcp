//! **The words that mean «now», and stop meaning it.**
//!
//! «Zoe is out tonight» is true for one evening and wrong for ever after:
//! read back in April it points at the wrong night, and nothing left in the
//! sentence says which night was meant. A fact's text has to stay true for as
//! long as it is kept, so a time word that only means something beside the
//! moment it was said has to become a date before the fact is stored.
//!
//! One lexicon, two callers, and the order between them is the rule the
//! product works by. [`crate::ingest`] reads every body the classifier writes
//! and asks once for the date — the fact is stopped at the door. [`crate::rem`]'s
//! nightly date normaliser sweeps what the door could not fix and the backlog
//! that predates it, which is a safety net and not the place the work is meant
//! to happen.
//!
//! **A pre-filter is allowed to miss.** What it flags is asked about; what it
//! does not flag is stored as it came. So the list is the phrases that actually
//! turn up in a memory's two languages, not an inventory of either — and the
//! model, not this list, decides whether a flagged phrase is really deictic
//! («as things stand today» is not).

/// The relative time phrase in this text, if it carries one.
///
/// One question asked two ways: whether a sentence still dates itself against
/// the moment it was said, and **which word** does it — the second is what a
/// receipt needs to name, and computing it twice is how the word and the
/// verdict drift apart.
///
/// Word-bounded and case-insensitive over both languages: «oggigiorno»
/// contains «oggi» and means nothing relative.
pub fn the_relative_time_in(text: &str) -> Option<&'static str> {
    const LEXICON: &[&str] = &[
        "oggi",
        "ieri",
        "domani",
        "dopodomani",
        "stasera",
        "stamattina",
        "stanotte",
        "questa settimana",
        "settimana prossima",
        "settimana scorsa",
        "questo mese",
        "mese prossimo",
        "mese scorso",
        "quest'anno",
        "anno prossimo",
        "anno scorso",
        "today",
        "yesterday",
        "tomorrow",
        "tonight",
        "this morning",
        "this afternoon",
        "this evening",
        "this weekend",
        "last night",
        "this week",
        "next week",
        "last week",
        "this month",
        "next month",
        "last month",
        "this year",
        "next year",
    ];
    let lower = text.to_lowercase();
    LEXICON.iter().copied().find(|phrase| {
        lower.match_indices(phrase).any(|(i, _)| {
            let before_ok = i == 0
                || !lower[..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric);
            let after = i + phrase.len();
            let after_ok = after >= lower.len()
                || !lower[after..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric);
            before_ok && after_ok
        })
    })
}

#[cfg(test)]
mod tests {
    use super::the_relative_time_in;

    /// The lexical pre-filter is word-bounded: «oggi» flags, a word that
    /// merely contains it does not.
    #[test]
    fn a_word_that_contains_a_time_word_is_not_one() {
        assert_eq!(
            the_relative_time_in("Oggi ha giocato 31 minuti"),
            Some("oggi")
        );
        assert_eq!(
            the_relative_time_in("ci vediamo domani alle 9"),
            Some("domani")
        );
        assert_eq!(
            the_relative_time_in("la recita è la settimana prossima"),
            Some("settimana prossima")
        );
        assert_eq!(
            the_relative_time_in("watched it yesterday evening"),
            Some("yesterday")
        );
        assert_eq!(the_relative_time_in("oggigiorno tutto cambia"), None);
        assert_eq!(the_relative_time_in("il viaggio del 10 giugno 2026"), None);
    }

    /// The phrase comes back, not just a yes: a receipt names the word it
    /// objected to, and a reader who cannot see which word it was cannot
    /// judge whether the engine was right.
    #[test]
    fn it_names_the_word_it_found() {
        assert_eq!(
            the_relative_time_in("Zoe is out tonight and will not be home."),
            Some("tonight")
        );
        assert_eq!(
            the_relative_time_in("Zoe is out on the night of 7 March 2026."),
            None
        );
    }
}
