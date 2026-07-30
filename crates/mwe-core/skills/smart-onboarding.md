---
name: smart-onboarding
version: 1.1.0
description: "First connect, once per project: the interactive intro (three questions — scope, name, is-the-existing-text-still-true — and the many questions you must NOT ask), the four situations (text already exists / code but no docs / brand-new project / the wiki is already on mwe), the faithful bulk copy that never passes through your context, the exact wiki_admin_push create wire shape, the post-import shape report (measured and reported, never asked), and the page-repair proposal that previews as a page list and only ever CUTS, never rewrites — proven by hash. Fetched when smart_bootstrap volunteers first_connect.hint, or when the user asks to bring a project into mwe. Propose once, record a decline, never open it mid-task."
depends_on: ["core", "smart-consumer"]
applies_to:
  consumer_class: smart
status: implemented
---

# mwe-mcp / smart-onboarding skill

The **first connect** of a project. It happens once per project, it is
the user's first impression of the product, and — unlike every other
flow in this system — **it cannot be re-run**: a wiki born from a
day-one rewrite of the user's documents, or from an LLM's summary of
their code, is memory that outlives its own errors because nobody ever
re-reads it.

This skill exists so that procedure is written **once**, here, instead of
being carried by the skills that load on every session. Everyday skills
pay for their length every time you connect; this one is fetched on the
rare occasion it applies.

## When to fetch and run this

Fetch it when **one** of these is true:

- `smart_bootstrap` came back with a **`first_connect.hint`** — you
  passed a `project_id` (see `core`) and the server answered that no wiki
  of yours carries it. That is the datum; this skill is the procedure.
- The user **asks** to bring a project, a `docs/` tree, or an existing
  wiki into mwe.
