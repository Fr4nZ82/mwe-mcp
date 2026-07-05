---
title: Memory-wiki filesystem SSOT — layout, frontmatter, atomic write
area: design-notes
status: implemented
last_review: "2026-06-29"
---

# Memory-wiki filesystem SSOT

[`mwe-core::wiki`](../../crates/mwe-core/src/wiki.rs) owns the
`<workdir>/wikis/` directory tree at runtime. This page documents what
the module guarantees and where the remaining gaps are.

The filesystem is the source of truth: every API here either reads the
tree or rewrites a file atomically while the caller mirrors the change
into the `fact_index` (the DB is a rebuildable cache). The conceptual
rationale lives in [`memory-model.md`](../concepts/memory-model.md) and
[`identity-and-acl.md`](../concepts/identity-and-acl.md); this note
records how the Rust module realises them.

## On-disk layout

```
<workdir>/
  .mwe-mcp.lock           (single-writer lockfile, see crate::lockfile)
  engine.db               (SQLite, see crate::db)
  wikis/
    index.md              (root collector index — loose file, see below)
    <root_slug>/          (a wiki-root: top-level memory wiki)
      _meta.md            (YAML frontmatter; see WikiMeta)
      index.md            (Hub-Writer-managed hub page, optional)
      rules.md            (actor-wiki only: user-policy page; seeded at creation)
      <leaf>.md           (leaf prose pages)
      <sub_slug>/         (sub-wiki, recursive)
        _meta.md
        …
```

The `wikis/index.md` at the top is the **root collector index**: a
marker-less Obsidian hub linking every top-level wiki
(`- [[<slug>/index|<Title>]]`, smart wikis excluded), written by
[`wiki::write_root_collector_index`](../../crates/mwe-core/src/wiki.rs)
after a top-level wiki is created and once at server bootstrap. It is
**operator convenience only** (open `wikis/` as an Obsidian vault) — the
engine never reads it. Because it has **no `_meta.md` beside it**, it is a
*loose* file like `_styles/`: the re-index resolves it to `wiki_id = None`
(zero `fact_index` rows) and wiki enumeration (which keys on `_meta.md`)
never treats it as a wiki. It is **not** the recall "root index",
which is rendered per-sender at recall time and never persisted.

`rules.md` (actor-wikis only — user + group) is the **user engine-policy page**
([`wiki::RULES_FILENAME`](../../crates/mwe-core/src/wiki.rs)): a *user-facing*
page (no underscore, unlike the `_meta`/`_captures`
plumbing) seeded with a neutral default at [`create_identity_wiki`](../../crates/mwe-core/src/wiki.rs)
time. It holds the user's standing **governance** rules in natural language —
privacy/sharing + do-not-store only (behaviour rules live in the consumer's own
wiki). It is *all prose, no metadata*: the ingest **reads** it
(`sender_rules`) and **writes** it (an `engine_rule` extraction is appended as a
prose bullet via [`wiki::append_engine_rule`](../../crates/mwe-core/src/wiki.rs),
never filed as a fact); nothing is ever materialised onto
the wiki-level `scope`. It carries no `{{f=…}}` fact regions and the compiler
never derives it from facts, so the slug `rules` is **reserved from placement** the
way `index` is ([`planner::placement_slug`](../../crates/mwe-core/src/planner.rs))
and the file survives the compile/REM cycle untouched.

`<root_slug>` is a [`WikiSlug`](../../crates/mwe-core/src/types.rs) (`[a-z0-9-]`
plus the Samvise `°N` collision marker, no edge dash, no `--`). The
`wiki_id` stored inside `_meta.md.wiki_id` is the dash-join of the slug
chain from the root (`alice-acmecorp-widget-pro`) and survives renames
of the underlying directory chain.

## `WikiMeta` field model

