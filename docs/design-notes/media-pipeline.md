---
title: Media pipeline — photos, video, audio and documents as memory
area: design-notes
status: implemented
last_review: "2026-06-12"
---

# Media pipeline — `mwe-core::media` + the `/media` byte pair

How a photo (or video, voice note, document) sent to a consumer agent
becomes memory, without betraying the two pillars:

- **Prose is the recall mechanism.** A stored blob nobody described is
  dead memory — it does not embed, does not navigate, does not emerge.
  A media item enters memory as an ordinary described **fact** (capture
  buffer → promotion → REM → validity, like every other fact), with the
  media as the fact's attachment — never the other way around.
- **Key-first, DB-authoritative.** The `{{embed=<catalog_id>}}` marker
  in a memory-wiki page is a **bare key**
  ([marker grammar](marker-grammar.md)). Everything behind it — kind,
  MIME, size and the **per-media ACL** (owner/sender/allow) — is
  authoritative in the `media_catalog` table of `engine.db`, the twin
  of `fact_index` ([engine DB](engine-db-and-migrations.md)); the bytes
  live on disk like the prose. Resolution is by key with the catalog
  ACL: a marker may be visible on a page while the bytes are denied at
  `GET` time. The per-fragment governance pillar covers photos too.

The authoritative code:

- [`crates/mwe-core/src/media.rs`](../../crates/mwe-core/src/media.rs) —
  catalog DAL, content-addressed blob store, `catalog_id` minting, the
  ACL widening primitive, the inline-serve safety call.
- [`crates/mwe-mcp-server/src/http_media.rs`](../../crates/mwe-mcp-server/src/http_media.rs) —
  the bearer-JWT byte pair (`POST /media`, `GET /media/:catalog_id`).
- [`crates/mwe-dashboard/src/routes/media.rs`](../../crates/mwe-dashboard/src/routes/media.rs) +
  [`md_render.rs`](../../crates/mwe-dashboard/src/md_render.rs) — the
  cookie-authenticated alias and the inline embed rendering.
- [`migrations/0039_media_catalog.sql`](../../migrations/0039_media_catalog.sql) —
  the table DDL.

## Storage — one global content-addressed store

Bytes live at `<workdir>/media/<aa>/<sha256>` (lowercase hex, sharded
by the first two hex chars), **outside any wiki directory**. Identical
bytes are stored once no matter how many catalog rows, facts or wikis
reference them; a media item embedded from a group wiki and a personal
wiki shares one blob, and a wiki move never touches the store (no
per-wiki path to rebase). Blobs are immutable: written to a temp name
and renamed, **always before** the catalog row is inserted — the same
ordering that makes the workdir snapshot self-consistent
([backup & DR](backup-and-dr.md)): a row present in a snapshot's DB
image always finds its blob in the later file copy.

The catalog row carries: `catalog_id` (PK), `sha256`, `kind`, `mime`,
`size_bytes`, the ACL triple (`owner_id` NOT NULL / `allow_ids` /
`sender_id`), `uploaded_by_consumer` (audit), `caption`, `description`,
`original_filename`, timestamps. Like the fact ACL after the
DB-authoritative move, the catalog is **not rebuildable from the
markdown** — the workdir snapshot is the recovery story, and a dogfood
reset must wipe `<workdir>/media/` together with `engine.db`
(`CLAUDE.md` §5.bis).

### `catalog_id` minting and the kind vocabulary

`c-YYYY-MM-DD-<kind>-NNN.<ext>` is minted server-side at upload: the
date is the server's upload day (the **fact** carries the semantic
time — `occurred_at`, validity — the id is just a key), `NNN` is a
per-day-per-kind counter, the extension comes from the client filename
when clean, else from the MIME type. The kind vocabulary is the closed
English enum **`photo` / `video` / `audio` / `doc`**, enforced at the
producer (`media::kind`, the `fact_index::decay` convention) — the
`CatalogId` *parser* deliberately accepts any `[a-z]+` kind so legacy
ids and imported archives stay valid input forever.

Upload dedup: a second upload of the same bytes by the same owner
returns the existing row (`dedup: true` — bridge retries and re-sent
photos are absorbed); the same bytes from a different owner mint a
fresh row sharing the blob, each row carrying its own ACL.

## Entry — two-phase: bytes out of band, then the ingest

The MCP ingest is JSON and stays JSON; bytes travel beside it:

