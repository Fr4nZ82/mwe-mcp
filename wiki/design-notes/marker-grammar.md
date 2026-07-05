---
title: Marker grammar — the region key + inline-attribute syntax
area: design-notes
status: implemented
last_review: "2026-06-29"
---

# Marker grammar — how `mwe-core::parser` reads `{{…}}…{{/}}`

A memory-wiki `.md` file is plain markdown with one custom addition:
**inline markers** that delimit *regions* of text and key them to a
`fact_id` (optionally carrying an inline ACL and a cross-user
attribution — see the runtime-vs-export note below). The choice is
deliberate — region-level granularity *inside* a single paragraph, at
the cost of giving up native Obsidian rendering of the markers
themselves. The rationale and the trade-off live in
[`../concepts/memory-model.md`](../concepts/memory-model.md); the ACL
model the markers feed lives in
[`../concepts/identity-and-acl.md`](../concepts/identity-and-acl.md).

Markers are **inline**: they can appear in the middle of a paragraph.
The markdown *outside* any marker is free prose and inherits the
`scope` of the node's `_meta.md` — but see the redaction note
below, prose is never *filtered*, only regions are.

**Scope: standard memory wikis.** Markers are the per-fragment mechanism
of the **standard** memory wikis (the founding ACL-per-fragment idea).
**Smart** (project) wikis carry no markers at all — they are plain
markdown, content-indexed by section, with one wiki-level ACL in `_meta`;
see [`smart-wikis.md`](smart-wikis.md) and
[`reindex-pipeline.md`](reindex-pipeline.md#smart-wikis--content-indexing-markerless).

This page is the **canonical grammar** for the marker syntax *and* the
implementation reference for
[`crates/mwe-core/src/parser.rs`](../../crates/mwe-core/src/parser.rs).
The hand-written Rust scanner is authoritative; the implementation
choices it makes are called out explicitly in [§4](#4-implementation-choices).

## 0. Runtime form vs export form — what gets written when

The grammar below accepts the full attributed marker, but the two
serializers write different subsets of it
([`capture::render_marker` / `render_full_marker`](../../crates/mwe-core/src/capture.rs)):

- **Runtime (what the engine writes on every capture/compile):** the
  **bare** form `{{f=<uuid>}}body{{/}}` — region key only. The ACL
  lives in the `fact_index` columns and gates the region **by that
  key** at render time
  ([`redaction-policy.md`](redaction-policy.md)); writing it inline
  too would be a second, driftable copy.
- **Export/interchange:** the **full** form
  `{{owner=… allow=… sender=… f=…}}body{{/}}` — each fragment
  self-describing, for a portable archive read without the engine DB.
  Produced by the dashboard export feature
  ([`mwe_core::export`](../../crates/mwe-core/src/export.rs), served at
  `GET /dashboard/wiki/:id/export`), which joins the on-disk prose with
  the DB ACL per fact key; a region the DB does not know stays bare and
  is counted in the export report.
- **Accepted input:** both, indefinitely. Legacy pages written before
  the bare form, and imported archives, parse unchanged; where the DB
  knows the fact key, the inline attributes are ignored (the DB wins),
  and a region the DB does *not* know is still gated by them. No
  one-shot migration is needed — any page recompile naturally rewrites
  to the bare form.

## 1. Grammar (EBNF-like)

```
text                ::= (region | embed | char)*
region              ::= marker_open content marker_close
marker_open         ::= '{{' attr_list '}}'
marker_close        ::= '{{/}}'
attr_list           ::= attr (WS attr)*
attr                ::= acl_attr | sender_attr | fact_attr
acl_attr            ::= 'owner=' principal | 'allow=' principal_list
sender_attr         ::= 'sender=' principal          # cross-user attribution (see §5)
fact_attr           ::= 'f=' fact_id
embed               ::= '{{embed=' catalog_id '}}'   # self-closing
content             ::= (text minus marker_close)*

principal           ::= 'global' | 'user:' user_id | 'group:' group_id
principal_list      ::= principal (',' principal)*
user_id             ::= [a-z][a-z0-9°]*              # lowercase + degree-sign collision marker
group_id            ::= [a-z][a-z0-9]*
                        # enrollment grammar (a subset of the WikiId charset, so every id
                        # gets its identity wiki); the wire parser itself is prefix-split
                        # and charset-lenient, so legacy ids keep parsing forever

fact_id             ::= uuid_v7                       # UUIDv7, lowercase with dashes
uuid_v7             ::= 8*hex '-' 4*hex '-' '7' 3*hex '-' variant 3*hex '-' 12*hex
variant             ::= '8' | '9' | 'a' | 'b'         # RFC 4122 variant 1
hex                 ::= [0-9a-f]
catalog_id          ::= 'c-' YYYY '-' MM '-' DD '-' kind '-' NNN '.' ext
kind                ::= [a-z]+                         # parser-permissive; canonical vocabulary below
ext                 ::= [a-z0-9]+

WS                  ::= ' '+                           # one or more spaces
```

The `fact_id` and `catalog_id` shapes are enforced by the validators in
[`mwe-core::types`](../../crates/mwe-core/src/types.rs)
(`FactId::parse`, `CatalogId::parse`) — see [§6](#6-related-types). The
canonical `fact_id` is the `UUIDv7` form; the older `f-YYYY-MM-DD-NNN`
shape fails validation.

### Syntax notes

- **`global` is the builtin universal group**, written **bare** (no
  `group:` prefix) for back-compat with legacy `owner=global` markers;
  `group:global` parses identically. Every user is implicitly a member, so
  `global` in any of `owner`/`sender`/`allow` opens the region to everyone.
- **`user:` / `group:` prefixes are mandatory** for every other
  principal, to disambiguate name collisions between a user and a group
  with the same id.
- **`allow=` takes a comma-separated list** with no spaces inside it:
  `allow=user:bob,group:team`.
- **No spaces around `=`**: `owner=user:alice` is valid,
  `owner = user:alice` is not.
- **A region with no `owner=` and no `allow=`** (e.g. `{{f=<uuid>}}body{{/}}`,
  pure `fact_id`) carries no ACL of its own: the parser leaves
  `acl.owner = None`, and `render` resolves the owner-of-last-resort to
  the region's own `sender` — unreadable if it has none, never the wiki
  `scope` (see [`../design-notes/redaction-policy.md`](../design-notes/redaction-policy.md)).
- **A region with no `f=`** (e.g. `{{owner=user:alice}}body{{/}}`, pure
  ACL) applies its ACL but does **not** create a `fact_index` entry.
  Used to put an ACL on prose, or to selectively redact a paragraph
  without promoting it to a fact.
- **`sender=` is optional and typed as a full principal**
  (User / Group / Global). Its meaning and the auto-attribution
  convention are described in [§5](#5-cross-user-attribution).
- **Markers must stay on a single line.** The open `{{…}}` and the
  close `{{/}}` may not span a newline; the *content* between them may.
- **A self-closing `{{embed=…}}` is legal inside a region body.** The
  embed rides with its fact (page reorganizations move regions as byte
  spans, so an in-body embed travels); the parser collects in-region
  embeds into the Region event's `embeds` list while the body bytes
  stay verbatim. Region nesting remains unsupported: any other `{{`
  inside a body warns `NestedRegion`.
- **The `kind` segment is parser-permissive, producer-closed.** The
  parser accepts any `[a-z]+` kind — legacy ids (`foto`, `vid`) and
  imported archives stay valid input forever; the canonical vocabulary
  `photo` / `video` / `audio` / `doc` is enforced at minting time by
  the media catalog ([media pipeline](media-pipeline.md)).
- **Nesting is not supported.** A region may not contain another
  region. For complex mixes, use adjacent markers.

### Valid examples

```markdown
{{owner=user:alice f=018f1234-5678-7abc-9def-0123456789ab}}
Going through a stressful period at work. See [[work/acmecorp]].
{{/}}
```

Multiple regions in one paragraph (inline granularity):

```markdown
Alice weighs {{owner=user:alice f=018f1234-5678-7abc-9def-0123456789ab}}72 kg{{/}} as of May 10,
and {{owner=global f=018f1234-5678-7abc-9def-0123456789ac}}got a haircut{{/}} yesterday.
```

Combined `owner` + `allow`, with an embed inside the body:

```markdown
{{owner=user:gollum allow=user:frodo,group:famiglia f=018f1234-5678-7abc-9def-0123456789ad}}
Sméagol got an 8 in maths today.
{{embed=c-2026-05-11-doc-01.jpg}}
{{/}}
```

Self-closing media embed (no terminator):

```markdown
{{embed=c-2026-05-10-vid-01.mp4}}
```

### Invalid examples (parser emits a warning, recovers)

```markdown
{{owner=alice}}body{{/}}                 # missing user: prefix
{{owner = user:alice}}body{{/}}          # spaces around =
{{allow=user:bob+group:team}}body{{/}}   # separator must be a comma
{{global}}body{{/}}                      # must be owner=global
{{f=018f1234-5678-7abc-9def-0123456789ab}}body   # missing {{/}} terminator
{{f=f-2026-05-11-001}}body{{/}}          # legacy fact_id format (UUIDv7 required)
{{f=NOTAVALIDUUID}}body{{/}}             # not a UUIDv7
{{owner=user:alice}}{{owner=user:bob}}body{{/}}{{/}}  # nesting not supported
```

None of these abort parsing — each turns into a [`ParseWarning`](#3-the-parser)
and the offending bytes either become prose or are skipped, so the
scanner always makes forward progress.

## 2. Field semantics

| Attribute | Maps to | Meaning |
|---|---|---|
| `owner=<principal>` | `RegionAttrs.acl.owner` | The fact's **subject** — who/what it is *about* (not its author `sender`, not its audience `allow`); "owner" because the subject governs who may read it (an `acl_change` is owner-or-admin). Distinct from the wiki-level `owner_user` (the wiki's proprietor). `None` (attribute absent) ⇒ owner-of-last-resort is the region's `sender`. |
| `allow=<principal>,…` | `RegionAttrs.acl.allow` | Extra principals granted read access, additive to `owner`. |
| `sender=<principal>` | `RegionAttrs.sender` | Who *captured* the region, orthogonal to `owner`. Grants the capturer guaranteed read-back. See [§5](#5-cross-user-attribution). |
| `f=<UUIDv7>` | `RegionAttrs.fact_id` | The region's `fact_id`. Absent ⇒ ACL-only region, no `fact_index` row. |

The effective read set of a region is the **union** `owner ∪ allow ∪
{sender}` — not an intersection. Resolution is **DB-first**: when the
region's `f=` key is in the engine DB, the `fact_index` ACL columns
gate it and the inline attributes are ignored; the attributes gate
only regions the DB does not know. ACL evaluation against a reader
lives in `acl::can_read` and the redaction pass that drives it is
documented in
[`../design-notes/redaction-policy.md`](../design-notes/redaction-policy.md).

A region body is markdown in one of the three per-page writing styles —
free text (`prosa` / `prosa-tecnica`) or one record per line (`lista`).
The marker grammar is identical regardless of the body's style; the
parser never interprets the body.

## 3. The parser

Pure function: `parse(text: &str) -> ParseOutput`. Takes the contents
of a single memory-wiki markdown file, returns an ordered list of
`ParseEvent`s (`Prose` / `Region` / `Embed`) plus a list of
recoverable `ParseWarning`s — a `Region` event also carries the
`embeds` collected from its body. **It never panics and never returns a
hard error.** Malformed markers turn into warnings and the offending
bytes either become prose or are skipped, so the parser always makes
forward progress.

`ParseOutput.events` is ordered by input position and covers the input
without overlap, so concatenating the `text` / `body` of every event
in order reconstructs every byte the parser preserved (some malformed
fragments are turned into `Prose` rather than dropped). The warning
kinds (`UnclosedMarker`, `UnclosedRegion`, `StrayCloseMarker`,
`InvalidAttr`, `InvalidPrincipal`, `InvalidFactId`, `InvalidCatalogId`,
`UnknownAttr`, `NestedRegion`) are stable enough for the `wiki_lint`
tool family to group on; the enum is the SSOT in
[`parser.rs`](../../crates/mwe-core/src/parser.rs).

### Parse algorithm

The main loop scans the input once, dispatching on the shape of each
`{{…}}` it finds:

1. Find the next `{{` from the current position. If none, the rest of
   the input is one `Prose` event — done.
2. Emit any text before the `{{` as a `Prose` event.
3. Find the matching `}}`. If there is none before EOF, emit
   `UnclosedMarker`, keep `{{`-to-EOF as prose, stop.
4. If a `\n` appears between `{{` and `}}`, the marker open spans a
   newline → emit `UnclosedMarker`, keep `{{` as prose, advance past
   the `{{` (the `}}` we found is **not** consumed — it is not a valid
   close), continue.
5. Inspect the attribute string between `{{` and `}}`:
   - `"/"` → a `{{/}}` with no matching open → emit `StrayCloseMarker`,
     keep the whole `{{/}}` as prose, continue.
   - starts with `embed=` → parse the `catalog_id`. On success emit an
     `Embed` event; on a bad id emit `InvalidCatalogId` and keep the
     `{{embed=…}}` span as prose. Continue.
   - otherwise it is a **region open** → continue to step 6.
6. Find the next `{{/}}` terminator. If none before EOF, emit
   `UnclosedRegion`, keep the open marker as prose, continue.
7. Scan the body (before the terminator) for inner `{{`: a valid
   self-closing `{{embed=…}}` is collected into the region's `embeds`
   (an invalid catalog id there warns `InvalidCatalogId`); any other
   `{{` emits `NestedRegion` once — the outer region is still accepted
   with its body verbatim (no recursion).
8. Parse the attribute string into `RegionAttrs`, emit a `Region`
   event spanning the open marker through the terminator, advance past
   `{{/}}`.

Attribute parsing splits the attribute string on ASCII whitespace and,
for each `key=value` clause, routes on the key:

- `owner` → `value.parse::<Principal>()` into `acl.owner`.
- `allow` → split on `,`, parse each into `acl.allow`.
- `sender` → `value.parse::<Principal>()` into `sender`.
- `f` → `FactId::parse(value)` into `fact_id`.
- any other key → `UnknownAttr` warning.
- a clause with no `=`, or an empty key → `InvalidAttr` warning.

Each attribute is validated **independently**. A malformed value emits
the appropriate warning (`InvalidPrincipal` / `InvalidFactId`) but does
**not** abort the region — the surviving region carries whatever
attributes parsed cleanly. A single typo in one marker never poisons
the rest of the file at lint or render time.

## 4. Implementation choices

The Rust scanner is the authoritative description of actual behavior.
The notable choices it makes:

### No `sender = owner` auto-attribution at parse time

The parser performs **no** auto-attribution: when `sender=` is
absent, `RegionAttrs.sender` stays `None`. Keeping the parser a pure
syntactic transform — no semantic inference — is what makes the
round-trip "parse then re-serialize" loss-free. Materializing the
implicit attribution is a **capture-side** concern, and capture
**always** does it: a fact is born with `sender` materialized (=
`owner` when the capturer did not name a distinct one), stored as a
separate, explicit `fact_index.sender_id` column. `sender` is the
immutable provenance — frozen at birth so a later owner change never
silently rebinds it — and `sender_id = NULL` survives only as the
degenerate "scrubbed" state (e.g. a deleted user) that falls back to
owner at read time. See [§5](#5-cross-user-attribution) and
[`redaction-policy.md`](redaction-policy.md).

### Zero regex, single-pass byte scan via `str::find`

The implementation does not pull in `regex` — it uses `str::find`
(two-way string search) for the four constant patterns it cares about:
`{{`, `}}`, `{{/}}`, and `\n`. Reasons:

- The grammar is regular enough that a hand-written byte scan stays
  short (~150 lines including warning bookkeeping).
- The `regex` crate would add ~600 KB to the binary for one literal
  search per call site.
- The benchmark already clears the throughput target with `str::find`
  alone (see "throughput" below).

All boundary offsets come from finding the ASCII sequences above, so
they always land on UTF-8 character boundaries — slicing is safe.

### Newline check is post-hoc, not pre-bounded

The single-line constraint on marker opens *could* be enforced by
computing `line_end = find('\n', …).unwrap_or(text.len())` *before*
searching for `}}`. On newline-poor inputs (memory wikis with long
paragraphs) that fallback to `text.len()` makes the scan O(n²) — each
marker open re-scans the entire tail of the document for a
non-existent newline. The current implementation finds `}}` first,
then post-checks the span between `{{` and `}}` for a stray `\n`. The
inline comment in the main loop documents this trap so it does not
creep back in.

### No nesting — outer region wins, inner is verbatim body

The parser detects a stray `{{` inside an open region body and, unless
it opens a valid self-closing `{{embed=…}}` (collected into the
region's `embeds`, no warning), emits a `NestedRegion` warning and
accepts the outer region with its body unchanged. The nested-looking
inner marker is preserved as part of the body string — no attempt is
made to recursively parse it, and the outer region terminates at the
**first** `{{/}}`. If the no-nesting rule is ever relaxed, this is one
of two places that change (the other being the warning enum).

### Recovery preserves bytes

Every recovery path emits prose for the malformed range rather than
silently dropping bytes. The property test
`parser_terminates_with_sane_offsets` asserts monotonic, in-bounds
event spans on arbitrary inputs. This lets `wiki_lint` point users at
the literal offending characters.

### `fact_id` validation is strict

The parser captures the raw `f=…` value, then routes it through
[`FactId::parse`](../../crates/mwe-core/src/types.rs), which enforces
the canonical `UUIDv7` lowercase-with-dashes form (exact length 36,
dashes at positions 8/13/18/23, version nibble `7`, variant nibble in
`{8,9,a,b}`). On failure: an `InvalidFactId` warning is emitted, the
region is still accepted, and `attrs.fact_id` stays `None`.

### Attributes are warning-only, never fatal

See the parse algorithm above: each attribute is validated
independently and a malformed value never aborts the region.

## 5. Cross-user attribution

`sender=` records **who captured** a region, distinct from `owner=`
which records **whose fact it is**. The two dimensions are orthogonal
(`sender ⊥ owner` as an invariant). `sender` is a full principal
(User / Group / Global), and the read evaluator treats it as one extra
principal in the region's effective ACL — so the principal named in
`sender=` always gets guaranteed read-back, even when it is not in
`owner` or `allow`.

| Scenario | Marker | Meaning |
|---|---|---|
| Alice talks about herself | `{{owner=user:alice f=<uuid>}}` | sender = owner = alice; an input marker may omit `sender=`, capture materializes it to the owner |
| Galadriel talks about Gollum | `{{owner=user:gollum sender=user:galadriel f=<uuid>}}` | sender ≠ owner, explicit; Galadriel can re-read |
| Household ambient microphone | `{{owner=user:gollum sender=group:famiglia f=<uuid>}}` | the group's device captured it → every `famiglia` member re-reads, even without `allow=group:famiglia` |
| A team member notes a group fact | `{{owner=group:team sender=user:alice f=<uuid>}}` | sender = a specific user, owner = the group |
| Public capture device (edge case) | `{{owner=user:alice sender=global f=<uuid>}}` | sender = global ⇒ effectively a public region |

**Materialization, not collapse.** `sender` and `owner` are two
separate, always-materialized fields: capture freezes `sender` at birth
(= `owner` when the capturer did not name a distinct one), the DB stores
it explicitly, and the **export full-marker always emits `sender=`**.
The previous "omit `sender=` when it equals `owner`" lean form is gone —
collapsing the two was only safe while the owner never changed, and it
*does* change (the `acl_change` verb today; ownership transfer in the
future), at which point a `NULL` sender would silently rebind to the new
owner and the original provenance would be lost (corrupting provenance
consumers like `structure_proposals` recipient routing and
behaviour-rule recall). **Input stays lenient**: a legacy or imported
marker that omits `sender=` parses fine and is materialized to `owner`
on ingest — but everything mwe-mcp *produces* is explicit. The only
remaining `NULL` is the degenerate "scrubbed" state (e.g. a deleted
user), which falls back to owner at read time.

## 6. Related types

The parser produces values defined in
[`mwe-core::types`](../../crates/mwe-core/src/types.rs):

- `Principal` — `user:<id>` / `group:<id>` (the builtin `global` group is
  written bare), with `Display` and `FromStr` for marker round-trip.
- `Acl` — `owner: Option<Principal>` (None ⇒ falls back to the region's `sender`)
  plus `allow: Vec<Principal>`.
- `RegionAttrs` — what a single `{{…}}` carries (`acl`, `sender`,
  `fact_id`).
- `FactId` — strict `UUIDv7` validator.
- `CatalogId` — `c-YYYY-MM-DD-<kind>-NNN.<ext>` validator.

## 7. Throughput

Criterion benchmark in
[`crates/mwe-core/benches/parser_throughput.rs`](../../crates/mwe-core/benches/parser_throughput.rs).
Synthetic 10 MiB corpus with a realistic prose-to-marker ratio:

```bash
cargo bench -p mwe-core --bench parser_throughput
```

Current measurement on the maintainer's workstation: **~700 MB/s**
single-thread, release build — comfortably above the 10 MB/s target.
The bench is **not gated in CI** (criterion is slow on
shared runners and the absolute numbers are CPU-dependent); run it
locally when touching the scanner hot path.

## 8. Where this fits

- `acl::can_read` ([`crates/mwe-core/src/acl.rs`](../../crates/mwe-core/src/acl.rs))
  consumes the `RegionAttrs.acl` the parser builds.
- `render::render_for_sender`
  ([`crates/mwe-core/src/render.rs`](../../crates/mwe-core/src/render.rs))
  drives the parser, applies `can_read` per region, and produces the
  declassified per-sender output. The redaction policy it implements
  (inline `[redacted]`, prose always visible, total-redaction collapse)
  is documented in
  [`../design-notes/redaction-policy.md`](../design-notes/redaction-policy.md).
