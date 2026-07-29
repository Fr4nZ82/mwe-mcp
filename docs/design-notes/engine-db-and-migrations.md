---
title: engine.db and the migrations layer — design notes
area: design-notes
status: implemented
last_review: "2026-07-20"
---

# `engine.db` and the migrations layer

This page is the **single source of truth** for the `engine.db` schema:
the runtime layer that opens the file (how `mwe-core::db` applies the
canonical pragmas and runs the embedded migrations), the **canonical
DDL** of every table, and the annotated migration ledger. The
migration files under [`migrations/`](../../migrations/) are
authoritative — when this page and a migration disagree, the migration
wins; this page is then corrected.

## What `engine.db` is

A single `SQLite` file at `<workdir>/engine.db`. `mwe-mcp` is its
**only writer** (single-writer constraint enforced by
[`single-writer-lockfile.md`](single-writer-lockfile.md); concurrent
readers — REM, the dashboard, a polling consumer — share the file via
WAL). This DB is the **authoritative fact store**: for a standard wiki,
`fact_index` owns the facts and the published pages are its prose render,
so the DB is **not rebuildable** from the markdown surface — the reindex
sweep repairs offsets and honours deletions, it never creates rows from
disk ([`reindex-pipeline.md`](reindex-pipeline.md)).

The recovery path is therefore a **backup**, not a re-index: treat
`engine.db` like the files (the dashboard's Backup console and the
snapshot tooling, `mwe_core::backup`, exist for exactly that). What *is*
regenerable from disk: smart-wiki content rows (re-chunked from page
content) and the `capture_buffer` (replayed from the per-wiki
`_captures.md` journals — [`narrative-buffer.md`](narrative-buffer.md)).
(One DB sits outside this story: `media_catalog.db` is a *separate*,
externally-populated catalog — not covered here.)

## Pragmas — applied per connection

[`db::open_or_init`](../../crates/mwe-core/src/db.rs) configures every
`SqlitePool` connection with three pragmas before any user query runs:

| Pragma | Value | Why |
|---|---|---|
| `journal_mode` | `WAL` | SQLite's WAL mode lets the single writer commit without blocking concurrent readers. Without it, readers serialize behind any writer and the audit/dashboard paths stall under load. |
| `foreign_keys` | `ON` | SQLite ships with FKs *off* for historical compatibility. We always want them enforced — the `ON DELETE CASCADE` rules on the identity tables (a user delete dropping their credential row) need the runtime to honor them. |
| `busy_timeout` | `5000` ms | The lockfile enforces single-*process*, but each process runs a multi-connection `SqlitePool` (default cap 10), so its own async tasks — ingest, REM, an `events_poll` stamping `last_seen_at`, an `events_ack` — are concurrent writers. A 5s wait turns the transient `SQLITE_BUSY` of two connections contending for the WAL write lock from a user-visible error into an imperceptible pause. **Caveat:** it does *not* cover `SQLITE_BUSY_SNAPSHOT` — see the write-first rule below. |

WAL mode also writes a sidecar `engine.db-wal` and `engine.db-shm`
into the workdir. Both are part of the durable state — a backup tool
that snapshots only `engine.db` will surface inconsistent reads on
restore. The canonical backup procedure uses `VACUUM INTO`, which
produces a single self-consistent file.

### Multi-statement writes must write first

`busy_timeout` retries the *ordinary* `SQLITE_BUSY` (two connections
contending for the WAL write lock), but it cannot retry
`SQLITE_BUSY_SNAPSHOT`. That variant arises when a `BEGIN DEFERRED`
transaction — what `pool.begin()` issues — runs a `SELECT` *before* its
first write: the `SELECT` pins a read snapshot, and if any other
connection commits before the transaction upgrades to the write lock,
SQLite refuses the upgrade outright. It surfaces as `(code: 5) database
is locked` and fails *instantly* rather than waiting its turn. A
multi-connection pool with a consumer polling every 30s hits this
reliably — it silently broke `events_ack` end-to-end in v1.5.0 (acks
never landed; the event queue could not drain).

The discipline: **a write transaction must take the write lock before it
reads.** Either make its first statement a write (e.g. patch a row in
place with `json_set` and derive existence from `rows_affected` instead
of a prior `SELECT`), or open it with `BEGIN IMMEDIATE`. Read-then-write
inside one `pool.begin()` is the footgun; `events::ack_events` is the
worked example of the write-first form.

## Canonical schema

The DDL below is absorbed from the migration files (the authoritative
SQL) and grouped by concern. Column comments capture the load-bearing
invariants; the migration ledger further down explains *when* each
table or column landed.

### `fact_index` — region-level index (0001)

One row per region delimited by `{{f=<UUIDv7>}}…{{/}}` markers (the
marker grammar lives in [`marker-grammar.md`](marker-grammar.md)). The
body is stored verbatim for re-embed + audit; `embedding` holds the
vector for similarity search. Fully rebuildable from the markdown
SSOT.

```sql
CREATE TABLE fact_index (
    fact_id          TEXT PRIMARY KEY,   -- UUIDv7, lowercase with dashes
    wiki_id          TEXT NOT NULL,
    source_path      TEXT NOT NULL,      -- relative to workdir
    region_start     INTEGER,            -- byte offset of {{f=...}}
    region_end       INTEGER,            -- byte offset of {{/}}
    "text"           TEXT NOT NULL,      -- region body (no markers)
    embedding        BLOB NOT NULL,      -- vector bytes
    embedding_dim    INTEGER NOT NULL,   -- explicit, for embedding-model migrations
    owner_id         TEXT NOT NULL,      -- "global" | "user:X" | "group:X"
    allow_ids        TEXT,               -- JSON array of principals
    sender_id        TEXT,               -- provenance: who captured it. Always materialized at
                                         -- birth since 0051; NULL survives only as the "scrubbed"
                                         -- fallback (deleted principal) and on smart-wiki
                                         -- section rows (wiki-level ACL, no per-fragment capturer)
    fact_type        TEXT,               -- see "fact_type convention" below
    topics           TEXT,               -- JSON array of strings
    created_at       TEXT NOT NULL,      -- ISO 8601
    updated_at       TEXT NOT NULL,
    superseded_at    TEXT,               -- NULL if active
    superseded_by    TEXT,               -- fact_id of replacement
    deleted_at       TEXT,               -- NULL if active
    deleted_reason   TEXT,               -- "filesystem_removed" | "user_request" | "gdpr_erasure" | ...
    last_recall_at   TEXT,
    recall_count_30d INTEGER NOT NULL DEFAULT 0,
    -- added by 0033 (per-fact validity model):
    valid_from       TEXT,               -- ISO 8601; NULL = unknown / "since forever"
    valid_to         TEXT,               -- ISO 8601; NULL = open ("true now"); a set value = closes at that horizon
    decay_reason     TEXT,               -- why valid_to closed; NULL while alive; closed vocab (see below)
    -- added by 0035 (the ingest placement axis, a hint):
    target_page      TEXT,               -- page the classifier proposed; NULL = unproposed
    style            TEXT,               -- proposed page style (prosa|prosa-tecnica|lista); NULL = unproposed
    page_description TEXT,               -- proposed "cosa ci va dentro" one-liner; NULL = unproposed
    -- per-fact salience for the index base context (migration 0037):
    salience         TEXT,                -- "high" | "normal" | "low"; NULL = unspecified; high → index.md
    -- document provenance (migration 0040):
    source_ref       TEXT,                -- catalog id / url the fact was extracted from; NULL for conversational captures
    -- group-17 provenance breadcrumbs (migration 0042):
    authored_refs    TEXT NOT NULL DEFAULT '[]', -- JSON array of [[wiki_id/page]] wikilinks; consolidation links instead of duplicating
    -- succession pointer on a LIVE closed row (migration 0056):
    successor_fact_id TEXT               -- fact_id that replaced this one; stamped by close_validity when the closer knows it
);
```

Two invariants ride on this table:

- **`fact_id` is a UUIDv7.** Time-ordered, so a `created_at` sort and a
  primary-key sort agree, and inserts stay append-friendly on the
  B-tree.
- **Default-active query filter.** A `superseded_at` + `superseded_by`
  pair forms the supersedence chain; `deleted_at` is the soft-delete
  tombstone. Every "give me the live facts" query filters
  **`WHERE deleted_at IS NULL AND superseded_at IS NULL`**. Audit /
  historical queries opt back into the superseded and deleted rows. The
  partial index `idx_fact_active ON (deleted_at, superseded_at)` backs
  the hot path.

**`fact_type` convention.** The column is a free `TEXT` (no DB-level
`CHECK`, no Rust enum constraining it), but the ingest pipeline emits
from a fixed 7-value vocabulary plus `null` — the SSOT for the set is
the ingest prompt's output schema
([`crates/mwe-core/prompts/ingest.md`](../../crates/mwe-core/prompts/ingest.md)):
`"bio" | "state" | "preference" | "rule" | "plan" | "episode" |
"other"`. New callers should stay inside that set so
`fact_type`-faceted recall keeps a stable surface.

