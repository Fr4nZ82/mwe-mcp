// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shared `<html>` shell used by every dashboard page.
//!
//! Two flavours of base layout:
//!
//! - [`anonymous_page`] for the setup wizard, the login form, and the
//!   accept-invite landing page — no top nav (the visitor is not yet
//!   logged in), no footer reference to "dashboard home".
//! - [`authenticated_page`] for everything behind the session
//!   middleware — renders the top nav with the admin-only sections
//!   gated on `SessionUser::is_admin`, plus a "logged in as …" badge.
//!
//! Both wrap a Maud `Markup` body and hand back a fully-rendered
//! `String` (ready for [`axum::response::Html`]). The error helper
//! [`error_page`] is what [`crate::error::DashboardError`] uses to
//! render its HTTP responses.
//!
//! Phase 2 of the phosphor-terminal restyle: this shell renders with
//! Tailwind v4 utility classes coming from `tailwind/app.css` (loaded
//! at `/dashboard/static/tailwind.css`). The legacy hand-written
//! `app.css` is still linked second so per-page bodies that have not
//! yet been migrated keep their styles; Phase 3 migrates the bodies
//! and removes the legacy link.

use axum::http::StatusCode;
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::auth::SessionUser;
use crate::state::DashboardState;

/// What the page **frame** needs to know about the deployment, as
/// distinct from what it knows about the visitor.
///
/// `SessionUser` answers "who is looking at this"; `Chrome` answers "what
/// kind of instance are they looking at it on". Keeping them apart is why
/// the deployment posture is a parameter of the layout rather than a
/// field on the session: an identity is per-request, a posture is per
/// process, and conflating them would put a config value somewhere a
/// reviewer would have to be told to look.
///
/// Threaded from the handler (which holds [`DashboardState`]) down to
/// [`shell`]. Cheap to clone (a bool and a refcount bump), so a render
/// helper can take it by value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chrome {
    /// The deployment is frozen ([`crate::read_only`]). The frame drops
    /// the chat panel and every nav entry whose page is not mounted, and
    /// carries a standing notice instead.
    pub read_only: bool,
    /// The identities a visitor may become without a password, in button
    /// order — empty unless the demo entrance is configured
    /// ([`crate::routes::demo`]). Non-empty puts the one-click identity
    /// switcher in the top bar, on every page: comparing the *same* page
    /// as two people is the demonstration, and a switcher that lived only
    /// on the sign-in screen would make the visitor navigate back each
    /// time.
    ///
    /// `Arc<[String]>` rather than a slice so `Chrome` stays owned and
    /// threadable through the render helpers without a lifetime; cloning
    /// is a refcount bump.
    pub demo_identities: std::sync::Arc<[String]>,
}

impl Chrome {
    /// The posture of the deployment this request is being served by.
    #[must_use]
    pub fn of(state: &DashboardState) -> Self {
        Self {
            read_only: state.config.read_only,
            demo_identities: if state.config.demo_entrance_enabled() {
                std::sync::Arc::clone(&state.config.demo_identities)
            } else {
                std::sync::Arc::from([])
            },
        }
    }

    /// `<body>` classes for an authenticated page.
    ///
    /// `has-chat-panel` reserves the right gutter the panel occupies; a
    /// frozen deployment renders no panel, so reserving the space would
    /// leave a permanent empty column.
    const fn body_class(&self, reading: bool) -> &'static str {
        match (self.read_only, reading) {
            (false, false) => "has-chat-panel",
            (false, true) => "has-chat-panel reading-main",
            (true, false) => "",
            (true, true) => "reading-main",
        }
    }
}

