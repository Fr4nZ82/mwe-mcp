// SPDX-License-Identifier: AGPL-3.0-or-later
//! Skill catalog viewer.
//!
//! Two read-only routes:
//!
//! - `/dashboard/skills`        — list the bundled skills (via
//!   `mwe_core::skills::list_bundled`).
//! - `/dashboard/skills/:name`  — render the full markdown body of one
//!   bundled skill (via `mwe_core::skills::fetch`).
//!
//! The bundled skills mirror the same files the MCP `skill_list` /
//! `skill_fetch` tools surface to consumers — operators can confirm
//! they're seeing the same artefacts the agents see.

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::response::Html;
use axum::routing::get;
use maud::{Markup, html};
use mwe_core::skills::{self, Skill};

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;
use crate::ui::layout;

pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/skills", get(list))
        .route("/skills/:name", get(view))
}

async fn list(State(_state): State<DashboardState>, user: SessionUser) -> Result<Html<String>> {
    let bundled = skills::list_bundled()
        .map_err(|e| DashboardError::Internal(format!("list skills: {e}")))?;

    let body = html! {
        p.muted {
            "Read-only catalog of the skills your consumers can fetch via "
            code { "skill_list" } " / " code { "skill_fetch" }
            " (MCP) or the HTTP " code { "/skills" } " endpoint. "
            "Bundled skills ship with the mwe-mcp binary."
        }

        h2 { "Bundled (" (bundled.len()) ")" }
        @if bundled.is_empty() {
            p.muted { "No bundled skills — unexpected; check the build." }
        } @else {
            (render_skill_table(&bundled))
        }
    };

    Ok(Html(layout::authenticated_page("Skills", &user, &body)))
}

fn render_skill_table(entries: &[Skill]) -> Markup {
    html! {
        table {
            thead { tr {
                th { "Name" }
                th { "Version" }
                th { "Description" }
                th { "ETag" }
            } }
            tbody {
                @for s in entries {
                    (render_skill_row(s))
                }
            }
        }
    }
}

fn render_skill_row(s: &Skill) -> Markup {
    html! {
        tr {
            td {
                a href=(format!("/dashboard/skills/{}", s.name)) {
                    code { (s.name) }
                }
            }
            td { code { (s.version) } }
            td { (s.description) }
            td.muted { code { (s.etag) } }
        }
    }
}

async fn view(
    State(_state): State<DashboardState>,
    user: SessionUser,
    AxumPath(name): AxumPath<String>,
) -> Result<Html<String>> {
    let (skill, body_md) = match skills::fetch(&name) {
        Ok(hit) => hit,
        Err(skills::SkillError::NotFound(_)) => return Err(DashboardError::NotFound),
        Err(e) => return Err(DashboardError::Internal(format!("fetch skill: {e}"))),
    };

    let body = html! {
        section.meta {
            dl {
                dt { "name" }        dd { code { (skill.name) } }
                dt { "version" }     dd { code { (skill.version) } }
                dt { "description" } dd { (skill.description) }
                dt { "source" }      dd { "bundled" }
                dt { "etag" }        dd { code { (skill.etag) } }
            }
        }

        h2 { "Body" }
        p.muted {
            "Verbatim markdown — what " code { "skill_fetch" }
            " returns to consumers. Frontmatter included."
        }
        pre.wiki-page { (body_md) }

        p { a href="/dashboard/skills" { "← Back to catalog" } }
    };

    let title = format!("Skill — {}", skill.name);
    Ok(Html(layout::authenticated_page(&title, &user, &body)))
}
