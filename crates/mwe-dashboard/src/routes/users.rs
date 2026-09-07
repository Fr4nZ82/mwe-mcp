// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-gated user management.
//!
//! The handlers:
//!
//! - GET  `/users`            — list of users with status (active /
//!   pending invitation) and per-row edit / forget / regenerate-invite
//!   links.
//! - GET  `/users/new`        — form to create a new regular user.
//! - POST `/users/new`        — validate, insert user row, mint a fresh
//!   `user_invitations` row, email the one-shot link when SMTP is
//!   configured, and render the page with it as a backup.
//! - GET  `/users/:id`        — edit form for `email` and `aliases`.
//! - POST `/users/:id`        — apply the edit. `is_admin` is never
//!   shown.
//! - GET  `/users/:id/export` — download everything this memory holds
//!   about the person as a tar archive ([`gdpr::export_user`]): their
//!   wiki, the facts other wikis hold about them, their uploads and
//!   their card.
//! - GET  `/users/:id/forget` — the strong-confirmation page for the
//!   erasure, with what it will destroy and what will change hands.
//! - POST `/users/:id/forget` — erase the person ([`gdpr::forget_user`]).
//!   This is the only way the dashboard removes a person: what somebody
//!   else remembers about them passes to that person, their own memory
//!   goes, and no copy is kept.
//! - POST `/users/:id/reinvite` — replace any open invitation for this
//!   user with a fresh one, email the new link when SMTP is configured,
//!   and re-render the list with it as a backup.
//! - POST `/users/:id/reset-2fa` — clear this user's two-factor
//!   enrolment so a lost authenticator does not lock them out.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use maud::{Markup, html};
use mwe_core::enrollment;
use mwe_core::gdpr;
use mwe_core::types::WikiId;
use mwe_core::wiki::{IdentityKind, create_identity_wiki};
use serde::Deserialize;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::routes::llm_config::require_memory;
use crate::routes::setup::is_plausible_email;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Sub-router for `/users/*`. Mounted inside the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/users", get(list))
        .route("/users/new", get(new_form).post(new_submit))
        .route("/users/:id", get(edit_form).post(edit_submit))
        .route("/users/:id/export", get(export_archive))
        .route("/users/:id/forget", get(forget_confirm).post(forget_apply))
        .route("/users/:id/reinvite", post(reinvite))
        .route("/users/:id/reset-2fa", post(reset_2fa))
}

