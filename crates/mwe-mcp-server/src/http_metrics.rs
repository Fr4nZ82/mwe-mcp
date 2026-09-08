// SPDX-License-Identifier: AGPL-3.0-or-later
//! `GET /metrics` — the Prometheus scrape, for the operator's own
//! monitoring.
//!
//! The numbers and their text are [`mwe_core::metrics`]; this module is
//! the address and the door.
//!
//! # Why a bearer token and not the dashboard cookie
//!
//! A scraper is a machine with no browser: it holds one credential in a
//! file and sends it on every request. So the door is
//! `Authorization: Bearer <token>`, verified by the same
//! [`mwe_core::jwt::verify`] that guards `/mcp` — the signature, the
//! expiry and the revocation list — and then required to carry
//! `is_admin`. The operator mints one from the Tokens page — a **smart**
//! token takes `is_admin` from its admin owner — or, with the server
//! stopped so the workdir lock is free, with `mwe-mcp token-issue
//! --is-admin`. Either is revoked from that same page, like any other.
//!
//! The dashboard's session cookie is not an alternative here, and not
//! only because a scraper has no cookie jar: the cookie is scoped
//! `Path=/dashboard`, so a browser would not send it to a route mounted
//! at the root — and this route is at the root because that is where a
//! scrape configuration expects `/metrics`.
//!
//! It is never public. The exposition names the models this deployment
//! calls, what they cost, which credentials talk to it and how busy it
//! is: a description of the installation, which is exactly what
//! [`/health`](crate::http_health) refuses to carry precisely so that
//! this one can.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mwe_core::jwt::{self, BlacklistCache, TokenSecret};
use mwe_core::metrics::{self, Reading};
use serde_json::json;
use sqlx::SqlitePool;
use tracing::warn;

/// The `Content-Type` of the Prometheus text exposition format.
///
/// The `version` parameter is what a scraper negotiates on; 0.0.4 is the
/// text format [`mwe_core::metrics::render`] writes.
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// What the route needs: the database and the three things that verify a
/// bearer, plus the two facts about this process that no table holds.
#[derive(Clone)]
pub struct MetricsState {
    /// Connection pool to `engine.db`. Shared with the dashboard.
    pub pool: SqlitePool,
    /// JWT secret — the same one `/mcp` and the dashboard verify with.
    pub secret: TokenSecret,
    /// Shared revocation cache, so a token revoked from the Tokens page
    /// stops scraping without a restart.
    pub blacklist: Arc<BlacklistCache>,
    /// Path of `engine.db`, for its size on disk.
    pub db_path: PathBuf,
    /// When this process started serving, for the uptime gauge.
    pub started_at: Instant,
}

/// The route, ready to be merged at the root of the HTTP tree.
pub fn router(state: MetricsState) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(state)
}