1. **`POST /media`** — multipart, the same bearer JWT (and
   `X-MWE-Act-As`) as `/mcp`. Fields: `file` (required), `kind`
   (required), `caption` / `description` (optional). Returns the minted
   `catalog_id` (201 fresh / 200 dedup); the provisional ACL is stamped
   from the **effective** (act-as-resolved) principal as owner. Body
   cap: 32 MiB (`http_media::MAX_UPLOAD_BYTES`).
2. **`wiki_ingest_message.attachments`** — optional array of
   `{catalog_id, kind?, caption?, description?}`. The dispatcher
   validates every id against the catalog and requires the effective
   sender to be **in the row's read set** (an ingest must not link —
   and later widen — someone else's media sight-unseen); the catalog
   row's kind is authoritative over the declared one. The rest of the
   per-turn contract is unchanged ([ingest pipeline](ingest-pipeline.md)).

## How a photo becomes memory

When attachments are present the classifier *looks at them*: for every
`photo` without a consumer-supplied `description`, the blob bytes ride
the existing ingest LLM call as inline image parts (no new LLM slot —
`CompletionRequest.images`, wired for Gemini `inlineData`, Anthropic
`image` content blocks and Ollama `images`; caps: 4 images / 8 MiB per
turn). The prompt (ingest v2.27, Part 9) instructs the model to fuse
what it sees with the user's caption into an extraction's `body` and to
**claim** the attachment by listing its `catalog_id` in the
extraction's `attachments` array.

The markers are **rendered by code, never by the model** (the same
invariant as region markers): the orchestrator validates each claimed
id against the turn's attachment window (anti-hallucination — unknown
ids are dropped) and appends `{{embed=…}}` to the validated body via
`capture::render_embed_marker`. The marker lives **inside** the fact's
future region, so every page reorganization moves it with its fact.

Per kind in v1: **photo** gets the full pipeline (bytes + vision +
fact); **video** is archived with its caption only; **audio** rides as
transcript-is-the-fact (hosts that transcribe already deliver the
transcript as the message text) with the file as attachment; **doc** is
described from caption/description on the conversational path — its
*content* enters memory through
[document ingest](document-ingest.md) (`wiki_ingest_external` with
`source.type = media`).

### The consumer-supplied description path

