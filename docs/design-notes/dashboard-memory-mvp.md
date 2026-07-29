---
title: Dashboard memory MVP — wiki explorer + proposals + chat
area: design-notes
status: implemented
last_review: "2026-07-26"
---

# Dashboard memory MVP

Alongside the identity console
([`dashboard.md`](dashboard.md)), the dashboard exposes the
**memory-side** surface every operator + every authenticated user
needs at runtime:

- a list of wikis with their active-fact counts;
- a read-only single-wiki view (`index.md` + page list + meta), with an
  admin-only **export** link that downloads the wiki subtree as a
  portable full-marker tar
  ([`mwe_core::export`](../../crates/mwe-core/src/export.rs) — the
  marker forms are in [`marker-grammar.md`](marker-grammar.md) §0);
- structural proposals — **operated from the chat, not a form** (see
  [`dashboard.md`](dashboard.md) and the *Proposal action routes* note
  below). In
  [`routes/proposals.rs`](../../crates/mwe-dashboard/src/routes/proposals.rs)
  the action routes + the `open-in-chat` bridge are wired end-to-end
  through the [`proposals`](../../crates/mwe-core/src/proposals.rs)
  chassis; reviewing / applying / confirming / reverting happens in the
  chat via the `structure_proposal_*` agentic tools;
- a chat page that posts a natural-language message through
  [`wiki_ingest_message`](../../crates/mwe-core/src/ingest.rs) and
  renders the response inline;
- the **chat onnipresente** — a resizable right-side panel with
  `localStorage`-backed history and a `fetch`-based submit that appends
  fragments without page navigation. The panel is the single chokepoint
  for every LLM call from the dashboard — even the welcome wizard primer
  enters the system through it.

## Routes added

| Route | Handler | Guard | Source |
|---|---|---|---|
| `GET  /dashboard/wiki` | list | session | [`routes/wiki_view.rs`](../../crates/mwe-dashboard/src/routes/wiki_view.rs) |
| `GET  /dashboard/wiki/:id` | view | session | same |
| `GET  /dashboard/wiki/:id/export` | subtree export (`.tar` attachment) | session + admin | same |
| `GET  /dashboard/wiki/:id/delete` | strong-confirmation page (type-the-id) | session + admin | same |
| `POST /dashboard/wiki/:id/delete` | recoverable subtree soft-delete (tombstone facts + move to `trash/`) | session + admin | same |
| `POST /dashboard/proposals/:id/apply` | apply action (→ 303 to `/dashboard/chat`) | session | [`routes/proposals.rs`](../../crates/mwe-dashboard/src/routes/proposals.rs) |
| `POST /dashboard/proposals/:id/confirm` | confirm action (→ 303 to `/dashboard/chat`) | session | same |
| `POST /dashboard/proposals/:id/revert` | revert action (→ 303 to `/dashboard/chat`) | session | same |
| `GET  /dashboard/proposals/:id/open-in-chat` | primer landing (review/apply, or modify/undo for emergences) | session | same |
| `GET  /dashboard/chat` | form | session | [`routes/chat.rs`](../../crates/mwe-dashboard/src/routes/chat.rs) |
| `POST /dashboard/chat` | submit | session | same |
| `GET  /dashboard/facts/sections` | read-only listing of the smart-wiki section index | session (per-wiki ACL) | [`routes/sections_view.rs`](../../crates/mwe-dashboard/src/routes/sections_view.rs) |

The home page (`/dashboard/home`) gained four KPI tiles (wiki count,
active facts, pending proposals, MCP calls in last 24 h) plus a
"Memory" actions list so the new pages are discoverable without
reading the nav.

## State extension

The identity handlers only need `pool` / `secret` / `blacklist`. The
memory handlers also need the wiki tree, an embedder, and the LLM
configuration — those live in a
[`MemoryHandles`](../../crates/mwe-dashboard/src/state.rs). The
`llm_config` field is **not** a flat `LlmConfig`: it is wrapped in an
`Arc<RwLock<…>>` and accompanied by two sibling handles so the admin
LLM-config editor can hot-reload the slots and API keys
in-process without a restart (see
[`admin-llm-config.md`](admin-llm-config.md) for the full editor
flow):

```rust
pub struct MemoryHandles {
    pub tree: WikiTree,
    pub embedder: Arc<dyn Embedder>,
    // Hot-reloadable slot config — swapped in place by the admin
    // LLM-config editor; every cloned MemoryHandles sees the new
    // values on the next read-lock.
    pub llm_config: Arc<RwLock<LlmConfig>>,
    // In-memory API-key overrides, populated by the dashboard
    // set-API-key handler. Read before `std::env::var` so a rotated
    // key takes effect on the next request without `set_var` (which
    // is unsafe under `forbid_unsafe_code`).
    pub api_key_overrides: Arc<RwLock<HashMap<String, String>>>,
    // Optional per-slot backend overrides — the seam tests use to
    // plant a `FakeLlmBackend`; always empty in production.
    pub llm_overrides: LlmBackendOverrides,
    pub workdir: PathBuf,
}
```

