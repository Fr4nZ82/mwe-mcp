// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end integration: creating a user or a group via
//! the admin CRUD auto-materialises the corresponding identity wiki
//! on disk. The wizard slice for the admin's own wiki is covered by
//! `bootstrap.rs`; this file focuses on the post-setup CRUD path.

mod common;

use axum::body::Body;
use axum::http::{Request, header};
use common::{extract_cookie_value, extract_set_cookie, make_app_with_memory, send};

/// Drive the setup wizard to create the canonical admin and return the
/// cookie value ready to be re-presented as `Cookie:`.
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
async fn admin_creating_user_materialises_identity_wiki_on_disk() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin_and_login(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/users/new")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "user_id=bob&email=bob@example.com&aliases=robert,bobby",
            ))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_success() || response.status().is_redirection(),
        "{}",
        response.status(),
    );

    // The wiki dir for bob must exist with a valid _meta.md after the
    // post-commit hook.
    let bob_meta = tree.wikis_dir().join("bob").join("_meta.md");
    assert!(
        bob_meta.exists(),
        "expected {} to exist after admin created bob",
        bob_meta.display()
    );
    let raw = std::fs::read_to_string(&bob_meta).unwrap();
    assert!(raw.contains("wiki_id: bob"), "{raw}");
    // `wiki-user` root → scope principal derives to `user:bob` (no
    // `acl_default` declared any more).
    assert!(raw.contains("wiki_type: wiki-user"), "{raw}");
    assert!(!raw.contains("acl_default"), "{raw}");
    // Title is always the user_id (there is no separate label channel).
    assert!(raw.contains("title: bob"), "{raw}");
    let index = tree.wikis_dir().join("bob").join("index.md");
    assert!(index.exists(), "index.md must be seeded too");
}

#[tokio::test]
async fn admin_creating_group_materialises_group_identity_wiki() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin_and_login(&app).await;

    let response = send(
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
        response.status().is_success() || response.status().is_redirection(),
        "{}",
        response.status(),
    );
    let raw = std::fs::read_to_string(tree.wikis_dir().join("famiglia").join("_meta.md")).unwrap();
    assert!(raw.contains("wiki_id: famiglia"), "{raw}");
    // `wiki-group` root → scope principal derives to `group:famiglia` (no
    // `acl_default` declared any more).
    assert!(raw.contains("wiki_type: wiki-group"), "{raw}");
    assert!(!raw.contains("acl_default"), "{raw}");
    // Title is always the group_id (description is gone; scope is the only prose).
    assert!(raw.contains("title: famiglia"), "{raw}");
}
