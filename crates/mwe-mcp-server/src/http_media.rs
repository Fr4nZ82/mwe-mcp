// SPDX-License-Identifier: AGPL-3.0-or-later
//! HTTP byte pair for the media pipeline — `POST /media` (multipart
//! upload) and `GET /media/:catalog_id` (ACL-enforced serving).
//!
//! Both routes sit behind the same bearer-JWT middleware as `/mcp`
//! ([`crate::mcp::auth::jwt_auth_middleware`]), so `X-MWE-Act-As` is
//! resolved before the handlers run and `IdentityProfile.sender_id` is
//! already the **effective** principal. The MCP ingest stays JSON — bytes
//! travel out of band through this pair, then ride
//! `wiki_ingest_message.attachments` as catalog ids (see
//! media pipeline).
//!
//! - `POST /media` — multipart fields: `file` (required; bytes + optional
//!   filename + content type), `kind` (required; `photo` / `video` /
//!   `audio` / `doc`), `caption` / `description` (optional). Stores the
//!   bytes content-addressed, dedups per (hash, owner), mints the
//!   `catalog_id`, stamps the provisional ACL (owner = the effective
//!   principal) and returns `{ catalog_id, kind, mime, sha256,
//!   size_bytes, dedup }` — `201` fresh, `200` deduplicated.
//! - `GET /media/:catalog_id` — loads the catalog row, evaluates the
//!   per-media ACL with `acl::can_read` semantics (no admin bypass) and
//!   serves the blob. A marker may be visible on a page while the bytes
//!   are denied here — denial is `403 media_forbidden` by design, not a
//!   404 masquerade: the embed key already disclosed existence.
//!
//! Error bodies use the same `{ "error": { "code", "message" } }` JSON
//! shape as the auth middleware.

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use mwe_core::media::{self, NewMedia};
use mwe_core::types::CatalogId;
use serde_json::json;
use tracing::warn;

use crate::mcp::state::{IdentityProfile, McpState};

/// Hard cap on one upload body. Telegram bots cap file downloads at
/// 20 MiB; this leaves headroom for other hosts without inviting abuse.
pub const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

use mwe_core::media::inline_safe;

/// Build the `/media` sub-router (bearer-JWT gated).
pub fn router(state: McpState) -> Router {
    Router::new()
        .route("/", post(upload_media))
        .route("/:catalog_id", get(fetch_media))
        .route_layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::mcp::auth::jwt_auth_middleware,
        ))
        .with_state(state)
}

/// The fields one upload body may carry, collected off the multipart
/// stream. Unknown fields are ignored for forward compatibility.
#[derive(Default)]
struct UploadFields {
    bytes: Option<Vec<u8>>,
    original_filename: Option<String>,
    mime: String,
    kind: String,
    caption: Option<String>,
    description: Option<String>,
}

async fn collect_upload_fields(mut multipart: Multipart) -> Result<UploadFields, Response> {
    let mut out = UploadFields::default();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "malformed_multipart",
                    &format!("could not read multipart body: {e}"),
                ));
            },
        };
        match field.name().unwrap_or("") {
            "file" => {
                out.original_filename = field.file_name().map(ToOwned::to_owned);
                out.mime = field.content_type().unwrap_or("").to_owned();
                match field.bytes().await {
                    Ok(b) => out.bytes = Some(b.to_vec()),
                    Err(e) => {
                        // axum surfaces the body-limit overrun as a field
                        // read error; classify it for the client.
                        return Err(error_response(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "upload_too_large_or_truncated",
                            &format!("could not read file field: {e}"),
                        ));
                    },
                }
            },
            "kind" => out.kind = read_text_field(field).await.unwrap_or_default(),
            "caption" => out.caption = read_text_field(field).await,
            "description" => out.description = read_text_field(field).await,
            _ => {},
        }
    }
    Ok(out)
}