/// Session keepalive: on user interaction (click / keypress), throttled to
/// at most once every 4 minutes and only while the tab is visible, ping
/// `/dashboard/session/keepalive`. That request passes through the
/// session-refresh middleware, which re-issues the cookie with a fresh
/// `exp` — so an actively-used tab keeps its sliding session alive even
/// across a long, all-client-side form (the welcome primer). The endpoint
/// returns 204; the response is ignored. See the
/// JWT & session model.
const SESSION_KEEPALIVE_JS: &str = "(function(){\
var url='/dashboard/session/keepalive',last=0,MIN=240000;\
function ping(){\
var now=Date.now();\
if(now-last<MIN||document.visibilityState==='hidden')return;\
last=now;\
fetch(url,{method:'GET',credentials:'same-origin',cache:'no-store'}).catch(function(){});\
}\
document.addEventListener('click',ping,true);\
document.addEventListener('keydown',ping,true);\
})();";

/// Render the full HTML page for a *not yet authenticated* visitor whose
/// content is a **single focused form** — login, setup wizard, invitation
/// acceptance, password recovery, the OAuth approve step.
#[must_use]
pub fn anonymous_page(title: &str, body: &Markup) -> String {
    // No nav, a single focused form (login / setup / invite / recovery): the
    // `anon-shell` class centers it as a narrow column (see app.css) instead of
    // pinning it to the left of a wide empty page.
    shell(Chrome::default(), title, None, "anon-shell", body).into_string()
}

/// Like [`anonymous_page`] but for an *informational / onboarding* anonymous
/// surface rather than a single focused form — the public front splash and the
/// bridges catalog / per-consumer guides.
///
/// These read as content pages (intro copy, the consumer table, the long
/// `mcp add` command blocks), so the 30rem login-form width of `anon-shell`
/// cramps them. They reuse the `reading-main` cap — a centered reading-width
/// column (see app.css), auth-agnostic — instead. Still no top nav or chat
/// panel: the visitor is anonymous.
#[must_use]
pub fn anonymous_reading_page(title: &str, body: &Markup) -> String {
    shell(Chrome::default(), title, None, "reading-main", body).into_string()
}

/// Like [`anonymous_reading_page`] but centred — a **hero**: one line of
/// explanation and a row of large buttons, with nothing else on the page.
///
/// The centring is on `<body>` so the shell's `<h1>` inherits it too.
/// Rendering the title flush left above a centred row of buttons reads as
/// two pages stacked, and this is the first screen a stranger sees of the
/// product. Anything inside the body that must stay flush left (a form's
/// labels and fields) says so locally.
///
/// Only the demo sign-in screen uses it ([`crate::routes::demo`]).
#[must_use]
pub fn anonymous_hero_page(title: &str, body: &Markup) -> String {
    shell(
        Chrome::default(),
        title,
        None,
        "reading-main text-center",
        body,
    )
    .into_string()
}

/// Render the full HTML page for an authenticated visitor; the top nav
/// includes admin-gated entries when `user.is_admin`, minus the ones the
/// deployment's [`Chrome`] does not mount.
#[must_use]
pub fn authenticated_page(
    chrome: Chrome,
    title: &str,
    user: &SessionUser,
    body: &Markup,
) -> String {
    let class = chrome.body_class(false);
    shell(chrome, title, Some(user), class, body).into_string()
}

/// Like [`authenticated_page`] but for a **reading / single-form** surface
/// whose content is a narrow column.
///
/// (A wiki page, a page viewer, an edit/comment/describe form, the help page.)
/// The `reading-main` class caps and centers `main` so the column sits centered
/// rather than pinned to the left of a wide screen. Data-heavy pages (tables,
/// config grids, dashboards) keep [`authenticated_page`] and use the full width.
#[must_use]
pub fn authenticated_reading_page(
    chrome: Chrome,
    title: &str,
    user: &SessionUser,
    body: &Markup,
) -> String {
    let class = chrome.body_class(true);
    shell(chrome, title, Some(user), class, body).into_string()
}

/// Shared body used both by the error converter and by ad-hoc
/// non-page payloads (e.g. CLI bootstrap response).
#[must_use]
pub fn render_page(title: &str, body: &Markup) -> String {
    shell(Chrome::default(), title, None, "anon-shell", body).into_string()
}