/// Answer one scrape, or refuse it.
async fn scrape(State(state): State<MetricsState>, req: Request) -> Response {
    let Some(token) = bearer(&req) else {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "missing_bearer",
            "GET /metrics needs `Authorization: Bearer <token>` carrying an admin token.",
        );
    };
    let claims = match jwt::verify(&state.secret, &token, &state.pool, &state.blacklist).await {
        Ok(claims) => claims,
        Err(error) => {
            warn!(%error, "metrics: token rejected");
            return refuse(
                StatusCode::UNAUTHORIZED,
                "invalid_token",
                "the bearer token is not valid for this server",
            );
        },
    };
    if !claims.is_admin {
        warn!(sender = %claims.sender_id, "metrics: token is not an admin's");
        return refuse(
            StatusCode::FORBIDDEN,
            "admin_required",
            "the exposition describes the whole deployment, so it needs an admin token",
        );
    }

    // The budget guard is installed beside the usage ledger before the
    // first backend is built, so `serve` always has one. Its absence is
    // a library caller's, and drops the spend families rather than
    // reporting a budget nobody set as zero.
    let spend = match mwe_core::budget::global() {
        Some(guard) => match guard.state().await {
            Ok(state) => Some(state),
            Err(error) => {
                warn!(%error, "metrics: budget state unreadable");
                return refuse(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "collect_failed",
                    "the spend figures could not be read",
                );
            },
        },
        None => None,
    };

    let reading = Reading {
        db_path: &state.db_path,
        uptime: state.started_at.elapsed(),
        version: env!("CARGO_PKG_VERSION"),
        spend: spend.as_ref(),
    };
    let families = match metrics::collect(&state.pool, reading).await {
        Ok(families) => families,
        Err(error) => {
            warn!(%error, "metrics: collection failed");
            return refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "collect_failed",
                "the metrics could not be read from the database",
            );
        },
    };

    let mut resp = metrics::render(&families).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(EXPOSITION_CONTENT_TYPE),
    );
    // Every value is read at scrape time; an intermediary answering from
    // a cache would hand the scraper a flat line that looks like a
    // healthy steady state.
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// The bearer token from `Authorization`, if there is a well-formed one.
fn bearer(req: &Request) -> Option<String> {
    let raw = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// Refuse in the shape `/mcp` refuses in — same credential, same error
/// body, so an operator debugging one recognises the other.
fn refuse(status: StatusCode, code: &str, message: &str) -> Response {
    let mut resp = (
        status,
        axum::Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response();
    if status == StatusCode::UNAUTHORIZED {
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"mwe-mcp\""),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use mwe_core::jwt::TokenClaims;
    use mwe_core::metrics::FAMILIES;
    use tower::ServiceExt as _;

    use super::*;

    /// A migrated database in a temporary workdir, with the state the
    /// route runs on. The `TempDir` is returned so the test's own scope
    /// keeps it alive.
    async fn fixture() -> (tempfile::TempDir, MetricsState) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = mwe_core::db::open_or_init(dir.path())
            .await
            .expect("db open");
        let state = MetricsState {
            pool,
            secret: TokenSecret::new(vec![7_u8; 64]).expect("secret"),
            blacklist: Arc::new(BlacklistCache::new()),
            db_path: mwe_core::db::engine_db_path(dir.path()),
            started_at: Instant::now(),
        };
        (dir, state)
    }

    /// Traffic and model calls stamped now, so the day-scoped families
    /// have samples — and so the exposition under test carries labels
    /// and a value an operator's label could have quoted.
    async fn seed(pool: &SqlitePool) {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO tool_executions
                 (timestamp, tool_name, sender_id, device_label, latency_ms)
             VALUES (?, 'wiki_ingest_message', 'alice', 'nanoclaw \"1\"', 9)",
        )
        .bind(&now)
        .execute(pool)
        .await
        .expect("audit row");
        sqlx::query(
            "INSERT INTO llm_usage
                 (ts, slot, backend, model, kind, billing, source,
                  prompt_tokens, completion_tokens, latency_ms)
             VALUES (?, 'ingest', 'ollama', 'qwen3.5:9b-q8_0', 'chat', 'local', 'serve',
                     100, 40, 250)",
        )
        .bind(&now)
        .execute(pool)
        .await
        .expect("usage row");
    }

    fn token(state: &MetricsState, is_admin: bool) -> String {
        let mut claims =
            TokenClaims::new("alice", "watchtower", "default", Duration::from_secs(60));
        claims.is_admin = is_admin;
        jwt::issue(&state.secret, &claims).expect("issue")
    }

    async fn scrape_with(state: MetricsState, auth: Option<&str>) -> (StatusCode, String, String) {
        let mut req = HttpRequest::builder().uri("/metrics");
        if let Some(value) = auth {
            req = req.header(header::AUTHORIZATION, value);
        }
        let resp = router(state)
            .oneshot(req.body(Body::empty()).expect("request"))
            .await
            .expect("response");
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .expect("body");
        (
            status,
            content_type,
            String::from_utf8(bytes.to_vec()).expect("utf-8"),
        )
    }

    #[tokio::test]
    async fn a_scrape_without_a_token_is_refused() {
        let (_dir, state) = fixture().await;

        let (status, _, body) = scrape_with(state, None).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("missing_bearer"), "{body}");
        assert!(!body.contains("mwe_"), "no exposition leaks: {body}");
    }

    /// A valid token is not enough. The exposition says which models
    /// this deployment calls, what they cost and which credentials talk
    /// to it — a description of the installation, so it takes the role
    /// that is allowed to read the installation.
    #[tokio::test]
    async fn a_valid_token_that_is_not_an_admin_is_refused() {
        let (_dir, state) = fixture().await;
        let bearer = format!("Bearer {}", token(&state, false));

        let (status, _, body) = scrape_with(state, Some(&bearer)).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("admin_required"), "{body}");
        assert!(!body.contains("mwe_"), "no exposition leaks: {body}");
    }

    #[tokio::test]
    async fn a_forged_token_is_refused() {
        let (_dir, state) = fixture().await;
        let other = MetricsState {
            secret: TokenSecret::new(vec![9_u8; 64]).expect("secret"),
            ..state.clone()
        };
        let bearer = format!("Bearer {}", token(&other, true));

        let (status, _, body) = scrape_with(state, Some(&bearer)).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("invalid_token"), "{body}");
    }

    #[tokio::test]
    async fn an_admin_token_is_served_the_exposition() {
        let (_dir, state) = fixture().await;
        seed(&state.pool).await;
        let bearer = format!("Bearer {}", token(&state, true));

        let (status, content_type, body) = scrape_with(state, Some(&bearer)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, EXPOSITION_CONTENT_TYPE);
        assert!(body.contains("mwe_uptime_seconds"), "{body}");
        // A device label an operator wrote with a quote in it stays
        // inside its own label value rather than ending the line.
        assert!(
            body.contains(r#"mwe_turns_today{device="nanoclaw \"1\""} 1"#),
            "{body}"
        );
    }

    /// The body has to be readable by a Prometheus scraper, which means
    /// the grammar and not just the words: every family declares its
    /// help and its type before its samples, and every sample line is a
    /// name, optional labels, and one numeric value.
    #[tokio::test]
    async fn the_body_parses_as_prometheus_text() {
        let (_dir, state) = fixture().await;
        seed(&state.pool).await;
        let bearer = format!("Bearer {}", token(&state, true));

        let (_, _, body) = scrape_with(state, Some(&bearer)).await;

        assert!(body.ends_with('\n'), "the document ends with a newline");
        let mut declared: Vec<&str> = Vec::new();
        let mut helped: Vec<&str> = Vec::new();
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, help) = rest
                    .split_once(' ')
                    .expect("a HELP line names and explains");
                assert!(!help.trim().is_empty(), "empty help for {name}");
                helped.push(name);
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').expect("a TYPE line names and types");
                assert!(
                    matches!(kind, "gauge" | "counter"),
                    "unknown metric type {kind:?}"
                );
                declared.push(name);
            } else {
                let (head, value) = line
                    .rsplit_once(' ')
                    .expect("a sample line ends in a value");
                value.parse::<f64>().expect("the value is a number");
                let name = head.split_once('{').map_or(head, |(n, labels)| {
                    assert!(labels.ends_with('}'), "unclosed label set: {line}");
                    n
                });
                assert!(
                    declared.contains(&name),
                    "sample for {name} before its # TYPE line"
                );
                assert!(
                    FAMILIES.contains(&name),
                    "{name} is not on the documented roster"
                );
            }
        }
        assert_eq!(declared, helped, "every family declares both headers");
        assert!(
            declared.contains(&"mwe_llm_calls_today"),
            "a labelled counter was among what parsed: {declared:?}"
        );
    }
}