async fn upload_media(
    State(state): State<McpState>,
    axum::Extension(profile): axum::Extension<IdentityProfile>,
    multipart: Multipart,
) -> Response {
    // Guest turns are ephemeral (roadmap 40): an upload is a permanent
    // catalog row + blob, so the builtin guest pseudo-identity gets none.
    if mwe_core::enrollment::is_guest(&profile.sender_id) {
        return error_response(
            StatusCode::FORBIDDEN,
            "guest_cannot_upload",
            "media upload is not available to the builtin `guest` pseudo-identity — guest \
             turns are ephemeral and leave no permanent state",
        );
    }
    let fields = match collect_upload_fields(multipart).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let Some(bytes) = fields.bytes else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "missing_file_field",
            "multipart body must carry a `file` field",
        );
    };
    if fields.kind.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "missing_kind_field",
            "multipart body must carry a `kind` field (photo | video | audio | doc)",
        );
    }

    let owner = match format!("user:{}", profile.sender_id).parse() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, sender = %profile.sender_id, "media upload: unusable principal");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "principal_resolution_failed",
                "the authenticated sender does not form a valid principal",
            );
        },
    };

    let new_media = NewMedia {
        bytes,
        kind: fields.kind,
        mime: fields.mime,
        owner,
        uploaded_by_consumer: profile.consumer_id.clone(),
        caption: fields.caption,
        description: fields.description,
        original_filename: fields.original_filename,
    };

    match media::store_media(&state.pool, &state.workdir, new_media).await {
        Ok(outcome) => {
            let status = if outcome.deduplicated {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            (
                status,
                [(header::CONTENT_TYPE, "application/json")],
                json!({
                    "catalog_id": outcome.row.catalog_id.as_str(),
                    "kind": outcome.row.kind,
                    "mime": outcome.row.mime,
                    "sha256": outcome.row.sha256,
                    "size_bytes": outcome.row.size_bytes,
                    "dedup": outcome.deduplicated,
                })
                .to_string(),
            )
                .into_response()
        },
        Err(media::MediaError::InvalidKind(k)) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_kind",
            &format!("kind {k:?} is not one of photo | video | audio | doc"),
        ),
        Err(media::MediaError::EmptyPayload) => error_response(
            StatusCode::BAD_REQUEST,
            "empty_payload",
            "the `file` field carried zero bytes",
        ),
        Err(e) => {
            warn!(error = %e, "media upload failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "media_store_failed",
                "the server could not store the media item",
            )
        },
    }
}

async fn read_text_field(field: axum::extract::multipart::Field<'_>) -> Option<String> {
    let text = field.text().await.ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

async fn fetch_media(
    State(state): State<McpState>,
    axum::Extension(profile): axum::Extension<IdentityProfile>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(catalog_id) = CatalogId::parse(&raw_id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_catalog_id",
            "expected c-YYYY-MM-DD-<kind>-NNN.<ext>",
        );
    };

    let groups = match mwe_core::enrollment::groups_for(&state.pool, &profile.sender_id).await {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "media fetch: groups_for failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "acl_resolution_failed",
                "could not resolve the caller's groups",
            );
        },
    };

    serve_media_bytes(&state, &catalog_id, &profile.sender_id, &groups, &headers).await
}

/// The shared serving core: catalog lookup → `can_read` → bytes. The
/// dashboard's session-cookie alias calls the same sequence so the two
/// mounts cannot drift.
pub async fn serve_media_bytes(
    state: &McpState,
    catalog_id: &CatalogId,
    sender_id: &str,
    sender_groups: &[String],
    request_headers: &HeaderMap,
) -> Response {
    let row = match media::find_by_id(&state.pool, catalog_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "unknown_media",
                "no catalog row for this catalog_id",
            );
        },
        Err(e) => {
            warn!(error = %e, "media fetch: catalog lookup failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "media_lookup_failed",
                "the server could not load the catalog row",
            );
        },
    };

    if !media::row_visible_to(&row, sender_id, sender_groups) {
        return error_response(
            StatusCode::FORBIDDEN,
            "media_forbidden",
            "the caller is not in this media item's read set",
        );
    }

    // Content-addressed bytes are immutable, so the hash is a perfect
    // strong ETag — a revalidating client never re-downloads.
    let etag = format!("\"{}\"", row.sha256);
    if let Some(if_none_match) = request_headers.get(header::IF_NONE_MATCH)
        && if_none_match.to_str().ok() == Some(etag.as_str())
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let blob_abs = media::blob_path(&state.workdir, &row.sha256);
    let file = match tokio::fs::File::open(&blob_abs).await {
        Ok(f) => f,
        Err(e) => {
            // A row without its blob breaks the blob-first write
            // invariant — surface loudly, never 404 (the row exists).
            warn!(
                catalog_id = %catalog_id,
                error = %e,
                "media fetch: catalog row present but blob missing"
            );
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "media_blob_missing",
                "the catalog row exists but its bytes are missing from the store",
            );
        },
    };

    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = Body::from_stream(stream).into_response();
    let headers = resp.headers_mut();
    let (content_type, disposition) = if inline_safe(&row.mime) {
        (
            row.mime.clone(),
            format!("inline; filename=\"{catalog_id}\""),
        )
    } else {
        (
            "application/octet-stream".to_owned(),
            format!("attachment; filename=\"{catalog_id}\""),
        )
    };
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).unwrap_or(HeaderValue::from_static("attachment")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&row.size_bytes.to_string()).unwrap_or(HeaderValue::from_static("0")),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"unknown\"")),
    );
    // Private: the read set is per-principal; immutable: content-addressed.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    resp
}

