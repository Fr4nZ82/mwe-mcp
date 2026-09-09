// SPDX-License-Identifier: AGPL-3.0-or-later
//! The guide, read inside the dashboard.
//!
//! `docs/` is the guide for people — one half for the operator who
//! installs and runs the server, one for whoever the memory is about. The
//! dashboard serves **that** folder: [`Guide`] embeds it from the
//! repository at compile time, the way the static assets and the bridge
//! trees are embedded, so the binary carries the pages and there is one
//! copy of them to keep true.
//!
//! Two routes:
//!
//! - `GET /dashboard/guide`       — the map, `docs/README.md`.
//! - `GET /dashboard/guide/*path` — one page, `docs/<path>.md`.
//!
//! # Who reads what
//!
//! `docs/user/` is for whoever the memory is about, and any signed-in
//! person opens it. `docs/operator/` describes the consoles only an admin
//! can open, so those pages follow their consoles and answer a reader
//! `403`. The map itself is served to everybody, with the operator's half
//! of it left out for a reader who is not the admin — the top bar hides
//! the admin entries the same way, and a listing of twenty links that each
//! answer `403` is worse than no listing.
//!
//! # The links between pages
//!
//! The guide is written to be read on GitHub as well, so its links are
//! plain relative markdown (`tokens.md`, `../user/your-facts.md`).
//! [`rewrite_link`] turns each one into the route that serves it, and
//! [`link_targets`] plus the tests below walk every page so a link that
//! reaches nothing is caught here rather than by a reader.

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::response::Html;
use axum::routing::get;
use maud::{PreEscaped, html};
use rust_embed::RustEmbed;

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::md_render::{self, PageRenderContext};
use crate::state::DashboardState;
use crate::ui::layout;

/// The guide's pages, embedded from the repository's `docs/` folder.
///
/// The path climbs out of the crate on purpose: `docs/` is the guide
/// people read on GitHub, and the binary carries that same folder rather
/// than a copy of it kept in step by hand.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../docs/"]
struct Guide;

/// The file that holds the map. Its address is `/dashboard/guide`, and it
/// is the one page not reachable under `/dashboard/guide/<path>`: a page
/// with two addresses is a page whose links disagree about where it is.
const MAP_FILE: &str = "README.md";

/// The guide's half that belongs to the admin. A page under it answers a
/// reader `403`, and a section of the map that only lists such pages is
/// not shown to them.
const OPERATOR_HALF: &str = "operator/";

/// Where the documents that are *not* the guide live. `docs/README.md`
/// points at `../INSTALL.md`, `../INTEGRATING.md`,
/// `../AGENT_INSTRUCTIONS.md` and `../CHANGELOG.md`, which sit in the
/// repository root and are not served here; a link that climbs out of
/// `docs/` is sent to the file on GitHub, which is where those four are
/// read.
const REPO_BLOB: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/main/");

/// Routes for the guide. Merged into the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/guide", get(map))
        .route("/guide/*path", get(page))
}

/// `GET /dashboard/guide` — the map, for every authenticated reader.
async fn map(State(state): State<DashboardState>, user: SessionUser) -> Result<Html<String>> {
    let source = read(MAP_FILE)?;
    let markdown = if user.is_admin {
        source
    } else {
        without_operator_sections(&source)
    };
    let (title, body) = title_and_body(&markdown, "The guide");
    Ok(render(&state, &user, title, "", body))
}

/// `GET /dashboard/guide/*path` — one page. `path` is the file's place
/// inside `docs/` without the `.md` (`user/your-facts`,
/// `operator/tokens`).
async fn page(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath(path): AxumPath<String>,
) -> Result<Html<String>> {
    if !is_page_path(&path) {
        return Err(DashboardError::NotFound);
    }
    let file = format!("{path}.md");
    if file == MAP_FILE {
        return Err(DashboardError::NotFound);
    }
    if file.starts_with(OPERATOR_HALF) && !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    let markdown = read(&file)?;
    let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let (title, body) = title_and_body(&markdown, &path);
    Ok(render(&state, &user, title, dir, body))
}

