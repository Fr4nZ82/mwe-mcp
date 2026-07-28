// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-gated user management (see the
//! dashboard design note).
//!
//! Five handlers:
//!
//! - GET  `/users`            — list of users with status (active /
//!   pending invitation) and per-row edit / delete / regenerate-invite
//!   links.
//! - GET  `/users/new`        — form to create a new regular user.
//! - POST `/users/new`        — validate, insert user row, mint a fresh
//!   `user_invitations` row, email the one-shot link when SMTP is
//!   configured, and render the page with it as a backup.
//! - GET  `/users/:id`        — edit form for `email` and `aliases`.
//! - POST `/users/:id`        — apply the edit. `is_admin` is never
//!   shown (see the
//!   dashboard design note).
//! - POST `/users/:id/delete` — delete the user via
//!   [`enrollment::remove_user`]: consumers bound to the identity
//!   (`consumers.system_user_id`) are dismantled with it, and CASCADE
//!   clears `user_credentials` + `user_invitations`.
//! - POST `/users/:id/reinvite` — replace any open invitation for this
//!   user with a fresh one, email the new link when SMTP is configured,
//!   and re-render the list with it as a backup.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::Utc;
use maud::html;
use mwe_core::enrollment;
use mwe_core::types::WikiId;
use mwe_core::wiki::{IdentityKind, create_identity_wiki};
use serde::Deserialize;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::routes::setup::is_plausible_email;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Sub-router for `/users/*`. Mounted inside the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/users", get(list))
        .route("/users/new", get(new_form).post(new_submit))
        .route("/users/:id", get(edit_form).post(edit_submit))
        .route("/users/:id/delete", post(delete))
        .route("/users/:id/reinvite", post(reinvite))
        .route("/users/:id/reset-2fa", post(reset_2fa))
}

/// Raw tuple from the user-list query (`user_id`, `is_admin`, `email`,
/// `cred_id`, open `invitation_id`). Aliased to keep the `query_as`
/// turbofish under clippy's type-complexity bar.
type UserListRow = (String, i64, Option<String>, Option<String>, Option<String>);

/// Raw tuple from the edit-form load query (`email`, `aliases` JSON,
/// `is_admin`, `require_2fa`, `timezone`).
type EditUserRow = (
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<String>,
    Option<String>,
);

/// Row shape used by the listing page.
#[derive(Debug)]
struct UserRow {
    user_id: String,
    is_admin: bool,
    /// Login email, set by the admin at invite. `None` only for legacy
    /// rows created before the email became mandatory.
    email: Option<String>,
    has_credentials: bool,
    open_invitation: Option<String>,
}

async fn fetch_users(state: &DashboardState) -> Result<Vec<UserRow>> {
    let now = Utc::now().to_rfc3339();
    let rows: Vec<UserListRow> = sqlx::query_as(
        "SELECT u.user_id,
                u.is_admin,
                u.email,
                c.user_id AS cred_id,
                (SELECT invitation_id FROM user_invitations i
                  WHERE i.user_id = u.user_id
                    AND i.consumed_at IS NULL
                    AND i.expires_at > ?
                  ORDER BY i.created_at DESC LIMIT 1) AS invitation_id
           FROM enrollment_users u
           LEFT JOIN user_credentials c ON c.user_id = u.user_id
          ORDER BY u.is_admin DESC, u.user_id ASC",
    )
    .bind(&now)
    .fetch_all(&state.pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(user_id, is_admin, email, cred_id, invitation_id)| UserRow {
                user_id,
                is_admin: is_admin != 0,
                email,
                has_credentials: cred_id.is_some(),
                open_invitation: invitation_id,
            },
        )
        .collect())
}

async fn list(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let users = fetch_users(&state).await?;
    Ok(Html(render_list(chrome, &users, admin.session(), None)))
}