fn error_response(status: StatusCode, code: &'static str, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        json!({ "error": { "code": code, "message": message } }).to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::Request;
    use mwe_core::config::LlmConfig;
    use mwe_core::db;
    use mwe_core::delegations::{self, DelegationCache};
    use mwe_core::embedder::FakeEmbedder;
    use mwe_core::jwt::{self, BlacklistCache, TokenClaims, TokenSecret};
    use mwe_core::wiki::WikiTree;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    async fn build_state() -> (McpState, TokenSecret, sqlx::SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        let secret = TokenSecret::new(vec![0xABu8; 32]).expect("secret");
        let state = McpState {
            pool: pool.clone(),
            tree,
            embedder: Arc::new(FakeEmbedder::new("fake", 4))
                as Arc<dyn mwe_core::embedder::Embedder>,
            secret: secret.clone(),
            blacklist: Arc::new(BlacklistCache::new()),
            delegations: Arc::new(DelegationCache::new()),
            llm_config: LlmConfig::default(),
            recall: Arc::new(std::sync::RwLock::new(
                mwe_core::config::RecallConfig::default(),
            )),
            workdir: dir.path().to_path_buf(),
            document_policy: mwe_core::document::DocumentPolicy::default(),
            reindex_tx: None,
        };
        (state, secret, pool, dir)
    }

    fn user_token(secret: &TokenSecret, sender: &str) -> String {
        let claims = TokenClaims::new(sender, "test", "default", Duration::from_secs(60));
        jwt::issue(secret, &claims).unwrap()
    }

    fn consumer_token(secret: &TokenSecret, sender: &str, consumer: &str) -> String {
        let mut claims = TokenClaims::new(sender, "test-bot", "default", Duration::from_secs(60));
        claims.consumer_id = Some(consumer.to_owned());
        jwt::issue(secret, &claims).unwrap()
    }

    const BOUNDARY: &str = "mwe-test-boundary";

    fn multipart_body(kind: &str, caption: Option<&str>, bytes: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut push_text = |name: &str, value: &str| {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        };
        push_text("kind", kind);
        if let Some(c) = caption {
            push_text("caption", c);
        }
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
                 filename=\"photo.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    fn upload_request(token: &str, body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap()
    }

    fn fetch_request(token: &str, catalog_id: &str) -> Request<Body> {
        Request::builder()
            .uri(format!("/{catalog_id}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn upload_then_fetch_round_trips_bytes_with_etag() {
        let (state, secret, _pool, _dir) = build_state().await;
        let token = user_token(&secret, "frodo");
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(upload_request(
                &token,
                multipart_body("photo", Some("at the gate"), b"jpegbytes"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let parsed = body_json(resp).await;
        let catalog_id = parsed["catalog_id"].as_str().unwrap().to_owned();
        assert!(catalog_id.contains("-photo-001.jpg"), "{catalog_id}");
        assert_eq!(parsed["dedup"], false);

        let resp = app
            .clone()
            .oneshot(fetch_request(&token, &catalog_id))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/jpeg"
        );
        let etag = resp.headers().get(header::ETAG).unwrap().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"jpegbytes");

        // Conditional re-fetch → 304.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/{catalog_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn second_upload_same_bytes_dedups_to_same_id() {
        let (state, secret, _pool, _dir) = build_state().await;
        let token = user_token(&secret, "frodo");
        let app = router(state);

        let first = body_json(
            app.clone()
                .oneshot(upload_request(&token, multipart_body("photo", None, b"x")))
                .await
                .unwrap(),
        )
        .await;
        let resp = app
            .oneshot(upload_request(&token, multipart_body("photo", None, b"x")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let second = body_json(resp).await;
        assert_eq!(first["catalog_id"], second["catalog_id"]);
        assert_eq!(second["dedup"], true);
    }

    #[tokio::test]
    async fn fetch_denied_for_principal_outside_read_set() {
        let (state, secret, _pool, _dir) = build_state().await;
        let frodo = user_token(&secret, "frodo");
        let saruman = user_token(&secret, "saruman");
        let app = router(state);

        let uploaded = body_json(
            app.clone()
                .oneshot(upload_request(&frodo, multipart_body("photo", None, b"x")))
                .await
                .unwrap(),
        )
        .await;
        let catalog_id = uploaded["catalog_id"].as_str().unwrap();

        let resp = app
            .oneshot(fetch_request(&saruman, catalog_id))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let parsed = body_json(resp).await;
        assert_eq!(parsed["error"]["code"], "media_forbidden");
    }

    #[tokio::test]
    async fn act_as_upload_stamps_the_effective_principal_as_owner() {
        let (state, secret, pool, _dir) = build_state().await;
        delegations::upsert(&pool, "sam-bot", &["frodo".to_owned()], "admin")
            .await
            .expect("delegation");
        let bot = consumer_token(&secret, "sambot", "sam-bot");
        let frodo = user_token(&secret, "frodo");
        let app = router(state);

        let resp = app
            .clone()
            .oneshot({
                let mut req =
                    upload_request(&bot, multipart_body("photo", Some("frodo's photo"), b"y"));
                req.headers_mut().insert(
                    &crate::mcp::auth::ACT_AS_HEADER,
                    HeaderValue::from_static("frodo"),
                );
                req
            })
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let uploaded = body_json(resp).await;
        let catalog_id = uploaded["catalog_id"].as_str().unwrap();

        // The served human owns the bytes and reads them back directly.
        let resp = app
            .oneshot(fetch_request(&frodo, catalog_id))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// A delegated `guest` act-as passes the auth middleware (the grant is
    /// the enable switch, roadmap 40) but the upload handler refuses it:
    /// guest turns are ephemeral, and a catalog row + blob is permanent.
    #[tokio::test]
    async fn delegated_guest_act_as_reaches_upload_and_is_refused() {
        let (state, secret, pool, _dir) = build_state().await;
        delegations::upsert(&pool, "sam-bot", &["guest".to_owned()], "admin")
            .await
            .expect("delegation");
        let bot = consumer_token(&secret, "sambot", "sam-bot");
        let app = router(state);

        let resp = app
            .oneshot({
                let mut req = upload_request(&bot, multipart_body("photo", None, b"z"));
                req.headers_mut().insert(
                    &crate::mcp::auth::ACT_AS_HEADER,
                    HeaderValue::from_static("guest"),
                );
                req
            })
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "the middleware admits the delegated guest; the handler refuses the write"
        );
        let parsed = body_json(resp).await;
        assert_eq!(parsed["error"]["code"], "guest_cannot_upload");
    }

    #[tokio::test]
    async fn bad_kind_unknown_id_and_missing_auth_are_rejected() {
        let (state, secret, _pool, _dir) = build_state().await;
        let token = user_token(&secret, "frodo");
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(upload_request(&token, multipart_body("foto", None, b"x")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let parsed = body_json(resp).await;
        assert_eq!(parsed["error"]["code"], "invalid_kind");

        let resp = app
            .clone()
            .oneshot(fetch_request(&token, "c-2026-06-12-photo-999.jpg"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app
            .clone()
            .oneshot(fetch_request(&token, "not-a-catalog-id"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Pin the real mount shape: `main.rs` nests this router at
    /// `/media`, and consumers POST to `/media` (no trailing slash —
    /// the hermes client derives exactly that). axum's `nest` must
    /// dispatch the bare prefix to the inner `/` route.
    #[tokio::test]
    async fn nested_mount_accepts_post_media_without_trailing_slash() {
        let (state, secret, _pool, _dir) = build_state().await;
        let token = user_token(&secret, "frodo");
        let app = axum::Router::new().nest("/media", router(state));

        let mut req = upload_request(&token, multipart_body("photo", None, b"x"));
        *req.uri_mut() = "/media".parse().unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let parsed = body_json(resp).await;
        let catalog_id = parsed["catalog_id"].as_str().unwrap();

        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{catalog_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unsafe_mime_is_served_as_attachment_octet_stream() {
        let (state, secret, _pool, _dir) = build_state().await;
        let token = user_token(&secret, "frodo");
        let app = router(state);

        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"kind\"\r\n\r\ndoc\r\n\
                 --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
                 filename=\"page.html\"\r\nContent-Type: text/html\r\n\r\n<script>x</script>\r\n\
                 --{BOUNDARY}--\r\n"
            )
            .as_bytes(),
        );
        let uploaded = body_json(
            app.clone()
                .oneshot(upload_request(&token, body))
                .await
                .unwrap(),
        )
        .await;
        let catalog_id = uploaded["catalog_id"].as_str().unwrap();

        let resp = app
            .oneshot(fetch_request(&token, catalog_id))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        let disposition = resp
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(disposition.starts_with("attachment"), "{disposition}");
    }
}