The struct is [`wiki::WikiMeta`](../../crates/mwe-core/src/wiki.rs);
`WikiMeta::parse` promotes every required field to a typed Rust value,
defaults the optionals, and preserves any unknown key verbatim.

| Field | Required | Notes |
|---|---|---|
| `wiki_id` | ✓ | [`WikiId`] — `[a-z0-9°-]`, no edge dash, no `--`. |
| `wiki_type` | ✓ | A **bare string label**, never validated against a registry (there is no registry). The actor kinds — `wiki-user`, `wiki-group`, `wiki-root` — are created from internal logic at their kind-specific creation sites; a smart or emerged sub-wiki carries a neutral placeholder string. The smart-family gate reads the separate `smart: bool`, not this string. See [`smart-wikis.md`](smart-wikis.md). |
| `parent_wiki_id` | ✓ (nullable) | `null` only for the root. |
| `slug` | ✓ | [`WikiSlug`]. Is *expected* to match the on-disk dirname. The drift cannot be introduced by the write paths (`wiki_promote::file_to_subwiki`, `scope::wiki_change_scope` both derive the dirname from the slug), so today the two never diverge — but no `wiki_lint` check asserts the invariant yet (see Known gaps). |
| `title` | ✓ | Free Unicode. |
| `scope` | ✓ | `inherit` (forbidden on the root — parse rejects it there), `user:<id>`, `group:<id>`, or `global`. The wiki's **placement/category** principal (never an ACL), resolved through the parent chain by `WikiTree::resolve_scope_principal` (cap `MAX_ACL_DEFAULT_HOPS = 64`, cycle-guarded). A legacy `acl_default:` key is read-and-ignored and never re-emitted. |
| `shared_with` | – | List of [`Principal`] given **read + notify** on top of the owner (smart wikis). Write tools stay owner-only; the share only widens the read/notify perimeter. Empty for almost every wiki. |
| `style_overrides` | – | Free YAML map. |
| `keywords` | – | Free YAML map. |
| `children` | – | List of `{ wiki_id, slug, wiki_type }`, validated row-by-row at parse time. |
| `promoted_from` | – | Path string for wikis born from auto-promotion. |
| `no_archive` | – | Bool, default `false`. |
| `smart` | – | Bool, default `false`. `true` marks a **smart wiki** (smart-consumer-owned, written only via `wiki_admin_*`, never by ingest). The canonical per-wiki family marker read by the smart-family gates and stamped at actor-wiki creation. On-disk key `smart:`; `companion:` (the family's pre-rename name) is accepted as a read alias forever and migrates to `smart:` on the first rewrite. Round-tripped only when `true`. NB: the **writing-style** axis is per-page (page frontmatter) and **validity** (`valid_from`/`valid_to`) is per-fact (`fact_index`) — neither is on `_meta.md` (see [`ingest-pipeline.md`](ingest-pipeline.md) and [`memory-model.md`](../concepts/memory-model.md)). |
| `created` / `updated` | – | ISO 8601 strings, round-tripped verbatim. |
| `extra` | – | **Every unknown key** is preserved verbatim so forge-specific
  fields (`lead_time_notify`, `ttl_done`, …) round-trip through
  read + write without loss. |

A wiki carries **no wiki-level visibility flags**: there is no access gate
above the per-fragment ACL. A reader reaches a wiki/page iff it holds at
least one fact they can read — visibility is *derived*, never declared on
`_meta.md` (see
[`../concepts/identity-and-acl.md` §5](../concepts/identity-and-acl.md#5-wiki-visibility-is-derived--there-is-no-wiki-level-access-gate)).
`to_yaml` writes the optional fields above back only when one carries a
non-default value, so wikis that never touched them keep a minimal
frontmatter on rewrite.

`MarkdownDoc::parse` splits a `_meta.md` body into the YAML frontmatter
and the (usually empty) trailing prose; empty frontmatter (`---\n---\n…`)
is supported.

## Atomic write protocol

All writes go through [`atomic_write`](../../crates/mwe-core/src/wiki.rs):

1. Acquire a [`WriteMarker`](../../crates/mwe-core/src/watcher.rs) RAII
   guard on the target. While the marker is fresh, the file watcher
   ([marker protocol](single-writer-lockfile.md)) suppresses events
   on the target — internal writes are paired with index updates by the
   caller, so the watcher only ever surfaces *external* edits.
2. Create a `NamedTempFile` in the *same directory* as the target so
   `rename(2)` stays on one filesystem.
3. Write payload → `sync_data` on the temp → `persist` (atomic rename).
4. `fsync` the parent directory to make the rename durable across a
   crash. No-op on platforms that do not expose directory fsync.
5. Drop the marker.

Because the marker guard is RAII, a panic inside the write still cleans
up; the startup `sweep_stale_markers` covers the residual case where the
process disappears before drop runs.

## What the module deliberately does not do

- **Acquire the per-workdir lockfile.** That is the server's job
  ([`mwe-core::lockfile`](../../crates/mwe-core/src/lockfile.rs)); the
  wiki module trusts the caller holds it for the duration of a write.
- **Touch `fact_index` or `wiki_events`.** Capture / supersede / forget
  and REM pair the file write with the DB update inside the
  applicative WAL ([applicative WAL](applicative-wal.md)). This module is
  the I/O floor.
- **Apply any ACL on read.** Raw page contents come out as bytes; the
  per-sender redaction happens in
  [`mwe-core::render`](../../crates/mwe-core/src/render.rs) — see
  [`redaction-policy.md`](redaction-policy.md). There is no wiki-level
  gate to evaluate here: a wiki/page's reachability is *derived* from that
  per-fragment ACL (see
  [`../concepts/identity-and-acl.md` §5](../concepts/identity-and-acl.md#5-wiki-visibility-is-derived--there-is-no-wiki-level-access-gate)).

## Path safety

[`is_safe_page_path`](../../crates/mwe-core/src/wiki.rs) rejects every
page path that is not a relative chain of `[a-z0-9._-]+` components. The
charset is stricter than what the OS would accept on purpose:
- it pre-empts traversal (no `..`, no absolute, no `/`-rooted),
- it keeps the on-disk filenames Obsidian-friendly without surprises,
- it forbids `.`-prefixed names so `mwe-write-in-progress` markers
  cannot be addressed as ordinary pages.

`WikiTree::walk` skips a sub-directory that does *not* carry a `_meta.md`
silently (so a stray `recipes/` directory inside a wiki appears as
nested pages via `list_pages`, not as a phantom wiki), and recurses into
sub-wikis as their own discovery rows.

## Known gaps

- Slug-on-disk vs. `_meta.md.slug` mismatch is not asserted at
  filesystem-write time, and no `wiki_lint` check covers the invariant
  yet. The shipped [`lint::Check::MetaInvalid`](../../crates/mwe-core/src/lint.rs)
  flags a `_meta.md` that is *missing or unparseable*, but it does not
  compare the parsed `slug` against the containing directory name. The
  write paths that *could* create the drift (`wiki_promote`
  `file_to_subwiki`, `scope::wiki_change_scope`) both derive the
  on-disk name from the slug directly, so today the two cannot diverge
  in practice — the missing piece is a defensive lint, not a live bug.
- `WikiTree::locate(id)` does a linear scan; acceptable for trees with
  <500 wikis (the order of magnitude the model targets), to be replaced
  by an in-memory id→path index when that ceiling is in sight.
- `wiki_catalog_list` returns a flat per-`wiki_type` grouping; the
  richer dashboard catalog page (filterable by `wiki_type`, sortable
  by recall recency) lives at
  [`/dashboard/facts`](../../crates/mwe-dashboard/src/routes/facts.rs)
  for the fact-level slice but a wiki-level cousin (catalogue
  of *wikis*, not facts) is not yet built.