Handlers do not touch `llm_config` directly: they go through
[`MemoryHandles::backend_for`](../../crates/mwe-dashboard/src/state.rs)
(prefers an injected `llm_overrides` slot, otherwise takes a short
read-lock, clones the slot config, and builds a backend with the
API-key override precedence baked in) and `defaults_for` for the
per-slot sampling knobs. The admin save path swaps the whole config
via `replace_llm_config` under a write-lock.

`DashboardState::memory` is `Option<MemoryHandles>`. Tests that don't
care about memory keep using `DashboardState::new(...)`; the memory
routes return a clear "memory handles not wired —
start with `mwe-mcp serve`" error when the operator (or a test) calls
them without populating the bundle. The production path always wires
it through
[`bootstrap_state`](../../crates/mwe-mcp-server/src/main.rs), which
leaves `llm_overrides` empty so the live `LlmConfig` does the work.

This keeps the dashboard's identity integration tests untouched while
the memory surface gets its own fixture.

## Chat panel

The persistent chat lives in
[`ui::layout::chat_panel`](../../crates/mwe-dashboard/src/ui/layout.rs).
It is rendered by the shell on
every authenticated page — same instance, same history — as a fixed
right-side aside:

- `position: fixed; right: 0; top: 0; height: 100vh; width: 380px`
  default; resizable in `[280, 720]` via a drag handle on the left
  edge; the width is persisted in `localStorage.mwe-mcp.chat.width`.
- The body of the authenticated page carries a `has-chat-panel`
  class that applies a matching `padding-right` so the main content
  does not slide underneath.
- On viewports `<= 720px` (mobile) the panel is hidden via a media
  query and the body padding resets; the operator falls back to the
  full `/dashboard/chat` page. A mobile toggle is not yet implemented.

### Routes

| Verb | Path | Behaviour |
|---|---|---|
| `GET`  | `/dashboard/chat` | Stand-alone chat page. The page itself only carries a heading + instructions; the real conversation lives in the right-side panel (hydrated client-side from `localStorage`). |
| `POST` | `/dashboard/chat` | Single user turn. Content-negotiated: `Accept: application/json` returns `{ user_text, response_html }` (200) or `{ error }` (400 on validation); anything else returns the full HTML page with the response inline as a no-JavaScript fallback. |

### `chat::process_submission` is the chokepoint

[`process_submission`](../../crates/mwe-dashboard/src/routes/chat.rs)
is the single entry point any dashboard route uses to talk to the
LLM. The chat handler is a thin wrapper over it; the welcome wizard
([`routes/welcome.rs`](../../crates/mwe-dashboard/src/routes/welcome.rs))
calls it directly with the primer message it composes from the form
fields. There is no second path. If logging, rate limiting, or prompt
adjustments are added to `process_submission`, every caller gets them
for free.

`process_submission` materialises the `ingest` backend per-request via
`LlmConfig::slot(Ingest).build_backend(...)` so config edits land
without restart (same convention as the MCP transport's
[`tools::call_wiki_ingest_message`](../../crates/mwe-mcp-server/src/mcp/tools.rs)).
A missing slot is surfaced as `DashboardError::Validation` (422 +
italian message), preserving the wizard's UX contract.

### History lives in the browser, not on the server, not in the model context

The panel persists turns to `localStorage.mwe-mcp.chat.history` (FIFO,
trimmed at 100 entries by `chat.js`). This is purely the user's
scrollback — to see what was said yesterday, what the engine answered,
etc.

The history is **not** persisted server-side and **not** passed back
into the model's context window. `recent_messages` on `IngestRequest`
stays `Vec::new()` on every turn. The engine's continuity is provided
by two native primitives instead of by replaying transcripts:

- **Autocapture**: every turn passes through `wiki_ingest_message`,
  which classifies and emits `wiki_capture` calls; the facts the user
  utters land in their wiki on the spot.
- **Recall**: every subsequent turn runs `wiki_recall` on the new
  text, retrieving whatever past facts (from the chat or from anywhere
  else in the wiki) are relevant *to this turn*.