**Per-fact validity (`valid_from` / `valid_to` / `decay_reason`, migration
0033).** These three columns carry the per-fact temporal-validity model — the
design SSOT is [`memory-model.md`](../concepts/memory-model.md).
Each fact owns an interval `[valid_from, valid_to)`: `valid_to IS NULL` means
**open** ("true now, no horizon"), a set `valid_to` means the fact holds only
through that horizon. `decay_reason` records *why* the interval was closed —
`NULL` while the fact is alive, stamped on closure from the
[`fact_index::decay`](../../crates/mwe-core/src/fact_index.rs) vocabulary:
`'completed'` (a consumable intention was spent), `'retracted'` (a relayed
forget/abandon gesture), `'contradicted'` (stamped by `mark_superseded` on
the predecessor). **Expiry stamps nothing** — a past `valid_to` *is* the
expiry. Same convention as `deleted_reason` / `fact_type`: **free `TEXT`, no
DB `CHECK`** — the vocabulary is enforced at the Rust producer, not the
schema (a future closure kind, e.g. condensation, needs no migration). The
interval is written at capture (the classifier resolves it), closed by
`close_validity` / `mark_superseded`, and rendered into prose by the
compiler; at recall it is a **soft-down-rank signal, never a hard filter**
(the stale/superseded fact is often the gold), and there is no TTL sweep
that deletes.