/// The HTML shell — `<head>`, top nav, page header, body slot, footer.
///
/// Authenticated pages also embed the persistent right-side chat panel
/// per the dashboard frontend, the floating "open chat"
/// FAB used when the panel is dismissed, and the small `ui.js` that
/// drives the hamburger and chat toggles. `chat.js` keeps the chat
/// content responsibilities (hydration from `localStorage`, agentic
/// submit, drag-resize); `ui.js` keeps the shell-toggle ones.
fn shell(
    chrome: Chrome,
    title: &str,
    user: Option<&SessionUser>,
    body_class: &str,
    body: &Markup,
) -> Markup {
    // Destructured rather than borrowed field by field: the two halves
    // are used in four places between `<head>`, the top bar and the
    // panel mounts, and taking the whole thing apart once keeps every
    // one of them a plain value.
    let Chrome {
        read_only,
        demo_identities,
    } = chrome;
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · mwe-mcp" }
                // SVG mark from the design pack — scales for every
                // browser tab size without rasterisation. Embedded via
                // rust-embed alongside the rest of the dashboard assets.
                link rel="icon" type="image/svg+xml" href="/dashboard/static/mwe-mark.svg";
                // Phase 3: the legacy `app.css` is gone; every rule
                // it owned was ported into `tailwind/app.css` under
                // `@layer components` with the colour references
                // rerouted through the design tokens. Page bodies
                // keep their original class names (`.flash`, `.kpi`,
                // `.config-table`, …) and pick up the phosphor
                // palette automatically.
                link rel="stylesheet" href="/dashboard/static/tailwind.css";
                @if let Some(u) = user {
                    // Per-user namespace for the chat panel's localStorage —
                    // history must not leak across accounts on a shared
                    // browser. Set before the deferred chat.js runs.
                    script { (PreEscaped(format!("window.__mweUser={:?};", u.sender_id))) }
                    script src="/dashboard/static/ui.js" defer {}
                    // chat.js only ever drives the chat panel, which a frozen
                    // deployment does not render.
                    @if !read_only { script src="/dashboard/static/chat.js" defer {} }
                    // A frozen instance renders every write control and then
                    // makes them visibly inert. The exempt list is the
                    // server's own `ALLOWED_WRITES`, handed to the script
                    // rather than restated in it, so what stays clickable is
                    // exactly what the guard still accepts.
                    @if read_only {
                        script { (PreEscaped(crate::read_only::live_writes_js())) }
                        script src="/dashboard/static/read-only.js" defer {}
                    }
                    // Keep the sliding session alive while the user is
                    // actively interacting (esp. the multi-step welcome
                    // primer, which makes no request until final submit).
                    script { (PreEscaped(SESSION_KEEPALIVE_JS)) }
                }
            }
            body class=(body_class) {
                (header(read_only, &demo_identities, user))
                // Content width: the old hard 1040px cap left big dead zones
                // on wide (≥2k / 4k) screens. We now let `main` use the width
                // up to a generous cap and manage readability at the element
                // level instead — inputs/forms cap their own width and
                // flex-wrap into columns (.field-grid), prose columns cap their
                // line length, tables take the full width and scroll inside
                // their own container. Mobile-first is preserved: the padding
                // and every per-element rule collapse to a single column on
                // small viewports.
                main class="w-full app-main mx-auto px-4 md:px-6 lg:px-8 mt-6 md:mt-8" {
                    @if read_only && user.is_some() {
                        div class="mb-4" { (crate::read_only::banner()) }
                    }
                    h1 class="text-xl md:text-2xl mb-4 mt-0" { (title) }
                    (body)
                }
                footer class="site-footer mt-12 mb-6 text-text-dim text-xs text-center" {
                    span { "mwe-mcp " (PreEscaped(crate::VERSION)) }
                }
                // The chat panel captures memory on every turn, so a frozen
                // deployment does not render it — nor its reopen FAB, nor the
                // Help overlay, which is entirely about how to operate the
                // memory through that chat.
                @if user.is_some() && !read_only {
                    (chat_panel())
                    (chat_reopen_fab())
                    // Help overlay. Hidden;
                    // ui.js reveals it when the topnav "Help" button is
                    // clicked. Shown to every authenticated user — the
                    // free-form operative chat needs discoverability for
                    // non-admins too.
                    (help_modal())
                }
            }
        }
    }
}

