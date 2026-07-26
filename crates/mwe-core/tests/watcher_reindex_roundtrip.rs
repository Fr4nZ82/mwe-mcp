// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end round-trip test for the watcher → `wiki_sections`
//! pipeline (the smart-wiki content index).
//!
//! Uses a real `notify` watcher backed by `tempfile` so the event
//! delivery + marker filter + reindex consumer all run under the same
//! `tokio` runtime that production uses.

#![cfg(feature = "test-fakes")]

use std::sync::Arc;
use std::time::Duration;

use mwe_core::embedder::FakeEmbedder;
use mwe_core::reindex;
use mwe_core::sections;
use mwe_core::watcher::WikiWatcher;
use mwe_core::wiki::{WikiTree, atomic_write};
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::tempdir;

/// How long to give the `notify`-backed watcher to register its watch
/// before the test fires the triggering filesystem mutation. Linux
/// `inotify` is effectively synchronous, but macOS `FSEvents` takes a
/// non-trivial fraction of a second to arm — without this delay the
/// triggering write can race ahead of the watch and be missed.
const WATCHER_ARM_DELAY: Duration = Duration::from_millis(500);

/// Upper bound on event delivery + reindex round-trip. Generous to
/// stay green under slow CI runners on every supported platform; the
/// happy path completes in tens of milliseconds locally.
const WATCHER_DELIVERY_TIMEOUT: Duration = Duration::from_secs(20);

async fn make_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("migrate");
    pool
}

/// A **smart** wiki — the content-indexed family the section-indexer
/// drop-and-reinserts from disk state (standard wikis get
/// offset-and-existence repair only; their unit coverage lives in
/// `reindex::tests`).
fn write_smart_wiki_meta(abs_dir: &std::path::Path, wiki_id: &str) {
    std::fs::create_dir_all(abs_dir).unwrap();
    let slug = wiki_id.rsplit('/').next().unwrap_or(wiki_id);
    // A smart wiki owned by the user whose id is `wiki_id` — a `wiki-user`
    // identity root, so the scope-principal derivation yields `user:<wiki_id>`.
    let meta = format!(
        "---\nwiki_id: {wiki_id}\ntitle: {slug}\nwiki_type: wiki-user\nslug: {slug}\nparent_wiki_id: null\nsmart: true\n---\n",
    );
    atomic_write(&abs_dir.join("_meta.md"), meta.as_bytes()).expect("write meta");
}

