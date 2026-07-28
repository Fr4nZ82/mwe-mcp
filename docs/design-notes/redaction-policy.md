---
title: Redaction policy — region-level ACL and per-sender rendering
area: design-notes
status: implemented
last_review: "2026-07-02"
---

# Redaction policy — `mwe-core::acl` + `mwe-core::render`

This page is the canonical, self-contained description of mwe-mcp's
**region-level** access control: the `can_read` predicate that decides
whether a single sender may see a single fact-region, and the
`render_for_sender` projection that walks a markdown file region by
region and emits a declassified view. Both functions are pure — all
I/O (loading the page's ACL map from the engine DB) stays with the
call sites; the authoritative source is the code:

- [`crates/mwe-core/src/acl.rs`](../../crates/mwe-core/src/acl.rs) — `can_read`, `RegionAcl`, `FactAclMap`
- [`crates/mwe-core/src/render.rs`](../../crates/mwe-core/src/render.rs) — `render_for_sender`, `RenderOutput`
- [`crates/mwe-core/src/fact_index.rs`](../../crates/mwe-core/src/fact_index.rs) — `page_acl_map` (the per-page ACL-map loader)

For the data model behind it (principals, ownership vs. attribution,
the owner-of-last-resort rule) see
[`../concepts/identity-and-acl.md`](../concepts/identity-and-acl.md)
and [`../concepts/memory-model.md`](../concepts/memory-model.md); for
the marker syntax that produces the regions this page gates see
[`marker-grammar.md`](marker-grammar.md).

## The region-level ACL is the only access gate

There is **one** read-side ACL gate, and it is the subject of this page:
`render_for_sender` admits a sender to a file and then gates each
`{{…}}…{{/}}` region individually by its own ACL. There is no wiki-level
access gate above it — a whole wiki or page is never refused as a unit.
Wiki/page visibility is instead **derived** from this per-region predicate
(a wiki/page surfaces to a reader iff it holds at least one region that
reader `can_read`); the model is at
[`../concepts/identity-and-acl.md` §5](../concepts/identity-and-acl.md#5-wiki-visibility-is-derived--there-is-no-wiki-level-access-gate).

**This redaction applies to standard memory wikis** — smart (project)
wikis carry no per-fragment regions, so their reads are governed by the
**wiki-level ACL** (owner = the wiki's resolved `scope`, allow =
`shared_with`) projected onto their content-indexed `fact_index` rows and
checked through the same `can_read`; see [`smart-wikis.md`](smart-wikis.md).

## `can_read` — the region-level predicate

`can_read` answers one question: *may this sender see this region?* It
is a pure function with no I/O, no database access, and — critically —
**no admin bypass**. `isAdmin` is a consumer-side UI hint, never a
server-side override: the function has no `is_admin`
parameter, so by construction the API — and every MCP tool that funnels
through `render_for_sender` (`wiki_read`, `wiki_search`, recall) — cannot
grant blanket access. The one operator-only exception lives entirely in
the *presentation* layer, never touches this predicate, and is described
under [Dashboard admin reveal](#dashboard-admin-reveal).

```rust
pub fn can_read(
    region_acl: &Acl,
    sender_id: &str,
    sender_groups: &[String],
    sender_of_region: Option<&Principal>,
) -> bool
```

The algorithm reduces to a single rule. Build the **effective
principal set** and check whether *any* of its members matches the
current reader:

```
effective = region_acl.owner  ∪  region_acl.allow  ∪  {sender_of_region}
can_read  = ∃ p ∈ effective . principal_matches(p, sender_id, sender_groups)
```

It is a **union**, not an intersection — `owner`, every `allow=`
entry, and the capturing `sender` are each independently sufficient to
grant access. `principal_matches` is the obvious per-principal test:

| Principal | Matches when |
|---|---|
| `Global` | always — the region is readable by every authenticated sender |
| `User(id)` | `id == sender_id` |
| `Group(id)` | `sender_groups` contains `id` (group membership comes from [`enrollment::groups_for`](../../crates/mwe-core/src/enrollment.rs)) |

Properties pinned by the proptest suite in `acl.rs`:

- **Global anywhere in the set ⇒ visible to anyone.** A `Global` owner,
  a `Global` in `allow`, or a `Global` capturing-sender each open the
  region universally.
- **`owner = User(sender)` always reads.** A user always rereads a
  region they own.
- **Monotonicity in `allow`.** Adding a principal to `allow` can only
  grant visibility, never revoke it (`before ⇒ after`).
- **The capturer always rereads** (see `sender_of_region` below),
  regardless of the rest of the ACL.

### `owner = None` is resolved *before* `can_read`

The marker grammar lets a region omit `owner=`, in which case
[`Acl`](../../crates/mwe-core/src/types.rs) carries `owner: None`. The caller (always `render_for_sender`)
substitutes the **owner of last resort** — the region's own `sender` —
into the slot *before* the check; the wiki's `scope` is placement only
and is never consulted as a region's ACL fallback. `can_read` itself is
a pure ACL evaluator with no notion of inheritance. If you ever call
`can_read` with `Acl { owner: None, allow: [] }` directly, it denies
everyone except a matching `sender_of_region` — that is the documented
contract, not a bug.

### `sender_of_region` — cross-user attribution as a third union element

`sender=<principal>` on a marker records **who captured** the region,
as distinct from `owner=`, which records **whose fact it is**. The two
dimensions are orthogonal (`sender ⊥ owner`). `can_read` folds the
capturing sender into the effective set as one more principal, and it
is a full `Principal` — `User`, `Group`, or `Global`, not a scalar
user id:

- **`sender=user:galadriel`** — Galadriel captured a fact about Gollum
  on behalf of herself; she always rereads it (personal audit trail),
  even when she is neither the owner nor in `allow`.
- **`sender=group:famiglia`** — the "family microphone" case: an
  ambient capture device attributed to the household group. *Every*
  member of `famiglia` rereads the region via the sender shortcut,
  even when `famiglia` is absent from `allow=`.
- **`sender=global`** — a public capture device. Functionally
  equivalent to making the region public: anyone reads it.

`render_for_sender` passes `attrs.sender.as_ref()` straight through.
`sender` is materialized at capture time — a fact is born with
`sender_id` explicit (= `owner` for the common "user talks about
themself" case), never collapsed — so this predicate sees a concrete
principal, not an implied one. A `sender_id = NULL` in the DB is the
degenerate "scrubbed" state (e.g. a deleted user) and, as documented
under `owner = None` above, the union still admits the owner. See
[`marker-grammar.md` §5](marker-grammar.md#5-cross-user-attribution).

## `render_for_sender` — region-by-region projection

```rust
pub fn render_for_sender(
    text: &str,
    db_acl: &FactAclMap,           // authoritative fact-key → ACL map (engine DB)
    meta_acl_default: &Principal,  // kept for signature stability; not consulted
    sender_id: &str,
    sender_groups: &[String],
) -> RenderOutput

pub struct RenderOutput {
    pub text: String,          // declassified markdown for the sender
    pub blocks_redacted: usize // count of regions replaced by a placeholder
}
```

`render_for_sender` parses the file into a stream of `Prose`, `Region`,
and `Embed` events and projects each one onto the sender's view:

| Event | If visible to sender | If invisible to sender |
|---|---|---|
| **Prose** (text outside any marker) | included verbatim | *cannot be invisible — prose always passes* |
| **Region** (`{{…}}…{{/}}`) | body included verbatim, `visible_regions += 1` | replaced by the inline placeholder `[redacted]`, `blocks_redacted += 1` |
| **Standalone embed** (`{{embed=…}}` in prose) | included verbatim | *cannot be invisible — passes with the prose* |
| **Embed inside a region** | follows the region's fate (kept/replaced wholesale) | follows the region's fate |

For each region the function resolves the gating ACL (next section),
then calls `can_read`. Visible → emit the body; invisible → emit the
placeholder. `blocks_redacted` is the per-region redaction count,
useful for a UI badge or telemetry; prose and standalone embeds never
contribute to it.

### Where the region ACL comes from — DB first, by fact key

The engine DB is the **authoritative** source for a region's ACL — and
for all per-fact metadata (owner/ACL, validity, topics, salience), the
runtime marker being the bare region key (see
[marker-grammar.md §0](marker-grammar.md#0-runtime-form-vs-export-form--what-gets-written-when)).
Resolution per region, implemented in `resolve_region_acl`:

1. **DB record** — when the region's `f=<uuid>` key is present in the
   `db_acl` map, the `fact_index` record (`owner_id` / `allow_ids` /
   `sender_id`) gates the region **alone**. The owner is always
   explicit in the DB, so no fallback applies, and the inline
   marker attributes are ignored even when present — they are a
   derived projection of the DB, not the source of truth. This is
   what makes the bare runtime marker (`{{f=uuid}}`, no attributes)
   enforceable, and it means an ACL edit in the DB takes effect at
   the next read even before the file is rewritten.
2. **Inline fallback** — a region the map does not cover (a file not
   yet indexed, or a marker without `f=`) is gated by its inline
   attributes, with the region's own **`sender`** as the owner of last
   resort when the marker omits `owner=` (never the wiki's scope
   principal). A region with neither an inline owner nor a sender is
   left **unreadable** rather than inheriting a category-wide audience.
   An **empty map therefore reproduces the pure inline-attribute
   behaviour** — which is how text that never went through capture is
   rendered.

The map is loaded per page (a slim query — no embedding/text columns) in
two variants. The **reader/redaction** paths — recall-by-navigation
(`recall_nav.rs`), `wiki_read`, and the dashboard's normal per-user page
render — use `fact_index::page_acl_map_active(pool, source_path)`, which
**excludes superseded and tombstoned rows**: a region left on disk after its
fact was retired is no longer in the map, so a bare `{{f=uuid}}` marker falls
through to the owner-of-last-resort, finds neither an inline owner nor
sender, and **redacts fail-closed** — no reader surface ever renders a stale
or contradictory region whose fact the DB has already retired, not even to
its last-known audience. The full `fact_index::page_acl_map` (retired rows
kept, their last-known gate preserved) is used in exactly two places:
**interchange** — export rewrites every on-disk region to its full-marker
form, deliberately carrying whatever retired residue is still on disk with
its last-known ACL — and the **dashboard admin reveal**, so the operator
supervision lens can still see retired residue (highlighted per its
last-known gate). Both variants key on `source_path` **alone** (the
workdir-relative path identifies one physical file; `fact_id` is the PK so
the key never collides).

Retirement also has a **disk half**: `reindex::strip_fact_region` excises
the retired region's `{{f=id}}…{{/}}` bytes from its page (refusing active
rows, so a racing revert can never lose live prose), re-syncs the surviving
markers via the reindex, and settles the retired row's offsets to NULL. It
runs **act-time, best-effort** (a failure warns and never fails the retire —
the residue stays fail-closed-redacted) on every retire path that has the
engine context at hand:

- `capture::wiki_supersede` — the consumer/chat supersede;
- `capture::wiki_forget` — the consumer `wiki_forget` tool, the dashboard
  chat forget verb, the dashboard fact **delete** button, and the REM
  comment-channel `remove` all funnel here;
- the consumer `wiki_forget_bulk` (per collected fact after the bulk
  tombstone);
- the light dream's classifier **supersede hint** (`dream_light`);
- the REM revisor's act-first **dedup merge** (`dedup::apply_dedup_merge_direct`
  — the loser's bytes);
- the `fact_forget` vote resolutions that run outside the apply chassis:
  the sole-reader immediate apply and the all-voted consent
  (`votes::open_forget_request` / `votes::cast_vote`).

The retire paths that run **inside the proposal apply chassis** — a pending
`dedup_merge` applied manually or by the overdue auto-apply, and the
silent-deadline `fact_forget` sweep — carry no tree/embedder and do not
strip; their residue is picked up by the light dream's **retirement hygiene
sweep** (`reindex::sweep_retired_regions`, [rem-cycle.md](rem-cycle.md)),
which strips retired regions from pages **outside the compilation plan**
(`rules.md`, husks — where residue would otherwise be permanent; plan pages
self-clean at their next compile). The reindex pipeline's own
file-removed / marker-removed tombstones are deliberately **not** strip
sites — there the disappearance of the file or marker is the trigger, not a
leftover. Page writes are not serialized per page, so two concurrent strips
can lose one excision — degraded, not corrupting (the sweep converges it);
the wider fix is the concurrency hardening of roadmap group 4e.

### Prose is context, not a fact-region — the gate never touches it

The load-bearing model rule this implementation depends on: **free
prose between markers is the file's narrative scaffolding, and it
always passes through to every sender, every time.** Only the
`{{…}}…{{/}}` regions are gated.

The rationale is twofold. The wiki is written in flowing prose so the
consumer LLM can understand the context of a region it is about to
extract or supersede; and a human reader needs the words around a
redacted block to make sense of the sentence. If the gate
filtered prose, an inline-granularity sentence like *"Alice pesa
{{owner=user:alice}}72 kg{{/}} al 10 maggio."* would lose its
surrounding words for Bob, leaving only the visible region body
floating in nothing. Prose is treated as scaffolding and only the
regions are gated. The dedicated regression test
`inline_granularity_preserves_surrounding_words` locks this
behaviour.

Because a compiled page's prose is **written by the Cronista from that
page's facts**, "prose passes to everyone" is only safe if that prose
carries nothing a reader may not see. That discipline lives in the
**compiler**, not in this gate: the Cronista is handed a per-fact
`(audience: …)` hint (its read-set, from
[`compiler::audience_hint`](../../crates/mwe-core/src/compiler.rs)) and
instructed to keep a **restricted** fact's substance inside its own
`<fN>…</fN>` span — where this gate redacts it — leaving the untagged
connective prose free of restricted-fact content (see
[narrative-compiler.md §Il Cronista](narrative-compiler.md#il-cronista--the-leaf-writer-strong-model)
and the `cronista` prompt's FACT TAGS rule). The render gate stays a
pure per-region projection; the compiler is what keeps the
default-visibility prose safe to pass.

Concretely:

- The owner of last resort for a region whose marker omitted `owner=`
  is the region's own **`sender`** (its captured provenance) — never the
  wiki's scope principal: a fact's ACL is the fact's, not the category's.
  A region with no inline owner **and** no sender stays unreadable. This
  fallback fires on a not-yet-indexed file or a marker without `f=`; once
  the region is in `fact_index` the DB record's explicit owner gates it.
- The gate does **not** filter free prose. A heading or a
  separator paragraph always reaches the sender, regardless of whose
  wiki the file lives under.
- The gate does **not** filter standalone embeds in prose. An
  embed sitting between paragraphs is part of the file context, not a
  fact-region. The **bytes** behind an embed are gated separately, at
  `GET` time, by the media catalog's own ACL — a visible marker with
  denied bytes is the intended state
  ([media pipeline](media-pipeline.md)).

`meta_acl_default` is threaded to the call sites (resolved from
`WikiTree::resolve_scope_principal`) for signature stability, but the
render path does not consult it: the owner of last resort is the
region's `sender`.

### Inline placeholder, not block callout — and the deliberate existence leak

An invisible region is replaced by the inline marker `[redacted]`
sitting exactly where the region body was, **not** by a block-level
callout. This is intentional: an inline region inside a sentence must
keep the sentence flowing, otherwise the reader sees one sentence
visually shredded across three paragraphs.

Concrete comparison on the canonical inline-granularity example
(*"Alice pesa {{owner=user:alice}}72 kg{{/}} al 10 maggio, ha
{{owner=global}}tagliato i capelli{{/}} ieri."*) viewed by Bob:

- **Block callout (rejected)** — three paragraphs:
  ```
  Alice pesa

  > [!redacted]
  > Blocco non visibile (ACL).

  al 10 maggio, ha tagliato i capelli ieri.
  ```
- **Inline placeholder (implemented)** — one sentence:
  ```
  Alice pesa [redacted] al 10 maggio, ha tagliato i capelli ieri.
  ```

The placeholder preserves the intent — **mark the
redaction so the reader does not silently miss it, and leak the
existence and position of the hidden block, never its content.** For
block-level regions (a marker sitting between two `\n\n` paragraph
breaks) the inline `[redacted]` naturally lands on its own line, so the
visual is right for that case too. The whole-page collapse (next
section) is the one place a block-level callout is still used, because
there is no surrounding sentence to keep flowing.

## Total-redaction collapse

When everything meaningful in a file is invisible, emitting N
consecutive `[redacted]` markers would leak the exact count of hidden
regions. To avoid that, `render_for_sender` collapses the output to a
single block-level callout when **all** of the following hold:

- the file has at least one region (`n_regions > 0`),
- every region was redacted (`visible_regions == 0`),
- and no prose with non-whitespace content remains to anchor the
  output (`!has_meaningful_prose`).

In that case `text` becomes exactly:

```
> [!redacted] This entire page is private.
```

If any of the three conditions is false the output is the usual
`prose ⊕ region bodies ⊕ [redacted] placeholders`. In particular, a
file with a heading or any other non-whitespace prose is **not**
collapsed even when every region is redacted — the sender still sees
the scaffolding and N placeholders. That trades the count leak (the
heading already implied something exists) for keeping the visible
scaffolding.

### The collapse is observable only through `text`

`RenderOutput` exposes no `fully_redacted` boolean. The collapse
detection is internal to `render_for_sender` and surfaces only through
the returned `text`:

| Observable | Interpretation |
|---|---|
| `text != FULLY_PRIVATE_CALLOUT` | Page rendered as `prose ⊕ region bodies ⊕ inline [redacted]`. Ship `text` directly. |
| `text == FULLY_PRIVATE_CALLOUT` | All regions redacted with no anchoring prose; `blocks_redacted` still carries the per-region count for telemetry. Ship `text` (soft path). |

Dropping the collapse entirely was considered and rejected precisely
because it would re-introduce the count leak.

## Call sites

Every consumer of `render_for_sender` loads the page's **active** ACL map
via `fact_index::page_acl_map_active` (retired regions redact
fail-closed), resolves `sender_groups` via
[`enrollment::groups_for`](../../crates/mwe-core/src/enrollment.rs),
and passes `RenderOutput` through verbatim:

- **`wiki_read` MCP tool**
  ([`crates/mwe-mcp-server/src/mcp/tools.rs`](../../crates/mwe-mcp-server/src/mcp/tools.rs))
  ships `text` as `content_rendered_for_sender` and `blocks_redacted`
  as `redacted_count`. No `fully_redacted` field is emitted. A failed
  ACL-map load is a **hard error** — serving the page on weaker gating
  would be a leak, not a degradation.
- **Dashboard viewer**
  ([`crates/mwe-dashboard/src/routes/wiki_view.rs`](../../crates/mwe-dashboard/src/routes/wiki_view.rs))
  renders `text` inside the markdown preview and shows a muted
  "Showing the declassified view for &lt;sender&gt; — N region(s)
  replaced by `[redacted]`" banner whenever `blocks_redacted > 0`.
  Same hard-error stance as `wiki_read`. The operator can switch this
  surface into the reveal mode below — the one render that loads the
  **full** map instead, so retired residue stays visible to the
  supervision lens. It is also the one caller of the **segmented
  variants** (`render_for_sender_segments` / `render_admin_reveal_segments`
  in [`render.rs`](../../crates/mwe-core/src/render.rs)): same policy,
  identical joined text (the plain functions *are* the segmented ones
  joined), but each shown region arrives as its own
  `(text, Option<fact_id>)` segment — the id present only when the region
  is shown **and** its key is in the loaded map — so the dashboard can
  append the region → fact-record anchor
  ([dashboard-memory-mvp.md §Wiki view](dashboard-memory-mvp.md#wiki-view)).
  A redacted region's placeholder is fact-less by construction: no anchor,
  no id leak. Consumer surfaces keep the plain `RenderOutput`.
- **Recall navigator funnel**
  ([`crates/mwe-core/src/recall_nav.rs`](../../crates/mwe-core/src/recall_nav.rs))
  projects every page it opens before the navigator LLM sees it. Here
  a failed map load **skips the page** (same soft-fail class as an
  unreadable file): recall degrades, the turn survives, and the page
  is never rendered on weaker gating.

## Dashboard admin reveal

The dashboard — and **only** the dashboard — offers the admin a
per-browser **ACL-reveal** toggle: a single control on the
[Settings page](dashboard-frontend.md) (`/dashboard/settings/me`) that
governs every reveal-aware surface. The logic lives in one place,
`mwe_dashboard::reveal`; a surface opts in by consulting `reveal::active`
and skipping its ACL projection when it returns true. Four surfaces
honour it today:

- **The memory-wiki read surfaces** (`/dashboard/wiki/:id` and
  `/dashboard/wiki/:id/view/*path`) render through
  `render::render_admin_reveal` instead of `render_for_sender`: every
  region body is shown, and the ones the viewer could *not* read are
  highlighted (amber) rather than replaced by `[redacted]`. Reveal also
  loads the **full** ACL map (retired rows kept) where the normal view
  loads the active one — so a retired region still on disk, invisible to
  every normal reader, remains inspectable here, highlighted per its
  last-known gate.
- **The facts table** (`/dashboard/facts`) lists **every** user's facts
  instead of only the reader's ACL-projected set, so the owner-or-admin
  structured fact actions (ACL / validity / delete — see
  [dashboard-memory-mvp.md](dashboard-memory-mvp.md)) can **reach**
  another user's fact. Without reveal those facts are filtered out of the
  list and the per-fact form 404s, so the actions are unreachable.
- **The in-flight proposals** — the topnav badge count
  (`/dashboard/proposals/in-flight-count`) and the chat read tools
  `structure_proposal_list` / `structure_proposal_get`. By default every
  operator — admins included — is scoped to their own proposals (those
  addressed to them plus the unaddressed / admin-fallback bucket); reveal
  lifts an admin to the whole deployment. A proposal's `context` carries
  the underlying fact text, which is per-fragment ACL'd and is **not**
  re-projected per reader, so an unconditional admin-wide listing would
  leak other users' content — hence the same reveal gate as the facts
  table. The act tools (`apply` / `confirm` / `revert`) are unchanged:
  an admin may still act on any proposal *by id* (the addressee gate
  admits admins), so reveal governs discovery, not authority.
- **The recall-traces journal** (`/dashboard/admin/recall-traces`, its
  viewer and its `/data` feed). By default every operator — admins
  included — sees only the traces whose `sender_id` is themself; another
  user's trace is `404` on the viewer and on the feed (not `403`: the id
  space is a dense autoincrement, so a `403` would confirm the row
  exists). A trace journals the recalled fact bodies verbatim and is not
  re-projected per reader, so the exposure class is the facts table's;
  reveal lifts an admin to the whole journal. See
  [dashboard.md](dashboard.md).

A banner marks the mode so a revealed-private fragment / fact is never
mistaken for a public one.

Reveal is a deliberate, banner-marked admin supervision lens, fenced on
three sides:

- **It never touches `can_read`.** `render_admin_reveal` still *calls*
  `can_read` per region — only to decide which fragments to highlight,
  then shows them anyway; the `/facts` fetches pass an explicit `reveal`
  flag into `recall::wiki_facts_full_for` / `wiki_buffered_full_for` that
  skips the per-row gate. The predicate itself keeps its no-admin-bypass
  invariant (and the `admin_does_not_bypass` test stays green).
- **It is gated server-side on the admin role, and the deployment can
  withdraw it entirely.** The toggle is a cookie (`mwe_admin_reveal`),
  but it is *honoured* only when the session's `is_admin` is true **and**
  `instance.admin_reveal_locked` is unset (`reveal::active`); a forged
  cookie on a non-admin session, or on a locked deployment, does nothing.
  The cookie merely records the on/off preference. See
  [The machine operator can lock reveal](#the-machine-operator-can-lock-reveal).
- **It is dashboard-only.** No MCP tool can reach it; `wiki_read` /
  `wiki_search` / recall — and the `pending_attention` in-flight count on
  the `wiki_ingest_message` response — always honour the ACL, scoped to
  the calling identity even for an admin consumer. Reveal changes only
  what the *admin* sees and can act on **through the dashboard**, never
  what a consumer agent can read.

### The machine operator can lock reveal

Reveal is gated on the admin role, and that is the right gate exactly
when the admin *is* the household. It is the wrong gate whenever the
deployment's point is that the admin **cannot** read what members did not
share with them.

`mwe-mcp.config.yaml > instance.admin_reveal_locked`
([config-schema.md § `instance`](../protocol/config-schema.md#instance))
is the switch that separates the two roles: **who administers the panel**
and **who runs the machine**. In a household they are the same person and
the distinction costs nothing. In an office they are not — the manager
can hold the dashboard admin account without having a shell on the host —
and there the whole value is in the gap: the admin keeps the deployment,
the members keep their private fragments. The section has **no dashboard
editor by design**; a switch a panel admin can flip is not a switch that
constrains a panel admin.

The lock is enforced inside `reveal::active`, which is why every
reveal-aware surface inherits it without knowing it exists. All three
doors are shut, and the distinction matters:

| Door | Locked behaviour |
|---|---|
| The Settings checkbox | Replaced by a line naming `instance.admin_reveal_locked` and saying only machine access lifts it. |
| `POST /dashboard/settings/reveal` called directly | `403 Forbidden`, and **no** `Set-Cookie` — handing back a preference nothing honours would be a lie told where the operator can see it. |
| A hand-written `mwe_admin_reveal=1` cookie | Ignored: `active` returns false before it looks at the jar. |

Only the third one is the lock. Hiding the checkbox is a curtain — the
route is still routed and the cookie is a string anyone can type — so
`crates/mwe-dashboard/tests/reveal_lock.rs` tries all three, and its
first test is the **baseline**: without the lock the same hand-written
cookie *does* widen the journal, so the locked assertions cannot pass for
an unrelated reason.

The lock does not touch anything else about the admin role: an admin
still administers users, tokens and configuration. It removes exactly one
capability — reading past the per-fragment ACL.

> Reveal is **not** "presentation-only": on `/facts` it is precisely what
> lets an admin act on another user's fact (the structured ACL change is
> the intended supervision path — `/facts` is ACL-projected, so without
> reveal an admin does **not** already see another user's facts there).
> It deliberately does **not** extend to the inline-comment write path,
> which stays scoped to the page's read-set — a comment is applied by REM
> as fact ops on the owner's memory with no commenter provenance (see
> [agentic-chat.md](agentic-chat.md)).

Mechanically the reveal wraps each highlighted region in a fixed,
attribute-frozen tag — `<div class="acl-revealed">` for a whole-line
region (blank-line padded so the inner markdown still renders),
`<span class="acl-revealed">` for an inline fragment. The dashboard
markdown renderer (`md_render::render_reveal*`) passes through *only*
those four exact tags (depth-balanced) and keeps dropping every other raw
HTML tag, so the reveal cannot widen the renderer's XSS surface.

## Continuous text vs. list of results — only the first lives here

There are two redaction output modes; `render_for_sender`
implements only the first:

- **Continuous text** (`wiki_read`, and `wiki_recall.snippet` for
  adjacent-block redaction) — invisible regions are replaced inline
  with the `[redacted]` placeholder so the structure of the file is
  preserved. *This is `render_for_sender`.*
- **Lists of results** (`wiki_search`, `wiki_recall`, `wiki_facts_for`,
  `wiki_list_pages`, …) — invisible entries are silently omitted and
  the response carries a `redacted_count`. That is a simple `filter()`
  over the result set and lives inside each of those tools, **not** in
  `render`.

## Callout strings

The user-visible redaction strings are baked in `render.rs` as
constants, in English (the product-surface language). The inline
placeholder:

```
[redacted]
```

and the whole-page callout:

```
> [!redacted] This entire page is private.
```

The dashboard does not yet ship i18n; if/when it does, these strings
would move into a localized table. For now they are constants.

## Tests

- **`can_read`** — unit tests covering owner-only, group membership,
  `allow` extension, the three `sender_of_region` cases (user / group /
  global), the `owner = None` deny rule, and the admin-no-bypass
  invariant; plus a proptest suite asserting global-admits-anyone,
  owner-self-reads, `allow` monotonicity, and the capturer-always-
  rereads property.
- **`render_for_sender`** — the four-viewer scenario (alice owner, bob
  in team, carol outsider, dave reading a `global` region) pinned
  both as assertions and as `insta::assert_snapshot!` snapshots under
  `crates/mwe-core/src/snapshots/`; the
  `inline_granularity_preserves_surrounding_words` regression test for
  the prose-passes/region-redacted case; and
  dedicated tests for the family-microphone group sender, the embed
  cases, empty input, and the total-redaction collapse.
- **DB-first resolution** — `render.rs` pins that the DB record wins
  over the inline attributes in both directions (tightening and
  loosening), that a bare `{{f=uuid}}` marker is gated by the DB and
  never rescued by the wiki `scope`, that the cross-user sender shortcut
  works from the DB record, and that a map miss falls back to the
  inline attributes; `fact_index.rs` pins both loaders (`page_acl_map`
  keeps retired rows for export, `page_acl_map_active` drops them so a
  retired on-disk region redacts fail-closed); `recall_nav.rs` pins the funnel-level
  wiring end-to-end
  (`navigate_gates_regions_by_db_acl_over_inline_attributes`);
  `page_view_inline_comments.rs` pins the dashboard split
  (`page_view_redacts_a_retired_region_but_reveal_still_shows_it` — the
  normal view redacts a superseded on-disk region even for its own
  subject, the reveal still shows it via the full map).
- **Retirement disk half** — `reindex.rs` pins the strip primitive
  (retired region excised + neighbours re-synced, an ACTIVE fact refused,
  idempotent re-strip), the page-level strip (stale offsets survived by
  re-parsing, offsets settled), and the hygiene sweep (non-plan pages
  cleaned, plan pages skipped, convergence); `capture.rs`, `dedup.rs`,
  `dream_light.rs` and the dashboard `structured_fact_actions.rs`
  (`delete_action_strips_the_regions_bytes_from_disk`) pin the act-time
  wiring per retire path, including that a strip problem never fails the
  retiring caller.
- **Admin reveal** — `render.rs` pins `render_admin_reveal` (shows every
  region, highlights only the unreadable ones, never collapses, inline
  vs block wrapper); `md_render.rs` pins that the reveal renderer passes
  through only the four wrapper tags while still dropping all other HTML;
  `page_view_inline_comments.rs` pins the dashboard end-to-end (admin
  without the cookie sees `[redacted]`, admin with it sees the
  highlighted body + banner, and the Settings toggle sets/clears the
  cookie); `structured_fact_actions.rs` pins the `/facts` lens
  (`facts_list_hides_other_users_facts_until_admin_reveal` — another
  user's fact is absent from the list until the reveal cookie is set);
  `agentic.rs` pins the proposal read tools
  (`proposal_list_hides_other_users_until_reveal` — an admin sees another
  user's pending, and its `context`, only with reveal);
  `proposals_in_flight_count.rs` pins the badge count
  (`admin_count_is_scoped_to_self_without_reveal_full_with_reveal`); and
  `recall_traces.rs` pins the journal
  (`journal_hides_another_users_trace_until_admin_reveal` — the admin's
  own trace is listed, another user's is absent from the list and `404`
  on both the viewer and the `/data` feed, and reveal turns all three
  around).
- **The server lock** — `reveal_lock.rs` pins
  `instance.admin_reveal_locked` from all three directions plus its
  baseline: the forged cookie works when unlocked
  (`a_hand_written_reveal_cookie_works_when_the_server_does_not_lock_it`),
  and when locked the route `403`s without a `Set-Cookie`, the forged
  cookie is inert, and only then is the Settings notice checked
  (`a_locked_reveal_cannot_be_switched_on_by_form_route_or_cookie`); a
  third test keeps the default install unchanged
  (`an_unlocked_deployment_still_offers_the_toggle`).
- **Segmented variants** — `render.rs` pins the segment contract
  (readable region carries its map-covered fact id, redacted placeholder
  and connective prose are fact-less, a map-uncovered region shows
  without an id, the total-redaction collapse is one fact-less callout,
  and the joined segments equal the plain render byte-for-byte in both
  modes — `segments_*` / `reveal_segments_*`); the dashboard route test
  `page_view_link_fabric.rs` pins the rendered anchor end-to-end
  (`page_view_readable_region_carries_fact_anchor_and_redacted_one_does_not`:
  readable region → `sup.fact-ref` anchor to its record, redacted region
  → none and no id leak, reveal → anchors on revealed regions too).

After intentional changes to redaction text or whitespace handling,
refresh snapshots with:

```bash
cargo insta review --workspace
```