fn render_list(
    chrome: layout::Chrome,
    users: &[UserRow],
    session: &crate::auth::SessionUser,
    flash_msg: Option<(&str, &str)>,
) -> String {
    let body = html! {
        @if let Some((kind, msg)) = flash_msg {
            (components::flash(kind, msg))
        }

        p { a href="/dashboard/users/new" class="primary-action" { "+ Add user" } }

        table {
            thead { tr {
                th { "User id" }
                th { "Email" }
                th { "Role" }
                th { "Status" }
                th { "Actions" }
            } }
            tbody {
                @for u in users {
                    tr {
                        td { (u.user_id) }
                        td {
                            @if let Some(email) = &u.email { (email) }
                            @else { span.muted { "— no email (can't sign in)" } }
                        }
                        td { @if u.is_admin { "admin" } @else { "user" } }
                        td {
                            @if u.has_credentials { "active" }
                            @else if let Some(invitation_id) = &u.open_invitation {
                                "invited — "
                                a href=(format!("/dashboard/accept-invite/{invitation_id}")) {
                                    "share link"
                                }
                            }
                            @else { "no credentials, no invitation" }
                        }
                        td {
                            a href=(format!("/dashboard/users/{}", u.user_id)) { "edit" }
                            " · "
                            @if u.is_admin {
                                span.muted { "admin row — managed by setup wizard / CLI" }
                            } @else {
                                form action=(format!("/dashboard/users/{}/reinvite", u.user_id))
                                     method="post" class="inline-form" {
                                    button type="submit" class="link-button" { "reinvite" }
                                }
                                " · "
                                form action=(format!("/dashboard/users/{}/delete", u.user_id))
                                     method="post" class="inline-form"
                                     onsubmit=(format!(
                                        "return confirm('Delete user {}? This also drops their credentials.')",
                                        u.user_id
                                     )) {
                                    button type="submit" class="link-button danger" { "delete" }
                                }
                            }
                        }
                    }
                }
                @if users.is_empty() {
                    tr { td colspan="5" class="muted" { "No users yet." } }
                }
            }
        }
    };
    layout::authenticated_page(chrome, "Users", session, &body)
}

#[derive(Debug, Deserialize, Default)]
pub struct NewUserSubmission {
    pub user_id: String,
    /// Login email — mandatory, set by the admin here at invite time and
    /// changeable only from the edit page. It is the user's only sign-in
    /// identifier (see [`super::login`]).
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub aliases: String,
    /// Optional IANA timezone (`Europe/Rome`) — where this user lives.
    /// Used to stamp wall-clock times they speak during ingest; empty →
    /// the deployment-wide `recall.ingest_timezone` applies.
    #[serde(default)]
    pub timezone: String,
    /// BCP-47 language this person's memory is written in (`it`, `en-GB`).
    /// Empty → the engine falls back to English, so an admin onboarding a
    /// non-English household should set it here rather than leave it blank.
    #[serde(default)]
    pub locale: String,
}

/// Shape-check the two columns the *engine plumbing* reads off a user
/// row — `timezone` (reference-time stamping) and `locale` (the prompt
/// LANGUAGE directive). Both forms parse them the same way and reject
/// with the same message, so they are parsed together and the caller
/// only has to decide how to re-render.
fn parse_engine_columns(
    timezone: &str,
    locale: &str,
) -> std::result::Result<(Option<String>, Option<String>), String> {
    Ok((parse_timezone_field(timezone)?, parse_locale_field(locale)?))
}

/// Light shape check for a typed BCP-47 locale field: empty → `None`;
/// otherwise a single token of at most 32 chars. This is the language
/// the engine writes this person's memory in — every slot that composes
/// prose resolves it through [`mwe_core::enrollment::locale_for`], so a
/// blank here is not cosmetic: it drops the user to the deployment
/// fallback. Same deliberately loose gate as the timezone field above —
/// the tag reaches the prompt verbatim and "obviously not a locale" is
/// all we check.
fn parse_locale_field(raw: &str) -> std::result::Result<Option<String>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.len() > 32 || t.chars().any(char::is_whitespace) {
        return Err("Language must be a single BCP-47 tag like en, en-GB or it-IT.".to_owned());
    }
    Ok(Some(t.to_owned()))
}