/// Spin until `predicate` returns `Some(v)` or `deadline` expires.
async fn wait_for<T, F, Fut>(timeout: Duration, mut predicate: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = predicate().await {
            return Some(v);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn third_party_write_triggers_reindex_insert() {
    let dir = tempdir().unwrap();
    let wiki_dir = dir.path().join("wikis/alice");
    write_smart_wiki_meta(&wiki_dir, "alice");

    let tree = WikiTree::open(dir.path()).expect("open tree");
    let pool = make_pool().await;
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

    let (watcher, _tx, rx) = WikiWatcher::start(tree.wikis_dir()).expect("start watcher");
    let _watcher_keep = watcher; // hold across the test

    let _loop_handle =
        reindex::spawn_watcher_loop(pool.clone(), Arc::new(tree.clone()), embedder.clone(), rx);

    // FSEvents (macOS) does not register an active watch synchronously
    // — give the notify thread a beat to arm before the third-party
    // write fires, otherwise the first event can be missed on cold CI
    // runners.
    tokio::time::sleep(WATCHER_ARM_DELAY).await;

    // Third-party (non-mwe-mcp) write: plain markdown, no markers. The
    // watcher must forward the Touched event and the smart section-indexer
    // must turn the page content into a recallable row.
    std::fs::write(wiki_dir.join("intro.md"), "# Intro\n\nthird-party note.\n").expect("write");

    let pool_for_wait = pool.clone();
    let found = wait_for(WATCHER_DELIVERY_TIMEOUT, move || {
        let pool = pool_for_wait.clone();
        async move {
            sections::find_page_sections(&pool, "wikis/alice/intro.md")
                .await
                .expect("query")
                .into_iter()
                .next()
        }
    })
    .await
    .expect("section landed in index within watcher delivery timeout");
    assert!(found.text.contains("third-party note"));
    assert_eq!(found.source_path, "wikis/alice/intro.md");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_with_held_marker_is_suppressed_by_filter() {
    // Marker filter: when the writer holds a `WriteMarker` over
    // the target, the watcher must NOT forward the event. The reindex
    // consumer therefore sees no event and does not touch the DB.
    //
    // We deliberately hold the marker for the full duration of the
    // test instead of going through `atomic_write` (which drops the
    // marker on return). Otherwise an inotify event queued by the
    // kernel before the marker file is unlinked, but delivered to the
    // watcher thread after, would race past the filter — the reindex
    // pipeline is idempotent so that race is harmless in production,
    // but it makes an end-to-end "suppressed" assertion non-deterministic.
    use mwe_core::watcher::WriteMarker;

    let dir = tempdir().unwrap();
    let wiki_dir = dir.path().join("wikis/alice");
    write_smart_wiki_meta(&wiki_dir, "alice");

    let tree = WikiTree::open(dir.path()).expect("open tree");
    let pool = make_pool().await;
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

    let (watcher, _tx, rx) = WikiWatcher::start(tree.wikis_dir()).expect("start watcher");
    let _watcher_keep = watcher;
    let _loop_handle =
        reindex::spawn_watcher_loop(pool.clone(), Arc::new(tree.clone()), embedder.clone(), rx);

    let page = wiki_dir.join("intro.md");
    let _marker = WriteMarker::acquire(&page).expect("acquire marker");
    std::fs::write(&page, "# Held\n\nheld-marker note.\n").expect("write under marker");

    // Give the watcher more than enough time to dispatch (or not
    // dispatch) the event.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let rows = sections::find_page_sections(&pool, "wikis/alice/intro.md")
        .await
        .expect("query");
    assert!(
        rows.is_empty(),
        "events under a held WriteMarker must be suppressed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn third_party_delete_hard_drops_index_rows() {
    let dir = tempdir().unwrap();
    let wiki_dir = dir.path().join("wikis/alice");
    write_smart_wiki_meta(&wiki_dir, "alice");

    let tree = WikiTree::open(dir.path()).expect("open tree");
    let pool = make_pool().await;
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

    // Pre-seed: write a page + section-index it synchronously so the row
    // is in the DB before we attach the watcher.
    let page = wiki_dir.join("intro.md");
    std::fs::write(&page, "# Condemned\n\ncondemned.\n").unwrap();
    reindex::reindex_file(&pool, &tree, embedder.clone(), &page)
        .await
        .unwrap();
    let seeded = sections::find_page_sections(&pool, "wikis/alice/intro.md")
        .await
        .unwrap();
    assert_eq!(seeded.len(), 1);

    let (watcher, _tx, rx) = WikiWatcher::start(tree.wikis_dir()).expect("start watcher");
    let _watcher_keep = watcher;
    let _loop_handle =
        reindex::spawn_watcher_loop(pool.clone(), Arc::new(tree.clone()), embedder.clone(), rx);

    tokio::time::sleep(WATCHER_ARM_DELAY).await;
    std::fs::remove_file(&page).expect("delete page");

    // Markerless smart wiki: the deleted page's row is hard-dropped (no
    // tombstone), so it disappears from the index entirely.
    let pool_for_wait = pool.clone();
    let gone = wait_for(WATCHER_DELIVERY_TIMEOUT, move || {
        let pool = pool_for_wait.clone();
        async move {
            let rows = sections::find_page_sections(&pool, "wikis/alice/intro.md")
                .await
                .expect("query");
            rows.is_empty().then_some(())
        }
    })
    .await;
    assert!(
        gone.is_some(),
        "deleted smart page's sections hard-dropped within watcher delivery timeout"
    );
}