/// Top navigation. Always shows the brand; for logged-in users adds
/// the nav links + the "logged in as <id>" badge + a logout form.
///
/// Mobile-first layout: under the `md` breakpoint the nav collapses
/// behind a hamburger button (`#nav-toggle`). The button toggles a
/// `.nav-open` class on the nav element, which the components layer
/// of `tailwind/app.css` translates into a stacked column underneath
/// the topbar. The session badge is hidden on mobile to keep the
/// topbar single-row when the nav is closed.
fn header(read_only: bool, demo_identities: &[String], user: Option<&SessionUser>) -> Markup {
    html! {
        header class="site-header sticky top-0 z-30 flex flex-wrap items-center gap-3 px-4 md:px-6 py-3 border-b border-border bg-bg-2" {
            a href="/dashboard/"
                class="brand flex items-center gap-3 no-underline shrink-0 whitespace-nowrap" {
                img src="/dashboard/static/mwe-mark.svg" alt="" class="h-9 w-9 shrink-0";
                // Two-line vertical lockup: wordmark on top,
                // "dashboard" tagline underneath. Reads as one
                // logo block at every viewport — no separate
                // tagline span floating in the topbar anymore.
                span class="flex flex-col leading-tight" {
                    span class="wordmark text-base md:text-lg" {
                        "mwe" span.dim { "-mcp" }
                    }
                    span class="text-text-dim text-[10px] uppercase tracking-[0.18em] mt-0.5" {
                        "dashboard"
                    }
                }
            }
            @if let Some(u) = user {
                button id="nav-toggle" type="button"
                    class="md:hidden ml-auto inline-flex items-center justify-center w-9 h-9 border border-border rounded bg-bg-3 text-phosphor text-lg leading-none"
                    aria-label="Toggle navigation" aria-expanded="false" aria-controls="site-nav" {
                    "≡"
                }
                nav id="site-nav"
                    class="site-nav md:flex md:flex-row md:flex-wrap md:items-center md:gap-1 md:ml-3 basis-full md:basis-auto order-last md:order-none" {
                    (nav_link("/dashboard/home", "Home"))
                    // Single "Wikis" entry; the Standard / Smart split is a
                    // tab bar on the page itself (`wiki_view::wiki_family_tabs`),
                    // so a smart wiki no longer shows up under two nav links.
                    (nav_link("/dashboard/wiki", "Wikis"))
                    (nav_link("/dashboard/skills", "Skills"))
                    (nav_link("/dashboard/facts", "Facts"))
                    (nav_link("/dashboard/bridges", "Bridges"))
                    // "Traces" — your own last recalls and the 3D replay of
                    // the route each took. Not admin-gated: a trace belongs
                    // to the sender it was recorded for, so reading your own
                    // is transparency about the answer you were given, not
                    // operator telemetry.
                    (nav_link("/dashboard/recall-traces", "Traces"))
                    @if u.is_admin {
                        (nav_link("/dashboard/admin/health", "Health"))
                        // The operator's consoles, linked on every
                        // deployment including a frozen one: `routes::build`
                        // mounts them unconditionally, and a shown instance
                        // that hid them would be showing half the product.
                        // Keep the two lists in step.
                        (nav_link("/dashboard/users", "Users"))
                        (nav_link("/dashboard/groups", "Groups"))
                        (nav_link("/dashboard/tokens", "Tokens"))
                        (nav_link("/dashboard/prompts", "Prompts"))
                        (nav_link("/dashboard/admin/llm-config", "LLM"))
                        (nav_link("/dashboard/admin/embedding", "Embedding"))
                        (nav_link("/dashboard/admin/recall-settings", "Recall"))
                        (nav_link("/dashboard/admin/rem-settings", "REM"))
                        // Admin "Spool" — the training-pair recorder
                        // (distillation dataset toggle + inventory).
                        (nav_link("/dashboard/admin/training-spool", "Spool"))
                        (nav_link("/dashboard/admin/backup", "Backup"))
                        // Admin "Dream" — the on-demand console + run history
                        // (forms at the top, the journal table below).
                        (nav_link("/dashboard/dream", "Dream"))
                    }
                    (nav_link("/dashboard/settings/me", "Settings"))
                }
                // Session badge + logout button live in one flex
                // block so they always stay glued together and
                // always sit on the right edge of the topbar. The
                // wrapper gets `md:ml-auto` to push itself (and the
                // implicit margin between it and the nav) to the
                // right on desktop; on mobile the hamburger above
                // already has `ml-auto`, which pushes the wrapper
                // to the right side of row 1 too. The badge text
                // hides below md to keep the mobile row short.
                div class="user-block flex items-center gap-3 shrink-0 md:ml-auto" {
                    // Dream indicator (admin-only). Hidden by default; ui.js
                    // shows an animated "dream…" pill while a background dream
                    // (Compile / Full REM, kicked off from the Dream modal)
                    // runs — polling /dashboard/dream/status — then swaps it for
                    // the one-line outcome (click to dismiss). Inline design
                    // tokens, no new Tailwind utility to recompile.
                    @if u.is_admin && !read_only {
                        span id="dream-indicator"
                            style="display:none;align-items:center;padding:.1rem .55rem;font-size:.72rem;font-weight:700;border-radius:9999px;white-space:nowrap;color:var(--bg);background:var(--p)" {}
                    }
                    // In-flight badge. Hidden by default; ui.js fetches
                    // /dashboard/proposals/in-flight-count on load and reveals
                    // it (style.display) with the count when total > 0.
                    // chat.js intercepts the click: it opens the chat panel
                    // and fetches the overview turn from
                    // /dashboard/proposals/in-flight/chat-turn, rendering it
                    // inline with a spinner — no page navigation. The href is
                    // a safe fallback to the chat surface (the badge is itself
                    // JS-revealed, so it only matters if the click handler
                    // failed to attach). Styled with inline design tokens
                    // (like the Dream/Help modals) so no new Tailwind utility
                    // needs recompiling.
                    // Nothing is actionable on a frozen deployment, and the
                    // badge's whole affordance is "open this in chat".
                    @if !read_only {
                        a id="in-flight-badge" href="/dashboard/chat"
                            style="display:none;align-items:center;padding:.1rem .55rem;font-size:.72rem;font-weight:700;color:var(--bg);background:var(--amber);border-radius:9999px;text-decoration:none;white-space:nowrap"
                            title="Things you can still act on — open in chat" {
                            span id="in-flight-badge-count" {}
                        }
                    }
                    // The operative-chat Help trigger lives in the chat
                    // panel header (see `chat_panel`), not here — Help is
                    // about the chat, so it sits with it. The in-flight
                    // badge stays in the topnav.
                    // The one-click identity switcher. It sits in the frame,
                    // not on the sign-in screen, because the demonstration is
                    // reading the *same page* as two people: the switch has to
                    // land the visitor back where they were reading, and a
                    // sign-out-and-in round trip would end the comparison.
                    @if !demo_identities.is_empty() {
                        (crate::routes::demo::buttons(
                            demo_identities,
                            Some(&u.sender_id),
                            /* compact */ true,
                        ))
                    }
                    // The badge normally hides below `md` to keep the mobile
                    // topbar to one row. On a demo instance it stays: whose
                    // eyes you are using is the single most important thing
                    // on the screen, and the switcher next to it is useless
                    // without it.
                    span class=(if demo_identities.is_empty() {
                        "session-badge text-xs text-text-dim hidden md:inline"
                    } else {
                        "session-badge text-xs text-text-dim"
                    }) {
                        @if demo_identities.is_empty() { "Signed in as " } @else { "You are " }
                        strong class="text-text font-bold" { (u.sender_id) }
                        @if u.is_admin {
                            " · "
                            span class="text-amber" { "admin" }
                        }
                    }
                    form action="/dashboard/logout" method="post" class="logout-form contents" {
                        button type="submit"
                            class="px-3 py-1.5 text-xs border border-border rounded bg-bg-3 text-text-dim hover:text-rose hover:border-rose transition-colors" {
                            "Log out"
                        }
                    }
                }
            }
        }
    }
}

