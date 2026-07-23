---
title: Document ingest — long-form content that is not a conversational turn
area: design-notes
status: implemented
last_review: "2026-07-19"
---

# Document ingest — `mwe-core::document` + `wiki_ingest_external`

How a document — a meeting transcript, an appliance manual, a long
voice-note dump, a catalogued `doc` attachment — enters memory. The
conversational pipeline ([ingest pipeline](ingest-pipeline.md)) is
per-turn by contract; this pipeline is its batch sibling for content
that is not a turn. Short voice notes never come here: a host that
transcribes delivers the transcript as the message text and the normal
per-turn ingest handles it.

## The disposition dial — a document is a unit by default

The founding decision (2026-06-12): **atomize-everything is wrong for
documents.** A pellet-stove manual must be *consultable*, not shredded
across the wiki; a meeting must be *askable-about* as a unit. One
mechanism, one dial, three positions:

| Disposition | The document keeps | What crosses into the wiki | Canonical case |
|---|---|---|---|
| **`consult`** | Full identity: a **document page** (a normal page — testata, summary prose, the `{{embed=…}}` to the catalog blob) | Nothing | Appliance manual |
| **`dossier`** | The same document page (participants, date, summary) | **Selective extraction** — only facts that *transcend* the document (commitments, decisions, dates, personal facts), each carrying the backlink to the document page in its `authored_refs` | Meeting, phone call |
| **`dissolve`** | No identity (the blob stays in the catalog if there is one) | Full extraction: everything worth remembering, routed to the right pages | Long voice notes |

