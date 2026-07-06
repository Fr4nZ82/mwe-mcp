---
title: Proposal apply / revert engine
area: design-notes
status: implemented
last_review: "2026-07-02"
---

# Proposal apply / revert engine

This page documents the **chassis** behind the structural-change
lifecycle, plus the concrete kind handlers shipped today:
`wiki_promote` (both `paragraph_to_file` and `file_to_subwiki`
variants), `dedup_merge` (two-way merge), `bundle` — a
**born-applied-only** kind wrapping many sub-operations into one
revertible receipt ([`mwe_core::bundle`], the page-deletion unit) — and
`fact_forget`, a **propose-first vote** a non-sender owner opens to forget
one fact ([`mwe_core::votes`], identity model Part 3). `bundle` has no
chassis *apply* path (a bundle is born-applied, never `pending`), so its
`apply` arm stays `KindNotYetImplemented` on purpose; only its **revert**
(`revert_bundle`) is wired. `fact_forget` is the inverse shape: it opens
`pending` (the fact stays active), its apply arm tombstones the fact when
the audience consents, and it has **no revert** (a vote-resolved forget is
final — see [Fact-forget handler](#fact-forget-handler)).

The lifecycle is a **5-state machine**, but since the apply-and-notice
conversion **no REM emitter enters it from `pending`**:
REM applies a `wiki_promote` change (`paragraph_to_file` /
`file_to_subwiki` / `fact_refile`), a page merge, and a revisor
`dedup_merge` **directly**
and records a **born-applied receipt**
via `emit_applied_proposal` — a row inserted straight into `applied`
with a `revert_token` + 7-day `revert_deadline`, announced to the
consumer with a `structure_applied` notice that names the affected user
and carries the undo `dashboard_path`. The pending lifecycle below
remains honoured for **questionnaire rows** reaching it from a non-REM
emitter (`dedup_merge` shape; none ship today): the
`confirm_window` policy is silence = consent — rows the user does not
answer within `pending_timeout` are auto-applied with the
questionnaire's `recommended` answers and parked in
`applied_pending_confirm` for `confirm_window` (default 7 days); if the
user does not confirm or revert by the `confirm_deadline`, the finalize
sweep flips the row silently to `applied` (locked, no `revert_token`
minted, no event emitted).

**MCP exposure**: none. The whole `structure_proposal_*` family is off
the MCP surface — consumers learn about applied structural changes from
the `structure_applied` event (and about sweep-applied questionnaire
kinds from `auto_applied`); the dashboard is the sole surface for
proposal writes and it calls the chassis functions directly. The
invariant is explicit: **mwe-mcp is not usable without the dashboard**.

## Where it lives

- **Tool surface**:
  [`tool-reference.md`](../protocol/tool-reference.md) — the
  apply-and-notice contract (no proposal family on MCP; the
  `structure_applied` notice rides `events_poll`). The canonical kind
  names are [`kind::ALL`](../../crates/mwe-core/src/proposals.rs). The
  direct chassis API the dashboard uses (`mwe-core::proposals`,
  dashboard-only, not MCP) is documented below and in the code.
- **DDL**: the 5-state `structure_proposals` schema (with `apply_mode`,
  `confirm_deadline`, `confirmed_at`, `confirmed_by`,
  `revert_triggered_by`) is in
  [`engine-db-and-migrations.md`](../design-notes/engine-db-and-migrations.md).
- **Auto-apply policy + `confirm_window` rationale + cruscotto-only
  rule** are documented in the sweeps section below and in
  [`rem-cycle.md`](../design-notes/rem-cycle.md).
- **Code**:
  [`crates/mwe-core/src/proposals.rs`](../../crates/mwe-core/src/proposals.rs)
  — `apply_proposal`, `auto_apply_proposal`, `confirm_proposal`,
  `revert_proposal` (`RevertAuth`-driven), `mark_applied`,
  `mark_auto_applied`, `mark_confirmed`, `mark_reverted`,
  `auto_apply_overdue_proposals`, `auto_finalize_unconfirmed_proposals`
  (no kind inverse, no `revert_token`, no event),
  `expire_overdue_proposals`
  (grace-period gated), `emit_proposal`, `emit_applied_proposal`
  (born-applied receipts for the act-first structural rungs), plus the
  `kind` module with
  the canonical string constants and the `ApplyMode` /
  `RevertAuth` / `AutoApplyOutcome` / `ConfirmOutcome` types. Per-kind
  handlers live in
  [`mwe-core::promote`](../../crates/mwe-core/src/promote.rs) and
  [`mwe-core::dedup`](../../crates/mwe-core/src/dedup.rs).
- **MCP wiring**: none —
  [`crates/mwe-mcp-server/src/mcp/tools.rs`](../../crates/mwe-mcp-server/src/mcp/tools.rs)
  has no `structure_proposal_*` handlers; the notice reaches the
  consumer through the events tools (family B).
- **Dashboard action routes (write surface)**:
  [`crates/mwe-dashboard/src/routes/proposals.rs`](../../crates/mwe-dashboard/src/routes/proposals.rs)
  — the operative chat is the proposals form surface (see
  [`dashboard.md`](dashboard.md)). The `apply` / `confirm` /
  `revert` POST handlers act as **bridge endpoints** and call
  `proposals::apply_proposal` / `confirm_proposal` / `revert_proposal`
  directly, then **303-redirect to
  `/dashboard/chat`** (the single surface) on both success and error. The
  `revert` handler is still status-aware: `applied` rows use
  `RevertAuth::Token` (token fetched server-side from the row),
  `applied_pending_confirm` rows use
  `RevertAuth::Caller(session.sender_id)`.
- **Dashboard agentic chat (write surface)**:
  [`crates/mwe-dashboard/src/agentic.rs`](../../crates/mwe-dashboard/src/agentic.rs)
  — the operative chat panel drives the chassis in-process and is the
  **primary** proposal surface. Apply and originate are chat-exposed
  (`structure_proposal_apply` / `_emit`); **revert** is chat-exposed
  (`structure_proposal_revert` — see
  [`agentic-chat.md`](agentic-chat.md#revert-in-the-chat)); and
  **confirm** is chat-exposed (`structure_proposal_confirm`
  — see [`agentic-chat.md`](agentic-chat.md#confirm-in-the-chat)).
  `dispatch_proposal_revert` replicates the route's status-aware
  `RevertAuth` selection (`applied` → `Token`, `applied_pending_confirm`
  → `Caller`) and adds the 0032 recipient gate so a non-admin can only
  revert proposals addressed to them; `dispatch_proposal_confirm` is a
  thinner shim — `confirm_proposal` gates by recipient/admin internally,
  so it just calls and maps the error.
- **REM sweep wiring**:
  [`crates/mwe-core/src/rem.rs`](../../crates/mwe-core/src/rem.rs)
  — `run_auto_apply_sweep` and `run_auto_finalize_sweep` are thin
  wrappers over the two chassis sweeps; both are part of `run_cycle`,
  with auto-apply running before auto-finalize so a single cycle can
  both auto-apply a fresh pending row and finalize a stale
  `applied_pending_confirm` row.
- **Schema**:
  [`migrations/0005_structure_proposals.sql`](../../migrations/0005_structure_proposals.sql)
  (base schema) +
  [`migrations/0019_proposals_5state_lifecycle.sql`](../../migrations/0019_proposals_5state_lifecycle.sql)
  (5-state columns + `idx_struct_confirm_deadline` partial index used by
  the finalize sweep). The full DDL walkthrough is in
  [`engine-db-and-migrations.md`](../design-notes/engine-db-and-migrations.md).

## State machine

A `structure_proposals` row lives in exactly one of five states:

```text
 pending ── apply (manual, apply_mode='manual') ────────────────────► applied
    │                                                                   │
    │                                                                   ├── revert(Token) within 7gg ─► reverted (revert_triggered_by='user')
    │                                                                   └── 7gg silent ─────────────► applied (permanent)
    │
    ├── auto_apply (sweep on timeout_at, apply_mode='auto') ──► applied_pending_confirm
    │                                                                   │
    │                                                                   ├── confirm ────────────────► applied  (apply_mode stays 'auto', revert_token minted)
    │                                                                   ├── revert(Caller) ──────────► reverted (revert_triggered_by='user')
    │                                                                   └── finalize sweep on confirm_deadline ─► applied  (silent, locked, NO revert_token)
    │
    └── auto-apply sweep failed past grace ─────────────────► expired
```

Transitions are atomic at the SQL layer: every state change is a single
`UPDATE … WHERE status = '<previous>'` (and, for the Token revert
path, `AND revert_token = ?`). If the conditional clause matches zero
rows (concurrent writer raced us, or the row was already in another
state) the helper returns the appropriate error variant
(`ApplyError::NotPending` / `ConfirmError::NotPendingConfirm` with
`status="race"`, `RevertError::InvalidRevertToken`, or
`RevertError::NotRevertable`). The kind handler's filesystem + DB
work happens **before** the final UPDATE — if the handler fails,
the row stays in its previous state and the caller can retry.

The `apply_mode` column on the row (`'manual'` vs `'auto'`)
disambiguates the two paths into `applied`: a user who answered the
questionnaire in the dashboard gets `apply_mode='manual'`, whereas
auto-apply (whether the user later confirms, reverts during the
window, or stays silent) preserves `apply_mode='auto'` for audit. The
`revert_triggered_by` column distinguishes user-initiated reverts
(`'user'`, both Token and Caller paths). There is no
`revert_triggered_by='sweep'` row — the finalize sweep does
not revert, it flips the row silently to `applied`.

**Asymmetric revert-token reach**. The two paths that arrive at
`applied` differ on `revert_token`:

- **Manual apply** (`pending → applied`): mints `revert_token` +
  `revert_deadline = applied_at + 7gg`. The user can revert via
  dashboard click (or future MCP token-bearing tool) for the next 7
  days.
- **Auto-apply then explicit confirm** (`pending →
  applied_pending_confirm → applied`): mints `revert_token` +
  `revert_deadline = confirmed_at + 7gg`. Same revert reach as the
  manual path, starting at the confirm event.
- **Auto-apply then silent finalize** (`pending →
  applied_pending_confirm → applied` via the finalize sweep): **does
  NOT mint** a `revert_token`. The user had 7gg in
  `applied_pending_confirm` to act and didn't; the modification is
  now fixed. This is the maintainer's deliberate choice:
  *"modifica diventa fissa"* — silence is consent, and once consent
  is given the window of intervention is closed. Users who want the
  revert window must click "Conferma" explicitly while the row is in
  `applied_pending_confirm`.

## `RevertAuth` contract

[`RevertAuth`](../../crates/mwe-core/src/proposals.rs) is a two-variant
enum the caller passes to `revert_proposal` to pick which revert path
applies; the chassis dispatches on `(prior_status, auth)`:

- **`RevertAuth::Token(&str)` — path post-`applied`.** Classic single-
  click revert with the `revert_token` minted at apply or confirm time:
  - Generated as a UUIDv4 (`uuid::Uuid::new_v4`), stored on the row in
    `revert_token`, returned to the caller in the `ApplyOutcome` /
    `ConfirmOutcome`.
  - Mismatch ⇒ `RevertError::InvalidRevertToken`.
  - `revert_deadline = applied_at + 7d` for the manual path, or
    `confirmed_at + 7d` for the confirm path (constant `REVERT_WINDOW`).
    Past the deadline ⇒ `RevertError::RevertWindowClosed`.
- **`RevertAuth::Caller(&str)` — path post-`applied_pending_confirm`.**
  No token (none has been minted yet — the auto-apply sweep does not
  emit a token until the user confirms):
  - Caller identifies as a `sender_id` (the dashboard / MCP forwards
    the authenticated session). Empty caller ⇒
    `RevertError::RevertNotAuthorized`.
  - Window is `confirm_deadline` (not `revert_deadline`). Past
    `confirm_deadline` ⇒ `RevertError::ConfirmWindowExpired` (the
    auto-revert sweep should be picking the row up next; this is the
    explicit refusal during the race window).
  - Stamps `revert_triggered_by='user'` on the row.

Wrong-pair calls — `(Applied, Caller)` or `(AppliedPendingConfirm, Token)`
— surface `InvalidRevertToken` instead of silently falling through to
the other path. The chassis treats the auth choice as a load-bearing
signal of the caller's intent: a caller that passes the wrong shape
has a bug in routing (e.g., the dashboard read `status` wrong), and
silence would hide it.

Operators with DB access can always revert manually. The token is a
confirm-by-knowledge mechanism, not a shared-state API.

## Kind dispatch

`apply_proposal`, `auto_apply_proposal`, and `revert_proposal` (both
auth paths) each end in a `match` over `kind::ALL`. `confirm_proposal`
does **not** invoke the kind handler — the changes are already on disk
from the prior auto-apply; confirm only flips the state and mints a
token.

| Kind constant | Wire string | Handler status |
|---|---|---|
| `kind::WIKI_PROMOTE` | `wiki_promote` | **Shipped** — variants `paragraph_to_file` (default), `file_to_subwiki`, `page_merge`, `fact_refile` (born-applied only), `validity_close` (born-applied only). See [Promote handler](#promote-handler). |
| `kind::DEDUP_MERGE` | `dedup_merge` | **Shipped** — two-way merge variant. See [Dedup-merge handler](#dedup-merge-handler). |
| `kind::BUNDLE` | `bundle` | **Revert shipped** (`bundle::revert_bundle`) — born-applied only (wraps tombstones + cross-wiki refiles for the page deletion); no chassis *apply* path, so `apply` stays `KindNotYetImplemented` by design. See [Bundle handler](#bundle-handler). |
| `kind::FACT_FORGET` | `fact_forget` | **Apply shipped** (`proposals::apply_fact_forget`) — born-`pending` (a non-sender owner's forget request, [`mwe_core::votes`]); apply tombstones the fact when its audience consents; **no revert** (final). See [Fact-forget handler](#fact-forget-handler). |

New-wiki emergence is driven by page mass (auto-promote → page→sub-wiki);
the classifier is prose-only. See
[ingest-pipeline.md](ingest-pipeline.md).

Until a kind handler lands, the dispatch returns
`ApplyError::KindNotYetImplemented(<kind>)` /
`RevertError::KindNotYetImplemented(<kind>)`, which the MCP layer
surfaces as the wire-stable `not_implemented_phase_c` class. The row
is left untouched so the consumer can re-attempt once the handler
ships.

Rows whose `kind` is not in `kind::ALL` (DB drift, manual edit, code
emit path with a typo) surface `UnknownKind` instead and map to
`invalid_input`. This is intentionally distinct from
`not_implemented_phase_c` — the wire signal "you sent an unknown
kind" tells the consumer to recheck its emission code, whereas "this
kind is canonical but not yet wired" tells it to wait or fall back.

### Emergent concept pages add no new kind

The compilation planner ([`crate::planner`](../../crates/mwe-core/src/planner.rs),
documented in [`narrative-compiler.md`](narrative-compiler.md)) mints
**emergent concept pages** when the Cartografo decides a cluster of facts
deserves its own page. A routine emergent concept page is **content the
Cronista writes** — a new `.md` page (or `index.md` hub) inside an
**existing** standard wiki — so it does **not** raise a
`structure_proposal` at all; it is a normal compiled write, not a gated
structural change. Only the **escalation** of a grown concept page into a
dedicated **sub-wiki** is a structural change, and that reuses the
**existing** [`wiki_promote` / `file_to_subwiki`](#promote-handler)
machinery driven by the proposal-gated REM auto-promote sub-job
([`rem-cycle.md`](rem-cycle.md)). The upshot: **the planner adds no new
proposal kind** — the `kind::ALL` roster above is unchanged, and the
planner plugs into the chassis only through the sub-wiki escalation path
that was already here.

## Auto-apply + auto-finalize sweeps

The two sweeps live in the chassis as public APIs and run inside
`rem::run_cycle` (in the order auto-apply → auto-finalize) so a single
24h tick advances both ends of the 5-state lifecycle.

### `auto_apply_overdue_proposals`

Loads every `pending` row past `timeout_at` (ordered by `proposed_at ASC`
so the eldest go first), derives `recommended` answers via
`build_recommended_answers`, and invokes `auto_apply_proposal` on each.
On success the row lands in `applied_pending_confirm` with
`apply_mode='auto'`, `confirm_deadline = applied_at + CONFIRM_WINDOW`
(7 days), and **no** `revert_token` — the user reverts via
`RevertAuth::Caller` while the row is pending confirm; a token is
minted only at `confirm_proposal`.

**`fact_forget` is the one exception** to this two-window auto-apply. Its
`timeout_at` is the *voting* deadline, and the audience's silence past it
is already consent — so an overdue, un-blocked `fact_forget` is resolved
**straight to `applied`** (`apply_fact_forget_now`, tombstoning the fact),
skipping `applied_pending_confirm` entirely (there is nothing left to
confirm). One carrying a recorded NO-majority at the deadline is `expired`
instead, never applied. See the
[Fact-forget handler](#fact-forget-handler).

Each successful auto-apply emits a `wiki_events.kind='auto_applied'`
row with payload:

```json
{
  "proposal_id": "<uuid>",
  "kind": "wiki_promote" | "dedup_merge" | "bundle",
  "applied_at": "<rfc3339>",
  "confirm_deadline": "<rfc3339>",
  "dashboard_path": "/dashboard/proposals/pending-confirms",
  "summary": "auto-applied <kind> (proposal <id>) — confirm or revert by <deadline>"
}
```

`dashboard_path` is **relative** on purpose: mwe-mcp has no public
base URL config yet (the deployment may sit behind a reverse proxy,
tunnel, or internal hostname), and the consumer concatenates with
whatever base it knows the operator is serving from. The summary is
plain English server-side; consumer code may render its own
localized version from the structured fields.

This event is the **lynchpin** of the auto-apply model: it's how the
agent consumer (Telegram, Discord, …) learns that a modification needs the
user's attention and surfaces the `dashboard_path` link. Without this
event, silence in `applied_pending_confirm` would happen with no user
awareness — which is precisely the case where the spec demands
notification.

Per-row failures (kind handler error, malformed questionnaire, missing
`recommended` option, event emission error) become soft errors in the
returned `AutoApplySweepReport` — the row stays `pending` for the
next sweep. Only infrastructure SQL errors bubble up via `RemError`.

### `auto_finalize_unconfirmed_proposals`

Single-statement sweep:

```sql
UPDATE structure_proposals
   SET status = 'applied'
 WHERE status = 'applied_pending_confirm' AND confirm_deadline < ?
```

That's the whole body. **No kind inverse handler is invoked** (the
modifications are already on disk from the auto-apply step). **No
`revert_token` is minted** (the user had `confirm_window` to act; the
window is closed). **No event is emitted** (the user was already
notified at auto-apply time via the `auto_applied` event with the
deadline clearly stated; silence is a valid form of consent and
emitting a "we kept your stuff" notification would be noise).

The sweep follows a silence = consent policy. Its properties:

| Aspect | silence = consent (current) |
|---|---|
| Sweep body | Single UPDATE |
| Kind handler call | No |
| Failure modes | SQL only (sweep is race-safe by single-statement atomicity) |
| Outcome on row | `applied` + `apply_mode='auto'` preserved + NO token |
| Event emitted | None (the `auto_applied` event already covered the deadline) |
| User notification | None (the `auto_applied` event already covered the deadline) |

The user's frame is: *you have 7 days to disagree; after that I
take silence as a yes and lock the change*. This is the maintainer's
explicit choice — more forgiving than undoing the change on silence.

If the operator wants the revert window despite the silence, they
must click "Conferma" while the row is still in `applied_pending_confirm`
— that path mints the `revert_token` + 7gg window starting at
`confirmed_at`. After the silent finalize, the row is fixed.

### Cron cadence

Both sweeps run inside the existing REM scheduler tick (default:
24h). The `confirm_window` of 7 days gives ample margin against the
24h ticker, so there is no separate sub-tick today. There is no
dedicated shorter ticker (e.g. hourly) for `applied_pending_confirm`
rows — a tighter window for user-initiated proposals is not yet
supported.

## Expire sweep

`expire_overdue_proposals` is a grace-period gated sweep that
flips `pending` rows past `timeout_at + EXPIRE_GRACE_PERIOD`
(24h default) to `expired`. The grace period gives
`auto_apply_overdue_proposals` several retry cycles to recover from
transient failures (LLM down, embedding endpoint unreachable) before
the chassis declares the proposal dead.

`expire_overdue_proposals_at(pool, now)` is the test-injectable
variant; the no-clock façade uses `chrono::Utc::now()`. A single
conditional `UPDATE` flips every qualifying row in one statement, no
per-row handler — the expire path is meant for proposals whose
auto-apply has consistently failed, and the operator inspects the
audit log to understand why.

## Promote handler

Lives in [`mwe-core::promote`](../../crates/mwe-core/src/promote.rs).
Several variants share the chassis arm and are picked by a `variant`
field in `answers` (default `paragraph_to_file`):

- **`paragraph_to_file`** — move N facts from one page of a wiki to
  another page **of the same wiki**. The wiki itself is not created
  or destroyed.
- **`fact_refile`** — move **one** fact from a page of the source wiki
  to a page of a **different existing** wiki — the executing verb of the
  [cross-wiki refile sweep](rem-cycle.md#cross-wiki-refile-sweep-sub-job).
  Repoints the row's `wiki_id` via `fact_index::move_to_wiki` (the only
  primitive that touches `wiki_id`; `move_region` never does), splices
  the marker off the source page and weaves it onto the dest page (path
  wiki-relative, joined onto the dest wiki's `abs_dir`), following the
  same DB-first NULL-offset commit order as `paragraph_to_file`.
  **Born-applied only** (by REM) — the chassis apply arm refuses it
  loudly; the revert repoints `wiki_id` back to the source + restores the
  prose + re-homes the plan back. Refuses a same-wiki move (that is
  `paragraph_to_file`).
- **`file_to_subwiki`** — take an entire page of a wiki and turn it
  into a new dedicated sub-wiki whose `index.md` carries the page's
  content verbatim. The new wiki id is derived via
  [`WikiId::child_of`] (parent + child slug joined with `-`); the new
  directory lives at `<parent_abs_dir>/<child_slug>/`. Refuses partial
  fact sets — use `paragraph_to_file` if you only want to move some
  of the facts.
- **`page_merge`** — move **every** active fact of one concept page (the
  husk) onto a near-synonym survivor page of the same wiki, **delete the
  husk file**, and re-home the move in the persisted compilation plan
  (husk dropped from plan + registry) — the executing verb of the
  [REM merge sub-job](rem-cycle.md#page-merge-sub-job-semantic-page-consolidation).
  Refuses `index.md` on either side and refuses a partial move (an
  active row left on the husk would be stranded by the delete). The
  spec stores the husk's shell (its contents minus the moved regions)
  plus its title/description/style, so the **revert recreates the
  deleted file** — regions read back off the survivor at their current
  bytes (user edits preserved) — and re-seeds the husk into the plan.
- **`validity_close`** — the only variant that moves nothing: a batch of
  per-fact validity closures (`valid_to` + `decay_reason` stamped by the
  [ingest closure verb](ingest-pipeline.md#the-closure-verb--completion--the-relayed-forget-gesture)).
  **Born-applied only** — the ingest orchestrator applies the closures
  before the receipt exists, so the chassis apply arm refuses the
  variant loudly; the receipt exists for the revert: the spec snapshots
  each target's previous `valid_to`/`decay_reason`, and the revert
  restores them — probing the fact row first, then the still-buffered
  capture (the id is stable across promotion), and skipping a vanished
  target softly instead of failing the batch.

The *move* variants preserve every `fact_id` verbatim. The marker
on disk and the row in `fact_index` keep their UUID across the move; what
changes is the row's `source_path` (always) and `wiki_id` (for the
file → sub-wiki and `fact_refile` variants — both go through
`fact_index::move_to_wiki`, the only `wiki_id`-touching primitive). **All
of them** close with the
[plan re-home seam](narrative-compiler.md#act-first-moves-and-the-plan--the-re-home-seam)
(apply **and** revert) — the emergence re-homes onto the emerged wiki's
`index.md` plan entry; the `fact_refile` re-homes onto the dest page of
the dest wiki (the seam's `RehomePageSeed` natively carries the wiki id,
so cross-wiki re-home is native, force-dirtying both source and dest) —
so the planner's carry-over never fights an applied move or an operator's
revert. `validity_close` needs no re-home:
nothing changes pages — the validity fields are part of the page
fingerprint, so the closure (and its revert) recompiles exactly the
touched page on the next dream.

### Paragraph → file

Default variant. The wiki id is unchanged, so no cross-link rewriting
is required (the rewriter targets ops that change `wiki_id` —
`change_scope` and the file → sub-wiki variant below). See
[`tool-reference.md`](../protocol/tool-reference.md) for the
`structure_proposal` wire surface.

**Inputs on the proposal row** (populated by the emitter — the
auto-promotion REM job):

- `context.source_wiki_id` — wiki the facts currently live in.
- `context.source_page` — page within the wiki (e.g. `index.md`).
- `context.fact_ids` — ordered list of `UUIDv7` facts to move. Order
  is preserved when assembling the appended block on the target page.

**Inputs on the apply call** (populated by the user via the dashboard
confirmation form):

- `answers.target_page` — page within the same wiki where the regions
  should land. Created on first write if it does not exist. Must
  differ from `source_page`.

**Apply algorithm**:

1. Deserialise + validate `context` and `answers`. Both pages must
   pass `wiki::is_safe_page_path`; the two paths must differ.
2. `tree.locate(source_wiki_id)` for the wiki handle, then derive the
   workdir-relative paths of source and target.
3. For each requested fact id: confirm the row is in `fact_index`,
   active (not superseded, not tombstoned), and its `source_path`
   matches the wiki's source page. Any mismatch ⇒ `HandlerData`.
4. Read the source page, parse via `mwe-core::parser`, and grab each
   requested region's full byte slice (the parser already gives us
   `start`, `end`, and `fact_id` on every `Region` event).
5. Build the new target = existing target content + newline (if
   needed) + each region byte slice in `context.fact_ids` order
   separated by newlines. Record per-fact `(new_start, new_end)`
   byte offsets in the composed result.
6. Build the new source = old source with the moved byte spans
   spliced out (spans sorted ascending; cursor walk).
7. **DB rows first, files second** (the capture commit-point pattern):
   for each moved fact, `fact_index::move_region` repoints
   `source_path` at the target with **NULL offsets** — a pending
   render the [reindex orphan sweep](reindex-pipeline.md) spares on
   both pages. A watcher reindex racing the writes below can therefore
   never mistake the in-flight move for a hand deletion of the
   markers. A failure here rolls the already-repointed rows back and
   aborts with nothing changed on disk.
8. `atomic_write` target, then source. Both writes carry their
   `WriteMarker`, and the `WriteMarker` suppression is best-effort
   only (the guard is dropped before the event is consumed) — the
   DB-first ordering above is what actually makes the race harmless.
   A failure here rolls the repointed rows back to the source page
   (best-effort; a row left behind is a pending render the next
   compile re-emits on the target).
9. Stamp the rendered `(new_start, new_end)` offsets onto each row via
   a second `move_region`. Body, embedding, ACL, `created_at`, recall
   counters all stay untouched — same fact, new home.
10. Serialise a `PromoteSpec` (variant + source / target / `target_existed_before`
    flag + per-fact old + new byte offsets) and return it; the chassis
    stamps it onto the proposal row's `spec` column.

**Revert algorithm**:

1. Deserialise the stored `PromoteSpec`. Reject if `variant` is not
   the one this handler ships.
2. Read the target page, parse it, look every fact up by `fact_id`
   (not by stored byte offset — the user may have edited the target
   between apply and revert; the marker survives the edit).
3. Build the new source = current source + newline + appended region
   slices (record per-fact `(new_start, new_end)` offsets). Append at
   end is intentional: exact byte-level restoration of the original
   source layout is not worth the complexity, and the user can
   re-promote if they want a specific arrangement.
4. Build the new target = current target with the moved-back byte
   spans spliced out.
5. Repoint every `fact_index` row back at the source with NULL offsets
   — the same DB-first race shield as the apply.
6. `atomic_write` source, then target.
7. Stamp the rendered source offsets via a second
   `fact_index::move_region`.

**Failure modes worth knowing about**:

- If the apply crashes between the two `atomic_write` calls, the same
  fact briefly lives on both pages on disk. A retry will detect the
  inconsistency in step 3 (the marker is no longer on the source) and
  surface `HandlerData` — the operator can resolve manually. Adding a
  WAL `proposal_ops_log` entry per write would buy back automatic
  recovery; the current scope leans on `atomic_write` per file and
  this being a single-operator deployment (the race window is
  sub-second).
- If the user manually deletes a marker on the target between apply
  and revert, the revert errors `HandlerData` rather than corrupting
  the source. The user can `wiki_capture` the missing fact back if
  needed.

## Concurrency model (single operator)

The chassis is sized for the single-operator homelab target documented
in [`memory-model.md`](../concepts/memory-model.md):

- One Axum process per workdir (lockfile).
- Apply / auto-apply / confirm / revert calls are serialised at the
  SQL layer by the atomic conditional UPDATE. Each transition has a
  guard:
  - `apply` and `auto_apply` flip `WHERE status='pending'` → race
    surfaces as `ApplyError::NotPending { status: "race" }`.
  - `confirm` flips `WHERE status='applied_pending_confirm'` → race
    surfaces as `ConfirmError::NotPendingConfirm { status: "race" }`.
  - `revert(Token)` flips
    `WHERE status='applied' AND revert_token=?` → race or wrong token
    surfaces as `RevertError::InvalidRevertToken`.
  - `revert(Caller)` flips `WHERE status='applied_pending_confirm'`
    → race surfaces as `RevertError::NotRevertable { status: "race" }`.
  - The finalize sweep flips `WHERE status='applied_pending_confirm'`
    AND `confirm_deadline < now` — race with a concurrent user confirm
    or Caller-path revert simply means the sweep's UPDATE matches one
    fewer row (the user got there first), no error needed.
- Filesystem work in a kind handler runs **before** the state flip;
  partial filesystem mutations on handler failure stay as orphans
  (kind handlers must be idempotent on retry).

An `applying` transitional state — to lock the row across the
handler's filesystem work — is **explicitly not** introduced here.
It would be required for a multi-operator deployment that
parallelises apply calls, but no consumer demands it yet. When that
demand appears, the migration is local: add `applying` to
`ProposalStatus`, flip `pending → applying` at handler start,
`applying → applied` (or `applying → applied_pending_confirm` on the
auto-apply path) at handler end, and extend `expire_overdue` to sweep
stale `applying` rows.

## WAL journaling

The chassis itself **does not** write to `proposal_ops_log`: the
transition is a single conditional UPDATE, atomic at the SQL layer,
so there is nothing to roll back at startup.

The shipped handlers (`promote`, `dedup`, `bundle`) lean on
`atomic_write` idempotency rather than
per-step WAL inverses: each can be safely retried after a crash
because the steps either succeed atomically (filesystem rename, SQL
conditional UPDATE) or leave a recoverable orphan that the watcher /
registry re-reads on next boot. The `bundle` revert reverses its
sub-ops one at a time and leans on the same idempotency: a partial
revert (some sub-ops undone, then a failure) leaves the receipt
`applied` and is re-runnable, since `restore_forgotten` is idempotent
and the sub-ops act on independent facts. A true multi-op
*transaction* with all-or-nothing rollback would still want
`wal::begin_proposal_op` + matching `OpInverse` impls; `bootstrap_state`
uses `NoopInverse` for the recovery sweep and relies on handler
idempotency. There is no cross-link rewriter for handlers that change
`wiki_id` (`file_to_subwiki`, `scope::wiki_change_scope`) — the
`wiki_id` stability invariant from
[`memory-model.md`](../concepts/memory-model.md) means existing
`[[wiki_id]]` cross-links keep resolving without rewrite.

## Dedup-merge handler

Lives in [`mwe-core::dedup`](../../crates/mwe-core/src/dedup.rs).
Marks one fact as superseded by another once the `rem_dedup_semantic`
LLM has confirmed the pair. The REM revisor applies it **act-first**
via `apply_dedup_merge_direct` — supersede in-cycle, loser's on-disk
region excised (the retirement disk half, best-effort —
[redaction-policy](../design-notes/redaction-policy.md)), born-applied
receipt, `structure_applied` notice, dashboard revert — the same
authority model as the promote variants; the `pending` + questionnaire
lifecycle stays honoured for rows planted by a non-REM emitter (none
ship today). The **kind handler itself** touches no filesystem state —
the supersede tombstone lives entirely in the `fact_index` table, and a
merge applied through the chassis (manual apply, overdue auto-apply) has
no tree/embedder at hand, so the loser's marker stays on disk there:
fail-closed-redacted for every reader, and cleaned up by the light
dream's retirement hygiene sweep
([rem-cycle](../design-notes/rem-cycle.md)).

**Inputs on the proposal row**:

- `context.loser_fact_id` — the fact that becomes superseded.
- `context.winner_fact_id` — the fact that survives as canonical.
- Any other context field (`similarity`, `loser_summary`, …) is pure
  presentation hint for the dashboard questionnaire and is ignored by
  the handler.

**Inputs on the apply call**: empty. The act of clicking Apply is the
confirmation; there is no answers form. Future variants may add an
optional `direction: "swap"` to let the user flip loser ↔ winner from
the dashboard, but the canonical convention is "the emitter picked
the right survivor (typically the newer fact)".

**Apply algorithm**:

1. Parse the context, validate both fact ids are syntactically valid
   and differ.
2. Look up both rows in `fact_index`. Reject (`HandlerData`) if either
   is tombstoned, if the winner is already superseded, or if the
   loser is already superseded (the inverse can only re-activate what
   *this* apply put down — if the loser is already in a chain the
   operator must resolve it manually).
3. Call `fact_index::mark_superseded(loser, winner)`.
4. Return a `DedupSpec` carrying `loser_fact_id` + `winner_fact_id`
   for the chassis to stamp on the proposal row.

**Revert algorithm**:

1. Deserialise the stored `DedupSpec`. Reject if `variant` is not the
   one this handler ships.
2. Call the new `fact_index::clear_supersede(loser, expected=winner)`
   primitive. The conditional `WHERE superseded_by = ?` is
   load-bearing: if the chain has grown past our pair
   (`old → new → newer`), the clear matches zero rows and we surface
   `HandlerData` rather than orphan `newer`.
3. The loser row is active again. When the direct apply excised its
   region it comes back as a pending render (NULL offsets) — the page's
   next compile re-renders its prose from the DB-authoritative claim
   text; a chassis-applied merge never excised, so its marker is still
   in place.

**Failure modes worth knowing about**:

- Re-applying after a successful apply rejects cleanly with
  `HandlerData("loser is already superseded")`. Idempotency by error,
  not by no-op.
- Reverting after the supersede chain has grown surfaces a clean error
  with a hint to resolve manually. The chassis leaves the proposal in
  `applied` so the operator can still act on it through other tools.

## Bundle handler

Lives in [`mwe-core::bundle`](../../crates/mwe-core/src/bundle.rs). A
`bundle` wraps many born-applied sub-operations into **one** revertible
receipt, so a multi-op change reverts in block. Its only producer today
is the admin page deletion
([`mwe_core::page::delete_page_direct`](../../crates/mwe-core/src/page.rs)), which fills
the spec as it tombstones / evacuates the page's facts.

**No apply path.** A bundle is **born-applied** (emitted straight to
`applied` via `emit_applied_proposal`), never `pending`, so the chassis
`dispatch_apply_kind` arm for `bundle` stays `KindNotYetImplemented` on
purpose — there is no questionnaire and no auto-apply sweep that could
reach it.

**Spec** (`structure_proposals.spec`): `BundleSpec { ops: [BundleOp] }`,
each op tagged and self-describing:

- `Tombstone { fact_id }` — a fact the deleter forgot; the inverse is
  `fact_index::restore_forgotten` (un-tombstone — `deleted_at = NULL`).
- `Refile { spec }` — a fact evacuated cross-wiki; `spec` is the
  `fact_refile` spec, so the inverse is the existing
  `promote::revert_wiki_promote` (`fact_refile` variant).

**Revert algorithm** (`revert_bundle`): deserialise the `BundleSpec`
(reject a non-bundle spec as `InvalidPayload`), then undo each op in
**reverse** application order. The admin/deleter's dashboard undo (within
the revert window) and an admin annul reach it through
`dispatch_revert_kind`. A sub-op failure aborts with `RevertError` and
leaves the receipt `applied` (re-runnable, since the sub-ops are
idempotent and independent — see [WAL journaling](#wal-journaling)).

Deleting a page is **admin authority** (structure is recall shape, not
access);
the receipt's undo is the only post-deletion lever, there is no member
vote over a deletion.

## Fact-forget handler

Lives in [`mwe-core::votes`](../../crates/mwe-core/src/votes.rs). A
`fact_forget` is the **non-sender owner's path** to forget one fact: the
fact's *subject* (`owner`) — or a member of an owning group — who did not
**author** it (a sender deletes directly via
[`acl::can_delete`](../../crates/mwe-core/src/acl.rs)) opens a request the
fact's **audience** votes on (the write-authority model — see
[`identity-and-acl.md`](../concepts/identity-and-acl.md)). This is the
**propose-first inverse** of the act-first bundle deletion above.

**Lifecycle** (`votes::open_forget_request` → `votes::cast_vote` /
sweep):

- **emit (`pending`).** The request opens a `pending` `fact_forget`
  proposal (via the ordinary `emit_proposal`); **the fact stays active** —
  the requester has no authority to remove a contribution they did not
  write. `context = { variant: "fact_forget", fact_id, requester,
  eligible_voters }`; `timeout_at` is the **voting deadline**
  (`now + REVERT_WINDOW`, 7 days). The eligible voters are the fact's
  [`acl::audience`](../../crates/mwe-core/src/acl.rs) (`owner ∪ allow ∪
  {sender}`, groups expanded, `global` dropped — a public fact has no
  finite electorate) **minus the requester**. If that set is empty (the
  requester is the fact's only reader) the forget applies immediately
  (tombstone, no vote).
- **vote (block / early-apply).** `cast_vote` records one final vote per
  voter (reusing `structure_proposal_votes`). A **NO-majority**
  (`no * 2 > eligible`) **blocks** it: the pending proposal is flipped to
  `expired` and the fact stays (`VoteOutcome::Rejected`). Once **every**
  eligible voter has voted with no NO-majority the forget **applies now**
  (`VoteOutcome::Applied`).
- **silence (sweep).** A pending `fact_forget` past its deadline with no
  NO-majority is consent: `auto_apply_overdue_proposals` resolves it
  **straight to `applied`** (it does *not* use the two-window
  `applied_pending_confirm` path — the vote already settled consent; see
  [`auto_apply_overdue_proposals`](#auto_apply_overdue_proposals)). A
  recorded NO-majority at the deadline is `expired` instead (silence is
  consent, a NO-majority is not).

**Apply** (`proposals::apply_fact_forget`): read `fact_id` from the
context and `fact_index::mark_forgotten` it (reason `fact_forget_vote`).
Both apply paths go through `apply_fact_forget_now`, which then **clears
the revert token/deadline** the manual apply minted — a vote-resolved
forget is **final**, so the row never offers an undo and
`dispatch_revert_kind` refuses `fact_forget` defensively. The only way
back is to re-state the fact. The handler is DB-half only (the chassis
carries no tree/embedder): the retirement disk half is stripped act-time
by the paths outside the chassis — the sole-reader immediate apply and
the all-voted `cast_vote` — while a sweep-resolved silence leaves the
bytes (fail-closed-redacted) to the light dream's retirement hygiene
sweep ([redaction-policy](redaction-policy.md)).

The dashboard surfaces it through two agentic verbs: `wiki_request_forget`
(open the request) and `structure_proposal_vote` (cast a vote); see the
[Dashboard surface](#dashboard-surface).

## Dashboard surface

The dashboard is the **sole surface** for proposal writes.
[`crates/mwe-dashboard/src/routes/proposals.rs`](../../crates/mwe-dashboard/src/routes/proposals.rs)
hosts the human-driven side of the chassis on **two** pages, mirroring
the 5-state lifecycle:

- **`/dashboard/proposals`** — the main tray:
  - A **Pending** section with one row per `pending` proposal. For
    `wiki_promote` rows the apply form exposes a single `target_page`
    text input; `dedup_merge` needs no input (click = confirm). A
    `bundle` is born-applied, so it never appears here.
  - An **Applied (revertable)** section with rows still inside the
    7-day revert window (status `applied`, `apply_mode` may be `'manual'`
    or `'auto'` — both reach the revertable state). Each row gets a
    single-click "Revert" button: the dashboard fetches the row's
    `revert_token` server-side and dispatches to
    [`mwe_core::proposals::revert_proposal`] with `RevertAuth::Token`.
    This is the kind-agnostic path a **page-deletion `bundle`** rides for
    the deleter's undo / an admin's annul (`revert_bundle` runs underneath).
  - A **banner** above the tray when one or more rows are
    `applied_pending_confirm`, linking to the dedicated page below.
    The banner text is `"<N> auto-apply in attesa di conferma"` and is
    rendered only when the count is > 0.
- **`/dashboard/proposals/pending-confirms`** — the
  tray for `applied_pending_confirm` rows:
  - One row per `applied_pending_confirm` proposal, with `proposal_id`,
    `kind`, `applied_at`, `confirm_deadline` columns plus two
    destructive buttons: "Conferma" (POSTs to
    `/dashboard/proposals/:id/confirm` → `proposals::confirm_proposal`)
    and "Annulla" (POSTs to `/dashboard/proposals/:id/revert` with no
    body — the dashboard's revert handler is status-aware and
    auto-routes to `RevertAuth::Caller { sender, is_admin }` for this
    state).
  - Lists the `applied_pending_confirm` rows addressed to the session
    user, plus the unaddressed / admin-fallback rows; an admin sees all
    (0032). The "addressee or admin" rule is enforced in the chassis by
    `recipient_can_act`; a non-recipient non-admin caller is refused
    with `RevertError::RevertNotAuthorized`.

The dashboard revert handler is a single endpoint
(`POST /dashboard/proposals/:id/revert`) that selects the right
`RevertAuth` based on the row's `status` (a `SELECT status,
revert_token` precedes the call): `applied` → `Token(stored_token)`,
`applied_pending_confirm` → `Caller(session.sender_id)`. After the
flip, the user is returned to the page that matches their original
context (main tray for Token path, pending-confirms for Caller path).

The **operative chat** is a second in-process driver of the same
chassis: `structure_proposal_revert`
([`agentic-chat.md`](agentic-chat.md#revert-in-the-chat))
replicates this exact status → `RevertAuth` mapping in
`dispatch_proposal_revert`, adds the 0032 recipient gate (a non-admin
reverts only proposals addressed to them), and maps the refusal
variants (a per-kind guard refusing via `HandlerData`) back to the model
as ordinary tool refusals. This is the path behind undoing an applied
proposal through the chat
(see [agentic-chat.md](agentic-chat.md)).

After apply, the page reloads with a flash that surfaces the freshly
minted `revert_token` for audit visibility. After confirm the same
flash shape applies — the token minted at confirm is just as valid
within the 7-day window starting at `confirmed_at`. Apply / confirm /
revert errors map to inline flash banners (one per submit), never to
500 pages — every classified failure mode is recoverable by editing
the form and re-submitting.

The token remains a real anchor in `structure_proposals.revert_token`
for audit, but the dashboard UX never asks the user to paste it
anywhere — the revert is single-click on the row, and there is no
MCP write surface, so no cross-client revert via token either.

## Consumer warnings via `wiki_ingest_message`

With the proposal tools off the MCP surface, the consumer agent (the
LLM the user actually talks to) is otherwise in the dark about open
questionnaire proposals between turns: there is no proposal list to
poll. This surface closes that gap by riding on the conversational turn
the agent is already making.

Every call to `call_wiki_ingest_message`
([`crates/mwe-mcp-server/src/mcp/tools.rs`](../../crates/mwe-mcp-server/src/mcp/tools.rs))
runs [`mwe_core::proposals::count_in_flight`] after the ingest body
returns. `count_in_flight(pool, recipient, now)` snapshots three
in-flight classes in one query — `pending`, `applied_pending_confirm`,
and `revertable_applied` (`status = 'applied'` whose `revert_deadline`
is still open at `now`) — and its `total()` sums all three. The `now`
parameter is explicit (not `datetime('now')`) so callers and tests
control the clock against which the revert window is measured.

The ingest warning deliberately ignores the third class: it gates on
`pending + applied_pending_confirm` only, because its job is "don't pile
more state on top of an *unconfirmed* change", and an applied proposal
within its open revert window is already applied (and already surfaced on
the dashboard badge). Folding it in here would fire the warning on every
just-applied proposal — noise. The dashboard in-flight badge, by contrast,
reads `total()` (all three), because there "everything you can still act on"
is exactly the right scope.

When the unconfirmed count is non-zero, the response payload gains an
extra `pending_attention` block:

```json
{
  "pending_count": 2,
  "applied_pending_confirm_count": 1,
  "dashboard_path": "/dashboard/proposals",
  "note": "scoped_to_recipient"
}
```

When the count is zero, the field is absent — we keep the default
wire shape quiet so existing consumers do not see noise on every
turn.

Design constraints baked into this surface:

- **Structured, not prosaic.** The block carries counts + a path,
  never a pre-rendered sentence. The consumer agent composes the
  user-visible warning in the user's locale ("hai N proposte in
  attesa…" / "you have N pending proposals…") from these primitives.
  Same reasoning as the rest of the wire shape: mwe-mcp talks
  machine, the consumer talks human.
- **Scoped to the recipient (0032).** `count_in_flight` takes the
  caller's recipient principal: a non-admin counts only the rows
  addressed to them (`recipient_id = "user:<id>"`) plus the unaddressed
  / admin-fallback ones (`recipient_id IS NULL`); an admin gets the
  deployment-wide count. The path through `tools.rs` passes
  `identity.sender_id` (or `None` for an admin) straight through, and
  the `note` field reads `"scoped_to_recipient"`. The recipient is the
  same `recipient_id` column the dashboard tray and
  `structure_proposal_list` filter on (see `recipient_from_fact` /
  `recipient_can_act`).
- **`dashboard_path` is a path, not a signed URL.** The block is a
  hint, not a one-click action: the consumer either composes the
  full URL by calling `dashboard_link` with `intent: "home"` and
  pointing the user at `/dashboard/proposals`, or surfaces the path
  raw on an already-authenticated dashboard. Avoiding a fresh signed
  token here keeps `wiki_ingest_message` from minting JWTs on every
  turn whose only purpose is a warning.
- **Best-effort, not strictly transactional.** The count is read
  *after* the ingest body has already committed any capture, so it
  is a snapshot at end-of-turn — the consumer is told about
  proposals that exist *now*, not proposals that existed when the
  turn started. This is correct: the warning's role is "don't pile
  more state on top of unresolved proposals", which is exactly the
  state the next turn will face.

The block lives alongside `intent_classified`, `suggested_seed`,
`capture_id` and friends — it is part of the public wire shape of
`wiki_ingest_message`, documented in
[`tool-reference.md`](../protocol/tool-reference.md) and
[`mcp-tools.md`](../protocol/mcp-tools.md).
The dispatcher integration test
`structure_proposal_in_flight_surfaces_warning_in_wiki_ingest_message_response`
in [`crates/mwe-mcp-server/tests/dispatcher.rs`](../../crates/mwe-mcp-server/tests/dispatcher.rs)
pins the contract: seed two `pending` + one `applied_pending_confirm`
plus three terminal rows (`applied`, `reverted`, `expired`), call
`wiki_ingest_message` with the test-fakes ingest backend
(`backend="fake"`, `model=<canned response>`, gated on the
`test-fakes` Cargo feature), and assert the block carries
`pending_count=2`, `applied_pending_confirm_count=1`, the right
`dashboard_path`, and the `note` tag. A sibling test
`wiki_ingest_message_omits_pending_attention_when_no_proposals_in_flight`
locks the silence-when-zero invariant.

## Test coverage

The chassis + sweep behaviors are covered by tests in four layers:

- **Unit (`crates/mwe-core/src/proposals.rs`)** — covers state machine
  invariants per helper: `mark_applied` / `mark_auto_applied` /
  `mark_confirmed` happy paths + race conditions, `confirm_proposal`
  not-found / not-pending-confirm / window-expired, `revert_proposal`
  dispatch on `(prior_status, auth)` pairs (Token + Caller paths,
  wrong-pair refusals, empty caller, expired confirm window),
  `auto_apply_overdue_proposals` skip-within-window + soft errors,
  `auto_finalize_unconfirmed_proposals` skip-within-window +
  flip-past-deadline (asserts no `revert_token` minted, no event
  emitted) + idempotence, `expire_overdue_proposals_at` grace boundary.
- **REM integration (`crates/mwe-core/src/rem.rs::tests`)** — two
  end-to-end scenarios on a real workdir + dedup setup:
  `auto_apply_sweep_applies_dedup_merge_past_timeout` asserts the row
  lands on `applied_pending_confirm` with `apply_mode='auto'` and a
  `wiki_events.kind='auto_applied'` row appears;
  `auto_finalize_sweep_locks_dedup_merge_past_confirm_deadline` runs
  two cycles (auto-apply, then finalize after back-dating
  `confirm_deadline`) and asserts the loser **stays superseded**
  (silence = consent, the kind handler is not re-invoked), status =
  `applied`, no `revert_token`, no `revert_triggered_by`, and only
  the `auto_applied` event from cycle 1 — nothing from cycle 2.
- **MCP dispatcher
  (`crates/mwe-mcp-server/tests/dispatcher.rs`)** — covers the tool
  surface: the whole `structure_proposal_*` family (`_list` / `_apply` /
  `_confirm` / `_revert`) is not registered and call attempts return
  `not_found`.
  `structure_proposal_in_flight_surfaces_warning_in_wiki_ingest_message_response`
  seeds a mix of in-flight + terminal rows, runs
  `wiki_ingest_message` against the test-fakes ingest backend, and
  asserts the `pending_attention` block carries the right counts +
  dashboard path; the paired
  `wiki_ingest_message_omits_pending_attention_when_no_proposals_in_flight`
  test locks the silence-when-zero invariant.
- **Dashboard integration
  (`crates/mwe-dashboard/tests/`)** — the action
  routes (`POST :id/apply` / `_confirm` / `_revert`) are exercised
  through the chat / bridge. The in-flight badge's
  JSON endpoint is covered by `proposals_in_flight_count.rs`: the JSON
  shape, all three counted classes (`pending`,
  `applied_pending_confirm`, and `applied`-with-open-revert-window), the
  silence/auth behaviour, and the recipient ACL scoping (admin sees the
  deployment; a non-admin sees only rows addressed to them plus the
  unaddressed ones).

## Current limitations

These are gaps in the present surface (planned — see the
roadmap):

- **Remaining per-kind handler** (`bundle`) — the multi-op transaction coordinator is not implemented; it needs a real use case that requires several ops in one atomic apply.
- **Dashboard questionnaire UI** — the apply forms are flat textarea-style submits. The full multi-step questionnaire (radio groups with `recommended` options + per-stage validation) is not yet built.
- **Cross-link rewriter** — see the Cross-link concern above. It needs its own design to handle ambiguous wikilink targets, alias forms, and prose-vs-marker discrimination.
- **Per-kind `confirm_window` differentiation** — proposals from REM are conservative (default 7gg), but a proposal emitted from a live `wiki_ingest_message` could plausibly want a shorter window because the user is actively engaged. The default is uniform 7gg regardless of emitter; per-emitter differentiation is not yet supported.
- **Per-proposal recipient scoping** — **shipped (0032)** as the `recipient_id` addressee column. The tray, the dashboard agentic `structure_proposal_list`, and `pending_attention` scope to "addressed to me **or** unaddressed" for a non-admin (admins see all); `recipient_can_act` gates apply / confirm / revert, and `RevertError::RevertNotAuthorized` fires for a non-recipient non-admin caller. REM derives the addressee with `recipient_from_fact`.
- **MCP write surface for proposals** — not exposed, on the grounds that confirming or annulling a structural change requires the dashboard's context (full diff, downstream effects, audit trail). If a consumer agent has a legitimate use case for cross-client write access (e.g. an admin CLI that needs to confirm proposals over MCP without a browser), the chassis primitives are already there; the only work is re-exposing them through MCP and re-introducing the per-error-class wire mapping.
- **Dedicated finalize ticker** — the REM scheduler default ticks every 24h, and `confirm_window` of 7gg gives ample margin. There is no separate shorter ticker for the tighter window user-initiated proposals would want.
- **Per-step WAL journaling inside the `wiki_promote` handler** — `atomic_write` is enough for the single-operator floor; per-step `proposal_ops_log` + per-step inverse driver are not present and would be needed only for a multi-writer scenario.
