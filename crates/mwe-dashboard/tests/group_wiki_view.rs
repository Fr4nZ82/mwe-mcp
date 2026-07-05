// SPDX-License-Identifier: AGPL-3.0-or-later
//! Regression: a **group-owned wiki** (a `wiki-group` root whose scope
//! principal derives to `group:…`) page view must render — the per-fragment
//! ACL governs the content directly — instead of 500-ing inside the comment /
//! edit-affordance read-access checks.
//!
//! `resolve_read_access` now resolves a group-owned wiki directly: a member of
//! the owning group is owner-equivalent, a non-member falls through to
//! per-fragment only. Either way the leaf-page view renders. Here the admin
//! `alice` is *not* a member of `famiglia` (a non-member), and the page still
//! renders.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{extract_cookie_value, extract_set_cookie, make_app_with_memory, send};

async fn setup_admin_and_login(app: &axum::Router) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=alice@example.com&admin_id=alice\
                 &password=correct-horse-battery&password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

#[tokio::test]
async fn group_owned_wiki_page_view_renders_instead_of_500() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin_and_login(&app).await;

    // Provision a group-owned wiki — its `_meta.md` carries
    // `acl_default: group:famiglia` (no single user owner) and a seeded index.md.
    let create = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/groups/new")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("group_id=famiglia&scope="))
            .unwrap(),
    )
    .await;
    assert!(
        create.status().is_success() || create.status().is_redirection(),
        "group create: {}",
        create.status(),
    );
    // Sanity: the group wiki really is group-owned (the precondition the fix is
    // about). The owning principal is derived from the `wiki-group` root type +
    // id (no `acl_default` declared), so `group:famiglia` is what the scope
    // resolver yields.
    let meta = std::fs::read_to_string(tree.wikis_dir().join("famiglia").join("_meta.md")).unwrap();
    assert!(meta.contains("wiki_type: wiki-group"), "{meta}");
    assert!(!meta.contains("acl_default"), "{meta}");
    let parsed =
        mwe_core::wiki::wiki_get_meta(&tree, &mwe_core::types::WikiId::parse("famiglia").unwrap())
            .expect("get meta");
    assert_eq!(
        tree.resolve_scope_principal(&parsed).expect("resolve"),
        mwe_core::types::Principal::Group("famiglia".into())
    );

    // The leaf-page view (`view_page`, not the wiki overview) is the handler
    // that runs the affordance read-access checks. Before the fix this 500-ed.
    let view = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/wiki/famiglia/view/index.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        view.status(),
        StatusCode::OK,
        "a group-owned wiki page must render, not 500"
    );
}