- You are in a project with a wiki and a page has grown too dense to
  index cleanly — a `warnings[]` line on a `wiki_admin_push` response, or
  a bad `shape` report. Jump straight to
  [Repairing a page](#repairing-a-page--preview-cut-prove).

## Three rules that govern *whether* you open it at all

They come before the procedure because getting them wrong is worse than
onboarding badly.

1. **Never mid-task.** If the user came to fix a bug at 23:00, they get
   no questionnaire. Finish what they asked for. Raise onboarding at a
   natural break, or at the start of the next session.
2. **Propose once.** If they decline, record it and never re-ask —
   "propose on connect" re-asked every session is worse than never
   asking. Write the decline into `.mwe/state.json`:

   ```json
   { "onboarding": { "declined_at": "2026-07-27", "project_id": "18a486b5c823a33f" } }
   ```

   The server has no way to know this: `first_connect.hint` will keep
   arriving, and **a recorded decline silences it**. Mention it again
   only if the user brings it up, or if something big changes (they ask
   you to remember project knowledge you have nowhere to put — park it in
   your operational wiki meanwhile so it is not lost).
3. **You propose; you never write silently.** Creating a wiki, copying
   documents up, restructuring pages — every one of them waits for a yes.
   The user may be browsing a read-only checkout, or may want this folder
   to stay out of mwe.

## What you ask — and what you must not

The filter, and it is worth internalising because it decides every future
question too:

> **A question belongs in the intro only if the user could answer it
> without opening the code, *and* the machine cannot answer it itself.**

That leaves **three**:

1. **What goes in** — scope. Only the user knows which parts of this
   project are relevant, and which are private. Ask in their terms:
   "everything under `docs/`, or only some of it? Anything in here that
   should *not* be remembered?"
2. **What it is called** — the wiki's title (and slug). Say what you
   propose and let them correct it: the folder name is often not the
   name they use for the project.
3. **How much you should trust the existing text** — *only* when
   importing. "Is this documentation still true, or is some of it stale?"
   Importing a two-year-old document as truth manufactures
   confidently-wrong memory. Import it either way — but if they say parts
   are stale, say so **in the wiki**: a line at the top of each imported
   page (`imported 2026-07-27, not verified`) costs nothing and stops a
   future answer from being quoted with unearned confidence.

**Never ask** — these are yours, and asking is a defect, not courtesy:
page sizes, section structure, heading style, folder layout, batch sizes,
`project_id` derivation, how your client authenticates, whether pages
"cover one thing". Shape is **measured and reported**, never asked (see
[the report](#after-the-copy-the-shape-report)).

Keep the whole intro to a handful of sentences. Say what a project wiki
*is* in one line — "a memory of this project that survives between
sessions, that I write and you can read and correct" — then the three
questions, then act.

## The four situations

They differ only in **where the first bytes come from**. The retrieval
path does not care whether the source was "a wiki" or "some notes":
smart wikis are indexed by **heading-delimited sections**, and they are
deliberately *not* part of the navigable link graph, so cards, wikilinks
and folder conventions buy a project wiki nothing. The only questions
that matter are **is it still true** and **does it chunk**.

### 1. Text already exists (a wiki, `docs/`, a long README, loose notes)

**Copy it faithfully first. Do not rewrite it on day one.** An agent
improving material it has just met is the fastest way to lose a user's
trust — and the fastest way to lose the one thing your copy has going for
it, which is that it is *theirs*.

Order of operations, and it is not negotiable:

1. Faithful bulk copy (below) — byte-for-byte, `mode=create` then
   `upsert`, the originals never renamed, moved, or deleted.
2. The shape report — measured, in plain language.
3. *Then*, if the report found something, a repair **proposal**.

**Where the local mirror lives.** If you copied an existing directory
(e.g. the repo's `wiki/`), that directory **is** the mirror: record it as
`local_wiki_root` and keep editing it in place. Never duplicate it into
`.mwe/wiki/` — that path is only the default for a wiki you seed fresh.

### 2. Code but no docs

**Do not auto-generate an overview from the code.** An LLM's summary of
a codebase reads authoritative, is largely-but-not-entirely right, and as
*memory* it outlives the error: nobody ever re-reads it, so the wrong 10%
is quoted back for years as fact.

The wiki is born **near-empty** and accretes from real work: an `index.md`
written from your conversation with the user — four sentences on what this
is and what matters — plus whatever the current session actually
establishes. That is a better first page than anything generated, because
every line of it was confirmed by a human this morning.

If the user *wants* a generated draft, produce it — presented as a
**draft to review**, not as memory, and only on their explicit request.

### 3. A brand-new project

The wiki is born with it: `index.md` plus a decision log, growing in
lockstep with the code. This is the cheapest case and the best one — say
so, and keep the first pages short.

### 4. The wiki already exists on mwe

`smart_bootstrap` answers this before you ask anything: `first_connect`
carries a `wiki_id` and **no** hint. The project was bootstrapped on
another machine, or the local `.mwe/` was wiped. **Reconnect and sync —
never a second wiki**: `wiki_admin_pull`, write `.mwe/state.json`
(`wiki_id`, `local_wiki_root`, `last_op_log_head`), reconcile any local
edits, then resume the ordinary loop in `smart-consumer`. Nothing in this
skill applies past that point.

## Creating the wiki — the exact wire shape

`mode=create` makes a **new wiki** (not a page) and needs four fields the
call is easy to get wrong:

```jsonc
wiki_admin_push({
  "mode": "create",
  "parent_wiki_id": "<your own root wiki = your sender_id, e.g. \"franz\">",
  "slug": "<short id under the parent, e.g. \"lnprint\">",
  "title": "<display name, e.g. \"LNPrint — engineering wiki\">",
  "wiki_type": "project",     // free-form label; it does NOT make the wiki smart
  "smart": true,              // THIS makes it a smart wiki (markerless, section-indexed)
  "project_id": "<the derived id you passed to smart_bootstrap>",
  "pages": [ { "path": "index.md", "content": "…" } ]
})
```

The response carries the new `wiki_id` — every later call uses
`mode=upsert` with it. Omitting `parent_wiki_id` yields
`400 wiki_type_requires_parent`; the message now names the value to pass,
but you should not need it.

Always pass `project_id`: it is what makes this wiki findable from
another machine, and what turns off `first_connect.hint` for good.

## The bulk copy — file → script → server, never through you

The **first** copy of an existing wiki or `docs/` tree moves many
(sometimes large) pages. Reading each page into your context and
re-emitting it as a `wiki_admin_push` argument *works*, but the bytes
pass through you twice: a big wiki burns tokens for nothing, and a page
over your file-read ceiling cannot be read in one go at all.

**So don't.** You have a shell: write and run a small script that walks
the tree and calls `wiki_admin_push` over `/mcp` itself.

```
POST <server>/mcp
Authorization: Bearer <smart JWT>
Content-Type: application/json
Accept: application/json, text/event-stream

{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
  "name":"wiki_admin_push",
  "arguments":{ "mode":"create", "wiki_type":"project", "smart":true,
                "project_id":"<id>", "parent_wiki_id":"<your sender_id>",
                "slug":"<slug>", "title":"<title>",
                "pages":[ {"path":"index.md","content":"…"} ] }}}
```

A Windows PowerShell sketch — no extra tools, `ConvertTo-Json` escapes the
content for you (elsewhere the same shape over `bash` + `curl`, using `jq`
to build the JSON):

```powershell
$srv=$env:MWE_SERVER; $jwt=$env:MWE_JWT; $root="wiki"; $proj=$env:MWE_PROJECT_ID
$hdr=@{Authorization="Bearer $jwt"; "Content-Type"="application/json";
       Accept="application/json, text/event-stream"; "User-Agent"="mwe-bulk-copy/1.0"}
function Push($a){ $b=@{jsonrpc="2.0";id=1;method="tools/call";
   params=@{name="wiki_admin_push";arguments=$a}} | ConvertTo-Json -Depth 20
   Invoke-RestMethod "$srv/mcp" -Method Post -Headers $hdr -Body $b }
$base=(Resolve-Path $root).Path
$pages = Get-ChildItem $root -Recurse -Filter *.md | ForEach-Object {
   @{ path = $_.FullName.Substring($base.Length+1) -replace '\\','/';
      content = [IO.File]::ReadAllText($_.FullName) } }
$r  = Push @{mode="create"; wiki_type="project"; smart=$true; project_id=$proj;
             parent_wiki_id=$env:MWE_SENDER; slug=$env:MWE_SLUG; title=$env:MWE_TITLE;
             pages=@($pages[0])}
$wid= ($r.result.content[0].text | ConvertFrom-Json).wiki_id
for($i=1; $i -lt $pages.Count; $i+=10){
   Push @{mode="upsert"; wiki_id=$wid; pages=$pages[$i..([math]::Min($i+9,$pages.Count-1))]} }
```

**Auth.** The script needs a **smart** Bearer JWT in `$MWE_JWT`. Either
reuse the OAuth `access_token` your own connection already holds, *if your
host exposes it* to a shell (short-lived, ~1 h — fine for a one-shot
copy); or the operator mints one once (`mwe-mcp token-issue --class smart
…`, or the dashboard token page). The token is the only setup step on this
machine — `mwe-mcp` itself is never installed here; the script only speaks
HTTP to the remote server.

**Identify your client.** Set an explicit `User-Agent` naming the flow
(e.g. `mwe-bulk-copy/1.0`) instead of shipping your HTTP library's
default. It is what the operator reads in the access log when a bulk run
misbehaves, and stock library defaults are the signatures a filter in
front of the server drops first — so a `403` whose body never mentions
mwe-mcp came from that edge, not from your token. Never impersonate a
browser to get past one: say what you are.

**Batch size.** Keep batches small (≈10 pages). The push acks before the
sections are embedded — the response says `section_indexing: "queued"` —
so latency is transfer-bound, not embedding-bound; but a fronting proxy
still cuts a long request (~100 s), and a batch that dies takes its whole
payload with it. Small batches also make a partial failure obvious.

**`op_log_id` is global, not per-wiki.** The id in a push response is the
server's own counter across all wikis, so it jumps: expecting 33 and
seeing 40 is normal. Stamp whatever the last response returned into
`.mwe/state.json.last_op_log_head` and pass it back as
`expected_op_log_head`; never compute what it "should" be.

**Copy verbatim, never paraphrase**, and copy a chronological `log.md` /
`CHANGELOG.md` **whole** — it is the trail maintainers read to retrace
work. Structuring and rotating it is a follow-up curation pass (see
`smart-codebase`), never something done during the copy.

## After the copy: the shape report

Now — and only now — tell the user how their documents will actually
retrieve. **Measured, never asked:**

```
wiki_admin_pull({ "wiki_id": "<id>", "shape": true })
```

It returns no page bytes, only per-page numbers plus a
`shape_summary: { pages, pages_needing_repair }`. It reads the pages from
disk rather than the index, so it answers correctly even though sectioning
is queued.

Per page you get: `sections`, `sections_sharing_a_heading`,
`oversize_blocks` (blocks too long to index as one), `oversize_chars`,
`longest_block_chars`, `needs_repair`, and a ready-made plain-language
`note`.

Report it as a sentence, not a table:

> *"45 pages copied. Three of them will retrieve badly: they hold blocks of
> text too long for the index to keep whole, so it cuts them mid-sentence.
> I can fix that if you like — in most cases a blank line between the long
> entries is all it takes."*

What the numbers mean, so you can explain rather than recite:

- The index cuts a page into **sections at its headings**. A section that
  would still be too long is packed and, if a single block exceeds the
  hard cap, cut at an arbitrary offset — mid-sentence.
- Cut pieces are **not unlabelled**: each carries its heading chain. The
  defect is *siblings sharing one label with different content*, which is
  why a query matching that heading gets an arbitrary half.
- **Density, not size.** A 55 KB page written in ordinary paragraphs is
  fine. A 18 KB page whose four paragraphs hold 60% of it is not. Never
  report page size as if it were a problem.

## Repairing a page — preview, cut, prove

Same mechanic in both moments: after an import, and in daily life when a
page has grown dense (a `warnings[]` line on your push response).

**Offer the cheap repair first.** The commonest cause is a long list or a
changelog written without blank lines between entries: one blank line
between them moves every cut onto a clean boundary and costs nothing. Try
that before proposing surgery.

When a page genuinely needs splitting:

1. **Preview the *list*, not the text.** Propose page names, one line each
   on what goes in it, and its size — so the user can rename, recombine,
   or refuse. Show the full text of one page only if they ask. Never dump
   55 KB into the conversation.
2. **Splitting is cutting, never rewriting.** Every byte of the original
   lands in exactly one new page. No summarising, no "while I'm here"
   improvements, no reflowing.
3. **Prove it.** Re-concatenate the pieces in order, hash, compare with
   the hash of the original, and *tell the user the check passed*. That
   proof is what lets someone approve a split without reading the page.
4. **The original keeps its name** for the half that stays live; closed
   material moves to dated siblings (`conversations.2026-Q2.md`), the
   idiom `smart-consumer` already uses for rotation.
5. **One atomic push**: the new pages and the trimmed original in a single
   `wiki_admin_push mode=upsert`, so no intermediate state exists where
   content is duplicated or missing.

## Pre-existing `CLAUDE.md` / `AGENTS.md` documentation rules

Many repos carry a `CLAUDE.md` (or `AGENTS.md`) with rules about *how to
write documentation* — heading conventions, a required per-module
structure, a house style. Scan for them **before** generating anything,
and resolve them with the user: silently *following* a repo's bespoke doc
rules and silently *ignoring* them are both wrong, because each surprises
someone.

What counts as such a rule: a heading like `## Documentation rules`,
`## Wiki style`, `## Docs`; or imperative prose conventions ("every module
must have a doc page", "one ADR per decision", a mandated frontmatter
shape).

Show the exact lines and offer **two** options — there is no third:

- **(a) Adopt the mwe conventions.** The repo's doc rules stay on disk,
  untouched, simply not applied to the wiki.
- **(b) Stop**, so the user edits `CLAUDE.md` themselves and you re-run
  the bootstrap. You never edit `CLAUDE.md` for them.

Record the choice so a later session does not re-ask:

```json
{
  "bootstrap_decisions": {
    "claude_md_doc_rules": {
      "choice": "adopt_mwe",
      "scanned_headings": ["## Documentation rules"],
      "at": "2026-07-27T10:00:00Z"
    }
  }
}
```

`choice` is `"adopt_mwe"` or `"user_will_edit"` (pending the re-run).

## Finishing: `.mwe/state.json`

Onboarding ends when the state file exists and the ordinary loop can take
over:

```json
{
  "wiki_id": "franz-lnprint",
  "project_id": "15c00e903646c17e",
  "wiki_type": "project",
  "local_wiki_root": "wiki/",
  "last_op_log_head": 184,
  "checksums": { "index.md": "<sha256>" }
}
```

Then say what changed, in one sentence, and go back to whatever the user
was actually doing. From here on `smart-consumer` (and `smart-codebase`
for a software project) is the contract; this skill is not needed again
until another project's first connect.

## Anti-patterns

- ❌ **Opening the intro in the middle of a task.** The single fastest way
  to make memory feel like an interruption.
- ❌ **Re-proposing after a decline.** Record it; respect it.
- ❌ **Rewriting the user's documents while importing them.** Copy, then
  report, then propose.
- ❌ **Generating a wiki from the code** because the repo has no docs. A
  confident summary nobody re-reads is the worst thing you can put in a
  memory.
- ❌ **Reading the pages into your context for a bulk copy.** Use a
  script; the bytes never need to pass through you.
- ❌ **Creating a second wiki for a project that already has one.**
  `first_connect.wiki_id` exists precisely to prevent this.
- ❌ **Asking the user about shape, structure, or sizes.** Measure it and
  tell them.
- ❌ **Splitting a page by rewriting it.** Cut, and prove the cut with a
  hash.

## Cross-references

- [`core`](core.md) — the `project_id` recipe and the
  `first_connect` datum that sends you here.
- [`smart-consumer`](smart-consumer.md) — the day-to-day loop that takes
  over once this is done, including rotation of long pages.
- [`smart-codebase`](smart-codebase.md) — the folder conventions and
  page shapes for a software project.