/// One link inside `.site-nav`. Phosphor-dim by default, lights up
/// to primary phosphor on hover.
fn nav_link(href: &str, label: &str) -> Markup {
    html! {
        a href=(href)
            class="px-3 py-1.5 text-xs text-text-dim hover:text-phosphor hover:bg-bg-3 rounded no-underline transition-colors" {
            (label)
        }
    }
}

/// Persistent right-side chat panel rendered on every authenticated
/// page per the dashboard frontend.
///
/// The default visibility is driven by the body's `.chat-open` class
/// (managed at runtime by `ui.js` from `localStorage` + viewport
/// width). Without `.chat-open` the panel is hidden and the
/// [`chat_reopen_fab`] takes over; with `.chat-open` it becomes the
/// fixed right-edge sidebar and, on `xl` (≥1280 px) viewports, the
/// body reserves padding-right for it so page content does not slide
/// underneath.
///
/// Anatomy (semantic classes preserved as JS hooks + integration-test
/// assertions):
///
/// - `.chat-panel-resize-handle` on the left edge — `chat.js` listens
///   for `mousedown` here and updates `panel.style.width` while
///   dragging; the width is persisted under
///   `localStorage.mwe-mcp.chat.width` and rehydrated on subsequent
///   loads.
/// - `.chat-panel-header` carries the H2 title and the close
///   button. `#chat-close` is the close affordance added in Phase 2;
///   `ui.js` toggles the body class and persists the choice under
///   `localStorage.mwe-mcp.chat.open`.
/// - `.chat-panel-messages` is the scroll area populated entirely
///   client-side from `localStorage.mwe-mcp.chat.history` (FIFO, 100
///   entries). The server *never* injects past turns into this list:
///   conversation history is a client concern — the engine relies on
///   `wiki_recall` + autocapture for the continuity that an LLM
///   context window would normally provide.
/// - A form at the bottom that posts to `/dashboard/chat` with
///   `Accept: application/json`. `chat.js` intercepts the submit,
///   `fetch`-es the endpoint, appends the returned `response_html` to
///   the scroll area, and persists the turn to `localStorage` —
///   trimming the oldest entries when the list exceeds 100.
///
/// The form keeps a real `method="post"` action so the no-JavaScript
/// path is honest: visitors without JS submit a vanilla form and the
/// server returns the full chat page with the response inline.
fn chat_panel() -> Markup {
    html! {
        aside id="chat-panel"
            class="chat-panel fixed top-0 right-0 h-screen w-[380px] bg-bg-1 border-l border-border z-40 flex-col" {
            div id="chat-panel-resize"
                class="chat-panel-resize-handle absolute top-0 -left-1 w-2 h-full cursor-ew-resize hover:bg-phosphor/25 z-10"
                role="separator" aria-orientation="vertical"
                title="Drag to resize the chat panel" {}
            header class="chat-panel-header flex items-center justify-between gap-2 px-4 pt-3 pb-2 border-b border-border" {
                h2 class="text-sm font-bold text-phosphor m-0 leading-none" { "Chat" }
                // Right-side controls: Help sits between the title and the
                // close (×). Help is about the chat — operating on the
                // memory in plain language — so it lives with it rather
                // than in the topnav. Both stay in this single flex group
                // so `justify-between` keeps the title on the left.
                div class="flex items-center gap-2" {
                    // Clear — wipes this user's chat scrollback from
                    // localStorage + the panel (wired in chat.js).
                    button id="chat-clear" type="button"
                        class="px-3 py-1.5 text-xs border border-border rounded bg-bg-3 text-text-dim hover:text-phosphor hover:border-phosphor transition-colors leading-none"
                        title="Clear this conversation" {
                        "Clear"
                    }
                    // Help — opens the operative-chat help modal via ui.js;
                    // the href is the honest no-JS fallback page. Uses the
                    // same utilities the topnav Help carried (all present
                    // in the compiled CSS — no new Tailwind class).
                    a href="/dashboard/help" id="help-open"
                        class="px-3 py-1.5 text-xs border border-border rounded bg-bg-3 text-text-dim hover:text-phosphor hover:border-phosphor transition-colors no-underline leading-none" {
                        "Help"
                    }
                    // Understated close — phosphor-dim ramping to text
                    // on hover (no aggressive rose) so the X feels like
                    // a calm dismissal, not a danger action.
                    button id="chat-close" type="button"
                        class="w-7 h-7 inline-flex items-center justify-center text-phosphor-dim hover:text-text hover:bg-bg-3 rounded text-lg leading-none"
                        aria-label="Close chat panel" { "×" }
                }
            }
            p class="chat-panel-hint text-xs text-text-dim px-4 pt-2 pb-0 m-0 leading-snug" {
                "History stays in your browser; each message is a fresh turn. "
                "This is an operative surface — it acts on the memory through tools, "
                "it does not auto-capture or recall what you type here."
            }
            div id="chat-panel-messages"
                class="chat-panel-messages flex-1 overflow-y-auto px-4 py-3 flex flex-col gap-3"
                aria-live="polite" {}
            form id="chat-panel-form" class="chat-panel-form"
                action="/dashboard/chat" method="post" {
                label for="chat-panel-text" class="sr-only" { "Chat with the engine" }
                textarea id="chat-panel-text" name="text" rows="2"
                    class="w-full bg-bg p-2 border border-border rounded text-text font-mono text-sm resize-y focus:outline-none focus:border-phosphor"
                    placeholder="Operate on the memory by chatting — move a wiki, retune or reshape items, undo a change, or ask what's pending." {}
                button type="submit"
                    class="self-end px-3 py-1.5 text-xs font-bold border border-phosphor rounded bg-bg-2 text-phosphor hover:bg-bg-3 hover:text-phosphor-bright" {
                    "Send"
                }
            }
        }
    }
}