/// Light shape check for a typed IANA timezone field: empty → `None`;
/// otherwise a single token of at most 64 chars. The value reaches the
/// classifier prompt verbatim, so "obviously not a timezone" is the
/// gate — full IANA validation is deliberately not attempted (same
/// stance as the deployment-wide field on the Settings page).
fn parse_timezone_field(raw: &str) -> std::result::Result<Option<String>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.len() > 64 || t.chars().any(char::is_whitespace) {
        return Err("Timezone must be a single IANA name like Europe/Rome.".to_owned());
    }
    Ok(Some(t.to_owned()))
}

async fn new_form(State(state): State<DashboardState>, admin: AdminUser) -> Html<String> {
    let chrome = layout::Chrome::of(&state);
    Html(render_new_form(
        chrome,
        admin.session(),
        &NewUserSubmission::default(),
        None,
    ))
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear create-user flow: validate, insert, invite, re-render"
)]
async fn new_submit(
    State(state): State<DashboardState>,
    admin: AdminUser,
    headers: HeaderMap,
    axum::Form(form): axum::Form<NewUserSubmission>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let user_id = form.user_id.trim();
    if let Some(msg) = validate_user_id_for_create(&state, user_id).await? {
        return Ok(
            Html(render_new_form(chrome, admin.session(), &form, Some(&msg))).into_response(),
        );
    }
    let email = form.email.trim();
    if let Some(msg) = validate_email_for_account(&state, email, None).await? {
        return Ok(
            Html(render_new_form(chrome, admin.session(), &form, Some(&msg))).into_response(),
        );
    }

    let (timezone, locale) = match parse_engine_columns(&form.timezone, &form.locale) {
        Ok(pair) => pair,
        Err(msg) => {
            return Ok(
                Html(render_new_form(chrome, admin.session(), &form, Some(&msg))).into_response(),
            );
        },
    };

    let aliases = parse_aliases(&form.aliases);
    let aliases_json = serde_json::to_string(&aliases)
        .map_err(|e| DashboardError::Internal(format!("encoding aliases: {e}")))?;

    let now = Utc::now();
    let invitation_id =
        uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new())).to_string();
    let expires_at = now + chrono::Duration::hours(state.config.invitation_ttl_hours);

    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO enrollment_users (user_id, email, aliases, is_admin, timezone, locale)
         VALUES (?, ?, ?, 0, ?, ?)",
    )
    .bind(user_id)
    .bind(email)
    .bind(&aliases_json)
    .bind(timezone.as_deref())
    .bind(locale.as_deref())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO user_invitations
            (invitation_id, user_id, created_at, expires_at, consumed_at, invited_by)
         VALUES (?, ?, ?, ?, NULL, ?)",
    )
    .bind(&invitation_id)
    .bind(user_id)
    .bind(now.to_rfc3339())
    .bind(expires_at.to_rfc3339())
    .bind(admin.sender_id())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(
        actor = admin.sender_id(),
        user = user_id,
        email = email,
        "dashboard created user + invitation"
    );

    // Materialize the user's personal identity wiki right away,
    // post-commit, so the new user lands on a working wiki the first
    // time they sign in. Filesystem failures are non-fatal (the DB
    // account is already up; the admin can investigate via
    // `mwe-mcp doctor`).
    if let Some(memory) = state.memory.as_ref() {
        match WikiId::parse(user_id) {
            Ok(wiki_id) => {
                if let Err(e) =
                    create_identity_wiki(&memory.tree, &wiki_id, user_id, IdentityKind::User)
                {
                    tracing::error!(
                        user = user_id,
                        error = %e,
                        "identity wiki creation failed for new user; DB row is up, run mwe-mcp doctor"
                    );
                }
            },
            Err(e) => {
                tracing::error!(
                    user = user_id,
                    error = %e,
                    "user id failed WikiId::parse; identity wiki not created"
                );
            },
        }
    }

    let emailed = deliver_invitation(&state, &headers, Some(email), &invitation_id);
    let users = fetch_users(&state).await?;
    let ttl = state.config.invitation_ttl_hours;
    let msg = emailed.map_or_else(
        || {
            format!(
                "Created {user_id}. Share this single-use link \
                 (expires in {ttl}h): /dashboard/accept-invite/{invitation_id}"
            )
        },
        |to| {
            format!(
                "Created {user_id} and emailed the sign-in link to {to}. Backup single-use \
                 link (expires in {ttl}h): /dashboard/accept-invite/{invitation_id}"
            )
        },
    );
    Ok(Html(render_list(
        chrome,
        &users,
        admin.session(),
        Some(("success", &msg)),
    ))
    .into_response())
}

