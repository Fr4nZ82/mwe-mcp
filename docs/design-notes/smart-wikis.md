---
title: Smart-wikis — smart consumers authoritatively managing their own memory wikis
area: design-notes
status: partial
last_review: "2026-07-26"
---

# Smart-wikis

Smart-wikis are the memory wikis a **smart consumer** writes
authoritatively through the family-H `wiki_admin_*` MCP tools. The
concept and the identity / ACL model behind them live in
[memory-model.md](../concepts/memory-model.md) and
[identity-and-acl.md](../concepts/identity-and-acl.md). A smart wiki
is marked by a single per-wiki `smart: true` bool in its `_meta.md`
(standard wikis simply omit the key; `companion:` — the family's
pre-rename name — is accepted as a read alias forever and migrates to
`smart:` on the first rewrite); this page is the runtime SSOT for what
the engine does with that marker.

## The problem

mwe-mcp is an online server that several consumer agents per operator
talk to:

- **Standard consumers** route every conversation turn through
  `wiki_ingest_message`, which calls the server's `ingest` LLM slot
  to classify intent and write into the memory wiki. Openclaw on
  Telegram, hermes, nanoclaw, the dashboard chat — all standard.
- **Smart consumers** bring their own LLM subscription (Claude Code,
  Cowork, Codex, custom MCP-compatible agents). They already pay to
  classify the user's message; running their captures through the
  server's `ingest` slot would be a *double bill*. Worse, smart
  consumers often own a project (`~/projects/lnprint/`) whose docs
  the consumer wants to maintain in mwe-mcp as **its own memory wiki**
  — and `wiki_ingest_message`'s LLM round-trip per page is the wrong
  shape for bulk doc maintenance. (That double-bill argument is about
  **bulk doc maintenance**, *not* per-turn conversational memory: a
  smart consumer's **conversation** still runs `wiki_ingest_message` —
  the superset path, see §4.)

mwe-mcp provides a parallel write path — the **family H**
MCP tools (`wiki_admin_*`) — exclusively for smart consumers managing
their own memory wikis, with a marker on the target wiki type that
keeps the server-side pipeline (REM, `wiki_ingest_message`) from
trying to "help".

The wikis those tools write to are called **smart wikis**.

## Architectural pillars

### 1. `consumer_class` JWT claim

The JWT carries an optional claim:

```json
"consumer_class": "smart" | "standard"
```

Default is `standard`. Tokens without the claim parse as standard via
`#[serde(default)]`, and standard tokens omit the claim on the
wire via `skip_serializing_if = "ConsumerClass::is_standard"` — the
wire format is backward-compatible byte-for-byte.

What it gates:

- Family H write tools (`wiki_admin_push` / `wiki_admin_pull`) refuse
  standard tokens with `403 requires_consumer_class_smart`.
- `wiki_admin_notify` is not class-*denied* outright, but is
  governed by the `consumer_class × wiki_family` matrix (§3,
  `wiki_admin_notify`): a standard openclaw can relay a user
  observation into a smart consumer's companion `_briefing.md`; a
  smart consumer is told to administer its own companion directly
  instead of notifying itself; a standard consumer is redirected to
  `wiki_ingest_message` for standard wikis.