/// Floating "open chat" button shown whenever the chat panel is
/// dismissed (or on viewports smaller than `xl` by default). Sits
/// bottom-right with a solid phosphor fill + glow so it reads as
/// the primary affordance to bring the chat back. Outline-only
/// styling was too easy to miss on the dark gradient background.
/// `ui.js` wires the click to the
/// body-class toggle + `localStorage` persistence.
fn chat_reopen_fab() -> Markup {
    html! {
        button id="chat-reopen" type="button"
            class="fixed bottom-5 right-5 z-30 w-14 h-14 rounded-full bg-phosphor text-bg text-2xl font-bold hover:bg-phosphor-bright items-center justify-center shadow-glow border-2 border-phosphor-bright"
            aria-label="Open chat panel" {
            // Speech-bubble glyph from CJK punctuation — fills the
            // button visually without pulling in an SVG icon set.
            "💬"
        }
    }
}

/// The operative-chat help modal, rendered
/// in the shell for every authenticated user. Hidden by default; `ui.js`
/// flips `display` to `flex` when the topnav "Help" button is clicked
/// (and back on backdrop click / close button / Escape). Without JS the
/// "Help" link is a real anchor to `GET /dashboard/help`, which renders
/// the same body as a standalone page, so the surface degrades cleanly.
///
/// Styled with the inline CSS design tokens (`var(--bg-1)`, `var(--p)`,
/// …) the Dream modal already uses — no new Tailwind utility classes,
/// so nothing needs recompiling.
fn help_modal() -> Markup {
    html! {
        div id="help-modal" role="dialog" aria-modal="true" aria-label="Help"
            style="display:none;position:fixed;inset:0;z-index:60;align-items:center;justify-content:center;background:rgba(0,0,0,.62);padding:1rem" {
            div style="max-width:38rem;width:100%;max-height:85vh;overflow-y:auto;background:var(--bg-1);border:1px solid var(--border-hi);border-radius:.6rem;padding:1.4rem;box-shadow:0 12px 40px rgba(0,0,0,.5)" {
                div style="display:flex;align-items:center;justify-content:space-between;gap:1rem;margin-bottom:.4rem" {
                    h2 style="margin:0;font-size:1rem;font-weight:700;color:var(--p)" { "Talking to the chat" }
                    button id="help-close" type="button" aria-label="Close"
                        style="width:1.8rem;height:1.8rem;display:inline-flex;align-items:center;justify-content:center;font-size:1.2rem;line-height:1;color:var(--p-dim);background:transparent;border:0;cursor:pointer" {
                        "×"
                    }
                }
                (help_body())
            }
        }
    }
}

