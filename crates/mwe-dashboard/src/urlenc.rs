// SPDX-License-Identifier: AGPL-3.0-or-later
//! Percent-encoding for the query strings this crate builds itself.
//!
//! Two places hand a browser a URL they assembled: the session layer
//! bounces an unauthenticated visitor to the sign-in page carrying the
//! page they asked for ([`crate::auth::session`]), and the OAuth consent
//! step bounces one carrying the whole authorization request
//! ([`crate::routes::webagentoauth`]). Both put a value inside a query,
//! and a value that is not encoded stops being one value.
//!
//! It is hand-rolled rather than pulled in: the dashboard depends on no
//! URL crate, the inputs are a local path, an auth code and an opaque
//! `state`, and a dependency for fifteen lines would be the larger thing
//! to maintain. It lives here rather than beside either caller so there
//! is one of it — the two differed by a single character in the allowed
//! set for as long as there were two, which is the shape this kind of
//! duplication always has.

/// Percent-encode one value for a query string: the RFC 3986 unreserved
/// set (`A-Z a-z 0-9 - _ . ~`) survives, everything else becomes `%XX`.
///
/// `/` is escaped like anything else. The result is uglier in the
/// address bar than a literal path would be, and that is the trade taken
/// deliberately: one rule for every value this crate encodes beats a
/// second rule that exists only because paths look nicer.
#[must_use]
pub fn query_value(raw: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            },
            other => {
                out.push('%');
                out.push(char::from(HEX[usize::from(other >> 4)]));
                out.push(char::from(HEX[usize::from(other & 0x0F)]));
            },
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unreserved_set_survives_and_everything_else_escapes() {
        assert_eq!(query_value("abcXYZ-_.~123"), "abcXYZ-_.~123");
        assert_eq!(
            query_value("/dashboard/x?a=b&c"),
            "%2Fdashboard%2Fx%3Fa%3Db%26c"
        );
    }

    /// The point of encoding at all: whatever goes in stays **one**
    /// value. It cannot split into a second parameter, open a fragment,
    /// or lose a space or an accent on the way.
    #[test]
    fn the_value_cannot_break_out_of_the_query() {
        assert_eq!(
            query_value("/wiki/bob&admin=1"),
            "%2Fwiki%2Fbob%26admin%3D1"
        );
        assert_eq!(query_value("/wiki/bob#top"), "%2Fwiki%2Fbob%23top");
        assert_eq!(query_value("a b"), "a%20b");
        assert_eq!(query_value("caffè"), "caff%C3%A8");
    }
}