This is structurally different from a generic LLM chat. The trade-off
is intentional: keep contexts small,
scale to local workhorse models, never lose info to a sliding window,
and stay coherent with the product promise ("memoria di lungo periodo,
non buffer di sessione").

### Welcome wizard as primer source

The welcome wizard composes the form fields into a first-person
italian prose blob (`compose_ingest_message` in `routes/welcome.rs`)
and, on Save, calls `chat::process_submission(state, user, primer)`.
The handler then renders a small landing page whose only meaningful
element is an inline `<script>window.__mweChatPrimer = {…};</script>`.
On its first run `chat.js` consumes that global exactly once, appends
to `localStorage`, renders the turn in the panel, and scrolls down.
End-to-end server-side: no auto-submit JS, no visual flash, no extra
network call at page-load.

### `chat.js`

A single vanilla ~180-line script at
[`crates/mwe-dashboard/assets/chat.js`](../../crates/mwe-dashboard/assets/chat.js),
served by `rust-embed` like every other asset. Loaded with `defer`
from the shell so the body is parsed before hydration runs. No HTMX.

### Ollama `think: false`

Independent but adjacent decision in the same session. Ollama 0.24
attivates `<think>` blocks by default on thinking-capable models
(Qwen 3.x). With our structured prompts that swallowed `num_predict`
inside reasoning and returned an empty `response`, breaking the
ingest path. `OllamaBackend` now pins `think: false` on every
generate request body. Non-thinking models ignore the flag. A panel
that would *show* the thinking to the user is not implemented; "off"
is the only coherent setting today.

## Wiki view

`GET /dashboard/wiki/:id`:

1. Parses the path segment as a [`WikiId`](../../crates/mwe-core/src/types.rs).
2. Locates the wiki via
   [`wiki_get_meta`](../../crates/mwe-core/src/wiki.rs) (returns 404
   on `WikiNotFound`).
3. Renders meta (id, title, type, slug, active-fact count,
   `scope`). A wiki carrying the `_meta.md` `is_agent` marker gets an
   **`agent` badge** next to its type here and in both list tabs (Standard
   and Smart), because the type alone hides the distinction: an agent's own
   wiki is a `wiki-user` exactly like a person's. The user list makes the
   same call on the identity side — an `is_agent` row reads role `agent`
   with the status "consumer agent (no login by design)", instead of a
   `user` whose status ("no credentials, no invitation") is equally true of
   a human waiting to accept an invite.
4. Reads `index.md` if present, **strips the testata**
   (`MarkdownDoc::parse(raw).map_or(raw, |d| d.body)` — so the frontmatter
   card's owner-tier `keywords`/`description` never leak, exactly as
   `wiki_read` / the recall navigator do; the structured fields come from
   `meta`, step 3), projects the body through
   [`render::render_for_sender_segments`](../../crates/mwe-core/src/render.rs)
   (the segment-emitting sibling of `render_for_sender` — identical joined
   text, each shown DB-known region tagged with its fact id; see
   [redaction-policy.md](redaction-policy.md)) with the connected session
   user's `(sender_id, sender_groups)` over the page's **active** ACL map
   (`fact_index::page_acl_map_active` — a retired region still on disk
   redacts fail-closed, same as `wiki_read`; only the
   [admin reveal](redaction-policy.md#dashboard-admin-reveal) loads the
   full map), then renders the declassified text to **HTML** via
   [`md_render::render_page`](../../crates/mwe-dashboard/src/md_render.rs)
   and emits it `PreEscaped` inside a `section.wiki-page-view.prose`.
   The markdown→HTML preview is shipped (no `<pre>` fallback): every
   heading gets `id="<slug>"` matching the comment-anchor grammar, and
   raw HTML in the source is dropped (defense in depth). When the
   projection redacted at least one region a muted "Showing the
   declassified view for &lt;sender&gt; — N region(s) replaced by
   `[redacted]`" banner is rendered above the body.
5. Lists every page returned by
   [`wiki_list_pages`](../../crates/mwe-core/src/wiki.rs) with their
   workdir-relative path and byte size — **view links only**, no
   discoverable raw-edit affordance.

**Wikilink click-through.** The rendered body (this preview, the per-page
view below, and the fact record's body) linkifies wikilinks
([recall-pipeline.md §Link grammar](recall-pipeline.md#link-grammar)) into
in-dashboard navigation: `[[wiki_id]]` → that wiki's view, `[[wiki_id/page-slug]]`
→ that page's view, a `|display` alias renders as the label, and a legacy
bare slug resolves as a same-named page over the whole tree — current wiki,
ancestors, sub-wikis, then id order; wiki id wins on a tie (the grammar's
emit-canonical / resolve-legacy fallback). Resolution is
tree-backed (`wiki_view::resolve_wikilink_href` over a `wiki_id → dir` walk
index): the wiki must exist and a page hop's file must exist, else the link
stays **literal prose — never a broken link** (the dead-rail posture; the
underscored mutant grammar falls here). Linkification happens strictly
**after redaction** (only declassified text reaches the renderer), labels are
emitted as text events (pulldown escapes them — an alias cannot inject HTML)
and hrefs are percent-encoded per segment. The chat-reply render keeps no
link context — `[[…]]` stays literal there.

**Relative markdown links.** A regular `[label](concepts/page.md)` link in
wiki prose is rewritten at render time to the canonical page-view route
(`wiki_view::resolve_relative_page_href` via the `PageRenderContext`'s
`resolve_md_link` hook): the destination is resolved against the rendering
page's wiki-relative directory (`.`/`..` normalized), case-insensitively
against the disk (the href carries the on-disk spelling), with a `#fragment`
riding along. Without the rewrite the wiki home — which serves at
`/dashboard/wiki/:id`, not under `/view/` — would hand the raw relative href
to the browser, which resolves it against `/dashboard/wiki/` into a dead URL.
Anything that is not a wiki-relative `.md` target stays exactly as authored:
absolute paths, scheme'd URLs, bare `#anchors`, query-carrying or non-`.md`
targets, `..` escapes above the wiki root, and targets whose file does not
exist (the wikilink resolver's dead-rail posture — never invent a broken
href). The fact record's body render resolves against its wiki's root; the
chat-reply render has no context and never rewrites.

**Region → source-fact click-through.** Each fact region the viewer can read
carries a small superscript anchor (`§`, `sup.fact-ref`) at the region's end,
linking to that fact's **record** — `/dashboard/facts/:fact_id/edit`, the
per-fact view with the canonical text, owner/sender, validity, provenance and
the structured actions — so a reader who spots something wrong in the prose
is one click from the surface that corrects it. Mechanics: the page view
walks the segmented render and appends a `{{factref=<id>}}` marker to each
fact-backed segment (`wiki_view::annotate_fact_refs`); `md_render` rewrites
the marker into the anchor. Gating is alignment, not a new gate: a segment
carries an id only when the region is **shown** and its key is in the page's
ACL map, and the target route's own gate
([`load_visible_fact`](../../crates/mwe-dashboard/src/routes/facts.rs)) is
the same `can_read` on the same row — so the anchor never leads a signed-in
reader to a 404/403. A redacted region is fact-less filler: no anchor, no id
leak. Under the **admin reveal** every shown region (retired residue
included, via the full map) carries its anchor, and `load_visible_fact`'s
reveal bypass makes the record reachable. Consumer MCP prose is untouched —
the segments variant is dashboard-only.

**Manual free-text wiki editing is forbidden / discouraged** (the
blessed write channels are inline **comments**, the **operative chat**,
and the **structured fact actions** above). The raw page editor
(`/dashboard/wiki/:id/edit/*path` in
[`routes/wiki_view.rs`](../../crates/mwe-dashboard/src/routes/wiki_view.rs))
is gated by `enforce_raw_editor_allowed`: **hard-forbidden on smart
wikis** (the smart consumer is the sole writer — surfaced as a `404` so
the editor is not even discoverable), **admin-only on standard wikis**
(non-admins get a `403`), with the owner check still on top. There is no
link to it from the normal page view; the per-page read view footer
points at the blessed channels instead. The inline-comment path
(`comment_form` / `submit_comment`) is fully intact — it is a blessed
channel open to anyone who can read the wiki.

**The page «what goes here» editor** (`/dashboard/wiki/:id/describe/*path`)
is a far narrower surface than the raw editor: it edits *only* the page's
one-line testata `description` — the hand-authored card field that guides
fact placement (the planner shows it to other pages) and labels the page in
recall navigation — preserving the body and every sibling frontmatter field
(`mwe_core::meta_annotate::set_page_description`; REM never overwrites it).
The gate is **standard-wiki + owner-OR-admin** (lighter than the raw
editor's admin-AND-owner: a placement hint, not content), and the per-page
read view links to it (`✎ Edit «what goes here»`) only for callers who may
edit.

**Deleting a wiki** is an admin-only action — the only manual structural
mutation the dashboard exposes (creation / reorg still ride the consumer /
REM). It is a three-step flow: a `delete` link in the wiki list
(`GET /dashboard/wiki`, hidden on identity wikis) → a strong-confirmation
page (`GET /dashboard/wiki/:id/delete`: the operator must re-type the id,
and it shows the blast radius — sub-wiki count + active facts) →
`POST /dashboard/wiki/:id/delete`. The apply calls
[`wiki_delete::delete_wiki_subtree`](../../crates/mwe-core/src/wiki_delete.rs),
which always **moves** the directory subtree into `<workdir>/trash/` — a
sibling of `wikis/`, so a trashed subtree never reappears in
`WikiTree::walk` — never an `rm -rf`.

The **files** are therefore always recoverable; what happens to the **facts**
is the operator's choice on the confirmation page, and it is the part that
is not symmetrical, because putting a trashed directory back does *not*
revive tombstoned rows (a standard wiki's pages are renders — the reindex
leaves a marker whose row is inactive alone, as stale render residue).
Hence three dispositions, ordered by what they destroy:

| Disposition | Form value | What it does |
|---|---|---|
| **Dissolve** *(default)* | `dissolve` | Destroys the structure, keeps every fact. Nothing is tombstoned: each fact moves to a live wiki (`page::dissolve_home` — its sender's home, else its owner's, else the deleter's) and the dissolved wiki's plan slugs are parked as `reopen_pages`, so the next Cartografo build **re-decides where each fact belongs** corpus-wide rather than letting it inherit the page it sat on. The evacuation target is a waiting room, not the answer. Only a fact with no live home anywhere is tombstoned — and it is **counted** in `facts_tombstoned`. |
| **Return to each author** | `authors` | The `SenderKeyed` arm: a fact the deleter sent is tombstoned; a foreign-authored one is evacuated intact to its sender's home wiki (owner as fallback). A fact whose sender and owner both lack a home is tombstoned. |
| **Tombstone all** | `tombstone` | Tombstones every fact regardless of sender, destroying others' contributions. |

A dissolve that actually freed facts then kicks off a **background full
reorg** (`dream::spawn_dream(DreamKind::Full)`) so the Cartografo re-places
them while the operator is still looking, instead of the memory sitting
lopsided until the nightly cycle. A busy REM gate is not an error: the park
is persisted on the plan, so whichever build runs next consumes it.

**Identity wikis (`wiki-user` / `wiki-group`) are refused**: they are an
account's autobiographical store, removed through the user / group deletion
flow instead.

The redaction wiring projects the page for the connected user. Effective
`scope` is resolved via
[`WikiTree::resolve_scope_principal`](../../crates/mwe-core/src/wiki.rs) so
sub-wikis declared with `scope: inherit` walk up the parent
chain. `sender_groups` is populated via
[`enrollment::groups_for`](../../crates/mwe-core/src/enrollment.rs),
so the dashboard projection matches the `wiki_read` MCP output
byte-for-byte for the same user (the joined segments *are* that text; the
`{{factref=…}}` annotation is added only on the markdown handed to the
dashboard's own HTML renderer, never on a consumer surface).

## Proposal action routes

Proposals are reviewed, applied, confirmed,
and reverted **from the chat** (see [`dashboard.md`](dashboard.md) and
[`agentic-chat.md`](agentic-chat.md)); there is no GET
`/dashboard/proposals` list or pending-confirms tray. In
[`routes/proposals.rs`](../../crates/mwe-dashboard/src/routes/proposals.rs)
the surface is the **action routes** + the **open-in-chat bridge**:

- POST `:id/apply` / `:id/confirm` / `:id/revert` call
  [`proposals::apply_proposal`](../../crates/mwe-core/src/proposals.rs) /
  `confirm_proposal` / `revert_proposal` directly (the `revert` handler
  stays status-aware: `applied` → `RevertAuth::Token` fetched
  server-side, `applied_pending_confirm` → `RevertAuth::Caller`), and
  each **303-redirects to
  `/dashboard/chat`** on both success and classified error.
- GET `:id/open-in-chat` composes a server-side primer, runs it through
  the agentic loop, and lands the operator inside the chat with the
  proposal already summarised — a *review/apply* primer for the pending
  questionnaire.

The admin "DO REM NOW" trigger is
the Dream console (`/dashboard/dream`, see [`dashboard.md`](dashboard.md)),
running one synchronous [`rem::run_cycle`](../../crates/mwe-core/src/rem.rs)
against the live workdir (the only place REM can be triggered while
`serve` holds the workdir lock).

## Tests

[`crates/mwe-dashboard/tests/wiki_explorer.rs`](../../crates/mwe-dashboard/tests/wiki_explorer.rs)
is the integration suite (the file is the roster). It covers the wiki
list / view, the chat panel, the home memory section, and the
**proposal action routes** as redirects:
each POST `apply` / `confirm` / `revert` asserts the **303 to
`/dashboard/chat`** plus the resulting DB / on-disk state (no form HTML),
and the `open-in-chat` bridge has a review/apply primer test for a pending
proposal. The nav tests assert the Proposals tab is
**absent** (the chat panel, rendered on every authenticated page,
replaces it).

The dashboard integration fixture plants a deterministic
`FakeLlmBackend` per slot via the `MemoryHandles.llm_overrides` seam,
so it drives the agentic chat end-to-end without spinning up Ollama.

## The chat panel is operative, not Q&A

The chat panel is the single LLM chokepoint for the dashboard, and its
*role* is deliberately narrow: it is an
**operative tool on the memory itself**, not a Q&A system. It composes
`_internal.*` operations of `mwe-core` through an in-process agentic
loop (Ollama function calling, `hub_writer` slot) and is the place
where structural intents that don't have a dedicated page land —
proposal questionnaires (`wiki_promote`, `dedup_merge`, `bundle`),
batch ops on facts, hierarchical
wiki moves. What the chat does **not** do: conversational recall
(that's the consumer agent's job — mwe-mcp exists to *serve* their
recall, not duplicate it), users/groups/tokens CRUD (admin pages have
those), or routing to existing pages.

### Agentic loop and tool surface

The agentic loop runs through `chat::agentic_submission` with an
iteration cap (`MAX_AGENTIC_ITERATIONS = 8`) and a transparent
tool-call trace in the panel, backed by `LlmBackend::chat` /
`OllamaBackend` `POST /api/chat` and the
`ChatRequest`/`ChatResponse`/`Tool`/`ToolCall` types. Endpoint:
`POST /dashboard/chat/agentic`. The tool surface:

- **Read tools**: `wiki_recall`, `wiki_list_pages`, `wiki_get_meta`,
  `structure_proposal_list`, `structure_proposal_get`,
  `wiki_facts_for` (ACL-filtered filters).
- **Write tools** (each gated in the system prompt by an
  explicit-confirmation rule): `structure_proposal_apply`,
  `structure_proposal_revert`, `structure_proposal_confirm`,
  `wiki_forget` (tombstone), `wiki_supersede`, `wiki_change_scope`,
  `wiki_move_fact` (relocate one fact same-wiki or cross-wiki).
  *(The current set is canonical in [agentic-chat.md](agentic-chat.md).)*

`structure_proposal_apply` is wired into the chassis;
`AgenticContext.hub_writer` is threaded through to `apply_proposal` for
kinds that need an LLM at apply time. `wiki_supersede` inherits owner /
ACL / `fact_type` / topics from the targeted fact, so the chat only has
to surface a `fact_id` (via `wiki_recall`) plus the corrected body; the
dispatcher refuses already-superseded / already-tombstoned rows with a
typed error so the loop never chains a supersede onto a zombie.

`/dashboard/proposals` rows carry an "Apri in chat" affordance via
`GET /dashboard/proposals/:id/open-in-chat`, which runs the agentic
loop server-side with a primer and injects the resulting turn into the
panel's `localStorage` via `window.__mweChatPrimer`.

### Hierarchical wiki move

`mwe_core::scope::wiki_change_scope` is the composite primitive behind
the chat's wiki-move tool: it validates cycle/self/inherit-root before
any filesystem mutation, then `fs::rename`s, rewrites
`_meta.md.parent_wiki_id`, syncs parent `children` lists, and rebases
`fact_index.source_path` via `fact_index::rebase_source_path_prefix`.
`wiki_id` stays stable per the memory-model invariant, so existing
`[[wiki_id]]` cross-links keep resolving. An applicative WAL and an
ACL-widening warning on this path are not yet implemented (planned —
see the roadmap).

### The memory browser — two tabs, one per corpus

The engine indexes two different things, so the browser has two tabs, the
same way the wiki explorer splits Standard from Smart
(`sections_view::corpus_tabs`, rendered at the top of both pages):

| Tab | Route | Table | Posture |
|---|---|---|---|
| **Facts** | `/dashboard/facts` | `fact_index` | The governed memory of standard wikis. Editable: ACL, validity, supersede, forget. |
| **Sections** | `/dashboard/facts/sections` | `wiki_sections` | What was indexed from smart-wiki pages. **Read-only.** |

Each row lives under exactly one tab: a smart wiki has no `fact_index`
rows, and a standard wiki has no sections.

### Facts browser

`GET /dashboard/facts` is a paginated browser (filters on `wiki_id` /
`fact_type` / `topic` / `created_after` / `created_before`, 1-based
pagination with `page_size` default 50, cap 100). ACL projection
mirrors `wiki_facts_full_for` via `SenderContext::user(&user.sender_id)` —
so by default an admin sees only the facts they can read, **not** every
user's. The [admin-reveal lens](redaction-policy.md#dashboard-admin-reveal)
lifts that: with reveal on, the list (and `load_visible_fact`) pass
`reveal = true` to skip the per-row ACL gate, so the owner-or-admin
actions below can reach another user's fact. Per-row "wiki" + "modifica" +
"elimina" actions. "modifica" targets
`GET /dashboard/facts/:fact_id/edit`; "elimina" is a `confirm()`-gated
inline `POST /dashboard/facts/:fact_id/delete` that forgets the fact via
[`capture::wiki_forget`](../../crates/mwe-core/src/capture.rs) — the
`deleted_at` tombstone plus the best-effort excision of the region's
on-disk bytes ([redaction-policy](redaction-policy.md)) — (reloads through
`load_visible_fact`, enforces **sender-OR-admin**, reason
`dashboard_delete`) so it leaves recall at once and survives as a
tombstone. Both appear only on active promoted rows.

It renders as a **compact, wide data-grid** (`.facts-table.compact`)
that surfaces every column of the row — ACL (`owner_id` / `sender_id` /
`allow_ids`), classification (`fact_type` / `salience` / `topics` /
`style` / `page_description`), validity (`valid_from` / `valid_to` /
`decay_reason`), recall signals (`last_recall_at` / `recall_count_30d`),
provenance (`source_ref` / `authored_refs`), and lifecycle
(`superseded_*` / `successor` / `deleted_*`) — minus the placement hints
(`source_path` / `target_page`) and the un-renderable embedding. `fact_id` shows
abbreviated and copies the full id on click (`ui.js`, `data-fact-id`);
timestamps render without fractional seconds or timezone (`fmt_ts`). The
shared `.table-wrap` (added by `ui.js`) gives the grid horizontal scroll,
so the many columns stay swipeable on mobile.

Because it carries every column, the page fetches the **full**
`FactIndexRow` (via `recall::wiki_facts_full_for`) rather than the slim
`RecallHit` projection, and unifies it with `BufferedCapture` into a
local `FactRow`. Scalar columns are **sortable**: their headers are links
(`FactSortKey` whitelist → SQL `ORDER BY`, injection-safe; `salience`
ranks `high < normal < low` via a `CASE`) that flip ↑/↓ and reset to
page 1. An **"includi inattivi"** toggle (`FactFilters::include_inactive`)
drops the default tombstone exclusion so superseded / deleted rows show,
badged `superato` / `eliminato`; those, like fresh rows, omit the
"modifica" / "elimina" actions.

The page also reads **un-promoted captures** still in `capture_buffer`
(which the light dream has not consolidated yet) via
`recall::wiki_buffered_full_for` — the same ACL projection and filter set,
but no semantic ranking (it is a list, not a search, so it needs no
embedder). Those buffered rows lead the listing, carry an **`in
consolidamento`** badge (`badge-fresh`), and drop the "modifica" /
"elimina" actions — there is no `fact_index` row to edit or tombstone by
`fact_id` until promotion.
Pagination's next-page estimate keys off the promoted count alone; the
fresh prefix is small and capped, so it never spans pages. This mirrors
the consumer-side mid-range bridge (`recall_fresh_captures`): a claim is
recallable the moment it is captured, and now visible in the dashboard
from that moment too.

### Sections browser

`GET /dashboard/facts/sections`
([`sections_view`](../../crates/mwe-dashboard/src/routes/sections_view.rs))
is the smart-family half: the read-only listing of `wiki_sections` — what
the engine chunked and embedded out of the pages a smart consumer
authored. Filters on `wiki_id` / page-path substring / text substring
(operator text is `LIKE`-escaped, so a literal `%` searches for itself);
same 1-based pagination and `MAX_SCAN_ROWS` lower-bound total as the
Facts tab. The listing query
([`sections::browse`](../../crates/mwe-core/src/sections.rs)) deliberately
leaves the **embedding** in the DB: at ~4 KB a row it is useless to a
table and would make a page of sections megabytes of float.

Columns are the ones that mean something for a chunk of a document: wiki,
page (deep-linked to `/dashboard/wiki/:id/view/*path` — the actionable
surface), position on the page, heading chain, a truncated preview, and
the recall signals (`recall_count_30d` / `last_recall_at`).

**Read-only by construction, not by policy.** A section has no lifecycle
to act on: it is re-derived from its page on every reindex, so there is
nothing to supersede, forget, re-own or date. The operator's lever is the
page — edit it (or have the smart consumer push it) and the index
follows.

Access is resolved **per wiki**, from the `smart_wikis` registry (owner ∪
`_meta.shared_with`), the same set `recall::search_sections` uses — a
section is readable because its *wiki* is. The admin-reveal lens lifts it
exactly as on the Facts tab. An account with no readable smart wiki gets
an explicit empty state rather than a bare empty table.

The edit page (`GET /dashboard/facts/:fact_id/edit`) opens on the fact's
**record** — a reading-width page (the `authenticated_reading_page`
layout) whose summary grid (`section.meta`) surfaces the placement
(`wiki_id`), all three ACL axes (`owner` subject / `sender` provenance /
`allow` audience), taxonomy (`topics` / `fact_type`), the validity
bounds, the `successor` pointer when a closure stamped one (linked to the
successor fact's own record — one click from the obituary to the current
truth), `created`, and — for document-extracted facts — the `source_ref`
citation, linked to the ACL-gated media alias when it is a catalog id (a
cited document must be viewable/downloadable; a URL or legacy ref stays
text). The canonical text renders below it as **prose**
(`md_render::render_page`): markdown, media embeds, and wikilink
click-through resolved against the fact's own wiki (see §Wikilink
click-through) — it is the landing surface of the `§` region anchors, so
it reads like the page the reader came from, not like a raw dump.

The edit form carries **three** surfaces, split by how they apply:

- **ACL** (`POST /facts/:id/acl`) and **validity**
  (`POST /facts/:id/validity`) are **structured, engine-direct** actions
  — plain `<form method=post>` + 303-redirect, no chat. Each handler
  reloads the fact (`load_visible_fact`), enforces **owner-OR-admin**
  (`owner_id == user:<sender>` or `is_admin` → else 403) and
  **standard-wikis-only** (a smart wiki's ACL / validity is wiki-level,
  not per-fragment — refused with 422 pointing at the consumer / sharing
  page), then calls the act-first wrapper in
  [`mwe_core::operator_edits`](../../crates/mwe-core/src/operator_edits.rs)
  (`acl_change_operator` / `validity_edit_operator`). Those wrappers
  write the engine column (promoted `fact_index` row first, still-buffered
  capture as fallback), compute `acl::widens` + a `disclosure_audit` row
  for an ACL change, and mint **one born-applied `wiki_promote` receipt**
  (variant `acl_change` / `validity_edit`) — the same receipt the chat
  ingest verbs mint. The route posts a `structure_applied` notice event
  and 303-redirects to `/dashboard/proposals/:id/open-in-chat`, so the
  operator lands on the **revertible** receipt. Revert is free: the
  receipt flows through the existing `POST /dashboard/proposals/:id/revert`
  route (the chassis dispatches the `wiki_promote` variant to
  `promote::revert_wiki_promote`).
- **Moving a fact** is **not** a mechanical picker on this form. The
  operator *comments* the intent on the fact (e.g. "questo starebbe meglio
  sulla wiki salute") or says it in the **operative chat**, and the internal
  LLM performs the relocation: the [comment-apply](rem-cycle.md#briefing-processor-non-smart-sub-job)
  `move` op (REM, batched) and the operative-chat `wiki_move_fact` tool both
  reuse the same engine (`promote::apply_paragraph_to_file_direct` for a
  same-wiki page→page move, `promote::apply_fact_refile_direct` for a
  cross-wiki move onto the destination wiki's `index.md`), each minting a
  born-applied + revertible receipt. ACL / validity stay structured because
  no LLM judgment is involved; a move follows an operator's stated *intent*,
  so it rides the intent-driven channels, never a `<select>`.
- The **body / topics / `fact_type` supersede**
  (`POST /facts/:id/edit/submit`) still rides the deterministic
  **form-to-chat bridge**: it composes the edit instruction and routes
  the write through the agentic loop with a HARD-RULE explicit
  confirmation. ACL / validity left this bridge because no chat tool
  applied them deterministically.

## Current limitations

- **`_internal.wiki_forge`** — the *concrete* wiki creation given a
  type — is not yet implemented in `mwe-core`. Until it lands, the
  natural-language flow "vorrei una wiki per i libri" cannot close
  end-to-end with a real wiki row.
- **Batch fact move between wikis** is not implemented: it would need a
  dedicated `mwe-core` primitive (re-emit in the target wiki with fresh
  marker + post-move ACL projection). `wiki_change_scope` moves the
  whole wiki and covers the common case.
- **Rate limiting on the chat endpoint** is not enforced: the JWT
  `rate_limit_id` claim is parsed-but-not-enforced today, pending
  tower-governor wiring.
- **Server-side / model-context chat history** is not retained — history
  lives in `localStorage` only, and there is no HTMX partial-update
  layer.
- **A dedicated per-user "modify wiki X" questionnaire UI** is not
  built; structural intents are handled through the operative chat
  instead.
