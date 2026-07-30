// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-gated group management (see dashboard +
//! [memory model](../../../../docs/concepts/memory-model.md)).
//!
//! Groups are metadata-only — no invitation, no credentials. The form
//! shows a checkbox for each existing user so the admin builds the
//! membership list without typing free-form ids.
//!
//! Five handlers: list, new (GET+POST), edit (GET+POST), delete.

use axum::Router;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use maud::html;
use mwe_core::enrollment;
use mwe_core::types::WikiId;
use mwe_core::wiki::{IdentityKind, create_identity_wiki};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/groups", get(list))
        .route("/groups/new", get(new_form).post(new_submit))
        .route("/groups/:id", get(edit_form).post(edit_submit))
        .route("/groups/:id/delete", post(delete))
}

#[derive(Debug)]
struct GroupRow {
    group_id: String,
    members: Vec<String>,
    scope: Option<String>,
}

async fn fetch_groups(state: &DashboardState) -> Result<Vec<GroupRow>> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT group_id, members, scope
           FROM enrollment_groups
          ORDER BY group_id ASC",
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(group_id, members_json, scope)| GroupRow {
            group_id,
            members: serde_json::from_str(&members_json).unwrap_or_default(),
            scope,
        })
        .collect())
}

async fn fetch_user_ids(pool: &SqlitePool) -> Result<Vec<String>> {
    // Exclude AGENT identities from the membership roster. A consumer's bound
    // `system_user_id` (the diagonal-identity model) is the agent's own memory
    // identity, not a person you add to a group: the agent is the family's
    // assistant, not a family member. Letting it into a group would also be a
    // category error downstream — its `author=assistant` turns would carry the
    // group in `sender_groups` and the classifier could route its derived facts
    // to the group. The consumer binding is the authoritative "is_agent" source
    // (mirrored self-describingly by the wiki `is_agent` `_meta` marker). This
    // roster also gates `validate_members`, so a hand-POSTed agent id is rejected
    // too, not just hidden from the checkboxes.
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT user_id FROM enrollment_users
          WHERE user_id NOT IN (
                  SELECT system_user_id FROM consumers WHERE system_user_id IS NOT NULL
                )
          ORDER BY user_id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

async fn list(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let groups = fetch_groups(&state).await?;
    Ok(Html(render_list(chrome, &groups, admin.session(), None)))
}

fn render_list(
    chrome: layout::Chrome,
    groups: &[GroupRow],
    session: &crate::auth::SessionUser,
    flash_msg: Option<(&str, &str)>,
) -> String {
    let body = html! {
        @if let Some((kind, msg)) = flash_msg {
            (components::flash(kind, msg))
        }

        p { a href="/dashboard/groups/new" class="primary-action" { "+ Add group" } }

        table {
            thead { tr {
                th { "Group id" }
                th { "Members" }
                th { "Scope" }
                th { "Actions" }
            } }
            tbody {
                @for g in groups {
                    @let is_global = enrollment::is_global_group(&g.group_id);
                    tr {
                        td { (g.group_id) @if is_global { " " em.muted { "(builtin)" } } }
                        td {
                            @if is_global { em.muted { "everyone" } }
                            @else { (g.members.join(", ")) }
                        }
                        td { (g.scope.clone().unwrap_or_default()) }
                        td {
                            a href=(format!("/dashboard/groups/{}", g.group_id)) { "edit" }
                            @if !is_global {
                                " · "
                                form action=(format!("/dashboard/groups/{}/delete", g.group_id))
                                     method="post" class="inline-form"
                                     onsubmit=(format!(
                                        "return confirm('Delete group {}?')",
                                        g.group_id
                                     )) {
                                    button type="submit" class="link-button danger" { "delete" }
                                }
                            }
                        }
                    }
                }
                @if groups.is_empty() {
                    tr { td colspan="4" class="muted" { "No groups yet." } }
                }
            }
        }
    };
    layout::authenticated_page(chrome, "Groups", session, &body)
}

#[derive(Debug, Default)]
struct GroupForm {
    group_id: String,
    members: Vec<String>,
    scope: String,
}

async fn new_form(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let users = fetch_user_ids(&state.pool).await?;
    Ok(Html(render_form(
        chrome,
        admin.session(),
        &FormMode::New,
        &GroupForm::default(),
        &users,
        None,
    )))
}

/// Body of POST to either `/groups/new` or `/groups/:id`.
///
/// `members[]` arrives via repeated `members=<id>` pairs from the
/// checkbox group; serde decodes that into a `Vec<String>` thanks to
/// `serde_urlencoded`'s default sequence handling.
#[derive(Debug, Deserialize)]
pub struct GroupSubmission {
    pub group_id: Option<String>,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub scope: String,
}

async fn new_submit(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(submission): HtmlForm<GroupSubmission>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let group_id = submission.group_id.as_deref().unwrap_or("").trim();
    let users = fetch_user_ids(&state.pool).await?;

    let form = GroupForm {
        group_id: group_id.to_owned(),
        members: submission.members.clone(),
        scope: submission.scope.clone(),
    };

    if let Some(msg) = validate_group_id_for_create(&state, group_id).await? {
        return Ok(Html(render_form(
            chrome,
            admin.session(),
            &FormMode::New,
            &form,
            &users,
            Some(&msg),
        ))
        .into_response());
    }
    if let Some(msg) = validate_members(&submission.members, &users) {
        return Ok(Html(render_form(
            chrome,
            admin.session(),
            &FormMode::New,
            &form,
            &users,
            Some(&msg),
        ))
        .into_response());
    }

    let members_json = serde_json::to_string(&submission.members)
        .map_err(|e| DashboardError::Internal(format!("encoding members: {e}")))?;
    sqlx::query(
        "INSERT INTO enrollment_groups (group_id, members, scope)
         VALUES (?, ?, ?)",
    )
    .bind(group_id)
    .bind(&members_json)
    .bind(trimmed_or_none(&submission.scope))
    .execute(&state.pool)
    .await?;

    tracing::info!(
        actor = admin.sender_id(),
        group = group_id,
        "dashboard created group"
    );

    // Materialize the group's shared identity wiki right away,
    // post-commit. Same defensive pattern as users::new_submit.
    if let Some(memory) = state.memory.as_ref() {
        match WikiId::parse(group_id) {
            Ok(wiki_id) => {
                if let Err(e) =
                    create_identity_wiki(&memory.tree, &wiki_id, group_id, IdentityKind::Group)
                {
                    tracing::error!(
                        group = group_id,
                        error = %e,
                        "identity wiki creation failed for new group; DB row is up, run mwe-mcp doctor"
                    );
                }
            },
            Err(e) => {
                tracing::error!(
                    group = group_id,
                    error = %e,
                    "group id failed WikiId::parse; identity wiki not created"
                );
            },
        }
    }

    let groups = fetch_groups(&state).await?;
    let msg = format!("Created group {group_id}.");
    Ok(Html(render_list(
        chrome,
        &groups,
        admin.session(),
        Some(("success", &msg)),
    ))
    .into_response())
}

async fn edit_form(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(group_id): Path<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT members, scope FROM enrollment_groups WHERE group_id = ?")
            .bind(&group_id)
            .fetch_optional(&state.pool)
            .await?;
    let (members_json, scope) = row.ok_or(DashboardError::NotFound)?;
    let members: Vec<String> = serde_json::from_str(&members_json).unwrap_or_default();

    let users = fetch_user_ids(&state.pool).await?;
    let form = GroupForm {
        group_id: group_id.clone(),
        members,
        scope: scope.unwrap_or_default(),
    };
    Ok(Html(render_form(
        chrome,
        admin.session(),
        &FormMode::Edit(&group_id),
        &form,
        &users,
        None,
    )))
}

async fn edit_submit(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(group_id): Path<String>,
    HtmlForm(submission): HtmlForm<GroupSubmission>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let exists: i64 =
        sqlx::query_scalar("SELECT count(*) FROM enrollment_groups WHERE group_id = ?")
            .bind(&group_id)
            .fetch_one(&state.pool)
            .await?;
    if exists == 0 {
        return Err(DashboardError::NotFound);
    }

    let users = fetch_user_ids(&state.pool).await?;
    let form = GroupForm {
        group_id: group_id.clone(),
        members: submission.members.clone(),
        scope: submission.scope.clone(),
    };

    // The builtin `global` group has universal membership: ignore any
    // submitted member list, keep it empty, and only its scope is editable.
    let is_global = enrollment::is_global_group(&group_id);
    if !is_global && let Some(msg) = validate_members(&submission.members, &users) {
        return Ok(Html(render_form(
            chrome,
            admin.session(),
            &FormMode::Edit(&group_id),
            &form,
            &users,
            Some(&msg),
        ))
        .into_response());
    }

    let members_json = if is_global {
        "[]".to_owned()
    } else {
        serde_json::to_string(&submission.members)
            .map_err(|e| DashboardError::Internal(format!("encoding members: {e}")))?
    };
    sqlx::query(
        "UPDATE enrollment_groups
            SET members = ?, scope = ?
          WHERE group_id = ?",
    )
    .bind(&members_json)
    .bind(trimmed_or_none(&submission.scope))
    .bind(&group_id)
    .execute(&state.pool)
    .await?;

    tracing::info!(actor = admin.sender_id(), group = %group_id, "dashboard edited group");

    let groups = fetch_groups(&state).await?;
    let msg = format!("Updated group {group_id}.");
    Ok(Html(render_list(
        chrome,
        &groups,
        admin.session(),
        Some(("success", &msg)),
    ))
    .into_response())
}

async fn delete(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(group_id): Path<String>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    // The builtin universal group is not deletable — it is the public/world
    // ACL principal, not an operator-managed collaborative group.
    if enrollment::is_global_group(&group_id) {
        let groups = fetch_groups(&state).await?;
        return Ok(Html(render_list(
            chrome,
            &groups,
            admin.session(),
            Some(("error", "The builtin “global” group cannot be deleted.")),
        ))
        .into_response());
    }
    let n = sqlx::query("DELETE FROM enrollment_groups WHERE group_id = ?")
        .bind(&group_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(DashboardError::NotFound);
    }

    // 23d: reassign any fact whose `sender` is this group (e.g. one a prior 23d
    // pass auto-attributed to the collective) to its wiki's scope principal, so
    // no active fact points at a vanished sender (the sender-scrub invariant —
    // docs/concepts/identity-and-acl.md). Owner/allow that
    // name this group are out of 23d's scope (sender only). Best-effort — a
    // failure (or absent memory handles) is logged, never blocks the delete.
    if let Some(memory) = state.memory.as_ref() {
        let gone = mwe_core::types::Principal::Group(group_id.clone());
        match mwe_core::fact_index::reassign_sender_to_scope(&state.pool, &memory.tree, &gone).await
        {
            Ok(n) => {
                tracing::info!(group = %group_id, reassigned = n, "23d: group delete reassigned facts' sender to wiki scope");
            },
            Err(e) => {
                tracing::warn!(group = %group_id, error = %e, "23d: sender reassignment failed after group delete");
            },
        }
    } else {
        tracing::warn!(group = %group_id, "23d: memory handles unavailable — facts' sender not reassigned");
    }

    tracing::info!(actor = admin.sender_id(), group = %group_id, "dashboard deleted group");
    Ok(Redirect::to("/dashboard/groups").into_response())
}

enum FormMode<'a> {
    New,
    Edit(&'a str),
}

fn render_form(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    mode: &FormMode<'_>,
    form: &GroupForm,
    users: &[String],
    error: Option<&str>,
) -> String {
    let (title, action) = match *mode {
        FormMode::New => ("Add group".to_owned(), "/dashboard/groups/new".to_owned()),
        FormMode::Edit(id) => (
            format!("Edit group — {id}"),
            format!("/dashboard/groups/{id}"),
        ),
    };
    // The builtin `global` group has universal membership — only its scope
    // is editable; the member checkboxes are hidden.
    let editing_global = matches!(*mode, FormMode::Edit(id) if enrollment::is_global_group(id));

    let body = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }

        form action=(action) method="post" {
            @match *mode {
                FormMode::New => {
                    (components::text_field("group_id", "Group id", "text", &form.group_id, true))
                    p.help.muted { "Lowercase letters, digits, underscore only — no `°`. Must start with a letter." }
                }
                FormMode::Edit(id) => {
                    p { label { "Group id" } p { code { (id) } } }
                }
            }
            (components::text_area("scope", "Scope (prose hint for the ingest classifier)", &form.scope, false))

            @if editing_global {
                p.muted { "Everyone is a member of the builtin “global” group — only its scope is editable." }
            } @else {
                p {
                    label { "Members" }
                    @if users.is_empty() {
                        p.muted { "No users yet — add some users first." }
                    } @else {
                        fieldset.members-list {
                            @for user in users {
                                label.member-checkbox {
                                    input
                                        type="checkbox"
                                        name="members"
                                        value=(user)
                                        checked[form.members.iter().any(|m| m == user)];
                                    " " (user)
                                }
                            }
                        }
                    }
                }
            }

            (components::submit("Save"))
        }

        p { a href="/dashboard/groups" { "Back to the list" } }
    };
    layout::authenticated_reading_page(chrome, &title, session, &body)
}

