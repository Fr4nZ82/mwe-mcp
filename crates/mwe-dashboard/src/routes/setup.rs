// SPDX-License-Identifier: AGPL-3.0-or-later
//! First-run setup wizard (see
//! setup and identity).
//!
//! Served **only while no admin exists**. Once the first row with
//! `is_admin = 1` is in the DB, every GET/POST here redirects to
//! `/dashboard/login`. The route stays mounted: the "disappearing" is
//! an application guard, not a routing decision.
//!
//! The wizard asks for **email + `user_id` slug + password**: the
//! email is the login channel (and the SMTP recovery channel); the
//! `user_id` slug stays the domain principal in markers and the
//! directory name on disk. Besides creating the admin and their
//! credential, the wizard **auto-creates the admin's personal wiki** —
//! so the first dashboard visit already shows your empty wiki ready to
//! receive captures.

use axum::extract::State;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;
use mwe_core::enrollment::{EnrollmentFile, UserEntry, validate};
use mwe_core::types::WikiId;
use mwe_core::wiki::{IdentityKind, create_identity_wiki};
use serde::Deserialize;

use crate::auth::{password, session::issue_session_cookie};
use crate::error::{DashboardError, Result};
use crate::routes::redirect::admin_exists;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// GET `/dashboard/setup` — render the wizard, or 303 to login if an
/// admin already exists.
pub async fn form(State(state): State<DashboardState>) -> Result<Response> {
    if admin_exists(&state).await? {
        return Ok(Redirect::to("/dashboard/login").into_response());
    }
    Ok(render_form(&state, None, "", "").into_response())
}

/// Body of the POST submission.
#[derive(Debug, Deserialize)]
pub struct SetupSubmission {
    /// Email — the credential used at login (and the future SMTP
    /// recovery target). Stored on `enrollment_users.email`.
    pub email: String,
    /// `user_id` slug — the canonical principal used in markers, on
    /// disk, and as the JWT `sender`. Validated against the enrollment
    /// regex.
    pub admin_id: String,
    /// New password.
    pub password: String,
    /// Confirmation — must match `password` exactly.
    pub password_confirm: String,
}

/// POST `/dashboard/setup` — create the admin row, hash + store the
/// password, mint a session cookie, redirect to `/home`.
pub async fn submit(
    State(state): State<DashboardState>,
    jar: CookieJar,
    axum::Form(form): axum::Form<SetupSubmission>,
) -> Result<Response> {
    // Re-check the guard: even if the GET form leaked to a tab, the
    // POST must refuse if an admin has been created in the meantime.
    if admin_exists(&state).await? {
        return Ok(Redirect::to("/dashboard/login").into_response());
    }

    let email = form.email.trim();
    let admin_id = form.admin_id.trim();
    let password = form.password.as_str();
    let confirm = form.password_confirm.as_str();

    if let Some(msg) = validate_setup(&state, email, admin_id, password, confirm) {
        return Ok(render_form(&state, Some(&msg), email, admin_id).into_response());
    }

    // Build a one-row EnrollmentFile and feed it through the canonical
    // validator so the same regex / filesystem-safe rules apply to the
    // first admin as to every other user.
    let file = EnrollmentFile {
        version: 1,
        users: vec![UserEntry {
            id: admin_id.to_owned(),
            aliases: Vec::new(),
            is_admin: true,
            locale: None,
        }],
        groups: Vec::new(),
    };
    if let Err(e) = validate(&file) {
        return Ok(render_form(&state, Some(&e.to_string()), email, admin_id).into_response());
    }

    // Insert admin row + credentials in a single transaction so the
    // wizard cannot leave a half-finished bootstrap. The identity wiki
    // is created **after** the commit because it's a filesystem write
    // and would not be cleanly rolled back if the SQL fails — better
    // to have DB-without-wiki than DB-without-DB at half-commit.
    let phc = password::hash(password)?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO enrollment_users (user_id, email, aliases, is_admin)
         VALUES (?, ?, NULL, 1)",
    )
    .bind(admin_id)
    .bind(email)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO user_credentials (user_id, password_hash, hashed_at, must_change)
         VALUES (?, ?, ?, 0)",
    )
    .bind(admin_id)
    .bind(&phc)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(
        admin = %admin_id,
        email = %email,
        "dashboard setup wizard created the deployment admin"
    );

    // Auto-create the admin's identity wiki. When the memory
    // bundle is missing (e.g. an integration test that wires DashboardState
    // without the WikiTree), we log a warning and continue — the admin
    // can create the wiki manually later. In a real `mwe-mcp serve` the
    // bundle is always populated.
    if let Some(memory) = state.memory.as_ref() {
        let wiki_id =
            WikiId::parse(admin_id).map_err(|e| DashboardError::Internal(e.to_string()))?;
        match create_identity_wiki(&memory.tree, &wiki_id, admin_id, IdentityKind::User) {
            Ok(outcome) => {
                tracing::info!(
                    admin = %admin_id,
                    wiki_id = wiki_id.as_str(),
                    created = outcome.created,
                    "identity wiki materialized for admin"
                );
            },
            Err(e) => {
                // Filesystem failure is non-fatal here: the admin
                // still has a working DB account and can run
                // `mwe-mcp doctor` to investigate. Log loudly.
                tracing::error!(
                    admin = %admin_id,
                    error = %e,
                    "identity wiki creation failed; admin DB account is up but personal wiki must be created manually"
                );
            },
        }
    } else {
        tracing::warn!(
            admin = %admin_id,
            "setup wizard ran without a WikiTree bundle; identity wiki was not created"
        );
    }

    let cookie = issue_session_cookie(&state, admin_id, true)?;
    // Fresh admin → the LLM config page first. It is step 1 of onboarding:
    // the profile primer that follows calls `wiki_ingest_message` and needs
    // a usable ingest model, so the model is wired before the primer runs.
    // The page shows a step-1 banner while the primer is pending, and
    // `welcome::form` guards the primer (bouncing back here until the
    // ingest role is usable). Both redirects are idempotent once the
    // profile flag is set, so a re-navigated `/dashboard/setup` is safe.
    Ok((jar.add(cookie), Redirect::to("/dashboard/admin/llm-config")).into_response())
}