The dashboard's `/dashboard/tokens/issue` form ships a "Smart
consumer" checkbox that flips the bit and demands a `consumer_id`
(the smart device label like `cc-laptop`). The CLI has `mwe-mcp
token-issue --class smart`. See
[`jwt-and-session-model.md` §Consumer class](jwt-and-session-model.md#consumer-class--the-smart-vs-standard-gate)
for the wire detail.

### 2. `smart: true` marker in `_meta.md`

A smart wiki is marked by a single per-wiki **`smart: bool`**
field in its `_meta.md` frontmatter (the
[`WikiMeta.smart`](../../crates/mwe-core/src/wiki.rs) field; `companion:`
is the pre-rename read alias). It is the **single source of truth** for
"is this smart?" — every module that branches on it reads the flag
directly from `_meta.md` (or through the cycle-scoped index map REM
builds once per run by walking the tree, see §7). There is no registry
and no template: the flag is authored straight onto the wiki at creation
time. `smart: true` is stamped by `wiki_admin::push_create` from the
**explicit `smart` flag** the smart consumer passes on create (it decides
to forge a smart wiki — importing a previously-local wiki, or a new
project wiki on user request); every other wiki omits the key. The
`wiki_type` is a free-form tone/label and does **not** decide smart-ness.
**"Standard" simply means `!smart`.**

The smart actor wiki is **generic** — software docs, research notes,
recipe books, runbooks — so it ships no fixed folder structure: the smart
consumer is authoritative on the on-disk shape (default ACL `owner_user`,
no REM hub regeneration underneath it). Smart *content* shape is the
smart consumer's to organise.

**Recall indexing — its own tables, content-indexed and markerless.** A
smart wiki carries **no per-fragment `{{f=…}}` markers and no
per-fragment ACL**: the consumer writes plain markdown (move / rename /
add / remove pages, exactly as the engineering wiki of *this* repo is
maintained) and recall indexes the *content*. That content does **not**
live in `fact_index` — it has its own pair of tables
([`mwe_core::sections`](../../crates/mwe-core/src/sections.rs), migration
`0062`):

- **`wiki_sections`** — one row per heading-delimited section, keyed by
  `(source_path, section_ord)`. No owner, no sender, no allow list, no
  supersedence, no tombstone, no validity window: a section is a chunk of
  a document, not a governed claim, and it exists exactly as long as its
  page does.
- **`smart_wikis`** — a queryable projection of each smart wiki's
  `_meta.md`: resolved owner, `shared_with`, `project_id`, `wiki_type`,
  and `description` (migration `0067`). The file stays the source of
  truth; the table exists so the engine can ask *in SQL* which wikis are
  smart, who may read them, and what each project is about.

  **`description` is the project's door sign**, and it is read from the
  wiki's own `_meta.scope` via
  [`WikiMeta::door_description`](../../crates/mwe-core/src/wiki.rs) —
  no second field. `scope` already means *"prose description of this
  container — what goes in here"*; on a standard wiki that prose is the
  classifier's placement signal, and a smart wiki is never a placement
  target (it is filtered out of the router window), so the field is free
  here and the two readings cannot collide.

  It is populated for **project** wikis only. An agent's operational
  notebook is a smart wiki too and is nobody's door — it holds one
  agent's working notes rather than a subject anyone would ask about —
  so `door_description` declines on **either** marker, `is_agent` or
  `wiki_type: agent`, because production carries a wiki with the type
  and no on-disk flag. `NULL` is a legitimate state, not a defect: an
  undescribed project is simply not offered as a door, and the column
  makes that gap *visible* where the mechanism it replaces (a signpost
  fact somebody had to remember to write) left no trace at all when it
  was skipped.

  Any path that rewrites a registry row must carry the description
  through — the dashboard's sharing editor rebuilds the whole row, and
  dropping it would blank a project's door until the next reindex sweep.

  **The column is not where recall reads it from.** A standard consumer's
  per-turn recall reads the *fact* corpus only, so a door that lived just
  in a column would be invisible to the ranking that fills the block.
  [`signposts::project_descriptions`](../../crates/mwe-core/src/signposts.rs)
  therefore mirrors each description onto the owner's `projects.md` as an
  ordinary signpost fact, on every full sweep, right after the registry
  refresh. Idempotent: a write happens only where the text actually
  moved, an unchanged description is a no-op, and withdrawing the line
  from `_meta.md` **retires** the fact rather than leaving a door open
  onto a project that stopped describing itself.

  So the description exists in two places and cannot diverge, because only
  one of them is ever written by hand: `_meta.md` is authored, the column
  and the fact are both derived from it.
- **`wiki_sections_fts`** (migration `0065`) — an FTS5 index over the
  section text and its heading chain, maintained by triggers and
  regenerable at any time. It is what lets a project wiki be searched by
  the tokens project wikis are actually written with — `D-006`, an ADR
  number, a ticket id, a symbol from a stack trace — which an embedding
  has almost nothing to encode. Recall fuses the two rankings; see
  [recall-pipeline.md](recall-pipeline.md#the-section-corpus-is-ranked-by-two-passes-fused).

The split is what makes wiki-level ACL actually wiki-level: read access
is stored **once per wiki** and resolved once per query
([`recall::search_sections`](../../crates/mwe-core/src/recall.rs) keeps
the readable wikis, then loads only their sections — an unreadable wiki's
bytes are never read). A `shared_with` edit is a **single-row** write
that closes the read window immediately; it used to re-stamp one row per
indexed section, over a thousand on a large project wiki.

It is also what makes the family filter honest: `wiki_search`'s
`scope.smart` now selects a **table before ranking** instead of
discarding hits after it, so the caller's `top_k` is honoured. And the
conversational path (`wiki_ingest_message`) recalls facts only —
structurally, because documentation is not in the table it reads — so
project docs can no longer crowd a personal turn's context.

A `wiki_admin_push` enqueues its touched pages onto the reindex queue and
acks immediately (embedding runs off the request path — a bulk import of
large pages must not hold the HTTP response past a proxy timeout); the
safety-net sweep is the backstop. See
[reindex-pipeline.md](reindex-pipeline.md#smart-wikis--indexing-on-push-queued). Per-fragment markers/ACL are the pillar of **standard** memory
wikis only (the founding ACL-per-fragment idea — see
[redaction-policy.md](redaction-policy.md)).

**The dashboard chat keeps its hands off.** Smart wikis are the
consumer's: every management verb of the dashboard chat panel
(`wiki_change_scope`, `wiki_move_fact`, `wiki_delete_page`,
`wiki_forget`, `wiki_supersede`, `wiki_request_forget`) refuses a smart
target, keying on this `smart` flag
([`agentic.rs`](../../crates/mwe-dashboard/src/agentic.rs)). Those
refusals are now belt-and-braces rather than the only line of defence:
those verbs act on `fact_index` rows, and a smart wiki has none — its
content is in `wiki_sections`, where supersede and forget do not exist.
The guards stay because they give the operator a clear message instead of
a silent no-op. The core
[`scope::wiki_change_scope`](../../crates/mwe-core/src/scope.rs)
primitive carries the same refusal for any caller: a smart wiki's
wiki-level read audience derives from its position in the tree (the
scope principal), so a re-parent would change effective read access on
the next reindex/push. The operator's touchpoint for a smart wiki is
its **briefing** (`wiki_admin_notify`, §3; the REM read-jobs, §7).

### 3. Family H — `wiki_admin_*` MCP tools

Their schemas are in
[`schemas.rs`](../../crates/mwe-mcp-server/src/mcp/schemas.rs); their
handlers in
[`tools.rs`](../../crates/mwe-mcp-server/src/mcp/tools.rs); the
business logic in
[`wiki_admin.rs`](../../crates/mwe-core/src/wiki_admin.rs) and
[`briefing.rs`](../../crates/mwe-core/src/briefing.rs). See
[mcp-tools.md](../protocol/mcp-tools.md)
for the user-facing roster (the count is derived from
`schemas::all_tools()` — see the page for the live list).

#### `wiki_admin_push` (MVP)

```
input: {
  wiki_id?         (required for upsert)
  project_id?      (required for create — stamped into _meta.md.extra)
  wiki_type?       (required for create — a free-form tone/label; does
                    NOT determine smart-ness)
  smart?           (create takes an explicit `smart: true` to forge a
                    smart wiki; default false)
  mode:            "create" | "upsert"
  pages: [{ path, content }]
  deletes:         [ "..." ]
  expected_op_log_head?   (optimistic concurrency, enforced on upsert)
}
output: { wiki_id, ops_applied, op_log_id, warnings }
```

**Auth gates (in order)**. `push` is parameterised
by `ActorKind { SmartConsumer | Dashboard | System }` — the matrix below
documents when each gate fires.

| `ActorKind`     | `consumer_class=smart`?      | owner-match? | `_meta.companion=true`?           |
|-----------------|------------------------------|--------------|---------------------------------------|
| `SmartConsumer` | yes (`requires_consumer_class_smart`) | yes (`wiki_owned_by_other_user`) | yes (`wiki_type_not_admin_writable`)    |
| `Dashboard`     | no (the dashboard session is the gate) | yes        | **relaxed** — any wiki the operator owns |
| `System`        | no — reserved for the revert handler's compensating row | n/a | no                                    |

1. `token.consumer_class == "smart"` → else `403 requires_consumer_class_smart`.
   Smart-consumer only.
2. `wiki.owner_user == token.owner_user` (derived from the wiki's
   `scope`; rejects `Group` / `Global` ACLs with
   `AmbiguousOwner`). MVP smart wikis require `scope =
   User(id)` — multi-owner smart wikis are deferred. → else
   `403 wiki_owned_by_other_user`. Applies to both smart-consumer and
   dashboard writes.
3. the wiki's `smart: true` flag, read straight from its
   `_meta.md`. → else
   `400 wiki_type_not_admin_writable`.
   **Smart-consumer only** — the dashboard textual
   editor (`/dashboard/wiki/:id/edit/*path`) bypasses this gate under
   the unified op-log scope, so the operator can fix
   pages in any non-smart wiki from the
   same editor surface that handles smart wikis.

**Page-path rules (both modes).** Every `pages[].path` is vetted by
`validate_push_pages` **before any disk write**: safe path
(`is_safe_page_path` — `[A-Za-z0-9._-]` components, uppercase welcome
so an imported vault keeps its casing byte-for-byte), no `_meta.md`, no
ASCII-case variant of a reserved filename (`_Meta.md`, `RULES.md`,
`_Briefing.md`) or of the `.md` extension
(`wiki::page_path_case_hazard`), and no two pages in one request whose
paths differ only by case. At write time, creating a page whose path
case-collides with an existing on-disk entry (`Index.md` vs `index.md`,
`Modules/` vs `modules/`) is refused with the existing spelling echoed
back (`wiki::page_case_conflict`) — the server disk is case-sensitive
but the consumer's local mirror usually is not, and two case-twin
entries would silently clobber each other on the next pull. Whether the
page "already exists" (upsert) or is new (and must pass the guard) is
decided by `wiki::page_exists_byte_exact`, which reads the directory
listing: a server running on macOS or Windows would otherwise take
`Path::exists`' word for it and overwrite `index.md` with a push aimed
at `Index.md`.

**`mode: create`**:
[`push_create`](../../crates/mwe-core/src/wiki_admin.rs) derives a
fresh `wiki_id` via `WikiId::child_of(parent, slug)`, stamps
`scope = User(caller.sender_id)` + `project_id` into
`_meta.md.extra`, writes every page via `atomic_write`, refuses
duplicates (404 if the wiki_id already exists is a sanity
backstop — the slug derivation should not collide). Page paths are
validated **before** `_meta.md` is forged, so a rejected request
leaves no half-made wiki directory on disk.

**`mode: upsert`**:
[`push_upsert`](../../crates/mwe-core/src/wiki_admin.rs) writes the
pages, deletes the listed paths, refuses any write to `_meta.md`
(reserved for `create`-time stamping; `scope` /
`shared_with` edits go through a separate dashboard route or the
sharing flow, §10).

**Audit**: every successful `push` writes one row in
`wiki_admin_op_log` (append-only, migration 0022 + extended in
0027) with `payload_hash` = sha256 of the canonical input
(`mode` + sorted `paths` + content hashes). Raw page content is
**never** stored in the audit table — the workdir is the SSOT.

The row carries two additional columns
exercised at insert time:

- **`actor_kind`** — `'smart_consumer' | 'dashboard' | 'system'`,
  populated from the `ActorKind` parameter passed by the caller (the
  MCP handler always pins `SmartConsumer`; the dashboard editor pins
  `Dashboard`). The op-log is the unified write log across all
  wiki families, not smart-wiki-only.
- **`pre_image_json`** — JSON `{ "pages": [{ "path", "content":
  "<body>" | null }, …] }` snapshot of the pages **just before** the
  write. Populated on `Upsert` (one entry per touched/deleted path);
  `null` `content` discriminates "did not exist" from "existed and
  was empty". `NULL` on `Create` rows (no pre-state) and on `pull`
  rows. The revert handler reads this column to roll back individual
  ops — see below.

#### `wiki_admin::op_revert`

A public mwe-core API that rolls back a single
[`wiki_admin_op_log`](../../migrations/0022_wiki_admin_op_log.sql)
row by restoring its `pre_image_json` to disk:

```rust
pub async fn op_revert(
    pool: &SqlitePool,
    tree: &WikiTree,
    op_id: i64,
    reverted_by: &str,
) -> Result<RevertOutcome, RevertError>;
```

Not exposed via MCP — revert / compensation surfaces are
dashboard-only. Dashboard surface: `POST
/dashboard/wiki/:id/op-log/:op_id/revert`, gated by the
[`AdminUser`](../../crates/mwe-dashboard/src/auth/session.rs)
extractor. The op-log view (`GET /dashboard/wiki/:id/op-log`)
renders a Revert button (via
[`components::destructive_form`](../../crates/mwe-dashboard/src/ui/components.rs))
only on revertable rows; non-revertable rows show a muted dash with
a tooltip describing the reason. Flash banners are passed back via
`?flash=<key>` redirects, mapped to hard-coded markup blocks
(`revert_ok` / `revert_conflict` / `revert_not_revertable` /
`revert_not_found` / `revert_failed`) to keep the URL surface
out of XSS / open-redirect range.

**Algorithm**:

1. Load the target row. Refuse with `RevertError::NotFound` if
   missing.
2. Refuse `NotRevertable` if `op_kind` is not `push_*` (no
   `pull` / `notify` row carries a pre-image) or if
   `actor_kind = 'system'` (chained revert-of-revert is performed by
   re-clicking the original target, not by reverting the
   compensation — and the UI hides the Revert button on system rows
   anyway).
3. Refuse `NoPreImage` if `pre_image_json IS NULL` (older rows without
   a captured pre-image, or `Create` rows — the wiki did not exist before).
4. **Conflict scan**:
   `SELECT op_id, pre_image_json FROM wiki_admin_op_log WHERE wiki_id = ?
    AND ts > ? AND op_kind LIKE 'push_%' ORDER BY op_id ASC`.
   For each newer row, intersect its touched paths (parsed from its
   own `pre_image_json`) with the target's paths. Rows whose
   `pre_image_json IS NULL` take the conservative branch — treat as
   touching every target page → conflict, refuse strictly. If any
   newer row intersects, refuse with
   `RevertError::TargetChanged { conflicting_ops, conflicting_pages }`
   → wire `409 op_log_target_changed_since`. The conflict policy is
   **strict**: no force, no "revert as new push annotated"; the
   operator falls back to either reverting the conflicting later op
   first, or pasting the pre-image content into the editor manually.
5. Snapshot the *post-state* of every page referenced by the target
   (what is on disk right now, just before we overwrite). This goes
   into the compensating row's `pre_image_json`, so a future revert
   of the compensation would restore the post-state — available as
   audit data even though the UI hides the button.
6. For each `(path, content)` in the target's `pre_image_json`:
   `content: Some(body)` → `atomic_write` the body; `content: None`
   → delete the file (idempotent if already absent). Defensive
   refuse on `_meta.md` paths even though push already blocks them.
7. INSERT the compensating row with `actor_kind = 'system'`,
   `op_kind = 'push_upsert'`, `op_mode = 'upsert'`,
   `sender_id = reverted_by`, `consumer_id = NULL`, and the
   post-state JSON in `pre_image_json`. Return the new `op_id` +
   list of restored paths.

**Wire mapping** (dashboard surface):

| `RevertError`                          | Status | Wire code                       | Flash banner          |
|----------------------------------------|--------|---------------------------------|-----------------------|
| `NotFound`                             | 404    | `op_not_found`                  | `revert_not_found`    |
| `NoPreImage` / `NotRevertable` / `MalformedPreImage` | 400 | `op_not_revertable` | `revert_not_revertable` |
| `TargetChanged { ops, pages }`         | 409    | `op_log_target_changed_since`   | `revert_conflict`     |
| `Db` / `Io` / `Internal`               | 500    | `internal_error`                | `revert_failed`       |
| `Ok(RevertOutcome { … })`              | 302    | redirect to op-log              | `revert_ok`           |

**What is intentionally NOT here**: force
revert; revert "as new push annotated" (kept strict until real use
shows the strict policy is too aggressive); chained
revert-of-revert chains in the UI (the button is hidden on system
rows by design); fact-index re-indexing inside `op_revert` itself
(the watcher pipeline picks up the restored files asynchronously;
`push` additionally enqueues its touched pages onto the reindex queue, see
[reindex-pipeline.md](reindex-pipeline.md#smart-wikis--indexing-on-push-queued)).

**Optimistic concurrency is enforced** on `upsert`: a push carrying
`expected_op_log_head` is rejected with `409 conflicting_op_log_head`
when a newer *write* op (`push_create` / `push_upsert`, including a
dashboard revert's compensation row) landed on the wiki since the caller
synced. Read ops (`pull` / `notify`) never trip it, so the caller's own
pulls stay safe; `None` keeps last-writer-wins.

**Not yet supported** (planned — see the
roadmap):
`snapshot_replace` mode (delete-all-then-write semantics for
filesystem-style sync), `since_op_log_id` delta-pull,
`project_id`-search filter on `wiki_search`,
`administered_by_self` filter.

#### `wiki_admin_push.mark_processed` — recepiment dal custode

This closes the inline-comment loop. Once the
smart consumer has read a `wiki_briefing_items` comment, addressed it
in code, and is about to push the fix, the **same push call** marks
the addressed comment(s) as recepiti. The argument is an optional
`mark_processed: Vec<String>` on `wiki_admin_push` carrying briefing
item ids in the canonical `bi_<N>` form (or bare `<N>` — the helper
[`briefing::parse_bi_id`](../../crates/mwe-core/src/briefing.rs)
accepts both shapes).

```text
input:  { mode, …, pages, deletes?, mark_processed?: [bi_<N>, …] }
output: { wiki_id, ops_applied, op_log_id, warnings, marked_processed: [bi_<N>, …] }
```

**Atomicity strategy.** The flow opens a single sqlx transaction
*before* the file writes so a `mark_processed` validation failure
aborts cleanly without leaving files on disk:

1. Pre-flight cap check (`MARK_PROCESSED_CAP_PER_PUSH = 50`, matching
   the per-wiki notify rate-limit cap) + per-string parse/dedup. An
   unparseable string immediately yields
   `AdminError::UnknownBriefingItemId { bi_id, wiki_id }`.
2. `let mut tx = pool.begin().await?` — every subsequent DB
   touch runs on `tx`.
3. `validate_and_mark_processed(&mut tx, …)` performs one
   `SELECT wiki_id FROM wiki_briefing_items WHERE id = ?` per id and
   fail-fasts on the first row that is missing or whose `wiki_id`
   does not match the push target. Dropping `tx` on the error path
   rolls back everything.
4. Page writes via the existing `write_pages` / deletes loop (file
   writes themselves are not transactional, but they happen only
   *after* validation passes, so the negative tests
   "no op-log row" + "page wasn't written" both hold).
5. Batched `UPDATE wiki_briefing_items SET processed_at = ? WHERE id
   IN (…) AND wiki_id = ?` — the `AND wiki_id` is defense-in-depth on
   top of the per-id SELECT (a race that swapped a row's `wiki_id`
   between SELECT and UPDATE would still refuse to flip it).
6. `record_op_log` is now generic over `sqlx::Executor`, so the same
   helper writes the audit row on `&mut *tx` instead of `&pool`.
7. `tx.commit().await?`.

The response carries `marked_processed: Vec<String>` (canonical
`bi_<N>` strings, sorted ascending, no duplicates) so the smart
consumer can echo the recepito back to the user.

**Wire errors.** Two classes in `mcp::error`:

- `400 unknown_briefing_item_id` — the id either does not exist or
  belongs to a different wiki. Payload includes the failing `bi_id`
  string so the caller can drop it and retry the rest.
- `400 too_many_briefing_items` — list size exceeds
  `MARK_PROCESSED_CAP_PER_PUSH`. The caller is expected to split
  the marks across multiple pushes.

**No special-casing on `ActorKind`.** The same pipeline runs for
`Dashboard` and `SmartConsumer` actors — a future dashboard write
that wanted to opportunistically mark items would already work
without further plumbing. The smart-consumer auth gates
(`consumer_class=smart`, owner-match, `smart: true` in `_meta.md`)
are unchanged: `mark_processed` is an *additional*
ability for the same caller, not a way to bypass anything. Clients
omitting the field work unchanged — the schema field is optional and
the default behaviour is an empty list and empty `marked_processed`.

#### `wiki_admin_pull` (MVP)

Dual of push: returns every page of the smart wiki + the current
`op_log_head` (UUIDv7 of the last `wiki_admin_op_log` row for the
wiki). Used by a smart consumer to reconstruct a missing local
`.mwe/wiki/` cache, or to realign after a token revoke/reissue
cycle. Same 3-gate auth.

`since_op_log_id` delta-pull is deferred — MVP returns the full
content every time. Acceptable because the typical smart wiki is
small (tens of pages, kilobytes) and pull is rare (session start +
recovery only).

#### `wiki_admin_notify` (MVP, full)

```
input:  { wiki_id, topic, body, source:{kind,ref}, kind?, ts? }
output: { briefing_item_id }
```

Appends a `## From <source>` section to `_briefing.md` of a
smart wiki. The file is created on-demand with a
`type: session_briefing` frontmatter; `last_updated` is bumped
in-place via `touch_last_updated`. A mirror row is inserted into
`wiki_briefing_items` (migration 0023) for indexed/processed-state
queries.

**Auth + the matrix gate**. Read access is the access
gate (owner_user, plus `shared_with` users via the sharing path, §10).
On top of that, the public MCP path runs
[`gate_notify_target_matrix`](../../crates/mwe-core/src/briefing.rs),
which crosses the caller's `consumer_class` with the target's
smart flag (from `_meta.md`):

| caller × target | outcome |
|---|---|
| **standard consumer × smart wiki** | `FullCompanion` — write the DB row **and** the `_briefing.md` section (the classic openclaw relay) |
| **smart consumer × standard wiki** | `NarrativeDbOnly` — write the DB row only; standard wikis own no `_briefing.md`, so the REM Briefing-processor sub-job drains the row next cycle |
| **smart consumer × smart wiki** | refused `403 smart_does_not_notify_own_wiki` — administer via `wiki_admin_push`, don't notify yourself |
| **standard consumer × standard wiki** | refused `403 standard_uses_ingest_for_memory` — use `wiki_ingest_message` (the workhorse 4-intent classifier) |

The REM-internal entry [`notify_as_rem`](../../crates/mwe-core/src/briefing.rs)
bypasses this matrix (it is a server actor with no `consumer_class`);
its only family check is the legacy `WikiTypeNotBriefingCapable`
guard, which still refuses a non-smart target with
`400 wiki_type_not_briefing_capable`.

**Validation**: `topic ≤200B`, `body ≤4KB`,
`kind ∈ {observation, reasoning, external}` (the three-layer
classification with semantic routing is in §8).

**Rate limit**: 50 notify/wiki/hour, enforced **before** INSERT via
`SELECT COUNT(*) FROM wiki_briefing_items WHERE wiki_id=? AND ts >
datetime('now', '-1 hour')`. Counting first means a buggy caller
that retries on a transient error can't bypass the cap.

#### `wiki_admin_signpost` (roadmap 48)

```
input:  { wiki_id, description?, activity?:{ day, text } }
output: { owner_wiki_id, page, description, activity, retired, active_days }
```

Writes **signposts** — short facts on the *owner's* reserved
`projects.md` saying that this project exists and what happened lately.
Logic in [`signposts.rs`](../../crates/mwe-core/src/signposts.rs).

Why the tool exists: a standard consumer's per-turn recall is
facts-only, so a project the user never names is invisible to their
everyday agent. A signpost is the dot that makes it visible — and when
one surfaces in a turn, recall opens that project's sections in the same
turn ([recall-pipeline](recall-pipeline.md)). It is a **pointer, not a
record**: what was done lives in the project wiki.

| Rule | Value |
|---|---|
| description | ≤400 chars, one per project, replaced on rewrite |
| activity | ≤250 chars, one per project per day, replaced on rewrite |
| window | the last 5 days; older lines are tombstoned on the next write |
| over the cap | **refused** with the measured length, never truncated |
| unchanged text | no-op — no write, no re-embed, same fact id |
| read access | mirrors the project's `shared_with` |

**Auth**: `consumer_class=smart` (checked in the handler), the caller
must own the project wiki, and the wiki must be smart — a standard wiki
needs no pointer, its facts are recalled directly.

**Deterministic on purpose.** It writes through `capture::wiki_capture`
with dedup *off*: a signpost's identity is its topic key (project, and
day for an activity line), not similarity — two projects described in
similar words stay two signposts. Going through the ordinary ingest path
would hand placement to the classifier, and placement is exactly what
must be guaranteed here. `projects.md` is a **reserved channel page**
([`wiki::is_channel_page`](../../crates/mwe-core/src/wiki.rs)), fenced
out of the compiler, the REM refile and the contradiction sweeps the way
`rules.md` is — but, unlike `rules.md`, it stays recallable, which is
the entire point.

**The nudge (48f)**: every `wiki_admin_push` response carries
`signpost_hint` — `null` when the signposts are current, otherwise one
line naming what is missing (no description at all, or no activity line
for today). The push is the right moment: the agent is already here, and
something worth signposting just happened.

The nudge is **silent on a consumer's own operational wiki** — the wiki
the sign-in flow forges. That wiki
is the agent's private working memory, not a project, and signposting it
would only add noise to the page whose whole job is to let a turn
discover *projects*. The test is deliberately that property and not "has
a `project_id`": that field is optional on create, so keying on it would
silently un-signpost a real project wiki pushed without one — the exact
failure this area exists to prevent.

What identifies the wiki is the **`is_agent` marker** the sign-in flow
stamps on it, with the `wiki_type: agent` label
([`wiki::AGENT_WIKI_TYPE`](../../crates/mwe-core/src/wiki.rs)) kept as the
fallback for an operational wiki forged before the marker existed and not
yet re-authed. The label alone was never a safe test: `wiki_type` is a
free-form string the **consumer** passes to `wiki_admin_push`, so any wiki
can claim `agent` and dodge the nudge, while a consumer that labelled its
operational wiki anything else collected nudges on its private working
memory. `is_agent` has no field on the tool surface — only the server
writes it.

### 4. Ingest filter — and the conversation superset

`wiki_ingest_message`'s step 2 ("enumerate wikis") drops every
smart wiki before assembling the LLM router prompt. Each enumerated
wiki's `AvailableWiki` carries its smart flag read straight from
`_meta.md` (no registry query), and the filter keeps only `!smart`. A
smart wiki therefore never appears in the routing window, so the
classifier cannot target one; the defensive backstop is the
capture-plan validation, which rejects any target not in the offered
list. See
[ingest-pipeline.md §Smart-family filter](ingest-pipeline.md#smart-family-filter).

**The filter excludes the smart wiki, not the conversation** (roadmap
group 17). A smart consumer is a **superset** of a standard one: it
authors its project (smart) wiki via the family-H `wiki_admin_*` tools
**and** routes the user↔agent **conversation** through
`wiki_ingest_message`, which lands in the user's standard personal wiki
— exactly as a standard consumer's turn does. There is **no
`consumer_class` gate** on the ingest path (`IngestRequest` carries no
such field): the engine is a superset by construction, and the
per-message routing (what reaches `wiki_ingest_message` at all) lives
in the `smart-consumer` skill, not the engine. The two write
paths are joined by **links, not duplicated detail** — consolidation
emits a reference to the project page rather than re-storing it (see
[narrative-compiler.md](narrative-compiler.md), the provenance link),
and the `smart-consumer` skill carries the per-message router (drop /
personal-fact / document-import-on-request / project-wiki+link). The
double-bill concern in §1 is about **bulk project-doc maintenance**,
not the low-volume per-turn conversational memory: the consumer's
router drops ephemeral ops before any server call, and the per-turn
classifier bill is the same one every standard consumer pays.

### 5. Skill catalog

Smart consumers need a place to fetch the documents that explain how
to behave (cardinal rules, the bootstrap flow, the `_briefing.md`
lifecycle, the companion-vs-transversal distinction). The
server-served skill catalog provides that: **bundled** stubs embedded
into the binary via `rust-embed`. The catalog ships bundled skills only.

The module
[`mwe_core::skills`](../../crates/mwe-core/src/skills.rs) defines
the `Skill` shape (`source: Bundled`) and the catalog API:
`list_bundled()`, `fetch(name)`, `fetch_bundled(name)` (public, used by
the HTTP endpoint). Each `Skill` carries an `etag = sha256(content)[..32]`
(16 bytes, 32 hex chars) that both the MCP and HTTP surfaces expose so
consumers can short-circuit on cache hit.

Two parallel distribution surfaces:

- **MCP tools** (family I): `skill_list` + `skill_fetch(name, version?)`.
  Open to every authenticated token — bundled skills are public
  documentation.
- **HTTP endpoints** mounted on the same Axum process via
  [`mwe_mcp_server::http_skills::router()`](../../crates/mwe-mcp-server/src/http_skills.rs):
  `GET /skills` (JSON bundled metadata) + `GET /skills/<name>.md`
  (raw markdown with `Content-Type: text/markdown; charset=utf-8`,
  `ETag`, honours `If-None-Match → 304`).

The bundled skills ship in
[`crates/mwe-core/skills/`](../../crates/mwe-core/skills/):

| Skill | Pre-requisite | Body |
|---|---|---|
| `core` | always loaded | cardinal rule + identity claims + dispatcher pseudocode + the exact `project_id` recipe and the `first_connect` datum + skill catalog + auth-failure matrix |
| `core-globalmemory` | `consumer_class=smart` + cwd without `.mwe/state.json` | forked-subagent recall pattern |
| `smart-consumer` | `consumer_class=smart` + cwd with `.mwe/state.json` | resume-the-project bootstrap + day-to-day editing loop + reading the push response (`warnings[]` / `signpost_hint` / `section_indexing`) + cooperative lease + `_briefing.md` lifecycle + graceful degradation; the smart consumer writes pages directly and verbatim, respecting the documented `_meta` / frontmatter constraints — no styles, no custom types; **the conversation superset** (group 17): the per-message router (drop / personal-fact / document-import-on-request / project-wiki+link) routing the user↔agent conversation through `wiki_ingest_message`, plus the `authored_refs` provenance echo that links a personal digest to a just-pushed project page |
| `standard-conversational` | `consumer_class=standard` (openclaw, hermes, …) | per-turn `wiki_ingest_message` loop + wire shape + disambiguation + `pending_attention` + `events_poll` + proposal lifecycle + consumer self-configuration |
| `smart-codebase` | smart consumer on a software project | folder mapping + module / decision / change-log page conventions + `source_ref`/`last_synced` discipline |
| `smart-onboarding` | **fetched on demand**: `first_connect.hint`, or the user asks | first connect, once per project: the three-question intro (and the questions that must never be asked), the four situations, the faithful bulk copy, the `create` wire shape, the post-import shape report, the cut-never-rewrite page repair |
| `web-smart-consumer` | bridge-less web client (claude.ai) | the reduced surface, bundled with the client rather than fetched |

**Why one skill is fetched on demand.** A one-shot procedure written into
the everyday skills is paid for by every session forever, and first
connect is the rarest path there is — it happens once per project. Moving
it out only works together with the trigger: `smart_bootstrap`'s
`first_connect` block *tells* the agent the project has no memory
(roadmap 51a/51b), so the procedure is not gated on the agent remembering
to look for it.

The bundled skills all ship at `status: implemented`; their roster and
per-skill `version` are the SSOT in
[`crates/mwe-core/skills/`](../../crates/mwe-core/skills/) (each read
from its own frontmatter — don't hardcode them here). The catalog
mechanism is `skill_list` +
`skill_fetch` + HTTP `/skills`; the
per-class bodies dispatch from `core` and reference each other in
their `depends_on` frontmatter.

The `core-globalmemory` body (`version: 1.0.0`, `status:
implemented`) codifies four pillars: (a) the first-prompt
transversal `wiki_search` call shape with the **mandatory**
`smart: false` scope filter that excludes the companion
family — without it, a generic-cwd recall would leak project context from
smart wikis the same user owns elsewhere; (b) the
consumer-neutral auto-memory integration table (auto-memory =
ephemeral per-cwd cache, mwe-mcp = persistent per-user layer); (c)
heuristics that recognise a cwd as a *companion candidate* (VCS
marker + project manifest + `docs/` + consumer instruction file)
and **prompt the user** instead of auto-bootstrapping — the
bootstrap is a write operation and must be explicit; (d) explicit
anti-patterns (no recall per turn, no filter omission, no
`wiki_admin_*` from transversal mode, no silent promotion).

### 6. No custom companion types

There is no custom companion **type** mechanism: no `wiki_type`
registry, no on-disk templates under `_styles/`, no auto-generated
custom skills, and no runtime type-forge. A companion's *shape* is the
smart consumer's own concern — it organises its pages freely under the
generic smart wiki (`smart: true` in `_meta.md`).

### 7. REM read/write split + smart-wiki sub-jobs

The nightly REM cycle is the only place where the server has carte
blanche to mutate any wiki it sees fit. That authority is cleaved
in two: the write-jobs **never touch smart wikis** (the smart
consumer owns those writes via `wiki_admin_push`), and two
smart-wiki-only **read-jobs** post observations into `_briefing.md` so
the smart consumer can act on them next session.

The corollary is a **standing ruling**, not an oversight to fix later: a smart
wiki — including an agent's own **operational** wiki — is never consolidated,
deduped, re-shaped or forgotten by the engine, and the upkeep is the owning
consumer's job. Anything that goes wrong there is corrected in the **bundled
skill** ([`smart-consumer`](../../crates/mwe-core/skills/smart-consumer.md),
§"Nothing tidies a smart wiki but you"), never by carving a server-side
exception into the split above. See planning item 27d-smart for the decision.

The split is enforced through a single cycle-scoped `SmartWikiIndex`
loaded by [`load_smart_wiki_index`](../../crates/mwe-core/src/rem.rs):

```rust
type SmartWikiIndex = HashMap<String, bool>;  // wiki_id -> smart

fn load_smart_wiki_index(tree) -> Result<SmartWikiIndex> {
    // one tree walk reading the per-wiki smart flag from each
    // _meta.md
}
```

Every sub-job shares the same map so a classification race between
sub-jobs is impossible. Unknown wiki ids (deleted between snapshot and
now) default to `false` — the legacy write-jobs keep operating on
partially-broken trees rather than silently dropping work.

Write-job exclusion is uniform: a single
`if is_companion(companion_index, wiki_id) { continue; }` at the top
of each `tree.walk()` loop in the four legacy write-jobs — revisor,
auto-promote, archive-detector, hub-writer (see
[`rem-cycle.md`](rem-cycle.md)).
Auto-apply / auto-finalize sweeps work on proposals, not wikis, and
proposals targeting companions are blocked at emission time by the
now-excluded write-jobs.

**Briefing dispatcher** ([`run_briefing_dispatcher`](../../crates/mwe-core/src/rem.rs))
scans each smart wiki's **sections** (`wiki_sections`, not `fact_index`)
for two findings. The `source_ref` therefore keys on the section's stable
`<source_path>#<ord>` handle:

| Finding | Trigger | `source_ref` |
|---|---|---|
| **Stale draft** | YAML body has `status: draft` at the top level and `created_at < now - briefing_stale_draft_age` (default 14 days) | `rem:briefing_dispatcher:stale_draft:<source_path>#<ord>` |
| **Recall-hot** | `wiki_sections.recall_count_30d >= briefing_recall_hot_threshold` (default 20) | `rem:briefing_dispatcher:recall_hot:<source_path>#<ord>` |

Because a section's identity is its position, both findings survive an
edit elsewhere on the page — the dedup window keeps working instead of
re-firing under a fresh id every time the page is touched.

**Backlink reciprocity detector** ([`run_backlink_reciprocity`](../../crates/mwe-core/src/rem.rs))
builds a `(target_companion, source_wiki)` matrix in one pass:

1. Collect smart wikis + cache their section bodies.
2. For each non-smart source wiki, scan each active fact body with
   [`recall::extract_wikilink_wiki_ids`].
3. For each `[[<wiki_id>...]]` whose target is a companion, check
   whether at least one fact body in the target companion mentions
   `[[<source_wiki_id>...]]`. If not, the inverse is missing.
4. Post one briefing item on the companion with `source_ref =
   "rem:backlink_reciprocity:<source_wiki_id>"`.

Both sub-jobs key idempotency on `(wiki_id, source_ref)` and absorb
the same finding for `briefing_dedup_window` (default 7 days), so REM
never spams the same observation night after night. Per-wiki cap is
`briefing_notify_cap` (default 10); the global `50 notify/wiki/hour`
cap from [`briefing::NOTIFY_RATE_PER_HOUR`] still applies as the
inbox-level backstop.

To let REM emit notifications without a user `sender_id`, the briefing
module gained [`briefing::notify_as_rem`](../../crates/mwe-core/src/briefing.rs):

```rust
pub async fn notify_as_rem(pool, tree, req) -> Result<NotifyResponse>;
```

It shares the validate → smart-family gate → rate-limit → DB →
filesystem pipeline with the user-facing `notify` via the new private
`notify_append` helper. The two differences: ACL is **bypassed**
(REM is a server-internal actor, no `sender_id`), and `source_kind` is
**forced to `Rem`** regardless of the input value. The wiki owner sees
the items in their own `_briefing.md` alongside the user-typed ones.

### 8. Three-layer briefing classification

The `_briefing.md` inbox is the smart consumer's triage surface — and
not every item there has the same shape. The free-string `kind` field
(validated against `observation|reasoning|external`) is wrapped in a
typed surface + two query helpers, and the REM sub-jobs route to the
appropriate layer:

| `BriefingKind` | What it means | Typical sources |
|---|---|---|
| `Observation` | "REM noticed X" / "the data says Y" — passive fact, no decision required | REM Briefing dispatcher (stale draft, recall-hot), openclaw forwarding user observations |
| `Reasoning` | "we should decide whether to Z" — a concrete action the smart consumer is invited to take, with context attached | REM Backlink reciprocity detector (add the inverse link?), future REM sub-jobs that flag drift |
| `External` | "alice left this for you" — an appointment from the end user or from a `shared_with` reader | Dashboard manual input, user notes relayed by openclaw, commit/URL pointers |

Typed surface in [`mwe-core::briefing`](../../crates/mwe-core/src/briefing.rs):

```rust
pub enum BriefingKind { Observation, Reasoning, External }
impl BriefingKind {
    pub const fn as_str(self) -> &'static str;
    pub fn parse_wire(s: &str) -> Option<Self>;  // tolerant: unknown → None
}

pub struct BriefingItem {
    pub briefing_item_id: String,           // bi_<id>
    pub wiki_id: WikiId,
    pub source_kind: BriefingSourceKind,    // user | rem | consumer | dashboard
    pub source_ref: String,
    pub topic: String,
    pub body: String,
    pub kind: Option<BriefingKind>,         // None on legacy NULL rows or unknown wire
    pub ts: String,
    pub processed_at: Option<String>,
}

pub struct ListItemsFilter {
    pub kind: Option<BriefingKind>,
    pub pending_only: Option<bool>,
    pub limit: Option<i64>,                 // default 50, hard cap 200
}
```

Two query helpers — both single SQL round-trip, ordered freshest-first
where applicable:

- **`list_items(pool, wiki_id, filter)`** projects rows as
  `Vec<BriefingItem>`. Used by `smart_bootstrap` at session start (no
  filter → freshest 50 items pending + drained) and by the dashboard
  briefing tab (filtered by `kind` to render per-layer tabs).
- **`counts_by_kind(pool, wiki_id)`** returns a
  `BriefingKindCounts { pending_observation, pending_reasoning,
  pending_external, pending_unclassified, total }` — drives the
  dashboard "X items to triage" badge without fetching bodies. The
  `pending_unclassified` bucket counts legacy NULL-kind rows + any row
  whose stored `kind` string is not in the typed enum (defence against
  future corruption).

REM semantic routing in [`emit_dispatcher_notify`](../../crates/mwe-core/src/rem.rs)
and [`emit_backlink_notify`](../../crates/mwe-core/src/rem.rs):

- **Stale draft + recall-hot** → `BriefingKind::Observation`. REM
  observed something about a fact; the smart consumer can absorb the
  observation without taking an action.
- **Backlink reciprocity** → `BriefingKind::Reasoning`. REM is
  recommending a concrete edit ("add the inverse `[[<source>]]` link
  to your page"), so the consumer must decide before the next push.

Richer triage UX (auto-collapse pending observations, prioritise
reasoning items in the triage queue) is not yet implemented (planned —
see the roadmap).

### 9. Citation IDs nel briefing flow

The smart consumer reads `_briefing.md` at session start; without
stable handles back into the wiki, every "this is about MFA recovery
codes" item forces it to grep for the relevant page. The citation
surface is a typed handle that points at a
specific wiki section, plumbed end-to-end through `wiki_admin_notify`
→ DB → markdown render → `BriefingItem`.

Wire format (see [mcp-tools.md](../protocol/mcp-tools.md) for the tool
roster):

```
wiki://<wiki_id>/<page_path>(#<heading-slug>)?
```

Examples:

```
wiki://alice-lnprint/modules/auth.md#mfa-flow
wiki://alice-lnprint/decisions/2026-05-24-recovery-codes.md
wiki://alice-lnprint/_briefing.md#openclaw-202605241830
```

Pure utility surface ([`mwe-core::briefing`](../../crates/mwe-core/src/briefing.rs)):

```rust
pub const CITE_SCHEME_PREFIX: &str = "wiki://";
pub const CITE_MAX_BYTES: usize = 512;

pub fn slug_from_heading(text: &str) -> String;             // lowercase + [^a-z0-9]+ → "-" + trim "-"
pub fn extract_anchors_from_markdown(body: &str) -> Vec<HeadingAnchor>;
pub fn compose_cite(wiki_id: &WikiId, path: &str, anchor: Option<&str>)
    -> Result<String, BriefingError>;
pub fn parse_cite(s: &str) -> Result<ParsedCite, BriefingError>;

pub struct ParsedCite { wiki_id: WikiId, path: String, anchor: Option<String> }
pub struct HeadingAnchor { line_number: usize, level: u8, heading_text: String, anchor: String }
```

End-to-end plumbing:

1. **MCP schema** — `wiki_admin_notify` (family H) carries an
   optional `target_cite` string property with `parse_cite`-shape
   description.
2. **Dispatcher** — `WikiAdminNotifyArgs.target_cite: Option<String>`
   propagates straight to `NotifyRequest`.
3. **Validation** — `notify_append` trims the value; blank → `None`,
   non-blank → `parse_cite(...)?` so a malformed handle surfaces as
   `400 invalid_input` *before* the DB write.
4. **Persistence** — migration `0025_briefing_target_cite.sql` adds
   the `target_cite TEXT NULL` column; `INSERT … VALUES (…, ?)` binds
   the validated value.
5. **On-disk render** — `render_section` appends an inline
   `*→ <wiki://…>*` Obsidian autolink right after the `*from <source>…*`
   attribution, only when the handle is present. Plain-text readers see
   the URL; Obsidian renders it as a clickable link.
6. **Read-back** — `BriefingItem.target_cite: Option<String>` exposes
   the value through `list_items` so the smart consumer (and the
   dashboard `/cite/` resolver) can dispatch on it without re-parsing
   the markdown.

REM sub-jobs currently emit with `target_cite = None`: the heuristics
in [§7](#7-rem-readwrite-split--smart-wiki-sub-jobs)
don't yet derive a specific anchor from the finding (e.g. backlink
reciprocity could point at the page-level cite; stale-draft already
knows the source path). Wiring those defaults is not yet done (planned
— see the roadmap).

The citation-resolver surface:

- Dashboard `GET /cite/<bi_id>` resolver — a public route in
  `crates/mwe-dashboard/src/routes/cite.rs`. The
  handler accepts both the canonical `bi_<N>` shape (the user-facing
  form returned by `BriefingItem::briefing_item_id`) and the bare
  integer; it looks up `wiki_briefing_items.target_cite`, runs it
  through `parse_cite`, and emits a `302 Found` with `Location:
  /dashboard/wiki/<wiki_id>/view/<path>#<anchor>` (the anchor is
  omitted when `target_cite` has no fragment). Row not found,
  `target_cite IS NULL`, and `parse_cite` failure all collapse to a
  generic `404 Page not found.` so a probing caller cannot distinguish
  "row missing" from "row without anchor". The destination
  read-rendering route is `/dashboard/wiki/:id/view/*path`. The
  `/view/` prefix is a deliberate deviation from the spec's
  `/dashboard/wiki/:id/<path>`:
  axum 0.7's `matchit` router refuses to host a bare capture next to
  the existing `/wiki/:id/edit/*path` editor route (overlapping
  captures panic at startup), so the read viewer takes the `/view/`
  segment and the resolver redirects
  there. The mapping is invisible to consumers — they hand `/cite/`
  URLs to the user, and the resolver hops to the correct destination
  page.
- **Auth posture**: the resolver is anonymous — no `SessionUser` /
  `AdminUser` extractor on the handler, and the route lives in the
  dashboard's public tree (no session middleware). Access control
  fires on the destination `/dashboard/wiki/...` page, which already
  runs through the session layer. Rationale: smart consumers embed
  short `/cite/bi_42` URLs in their replies to the operator; the
  click must reach the destination whether or not the recipient is
  logged in (they get redirected to `/dashboard/login` on the
  destination if not).
- **Two mount points, one handler**: the same `routes::cite::router()`
  is merged into the dashboard public tree (giving the discoverable
  alias `/dashboard/cite/:bi_id`) and also exposed via
  `mwe_dashboard::cite_router(state)` for `mwe-mcp-server` to nest
  at the top level (giving the canonical short form `/cite/:bi_id`).
  Both paths share the same handler so their behaviour cannot drift.

Not yet implemented (planned — see the
roadmap):

- Persistent `wiki_anchors` table populated by `wiki_admin_push`
  (using the `extract_anchors_from_markdown` utility that ships
  today). Not strictly required by the resolver — `parse_cite`
  validates the handle structure regardless — but it would let the
  dashboard page view detect orphaned `target_cite`s when the heading
  they point at has been renamed or removed.
- Disambiguation suffixes (`-2`, `-3`) when multiple headings produce
  the same slug — the utility returns duplicates verbatim, the
  persistence layer would write the suffixed form.

**Inline comment write path.** The viewer at
`/dashboard/wiki/:id/view/*path` accepts an optional `?mode=comment`
query parameter. With it set, every heading gets a `+ Comment on
#<slug>` affordance that points at a sibling form, and a `Stop
commenting` toggle at the top lets the operator exit. Without the
query parameter the read view stays clean — no per-heading button —
so a teammate who only wants to consume the page does not see a
noisy UI.

Clicking the affordance lands on `GET /dashboard/wiki/:id/comment/*path?anchor=<slug>`,
which renders a short form: a textarea (`required`, `maxlength=4096`),
a Save button, a Cancel link, and a context block that surfaces the
addressed heading text when the page currently has a heading that
produces the same slug (best-effort; if the lookup misses the comment
is still allowed and lands in the orphaned bucket of the inline
viewer until the heading is restored or renamed). The form posts to
the matching `POST`, which:

1. Resolves `SessionUser` (anonymous → redirected to `/dashboard/login`
   by the session middleware).
2. Calls `wiki_admin::resolve_read_access` — owner OR `shared_with`
   match (direct user, group via enrollment, or global) is allowed
   to comment; `Denied` is mapped to `403` (`DashboardError::NoAccess`,
   the content-ACL copy — not the admin-gate `Forbidden`, since
   `resolve_read_access` has **no admin bypass**: an admin outside the
   read set is denied too). Anyone who can *read* the wiki can leave a
   comment, which is the point of the companion sharing surface.
3. Trims + validates the body: empty → `422 Validation`; longer than
   4 KiB → `422 Validation`. No DB row is inserted on either rejection.
4. Validates the `anchor` query parameter against the slug charset
   (`[a-z0-9-]+`, no leading / trailing dash). Bad shape → `422`.
5. Composes the canonical `target_cite` via
   `briefing::compose_cite(wiki_id, page_path, Some(&anchor))`.
6. INSERTs a row directly into `wiki_briefing_items` with
   `source_kind='dashboard_comment'`, `source_ref='dashboard:<author_sender_id>'`,
   `topic=<first 80 chars on a word boundary>`, `body=<full>`,
   `kind='external'`, `author_sender_id=<signed-in user>`,
   `target_cite=<composed>`, `ts=NOW()`, `processed_at=NULL`.
7. Redirects 302 to `/dashboard/wiki/:id/view/*path` (read mode, no
   `?mode=comment`) so the operator lands on the freshly interpolated
   inline render.

**Direct INSERT instead of layering on `briefing::notify`.** The
write path is a raw `INSERT INTO wiki_briefing_items` rather than a
call through `mwe_core::briefing::notify` for four reasons: (a)
`notify` enforces the smart-family gate, but dashboard comments
should land on every wiki the operator can read; (b)
`notify` does not accept the new `author_sender_id` column; (c) the
new `source_kind='dashboard_comment'` value widens the enum only at
the SQL level (see migration `0027`) and does not map onto the
`BriefingSourceKind` enum `notify` consumes; (d) the `50 notify/wiki/h`
rate cap that protects against REM/consumer flood does not match the
expected cadence of a human reviewer. Bypassing keeps the two
surfaces coherent: `notify` stays the companion inbox channel, this
handler stays the dashboard feedback channel, and the resulting row
is identical at the DB level — so both surface in the inline viewer
and in `_briefing.md` rendering the same way.

**URL route deviation.** The natural form route would be
`/wiki/:id/view/*path/comment`, but axum 0.7's `matchit` router
cannot route a greedy capture (`*path`) followed by a literal suffix
— the suffix is consumed by the capture and the route never matches.
The implementation uses the sibling `/wiki/:id/comment/*path`
instead, which lives next to `view/` and `edit/` with no overlap.
The two segments share the same `:id` + `*path` shape so the handler
can identify the comment target identically.

**Range-selection comments are not supported.** The citation grammar
(`wiki://<id>/<path>#<heading-slug>`) accepts only heading slugs as
fragments. Range selection would require extending the grammar to a
byte-range fragment + updating the inline layout helper to render
inline blocks against a sub-line interpolation. Today the surface
supports only "click on heading".

### 10. Sharing smart wikis

The base companion is single-owner: `wiki.owner_user` is the only one
who can read or write. The sharing path opens the read channel (and the
`wiki_admin_notify` channel) to a roster of additional principals
without touching the write invariant.

`_meta.md` gains a new optional sequence:

```yaml
shared_with:
  - user:bob
  - group:lnprint-devs
  - global              # rare — anyone authenticated reads
```

Resolution lives in [`mwe-core::wiki_admin`](../../crates/mwe-core/src/wiki_admin.rs):

```rust
pub enum ReadAccessOutcome {
    Owner,
    SharedUser,
    SharedGroup(String),     // group_id preserved for audit
    Global,
    Denied { owner: String },
}

pub async fn resolve_read_access(
    pool: &SqlitePool,
    tree: &WikiTree,
    handle: &WikiHandle,
    caller_sender_id: &str,
) -> Result<ReadAccessOutcome, AdminError>;
```

Lookup order (first match wins so the audit always shows the most
specific grant):

1. Caller is the owner (resolved `scope` → `Principal::User`
   matches the caller).
2. Caller appears as a direct `Principal::User(_)` in `shared_with`.
3. Caller is a member of a `Principal::Group(_)` in `shared_with`
   — membership resolved through
   [`enrollment::groups_for`](../../crates/mwe-core/src/enrollment.rs)
   (one SQL round-trip only when the roster has group entries).
4. `shared_with` contains `Principal::Global` — every authenticated
   token reads.
5. Else `Denied { owner }` carrying the resolved owner id for the
   diagnostic message on the 403 path.

**Recall consistency.** `resolve_read_access` gates the `wiki_admin`
read surface (`wiki_read` / `wiki_search` at the wiki level) and
`wiki_admin_notify`. Content **recall** is gated on the *same* roster, at
the same granularity: [`recall::search_sections`](../../crates/mwe-core/src/recall.rs)
reads the wiki's owner + `shared_with` from the `smart_wikis` registry,
keeps the wikis the sender may read, and only then loads their sections.
A `shared_with` edit refreshes that **single registry row**
synchronously before the sharing request returns
([`sections::upsert_smart_wiki`](../../crates/mwe-core/src/sections.rs),
called from the dashboard sharing route) — a revoke must close the recall
read-window immediately, never waiting for the ~5-minute safety-net
sweep, which re-projects the roster as a backstop for a hand edit to
`_meta.md`.

`briefing::notify` (the `wiki_admin_notify` MCP tool's headless core)
routes through `resolve_read_access` rather than an
owner-only check, so a `user:bob` listed in `shared_with` can append a
`*from consumer (ref: cc-bob-laptop, at …)*` item into the owner's
`_briefing.md` (kind defaults to `external` per the briefing
classification, but the caller is free to override).

**The write invariant stays untouched.** `wiki_admin_push` and
`wiki_admin_pull` still enforce `wiki.owner_user == token.owner_user`
— a shared_with user with a smart token attempting either tool gets
the canonical `WikiOwnedByOtherUser`. Collaborative-write with
multiple authoritative owners (group-owned wikis, or push whitelists)
is not supported; the read+notify channel covers the
team-of-developers scenario.

The roster is managed via the dashboard `/wikis/<id>/sharing` route —
**smart-only**: the surface is a `404` on a standard wiki (not even
discoverable), because `shared_with` is the *wiki-level* ACL axis, which
standard wikis do not have — their reads are governed per-fragment, and a
wiki-level roster would flatten that granularity. The guard
lives in `load_sharing` (gating both the GET form and the POST), the
inverse of the raw editor's smart-wiki refusal. Operators can also edit
`_meta.md` by hand — the parser accepts the field and round-trips it
through `WikiMeta::to_yaml`.

## End-to-end happy path — Claude Code on `~/projects/lnprint/`

1. **Connect over OAuth** (one-time). The user runs `claude mcp add
   --transport http mwe-mcp <origin>/mcp --scope user`, then signs in to
   mwe-mcp inside Claude Code (open `/mcp`, or `claude mcp login
   mwe-mcp`) and approves the connection on the consent screen — **no
   token is minted or pasted**. The loopback OAuth redirect makes the
   mint stamp `consumer_class=smart` + the `Local` profile (the full tool
   catalog), and forges Claude Code's dedicated **operational** wiki.
   ([web-agent-oauth](web-agent-oauth.md).)

2. **Memory loads at session start.** Claude Code presents its
   short-lived OAuth access token on every `/mcp` call. An optional
   token-less `SessionStart` nudge reminds the model to load its memory;
   it then calls `smart_bootstrap` itself (recall is model-driven, not
   hook-driven).

3. **Session start in `~/projects/lnprint/`**. Smart consumer (skill
   `smart-consumer`) computes
   `project_id = sha256(normalized_origin + ":" + project_root)[:16]`
   and calls `wiki_search(scope={ smart: true,
   project_id: ... })`. The `project_id` filter is not yet enforced
   server-side — the client relies on `smart` plus a
   client-side filter.

4. **First-time bootstrap**. No smart-wiki for this project.
   Smart consumer asks the user for confirmation (bootstrap is never
   automatic) and generates `.mwe/wiki/` locally with its own LLM. If the
   repo already has docs, they are authored *from* — never renamed or
   moved (the local copy stays intact); an existing wiki is checked for
   mwe-compatibility and ingested faithfully. It then calls:
   ```
   wiki_admin_push(
     mode: "create",
     project_id: "abc123def456",
     wiki_type: "project",   // free-form label; does not decide smart-ness
     smart: true,            // this is what forges a smart wiki
     pages: [{ path: "index.md", content: "..." }, ...]
   )
   ```
   Server runs the 3-gate auth, derives `wiki_id = w_lnprint_<slug>`,
   writes the pages, appends a row to `wiki_admin_op_log`, returns
   `{ wiki_id, ops_applied: N, op_log_id: ... }`.

5. **Day-to-day edits**. Smart consumer keeps modifying `.mwe/wiki/`
   locally. Each batch of changes → `wiki_admin_push(mode: "upsert",
   wiki_id, pages, deletes)`. Server validates, applies, logs.

6. **Openclaw relays an observation** (parallel session, separate
   token). Operator on Telegram: "appunta: il modulo MFA è scaduto."
   Openclaw routes the message into `wiki_ingest_message` →
   classifies as `capture` → wants to target the lnprint companion.
   Server filters the companion out of `available_wikis` (it's
   never an LLM-driven capture target). Openclaw falls back to
   `wiki_admin_notify(wiki_id: <lnprint>, topic: "MFA expired", body:
   "il modulo MFA è scaduto.", source: { kind: "user", ref: "tg" })`.
   Server appends to `_briefing.md` + writes the
   `wiki_briefing_items` row.

7. **Next session on lnprint**. Smart consumer pulls
   `_briefing.md`, surfaces the unread "MFA expired" item to the
   operator, asks "shall I update `modules/auth.md` accordingly?"
   On confirmation: regenerates the page locally, `wiki_admin_push
   mode: "upsert"` with the delta, archives the briefing item in
   `_briefing.archive.md`.

8. **Disconnect + reconnect**. Operator disconnects the connection
   from the dashboard's *Web agent connections* (laptop lost): the
   refresh tokens are revoked so renewal stops and the short-lived
   OAuth access token lapses on its own (≤ 1 h) — the next
   `wiki_admin_push` past that → `401`. The smart consumer keeps the
   local `.mwe/wiki/` cache intact (no work lost), and the dedicated
   wiki is **kept** server-side. The user reconnects over OAuth (the
   idempotent consumer/wiki binding is reused). Next session: smart
   consumer calls `wiki_admin_pull` (absorbs any `wiki_admin_notify`
   items that landed in the gap from REM or another consumer), diffs
   vs local, `wiki_admin_push mode: "upsert"` with the delta.
   Realignment complete.

## Maturity

The smart-wiki surface is `status: partial`: the family-H
`wiki_admin_*` tools, the `smart: true` marker, the dedicated content
index (`wiki_sections` + the `smart_wikis` registry), the skill catalog,
the REM read/write split, three-layer
briefing classification, citation IDs, inline dashboard comments,
op-log revert, sharing, the lease coordination tools, and
`expected_op_log_head` optimistic concurrency are all implemented and
exercised by tests. The deferred items called out in the sections
above — `snapshot_replace` mode, `since_op_log_id` delta-pull, the
`project_id` / `administered_by_self` search filters, the persistent
`wiki_anchors` table, slug disambiguation suffixes, range-selection
comments, and richer briefing-triage UX — are not yet implemented
(planned — see the roadmap).

## Cross-references

- **Wire-level tool spec**: [protocol/mcp-tools.md](../protocol/mcp-tools.md) (user-facing roster) + [protocol/tool-reference.md](../protocol/tool-reference.md) (canonical schemas).
- **Auth model**: [jwt-and-session-model.md §Consumer class](jwt-and-session-model.md#consumer-class--the-smart-vs-standard-gate).
- **Smart marker**: the `smart: bool` flag in each wiki's `_meta.md` (on-disk key `smart:`, legacy read-alias `companion:`) (§2 above) — see also [../concepts/memory-model.md](../concepts/memory-model.md).
- **Ingest filter**: [ingest-pipeline.md §Smart-family filter](ingest-pipeline.md#smart-family-filter).
- **Dispatcher wiring**: [mcp-dispatcher.md §The tool roster](mcp-dispatcher.md#the-tool-roster).
- **Content index**: [`mwe_core::sections`](../../crates/mwe-core/src/sections.rs) (`wiki_sections` + `smart_wikis`) and [reindex-pipeline.md](reindex-pipeline.md#smart-wikis--content-indexing-markerless).
- **Recall corpora**: [recall-pipeline.md §The two corpora](recall-pipeline.md#the-two-corpora).
- **Migrations**: [engine-db-and-migrations.md](engine-db-and-migrations.md) — `wiki_admin_op_log` (0022), `wiki_briefing_items` (0023), `wiki_sections` + `smart_wikis` (0062).