/// Is `path` the place of a file inside `docs/`, and nothing else?
///
/// Every segment has to be a real name: an empty one, a `.` or a `..`
/// names a file by walking, and a walked path arrives at
/// [`OPERATOR_HALF`] without starting with it — around the one check
/// that keeps the operator's half the admin's. `rust_embed` resolves
/// such a path against the filesystem when the binary is built without
/// optimisations, so the walk is a real one and this is where it stops.
fn is_page_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Read one embedded page, or `404`.
fn read(file: &str) -> Result<String> {
    let bytes = Guide::get(file).ok_or(DashboardError::NotFound)?;
    String::from_utf8(bytes.data.into_owned())
        .map_err(|e| DashboardError::Internal(format!("guide page {file} is not UTF-8: {e}")))
}

/// A page's title and the prose under it.
///
/// The title is the `# ` heading the page opens with — the one it carries
/// on GitHub — and the body is what follows: the layout prints the title
/// above every screen, so a page that also rendered its own heading would
/// show its name twice. `fallback` is the title of a page that opens
/// without a heading.
fn title_and_body<'a>(markdown: &'a str, fallback: &'a str) -> (&'a str, &'a str) {
    let trimmed = markdown.trim_start();
    let Some(rest) = trimmed.strip_prefix("# ") else {
        return (fallback, markdown);
    };
    let (heading, body) = rest.split_once('\n').unwrap_or((rest, ""));
    (heading.trim(), body)
}

/// Render one page's markdown inside the usual reading layout, with its
/// links pointed at the routes that serve them.
///
/// `dir` is the page's folder inside `docs/` (`""` for the map), which is
/// what a relative link is resolved against.
fn render(
    state: &DashboardState,
    user: &SessionUser,
    title: &str,
    dir: &str,
    markdown: &str,
) -> Html<String> {
    let ctx = PageRenderContext {
        // The guide is prose for people: it carries no `[[wikilink]]`
        // and no fact references, only plain markdown links.
        resolve_wikilink: &|_| None,
        resolve_md_link: &|target| rewrite_link(dir, target),
        fact_refs: false,
    };
    let html = md_render::render_page(markdown, /* reveal */ false, &ctx, |_| None);
    let body = html! {
        section.wiki-page-view.prose { (PreEscaped(html)) }
    };
    Html(layout::guide_page(
        layout::Chrome::of(state),
        title,
        user,
        &body,
    ))
}

/// Drop the runs of the map that exist only to list the operator's pages.
///
/// A run is one `## ` heading and everything under it (plus whatever
/// stands above the first heading). It goes when it carries links and
/// every one of them points into [`OPERATOR_HALF`] — which is what "this
/// is the operator's half" means, read off the links themselves rather
/// than off the heading's wording, so a page that moves takes the rule
/// with it. The map's opening prose carries no such link and stays: it is
/// what tells a reader which half is theirs.
fn without_operator_sections(map: &str) -> String {
    let mut kept = String::with_capacity(map.len());
    let mut run = String::new();
    for line in map.lines() {
        if line.starts_with("## ") {
            keep_unless_the_operators(&run, &mut kept);
            run.clear();
        }
        run.push_str(line);
        run.push('\n');
    }
    keep_unless_the_operators(&run, &mut kept);
    kept
}

/// Append `run` to `kept` unless every link it carries points into
/// [`OPERATOR_HALF`].
fn keep_unless_the_operators(run: &str, kept: &mut String) {
    let targets = link_targets(run);
    if targets.is_empty() || !targets.iter().all(|t| t.starts_with(OPERATOR_HALF)) {
        kept.push_str(run);
    }
}