**Succession pointer (`successor_fact_id`, migration 0056).** The validity
axis's forward rail: `close_validity` stamps the fact that replaced this one
whenever the closer knows it (the REM contradiction sweep passes the seed's
superseding fact to its satellites, the completion sweep its evidence fact;
`None` never wipes an earlier pointer, and the `validity_close` receipt
snapshots/restores it on revert). Distinct from `superseded_by`, which is
welded to the `superseded_at` tombstone: a superseded row leaves the page,
while a closed row keeps rendering — the compiler projects the pointer as
the `(current: [[…]])` feed hint so the prose can point one hop from the
obituary to today's truth
([narrative-compiler.md](narrative-compiler.md#the-succession-pointer--one-hop-from-the-obituary-to-todays-truth)).

**Ingest placement axis (`target_page` / `style` / `page_description`, migration
0035).** Sibling of the validity axis, design SSOT
[`narrative-compiler.md`](narrative-compiler.md). The ingest
classifier (the "Cartografo at runtime") already decides, per claim, *where* it
goes plus the target page's style and a "cosa ci va dentro" one-liner. These
three columns carry that proposal onto the fact so the **light** dream can
settle a fact on its ingest page without re-running the strong-model Cartografo
(which becomes REM-only). They are a **hint, not the home**: the
compilation plan stays authoritative on placement and the REM Cartografo may
re-home a fact. The standard-wiki path stages `style`/`page_description` on the
buffer (`target_page` rode it already) and `promote_one` copies the whole axis
across; the direct path carries it from the request. **Additive and inert** as
of 0035 — the consumer (`build_wiki_plan` in the light cadence) lands later.

**Per-fact salience (`salience`, migration 0037).** Design SSOT
[`ingest-pipeline.md`](ingest-pipeline.md) (the "Per-fact salience" section). One more
producer-decided axis: how always-relevant a fact is to its owner — `'high'`
(the scarce always-on set: identity, health/safety, hard standing constraints),
`'normal'` (default), `'low'` (trivia); `NULL` = unspecified. Same convention as
`fact_type` / `decay_reason`: **free `TEXT`, no DB `CHECK`**, the closed
vocabulary enforced at the ingest producer (no hardcoded gate — the classifier
decides). It is **independent** from `fact_type` / validity / `style`. The ingest
classifier emits it; it is threaded through `CaptureRequest` /
`BufferedCapture` into `fact_index` on both paths (the `_captures.md` journal
mirrors it as the `sal=` attribute, like `vf`/`vt`). **Read by the light compile
cadence**: `ingest_placement_blueprint` routes a `'high'` fact to the
actor-wiki's `index.md` base context, overriding its proposed `target_page` (see
[narrative-compiler.md](narrative-compiler.md)).

Indices: `idx_fact_wiki_id`, `idx_fact_owner`, `idx_fact_created`,
`idx_fact_type` (partial, `WHERE fact_type IS NOT NULL`),
`idx_fact_active`, `idx_fact_recall` (partial), `idx_fact_path`,
`idx_fact_valid_to` (partial, `WHERE valid_to IS NOT NULL` — backs the REM
expiry scan and the recall actuality signal).

### `wiki_events` — event queue (0002)

Emitted by REM and the lifecycle policies, polled by consumers via
`events_poll`, acked via `events_ack`.

```sql
CREATE TABLE wiki_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT NOT NULL,             -- "dedup_proposed" | "structure_applied" | "archive_proposed" | "auto_applied" | … (extensible)
    wiki_id    TEXT,                      -- NULL for events not tied to a wiki
    fact_id    TEXT,
    payload    TEXT,                      -- JSON, shape per-kind
    created_at TEXT NOT NULL,
    acks       TEXT NOT NULL DEFAULT '{}' -- JSON map: { consumer_id: ack_ts }
);
```

`kind` is an open string set; the REM emitters define the
payload shape per kind (e.g. `structure_applied → { proposal_id, variant,
recipient_id, revert_deadline, dashboard_path, … }`,
`auto_applied → { proposal_id, summary, confirm_deadline, dashboard_path }`). The
**`acks` map** (`{ consumer_id: ack_ts }`) is what makes multi-consumer
ack tracking possible: an event acked by *every* registered consumer is
eligible for the optional retention sweep (default **30 days**), while
an event nobody acked stays indefinitely.

`events_ack` stamps `acks[consumer_id]` with a single in-place `json_set`
`UPDATE` per event id — never a read-then-write — so the ack transaction
takes the WAL write lock up front (see
[Multi-statement writes must write first](#multi-statement-writes-must-write-first)).
A missing row updates nothing and is reported back as `unknown`; an
existing row always counts one changed row, so a re-ack stays idempotent.

### `tool_executions` — audit trail (0003)

Every MCP-exposed tool invocation is logged here, queryable via the
`tool_log_search` MCP tool. It is never auto-injected into a consumer
agent's context.

```sql
CREATE TABLE tool_executions (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp      TEXT NOT NULL,
    tool_name      TEXT NOT NULL,
    sender_id      TEXT NOT NULL,
    device_label   TEXT NOT NULL,  -- from the MCP token
    rate_limit_id  TEXT,           -- JWT claim; parsed-but-not-enforced
    args_hash      TEXT,           -- SHA-256 of the args, never raw PII
    result_summary TEXT,
    latency_ms     INTEGER,
    cost_estimate  REAL,           -- EUR; parsed-but-inert (no budget enforcement yet)
    error          TEXT            -- error class, NULL on success
);
```

Two honest partials live in this row shape: **`args_hash` is the
SHA-256 of the canonical arguments** — the raw arguments (which can
carry PII) are never persisted, only their digest, so the audit trail
is forensically useful without becoming a data-leak surface.
**`rate_limit_id`** is the corresponding JWT claim and **`cost_estimate`**
is the per-call EUR estimate; both are *recorded* but inert — no rate
limiting and no budget cap currently keys off them (see
[`jwt-and-session-model.md`](jwt-and-session-model.md) for the claim
side).

### `archive_proposals` — REM archival candidates (0004)

Generated by the REM cycle; each row is a candidate node to move into
`_archive/`, gated on human approval.

```sql
CREATE TABLE archive_proposals (
    proposal_id TEXT PRIMARY KEY,                  -- UUID or "ap-YYYY-MM-DD-NNN"
    wiki_id     TEXT NOT NULL,
    path        TEXT NOT NULL,                     -- filesystem path of candidate
    reason      TEXT NOT NULL,                     -- "no_recall_hit_365d" | "no_modify_180d" | …
    proposed_at TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',   -- pending | approved | rejected | expired
    decided_at  TEXT,
    decided_by  TEXT,                              -- sender_id of approver
    selection   TEXT                               -- JSON: partial-approve subset
);
```

### `structure_proposals` — structural-change questionnaires (0005, 0019, 0032)

Drives the structural-change approval flow: stage-3 promotion, dedup
merges, packaged bundles. Each row carries the
questionnaire, the answers, the consolidated spec, and a revert window.
Migration 0019 extended it from a 4-state to a **5-state lifecycle**:
`pending → applied_pending_confirm → applied | reverted | expired`.

```sql
CREATE TABLE structure_proposals (
    proposal_id     TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,                  -- "wiki_promote" | "dedup_merge" | "bundle" (legacy rows may carry the dropped "wiki_type_forge")
    context         TEXT NOT NULL,                  -- JSON: { intent, sample_block, source_wiki_id, … }
    questions       TEXT NOT NULL,                  -- JSON: [{ id, text, options:[…] }]
    proposed_at     TEXT NOT NULL,
    timeout_at      TEXT NOT NULL,                  -- proposed_at + 24h (configurable)
    status          TEXT NOT NULL DEFAULT 'pending',
    applied_at      TEXT,
    applied_by      TEXT,                           -- sender_id (NULL on auto-apply)
    answers         TEXT,                           -- JSON: { question_id: chosen_option_id }
    spec            TEXT,                           -- JSON: consolidated spec post-integration
    revert_token    TEXT,                           -- UUID, valid for 7d after applied_at
    revert_deadline TEXT,                           -- applied_at + 7d
    reverted_at     TEXT,
    -- added by 0019 (5-state lifecycle):
    apply_mode          TEXT,  -- 'manual' | 'auto' | NULL (legacy)
    confirm_deadline    TEXT,  -- set iff status = 'applied_pending_confirm'
    confirmed_at        TEXT,  -- applied_pending_confirm → applied transition
    confirmed_by        TEXT,  -- sender_id of the confirmer post auto-apply
    revert_triggered_by TEXT,  -- 'user' | 'sweep' | NULL (legacy)
    -- added by 0032 (per-recipient proposals):
    recipient_id        TEXT   -- addressee: Principal "user:<id>"|"group:<id>"|"global"; NULL = unaddressed / admin-fallback
);
```

The **`recipient_id` column** (migration 0032) is the addressee of a
proposal — the human the consumer agent should notify and who (with an
admin) may apply / confirm / revert it. REM derives it from the
triggering fact (`proposals::recipient_from_fact`: the fact's
`sender_id`, else the owning user, else `NULL` for a group/global owner
with no sender). `NULL` is the unaddressed / admin-fallback bucket and
the value every pre-0032 row reads as. The dashboard tray, its agentic
`structure_proposal_list` tool, and the `pending_attention` count scope
to "addressed to me **or** unaddressed" for a non-admin (admins see
all); `index idx_struct_recipient(recipient_id, status)` backs that
query.

A manual apply from the dashboard goes straight to `applied`
(`apply_mode='manual'`). The auto-apply sweep on `timeout_at` instead
lands `applied_pending_confirm` (`apply_mode='auto'`) and gives the
user until `confirm_deadline` (default `applied_at + 7d`) to confirm;
silence past that window flips to `reverted`
(`revert_triggered_by='sweep'`). The partial index
`idx_struct_confirm_deadline … WHERE status = 'applied_pending_confirm'`
backs the auto-revert sweep.

### `structure_proposal_votes` — member votes on governed proposals (0053)

One row per `(proposal, voter)`: the cast votes on a proposal put to an
audience — a governed group-wiki page deletion (a born-applied `bundle`
receipt whose eligible voters are the owning group's roster minus the
deleter) and the propose-first `fact_forget` request (a non-sender
owner asking to forget a fact, put to the fact's audience). The tally
lives in [`mwe-core::votes`](../../crates/mwe-core/src/votes.rs):
more than half voting NO blocks/reverts, silence is consent, and an
all-voted quorum resolves early — explicit YES votes are recorded too
so a voted-yes member is distinguishable from a silent one.

```sql
CREATE TABLE structure_proposal_votes (
    proposal_id TEXT NOT NULL REFERENCES structure_proposals(proposal_id) ON DELETE CASCADE,
    voter_id    TEXT NOT NULL REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
    vote        TEXT NOT NULL,                          -- 'yes' | 'no'
    voted_at    TEXT NOT NULL,
    PRIMARY KEY (proposal_id, voter_id)                 -- a re-vote is a PK conflict: votes are final
);
```

`idx_proposal_votes_voter ON (voter_id)` backs the per-member
"do I still owe a vote?" scan behind the in-recall pending-vote
reminder. Both FKs cascade: dropping the proposal row or removing the
voter from enrollment clears their dangling votes.

### `enrollment_users` / `enrollment_groups` — identity SoT (0006, 0015, 0020, 0045–0047, 0049, 0050)

The identity tables. **These tables are the source of truth** (CRUD
through the dashboard).

```sql
CREATE TABLE enrollment_users (
    user_id  TEXT PRIMARY KEY,
    aliases  TEXT,                            -- JSON array of strings
    is_admin INTEGER NOT NULL DEFAULT 0,      -- bool 0/1
    -- added by 0020:
    locale   TEXT,                            -- optional BCP-47 tag for prompt LANGUAGE injection
    -- added by 0045 (moved here from user_credentials, where 0017 first put it):
    email    TEXT,                            -- the login identifier; partial-UNIQUE WHERE email IS NOT NULL
    -- added by 0049 (per-user 2FA enforcement; the deployment-wide
    -- "require 2FA for all" toggle lives in engine_meta, not here):
    require_2fa INTEGER NOT NULL DEFAULT 0,
    -- added by 0050 (explicit agent marker — a consumer agent's OWN identity,
    -- e.g. the system user a standard token binds; mutually exclusive with a
    -- user_credentials login, enforced in both directions):
    is_agent INTEGER NOT NULL DEFAULT 0,
    -- added by 0061 (per-user IANA zone for ingest reference-time stamping;
    -- wins over the deployment-wide `recall.ingest_timezone`; set from the
    -- users page or the welcome wizard):
    timezone TEXT
);
-- 0046 dropped the cosmetic free-prose blurbs: enrollment_users.profile
-- and enrollment_groups.description. The wiki title is now the id; a
-- group's routing prose is `scope`.

CREATE TABLE enrollment_groups (
    group_id    TEXT PRIMARY KEY,
    members     TEXT NOT NULL,                -- JSON array of user_id
    scope       TEXT                          -- free prose (ingest-classifier routing domain)
);
```

Migration 0015 adds the partial **unique** index
`idx_single_admin ON enrollment_users(is_admin) WHERE is_admin = 1`,
making "exactly one admin per deployment" a hard DB constraint — a
second `is_admin = 1` row fails at the SQL layer, even under manual
tampering. (`profile_initialized` lives on `user_credentials`, not
here — see below.)

Migration 0047 seeds the builtin **`global` group** as a normal
`enrollment_groups` row (`INSERT OR IGNORE`, so an admin-edited `scope`
is never clobbered; `enrollment::ensure_global_group` re-applies the
seed at runtime). The row exists purely so the admin can edit the
prose the ingest classifier consults to recognise a world fact — its
`members` column is unused, membership is universal and enforced in
code (`acl::principal_matches`), never stored.

### `wiki_types_registry` — dropped (0007 → 0036)

**Dropped by `0036_drop_wiki_types_registry`.** It was a zero-data cache
of every registered template, rebuilt from the `_styles/*.md` /
`_meta.md` files by the `_styles/` watcher so lookups didn't re-parse the
templates. **The family is an authored per-wiki smart flag in
each `_meta.md`** (read directly by the gates), and "standard" simply
means "not smart" — so no lookup against this cache is needed. See
[`smart-wikis.md`](smart-wikis.md).

### Applicative WAL — `proposal_ops_log` / `rem_ops_log` (0008, 0009)

Two step-by-step journals that make multi-step writes crash-safe; the
recovery contract lives in [`applicative-wal.md`](applicative-wal.md).
Each step writes a `pending` row, performs the file/DB mutation, then
flips to `done`; a startup sweep rolls back rows left
`pending`/`in_progress` past a staleness threshold.

```sql
CREATE TABLE proposal_ops_log (        -- structure-proposal apply
    op_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    proposal_id  TEXT NOT NULL,
    step_idx     INTEGER NOT NULL,      -- ordered within a proposal
    kind         TEXT NOT NULL,         -- "file_write" | "db_update" | "marker_propagation" | …
    payload_json TEXT,                  -- JSON args of the step (replay/debug)
    status       TEXT NOT NULL,         -- "pending" | "in_progress" | "done" | "failed"
    started_at   TEXT NOT NULL,
    completed_at TEXT,
    error_msg    TEXT
);

CREATE TABLE rem_ops_log (             -- REM cycle ops
    op_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    cycle_id       TEXT NOT NULL,       -- UUIDv7 of the REM cycle
    operation_kind TEXT NOT NULL,       -- "promotion" | "revalidate" | "dedup_merge" | "archive_proposal" | …
    target_wiki_id TEXT,                -- NULL for global ops
    snapshot_path  TEXT,                -- relative to workdir
    status         TEXT NOT NULL,       -- "pending" | "in_progress" | "done" | "failed"
    started_at     TEXT NOT NULL,
    completed_at   TEXT,
    error_msg      TEXT
);
```

### `token_blacklist` — JWT revocation (0010, 0011)

A revoked token's `jti` is inserted on `mwe-mcp token-revoke`. The
server caches the active blacklist in memory (60s TTL) so the verify
hot path doesn't hit SQLite; expired entries can be GC'd.

```sql
CREATE TABLE token_blacklist (
    jti        TEXT PRIMARY KEY,   -- JWT id (UUID minted at issuance)
    revoked_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,      -- original token exp; used for GC
    reason     TEXT,               -- free text or known code
    revoked_by TEXT                -- added by 0011: actor that revoked
);
```

### `consumers` — consumer-agent registry (0016, 0029)

Marks a consumer (an orchestrator, a discord bot, a vscode plugin) as
known to this deployment. `events_poll` / `events_ack` check the row
exists first; a missing row yields `consumer_not_registered`.

```sql
CREATE TABLE consumers (
    consumer_id      TEXT PRIMARY KEY,  -- chosen at registration, stable; mirrors the JWT consumer_id claim
    display_name     TEXT,
    callback_url     TEXT,              -- absent ⇒ pull-only via events_poll
    kinds_subscribed TEXT,              -- JSON array; empty/NULL ⇒ all kinds
    metadata         TEXT,              -- JSON object, free shape
    consumer_secret  TEXT NOT NULL,     -- hex; returned once, idempotent on re-register
    registered_at    TEXT NOT NULL,
    last_seen_at     TEXT,              -- last poll/ack timestamp
    -- added by 0029 (the consumer ↔ system-user binding):
    system_user_id   TEXT REFERENCES enrollment_users(user_id)
);
```

**`system_user_id`** (migration 0029) materialises the
consumer ↔ system-user binding of the diagonal identity model
([identity-and-acl.md](../concepts/identity-and-acl.md)): a *standard*
consumer is itself a credential-less system user with its own memory
wiki, and this column records which one. Populated by
`consumers::register` from the caller's `sender_id` when its token is
`consumer_class = standard`; `NULL` for consumers registered without
the binding (a re-registration backfills it).

### `user_credentials` / `user_invitations` — dashboard auth (0012, 0017, 0018, 0013)

```sql
CREATE TABLE user_credentials (
    user_id        TEXT PRIMARY KEY
                   REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
    password_hash  TEXT NOT NULL,            -- Argon2id PHC string (carries its own salt + params)
    hashed_at      TEXT NOT NULL,
    must_change    INTEGER NOT NULL DEFAULT 0,
    -- added by 0018 (0017's `email` column was moved to enrollment_users by 0045):
    profile_initialized INTEGER NOT NULL DEFAULT 0  -- gates the first-login welcome wizard
);

CREATE TABLE user_invitations (
    invitation_id  TEXT PRIMARY KEY,
    user_id        TEXT NOT NULL
                   REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
    created_at     TEXT NOT NULL,
    expires_at     TEXT NOT NULL,            -- default TTL 24h (configurable)
    consumed_at    TEXT,                     -- NULL ⇒ invite still usable
    invited_by     TEXT NOT NULL
);
```

Only users who sign in through the browser have a credential row;
"system" identities that only anchor a consumer token need a row in
`enrollment_users` and nothing here. The `ON DELETE CASCADE` means a
user delete drops the credential and any open invites in one shot.
Login is by **email** (0017 split it from the `user_id` slug, which
stays the marker principal + on-disk wiki id), and the email now lives
on **`enrollment_users`** (0045 moved it there from `user_credentials`).
It sits on the enrollment row because the admin sets it when *inviting* a
user — before any `user_credentials` row exists — and is the only one who
can change it. The column is partial-UNIQUE (one address per account);
login is email-only with **no username fallback**, so a row with a NULL
email cannot sign in until the admin sets one.
**`profile_initialized` stays here, on `user_credentials`** (migration
0018), gating the one-shot welcome wizard.

### `password_resets` — self-service recovery links (0048)

The forgot-password flow (design SSOT
[jwt-and-session-model.md](jwt-and-session-model.md)): a row is minted
per recovery request and the URL
`…/dashboard/reset-password/<reset_id>` is emailed to the address on
`enrollment_users.email` — the admin is never involved. Distinct from
`user_invitations` by TTL (~30 min vs 24 h) and audit meaning
(recovery, not onboarding).

```sql
CREATE TABLE password_resets (
  reset_id     TEXT PRIMARY KEY,   -- random UUIDv7; the only secret in the URL
  user_id      TEXT NOT NULL REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  created_at   TEXT NOT NULL,
  expires_at   TEXT NOT NULL,
  consumed_at  TEXT                -- NULL ⇒ still usable; flipped in the same
                                   -- transaction as the new hash (burn-once)
);
```

The partial index `idx_password_resets_expires … WHERE consumed_at IS
NULL` backs the expiry sweep over still-open links.

### `user_2fa` family — dashboard TOTP (0049)

TOTP two-factor auth for the human `/dashboard/login` surface only —
the MCP path is bearer-JWT with no interactive login, and system/bot
users have no `user_credentials` row, so they are exempt by
construction. Design SSOT
[jwt-and-session-model.md](jwt-and-session-model.md).

```sql
CREATE TABLE user_2fa (
  user_id      TEXT PRIMARY KEY REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  secret_enc   TEXT NOT NULL,               -- TOTP secret encrypted at rest (XChaCha20-Poly1305,
                                            -- key derived from MWE_TOKEN_SECRET — rotating the
                                            -- token secret invalidates every enrollment)
  enabled      INTEGER NOT NULL DEFAULT 0,  -- 0 = started but unconfirmed; 1 = active
  created_at   TEXT NOT NULL,
  confirmed_at TEXT
);

CREATE TABLE user_2fa_recovery_codes (      -- single-use codes, SHA-256-hashed
  user_id    TEXT NOT NULL REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  code_hash  TEXT NOT NULL,
  used_at    TEXT,                          -- set when spent
  PRIMARY KEY (user_id, code_hash)
);

CREATE TABLE pending_2fa (                  -- challenge between password and session mint
  challenge_id TEXT PRIMARY KEY,            -- opaque random id in a short-lived cookie —
                                            -- deliberately NOT a JWT, so it can never be
                                            -- mistaken for a session cookie
  user_id      TEXT NOT NULL REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  is_admin     INTEGER NOT NULL DEFAULT 0,
  next         TEXT,
  created_at   TEXT NOT NULL,
  expires_at   TEXT NOT NULL
);
```

Per-user enforcement is `enrollment_users.require_2fa` (same
migration); the deployment-wide "require 2FA for all non-system users"
toggle lives in `engine_meta` (key `auth.require_2fa_all`).
`idx_pending_2fa_expires` backs the challenge-expiry sweep.

### `consumer_delegations` — act-as authorization (0014)

Lets a consumer impersonate listed user ids via the
`X-MWE-Act-As: <user_id>` header. The delegation deliberately does
**not** ride inside the bot's JWT (which carries only `consumer_id`);
this table is re-read per tool call with a short TTL cache, so edits
propagate without re-issuing tokens.

```sql
CREATE TABLE consumer_delegations (
    consumer_id        TEXT PRIMARY KEY,
    allowed_sender_ids TEXT NOT NULL,   -- JSON array; referential integrity enforced applicatively
    granted_at         TEXT NOT NULL,
    granted_by         TEXT NOT NULL
);
```

### Smart-wiki tables — op-log / briefing / leases (0022–0027)

The smart-wiki surface (authoritatively-administered wikis driven
by a smart consumer — see [`smart-wikis.md`](smart-wikis.md))
adds three tables. (A fourth, `skills_custom` (0024) — the per-owner
custom skill catalog — was **dropped by 0036**; only bundled skills
remain.)

```sql
-- 0022 + 0027: append-only audit/op log for wiki_admin_push/pull/notify
CREATE TABLE wiki_admin_op_log (
    op_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    wiki_id        TEXT NOT NULL,
    sender_id      TEXT NOT NULL,
    consumer_id    TEXT,                -- nullable: early tokens may omit
    op_kind        TEXT NOT NULL,       -- 'push_create' | 'push_upsert' | 'push_snapshot_replace' | 'pull' | 'notify'
    op_mode        TEXT,                -- mirror of `mode` for pushes
    payload_hash   TEXT NOT NULL,       -- sha256 of canonical input (paths + sorted page-checksum manifest), never raw content
    pages_affected INTEGER NOT NULL DEFAULT 0,   -- count, not the list
    ts             TEXT NOT NULL,       -- ISO 8601 UTC
    -- added by 0027:
    actor_kind     TEXT NOT NULL DEFAULT 'smart_consumer'
                   CHECK (actor_kind IN ('smart_consumer','dashboard','system')),
    pre_image_json TEXT                 -- JSON page snapshot before the write, for revert; NULL ⇒ no revert
);

-- 0023 + 0025 + 0027: DB mirror of the `## From …` items appended to _briefing.md
CREATE TABLE wiki_briefing_items (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    wiki_id          TEXT NOT NULL,
    source_kind      TEXT NOT NULL,     -- 'user' | 'rem' | 'consumer' | 'dashboard' | 'dashboard_comment'
    source_ref       TEXT NOT NULL,
    topic            TEXT NOT NULL,     -- ≤ 200 chars
    body             TEXT NOT NULL,     -- markdown, ≤ 4 KB
    kind             TEXT,              -- observation | reasoning | external (NULL = observation)
    ts               TEXT NOT NULL,
    processed_at     TEXT,              -- set when smart_bootstrap drains the item; NULL = pending
    -- added by 0025:
    target_cite      TEXT,              -- opt-in handle: wiki://<wiki_id>/<page_path>(#<heading-slug>)?
    -- added by 0027:
    author_sender_id TEXT               -- human author behind the item; NULL for source_kind='rem'
);

-- 0026: optional cooperative lease coordinating wiki_admin_push across a user's smart consumers
CREATE TABLE wiki_admin_leases (
    lease_id    TEXT PRIMARY KEY,
    wiki_id     TEXT NOT NULL,
    sender_id   TEXT NOT NULL,
    consumer_id TEXT,                  -- nullable: smart-tokens may omit
    acquired_at TEXT NOT NULL,         -- ISO 8601 UTC
    expires_at  TEXT NOT NULL,         -- acquired_at + ttl
    released_at TEXT                   -- NULL while active
);
```

The notable invariants here:

- **`wiki_admin_op_log.actor_kind`** discriminates the writer behind a
  row so the revert UI handles smart-consumer pushes, dashboard editor
  saves, and `system` compensation rows uniformly; its DEFAULT covers
  the early history (all `smart_consumer` writes) without a
  backfill. **`pre_image_json`** is the page snapshot sufficient for
  revert — NULL on legacy rows and on `system` compensating rows (the
  dashboard hides the Revert button when it's NULL).
- **Leases are opt-in.** Without one, `wiki_admin_push` falls back to
  optimistic concurrency on the op-log head; with one, a push from a
  different consumer fails `423 wiki_locked_by_lease` until release or
  expiry. SQLite cannot express "at most one active lease per
  `wiki_id`" as a plain partial UNIQUE (re-acquire wants to touch the
  row in place), so the lease module enforces it inside the
  transaction. REM's `lease_expirer` sub-job prunes crashed
  (`released_at IS NULL AND expires_at < now - 1h`) and aged-out
  (released > 7 days) rows, keeping the table bounded.

### `capture_buffer` — standard-wiki pre-compilation staging (0031)

The captures buffer. For a **standard** wiki,
`wiki_ingest_message` does not write the classified claim into the
published `.md`; it stages the claim here (and in the on-disk journal)
and the nightly compiler turns the buffer into prose later. This row
shape mirrors `fact_index`'s classifier/ACL columns so promotion is a
straight copy. **Standard-wiki perimeter only** — smart-wiki
(smart-owned) wikis never touch this table (they write through the
`wiki_admin_*` family), and an explicitly requested container (a list /
collection the user asked to keep) takes the direct-write
`capture::wiki_capture` path instead of buffering.

```sql
CREATE TABLE capture_buffer (
    capture_id        TEXT PRIMARY KEY,                 -- UUIDv7; reused verbatim as fact_id on promotion
    wiki_id           TEXT NOT NULL,
    target_page       TEXT NOT NULL,                    -- page the classifier proposed (a compiler hint)
    body              TEXT NOT NULL,                    -- captured claim prose, verbatim, no markers
    owner_id          TEXT NOT NULL,                    -- "global" | "user:X" | "group:X"
    allow_ids         TEXT NOT NULL DEFAULT '[]',       -- JSON array of principals
    sender_id         TEXT,                             -- provenance: who captured it; materialized at
                                                        -- birth since 0051 (NULL only as scrubbed fallback)
    fact_type         TEXT,                             -- same vocabulary as fact_index.fact_type
    topics            TEXT NOT NULL DEFAULT '[]',       -- JSON array of strings
    supersede_hint    TEXT,                             -- fact_id the classifier flagged as superseded (optional)
    status            TEXT NOT NULL DEFAULT 'buffered', -- buffered | promoted | skipped_dup
    captured_at       TEXT NOT NULL,                    -- ISO 8601
    processed_at      TEXT,                             -- ISO 8601, set when the light dream resolves the row
    resolved_fact_id  TEXT,                             -- fact this capture became (== capture_id) or deduped into
    source_kind       TEXT NOT NULL DEFAULT 'ingest',   -- ingest | shadow_diff | dashboard
    source_ref        TEXT,
    -- added by 0034 (validity threaded through the
    -- narrative buffer→promote path; mirrored in the journal as vf/vt):
    valid_from        TEXT,                             -- ISO 8601; NULL = unknown / "since forever"
    valid_to          TEXT,                             -- ISO 8601; NULL = open ("true now, no horizon")
    -- added by 0035 (the placement style axis; staged here
    -- so promote_one copies it onto the fact; mirrored in the journal as
    -- style=/desc=, the free-text desc percent-escaped):
    style             TEXT,                             -- proposed page style (prosa|prosa-tecnica|lista); NULL = unproposed
    page_description  TEXT,                             -- proposed "cosa ci va dentro" one-liner; NULL = unproposed
    -- added by 0038 (a closure that lands while the target is still
    -- buffered; DB-only post-capture mutation, never journalled):
    decay_reason      TEXT,                             -- why the staged valid_to closed; NULL = alive
    -- added by 0042 (group-17 provenance breadcrumbs; staged here so
    -- promote_one copies them onto the fact; mirrored in the journal as the
    -- comma-joined aref= attr):
    authored_refs     TEXT NOT NULL DEFAULT '[]'        -- JSON array of [[wiki_id/page]] wikilinks
);
```

Indices: `idx_capture_buffer_wiki ON (wiki_id)` and the partial
`idx_capture_buffer_pending ON (status) WHERE status = 'buffered'`
(the light dream's backlog/drain query).

The load-bearing invariants:

- **This table is a rebuildable cache/index, not the SSOT.** The
  durable source of truth for a buffered capture is the per-wiki
  on-disk journal `<wiki_dir>/_captures.md`
  (`crate::wiki::CAPTURES_FILENAME`): a YAML frontmatter
  (`kind: capture_journal`, `wiki_id`) followed by one entry per
  capture, each fenced by `<!-- mwe-capture … -->` / `<!-- /mwe-capture -->`
  HTML comments with the verbatim body between them. Deliberately there
  is **no `journal_path` column** — a capture for wiki *W* always lives
  in *W*'s `_captures.md`, derived from the tree. `rm engine.db`
  followed by `mwe-mcp serve` regenerates every row, because
  `reindex::reindex_full` now calls
  `capture_buffer::reindex_capture_journal` per wiki (idempotent,
  `ON CONFLICT(capture_id) DO NOTHING`). The journal is excluded from
  `WikiHandle::list_pages` and the marker reindex sweep
  (`reindex::is_capture_journal` guards both `enumerate_pages` and
  `reindex_file`) so its entries are never indexed as facts. Capture
  bodies may not contain `{{`, `}}`, or `<!--` (the marker grammar and
  the journal delimiters are mwe-mcp-managed).
- **`capture_id` is a UUIDv7 minted at buffer time and reused verbatim
  as the `fact_id` on promotion**, so a claim keeps one stable id
  across buffer → fact → compiled-page (the correctness hinge for
  incremental compilation).
- **The `status` lifecycle is wired by the light dream.** Ingest lands
  rows `buffered`; the drain side runs in
  [`crate::dream_light`](../../crates/mwe-core/src/dream_light.rs).
  Per buffered capture the light dream embeds the body and inserts a
  fact whose `fact_id` **is** the `capture_id`, with
  `source_path = _captures.md` and `region_start` / `region_end` NULL
  (no published page yet — the Cronista repoints these on compile), then
  stamps the row `promoted` (with `resolved_fact_id = capture_id` and
  `processed_at`). An exact duplicate of an existing active fact is
  stamped `skipped_dup` (resolving to the survivor) and no new fact is
  created. The `idx_capture_buffer_pending` partial index backs that
  drain query. A standard-wiki capture becomes recallable once the light
  dream promotes it. Prose compilation of those facts into the published
  `.md` is the Cronista (landed — it compiles each promoted fact into a
  standard page and repoints `source_path`/offsets off `_captures.md`;
  see [`narrative-compiler.md`](narrative-compiler.md)); recall does not
  yet serve that compiled prose, only the fact body. For the full write-path design
  see [`narrative-buffer.md`](narrative-buffer.md); for the promotion
  algorithm, idempotency, and cadence see
  [`rem-cycle.md`](rem-cycle.md).

### `media_catalog` — per-media metadata + ACL (0039)

The twin of `fact_index` for media: one row per catalogued media item,
keyed by the minted `c-YYYY-MM-DD-<kind>-NNN.<ext>` catalog id — the
bare key a `{{embed=…}}` marker carries on a page. Columns: `sha256`
(the content address of the blob at `<workdir>/media/<aa>/<sha256>`),
`kind` (closed producer vocabulary `photo` / `video` / `audio` / `doc`
— free TEXT, no DB CHECK, the `decay_reason` convention), `mime`,
`size_bytes`, the ACL triple (`owner_id` NOT NULL / `allow_ids` JSON /
`sender_id` materialized at birth since 0051, NULL only as the scrubbed
fallback — byte-compatible with `fact_index`),
`uploaded_by_consumer` (audit), `caption` / `description` /
`original_filename`, timestamps. `UNIQUE(sha256, owner_id)` makes
re-uploads idempotent per owner. **Not rebuildable from the markdown**
(the marker is a bare key — per-media metadata is DB-authoritative,
like the per-fact ACL): the workdir snapshot is the recovery story.
Design SSOT: [`media-pipeline.md`](media-pipeline.md).

### `document_jobs` / `document_job_segments` — document ingest (0040)

The checkpointed lifecycle of one `wiki_ingest_external` document job
(`document_jobs`: source, disposition dial, per-phase checkpoints,
`status = queued | running | done | failed`) plus the per-segment
extraction checkpoint (`document_job_segments`: a crashed worker
resumes from the last `done` segment, never re-running the whole
document). The same migration adds `fact_index.source_ref` (the
catalog id / url a fact was extracted from). Column-level detail and
the job algorithm: [`document-ingest.md`](document-ingest.md).

### `engine_meta` — engine-level key/value state (0041)

A tiny `key TEXT PRIMARY KEY / value TEXT` table for engine-level
state with no per-fact / per-wiki home. Consumers: the
embedder-identity guard (`embedder_model_id` / `embedder_dim` — see
[reindex-pipeline.md](reindex-pipeline.md#embedder-identity-guard-roadmap-18g))
and the deployment-wide 2FA toggle (`auth.require_2fa_all`).

### `disclosure_audit` — per-fact ACL change log (0043)

Append-only log of per-fact ACL edits made from the consumer chat (the
`acl_changes` ingest verb) and the operator surfaces: one immutable row
per applied change (actor, previous + new owner/allow/sender, a
`widening` flag from `crate::acl::widens`, `reverted_at` stamped on
undo); indexed on `(wiki_id, ts)` and `(fact_id, ts)`. Durable engine
state, **not rebuildable** — it logs a DB-authoritative column. See
[ingest-pipeline.md](ingest-pipeline.md#operation-path-edits--validity-edit--acl-change).

### `webagentoauth_*` — inbound OAuth authorization server (0044)

Three tables backing the inbound OAuth 2.x surface for bridge-less
smart consumers (claude.ai, Claude Code over the loopback redirect):
`webagentoauth_clients` (dynamic client registrations),
`webagentoauth_codes` (short-lived authorization codes),
`webagentoauth_refresh` (refresh tokens). Design SSOT:
[`web-agent-oauth.md`](web-agent-oauth.md).

### `dream_runs` / `compile_failures` — dream journal + compile-failure ledger (0054, 0055)

`dream_runs` is the durable journal behind the admin Dream console:
one row per finished dream run (`light` / `compile` / `full`), written
by both the manual dashboard triggers and the scheduler. Bounded by
design: `crate::dream_journal` prunes to the newest 100 rows after
each insert (a resource cap, not a semantic gate), and a scheduled
light tick that scanned nothing is not recorded.

```sql
CREATE TABLE dream_runs (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    kind           TEXT    NOT NULL,  -- 'light' | 'compile' | 'full'
    trigger_source TEXT    NOT NULL,  -- 'manual' | 'scheduled'
    ok             INTEGER NOT NULL,  -- 0 | 1
    summary        TEXT    NOT NULL,  -- one-line outcome (table row), or error message when ok=0
    log_text       TEXT    NOT NULL,  -- full report dump (modal), or error detail when ok=0
    started_at     TEXT    NOT NULL,  -- RFC-3339 UTC
    finished_at    TEXT    NOT NULL,  -- RFC-3339 UTC
    -- added by 0055 (per-page compile-failure surfacing — a completed run
    -- stops reading as plain ok when the compile was not clean):
    pages_failed   INTEGER NOT NULL DEFAULT 0,
    pages_degraded INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE compile_failures (         -- one row per page in a failing streak
    source_path TEXT    PRIMARY KEY,    -- workdir-relative page path (wikis/<id>/<page>.md)
    consecutive INTEGER NOT NULL,       -- consecutive failed/degraded compiles of this page
    last_error  TEXT    NOT NULL,       -- most recent failure message
    updated_at  TEXT    NOT NULL       -- RFC-3339 UTC of the last increment
);
```

The compiler increments `consecutive` on every failed **or degraded**
compile of a page and deletes the row on a clean full rewrite; streak
thresholds emit the `compile_failure_streak` notice on `wiki_events`
(observability thresholds, not semantic gates). See
[rem-cycle.md](rem-cycle.md#per-page-compile-failure-surfacing).

### `recall_traces` — the recall journal (0057)

One row per recall run — the whole route (flat/fresh/due-soon hits, the
entry-point fan, every navigator hop with its decision `note` and vetting
outcome, the injected block verbatim) as a **versioned JSON payload**
(`mwe_core::recall_trace::RecallTrace`, tolerant `serde(default)` decode).
Written best-effort by the ingest per-turn injection and the `wiki_navigate`
tool; pruned to the newest 10 rows after each insert (a resource cap —
`tool_executions` remains the audit surface). Read by the admin Traces
page and its 3D replay viewer. Design SSOT:
[`recall-pipeline.md`](recall-pipeline.md#recall-traces--the-last-10-journal).

```sql
CREATE TABLE recall_traces (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL,  -- RFC-3339 UTC
    source     TEXT NOT NULL,  -- 'ingest' | 'navigate'
    sender_id  TEXT NOT NULL,  -- bare user id the recall ran as
    payload    TEXT NOT NULL   -- versioned JSON: recall_trace::RecallTrace
);
```

### `recall_log` + `recall_misses` — the hindsight floor (0058)

Self-correcting REM's detection floor (`mwe_core::recall_log`): one lean
`recall_log` row per LLM-routed ingest turn (the surfaced fact ids + the
navigated pages' source paths, age-pruned at 30 days), and one
`recall_misses` row per **judge-free restated-known-fact miss** — memory
held the fact, that turn's recall did not surface it, the user restated
it (the write-time dedup hit is the proof; 90-day prune). The migration
also adds `capture_buffer.recall_log_id`, the turn linkage the
promotion-time detector reads (DB-only — never in the journal codec; a
journal-recovered row has no linkage and detection skips it). Migration
`0059` layers the repair stages on top: the turn's classifier `topics`
on the log row, and the miss lifecycle (`status`: `new → repaired |
queued | discarded | stale`, `resolution`, `seed_topics`) the REM
recall-repair sub-job drives. Design
SSOT: [`recall-pipeline.md`](recall-pipeline.md#the-hindsight-log--the-judge-free-miss-signal).

```sql
CREATE TABLE recall_log (
    log_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL,                 -- the turn's clock
    sender_id  TEXT NOT NULL,
    fact_ids   TEXT NOT NULL DEFAULT '[]',    -- JSON: surfaced ids
    page_paths TEXT NOT NULL DEFAULT '[]'     -- JSON: navigated pages
);
CREATE TABLE recall_misses (
    miss_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at    TEXT NOT NULL,
    sender_id     TEXT NOT NULL,
    fact_id       TEXT NOT NULL,              -- the fact recall failed to surface
    wiki_id       TEXT NOT NULL,
    source_path   TEXT NOT NULL,
    surface       TEXT NOT NULL,              -- 'direct' | 'promotion'
    similarity    REAL,
    restated_text TEXT NOT NULL,
    log_id        INTEGER
);
-- + ALTER TABLE capture_buffer ADD COLUMN recall_log_id INTEGER;
```

## Migrations — compile-time embedded

`sqlx::migrate!("../../migrations")` is invoked at the lib's top level
so the SQL is baked into the binary at build time. Two consequences:

1. A release binary needs **no extra files on disk** to bootstrap a
   fresh workdir. `cargo install mwe-mcp` followed by `mwe-mcp init
   --workdir /var/lib/mwe-mcp` works without shipping the
   `migrations/` directory.
2. The migration content cannot drift between source and runtime —
   editing a numbered migration file after release would change the
   compiled SQL, which is the wrong semantics for sqlx-migrate's
   integrity check (it stores a checksum of the applied SQL and
   refuses to re-apply if the source changed). New schema state always
   lands as a *new* numbered file.

The migration runner is invoked once per `open_or_init`, is
idempotent, and silently no-ops when the DB is already up to date —
verified by the `open_or_init_is_idempotent` test.

## Migration ledger

The authoritative count and the exact SQL live in
[`migrations/`](../../migrations/) — the directory is the SSOT, not
this list. Naming convention: zero-padded ordinal + snake_case
description; the ordinal is the version sqlx tracks in its
`_sqlx_migrations` table, the description is for humans grepping the
directory. One annotated row per migration:

| Migration | What it lands |
|---|---|
| `0001_fact_index` | The region index; UUIDv7 `fact_id`, the default-active filter columns. |
| `0002_wiki_events` | Event queue with the multi-consumer `acks` map. |
| `0003_tool_executions` | Audit trail; `args_hash` SHA-256, inert `cost_estimate`. |
| `0004_archive_proposals` | REM-emitted archival candidates. |
| `0005_structure_proposals` | Forge/promote/dedup questionnaire rows. |
| `0006_enrollment` | The two identity tables (`enrollment_users` + `enrollment_groups`). |
| `0007_wiki_types_registry` | `wiki_type` cache (table dropped by 0036). |
| `0008_proposal_ops_log` | Applicative WAL for structure-proposal apply. |
| `0009_rem_ops_log` | Applicative WAL for REM cycles. |
| `0010_token_blacklist` | JWT revocation list. |
| `0011_token_blacklist_revoked_by` | Adds the `revoked_by` audit column. |
| `0012_user_credentials` | Argon2id password hashes (dashboard login). |
| `0013_user_invitations` | Single-use 24h invite links. |
| `0014_consumer_delegations` | Act-as authorization for bots. |
| `0015_single_admin` | Partial unique index enforcing one admin per deployment. |
| `0016_consumers` | Consumer-agent registry gating `events_poll` / `events_ack`. |
| `0017_user_credentials_email` | Splits login email from the `user_id` slug. (Superseded for *where the column lives* by `0045`, which moves it to `enrollment_users`.) |
| `0018_profile_initialized` | First-login welcome-wizard flag on `user_credentials`. |
| `0019_proposals_5state_lifecycle` | Extends `structure_proposals` to the 5-state lifecycle. |
| `0020_enrollment_users_locale` | Per-user BCP-47 `locale` for prompt LANGUAGE injection. |
| `0021_wiki_types_family` | Adds `family TEXT` (the family marker; superseded by 0028). |
| `0022_wiki_admin_op_log` | Append-only op log for smart-wiki writes. |
| `0023_wiki_briefing_items` | DB mirror of the `_briefing.md` items. |
| `0024_skills_custom` | Per-owner custom skill catalog (table dropped by 0036). |
| `0025_briefing_target_cite` | Adds the `target_cite` citation handle on briefing items. |
| `0026_wiki_admin_leases` | Optional cooperative lease for cross-consumer `wiki_admin_push`. |
| `0027_op_log_revert_and_briefing_author` | `actor_kind` + `pre_image_json` on the op-log; `author_sender_id` on briefing items. |
| `0028_wiki_types_companion_bool` | Replaces `family TEXT` with `companion BOOLEAN`; translates the legacy value, drops the old column + index. |
| `0029_consumers_system_user` | Adds `consumers.system_user_id` — materialises the consumer ↔ system-user binding of the diagonal identity model (a standard consumer's own credential-less identity), populated by `consumer_register` from a `consumer_class = standard` token. |
| `0030_wiki_types_narrative_bool` | Adds the derived `narrative BOOLEAN` marker splitting non-smart types into narrative (prose) vs structured. |
| `0031_capture_buffer` | The standard-wiki captures buffer — a rebuildable index over the per-wiki `_captures.md` journal. |
| `0032_structure_proposals_recipient` | Adds the `recipient_id` addressee column on `structure_proposals` (per-user notice routing). |
| `0033_fact_index_validity` | Adds the per-fact validity columns `valid_from` / `valid_to` / `decay_reason` + the partial `idx_fact_valid_to` (the per-fact validity model). |
| `0034_capture_buffer_validity` | Adds `valid_from` / `valid_to` to `capture_buffer` — threads validity through the narrative buffer→promote path so `promote_one` copies it into `fact_index`; mirrored in the `_captures.md` journal (`vf`/`vt`) to keep the captures-journal rebuild faithful. |
| `0035_placement_axis` | Adds the ingest placement axis `target_page` / `style` / `page_description` to `fact_index` and `style` / `page_description` to `capture_buffer` — carries the classifier's per-claim page/style/description proposal onto the fact so the light dream settles a fact on its ingest page without the strong-model Cartografo; the buffer columns are mirrored in the journal (`style`/`desc`, free-text `desc` percent-escaped). Additive + inert until the light-cadence consumer. |
| `0036_drop_wiki_types_registry` | Drops the two inert `wiki_type` caches — `wiki_types_registry` (0007–0030) and `skills_custom` (0024). The smart flag lives per-wiki in `_meta.md` (`smart: bool`), and only bundled skills remain. Pure removal; nothing reconstructible is lost (a `rm engine.db` rebuild simply no longer materializes them). |
| `0037_fact_salience` | Adds the per-fact `salience` column to `fact_index` and `capture_buffer` (journal attr `sal=`) — the always-on base-context axis. |
| `0038_capture_buffer_decay_reason` | Adds `decay_reason` to `capture_buffer` — stages the WHY of a closure gesture that lands while its target is still buffered; DB-only (never journalled), promotion stamps it onto the fact. |
| `0039_media_catalog` | The media catalog — per-media metadata + ACL behind `{{embed=…}}` keys, twin of `fact_index`; bytes live content-addressed under `<workdir>/media/`. |
| `0040_document_ingest` | Backing tables for the document-ingest job (`wiki_ingest_external`) — async checkpointed segmentation + map/reduce extraction. |
| `0041_engine_meta` | A tiny `key`/`value` table for engine-level state with no per-fact / per-wiki home. First consumer: the embedder-identity guard (the `embedder_model_id` / `embedder_dim` the store's vectors were built with — see [reindex-pipeline.md](reindex-pipeline.md#embedder-identity-guard-roadmap-18g)). |
| `0042_authored_refs` | `authored_refs TEXT NOT NULL DEFAULT '[]'` on `fact_index` **and** `capture_buffer` — a JSON array of plain `[[wiki_id/page]]` wikilinks (same shape as `topics`). Carries a smart consumer's project-page authorship breadcrumbs from `wiki_ingest_message`'s `metadata.authored_refs` through capture → light-dream → fact, so consolidation links instead of duplicating ("link, don't duplicate", roadmap group 17). |
| `0043_disclosure_audit` | Append-only `disclosure_audit` table — the change log of per-fact ACL edits made from the consumer chat (the `acl_changes` ingest verb; see [ingest-pipeline.md](ingest-pipeline.md#operation-path-edits--validity-edit--acl-change)). One immutable row per applied change (actor, previous + new owner/allow/sender, a `widening` flag from `crate::acl::widens`, `reverted_at` stamped on undo); indexed on `(wiki_id, ts)` and `(fact_id, ts)`. Durable engine state, not rebuildable — it logs a DB-authoritative column. |
| `0044_webagentoauth` | Inbound OAuth 2.x authorization-server state: `webagentoauth_clients` (dynamic client registrations), `webagentoauth_codes` (short-lived authorization codes), `webagentoauth_refresh` (refresh tokens) — backs an OAuth-connected smart consumer (the claude.ai web app, and Claude Code over the loopback redirect) with no per-turn bridge. See [`web-agent-oauth.md`](web-agent-oauth.md). |
| `0045_user_email_on_enrollment` | Moves the login `email` onto `enrollment_users` (the row born at invite, where the admin sets it) with a partial-UNIQUE index, back-fills it from `user_credentials`, then drops the `user_credentials.email` column + index that `0017` added. Login becomes email-only (no username fallback); see [setup-and-identity.md](setup-and-identity.md#the-login-resolution). |
| `0046_drop_enrollment_blurbs` | Drops the cosmetic free-prose blurbs `enrollment_users.profile` and `enrollment_groups.description` — nothing read them at runtime (the identity-wiki title falls back to the `user_id`; a group's routing prose is `scope`, the planner's group theme derives from the `group_id`). The surviving content channels are the per-user welcome primer and the group `scope`. |
| `0047_global_group` | Seeds the builtin universal `global` group as a normal `enrollment_groups` row (`INSERT OR IGNORE`, admin-editable `scope` for the ingest classifier's world-fact routing). Its `members` column is unused — membership is universal, enforced in code (`acl::principal_matches`); `enrollment::ensure_global_group` re-applies the seed at runtime. The default scope frames `global` ownership as world facts, NOT as "a public personal fact" (that is the `allow` visibility axis). |
| `0048_password_resets` | Single-use, short-TTL (~30 min) password-recovery links: `password_resets` (`reset_id` UUIDv7 as the only URL secret, `consumed_at` burn-once flip inside the new-hash transaction) + the partial expiry index. Self-service forgot-password — the admin is never involved; see [jwt-and-session-model.md](jwt-and-session-model.md). |
| `0049_user_2fa` | Dashboard TOTP 2FA: `user_2fa` (secret encrypted at rest with a `MWE_TOKEN_SECRET`-derived key — rotating the token secret invalidates every enrollment), `user_2fa_recovery_codes` (single-use, SHA-256-hashed), `pending_2fa` (the between-password-and-session challenge, deliberately not a JWT), plus `enrollment_users.require_2fa` (per-user enforcement; the deployment-wide toggle is `engine_meta` `auth.require_2fa_all`). Gates only the human login surface. |
| `0050_enrollment_is_agent` | Adds `enrollment_users.is_agent` — the explicit marker for a consumer agent's OWN identity (the system user a standard token binds), set when a standard consumer token connects or is issued. Mutually exclusive with a `user_credentials` login, enforced in both directions: an identity is EITHER a human with a login OR an agent. |
| `0051_materialize_sender_id` | `sender` and `owner` become two separate, always-materialized fields: backfills `sender_id = owner_id` on every NULL-sender row of `fact_index` (marker regions only — smart-wiki section rows kept `sender_id = NULL`; since `0062` they are not in this table at all), `capture_buffer`, `media_catalog` and `document_jobs`. Provenance is frozen at birth and never collapsed onto the *current* owner (a NULL read as "== owner" silently rebound provenance whenever an `acl_change` moved the owner); `NULL` survives only as the scrubbed-principal fallback. |
| `0052_behaviour_rules_page_rename` | Content migration unifying behaviour-rule storage onto the agent wiki's `rules.md` (every agent wiki already scaffolds one; the agent is never a *sender*, so the engine-policy reader never runs on an agent wiki — no collision): rewrites `fact_index.source_path` basenames `behaviour_rules.md` → `rules.md`. No-op on a fresh database. |
| `0053_structure_proposal_votes` | The `structure_proposal_votes` table — one final vote per `(proposal, voter)` (PK makes a re-vote a conflict), explicit `'yes'` / `'no'` so an all-voted quorum can resolve early, both FKs cascading, plus the per-voter index behind the pending-vote reminder. Backs the governed group-wiki page-deletion tally and the `fact_forget` audience vote (`crate::votes`). |
| `0054_dream_runs` | The `dream_runs` journal — one durable row per finished dream run (`kind` light/compile/full, `trigger_source` manual/scheduled, `ok`, `summary`, full `log_text`), written by the dashboard Dream console and the scheduler alike; `crate::dream_journal` prunes to the newest 100 rows (resource cap), and a no-op scheduled light tick is not recorded. |
| `0055_compiler_resilience` | Per-page compile-failure surfacing: adds `pages_failed` / `pages_degraded` to `dream_runs` (a completed run stops reading as plain ok when the compile was not clean) and creates the `compile_failures` ledger (`source_path` PK, `consecutive`, `last_error`, `updated_at`) behind the `compile_failure_streak` notice — see [rem-cycle.md](rem-cycle.md#per-page-compile-failure-surfacing). |
| `0056_fact_successor` | Adds `successor_fact_id` to `fact_index` — the succession pointer on a **live** closed row (`close_validity` stamps it when the closer knows the replacement), projected by the compiler as the `(current: [[…]])` hint so a closed fact's prose points at today's truth. Distinct from `superseded_by` (welded to the tombstone). |
| `0057_recall_traces` | The `recall_traces` journal — one row per recall run (`source` ingest/navigate, `sender_id`, versioned JSON `payload` = `mwe_core::recall_trace::RecallTrace`: hits, entry-point fan, per-hop funnel journal, injected block verbatim), written by the ingest turn and the `wiki_navigate` tool, pruned to the newest 10; behind the admin Traces page + 3D replay viewer — see [recall-pipeline.md](recall-pipeline.md#recall-traces--the-last-10-journal). |
| `0058_recall_log` | Self-correcting REM's detection floor — `recall_log` (one lean row per ingest turn: surfaced fact ids + navigated page paths, 30-day prune), `recall_misses` (one row per judge-free restated-known-fact miss, 90-day prune), and the `capture_buffer.recall_log_id` turn linkage the promotion-time detector reads — see [recall-pipeline.md](recall-pipeline.md#the-hindsight-log--the-judge-free-miss-signal). |
| `0059_recall_repair` | The repair stages on top of 0058 — `recall_log.topics` (the turn's classifier seeds, the query side of a gate replay), `recall_misses.{status,resolution,seed_topics}` (the miss lifecycle `new → repaired \| queued \| discarded \| stale` + the receipt anchor), and the status index — consumed by the REM [recall-repair sub-job](rem-cycle.md#recall-repair-sub-job--self-correcting-rems-repair-stage). |
| `0060_recent_exchanges` | The `recent_exchanges` buffer behind the cross-consumer recent window (group 43) — a bounded, TTL'd per-user serving buffer of the exchanges the per-turn ingest already receives (`user_id`, `consumer_id`, `channel`, `author`, `text`, `occurred_at` + the per-user index). **Not** a transcript store: never indexed, never embedded, never REM-processed; cap and TTL enforced in the write path (`mwe_core::recent_window`). |
| `0061_enrollment_users_timezone` | Per-user IANA `timezone` on `enrollment_users` for ingest reference-time stamping — the sender's zone wins over the deployment-wide `recall.ingest_timezone` (two users of one deployment can live in different places). Set from the users page or the welcome wizard; a per-turn zone from the consumer is a tracked protocol extension. |
| `0062_wiki_sections` | Smart-wiki content leaves `fact_index` for its own pair of tables: **`wiki_sections`** (one row per heading-delimited section, keyed by `(source_path, section_ord)` — no ACL, no lifecycle, see [`mwe_core::sections`](../../crates/mwe-core/src/sections.rs)) and **`smart_wikis`** (a queryable projection of each smart wiki's `_meta.md`: owner, `shared_with`, `project_id`, `wiki_type`). **DDL only** — SQL cannot tell which `wiki_id`s are smart, because that flag lives on disk, so the data move is the tree-aware idempotent boot pass [`reindex::backfill_smart_sections`](../../crates/mwe-core/src/reindex.rs), which copies embeddings verbatim. |
| `0063_smart_wikis_slug` | `smart_wikis.slug` — each smart wiki's directory slug, mirrored from `_meta.md`. Feeds the per-turn **named-project trigger**: a standard consumer's turn recalls facts only, unless the message names a readable smart wiki, in which case that wiki's sections are ranked into a labelled reference slot ([recall-pipeline.md](recall-pipeline.md#the-project-docs-slot--two-ways-in-one-of-them-gated)). Deliberately the slug and not the title — titles carry generic words that would fire on ordinary conversation. Backfilled empty; the registry projection fills it on the next boot or safety-net tick, and an empty slug simply never matches. |

| `0064_rem_verdicts` | The REM confirmers' **negative-verdict memo** — `(kind, key_hash)` PK plus a debugging `subject_ref` and the `created_at` the TTL sweep reads. A row means "this exact question, on this exact content, judged by this exact model and prompt, already came back no", so the per-cycle confirm caps stop being spent re-buying settled verdicts. `key_hash` is a SHA-256 over the model id and the rendered prompt, so content, prompt, and model changes all self-invalidate; only negatives are stored (a positive mutates the corpus and invalidates its own key). Bounded by `RemPolicy::verdict_memo_ttl` (default 90 days) at cycle start — see [`mwe_core::rem_verdicts`](../../crates/mwe-core/src/rem_verdicts.rs) and [rem-cycle.md](rem-cycle.md#the-verdict-memo--why-examined-now-means-asked). |

| `0065_wiki_sections_fts` | **`wiki_sections_fts`** — an FTS5 external-content index (`content='wiki_sections'`, `unicode61 remove_diacritics 2`) over each section's `heading_path` and `"text"`, plus the three triggers that maintain it. Recall fuses its `bm25` ranking with the cosine one so that an **identifier** — `D-006`, an ADR number, a ticket id, a stack-trace symbol — can be found at all: an embedding has almost nothing to encode in one, and the query `D-006` used to return the section that merely *cites* it. The heading is a separate, 4×-weighted column because `"text"` already contains the heading chain, and counting it twice is exactly what separates the section that *is* `D-006` from one that refers to it (measured: 4 of 7 decision identifiers ranked first with one column, 7 of 7 with two). Triggers live in the schema, not in the Rust write path, so no writer can bypass them. Fully regenerable, and cheap enough to be: 2.5 MB and 60 ms on the 4 220-section production corpus, with no embedder. See [recall-pipeline.md](recall-pipeline.md#the-section-corpus-is-ranked-by-two-passes-fused). |
| `0066_llm_usage` | **`llm_usage`** — one row per internal-LLM call: slot, backend, model, `kind`, `billing`, `source`/`tag`, the four token columns, latency, and the error *class* of a failed call. Written by the `usage::maybe_wrap` decorator in `build_backend`, so every slot and transport is covered without touching a call site; **no prompt text is stored**, which is the whole reason this is not the training spool. The prompt is kept as three quantities because providers price them at three rates — plain input is `prompt_tokens - cached_prompt_tokens - cache_write_tokens` — and `billing` is its own column because the same provider is metered against a key in one config and covered by a flat subscription in the next. `NULL` means *not reported*, `0` means *measured zero*. Swept against `usage.retention_days` (default 400) at most once a UTC day. See [`mwe_core::usage`](../../crates/mwe-core/src/usage.rs) and [llm-usage-ledger.md](llm-usage-ledger.md). |

## How the runtime gets here

```
mwe-mcp serve --workdir /var/lib/mwe-mcp
   │
   ├── lockfile::acquire(workdir)        ← fail with 409 if held
   │
   ├── db::open_or_init(workdir)
   │     ├── ensure dir, open .db file (create_if_missing)
   │     ├── apply pragmas: WAL, FK, busy_timeout
   │     └── run sqlx::migrate!() against the pool
   │
   ├── wal::rollback_stale_proposals(pool, DEFAULT_STALE_AFTER, NoopInverse)
   │     └── flips stale `pending` / `in_progress` rows to `failed`
   │         with `error_msg = "rolled_back_by_startup"`
   │
   ├── wal::rollback_stale_rems(pool, DEFAULT_STALE_AFTER, NoopInverse)
   │     └── same shape for the REM journal
   │
   └── bring up MCP transport, watcher, REM scheduler — see
       `mcp-dispatcher.md`, `reindex-pipeline.md`, `rem-cycle.md`.
```

The recovery sweep uses `NoopInverse` today: the shipped apply
handlers (`promote`, `dedup`, `forge`) lean on atomic-write
idempotency rather than per-step inverses, and REM cycles are
restartable. No apply handler ships a real `OpInverse` impl yet — the
`bundle` kind is the candidate to be the first (planned — see the
roadmap). See
[`applicative-wal.md`](applicative-wal.md) for the recovery contract.

## Schema drift policy

When this page (or the planning corpus) and a migration disagree, the
**migration wins** — that's the SQL the running DB actually has. The
drift is fixed by correcting the documentation, never by silently
editing a migration after release (sqlx-migrate's checksum integrity
forbids it anyway). A planning change that needs a schema update ships
a *new* numbered migration that performs the `ALTER TABLE`, and this
page's canonical DDL + ledger are updated in the same commit.