/// The shared Help content — used both inside the `help_modal` overlay
/// and by the no-JS `GET /dashboard/help` fallback page so the two
/// cannot drift.
///
/// Concise and skimmable: the chat is *how you operate on the memory*,
/// with example phrasings mapped to what they do. The dashboard chrome
/// is English, so the copy is English with the italian phrasings the
/// internal LLM understands shown as the spoken examples.
#[must_use]
pub fn help_body() -> Markup {
    html! {
        p style="margin:.2rem 0 .8rem;font-size:.85rem;color:var(--text-dim);line-height:1.5" {
            "The chat panel on the right is how you operate on your memory — "
            "moving things, retuning them, changing their shape, undoing them. "
            "Just say what you want in plain language; the assistant uses the "
            "tools and always asks you to confirm before it writes anything."
        }
        ul style="margin:0;padding:0;list-style:none;display:flex;flex-direction:column;gap:.55rem" {
            (help_row("\u{201C}sposta la lista nel gruppo famiglia\u{201D} / \u{201C}move X under Y\u{201D}",
                "Move a wiki to another scope (e.g. into a group)."))
            (help_row("\u{201C}tieni gli elementi 2 giorni\u{201D} / \u{201C}keep items two days\u{201D}",
                "Retune an item\u{2019}s permanence / time-to-live."))
            (help_row("\u{201C}aggiungi il campo \u{00AB}chi-ha-ordinato\u{00BB} agli elementi\u{201D} / \u{201C}add a field to the items\u{201D}",
                "Change the item schema."))
            (help_row("\u{201C}annulla la lista\u{201D} / \u{201C}undo that\u{201D}",
                "Undo a just-created structured wiki (within its revert window)."))
            (help_row("\u{201C}cosa ho in sospeso?\u{201D} / \u{201C}what\u{2019}s pending?\u{201D}",
                "Review pending proposals, applications awaiting confirmation, and still-revertable emergences."))
        }
        p style="margin:.9rem 0 0;font-size:.78rem;color:var(--text-dim);line-height:1.45" {
            "Tip: the badge in the top bar lights up when you have something in "
            "flight — clicking it opens the chat on exactly those items."
        }
    }
}

/// One labelled example row inside the Help body — the spoken phrasing in
/// phosphor, the effect underneath in dim text.
fn help_row(example: &str, effect: &str) -> Markup {
    html! {
        li style="border:1px solid var(--border);border-radius:.45rem;padding:.55rem .7rem;background:var(--bg-2)" {
            div style="font-size:.82rem;color:var(--p);font-weight:600" { (example) }
            div style="font-size:.78rem;color:var(--text-dim);margin-top:.15rem;line-height:1.4" { (effect) }
        }
    }
}

/// Render the canonical error page used by
/// [`crate::error::DashboardError::into_response`].
#[must_use]
pub fn error_page(status: StatusCode, message: &str) -> String {
    let title = format!(
        "{} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    let body = html! {
        section class="error border border-rose bg-bg-1 rounded p-6 mt-4" {
            p class="status text-2xl font-bold text-rose m-0" { (title) }
            p class="message text-text mt-2 mb-0" { (message) }
            p class="mt-4 mb-0" {
                a href="/dashboard/" class="text-phosphor hover:text-phosphor-bright" { "Back to dashboard home" }
            }
        }
    };
    render_page(&title, &body)
}