/// Every markdown link destination in `markdown`, in source order.
///
/// A deliberately small scan of `](…)`: it is fed the guide's own pages,
/// which are plain prose with plain links.
fn link_targets(markdown: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = markdown;
    while let Some(i) = rest.find("](") {
        rest = &rest[i + 2..];
        let Some(end) = rest.find(')') else { break };
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    out
}

/// Where one markdown link written in a guide page points, once the
/// dashboard is the reader.
///
/// `from_dir` is the page's own folder inside `docs/` (`""` for the map).
/// `None` leaves the author's destination untouched — an absolute URL, a
/// bare `#anchor`, anything that is not a markdown file or a folder of
/// the guide.
///
/// Three shapes come out of it:
///
/// - a page of the guide → its route, with any `#anchor` carried over;
/// - a folder of the guide (`operator/`, `user/`) → the map, which is
///   where the dashboard lists what that folder holds;
/// - a file above `docs/` → the same file on GitHub ([`REPO_BLOB`]).
fn rewrite_link(from_dir: &str, target: &str) -> Option<String> {
    if target.is_empty()
        || target.starts_with('#')
        || target.starts_with('/')
        || target.contains("://")
        || target.starts_with("mailto:")
    {
        return None;
    }
    let (path, anchor) = target.split_once('#').map_or((target, ""), |(p, a)| (p, a));
    let is_folder = path.ends_with('/');

    // Resolve `path` against the page's folder. `climbed` counts the
    // steps that went above `docs/` — the guide's root — because those
    // are the links that leave it.
    let mut parts: Vec<&str> = if from_dir.is_empty() {
        Vec::new()
    } else {
        from_dir.split('/').collect()
    };
    let mut climbed = 0usize;
    for part in path.split('/') {
        match part {
            "" | "." => {},
            ".." => {
                if parts.pop().is_none() {
                    climbed += 1;
                }
            },
            other => parts.push(other),
        }
    }
    let resolved = parts.join("/");

    if climbed > 0 {
        // One step above `docs/` is the repository root, where the four
        // documents that are not the guide live. Anything higher is not
        // a link this repository can answer.
        return (climbed == 1 && !resolved.is_empty()).then(|| format!("{REPO_BLOB}{resolved}"));
    }
    if is_folder {
        return Some("/dashboard/guide".to_owned());
    }
    let href = if resolved == MAP_FILE {
        // The map's own address is the guide's root.
        "/dashboard/guide".to_owned()
    } else {
        let page = resolved.strip_suffix(".md").filter(|p| !p.is_empty())?;
        format!("/dashboard/guide/{page}")
    };
    Some(if anchor.is_empty() {
        href
    } else {
        format!("{href}#{anchor}")
    })
}

/// The screens whose guide page is the one obvious answer to "what is
/// this?", keyed on the title the screen renders.
///
/// The layout hangs a small **?** beside that title ([`crate::ui::layout`]),
/// and only where the pairing is one-to-one: a screen the guide covers
/// across several pages, or a page that covers several screens, is absent
/// here rather than pointed somewhere approximate. `Settings` is the
/// clearest of those — the account half is a reader's, the server
/// sections below it are the admin's, and the guide has a different page
/// for each.
const SCREEN_PAGES: &[(&str, &str)] = &[
    ("Home", "user/your-home"),
    ("Wikis", "user/wikis-and-pages"),
    ("Search", "user/search"),
    ("Facts", "user/your-facts"),
    ("Recall traces", "user/traces"),
    ("Proposals", "user/proposals"),
    ("Skills", "operator/skills"),
    ("Bridges", "operator/bridges"),
    ("Users", "operator/users"),
    ("Groups", "operator/groups"),
    ("Tokens", "operator/tokens"),
    ("Prompts", "operator/prompts"),
    ("Health", "operator/health"),
    ("Dream", "operator/dream"),
    ("Backup", "operator/backup"),
    ("Usage & spend", "operator/usage-and-spend"),
    ("Training spool", "operator/training-spool"),
    ("Recall settings", "operator/recall-settings"),
    ("REM settings", "operator/rem-settings"),
    ("Embedding settings", "operator/embedder"),
    ("LLM config", "operator/model-slots"),
];

/// The guide page for the screen titled `title`, or `None`.
///
/// `is_admin` is asked because the operator's half answers a reader
/// `403`: a **?** that refuses the person who pressed it is worse than
/// no **?** at all.
#[must_use]
pub fn page_for_screen(title: &str, is_admin: bool) -> Option<&'static str> {
    let (_, page) = SCREEN_PAGES.iter().find(|(screen, _)| *screen == title)?;
    (is_admin || !page.starts_with(OPERATOR_HALF)).then_some(*page)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every page of the guide, as the route path that serves it.
    fn embedded_pages() -> Vec<String> {
        Guide::iter()
            .filter_map(|f| f.strip_suffix(".md").map(str::to_owned))
            .collect()
    }

    /// The metrics page is the one page of the guide whose contents are
    /// generated elsewhere: `/metrics` publishes a fixed roster of names,
    /// and an operator writes alerts against those names. So the page
    /// documents every one of them and invents none — a family added to
    /// the engine without a sentence for the operator fails here, which
    /// is the only place it can fail before a reader finds it.
    #[test]
    fn the_metrics_page_documents_every_published_metric() {
        let page = read("operator/observability.md").expect("the metrics page is embedded");

        for name in mwe_core::metrics::FAMILIES {
            assert!(
                page.contains(name),
                "`{name}` is published by /metrics and explained nowhere in the guide"
            );
        }
        for line in page.lines() {
            for word in line.split(['`', ' ', '{', '|']) {
                assert!(
                    !word.starts_with("mwe_") || mwe_core::metrics::FAMILIES.contains(&word),
                    "the guide documents `{word}`, which /metrics does not publish"
                );
            }
        }
    }

    #[test]
    fn the_repository_docs_folder_is_embedded() {
        let pages = embedded_pages();
        assert!(pages.iter().any(|p| p == "README"), "{pages:?}");
        assert!(
            pages.iter().filter(|p| p.starts_with("operator/")).count() >= 10,
            "{pages:?}"
        );
        assert!(
            pages.iter().filter(|p| p.starts_with("user/")).count() >= 10,
            "{pages:?}"
        );
    }

    /// The release rule the guide is kept by ("every page is re-verified,
    /// a page nobody can verify is deleted") needs the links to hold. A
    /// link into the guide must name a page that is there; a link out of
    /// it must reach the repository, not a route.
    #[test]
    fn every_link_in_the_guide_reaches_something() {
        let pages = embedded_pages();
        let mut broken: Vec<String> = Vec::new();
        for file in Guide::iter() {
            let markdown = read(&file).unwrap();
            let path = file.strip_suffix(".md").unwrap();
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            for target in link_targets(&markdown) {
                let Some(href) = rewrite_link(dir, target) else {
                    broken.push(format!("{file}: {target} (rewritten to nothing)"));
                    continue;
                };
                if href.starts_with(REPO_BLOB) || href == "/dashboard/guide" {
                    continue;
                }
                let wanted = href.trim_start_matches("/dashboard/guide/");
                let wanted = wanted.split('#').next().unwrap();
                if !pages.iter().any(|p| p == wanted) {
                    broken.push(format!("{file}: {target} → {href}"));
                }
            }
        }
        assert!(
            broken.is_empty(),
            "guide links reaching nothing:\n{}",
            broken.join("\n")
        );
    }

    /// The route carries the file's path verbatim, so a page whose name
    /// needed escaping would arrive at a different file than the link
    /// said.
    #[test]
    fn every_page_name_is_safe_in_a_url() {
        for file in Guide::iter() {
            assert!(
                file.chars().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_uppercase()
                    || c.is_ascii_digit()
                    || matches!(c, '-' | '_' | '.' | '/')),
                "{file} needs URL escaping"
            );
        }
    }

    /// The layout prints a page's title above its body, so the heading
    /// the page opens with becomes that title instead of being rendered
    /// a second time.
    #[test]
    fn a_pages_heading_becomes_its_title_and_leaves_the_body() {
        let (title, body) = title_and_body("# Tokens\n\nA **consumer** proves who it is.\n", "x");
        assert_eq!(title, "Tokens");
        assert!(!body.contains("# Tokens"), "{body}");
        assert!(body.contains("A **consumer**"), "{body}");

        let (title, body) = title_and_body("no heading here\n", "user/somewhere");
        assert_eq!(title, "user/somewhere");
        assert_eq!(body, "no heading here\n");
    }

    #[test]
    fn only_a_plain_place_inside_the_guide_is_a_page_path() {
        assert!(is_page_path("user/your-facts"));
        assert!(is_page_path("operator/tokens"));
        for walked in [
            "",
            "./operator/tokens",
            "user/../operator/tokens",
            "operator//tokens",
            "..",
            "user\\your-facts",
        ] {
            assert!(!is_page_path(walked), "{walked}");
        }
    }

    #[test]
    fn a_link_resolves_against_the_page_that_carries_it() {
        assert_eq!(
            rewrite_link("operator", "tokens.md").as_deref(),
            Some("/dashboard/guide/operator/tokens")
        );
        assert_eq!(
            rewrite_link("operator", "../user/your-facts.md").as_deref(),
            Some("/dashboard/guide/user/your-facts")
        );
        assert_eq!(
            rewrite_link("", "operator/first-start.md").as_deref(),
            Some("/dashboard/guide/operator/first-start")
        );
    }

    /// A folder is not a page, and the map is where the dashboard lists
    /// what a folder holds.
    #[test]
    fn a_folder_link_reaches_the_map() {
        assert_eq!(
            rewrite_link("", "operator/").as_deref(),
            Some("/dashboard/guide")
        );
        assert_eq!(
            rewrite_link("", "user/").as_deref(),
            Some("/dashboard/guide")
        );
    }

    /// The four documents that are not the guide are read where they
    /// live, not served as guide pages.
    #[test]
    fn a_link_above_the_guide_goes_to_the_repository() {
        let install = format!("{REPO_BLOB}INSTALL.md");
        assert_eq!(
            rewrite_link("", "../INSTALL.md").as_deref(),
            Some(install.as_str())
        );
        assert_eq!(
            rewrite_link("operator", "../../INSTALL.md").as_deref(),
            Some(install.as_str())
        );
        assert_eq!(rewrite_link("operator", "../../../elsewhere.md"), None);
    }

    /// The map is named by its file on GitHub and by the guide's root
    /// here, and both have to arrive at the same page.
    #[test]
    fn a_link_to_the_map_reaches_the_guides_root() {
        assert_eq!(
            rewrite_link("", "README.md").as_deref(),
            Some("/dashboard/guide")
        );
        assert_eq!(
            rewrite_link("operator", "../README.md").as_deref(),
            Some("/dashboard/guide")
        );
        assert_eq!(rewrite_link("", "notes.txt"), None);
    }

    #[test]
    fn an_absolute_destination_is_left_alone() {
        assert_eq!(rewrite_link("", "https://example.org/x"), None);
        assert_eq!(rewrite_link("user", "#a-heading"), None);
        assert_eq!(rewrite_link("user", "/dashboard/home"), None);
    }

    #[test]
    fn an_anchor_survives_the_rewrite() {
        assert_eq!(
            rewrite_link("operator", "tokens.md#issuing-one").as_deref(),
            Some("/dashboard/guide/operator/tokens#issuing-one")
        );
    }

    /// The map a reader who is not the admin gets: the operator's listing
    /// is gone, everything that is theirs stays.
    #[test]
    fn the_readers_map_keeps_its_own_half_and_drops_the_operators() {
        let map = read(MAP_FILE).unwrap();
        let readers = without_operator_sections(&map);

        assert!(
            !link_targets(&readers)
                .iter()
                .any(|t| t.starts_with(OPERATOR_HALF) && *t != OPERATOR_HALF),
            "the reader's map still lists an operator page:\n{readers}"
        );
        for target in ["user/your-facts.md", "user/the-chat.md", "../CHANGELOG.md"] {
            assert!(
                link_targets(&readers).contains(&target),
                "the reader's map lost {target}"
            );
        }
        // The prose above the first `## ` is what tells a reader which
        // half is theirs, so it is never one of the dropped sections.
        assert!(readers.contains("# The mwe-mcp guide"));
        assert!(map.len() > readers.len() + 500, "nothing was dropped");
    }

    /// Every **?** offered beside a screen's title names a page that is
    /// there.
    #[test]
    fn every_screen_shortcut_names_a_page_of_the_guide() {
        let pages = embedded_pages();
        for (screen, page) in SCREEN_PAGES {
            assert!(pages.iter().any(|p| p == page), "{screen} → {page}");
        }
    }

    /// The operator's half refuses a reader, so it is not offered to one.
    #[test]
    fn a_reader_is_offered_no_shortcut_into_the_operators_half() {
        assert_eq!(page_for_screen("Tokens", true), Some("operator/tokens"));
        assert_eq!(page_for_screen("Tokens", false), None);
        assert_eq!(page_for_screen("Facts", false), Some("user/your-facts"));
        assert_eq!(page_for_screen("Restarting", true), None);
    }
}
