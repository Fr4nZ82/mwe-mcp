---
title: Dashboard — frontend stack (HTML, CSS, JS)
area: design-notes
status: implemented
last_review: "2026-07-03"
---

# Dashboard — frontend stack

Sibling reference to [`dashboard.md`](dashboard.md): that page covers
routing, auth, and the server-side architecture; this page covers the
**user-facing HTML / CSS / JS layer** as it ships in
[`crates/mwe-dashboard/`](../../crates/mwe-dashboard/).

The dashboard renders with **phosphor-terminal aesthetics**: deep-
green/teal palette on near-black backgrounds, JetBrains Mono
monospace throughout, a glowing wordmark, an SVG knowledge-graph
mark, and corner-tick panel framing.

## Stack

| Layer | Implementation | File |
|---|---|---|
| Server-side HTML | [Maud](https://maud.lambda.xyz/) macro (compile-time templating in Rust) | [`src/ui/layout.rs`](../../crates/mwe-dashboard/src/ui/layout.rs) + every `src/routes/*.rs` |
| Component helpers | Maud helper functions for flash banners, form inputs, submit buttons | [`src/ui/components.rs`](../../crates/mwe-dashboard/src/ui/components.rs) |
| CSS engine | [Tailwind v4](https://tailwindcss.com) — standalone CLI, no Node toolchain, no `package.json`, no JS config file | [`tailwind/app.css`](../../tailwind/app.css) + [`tailwind/tokens.css`](../../tailwind/tokens.css) |
| Built CSS | Single embedded stylesheet, ~29 KB minified | [`assets/tailwind.css`](../../crates/mwe-dashboard/assets/tailwind.css) (committed) |
| Client-side JS | Vanilla files, IIFE-wrapped, no framework, no modules. Two are loaded on every authenticated page — `ui.js` (shell toggles) + `chat.js` (chat content); the rest are page-scoped progressive enhancements a single route appends (`llm-config.js` for the LLM admin page, `tokens.js` for the token issue form, `recall-trace.js` — the one **ES module**, see below — for the trace replay viewer) and degrade cleanly when absent | [`assets/ui.js`](../../crates/mwe-dashboard/assets/ui.js) + [`assets/chat.js`](../../crates/mwe-dashboard/assets/chat.js) + [`assets/llm-config.js`](../../crates/mwe-dashboard/assets/llm-config.js) + [`assets/tokens.js`](../../crates/mwe-dashboard/assets/tokens.js) + [`assets/recall-trace.js`](../../crates/mwe-dashboard/assets/recall-trace.js) |
| Asset serving | [`rust-embed`](https://docs.rs/rust-embed) embeds `assets/` at compile time, axum route serves them | [`src/assets.rs`](../../crates/mwe-dashboard/src/assets.rs) |
| Fonts | JetBrains Mono Regular + Bold + ExtraBold (WOFF2, self-hosted, SIL OFL 1.1) | [`assets/fonts/`](../../crates/mwe-dashboard/assets/fonts/) |
| 3D engine | [Three.js](https://threejs.org) **vendored and pinned** (0.185.1, MIT — `three.LICENSE.txt`), the repo's only third-party JS: `three.module.min.js` + its `three.core.min.js` split, imported relatively by `recall-trace.js` — self-hosted like the fonts, no CDN | [`assets/three.module.min.js`](../../crates/mwe-dashboard/assets/three.module.min.js) + `three.core.min.js` |
| Logo | SVG mark (3 variants: colour, mono `currentColor`, lockup with wordmark) | [`assets/mwe-mark.svg`](../../crates/mwe-dashboard/assets/mwe-mark.svg), `mwe-mark-mono.svg`, `mwe-logo.svg` |

Decisions that shape the stack:

- **Self-hosted fonts, not Google CDN.** Lines up with the PWA-
  offline-capable product direction: the dashboard never needs to
  reach an external font server, and
  the operator's air-gapped lab still renders correctly. Three
  weights cover every current need; VT323 (the CRT-display font in
  the design pack) is not loaded because the optional scanline
  aesthetic was dropped from v1 for sobriety.
- **Tailwind v4, not v3.** v4 expresses the design pack's
  `tailwind.theme.js` mapping in pure CSS via `@theme { … }`,
  eliminating the JS config file entirely. Class names and
  semantics are unchanged for callers (`text-phosphor`,
  `bg-bg-1`, `shadow-glow` etc.); only the configuration syntax
  shifts. The v4 standalone CLI is a single ~115 MB binary, no
  npm.
- **Tokens drive everything.** [`tailwind/tokens.css`](../../tailwind/tokens.css)
  defines the phosphor palette, the radius, the glow, the fonts.
  Every rule downstream (the `@theme` block, the `@layer
  components` styles, the per-page Tailwind utilities used inside
  Maud) references these tokens. Changing a value in one place
  re-colours the entire dashboard with no class edits.

## Asset pipeline

```
tailwind/
├── tokens.css   ── design tokens + legacy aliases
└── app.css      ── entry point: @import tokens + tailwindcss,
                    @source crates/mwe-dashboard/src/**/*.rs,
                    @theme mappings, @font-face, @layer base +
                    @layer components

         │  (tailwindcss CLI scans .rs files for utility classes,
         │   emits preflight + base + components + the utilities
         │   actually used)
         ▼
crates/mwe-dashboard/assets/tailwind.css   (committed, ~29 KB minified)

crates/mwe-dashboard/assets/
├── tailwind.css                ── rust_embed ──▶  /dashboard/static/tailwind.css
├── ui.js                       ── rust_embed ──▶  /dashboard/static/ui.js
├── chat.js                     ── rust_embed ──▶  /dashboard/static/chat.js
├── llm-config.js               ── rust_embed ──▶  /dashboard/static/llm-config.js   (page-scoped)
├── tokens.js                   ── rust_embed ──▶  /dashboard/static/tokens.js       (page-scoped)
├── recall-trace.js             ── rust_embed ──▶  /dashboard/static/recall-trace.js (page-scoped ES module)
├── three.module.min.js         ── rust_embed ──▶  /dashboard/static/three.module.min.js (vendored, pinned)
├── three.core.min.js           ── rust_embed ──▶  /dashboard/static/three.core.min.js   (its internal split)
├── three.LICENSE.txt           ── MIT attribution for the vendored Three.js
├── mwe-mark.svg                ── rust_embed ──▶  /dashboard/static/mwe-mark.svg
├── mwe-mark-mono.svg           ── rust_embed ──▶  /dashboard/static/mwe-mark-mono.svg
├── mwe-logo.svg                ── rust_embed ──▶  /dashboard/static/mwe-logo.svg
└── fonts/
    ├── JetBrainsMono-Regular.woff2
    ├── JetBrainsMono-Bold.woff2
    ├── JetBrainsMono-ExtraBold.woff2
    └── OFL.txt                 (SIL OFL 1.1, attribution)
```

The built `tailwind.css` is **committed to the repo** because
`rust-embed` pulls it in at compile time and Cargo cannot invoke
the standalone tailwindcss CLI itself. To rebuild after editing
`tailwind/*.css` or after introducing a new utility class in a
Maud template, run the command documented in
[`wiki/development/build-run.md`](../development/build-run.md)
under *Dashboard assets (Tailwind v4)*.

## Page anatomy

Every page goes through one of three shell helpers in
[`layout.rs`](../../crates/mwe-dashboard/src/ui/layout.rs):

- `anonymous_page(title, body)` — single-form pre-auth pages: login,
  setup wizard, invitation accept, password recovery, 2FA, the OAuth
  approve step. No top nav, no chat panel, no `has-chat-panel` body
  class. Its sibling `anonymous_reading_page(title, body)` renders the
  *informational* pre-auth pages — the front splash and the bridges
  catalog / per-consumer guides — at the wider `reading-main` column
  instead of the narrow login-form width.
- `authenticated_page(title, &user, body)` — everything behind the
  session middleware. Adds the top nav (admin-gated rows when
  `user.is_admin`), a "Signed in as …" badge, the logout form,
  the chat panel, the floating reopen FAB, and the JS shell
  scripts.
- `render_page(title, body)` — error pages and a few ad-hoc payloads
  with no session context.

DOM tree of an authenticated page:

```
<html>
  <head>
    <title>… · mwe-mcp</title>
    <link rel="icon" type="image/svg+xml" href="/dashboard/static/mwe-mark.svg">
    <link rel="stylesheet" href="/dashboard/static/tailwind.css">
    <script src="/dashboard/static/ui.js" defer></script>
    <script src="/dashboard/static/chat.js" defer></script>
  </head>
  <body class="has-chat-panel chat-open">          ← .chat-open set by ui.js
    <header class="site-header sticky … bg-bg-2">
      <a class="brand …" href="/dashboard/">
        <img src="…mwe-mark.svg" class="h-7 w-7">
        <span class="wordmark …">mwe<span class="dim">-mcp</span></span>
      </a>
      <span class="tagline">dashboard</span>
      <button id="nav-toggle" class="md:hidden …">≡</button>     ← mobile hamburger
      <nav class="site-nav md:flex …" id="site-nav">
        … the regular nav links (+ the admin-gated ones when is_admin) …
      </nav>
      <span class="session-badge hidden md:inline …">Signed in as <strong>…</strong></span>
      <form class="logout-form" action="/dashboard/logout">…</form>
    </header>
    <main class="w-full app-main mx-auto px-4 md:px-6 lg:px-8 mt-6 md:mt-8">
      <h1>{title}</h1>
      {body}                                            ← per-page Maud, uses
                                                          legacy class names
                                                          (.flash, .kpi,
                                                          .config-table) styled
                                                          via @layer components
    </main>
    <footer class="site-footer …">mwe-mcp {VERSION}</footer>
    <aside class="chat-panel fixed top-0 right-0 …" id="chat-panel">
      <div class="chat-panel-resize-handle …" id="chat-panel-resize"/>  ← chat.js drag
      <header class="chat-panel-header …">
        <h2>Chat</h2>
        <button id="chat-close" aria-label="Close chat panel">×</button>
      </header>
      <p class="chat-panel-hint …">…</p>
      <div class="chat-panel-messages …" id="chat-panel-messages"/>     ← chat.js hydration
      <form class="chat-panel-form" id="chat-panel-form">
        <textarea id="chat-panel-text" …/>
        <button type="submit">Send</button>
      </form>
    </aside>
    <button id="chat-reopen" class="fixed bottom-4 right-4 …" aria-label="Open chat panel">▢</button>
  </body>
</html>
```

## Responsive contract

Two breakpoints carry the layout, both Tailwind defaults:

- **`md` (768 px)** — below this, the nav collapses behind a
  hamburger button. The button (`#nav-toggle`) adds `.nav-open` on
  the nav element; the `@layer components` rule reveals it as a
  stacked column underneath the topbar. The session badge is
  hidden on mobile to keep the topbar single-row.
- **`xl` (1280 px)** — at or above this, the chat panel docks as
  a fixed right-edge sidebar (380 px wide) and the body reserves
  `padding-right: 380px` so page content does not slide under it.
  Below `xl`, the panel hides and the floating reopen button
  (`#chat-reopen`) takes its place; the user pops the panel as an
  overlay on demand.

State machine driven by **two body classes**:

| Class | Set by | Meaning |
|---|---|---|
| `has-chat-panel` | server (Maud, when `user.is_some()`) | the panel and FAB exist in DOM |
| `chat-open` | `ui.js` (from `localStorage.mwe-mcp.chat.open` + viewport default) | user has the panel open (or wants it open by default) |

`ui.js` reads `localStorage.mwe-mcp.chat.open` on load: `'1'` ⇒
open, `'0'` ⇒ closed, missing ⇒ default to "open" on viewports
≥ 1280 px and "closed" otherwise. Close button + reopen FAB
flip the class and persist the choice.

The shell handles four bug families: trailing-slash 404, missing
favicon, missing input autocomplete, and three structural UX
concerns (chat panel dismissable, layout stable below ~1230 px,
mobile hamburger present).

### Content width

`main` is not hard-capped to a narrow column: by default it uses the
width up to **`.app-main`** (`max-width: 2200px`) so wide (2k / 4k)
screens are not left with a band of dead space. (`main` carries the
`.app-main` **class**, not a `max-w-[…]` utility, so the centered
variants below can override it — a Tailwind utility would win over an
`@layer components` rule by layer order regardless of specificity.)
Readability and "fit" are managed at the **element level** instead, so
individual pieces never sprawl:

- **Inputs** cap their natural width (`input/select` at `32rem`,
  numeric inputs at `7rem`); `.input-wide` and `textarea.prompt-editor`
  opt out for fields that want the full column.
- **`.field-grid`** flex-wraps a form's fields into columns on a wide
  screen and collapses to one column on mobile. Mobile fields grow to
  fill the row (`flex: 1 1 17rem`); from `md` up they switch to a fixed
  `flex: 0 1 18rem` so a handful of short filter fields keep a natural
  width and pack left instead of fanning out to fill a 4k row.
  `.field-narrow` shrinks a short-value cell, `.field-wide` forces a
  full-row item. Used by the facts filter and the welcome primer's
  identity grid.
- **`.table-wrap`** (wrapped around every `main table` by `ui.js`) is
  the table's own scroll viewport on **both** axes, with
  `max-height: min(72vh, 760px)`: a long table scrolls inside its
  frame instead of pushing the page down, and a short one shows no
  scrollbar. The visual frame (border + rounded corners) lives on the
  wrapper and the wrapped table is `overflow: visible`, so the sticky
  header (`thead th { position: sticky; top: 0 }`) pins to the wrapper
  while the body scrolls — a table with `overflow: hidden` of its own
  would capture the sticky and the header would scroll away with it.
- **Prose** measure is capped (top-level intro paragraphs and the
  `.wiki-page-view.prose` reader) so copy stays readable on a wide
  `main`.
- A plain single-column **`form`** caps at `45rem` so it hugs its
  content; a form that carries a data layout — a `<table>`, a
  `.field-grid`, or a `.card-grid` — opts back out to full width via
  `form:has(table), form:has(.field-grid), form:has(.card-grid)`
  (modern-browser `:has()`).
- Card grids fill the width instead of stacking in the left column:
  the home navigation lists (`.home-sections`, auto-fit) and the
  LLM-config provider + role cards (`.card-grid`, a fixed **two**
  columns from `lg` up — never the third narrow column an auto-fit
  grid would pack onto a 4k screen). Both collapse to one column on
  mobile.
- **Centered columns for narrow surfaces.** A page whose content *is* a
  narrow column should not pin to the left of a wide screen, so two body
  classes cap `main` narrower (it is already `mx-auto`, so capping
  centers the whole column — heading and body together):
  `body.anon-shell` (`30rem`) for the no-nav single-form pages (login,
  setup, invite, password recovery, 2FA) via [`anonymous_page`](../../crates/mwe-dashboard/src/ui/layout.rs);
  `body.reading-main` (`52rem`) for the reading / single-form surfaces —
  authenticated ones via `authenticated_reading_page` (a wiki page, the
  page viewer, edit/comment/describe forms, help, the Settings + 2FA
  pages, the new/edit user and group forms) and the **anonymous
  informational** pages via `anonymous_reading_page` (the front splash,
  the bridges catalog / guides — content pages, not forms, so the 30rem
  login width would cramp the table and the `mcp add` command blocks).
  Data pages (tables, config grids, dashboards, and the **list** pages —
  wikis, users, groups) keep the wide `authenticated_page`.

## Inline media embeds

The wiki page preview renders `{{embed=<catalog_id>}}` markers as
`<img>` / `<video controls>` / `<audio controls>` / download-link
elements (kind dispatched from the id; code blocks stay literal).
Source HTML is dropped; the raw HTML `md_render` emits itself is
limited to these media elements, the heading anchors, and — on the page
surfaces — the wikilink `<a class="wikilink">` navigation plus the
`sup.fact-ref` region → fact-record anchors
([dashboard-memory-mvp §Wiki view](dashboard-memory-mvp.md#wiki-view)),
all code-built. The media elements point at the
cookie-authenticated `/dashboard/media/<id>` alias and are styled by
the `.wiki-embed` / `.wiki-embed-link` rules in the
`.wiki-page-view.prose` block of `tailwind/app.css`; the wikilink and
fact-ref anchors ride the prose block's default link styling (no
dedicated CSS — `sup` keeps them small and unobtrusive). A blob the
viewer may not read shows as a broken image — the per-media ACL fires
at byte time ([media pipeline](media-pipeline.md)).

## CSS architecture

[`tailwind/app.css`](../../tailwind/app.css) is the entry point.
Its sections, top to bottom:

1. `@import "./tokens.css"` — design tokens + legacy aliases.
2. `@import "tailwindcss"` — v4 preflight + utility engine.
3. `@source "../crates/mwe-dashboard/src/**/*.rs"` — explicit
   scan target for Oxide so it picks up the utility classes
   used inside Maud HTML macros.
4. `@theme { … }` — maps tokens onto Tailwind class names.
   `text-phosphor`, `bg-bg-1`, `border-border`, `shadow-glow`,
   `font-mono`, `rounded` are all produced here.
5. `@font-face { … }` — three weights of self-hosted JetBrains
   Mono pointing at `/dashboard/static/fonts/*.woff2`.
6. `@layer base { … }` — body radial-gradient background, font,
   headings, links, code.
7. `@layer components { … }` — three sub-groups:
   - **Design pack pieces**: `.term-panel` (corner-tick frame),
     `.wordmark` (glowing lockup).
   - **Shell state machines**: mobile-nav toggle (`nav.site-nav`
     hidden below md, `.nav-open` override), chat-panel state
     (`body.chat-open .chat-panel` shows, `#chat-reopen` swaps
     in when absent), `padding-right: 380px` on xl+.
   - **Per-page body rules**: the element styles (`.flash`, `.kpi`,
     `.config-table`, `.wiki-page`, `.comment-block`, etc.), with
     `var(--bg-elev)` etc. resolved through the compatibility
     aliases in [`tokens.css`](../../tailwind/tokens.css) and inline
     hex colours mapped onto phosphor equivalents. This is what
     styles every per-page body element without forcing a per-route
     Maud rewrite.

All styling lives in the single built `tailwind.css`. The aliases
in `tokens.css` (`--bg-elev: var(--bg-1)`, `--fg: var(--text)`,
`--accent: var(--p)`, …) are a compat shim so ported rules and any
markup that still references the old names keep rendering
correctly. Prefer the phosphor names (`--bg-1`, `--text`, `--p`)
for new code.

## Component helpers

[`components.rs`](../../crates/mwe-dashboard/src/ui/components.rs)
ships six Maud helpers reused across routes:

| Helper | Use |
|---|---|
| `flash(kind, message)` | top-of-page banner; `kind` ∈ `error \| success \| info` — styled via the three `.flash-*` rules in components layer |
| `text_field(name, label, kind, value, required)` | generic labeled `<input>` with `type ∈ text \| password \| email`; falls back to `text` |
| `password_field(name, label, autocomplete)` | password-specific: always required, never pre-fills, takes explicit WHATWG `autocomplete` token (`current-password \| new-password \| off`) |
| `text_area(name, label, value, required)` | labeled `<textarea>`, pre-fill escaped |
| `submit(label)` | primary submit button wrapper |
| `destructive_form(action, label, confirm)` | inline form with `onsubmit=confirm(…)`, used for delete / revoke / logout |

These helpers do not (yet) emit Tailwind utility classes — they
emit the legacy class names (`.flash flash-error`, …) that the
ported `@layer components` rules style. Per-page Maud bodies that
want bespoke layout reach for Tailwind utilities directly
(`class="grid grid-cols-2 gap-3"` etc.).

## JS architecture

Two IIFE-wrapped vanilla files, loaded with `defer` on every
authenticated page:

- [`chat.js`](../../crates/mwe-dashboard/assets/chat.js) — chat
  *content* (the bubbles in `#chat-panel-messages`). Responsibilities:
  hydrate from `localStorage.mwe-mcp.chat.history` (FIFO, max 100
  turns), intercept the form submit + POST to
  `/dashboard/chat/agentic` with `Accept: application/json` and
  render the `AgenticTurn` (tool-call trace + the final reply, the
  latter shown as server-rendered Markdown HTML — see
  [`agentic-chat.md`](agentic-chat.md)), drag-resize from the
  left handle (width persisted under
  `localStorage.mwe-mcp.chat.width`, clamped to `[280, 720]`),
  consume the one-shot welcome-wizard primer planted on
  `window.__mweChatPrimer`. See [`agentic-chat.md`](agentic-chat.md)
  for the wire contract.
- [`ui.js`](../../crates/mwe-dashboard/assets/ui.js) — shell
  *toggles* plus a few fetch-driven affordances, deliberately kept
  apart from chat content: the mobile hamburger (`#nav-toggle` flips
  `.nav-open` on `#site-nav`, updating `aria-expanded`); the chat
  open/close (`body.chat-open` with `localStorage.mwe-mcp.chat.open`
  persistence + the viewport-default fallback from *Responsive
  contract*); the Help modal and the Dream **run-log** modal open-close
  (a row's `log` button fetches `GET /dashboard/dream/runs/:id` into
  `#dream-log-body`); the in-flight badge fetch; and the **Dream
  background runs** — all three triggers (Light / Compile / Full) are
  POSTed with `Accept: application/json`, then an animated `dream…`
  topnav pill (`#dream-indicator`) polls `GET /dashboard/dream/status`;
  on completion the console page reloads to reveal the new history row
  (elsewhere it shows the one-line outcome, click to dismiss) and it
  resumes the pill on load if a dream is still running. No-JS users get
  the synchronous full-page report for all three.

Conversation continuity is *not* achieved by replaying past turns
to the model — the engine relies on `wiki_recall` + autocapture.
The history `chat.js` keeps is purely the user's visual scrollback.

One page-scoped file breaks the IIFE convention on purpose:
[`recall-trace.js`](../../crates/mwe-dashboard/assets/recall-trace.js), the
**animated 3D replay** of a recall trace (admin Traces viewer —
[dashboard.md](dashboard.md)). It is an **ES module** because it imports the
vendored Three.js relative to its own URL (`./three.module.min.js`, which in
turn pulls `./three.core.min.js`) — no import map, no CDN, PWA/air-gapped
friendly. It fetches the trace JSON from the viewer's `/data` endpoint and
replays the route as a guided scene — the turn, the similarity hits glowing
on their page cards (canvas-texture cards in the phosphor palette, brightness
from the score), the entry-point fan arranged by weight (seed families
colour-accented), the navigator orb (sphere + eye) opening pages hop by hop
with the prose streaming in and discovered links sprouting new cards, then
the injected yield — with transport controls (play/pause, speed, step,
restart), a caption bar fed by the navigator's own per-hop `note`, drag-to-
orbit, and click-to-inspect. Honest degradation: no JS / no WebGL / fetch
failure removes the stage and leaves the page's full textual trace; reduced
motion starts paused.

## Test surface

Integration tests in
[`crates/mwe-dashboard/tests/`](../../crates/mwe-dashboard/tests/)
exercise the *served HTML* (string match on class names, ids,
link hrefs) and the server-side behaviour, but not anything that
depends on a real layout engine — they use
`tower::ServiceExt::oneshot` against the router with a
`tempfile::TempDir` workdir, no TCP listener.

All 43 assertions pass because the semantic class names and ids
the tests rely on (`#chat-panel`, `#chat-panel-form`,
`.has-chat-panel`, `.chat-panel-resize`, …) coexist in the markup
alongside the Tailwind utility classes.

What is intentionally **not** tested today: TLS, real listener
binding, browser-side service worker, accessibility (a11y),
Lighthouse PWA score, visual regression. The Playwright MCP loop
for audit + restyle verification is operator-driven, not committed
to the repo. A baseline visual-regression test (one screenshot per
breakpoint, perceptual diff) would catch what `cargo test` cannot.

## What is intentionally still pending

- **A real CRT toggle** in user settings to opt into the scanline
  overlay (CSS is staged in `tokens.css` as `.crt::before`, VT323
  not yet loaded).
- **A11y pass** — axe-core sweep, keyboard navigation audit,
  focus-ring visibility on the phosphor palette.
- **Light theme** — tokens are parametric, adding a
  `:root.light { … }` alternative palette would be additive. Not
  implemented today.
- **Visual regression test** — see *Test surface* above.
- **PWA polish** — Web App Manifest, service worker, installable
  icon are not implemented today (planned — see the
  roadmap).