fn render_new_form(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    form: &NewUserSubmission,
    error: Option<&str>,
) -> String {
    let body = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }

        p.muted {
            "There is exactly one admin per deployment, so new users created here are regular users. "
            "Use the CLI break-glass " code { "mwe-mcp admin-reset" } " to recover the admin if needed."
        }

        form action="/dashboard/users/new" method="post" {
            (components::text_field("user_id", "User id", "text", &form.user_id, true))
            p.help.muted { "Lowercase letters, digits, underscore, and `°` only. Must start with a letter." }
            (components::text_field_ac("email", "Email", "email", &form.email, true, "off"))
            p.help.muted { "The user signs in with this email. Required, and only you (the admin) can change it later." }
            (components::text_field("aliases", "Aliases (comma-separated)", "text", &form.aliases, false))
            (components::text_field("timezone", "Timezone (IANA, optional)", "text", &form.timezone, false))
            (components::text_field("locale", "Language (BCP-47, e.g. en-GB or it)", "text", &form.locale, false))
            p.help.muted {
                "The language this person's memory is written in. It is not only how an "
                "assistant replies: every page the engine compiles for them is written in it. "
                "Leave it blank and the deployment falls back to English."
            }
            p.help.muted {
                "Where this user lives, e.g. " code { "Europe/Rome" } " or "
                code { "Australia/Sydney" } ". Times they speak (\"tomorrow at 9\") "
                "are read in this zone; empty = the deployment-wide default."
            }
            (components::submit("Create user + invitation link"))
        }

        p { a href="/dashboard/users" { "Cancel and return to the list" } }
    };
    layout::authenticated_reading_page(chrome, "Add user", session, &body)
}

