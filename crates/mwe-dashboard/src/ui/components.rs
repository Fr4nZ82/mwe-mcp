// SPDX-License-Identifier: AGPL-3.0-or-later
//! Small Maud helpers shared across pages — keeps form / table /
//! flash markup uniform without forcing every route to inline the
//! same html.

use std::fmt::Write;

use maud::{Markup, html};

/// Render a flash banner. `kind` should be `"error"`, `"success"`,
/// or `"info"`; the CSS in `app.css` covers the first two.
#[must_use]
pub fn flash(kind: &str, message: &str) -> Markup {
    html! {
        p.flash.(format!("flash-{kind}")) { (message) }
    }
}

/// Render a labeled text input. `kind` is one of `"text"`,
/// `"password"`, `"email"`; everything else falls back to `"text"`.
#[must_use]
pub fn text_field(name: &str, label: &str, kind: &str, value: &str, required: bool) -> Markup {
    let kind = match kind {
        "password" | "email" => kind,
        _ => "text",
    };
    html! {
        p {
            label for=(name) { (label) }
            input
                id=(name)
                name=(name)
                type=(kind)
                value=(value)
                required[required];
        }
    }
}

/// Like [`text_field`] but with an explicit `autocomplete` token.
///
/// (`"username"`, `"email"`, `"off"`, …) for credential-bearing forms
/// where the browser's password manager would otherwise guess wrong —
/// e.g. pasting the login email into a non-credential slug field, or
/// offering a saved password where a brand-new one is wanted.
#[must_use]
pub fn text_field_ac(
    name: &str,
    label: &str,
    kind: &str,
    value: &str,
    required: bool,
    autocomplete: &str,
) -> Markup {
    let kind = match kind {
        "password" | "email" => kind,
        _ => "text",
    };
    html! {
        p {
            label for=(name) { (label) }
            input
                id=(name)
                name=(name)
                type=(kind)
                value=(value)
                autocomplete=(autocomplete)
                required[required];
        }
    }
}

/// Render a labeled password input with an explicit `autocomplete` token.
///
/// Always `required`, never pre-fills the value (browsers would refuse to
/// autofill it anyway, and surfacing a "current password" placeholder in
/// markup leaks nothing useful).
///
/// `autocomplete` should be one of the WHATWG values:
/// `"current-password"` for "verify the password you already have",
/// `"new-password"` for "pick a brand new password", `"off"` for
/// secrets that are not the user's own (API keys, tokens).
#[must_use]
pub fn password_field(name: &str, label: &str, autocomplete: &str) -> Markup {
    html! {
        p {
            label for=(name) { (label) }
            input
                id=(name)
                name=(name)
                type="password"
                autocomplete=(autocomplete)
                required;
        }
    }
}

/// Render a labeled textarea — used for `profile`, `description`,
/// `scope`, etc. Pre-fills with the given value (escaped by Maud).
#[must_use]
pub fn text_area(name: &str, label: &str, value: &str, required: bool) -> Markup {
    html! {
        p {
            label for=(name) { (label) }
            textarea id=(name) name=(name) required[required] { (value) }
        }
    }
}

/// Render a primary submit button + the form action wrapper.
#[must_use]
pub fn submit(label: &str) -> Markup {
    html! {
        p { button type="submit" { (label) } }
    }
}

/// Render a horizontal tab bar that switches between sibling list views.
///
/// Each tab is `(label, href, active)`: the active one renders as a
/// highlighted non-link segment, the others as links. Used by the Wikis
/// page to flip between the Standard and Smart families without giving
/// each its own top-nav link. Only utility classes already present
/// elsewhere in the shell are used, so nothing new needs recompiling
/// into `tailwind.css`.
#[must_use]
pub fn tab_bar(tabs: &[(&str, &str, bool)]) -> Markup {
    html! {
        nav class="tab-bar flex flex-wrap items-center gap-2 mb-4 border-b border-border pb-2"
            role="tablist" {
            @for (label, href, active) in tabs {
                @if *active {
                    span class="px-3 py-1.5 text-xs font-bold text-phosphor bg-bg-3 rounded"
                        aria-current="page" { (label) }
                } @else {
                    a href=(href)
                        class="px-3 py-1.5 text-xs text-text-dim hover:text-phosphor hover:bg-bg-3 rounded no-underline transition-colors" {
                        (label)
                    }
                }
            }
        }
    }
}

/// Truncate `s` to at most `max` characters (not bytes), appending a
/// single trailing ellipsis when truncation actually happened.
///
/// Shared by the table cells in `proposals` and `facts` so every route
/// uses the same rule for body previews.
#[must_use]
pub fn truncate_chars(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (chars, c) in s.chars().enumerate() {
        if chars >= max {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
}

/// Render a small inline destructive form: one button that POSTs to
/// `action`. Used for "delete user", "revoke token", "logout".
#[must_use]
pub fn destructive_form(action: &str, label: &str, confirm: &str) -> Markup {
    html! {
        form action=(action) method="post" class="inline-form"
            onsubmit=(format!("return confirm({})", json_quote(confirm))) {
            button type="submit" class="danger" { (label) }
        }
    }
}

/// Minimal JSON-string encoder for the few literals we need to embed
/// in inline JavaScript (just the `confirm()` argument). We avoid
/// pulling `serde_json::to_string` for a constant in markup.
fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            },
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_renders_with_kind_class() {
        let m = flash("error", "boom").into_string();
        assert!(m.contains("flash-error"));
        assert!(m.contains("boom"));
    }

    #[test]
    fn text_field_ac_emits_autocomplete() {
        let m = text_field_ac("email", "Email", "email", "", true, "username").into_string();
        assert!(m.contains("autocomplete=\"username\""));
        assert!(m.contains("type=\"email\""));
        assert!(m.contains("for=\"email\""));
    }

    #[test]
    fn json_quote_handles_quotes_and_backslashes() {
        assert_eq!(json_quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
