// SPDX-License-Identifier: AGPL-3.0-or-later
//! `GET /health` — the liveness answer a machine reads.
//!
//! One unauthenticated route at the root of the HTTP tree, so a reverse
//! proxy, a container orchestrator or a watchdog can poll it without
//! holding a credential:
//!
//! - `200 {"status":"ok"}` — the process answers and `engine.db` is
//!   reachable.
//! - `503 {"status":"degraded","detail":"database unreachable"}` —
//!   the process answers and the database does not, which is the one
//!   failure a liveness probe can tell apart from being dead.
//!
//! # Why the body says so little
//!
//! Anybody on the network can read it. A version tells an attacker which
//! advisories apply; a slot name, a path or a count describes the
//! deployment to somebody who has not signed in. So the body carries a
//! fixed vocabulary and nothing that varies with this installation. The
//! operator's own answers are gated instead: the Health console at
//! `/dashboard/admin/health` behind the admin login, for a person, and
//! [`/metrics`](crate::http_metrics) behind an admin bearer token, for a
//! scraper.
//!
//! Cheap and cacheless by design: one `SELECT 1`, no cached verdict, and
//! `Cache-Control: no-store` so an intermediary cannot answer for a
//! server that has since stopped.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use sqlx::SqlitePool;
use tracing::warn;

/// The route, ready to be merged at the root of the HTTP tree.
///
/// Carries its own path so the address of the liveness probe is written
/// in one place — the module that answers it.
pub fn router(pool: SqlitePool) -> Router {
    Router::new().route("/health", get(probe)).with_state(pool)
}

/// Answer the probe: can this process still reach its database?
///
/// "Reachable" is the whole question. A liveness probe that also
/// consulted the model slots, the embedder or the disk would report
/// `503` for conditions a restart cannot fix — and a restart is what the
/// watchdog reading this does with the answer.
async fn probe(State(pool): State<SqlitePool>) -> Response {
    match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&pool)
        .await
    {
        Ok(_) => answer(StatusCode::OK, json!({ "status": "ok" })),
        Err(error) => {
            warn!(%error, "health: engine.db unreachable");
            answer(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "status": "degraded", "detail": "database unreachable" }),
            )
        },
    }
}

/// Build the response, with the caching rule both arms share.
fn answer(status: StatusCode, body: serde_json::Value) -> Response {
    let mut resp = (status, axum::Json(body)).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt as _;

    use super::*;

    async fn pool() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool")
    }

    async fn get_health(pool: SqlitePool) -> (StatusCode, String) {
        let resp = router(pool)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
    }

    #[tokio::test]
    async fn a_reachable_database_answers_ok() {
        let (status, body) = get_health(pool().await).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn a_closed_database_answers_degraded() {
        let pool = pool().await;
        pool.close().await;

        let (status, body) = get_health(pool).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains(r#""status":"degraded""#), "{body}");
        assert!(body.contains("database unreachable"), "{body}");
    }

    /// The body is read by anybody on the network, so it says what state
    /// the server is in and nothing about how it is built. This is the
    /// half of the endpoint a passing status code cannot check.
    #[tokio::test]
    async fn neither_answer_names_an_internal() {
        let ok = get_health(pool().await).await.1;
        let degraded = {
            let p = pool().await;
            p.close().await;
            get_health(p).await.1
        };

        for body in [&ok, &degraded] {
            assert!(
                !body.contains(env!("CARGO_PKG_VERSION")),
                "the version is an internal: {body}"
            );
            for slot in mwe_core::config::LlmFunction::ALL {
                assert!(
                    !body.contains(slot.yaml_key()),
                    "slot `{}` named in a public body: {body}",
                    slot.yaml_key()
                );
            }
            assert!(!body.contains('/'), "a path component leaked: {body}");
            assert!(
                !body.contains("engine.db"),
                "the database file is an internal: {body}"
            );
        }
    }
}