async fn validate_group_id_for_create(
    state: &DashboardState,
    group_id: &str,
) -> Result<Option<String>> {
    if group_id.is_empty() {
        return Ok(Some("Choose a group id.".into()));
    }
    if !enrollment::is_valid_group_id(group_id) {
        return Ok(Some(
            "Id must match /^[a-z][a-z0-9]*$/ — no underscores (the id becomes the identity \
             wiki's id), no `°` (reserved for user collision suffixes)."
                .into(),
        ));
    }
    if enrollment::is_global_group(group_id) {
        return Ok(Some(
            "“global” is the builtin universal group — it already exists and cannot be recreated."
                .into(),
        ));
    }
    if enrollment::is_guest(group_id) {
        return Ok(Some(
            "“guest” is the builtin unidentified-human pseudo-identity and cannot be a group."
                .into(),
        ));
    }
    if !enrollment::is_filesystem_safe(group_id) {
        return Ok(Some(
            "Id is not filesystem-safe (slashes, '..', whitespace).".into(),
        ));
    }
    let group_collision: i64 =
        sqlx::query_scalar("SELECT count(*) FROM enrollment_groups WHERE group_id = ?")
            .bind(group_id)
            .fetch_one(&state.pool)
            .await?;
    if group_collision > 0 {
        return Ok(Some(format!("Group id {group_id:?} already exists.")));
    }
    let user_collision: i64 =
        sqlx::query_scalar("SELECT count(*) FROM enrollment_users WHERE user_id = ?")
            .bind(group_id)
            .fetch_one(&state.pool)
            .await?;
    if user_collision > 0 {
        return Ok(Some(format!(
            "Id {group_id:?} clashes with an existing user — pick another id."
        )));
    }
    Ok(None)
}