async fn validate_user_id_for_create(
    state: &DashboardState,
    user_id: &str,
) -> Result<Option<String>> {
    if user_id.is_empty() {
        return Ok(Some("Choose a user id.".into()));
    }
    if !enrollment::is_valid_user_id(user_id) {
        return Ok(Some(
            "Id must match /^[a-z][a-z0-9°]*$/ — no underscores (the id becomes the identity \
             wiki's id)."
                .into(),
        ));
    }
    if enrollment::is_guest(user_id) {
        return Ok(Some(
            "\"guest\" is the builtin unidentified-human pseudo-identity and cannot be \
             enrolled as a user."
                .into(),
        ));
    }
    if !enrollment::is_filesystem_safe(user_id) {
        return Ok(Some(
            "Id is not filesystem-safe (slashes, '..', whitespace).".into(),
        ));
    }
    let user_collision: i64 =
        sqlx::query_scalar("SELECT count(*) FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(&state.pool)
            .await?;
    if user_collision > 0 {
        return Ok(Some(format!("User id {user_id:?} already exists.")));
    }
    let group_collision: i64 =
        sqlx::query_scalar("SELECT count(*) FROM enrollment_groups WHERE group_id = ?")
            .bind(user_id)
            .fetch_one(&state.pool)
            .await?;
    if group_collision > 0 {
        return Ok(Some(format!(
            "Id {user_id:?} clashes with an existing group — pick another id."
        )));
    }
    Ok(None)
}

/// Validate an account email for create / edit: non-empty, syntactically
/// plausible, and unique across `enrollment_users`. `exclude_user_id` is
/// the row being edited (so it does not clash with itself); pass `None`
/// on create. Returns `Some(message)` to re-render the form with the
/// error, `None` when the email is acceptable.
async fn validate_email_for_account(
    state: &DashboardState,
    email: &str,
    exclude_user_id: Option<&str>,
) -> Result<Option<String>> {
    if email.is_empty() {
        return Ok(Some(
            "Enter an email — it is the user's only way to sign in.".into(),
        ));
    }
    if !is_plausible_email(email) {
        return Ok(Some("Email must look like name@domain.tld.".into()));
    }
    let clash: i64 = match exclude_user_id {
        Some(uid) => {
            sqlx::query_scalar(
                "SELECT count(*) FROM enrollment_users WHERE email = ? AND user_id != ?",
            )
            .bind(email)
            .bind(uid)
            .fetch_one(&state.pool)
            .await?
        },
        None => {
            sqlx::query_scalar("SELECT count(*) FROM enrollment_users WHERE email = ?")
                .bind(email)
                .fetch_one(&state.pool)
                .await?
        },
    };
    if clash > 0 {
        return Ok(Some(format!(
            "Email {email:?} is already used by another user."
        )));
    }
    Ok(None)
}

#[derive(Debug, Deserialize)]
pub struct EditUserSubmission {
    /// Login email. Only the admin reaches this form, so this is the one
    /// place a user's email can change after invite.
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub aliases: String,
    /// Optional IANA timezone — see [`NewUserSubmission::timezone`].
    #[serde(default)]
    pub timezone: String,
    /// BCP-47 language — see [`NewUserSubmission::locale`].
    #[serde(default)]
    pub locale: String,
    /// Admin "require 2FA on this account" checkbox — present only when
    /// checked (roadmap 28).
    #[serde(default)]
    pub require_2fa: Option<String>,
}

async fn edit_form(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(user_id): Path<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let row: Option<EditUserRow> = sqlx::query_as(
        "SELECT email, aliases, is_admin, require_2fa, timezone, locale
           FROM enrollment_users WHERE user_id = ?",
    )
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await?;
    let (email, aliases_json, is_admin, require_2fa, timezone, locale) =
        row.ok_or(DashboardError::NotFound)?;
    let aliases: Vec<String> = aliases_json
        .as_deref()
        .map(|s| serde_json::from_str(s).unwrap_or_default())
        .unwrap_or_default();
    let pre = EditUserSubmission {
        email: email.unwrap_or_default(),
        aliases: aliases.join(", "),
        timezone: timezone.unwrap_or_default(),
        locale: locale.unwrap_or_default(),
        require_2fa: (require_2fa != 0).then(|| "1".to_owned()),
    };
    let twofa_enabled = crate::twofa::is_enabled(&state.pool, &user_id).await?;
    Ok(Html(render_edit_form(
        chrome,
        admin.session(),
        &user_id,
        is_admin != 0,
        &pre,
        twofa_enabled,
        None,
    )))
}

async fn edit_submit(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(user_id): Path<String>,
    axum::Form(form): axum::Form<EditUserSubmission>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let is_admin_raw: Option<i64> =
        sqlx::query_scalar("SELECT is_admin FROM enrollment_users WHERE user_id = ?")
            .bind(&user_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(is_admin_raw) = is_admin_raw else {
        return Err(DashboardError::NotFound);
    };

    let require_2fa = form.require_2fa.is_some();
    let email = form.email.trim();
    // Re-render the edit form with what the admin typed, plus the error.
    let reject = |msg: &str, twofa_enabled: bool| {
        let pre = EditUserSubmission {
            email: form.email.clone(),
            aliases: form.aliases.clone(),
            timezone: form.timezone.clone(),
            locale: form.locale.clone(),
            require_2fa: require_2fa.then(|| "1".to_owned()),
        };
        Html(render_edit_form(
            chrome,
            admin.session(),
            &user_id,
            is_admin_raw != 0,
            &pre,
            twofa_enabled,
            Some(msg),
        ))
        .into_response()
    };
    if let Some(msg) = validate_email_for_account(&state, email, Some(&user_id)).await? {
        let twofa_enabled = crate::twofa::is_enabled(&state.pool, &user_id).await?;
        return Ok(reject(&msg, twofa_enabled));
    }
    let (timezone, locale) = match parse_engine_columns(&form.timezone, &form.locale) {
        Ok(pair) => pair,
        Err(msg) => {
            let twofa_enabled = crate::twofa::is_enabled(&state.pool, &user_id).await?;
            return Ok(reject(&msg, twofa_enabled));
        },
    };

    let aliases = parse_aliases(&form.aliases);
    let aliases_json = serde_json::to_string(&aliases)
        .map_err(|e| DashboardError::Internal(format!("encoding aliases: {e}")))?;

    sqlx::query(
        "UPDATE enrollment_users
            SET email = ?, aliases = ?, require_2fa = ?, timezone = ?, locale = ?
          WHERE user_id = ?",
    )
    .bind(email)
    .bind(&aliases_json)
    .bind(i64::from(require_2fa))
    .bind(timezone.as_deref())
    .bind(locale.as_deref())
    .bind(&user_id)
    .execute(&state.pool)
    .await?;

    tracing::info!(actor = admin.sender_id(), user = %user_id, "dashboard edited user");

    let users = fetch_users(&state).await?;
    let msg = format!("Updated user {user_id}.");
    Ok(Html(render_list(
        chrome,
        &users,
        admin.session(),
        Some(("success", &msg)),
    ))
    .into_response())
}

fn render_edit_form(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    user_id: &str,
    is_admin: bool,
    form: &EditUserSubmission,
    twofa_enabled: bool,
    error: Option<&str>,
) -> String {
    let title = format!("Edit user — {user_id}");
    let require_2fa = form.require_2fa.is_some();
    let body = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }

        p.muted {
            "The user id and the admin role cannot be changed here. "
            "Delete and re-create if you really need a new id."
        }

        form action=(format!("/dashboard/users/{user_id}")) method="post" {
            p { label { "User id" } p { code { (user_id) } " · "
                @if is_admin { "admin" } @else { "user" }
            } }
            (components::text_field_ac("email", "Email", "email", &form.email, true, "off"))
            p.help.muted { "The user's sign-in email. You are the only one who can change it." }
            (components::text_field("aliases", "Aliases (comma-separated)", "text", &form.aliases, false))
            (components::text_field("timezone", "Timezone (IANA, optional)", "text", &form.timezone, false))
            (components::text_field("locale", "Language (BCP-47, e.g. en-GB or it)", "text", &form.locale, false))
            p.help.muted {
                "The language this person's memory is written in. It is not only how an "
                "assistant replies: every page the engine compiles for them is written in it. "
                "Leave it blank and the deployment falls back to English."
            }
            p.help.muted {
                "Where this user lives, e.g. " code { "Europe/Rome" } " or "
                code { "Australia/Sydney" } ". Times they speak (\"tomorrow at 9\") "
                "are read in this zone; empty = the deployment-wide default."
            }
            p {
                label for="require_2fa" {
                    input id="require_2fa" name="require_2fa" type="checkbox" value="1"
                        checked[require_2fa];
                    " Require two-factor authentication"
                }
            }
            p.help.muted {
                "When on, this user must set up 2FA before using the dashboard. "
                "2FA currently " strong { @if twofa_enabled { "ON" } @else { "off" } }
                " for this account."
            }
            (components::submit("Save"))
        }

        @if twofa_enabled {
            h3 { "Two-factor reset (break-glass)" }
            p.muted {
                "Clears this user's 2FA enrollment so they can sign in with just a "
                "password and set it up again — for a lost authenticator."
            }
            (components::destructive_form(
                &format!("/dashboard/users/{user_id}/reset-2fa"),
                "Reset this user's 2FA",
                "Clear this user's two-factor enrollment?"
            ))
        }

        p { a href="/dashboard/users" { "Back to the list" } }
    };
    layout::authenticated_reading_page(chrome, &title, session, &body)
}