`attachments[].description` is the trusted seam: a smart consumer with
its own vision, or a host-side recognizer ("photo of frodo and smeagol
at the gate"), describes the media itself; the server skips its own
vision pass and feeds the description to the classifier as text. This
is also the degrade path for a text-only ingest slot (the `all-local`
profile's workhorse cannot see) — vision capability is a **deployment
property of the ingest slot**, not a code guarantee
([config schema](../protocol/config-schema.md)).

### The never-dead guarantee (described media)

Once a turn is **accepted**, a catalogued attachment that carries any
describing text is filed: attachments the routed plan did not claim —
a recall/skip turn carrying a photo, an extraction that forgot its
claim, an unparseable plan, the LLM being down — are filed by a
deterministic fallback as one buffered fact each (body = description ∥
caption, plus the marker) into the sender's identity wiki. Three outs,
in all of which the media stays catalogued and recoverable (dashboard /
`GET /media`), just outside the wiki: an unclaimed attachment with **no
caption and no description** files nothing — a fact whose whole body
would be the kind word ("audio") has no recall surface and only
pollutes the identity page (the classic case is a voice note whose
transcript IS the message text, already captured as facts); a caption
carrying marker/comment syntax (`{{`, `}}`, `<!--`) would fail the
buffer validators and is likewise left catalogued-unfiled; and a turn
whose `text` is empty is rejected (`400 invalid_input`) **before**
anything files — a media turn must still carry non-empty text (send
the caption, or a placeholder like `[media]`, as the hermes bridge
does). A sender without an identity wiki is logged loudly, media
catalogued.

### Similarity surfaces ignore the marker

The embed marker is a key, not prose: embedding and n-gram dedup
compare the **marker-stripped** text (`parser::strip_embed_markers`) at
every similarity site (capture, light-dream promotion, the fresh-slot
re-embed, the REM date-normalizer re-embed). Two different photos with
near-identical captions dedup by their words, not their catalog ids;
the stored body keeps the marker.

### Downstream guards

- The REM **date normalizer** accepts a rewrite only when its embed set
  equals the original's — a date rewrite may never add, drop or alter a
  media link.
- The **compiler** repairs Cronista rewrites: any embed present in the
  fact's canonical text but missing from the model's `<fN>` span is
  re-appended by code before the marker is rendered
  (`compiler::restore_missing_embeds`); the deterministic paths
  (completeness guard, `lista` pages) copy the canonical text and need
  no repair.
- **`wiki_lint`** ships `embed_missing`: every embed on every page
  (standalone or in-region) must resolve to a catalog row whose blob
  exists.

## ACL — provisional at upload, widened by the facts

The row's `owner` is the effective principal at upload and never
changes. When ingest files a fact embedding the media, the fact's read
set (owner ∪ allow ∪ sender) is **unioned into the row's `allow_ids`**
(`media::widen_acl`) — monotone widening only, mirroring the `allow`
monotonicity invariant of `acl::can_read`; a media embedded by several
facts keeps the union. Widening is disclosure-relevant and traced; the
dedicated post-capture ACL-change verb (with its audit surface) is the
open roadmap-6 design and deliberately not built here.

Reads go through the same pure `can_read` as fact regions — owner,
every `allow` entry and the capturing sender are each sufficient;
**no admin bypass** ([redaction policy](redaction-policy.md)).

## Exit — serving, dashboard, export

- **`GET /media/:catalog_id`** (bearer + act-as): catalog ACL check,
  then the blob streamed with the row's MIME and a strong `ETag` (the
  sha256 — content-addressed bytes never change, so the bearer mount
  sends `Cache-Control: private, max-age=31536000, immutable`; the
  dashboard alias keeps a short `max-age=300, must-revalidate` so a
  shared browser's cache does not outlive logout, with If-None-Match →
  304 either way). Denial
  is **403 `media_forbidden`**, not a 404 masquerade — the embed key
  already disclosed existence. MIME safety: only image (minus SVG),
  video, audio and PDF are served inline; everything else goes out as
  `application/octet-stream` attachment so a stored HTML blob can never
  become same-origin script (`media::inline_safe`, shared by both
  mounts).
- **Dashboard** — `md_render` turns valid embed markers in prose into
  `<img>` / `<video controls>` / `<audio controls>` / download link
  (kind dispatched from the id; unknown/legacy kinds degrade to the
  link), pointing at the cookie-authenticated alias
  `GET /dashboard/media/:catalog_id` — the session cookie is
  path-scoped to `/dashboard` and never reaches the root mount. Markers
  inside code blocks/spans stay literal. The page-level redaction runs
  before rendering; a denied blob shows as a broken image, by design. A
  *direct* navigation to the alias (not an `<img>`/`<video>` fetch)
  renders the dashboard error page: the 403 carries the **content-ACL**
  copy (`DashboardError::NoAccess` — *"you don't have access to this
  item"*), deliberately distinct from the admin-gate copy (`Forbidden` —
  *"admin rights required"*), because admin does **not** bypass a read
  ACL, so claiming admin would unlock it is a misleading hint.
- **Export** — the archive bundles every referenced blob under
  `<root>/_media/<catalog_id>` plus a `_media/_catalog.json` manifest
  carrying each item's catalog row (kind, MIME, sha256, ACL, caption) —
  the media analogue of the full marker, since an embed key has no
  inline-attribute form. Dangling references are counted
  (`media_missing`), never silently dropped. This is what pays the
  self-sufficiency cost of the global store: a memory wiki's filesystem
  alone no longer carries its media; its export does.

## Per-host mapping (the agent-agnostic check)

| Host | Inbound media path |
|---|---|
| hermes (Telegram) | the host downloads via bot token into its cache → the bridge's gateway hook uploads from the cached file → `attachments` ([bridge](../../agents-bridges/hermes/README.md), [machinery](../development/agents-bridges.md)) |
| nanoclaw / openclaw / openhuman | same pattern on their own channel APIs |
| smart agents (Claude Code) | local file → `POST /media`; may self-describe via `description` |

The `description` seam is also where a host-side **recognizer** plugs
in today: names resolved against the enrolled users make the described
fact recallable by person. Recognition as a shared cross-agent
capability (enrollment galleries living in the memory, so different
agents resolve the same people) stays a future feature — nothing in
this pipeline precludes it.

## Deferred

- MCP base64 attach tool (`wiki_attach_media`) for stdio smart agents —
  v1 entry is HTTP-only; revisit with the first smart-consumer need.
- Video thumbnails / real video understanding and server-side STT →
  the multimodal-ingestion extension
  (roadmap, group 9).
- Bridge re-send of media on recall (outbound media in chat) — v1 only
  guarantees a cited embed is resolvable by whoever has the right to it.
- HTTP Range / 206 on the media mounts — Safari/iOS `<audio>`/`<video>`
  elements want byte ranges; v1 serves whole bodies.
- Recognition as a shared cross-agent capability (above).