fn validate_members(members: &[String], existing_users: &[String]) -> Option<String> {
    for m in members {
        if !existing_users.iter().any(|u| u == m) {
            return Some(format!("Unknown user id in members: {m:?}."));
        }
    }
    None
}

fn trimmed_or_none(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mwe_core::consumers::{self, RegisterRequest};
    use mwe_core::db;

    /// A pool with two humans and one agent identity (a bound consumer
    /// system-user), to exercise the group-roster agent filter.
    async fn seeded_pool() -> (SqlitePool, tempfile::TempDir) {
        // The guard goes back to the caller: a leaked temporary
        // directory is never removed by anything (see the leak that
        // filled tmpfs on the production host).
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db open");
        for u in ["franz", "morgana", "hermesbot"] {
            sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES (?, 0)")
                .bind(u)
                .execute(&pool)
                .await
                .expect("seed user");
        }
        // Bind `hermesbot` as a consumer's system-user → it is an agent identity,
        // not a person (the diagonal-identity model).
        consumers::register(
            &pool,
            &RegisterRequest {
                consumer_id: "hermesdeploy",
                display_name: None,
                callback_url: None,
                kinds_subscribed: None,
                metadata: None,
                system_user_id: Some("hermesbot"),
            },
        )
        .await
        .expect("register agent consumer");
        (pool, dir)
    }

    #[tokio::test]
    async fn group_roster_excludes_agent_identities() {
        let (pool, _workdir) = seeded_pool().await;
        let roster = fetch_user_ids(&pool).await.expect("roster");

        assert!(
            roster.contains(&"franz".to_owned()),
            "humans stay in the roster"
        );
        assert!(roster.contains(&"morgana".to_owned()));
        assert!(
            !roster.contains(&"hermesbot".to_owned()),
            "the agent's bound system-user must never appear in the group membership roster"
        );

        // Defense in depth: the roster also gates validation, so a hand-POSTed
        // agent id is rejected, not merely hidden from the checkboxes.
        assert!(
            validate_members(&["hermesbot".to_owned()], &roster).is_some(),
            "validate_members rejects the agent id (not in the roster)"
        );
        assert!(validate_members(&["franz".to_owned()], &roster).is_none());
    }
}