/// Admin break-glass: clear a user's 2FA enrollment (lost authenticator).
async fn reset_2fa(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(user_id): Path<String>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    crate::twofa::disable(&state.pool, &user_id).await?;
    tracing::info!(actor = admin.sender_id(), user = %user_id, "dashboard reset user 2FA");
    let users = fetch_users(&state).await?;
    let msg = format!("Cleared two-factor for {user_id}.");
    Ok(Html(render_list(
        chrome,
        &users,
        admin.session(),
        Some(("success", &msg)),
    ))
    .into_response())
}

async fn delete(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(user_id): Path<String>,
) -> Result<Response> {
    let is_admin: Option<i64> =
        sqlx::query_scalar("SELECT is_admin FROM enrollment_users WHERE user_id = ?")
            .bind(&user_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(is_admin_raw) = is_admin else {
        return Err(DashboardError::NotFound);
    };
    if is_admin_raw != 0 {
        return Err(DashboardError::Validation(
            "Refusing to delete the deployment admin from the dashboard. \
             Use the CLI / direct DB if you really mean it."
                .into(),
        ));
    }

    // One transactional teardown: consumers bound to this identity
    // (`consumers.system_user_id`), their delegation grants, the user's
    // web-agent OAuth rows, then the enrollment row itself (CASCADE takes
    // credentials, invitations, 2FA, votes).
    let removal = enrollment::remove_user(&state.pool, &user_id)
        .await?
        .ok_or(DashboardError::NotFound)?;
    if !removal.consumers_dismantled.is_empty() {
        // Act-as for the dismantled consumers must die on the next call,
        // not within the cache TTL. Best-effort: the TTL self-heals.
        if let Err(error) = state.delegations.refresh(&state.pool).await {
            tracing::warn!(%error, "delegation cache refresh failed after user delete");
        }
        tracing::info!(
            user = %user_id,
            consumers = ?removal.consumers_dismantled,
            oauth_rows = removal.oauth_rows_removed,
            "user delete dismantled bound consumer registrations"
        );
    }

    // 23d: a contribution outlives its author. Reassign any fact this user
    // authored (sender = user:<id>) to its wiki's scope principal so no active
    // fact is left pointing at a vanished sender (the sender-scrub invariant —
    // docs/concepts/identity-and-acl.md). Best-effort —
    // a failure (or absent memory handles) is logged, never blocks the delete.
    if let Some(memory) = state.memory.as_ref() {
        let gone = mwe_core::types::Principal::User(user_id.clone());
        match mwe_core::fact_index::reassign_sender_to_scope(&state.pool, &memory.tree, &gone).await
        {
            Ok(n) => {
                tracing::info!(user = %user_id, reassigned = n, "23d: user delete reassigned facts' sender to wiki scope");
            },
            Err(e) => {
                tracing::warn!(user = %user_id, error = %e, "23d: sender reassignment failed after user delete");
            },
        }
    } else {
        tracing::warn!(user = %user_id, "23d: memory handles unavailable — facts' sender not reassigned");
    }

    tracing::info!(actor = admin.sender_id(), user = %user_id, "dashboard deleted user");
    Ok(Redirect::to("/dashboard/users").into_response())
}

async fn reinvite(
    State(state): State<DashboardState>,
    admin: AdminUser,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let row: Option<(i64, Option<String>)> =
        sqlx::query_as("SELECT is_admin, email FROM enrollment_users WHERE user_id = ?")
            .bind(&user_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((is_admin_raw, email)) = row else {
        return Err(DashboardError::NotFound);
    };
    if is_admin_raw != 0 {
        return Err(DashboardError::Validation(
            "Use `mwe-mcp admin-reset` for the admin, not the dashboard.".into(),
        ));
    }
    // Mutual exclusion, agent side (migration 0050): a consumer-agent identity is
    // credential-less by construction (it authenticates with a standard token,
    // not a login). Never mint a dashboard invitation for it — the mirror of
    // `validate_token_identity`, which refuses a standard token to a credentialed
    // human. An identity is either a human with a login or a bot, never both.
    if let Err(msg) = mwe_core::enrollment::reject_if_agent(&state.pool, &user_id).await {
        return Err(DashboardError::Validation(msg));
    }

    let now = Utc::now();
    let invitation_id =
        uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new())).to_string();
    let expires_at = now + chrono::Duration::hours(state.config.invitation_ttl_hours);

    let mut tx = state.pool.begin().await?;
    // Drop any pre-existing open invitations so only the new one is
    // usable — the admin should always share the freshest link.
    sqlx::query("DELETE FROM user_invitations WHERE user_id = ? AND consumed_at IS NULL")
        .bind(&user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO user_invitations
            (invitation_id, user_id, created_at, expires_at, consumed_at, invited_by)
         VALUES (?, ?, ?, ?, NULL, ?)",
    )
    .bind(&invitation_id)
    .bind(&user_id)
    .bind(now.to_rfc3339())
    .bind(expires_at.to_rfc3339())
    .bind(admin.sender_id())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(actor = admin.sender_id(), user = %user_id, "dashboard regenerated invitation");

    let emailed = deliver_invitation(&state, &headers, email.as_deref(), &invitation_id);
    let users = fetch_users(&state).await?;
    let ttl = state.config.invitation_ttl_hours;
    let msg = emailed.map_or_else(
        || {
            format!(
                "Fresh link for {user_id}: /dashboard/accept-invite/{invitation_id} \
                 (expires in {ttl}h)"
            )
        },
        |to| {
            format!(
                "Emailed a fresh sign-in link to {to}. Backup link \
                 (expires in {ttl}h): /dashboard/accept-invite/{invitation_id}"
            )
        },
    );
    Ok(Html(render_list(
        chrome,
        &users,
        admin.session(),
        Some(("success", &msg)),
    ))
    .into_response())
}