Who turns the dial: a first classifier pass over the document proposes
it (the LLM judges — no hardcoded gates); the caller's explicit
`disposition` forces it (the consumer's gesture carries intent: "tienimi
da parte questo manuale" vs "ricordati tutto"); `dry_run` previews the
proposal before anything is written. An unparseable proposal degrades to
`consult` — the conservative fail-safe: on a bad day nothing scatters.

Consultation at recall time: the document page is a normal page (the
recall navigator finds it like any other); the agent then reads the blob
through `GET /media/<catalog_id>` (ACL-gated) and answers from it.

## The wire — `wiki_ingest_external`

The verb was completed in place (no new tool id). Sources:

- **`media`** — an already-catalogued `catalog_id`
  ([media pipeline](media-pipeline.md)): the consumer-handed-document
  gesture. The caller must be in the catalog row's read set. Text comes
  from the blob (v1 reads UTF-8 `text/*` / markdown blobs) **or from
  the `text` trusted seam** — the consumer-supplied extraction of the
  bytes, mirroring `attachments[].description`: extraction-from-bytes is
  a deployment capability, and PDF & co. arrive through the seam.
- **`inline`** — the document text in the call (the recorder-transcript
  case for a source connector). Document-shaped inline text is
  **auto-promoted to the media rail** so the verbatim original stays
  citable — see [verbatim source promotion](#verbatim-source-promotion--the-promote-dial).
- **`file` / `git` / `url`** — still `501 not_implemented_phase_c`.

Optional fields: `disposition`, `format` (`prose` | `dialogue`),
`title`, `occurred_at` (the document's **semantic clock** — relative
dates inside the document resolve against it; defaults to the catalog
row's timestamp for `media` sources), `promote` (`always` | `never`),
`dry_run`, `force`. Exact I/O in
the [tool reference](../protocol/tool-reference.md#wiki_ingest_external).

The call returns an **async job receipt** (`job_id`, `status`)
immediately; queueing refuses when the `ingest` LLM slot is not
configured (no job that can never run). Enqueue is **idempotent** by
(document sha256, owner) across non-failed jobs unless `force`.
Completion is a `document_ingested` notice on `events_poll` (job id,
resolved disposition, title, document page, facts buffered, source ref).

## Verbatim source promotion — the promote dial

The media rail used to engage only when the caller chose it, and a
document-shaped text *pasted into the conversation* (a forwarded email,
a report body) landed as `source_kind=inline`: no `source_ref`, nothing
to cite, the verbatim original gone. The server now backstops the
choice: a document-shaped inline ingest is **promoted to the media
rail** — the text is materialised verbatim as a content-addressed blob
+ `media_catalog` row (kind `doc`, mime `text/plain`, owner = the
effective sender, caption = the title hint or the first line) — and the
job proceeds exactly as a `source.type=media` call: facts cite
`source_ref = catalog_id`, the anchor page carries the `{{embed=…}}`,
the blob ACL widens to the anchor's read set, and the dashboard serves
the cited original.

Two doors, one shape heuristic
(`mwe_core::document::looks_like_document` — deterministic, no LLM):

- **`wiki_ingest_external source.type=inline`** — promoted when the
  text is document-shaped; the response carries `promoted_catalog_id`,
  and `dry_run` reports `would_promote` without minting anything.
- **An oversized `wiki_ingest_message` turn** (the paste-into-chat
  case) — additionally pre-gated on `message_min_chars`, so ordinary
  chat never qualifies. The turn is archived + enqueued as a document
  job, and the conversational ingest sees a bounded head excerpt plus
  the promoted document as a linked attachment (the existing
  media-on-a-turn seam): the thread stays coherent without
  double-extracting the body. Guests never promote (their turns are
  ephemeral by design); `dashboard_command` and assistant-authored
  turns are exempt. The response carries `document_promoted`
  (`catalog_id`, `job_id`, `existing`).

The heuristic (`PromotionPolicy`, conservative compile-time defaults):
under `min_chars` (600) never; at/above `unconditional_chars` (4000)
always; in between one structural signal suffices — email header
cluster, forwarded/quoted-reply banner, `>`-quote density, markdown
density, or a greeting/sign-off pair. The **`promote` dial** (`always`
| `never`) forces the decision in either direction,
disposition-style: the caller's explicit gesture wins, absence
delegates to the heuristic. Idempotency is two-layer and aligned by
construction: the blob bytes are the text verbatim, so the catalog's
(blob sha256, owner) dedup and the job's (text sha256, owner) dedup
move together on retries.

## The job — checkpointed phases, one worker

`document_jobs` + `document_job_segments` (migration `0040`) hold the
lifecycle; every phase checkpoints on the row, so a crashed or
interrupted worker **resumes instead of re-running** (transient LLM
failures leave the job runnable; terminal ones mark it `failed`; a
failed *segment* is recorded and skipped, never sinking the document).
The worker (`document::run_worker_loop`, spawned by `serve` next to the
REM scheduler) polls every `document.poll_secs` and drives jobs serially
on the **`ingest` LLM slot** (workhorse tier — same slot, no new config).

1. **Classify** — the `document-classify` prompt proposes disposition,
   format, title, page slug, target wiki, summary, testata seeds.
   Routing is anti-hallucination like ingest: an unknown wiki falls back
   to the owner's identity wiki, then the first standard wiki.
2. **Segment** — deterministic and code-owned (the model judges content,
   never where to cut): prose cuts on markdown headings + paragraph
   packing (`segment_target_chars`, hard cap `segment_max_chars`);
   `dialogue` cuts on utterance blocks, detecting per-block timestamps
   (full ISO instants, or `[HH:MM]` against the document date). A
   document segmenting past `max_segments` is refused — no silent
   truncation.
3. **Anchor** (`consult` / `dossier`) — the document page is born as a
   direct capture (the live-write exception, like `requested_container`):
   body = the classify summary, plus the code-rendered `{{embed=…}}` for
   `media` sources; the document page's **testata** (style, description,
   topics) is seeded from the classify plan; `fact_index.source_ref`
   stamped; the blob's ACL widens monotonically to the anchor's read set
   (the same soft-fail widening as conversational claims). (The seeds are
   held in memory from the classify pass, not checkpointed — a worker that
   resumes after a restart anchors without them.)
4. **Extract (map)** — per segment, the `document-extract` prompt with
   the disposition's selectivity posture substituted in (`dossier`: only
   what transcends the document — an empty array is a good answer;
   `dissolve`: everything worth remembering). Per-fact output mirrors
   the conversational extraction (body, routing, validity, salience,
   placement seeds, **and the fact's `owner_id`/`allow_ids`** — its
   subject and audience, decided under the same ingest rules), capped at
   `max_facts_per_segment`. The prompt input mirrors `ingest`'s assembly:
   the uploader's `sender_groups` (id + `scope`) and each `available_wikis`
   entry's `scope` prose are the audience signals the extractor reads. In
   `dialogue` format a fact without its own validity inherits the segment's
   instant as `valid_from` — per-utterance time flows to per-fact
   validity.
5. **Conciliate (reduce)** — a document repeats itself, and capture-time
   jaccard misses paraphrase: candidates cluster by embedding cosine at
   `merge_threshold` (deterministic prefilter), and only multi-member
   clusters spend a `document-merge` call that rewrites one best body
   (every other field — routing, ACL, taxonomy, validity, salience,
   testata seeds — re-stamped unconditionally from the first cluster
   member in code: the merge model returns only the body, and anything it
   emits beyond it is discarded).
6. **File** — reduced facts land in the **capture buffer**
   ([narrative buffer](narrative-buffer.md)) with
   `source_kind = "document"` and `source_ref` = the catalog id / url
   (or `document-job:<id>` for un-promoted inline — a promoted job
   carries a real catalog id) — promotion copies the
   provenance onto `fact_index.source_ref`. The **claim text stays
   clean** — no inline `[[link]]` suffix (it would pollute embeddings
   and dedup and freeze prose the Cronista cannot restyle); for
   `dossier` facts the code-built `[[wiki/page]]` backlink to the
   document page rides `authored_refs` instead (the model never writes
   links, and the extract prompt forbids source citations in the body).
   From the buffer on, everything is the standard machinery unchanged:
   light-dream promotion, fresh-captures recall, compile.

## Provenance and ACL

- **`fact_index.source_ref`** (migration `0040`) is the per-fact audit
  column: which document a fact came from. DB-authoritative metadata
  like the rest of the per-fact columns — never rendered into the page.
  The journal codec carries it (`src=` / `sref=` attrs) so a
  journal rebuild preserves it for buffered facts.
- **Reader-facing provenance rides `authored_refs`** — the designed
  provenance channel: a `dossier` fact carries the `[[wiki/page]]`
  backlink to its document page there (never inside the claim text),
  the compiler projects it as a provenance breadcrumb on the fact's
  `{primary_facts}` line, and the Cronista weaves a terse prose
  reference to the document page — the wiki way, restylable, without
  the link contaminating the canonical claim. (A REM hygiene sweep
  converges pre-existing rows that still carry the old trailing
  `([[…]])` suffix — [REM cycle](rem-cycle.md#provenance-hygiene-sweep-sub-job).)
- **ACL**: a fact is a fact — an extracted fact's `owner` (subject) and
  `allow` (audience) are decided **per fact by the extractor** under the
  ingest rules (default `owner = user:<uploader>`, `allow = []`; widened
  from the group/wiki `scope` signals and the document's own cues), never
  derived from where it lands. An extractor-emitted `owner` that
  enrollment does not back is re-owned to the uploader
  (`enrollment::principal_exists`, fail-open on a DB error) — the engine
  floor under the prompt's `known_users` roster, closing the path where
  the 2026-06-30 dangling principal was coined. Its `sender` stays the
  uploader. The
  **anchor** fact (the document's own identity page, `consult`/`dossier`)
  keeps the job's `owner` + the explicit `allow` that rode the upload — no
  placement-derived widening. The blob's read set still widens
  monotonically to the anchor's at `GET` time (the media catalog's own
  ACL).

## Configuration

The `document:` config section ([config schema](../protocol/config-schema.md))
holds resource caps only — poll cadence, segment sizing, segment/fact
caps, classify sample size, merge threshold, input size cap. Never a
semantic gate: dispositions and extractions stay LLM judgments
(`crate::config::DocumentConfig` → `document::DocumentPolicy`).

## Boundaries (deferred — tracked in extensions)

- **`import`-as-pages**: a document too big to consult as one blob
  becoming *its own pages* in its own container with scoped search (the
  oversized-manual case).
- **Server-side PDF/binary text extraction** — the `text` seam covers it
  meanwhile.
- **`url` / `file` / `git` sources** (`501`).
- **A job-status surface** beyond the completion notice (dashboard view).

Code SSOT: `crates/mwe-core/src/document.rs` (pipeline, policy, worker),
`crates/mwe-mcp-server/src/mcp/tools.rs` (`call_wiki_ingest_external`),
prompts `document-classify` / `document-extract` / `document-merge`
(bundled in `crates/mwe-core/prompts/`, operator-overridable at
`<workdir>/prompts/`).