/// Raw tuple from the user-list query (`user_id`, `is_admin`, `is_agent`,
/// `email`, `cred_id`, open `invitation_id`). Aliased to keep the `query_as`
/// turbofish under clippy's type-complexity bar.
type UserListRow = (
    String,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Raw tuple from the edit-form load query (`email`, `aliases` JSON,
/// `is_admin`, `require_2fa`, `timezone`, `locale`).
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
    /// A consumer agent's credential-less system user (`enrollment_users
    /// .is_agent`, the authoritative side of the diagonal identity model).
    /// Mutually exclusive with a login, and the reason the listing separates
    /// it from `user`: a bot showed up here as an ordinary user whose status
    /// happened to read "no credentials, no invitation" — true of a human
    /// mid-onboarding too, so the operator could not tell them apart.
    is_agent: bool,
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
                u.is_agent,
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
            |(user_id, is_admin, is_agent, email, cred_id, invitation_id)| UserRow {
                user_id,
                is_admin: is_admin != 0,
                is_agent: is_agent != 0,
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
                        // A consumer is an enrolled identity like any other, so
                        // its row is here — but it is a bot's memory identity,
                        // never a person, and it can never hold a login.
                        td {
                            @if u.is_admin { "admin" }
                            @else if u.is_agent { "consumer" }
                            @else { "user" }
                        }
                        td {
                            @if u.is_agent { "a consumer's own identity (no login by design)" }
                            @else if u.has_credentials { "active" }
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
                                a href=(format!("/dashboard/users/{}/forget", u.user_id))
                                  class="danger" { "forget" }
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

    let emailed = deliver_invitation(&state, Some(email), &invitation_id);
    let users = fetch_users(&state).await?;
    let ttl = state.config.invitation_ttl_hours;
    let link = format!("/dashboard/accept-invite/{invitation_id}");
    let msg = match emailed {
        Delivery::Sent(to) => format!(
            "Created {user_id} and emailed the sign-in link to {to}. Backup single-use \
             link (expires in {ttl}h): {link}"
        ),
        Delivery::LinkOnly => {
            format!("Created {user_id}. Share this single-use link (expires in {ttl}h): {link}")
        },
        Delivery::NoPublicAddress => format!(
            "Created {user_id}, but the invitation email was not sent: set the server's \
             public address in Settings first. Share this single-use link meanwhile \
             (expires in {ttl}h): {link}"
        ),
    };
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
            p.help.muted { "Lowercase letters and digits only, starting with a letter — no underscore, no hyphen. The id becomes the name of this person's memory wiki." }
            (components::text_field_ac("email", "Email", "email", &form.email, true, "off"))
            p.help.muted { "The user signs in with this email. Required, and only you (the admin) can change it later." }
            // The email is mandatory because this form makes a person who
            // signs in. A consumer has no inbox and no login, so its identity
            // is minted by the token that binds it — the standard-consumer
            // flow on the Tokens page. Said here because the founder came
            // looking for one on this page and was stopped by the email field,
            // with nothing to say where else to go.
            p.help.muted {
                "This form is for " strong { "people" } " — someone who signs in and reads "
                "their own memory. To create a " strong { "consumer" }
                " — the bot or assistant that talks to this memory — issue a "
                strong { "standard" } " consumer token on the "
                a href="/dashboard/tokens" { "Tokens" } " page instead: its "
                code { "Consumer id" } " field creates its identity and its wiki, with "
                "no email and no login."
            }
            (components::text_field("aliases", "Aliases (comma-separated)", "text", &form.aliases, false))
            (aliases_help())
            (components::text_field("timezone", "Timezone (IANA, optional)", "text", &form.timezone, false))
            (components::text_field("locale", "Language (BCP-47, e.g. en-GB or it)", "text", &form.locale, false))
            p.help.muted {
                "The language this person's memory is written in. It is not only the "
                "language a consumer answers them in: every page the engine compiles "
                "for them is written in it. Leave it blank and the deployment falls "
                "back to English."
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
    // An id somebody was erased under is spent: what the memory still holds
    // names them, and a new account under the same id would inherit it.
    if let Err(msg) = enrollment::reject_if_forgotten(&state.pool, user_id).await {
        return Ok(Some(msg));
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
    /// checked.
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
            chrome.clone(),
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
            "Forget this person and enrol them again if you really need a new id."
        }

        form action=(format!("/dashboard/users/{user_id}")) method="post" {
            p { label { "User id" } p { code { (user_id) } " · "
                @if is_admin { "admin" } @else { "user" }
            } }
            (components::text_field_ac("email", "Email", "email", &form.email, true, "off"))
            p.help.muted { "The user's sign-in email. You are the only one who can change it." }
            (components::text_field("aliases", "Aliases (comma-separated)", "text", &form.aliases, false))
            (aliases_help())
            (components::text_field("timezone", "Timezone (IANA, optional)", "text", &form.timezone, false))
            (components::text_field("locale", "Language (BCP-47, e.g. en-GB or it)", "text", &form.locale, false))
            p.help.muted {
                "The language this person's memory is written in. It is not only the "
                "language a consumer answers them in: every page the engine compiles "
                "for them is written in it. Leave it blank and the deployment falls "
                "back to English."
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

        @if !is_admin {
            h3 { "What this person can ask you for" }
            p.muted {
                "A copy of everything this memory holds about them, and its removal. "
                "Take the copy first — once they are forgotten it cannot be built."
            }
            p {
                a href=(format!("/dashboard/users/{user_id}/export")) class="primary-action" {
                    "Export everything about this person"
                }
            }
            p.help.muted {
                "A tar archive: their wiki, the facts other people's wikis hold about "
                "them with who said each one and when, the files they uploaded, and "
                "their card."
            }
            p {
                a href=(format!("/dashboard/users/{user_id}/forget")) class="danger" {
                    "Forget this person"
                }
            }
            p.help.muted {
                "Their own memory goes and no copy is kept. What other people said "
                "about them is those people's memory and stays, carrying their name "
                "as a plain external subject. The next page says exactly what happens "
                "and asks you to type the id."
            }
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

/// Admin-only download of everything this memory holds about one person, as
/// a portable tar archive (`mwe_core::gdpr::export_user`) — the portability
/// half of the two data-subject rights.
///
/// The archive carries every fragment about them in clear, so it is gated
/// exactly like the wiki export: on a deployment where
/// `instance.admin_reveal_locked` is set, the panel admin does not get it.
async fn export_archive(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(user_id): Path<String>,
) -> Result<Response> {
    let memory = require_memory(&state)?;
    if state.config.admin_reveal_locked {
        return Err(DashboardError::Forbidden);
    }
    let export = gdpr::export_user(&state.pool, &memory.tree, &user_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("export {user_id}: {e}")))?
        .ok_or(DashboardError::NotFound)?;
    tracing::info!(
        actor = admin.sender_id(),
        user = %user_id,
        wiki_entries = export.report.wiki_entries,
        facts_elsewhere = export.report.facts_elsewhere,
        media_bundled = export.report.media_bundled,
        media_missing = export.report.media_missing,
        "dashboard: personal data export served"
    );
    let filename = format!("{}-personal-data.tar", export.root_dir);
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-tar".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        export.tar_bytes,
    )
        .into_response())
}

/// Confirmation form body of `POST /dashboard/users/:id/forget`.
#[derive(Debug, Default, Deserialize)]
struct ForgetUserForm {
    /// The admin must re-type the person's id here. A mismatch is refused
    /// server-side — the same strength of confirmation a wiki delete asks
    /// for, and for a heavier act.
    #[serde(default)]
    confirm_id: String,
}

/// GET `/dashboard/users/:id/forget` — what the erasure will do, then the
/// form that asks the admin to type the id.
async fn forget_confirm(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(user_id): Path<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let is_admin: Option<i64> =
        sqlx::query_scalar("SELECT is_admin FROM enrollment_users WHERE user_id = ?")
            .bind(&user_id)
            .fetch_optional(&state.pool)
            .await?;
    // The deployment admin is refused on identity alone, so the refusal is
    // rendered before anything is counted — there is nothing to count.
    if is_admin.ok_or(DashboardError::NotFound)? != 0 {
        return Ok(Html(render_forget_confirm(
            chrome,
            admin.session(),
            &user_id,
            true,
            &gdpr::ForgetPreview::default(),
        )));
    }
    let memory = require_memory(&state)?;
    let preview = gdpr::forget_preview(&state.pool, &memory.tree, &user_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("forget preview {user_id}: {e}")))?
        .ok_or(DashboardError::NotFound)?;
    Ok(Html(render_forget_confirm(
        chrome,
        admin.session(),
        &user_id,
        false,
        &preview,
    )))
}

/// The two copies the erasure does not simply remove: the training spool,
/// which it empties whole because a spool line has no subject, and a
/// snapshot, which it cannot open at all.
///
/// Its own function because the confirmation page is long enough already,
/// and because this is the part an operator has to act on themselves.
fn beyond_this_pass(user_id: &str, preview: &gdpr::ForgetPreview) -> Markup {
    html! {
        h3 { "What this pass cannot reach" }
        p {
            strong { "A snapshot taken before today still holds everything." }
            " A backup is a sealed archive of the memory as it was, and "
            "restoring one brings " code { (user_id) } " back whole. Deciding "
            "what to do with the snapshots you already have is yours to make: "
            a href="/dashboard/admin/backup" { "open the backup console" }
            " and delete the ones from before this erasure, or keep them "
            "knowingly."
        }
        @if preview.training_spool_files > 0 {
            p {
                "The training spool is on disk and is reached: "
                strong { (preview.training_spool_files) }
                @if preview.training_spool_files == 1 { " file" } @else { " files" }
                " will be emptied. It records whole prompts, and a prompt carries "
                "the recalled memory word for word, so it holds them — with no "
                "column saying whose. That leaves no way to take out their lines "
                "and leave the rest, so all of it goes, everybody's pairs "
                "included. Running the deployment builds it again."
            }
        }
    }
}

fn render_forget_confirm(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    user_id: &str,
    is_admin: bool,
    preview: &gdpr::ForgetPreview,
) -> String {
    let title = format!("Forget — {user_id}");
    let body = html! {
        h2 { "Forget " code { (user_id) } }
        @if is_admin {
            p.flash.flash-error {
                "This is the deployment admin. Forgetting them would leave nobody who "
                "can operate the deployment, so it is refused here."
            }
            p { a href="/dashboard/users" { "Back to the list" } }
        } @else {
            p.flash.flash-error {
                strong { "Nothing is kept." }
                " Their wiki is erased where it stands — it does not go to the trash, "
                "and the 30-day window a deleted wiki gets does not apply. This pass "
                "leaves nothing to put back; the one copy it cannot reach is a "
                "snapshot, and that is at the bottom of this page."
            }

            h3 { "What is destroyed" }
            ul {
                li {
                    "Wikis erased, theirs and everything under it (the wiki a "
                    "connected consumer writes for them lives there too): "
                    strong { (preview.wikis) }
                }
                li {
                    "Facts destroyed — everything they said about themselves, plus "
                    "every behaviour rule about them: " strong { (preview.facts_destroyed) }
                    ". A rule is an instruction, not a memory: handed to somebody "
                    "else it would become an instruction about " em { "them" } "."
                }
                li {
                    "Files they uploaded: " strong { (preview.media) }
                    ". The copies on disk go too, unless somebody else uploaded the "
                    "same file."
                }
                li {
                    "Their sign-in and aliases, their group memberships, the "
                    "permissions letting a consumer speak as them, the notices waiting "
                    "for them, and the recent conversation window."
                }
                li {
                    "The id " code { (user_id) } " itself is spent. What stays behind "
                    "still names them, so handing the id to a new account would hand "
                    "that person everything the memory keeps under the name. Creating "
                    "a user under it is refused from here on; the same human coming "
                    "back gets a different id."
                }
            }

            h3 { "What stays, because it is somebody else's memory" }
            ul {
                li {
                    "Facts other people told about them: "
                    strong { (preview.facts_handed_over) } ". "
                    em { "\"" (user_id) " did a great job on the client presentation\"" }
                    " is the speaker's memory of their own working life, and it does "
                    "not go because " code { (user_id) } " leaves. Each one passes to "
                    "whoever said it, and " code { (user_id) } " stays written on it "
                    "as a plain name — an "
                    strong { "external subject" }
                    ", a name the memory holds without it being anybody's account, so "
                    "it gives nobody the right to read or change anything. A fact "
                    "filed in their wiki moves into the new owner's."
                }
                li {
                    "Facts they told about other people: "
                    strong { (preview.facts_disowned) }
                    ". They stay exactly where they are, and only the name of who "
                    "said it goes, replaced by " code { "user:_removed" }
                    " — an identity nobody can hold."
                }
                li {
                    "The record of what was done on this deployment stays; the name "
                    "of who did it is replaced the same way."
                }
            }

            (beyond_this_pass(user_id, preview))

            p.muted {
                "Take the copy first if they asked for one: "
                a href=(format!("/dashboard/users/{user_id}/export")) {
                    "download everything about " (user_id)
                }
                ". You cannot build it afterwards."
            }

            form action=(format!("/dashboard/users/{user_id}/forget")) method="post" {
                p {
                    label for="confirm-id" {
                        "Type the person's id (" code { (user_id) } ") to confirm:"
                    }
                }
                input id="confirm-id" type="text" name="confirm_id"
                    autocomplete="off" placeholder=(user_id);
                p {
                    button type="submit" class="danger" { "Forget this person" }
                    " · "
                    a href="/dashboard/users" { "Cancel" }
                }
            }
        }
    };
    layout::authenticated_reading_page(chrome, &title, session, &body)
}

/// POST `/dashboard/users/:id/forget` — erase the person.
///
/// Refuses unless `confirm_id` matches the path id exactly, then runs
/// [`gdpr::forget_user`] and re-renders the list with what it did.
async fn forget_apply(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(user_id): Path<String>,
    HtmlForm(form): HtmlForm<ForgetUserForm>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    if form.confirm_id.trim() != user_id {
        return Err(DashboardError::Validation(
            "Confirmation failed: type the exact user id to forget this person.".to_owned(),
        ));
    }
    let embedder = std::sync::Arc::clone(&memory.embedder);
    let report = gdpr::forget_user(&state.pool, &memory.tree, embedder, &user_id)
        .await
        .map_err(|e| match e {
            gdpr::GdprError::IsDeploymentAdmin(id) => DashboardError::Validation(format!(
                "Refusing to forget the deployment admin {id}: it would leave nobody \
                 who can operate this deployment."
            )),
            other => DashboardError::Internal(format!("forget {user_id}: {other}")),
        })?
        .ok_or(DashboardError::NotFound)?;

    // Act-as must die on the next call, not within the cache TTL: the
    // erasure both dismantles the consumers bound to the identity and strikes
    // the id out of every other consumer's grant list, and until the cache
    // reloads an app could still speak as somebody who is gone.
    // Best-effort: the TTL self-heals.
    if let Err(error) = state.delegations.refresh(&state.pool).await {
        tracing::warn!(%error, "delegation cache refresh failed after forget");
    }
    // The engine already logged the full tally; this line records WHO asked
    // for it, which is the half the audit trail needs and the engine has no
    // way to know.
    tracing::info!(
        actor = admin.sender_id(),
        user = %user_id,
        "dashboard: person forgotten"
    );

    let users = fetch_users(&state).await?;
    let msg = forget_summary(&report);
    Ok(Html(render_list(
        chrome,
        &users,
        admin.session(),
        Some(("success", &msg)),
    ))
    .into_response())
}

/// One sentence per category, in the order the erasure did them.
fn forget_summary(report: &gdpr::ForgetReport) -> String {
    let mut msg = format!(
        "Forgot {user}. Destroyed {destroyed} facts and erased {wikis} wikis with no copy \
         kept. {handed} facts other people told about them passed to whoever said them, \
         carrying \"{user}\" as an external subject ({moved} moved into the new owner's \
         wiki, {unplaced} freed for the cartographer to re-place). {disowned} facts they \
         told about other people stayed put with the author's name replaced. \
         {media} uploaded files removed, and their name struck out of {lists} lists that \
         granted something by naming it. {notices} notices and proposals addressed to them \
         went, along with {personal} rows of their own activity; {audit} audit entries kept \
         what happened and lost who did it.",
        user = report.user_id,
        destroyed = report.facts_tombstoned,
        wikis = report.wikis_erased,
        handed = report.facts_handed_over,
        moved = report.facts_moved,
        unplaced = report.facts_unplaced,
        disowned = report.facts_disowned,
        media = report.media_removed,
        lists = report.lists_amended + report.allow_lists_pruned,
        notices = report.notices_removed,
        personal = report.personal_rows_removed,
        audit = report.audit_rows_anonymised,
    );
    if report.captures_handed_over + report.captures_dropped > 0 {
        use std::fmt::Write as _;
        let _ = write!(
            msg,
            " Of the claims still waiting to be filed, {handed} changed hands and \
             {dropped} went.",
            handed = report.captures_handed_over,
            dropped = report.captures_dropped,
        );
    }
    if report.facts_left_as_tombstone > 0 {
        use std::fmt::Write as _;
        let _ = write!(
            msg,
            " {n} of the destroyed facts still have their sentence on a page: the row is \
             retired and out of recall, and the nightly hygiene pass takes the words off \
             the page, but those pages are worth a look.",
            n = report.facts_left_as_tombstone,
        );
    }
    if report.training_spool_files_emptied > 0 {
        use std::fmt::Write as _;
        let _ = write!(
            msg,
            " The training spool held whole prompts and therefore held them, with no \
             column saying whose: {n} spool files were emptied, everybody's pairs \
             included.",
            n = report.training_spool_files_emptied,
        );
    }
    {
        use std::fmt::Write as _;
        let _ = write!(
            msg,
            " A snapshot taken before now still holds all of it — restoring one brings \
             {user} back whole. Delete the old snapshots from the backup console, or \
             keep them knowingly.",
            user = report.user_id,
        );
    }
    if !report.orphan_smart_wikis.is_empty() {
        use std::fmt::Write as _;
        let _ = write!(
            msg,
            " Note: {n} wikis outside their own still name them as owner and were left \
             standing — nobody can read them now, and you can delete them from the wiki \
             list: {list}.",
            n = report.orphan_smart_wikis.len(),
            list = report.orphan_smart_wikis.join(", "),
        );
    }
    msg
}

async fn reinvite(
    State(state): State<DashboardState>,
    admin: AdminUser,
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
    // Mutual exclusion, consumer side (migration 0050): a consumer's identity is
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

    let emailed = deliver_invitation(&state, email.as_deref(), &invitation_id);
    let users = fetch_users(&state).await?;
    let ttl = state.config.invitation_ttl_hours;
    let link = format!("/dashboard/accept-invite/{invitation_id}");
    let msg = match emailed {
        Delivery::Sent(to) => {
            format!("Emailed a fresh sign-in link to {to}. Backup link (expires in {ttl}h): {link}")
        },
        Delivery::LinkOnly => format!("Fresh link for {user_id}: {link} (expires in {ttl}h)"),
        Delivery::NoPublicAddress => format!(
            "Fresh link for {user_id}: {link} (expires in {ttl}h). The email was not sent: \
             set the server's public address in Settings first."
        ),
    };
    Ok(Html(render_list(
        chrome,
        &users,
        admin.session(),
        Some(("success", &msg)),
    ))
    .into_response())
}

/// What became of the invitation email.
enum Delivery {
    /// On its way to this address.
    Sent(String),
    /// Nothing was sent, and nothing is wrong: no SMTP backend, or the
    /// account has no address. The link the admin hands over is the whole
    /// delivery, as it is on a deployment that never configured email.
    LinkOnly,
    /// SMTP is configured, but the server does not know the address it is
    /// reached at, so the link in the message would have to be built from
    /// the request `Host` header. The admin is told, because this one is
    /// a setting away from working.
    NoPublicAddress,
}

/// Fire-and-forget the invitation email when SMTP is configured, the
/// account has an address, and the server knows its own.
///
/// The raw accept-invite link stays the source of truth (shown as a
/// backup): a slow relay never blocks the response, and a send failure
/// only logs — the admin can still hand the link over.
fn deliver_invitation(
    state: &DashboardState,
    email: Option<&str>,
    invitation_id: &str,
) -> Delivery {
    let Some(to) = email.map(str::trim).filter(|e| !e.is_empty()) else {
        return Delivery::LinkOnly;
    };
    let cfg = crate::email::email_cfg(state);
    if !cfg.is_sendable() {
        return Delivery::LinkOnly;
    }
    let Some(origin) = crate::email::public_origin(state) else {
        tracing::warn!(
            "invitation email not sent: no `public_base_url` in mwe-mcp.config.yaml, and a link \
             built from the request `Host` header is one the requester chose"
        );
        return Delivery::NoPublicAddress;
    };
    let url = format!("{origin}/dashboard/accept-invite/{invitation_id}");
    let to_owned = to.to_owned();
    let send_to = to_owned.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::email::send_invitation_email(&cfg, &send_to, &url).await {
            tracing::warn!(error = %e, "invitation email send failed");
        }
    });
    Delivery::Sent(to_owned)
}

/// What the operator has to know before leaving the aliases field empty.
///
/// The roster a model resolving a name is shown — a conversational turn's
/// classifier and an uploaded document's extractor alike — is ids and these
/// aliases and nothing else, no surnames and no display names
/// (`mwe_core::enrollment::list_users` states the resolution contract, and
/// the match is exact). So this column is the only way to say "she is also
/// called that", and an empty one means every household name for that person
/// reaches nobody at all. The person themselves fills the first two entries,
/// with the full name and the nickname their welcome page asks for
/// (`mwe_core::enrollment::add_aliases`); this field is where those are
/// corrected, and where every other name is added.
fn aliases_help() -> Markup {
    html! {
        p.help.muted {
            "The other names this person answers to, separated by commas. The "
            "memory recognises somebody by their user id and by these names, "
            "and by nothing else — it holds no surnames, and it matches a name "
            "exactly, so a name that merely resembles one belongs to somebody "
            "else. A nickname, a full first name, whatever the household "
            "actually calls them: put it here, or that name reaches nobody — "
            "in a message, and in a document somebody uploads. The full name "
            "and the nickname somebody typed on their welcome page, at their "
            "first sign-in, are already in this list. A name written in "
            "several words counts as one name and is matched whole: half of "
            "it reaches nobody."
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn admin_session() -> crate::auth::SessionUser {
        crate::auth::SessionUser {
            sender_id: "alice".into(),
            is_admin: true,
            session_jti: "test-jti".into(),
        }
    }

    /// This form makes a **person**: someone who signs in, which is why
    /// the email is mandatory. A **consumer** has no inbox and no login —
    /// its identity is minted by the standard consumer token that binds
    /// it. Somebody who arrives here wanting one must be sent there
    /// rather than stopped by a required field with no explanation.
    #[test]
    fn the_new_user_form_says_where_a_consumer_is_created_instead() {
        let html = render_new_form(
            layout::Chrome::default(),
            &admin_session(),
            &NewUserSubmission::default(),
            None,
        );
        // The email stays required — this is not a new flag, it is a signpost.
        assert!(html.contains("name=\"email\""), "{html}");
        assert!(html.contains("The user signs in with this email. Required"));
        // And the signpost itself: what this form is for, and where the
        // other thing lives, as a link the admin can follow.
        assert!(html.contains("This form is for "), "{html}");
        assert!(html.contains("href=\"/dashboard/tokens\""), "{html}");
        assert!(html.contains("Consumer id"), "{html}");
        assert!(
            !html.contains("Bot id"),
            "the one name for the thing that connects is `consumer`: {html}"
        );
        // No second way to make one is offered here.
        assert!(
            !html.contains("is_agent") && !html.contains("name=\"agent\""),
            "a consumer is created by the token flow, not by a field on this form"
        );
    }
}