/// Fire-and-forget the invitation email when SMTP is configured and the
/// account has an address, returning the recipient so the caller can note
/// it in the flash. The raw accept-invite link stays the source of truth
/// (shown as a backup): a slow relay never blocks the response, and a send
/// failure only logs — the admin can still hand the link over. Returns
/// `None` (link-only) when email is off or the account has no address.
fn deliver_invitation(
    state: &DashboardState,
    headers: &HeaderMap,
    email: Option<&str>,
    invitation_id: &str,
) -> Option<String> {
    let to = email.map(str::trim).filter(|e| !e.is_empty())?;
    let cfg = crate::email::email_cfg(state);
    if !cfg.is_sendable() {
        return None;
    }
    let origin = crate::email::origin_of(cfg.public_base_url.as_deref(), headers);
    let url = format!("{origin}/dashboard/accept-invite/{invitation_id}");
    let to_owned = to.to_owned();
    let send_to = to_owned.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::email::send_invitation_email(&cfg, &send_to, &url).await {
            tracing::warn!(error = %e, "invitation email send failed");
        }
    });
    Some(to_owned)
}

fn parse_aliases(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod locale_field_tests {
    use super::parse_locale_field;

    /// The declared tag is what the admin typed, kept verbatim: this field
    /// is the *source of truth* for the language the engine writes this
    /// person's memory in, so it must not be silently normalised into
    /// something the operator did not choose.
    #[test]
    fn a_declared_tag_is_kept_verbatim() {
        assert_eq!(parse_locale_field("it"), Ok(Some("it".to_owned())));
        assert_eq!(parse_locale_field("en-GB"), Ok(Some("en-GB".to_owned())));
        // Surrounding whitespace is an input artefact, not a choice.
        assert_eq!(
            parse_locale_field("  it-IT \n"),
            Ok(Some("it-IT".to_owned()))
        );
    }

    /// Blank means "not declared", which is a real state and not an error:
    /// it drops the user to the deployment fallback. It must NOT be coerced
    /// into a default here — the fallback belongs to the engine, so that
    /// there is exactly one place that decides what "undeclared" means.
    #[test]
    fn blank_is_undeclared_not_a_default() {
        assert_eq!(parse_locale_field(""), Ok(None));
        assert_eq!(parse_locale_field("   "), Ok(None));
        // The negation that matters: blank never becomes a language.
        assert_ne!(parse_locale_field(""), Ok(Some("en".to_owned())));
    }

    /// A sentence is a common mis-fill ("English please") and would reach
    /// the prompt verbatim, so it is refused rather than stored.
    #[test]
    fn prose_and_overlong_input_are_refused() {
        assert!(parse_locale_field("English please").is_err());
        assert!(parse_locale_field(&"x".repeat(33)).is_err());
        // Just under the cap still passes — the gate is shape, not taste.
        assert!(parse_locale_field(&"x".repeat(32)).is_ok());
    }
}