fn validate_setup(
    state: &DashboardState,
    email: &str,
    admin_id: &str,
    password: &str,
    confirm: &str,
) -> Option<String> {
    if email.is_empty() {
        return Some("Choose an email.".to_owned());
    }
    if !is_plausible_email(email) {
        return Some("Email must look like name@domain.tld.".to_owned());
    }
    if admin_id.is_empty() {
        return Some("Choose an admin id.".to_owned());
    }
    if password.len() < state.config.min_password_len {
        return Some(format!(
            "Password must be at least {} characters.",
            state.config.min_password_len
        ));
    }
    if password != confirm {
        return Some("The two passwords do not match.".to_owned());
    }
    None
}

/// Tiny syntactic check for the email field — accepts anything with
/// exactly one `@` between non-empty halves and a `.` in the domain.
/// Not RFC-5322-conformant; mwe-mcp leaves real validation to the
/// recovery flow when SMTP arrives. Shared with the admin "Add user" /
/// "Edit user" forms ([`super::users`]) so every email entry point
/// applies the same rule.
pub(super) fn is_plausible_email(s: &str) -> bool {
    let parts: Vec<&str> = s.split('@').collect();
    if parts.len() != 2 {
        return false;
    }
    let (local, domain) = (parts[0], parts[1]);
    if local.is_empty() || domain.is_empty() {
        return false;
    }
    if !domain.contains('.') {
        return false;
    }
    if s.contains(char::is_whitespace) {
        return false;
    }
    true
}

fn render_form(
    state: &DashboardState,
    error: Option<&str>,
    email: &str,
    admin_id: &str,
) -> Html<String> {
    // Pre-fill admin_id from the email's local part when the operator
    // has typed an email but not yet a slug. Quality-of-life touch:
    // most operators want the two to match (e.g. franz@example.com →
    // franz) and only edit the slug if the local part is unsuitable.
    let admin_id_suggestion = if admin_id.is_empty() && !email.is_empty() {
        email.split('@').next().unwrap_or("").to_owned()
    } else {
        admin_id.to_owned()
    };
    let body = maud::html! {
        p { "Welcome. This deployment is empty, so the dashboard is asking you to "
            "create the single admin account that will manage it from now on. "
            "There is exactly one admin per deployment." }

        @if let Some(msg) = error { (components::flash("error", msg)) }

        form action="/dashboard/setup" method="post" {
            (components::text_field_ac("email", "Email (e.g. you@example.com)", "email", email, true, "username"))
            p.help.muted { "Used to sign in. Recovery via email arrives in a later milestone." }
            (components::text_field_ac("admin_id", "Admin id (slug, e.g. franz)", "text", &admin_id_suggestion, true, "off"))
            p.help.muted { "Lowercase letters, digits, underscore, and `°` only. Must start with a letter. Becomes your principal in markers and the name of your personal memory wiki." }
            (components::password_field("password", "Password", "new-password"))
            (components::password_field("password_confirm", "Confirm password", "new-password"))
            p.help.muted { "Minimum " (state.config.min_password_len) " characters." }
            (components::submit("Create admin"))
        }
    };
    Html(layout::anonymous_page("Set up admin", &body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_plausible_email_accepts_common_shapes() {
        assert!(is_plausible_email("franz@example.com"));
        assert!(is_plausible_email("a.b+tag@sub.example.co.uk"));
    }

    #[test]
    fn is_plausible_email_rejects_obvious_garbage() {
        assert!(!is_plausible_email(""));
        assert!(!is_plausible_email("plain"));
        assert!(!is_plausible_email("@example.com"));
        assert!(!is_plausible_email("franz@"));
        assert!(!is_plausible_email("franz@example"));
        assert!(!is_plausible_email("two@@signs.com"));
        assert!(!is_plausible_email("a b@example.com"));
    }
}
