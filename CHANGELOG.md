# Changelog

All notable changes to **mwe-mcp** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

From 1.0, the public interface (the MCP tool surface, by family — see
[`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md)) is a stable,
semver-governed surface — breaking changes are called out explicitly.

## 1.7.0 — 2026-07-29

### Added

- **An agent's own wiki now says so, and the whole pipeline reads it.**
  `is_agent` existed on `FamilyScope` but only the dedup revisor looked at
  it. It is now flattened to a `wiki_id → bool` map that completion,
  contradiction and page-merge all consult, and it reaches the
  classifier's `known_users` roster as well: one entry can be marked as
  the assistant itself, with the rule that a human name never resolves
  onto it and a name *addressed* to the agent is vocative, not
  attribution. `smart_bootstrap.is_self` no longer guesses from the slug —
  it requires the marker (or the legacy label) and repairs it on the
  caller's own operating wiki — and `wiki_admin_push` reserves the `agent`
  label for a consumer's own wiki. The page-merge guard matters most:
  `slug_kinship` was proposing one person's page and another's as twins
  because they share a token, and merging them would have undone the
  per-person separation the engine works to keep.

- **The autobiographical voice follows the page's subject, not the wiki.**
  An agent's wiki is its autobiography and gets the first-person voice —
  but measured against a real deployment, 31% of the facts in one agent's
  wiki turned out to be about other people, residue from before the
  routing guard existed. Compiling those in the first person would have
  had the assistant narrate someone else's life as its own. So the tone is
  decided per page (`compiler::tone_for_page`, by majority of fact
  ownership), and misrouted residue keeps the ordinary identity voice.

### Fixed

- **A slot that passed the boot health-check could still fail every real
  call.** `temperature` is deprecated for the Claude 5 generation, which
  was missing from the list of families that reject sampling parameters —
  so with Sonnet 5 behind the navigator, every recall returned HTTP 400
  while the boot line read *all configured slots reachable*. The failure
  is silent by design (`recall_nav` keeps the partial walk and logs a
  warning), so recall degraded without anything failing. Both halves are
  fixed: the Claude 5 family is on the list, **and the probe now pins the
  temperature the hot paths use**, so an unlisted family that drops
  sampling params fails at boot instead of degrading every answer. A probe
  gentler than production is not a check.

- **The navigator retries once when the model answers with no text.** Two
  of 276 calls in a corpus rebuild came back with no text block at all —
  the budget went to a thinking block — and each silently cost that turn
  its navigation. Retried on protocol, transport and backend failures
  only; a 400 reproduces exactly and auth or rate-limit errors want the
  operator or a back-off window instead.

- **A fumbled model response no longer costs the whole night.** The dedup
  revisor was the only one of the four confirmers that propagated a
  per-pair error, and `dream::run_full` aborts the cycle on any error from
  `rem::run_cycle` — so one bad response skipped promotion, reorg and
  every queued recompile, with the next attempt 24 hours away. The pair is
  now skipped as a soft error (no negative verdict memoised, so it stays a
  candidate) and only five *consecutive* failures stop the cycle, which is
  an outage rather than a fumble.

- **Facts about the assistant, said by a person, reach the assistant's
  wiki.** The `self` sentinel only fires on the assistant's own turn, so
  on a user turn ("you are good with paperwork") the fact arrived at the
  routing guard owned by the agent and was dropped. Also: with two bots, a
  fact about B aimed at A was binned instead of redirected home; and the
  marker check walked the entire tree on every request when the wiki did
  not exist.

### Changed

- **The ingest classifier declares its system prompt cacheable.** It is
  the engine's heaviest repeated caller by a wide margin — 4.84M prompt
  tokens against 56k of output over a 174-call corpus rebuild — and its
  system half is the bundled prompt with only the locale substituted,
  while everything per-turn rides in the user half. That split was already
  right; the flag was simply never set, so every call paid full price for
  a prefix it had already sent. Measured on a live run afterwards: 27,077
  of 28,174 prompt tokens served from cache on the second consecutive
  turn. The one-hour window means the discount lands on bursts — a
  conversation, a replay, a REM night — while an isolated turn arriving
  after it expires pays the write surcharge instead.

- **A smart consumer's operating wiki is the agent's to maintain.** REM
  skips every write job on a smart wiki by design, so nothing tidies it
  but its owner. The `smart-consumer` skill (1.15.0 → 1.16.0) now says so
  and describes the maintenance: merge repetitions (never two twins about
  different people or occasions), retire what stopped being true, re-cut
  pages when the content moved, throw away scaffolding — bounded to the
  pages the session already touched, on the push it was about to make
  anyway.

### Operational notes

- A restart resets the REM timer (`initial_delay_secs: 300`), so every
  deploy triggers a full cycle five minutes later. Worth knowing when
  restarting with dirty pages queued.

## 1.6.0 — 2026-07-28

### Added

- **Memory is now written in the language its owner declared — on every
  slot, not just the two that asked.** `enrollment_users.locale` has
  driven the LANGUAGE directive since migration 0020, but nothing ever
  wrote it: the welcome wizard turned its `language` answer into a prose
  clause and left the column NULL, and the users page had no field at
  all, so an admin who wanted a household's memory in its own language
  had to reach for `sqlite3`. The column is now a real field on both
  dashboard forms and a required wizard answer, pre-filled from
  `Accept-Language` and written *before* the primer is ingested, so the
  primer's own facts come out in the language just declared. And the
  directive reaches the slots that actually compose prose: eleven
  prompts put natural language in front of a person and only two were
  told which one to use — the rest answered in the language of their own
  few-shot examples, which are Italian, so an English deployment got
  Italian page titles and Italian document summaries over English facts.
  The Cronista, both hub writers, comment-apply, the three
  document-ingest phases and the REM cartographers now all carry it. A
  wiki's language is its scope principal's: a user wiki speaks its
  owner's declared language, a group wiki speaks the one every member
  declared and has none when they disagree. Undeclared resolves to
  "mirror the user's message" on the two slots that answer a live turn,
  and to English on the compiling slots, which never see a turn to
  mirror. `prompts::PROSE_REGISTRY` classifies every bundled prompt as
  prose or internal and **fails the build** on one it does not name, so
  a new slot cannot be merged without answering the language question.
- **Three REM slots regrouped so a language directive can mean
  something.** The cartographer, the conciliator and the date normaliser
  each batched the whole forest into one call, where a statement about
  one language says nothing. Each now groups by source wiki before
  chunking. The batch is what narrows — the *context* each model is
  shown is unchanged, and the normaliser still spends the same per-cycle
  cap — with the side benefit that one wiki's transport failure no
  longer sinks the whole sweep.
- **The prompt cache is measurable.** `CompletionUsage` gains
  `cached_prompt_tokens`, folding Gemini's `cachedContentTokenCount` and
  Anthropic's `cache_read_input_tokens` into one field whose meaning is
  written down (both are *inclusive* of the prompt count beside them),
  with a per-call hit-ratio log line and the value recorded on the
  training spool. The finding it produced: the cached span is
  **block-quantised** at ~4 090 tokens, so a shorter prompt can cost
  *more* — trimming 6.4% of the `ingest` prompt lowered the bill by
  1.1%, while deleting a section from its middle cut 13.4% of the tokens
  and raised the bill 26.1%, by dropping the cached prefix from six
  blocks to four.
- **A replay differential for prompt changes**
  (`examples/ingest_replay.rs`): replays recorded production requests
  against a modified system prompt and diffs the resulting plans field
  by field, with resume, bounded retry and a compare mode. Its first
  result is about the classifier rather than any prompt — replaying the
  same turns twice against the *unmodified* prompt agrees on 51.2% of
  fields and 45.7% of capturing turns, because Gemini 3 mandates
  `temperature: 1.0`. Every prompt A/B has to be read against that noise
  floor.

### Fixed

- **An agent's diary stops scattering into its users' pages.** Part 12's
  `owner_id: "self"` routes a fact the agent states about *itself* into
  its own wiki, and the engine matched that sentinel as a literal
  string. A model that writes its own principal instead
  (`user:<agent>`) is making the identical claim, but fell through to
  the normal path and filed the diary entry in whatever wiki
  `target_wiki_id` named — 40 agent-owned facts sitting in their users'
  wikis on the reference deployment. Both spellings now route to the
  self path. The alias cannot misfire: the agent principal resolves only
  on a turn that agent authored, so on a user turn an owner naming the
  agent keeps its ordinary meaning.

## 1.5.6 — 2026-07-27

### Fixed

- **A section *titled* with the query now outranks one that merely cites
  it.** 1.5.4 claimed identifiers rank their defining section first; the
  live check found otherwise, and the claim is corrected here in both
  senses. What was true: the lexical pass ranks the definition first
  (7 of 7 on the reference store). What was not: the *fusion* then put a
  citing section back on top, because a section quoting `D-006` is in
  **both** lists — leading on cosine and two places behind lexically,
  which reciprocal rank fusion cannot recover from at any `RRF_K` or
  lexical weight (both are monotone in a rank gap of two). The fix is a
  **tier, not a knob**: a second, sharper index question — which sections
  carry *every* query term in their heading — and those outrank every
  citation. It uses `AND` where the ranking pass uses `OR`, so a prose
  query matches no heading and the tier goes quiet; verified on the live
  corpus, where the identifier query promotes exactly the two defining
  sections and a five-word prose query promotes none.

## 1.5.5 — 2026-07-27

### Added

- **First connect is its own skill, and the server says when it
  applies.** `smart_bootstrap` accepts the exact `project_id` a consumer
  derives from its working directory and answers a `first_connect` block:
  the wiki to resume, or one line pointing at the new bundled
  `smart-onboarding` skill when this project has no memory yet. The
  procedure — the intro, the faithful import of existing documents, the
  post-import report, the page repair — moved out of the three places
  that carried it and is fetched only by the sessions that need it; the
  everyday skills shed 452 lines. The trigger had to move to the server
  for the split to be safe: a procedure behind an extra fetch, gated on
  an agent remembering to fetch it, is easier to skip than one already
  open.
- **Page shape is measured and reported.** `wiki_admin_push` returns a
  plain-language `warnings[]` line for each written page whose blocks are
  too long for the index to keep whole (they are cut mid-sentence, and
  several sections end up under one heading with different content), and
  `wiki_admin_pull` accepts `shape: true` to report a whole wiki —
  sections, over-cap blocks, the share of the page they hold, a per-page
  note and a summary — without returning any content. Both are derived
  from the bytes by the same segmentation the indexer runs, so they are
  correct while section indexing is still queued. The trigger is density,
  not size: three over-cap blocks, or a quarter of the page.
- **`wiki_admin_pull` accepts `paths`** to narrow a pull to named pages —
  the narrowing the smart-consumer skill had documented since the MVP.

### Changed

- **`signpost_hint` no longer fires on a consumer's own operational
  wiki** (`wiki_type: agent`). Signposts exist so a conversational turn
  can discover *projects*; nudging an agent to signpost its private
  working memory only added noise to the owner's `projects.md`.
- **Create-mode errors say what to pass.** A parent-less smart-wiki
  create now names the caller's own root wiki id in the message instead
  of stating only that top-level is not allowed; the `title` and
  `wiki_type` errors say what those fields are for.

### Fixed

- **The skill told agents to notify their own wiki, which the server
  refuses.** Writing your own `_briefing.md` is an ordinary push;
  `wiki_admin_notify` is how *others* reach it. The documented
  "note to next session" flow had been impossible as written.
- **Two documented behaviours that did not exist**: the folder-structure
  deviation warnings described in three documents (no validator was ever
  written — `warnings[]` now carries page shape instead) and the `paths`
  argument above.

## 1.5.4 — 2026-07-27

### Added

- **Exact-term matching fused into the section ranking** (migration
  `0065`). Recall was pure vector, so a query that *is* an identifier — a
  decision code, an ADR number, a ticket id, a file path — ranked worst
  exactly where a project's decision log lives. `wiki_sections_fts`
  indexes `heading_path` as its own 4×-weighted column beside the text
  and the three section entry points fuse the two rankings by position
  (RRF). Both passes always run: a per-query gate would spend a model
  call to guard a sub-millisecond index lookup, and a wrong "no" drops
  the hit with nothing to notice. The fusion reorders only — a hit's
  `score` keeps its cosine meaning — and the signpost floor still runs
  before it, so an `OR` match on an ordinary sentence can never start a
  dig into project documentation. Measured on the reference store (4 220
  sections): 7 of 7 decision identifiers now rank their defining section
  first, prose queries unmoved.
- **REM regroups pages into sub-wikis.** The nightly promotions slot
  reads a wiki's whole page inventory once per cycle and cuts the groups
  of pages that are *already* one subject area: a group founds a new
  sub-wiki (floor `auto_promote_group_min_pages`, default 9) or moves
  into one that already exists (no floor). The trigger is evidence on
  disk rather than a forecast about one page's mass.

### Changed

- **Deleting a wiki defaults to Dissolve**: the structure goes, every
  fact stays. Facts are evacuated to a live home and their pages parked
  on the compilation plan as `reopen_pages`, so the next narrative build
  re-decides where each fact belongs corpus-wide instead of letting it
  inherit the page it happened to sit on.

### Removed

- **Page → sub-wiki emergence**, its prompt, and the
  `auto_promote_subwiki_min_page_facts` knob — superseded by the
  page-group pass above. A wiki is born holding every page of its
  subject, so it can never be born with a single page.

## 1.5.3 — 2026-07-26

### Fixed

- **The section cap was bypassed by a heading whose body starts on the
  next line.** The prose segmenter splits on blank lines, so a heading
  followed immediately by its text — a changelog entry, a table, a dense
  list — is a *single* paragraph; the heading branch pushed those
  trailing lines into the packing buffer without ever applying
  `segment_max_chars`. Only the plain-paragraph branch enforced it. Both
  bodies now go through the same packing helper, so the cap holds for
  every shape. Observed on the reference store immediately after the
  1.5.2 deploy: two pages of that shape kept sections of 6 994 and 5 239
  characters through a full re-cut. Document ingest shares the segmenter
  and gains the same guarantee, which its own knob already promised.

## 1.5.2 — 2026-07-26

### Added

- **Smart-wiki documentation moved out of the fact store** (migration
  `0062`, plus `0063`). A project wiki's pages are chunked into
  `wiki_sections`, with read access held once per wiki in `smart_wikis`,
  instead of one governed `fact_index` row per chunk. On the reference
  store that was 72% of the fact table and **75.6% of the characters** a
  conversational turn spent on recall; a turn now recalls facts only, and
  a sharing change is a one-row write instead of one row per section. The
  data move is an idempotent boot pass that copies embeddings verbatim —
  no re-embedding, no migration downtime.
- **Project awareness: the everyday agent learns that a project exists.**
  A smart consumer writes short **signposts** into its owner's own wiki
  through the new `wiki_admin_signpost` (H family) — one plain-language
  description per project, plus one activity line per day over a rolling
  5-day window, on a reserved `projects.md`. Length caps are enforced
  server-side and an over-long field is refused with its measured length,
  never truncated; rewriting an unchanged signpost is a no-op, and
  `wiki_admin_push` answers with a `signpost_hint` when something is
  missing. When a signpost surfaces in a turn, that project's
  documentation can be opened for that turn — so a question that never
  names the project can still reach it.
- **The dig is a judgement, not a threshold** (ingest prompt v2.45). The
  classifier now answers, in the JSON it already returns, whether a
  signposted project's documentation would help answer this turn — at no
  extra model call. Built this way because the threshold version was
  measured first and no similarity signal separated "a customer says the
  content is frozen" (needs the docs) from "I have an appointment at that
  customer" (does not).
- **Dashboard: a Sections tab** in the memory browser, the smart half of
  the corpus, mirroring the Standard/Smart split of the wiki explorer.

### Changed

- **The nightly cycle stops re-buying the verdict it already has**
  (migration `0064`). Every REM confirmer asks about a stable piece of
  the corpus and mostly hears "no", and nothing recorded that "no": a
  single cycle spent its whole confirm budget re-judging the same pairs,
  so the backlog past the cap had never been examined once. Negative
  verdicts are now memoized for all seven confirmers, keyed by a hash
  over the slot's model id plus the rendered prompt — an edited fact, an
  edited prompt or a repointed model all invalidate themselves, with no
  hand-bumped cache constant. Positives are never stored. The same budget
  now drains the backlog instead of circling it.
- **The compiler stops re-buying its own standing brief** (`cronista`
  v1.14). Input, not output, was ~70% of a page's compile cost, and 97%
  of that input was the same brief and page index re-sent per page. The
  prompt now ships in two halves — the shared block as a cacheable system
  prompt, the page's own facts on the user turn — with a 1-hour cache
  window, because a compile run outlives the 5-minute default. A rejected
  request (invalid / auth) no longer buys a retry.
- **`rem-promotions` v2.1** presents facts as positional handles instead
  of UUIDs (~18 tokens of noise per fact on the most expensive slot);
  raw ids still resolve.
- **Sections are cut for retrieval, not for extraction.** Smart-wiki
  pages are chunked at 1 200/2 000 characters instead of the
  document-ingest 3 000/4 500 they used to borrow. A section is ranked by
  one embedding and quoted whole into a bounded recall slot, and at
  ingest sizes one oversized hit consumed the entire slot on its own —
  25% of sections were larger than the slot, the largest 6 994
  characters. The re-cut needs no migration: the next reindex sweep
  re-chunks any page whose stored cut no longer matches.

### Fixed

- **A documentation paragraph can no longer be filed as a fact about the
  user.** The rule forbidding it had been placed inside the prompt
  section that applies only to assistant-authored turns, whose opening
  line tells the model to ignore the whole part otherwise — so it was
  inactive on exactly the turns that carry documentation. It is now a
  turn-level rule.

## 1.5.0 — 2026-07-23

### Added

- **The reverse channel now tells the affected human.** When a turn (or a
  document upload) files a fact owned by an enrolled user who was *not* the
  human of that conversation, the engine emits a new `fact_minted_for_you`
  event carrying the fact bodies themselves — batched one notice per
  beneficiary — so a consumer can deliver the content to its subject instead
  of leaving them to stumble on it at their next recall. This is the server
  half of the consumer-push contract (`INTEGRATING.md` step 8).
- **hermes bridge: the reverse-channel half, zero fork.** A `mwe-events`
  gateway hook (auto-discovered from `$HERMES_HOME/hooks/`) drains
  `fact_minted_for_you` every ~30 s and delivers each notice to its
  recipient's private chat as an agent-composed message, routed through a
  one-shot cron job on hermes's own scheduler; a `mwe-daily-digest.py` cron
  script batches every other event kind into a once-a-day recap. Built
  entirely on supported hermes seams — no upstream patch.

### Fixed

- **A fact can no longer be owned by a principal that does not exist.** Both
  ingest paths (conversational and document) now check the classifier-emitted
  owner against enrollment before filing: an owner that resolves to no
  registered user/group is re-owned to the sender (or the uploader), closing
  the gap that let a fabricated subject take ownership of a memory. An
  enrolled third party — the legitimate subject of a fact about someone
  else — passes untouched.
- **Owner attribution on assistant turns follows the subject, not the
  speaker (ingest prompt v2.43).** Advice the agent synthesises *for* an
  enrolled user is owned by that user, decided deliberately via a necessity
  test rather than guessed from a mention; and the fact body narrates the
  advice passing through the sender instead of asserting an interaction with
  the absent subject (the wording that made a relayed plan read as a false
  memory).

## 1.4.8 — 2026-07-20

### Fixed

- **The agent's own memory now counts as "used" when it is used.** The
  agent's self-memory (identity + history with the current user) is injected
  into every turn, but the injection path never updated the recall counters —
  every self-fact read as never-recalled forever, hiding real usage from
  metrics and from recall-weighted REM decisions. The agent-self recall path
  now bumps `last_recall_at` / `recall_count_30d` for each fact it surfaces,
  exactly like the standard recall path.

## 1.4.7 — 2026-07-20

### Fixed

- **Nightly memory reorganization (REM) was silently inert for every
  standard wiki.** The auto-promote pass that splits an over-long page into
  sections or a dedicated sub-wiki skipped every candidate
  (`candidates_examined: 0`), so structure never emerged organically. Its
  skip-gate matched a `wiki_promote` proposal by kind plus a blind substring
  of the fact id — but that kind is overloaded: routine fact-lifecycle
  operations (validity close, refile, ACL change) share it, so any page
  holding one ever-touched fact was permanently marked "already promoted."
  The gate now counts only genuine page-promotion receipts (paragraph→page,
  page→sub-wiki), scoped to the receipt's own source page, so a fact that
  later migrated onto another page no longer freezes it.

- **The agent could "rename itself" from a mis-heard command.** Ingest read
  the agent's name used as a form of address inside an unrelated command
  ("Gandalf, turn it down" — mis-transcribed) as an explicit rename. A naming
  rule now changes the agent's name only on an explicit naming predicate
  ("your name is X"); a vocative address never renames (ingest prompt v2.40),
  with no edit-distance heuristic.

- **A user's or group's fact could be filed into an agent's own wiki**,
  fragmenting that principal's memory across two wikis. The capture planner
  now redirects a non-`self` fact aimed at an agent wiki to the owner's own
  wiki (or drops it rather than misfiling), via a new per-wiki `is_agent`
  signal. Owner (audience) and physical wiki stay decoupled otherwise.

### Changed

- **The agent's self-memory (its "diary") is now organized per served user.**
  Relationship self-facts are written to a per-user page
  (`esperienze_<user>.md`) and identity self-facts to the agent's index,
  instead of accumulating on one heterogeneous catch-all page.

## 1.4.6 — 2026-07-20

### Added

- **Per-user timezone — two users, two places, both right.** One
  deployment-wide `recall.ingest_timezone` is wrong the moment two
  users live in different zones (London and Sydney hear "tomorrow
  at 9" eleven hours apart), so the reference-time zone now resolves
  **per sender**: `enrollment_users.timezone` (new migration 0061) wins
  over the deployment default, which stays as the fallback; unset both,
  spoken times read as UTC as before. The admin sets it on the users
  page (create + edit); the welcome wizard's existing timezone question
  now also lands in the column (it previously became only a memory
  fact — the stamping plumbing reads the column, never the memory).
  A per-turn zone from the consumer (device time, covers travel) is a
  tracked protocol extension.

## 1.4.5 — 2026-07-20

### Added

- **Server-settings sections on the Settings page — no more YAML-only
  config.** Every typed config section without a dashboard surface is
  now an admin-only section of `/dashboard/settings/me`, closing the
  gap survey: **Ingest timezone** (`recall.ingest_timezone`, hot —
  swapped into the shared recall handle so the next ingest turn stamps
  wall-clock times in the deployment's zone), **Dream cadence**
  (`rem.schedule:` — mode, full/light intervals and initial delays,
  the light-dream backlog trigger), **Logging** (`logging:` — level,
  file rotation, file path), and **Document pipeline** (`document:` —
  segmenting, extraction caps, worker cadence, merge threshold). Same
  atomic `.bak`-guarded `load_raw` round-trip as the other editors;
  the boot-read sections say so and point at the Backup console's
  Restart button. The REM-settings and recall-settings panels now
  cross-link instead of declaring those keys YAML-only.
- **Recovery surfaces — automatic snapshots, dashboard restore, safe
  memory reset (roadmap 4d).** A new `backup:` config section (on by
  default: daily, retention 7) drives an automatic-snapshot scheduler:
  a due-check loop that hot-snapshots the whole workdir into a
  snapshots home (default: the `<workdir-name>-snapshots` sibling),
  prunes the oldest `auto-*` snapshots beyond the retention, persists
  its last-run stamp in `engine_meta` (a restart never re-fires inside
  the interval), and reports its outcome to the dashboard. The Backup
  console grows into the full recovery surface: settings editor
  (hot-swapped, `.bak`-guarded), the snapshots-on-disk listing with
  provenance badges, per-snapshot **Restore…** / **Delete**, and a
  type-`RESET`-to-confirm **Memory reset**. Restore and reset are
  **staged**: a one-shot `recovery-pending.json` marker (excluded from
  snapshots) that the next boot applies under the lockfile — automatic
  safety snapshot first, refusal-leaves-untouched, outcome persisted
  for the console — because a live server cannot safely replace its
  own open workdir. Reset wipes the memory tables and the
  `wikis/`/`media/`/`training-spool/` trees while preserving accounts,
  enrollment, consumers, tokens, 2FA/OAuth state, custom skills,
  config, env and prompt overrides; identity wikis are re-scaffolded
  empty and `profile_initialized` is cleared so the welcome wizard
  re-seeds each profile. A "Restart now" button applies a pending
  recovery from the dashboard: graceful shutdown, then exit code 75
  (`EX_TEMPFAIL`) so a `Restart=on-failure` systemd unit relaunches.
- **Verbatim source promotion — pasted text becomes a cited document
  (roadmap 46).** The server now backstops the media-first routing:
  document-shaped inline text is promoted to the media rail — the text
  is materialised verbatim as a content-addressed blob +
  `media_catalog` row (kind `doc`, `text/plain`) and the document
  pipeline runs against it, so extracted facts cite `source_ref =
  catalog_id` and the dashboard serves the preserved original, exactly
  like an uploaded file. Two doors, one deterministic shape heuristic
  (email headers, forwarded banners, quote/markup density,
  greeting/sign-off, size): `wiki_ingest_external source.type=inline`
  (response: `promoted_catalog_id`; `dry_run` reports `would_promote`)
  and an oversized `wiki_ingest_message` turn — the paste-into-chat
  case — which is archived + enqueued as a document job while the
  conversational ingest sees a bounded excerpt plus the promoted
  document as a linked attachment (response: `document_promoted`).
  A new `promote: always | never` dial on both tools forces the
  decision either way, disposition-style; guests, `dashboard_command`
  and assistant-authored turns never promote.
- **Behaviour-rule scopes — the user's rule for every assistant
  (roadmap 42).** The `behaviour_scope` axis grows a third value,
  `user-global`: a directive the user explicitly addresses to every
  assistant they talk to ("voglio che TUTTI gli assistenti mi parlino
  in italiano") now files in the **sender's own identity-wiki
  `rules.md`** (owner = the sender, no admin gate — it binds only
  their conversations) and the `rules` channel of every consumer
  serving that user surfaces it, the bindingless smart consumer
  included. Order pinned in `YOUR RULES`, most specific last:
  agent-wide → user-global → per-user. The classifier sees the union
  with fact ids and scope tags, so a revision supersedes across all
  three sources — but only the admin may supersede an agent-wide rule
  (a non-admin's revision files additively at its own scope). This
  retires the old salience-`high` workaround for cross-assistant
  directives, and the governance read (`sender_rules`) now strips fact
  regions from the user's `rules.md` so a user-global rule never leaks
  into the classifier's policy section.

### Fixed

- **Dashboard saves no longer persist `MWE_LLM_*` env overrides.** The
  config gains a `load_raw` round-trip primitive (file contents
  verbatim, no env overlay); every dashboard section editor now loads
  through it, so saving an unrelated panel can never bake a
  runtime-only override into the YAML. The LLM-config editor remains
  deliberately what-you-see-is-what-you-save.
- **hermes `mwe-watchdog` 0.2.0 — two verification gaps closed.** The
  watchdog now hashes the whitespace-trimmed turn text (the memory
  provider's canonical form), so a padded gateway message can no longer
  silently skip its handshake entry; and it requires the
  `<memory-context>` fence on the request's **last user message** —
  host injection is API-call-time only, so a fence on an earlier
  message is a stale injection index landing on the wrong turn and now
  counts as a miss.

## 1.4.1 — 2026-07-19

### Added

- **Training spool — teacher traces for local-slot distillation.** When
  `training_spool.enabled` is on (default off), every internal-LLM
  exchange — any slot, any backend, every transport (MCP ingest, REM
  cycle, dashboard chat) — is recorded verbatim as one JSON line (slot,
  backend, model, full request, full response, finish reason, token
  usage) into per-day files under `<workdir>/training-spool/`. The
  strong API slots act as teachers; their traces become the dataset for
  fine-tuning the local workhorse on mwe-mcp's own structured tasks.
  Recording is a decorator inside `build_backend` (no call-site
  changes), best-effort (an I/O failure never fails the turn); health
  probes and failed calls are excluded, images ride as MIME-only. New
  admin dashboard panel `/admin/training-spool` ("Spool" in the nav):
  checkbox with the atomic-YAML + `.bak` save idiom, hot-flip of the
  running recorder (no restart), on-disk inventory, and the privacy
  stance (the spool embeds recalled memory content — it stays on the
  host; scrub before sharing a dataset). See
  [`llm-functions.md` §6](docs/design-notes/llm-functions.md).
- **hermes bridge: `mwe-watchdog` verification plugin (trio → quartet).**
  Out-of-tree hermes plugin that verifies the mwe recall block actually
  reaches the model each turn — born from a silent memory-blackout
  incident where the host's stale injection index dropped the
  `<memory-context>` block after transcript repair. See
  [`agents-bridges.md`](docs/development/agents-bridges.md).

## 1.4.0 — 2026-07-15

### Added

- **Fresh-session resume: a blank-context requester is served its own
  surface.** A `wiki_ingest_message` turn that carries no `recent_messages`
  has no local context a served thread could duplicate — a reborn/blank
  session (e.g. a hermes gateway session silently reset by idle-expiry,
  upstream hermes-agent#43008) or a consumer that keeps no window at all.
  Such a turn now receives the cross-consumer recent window **including its
  own surface**: the thread the user is continuing, minus the message being
  spoken (the window fetch runs before the turn's own buffer write). Turns
  that bring their window keep the self-echo exclusion unchanged. A consumer
  on this contract never wakes up amnesiac — session resume with no
  host-side support.

## 1.3.0 — 2026-07-15

### Added

- **Cross-consumer recent window — the thread of discourse follows the user.**
  The server now retains a bounded serving buffer of the exchanges the
  per-turn ingest already receives (per user, hard cap `recent_window_entries`
  = 32 AND TTL `recent_window_ttl_hours` = 4, enforced in the write path;
  never indexed, never embedded, never REM-processed; deleted with the user)
  and every `wiki_ingest_message` response serves it back as the
  self-labelled `recent_window` field: the user's live thread from their
  OTHER surfaces, entries tagged with relative age and origin
  (`[2 min ago · via <consumer>/<channel>] user: …`), oldest first, newest
  winning the `recent_window_chars` (1200) budget, headed by an explicit
  do-not-re-answer framing. Consumers declare their surface with the new
  optional `metadata.channel` label; self-echo is excluded by
  (consumer, channel) — whole consumer when no label is sent. Windows never
  cross users. This restates the no-transcript invariant as *no unbounded
  transcript*: the buffer serves the live thread (minutes-to-hours), while
  long-range continuity stays with recalled facts.
- **hermes bridge, memory plugin 0.2.0** — sends the gateway key as
  `metadata.channel` and injects `recent_window` verbatim between the rules
  and the recalled facts.

### Fixed

- **Dashboard Facts pager: real ACL-projected totals and an editable page
  box.** Prev/next now derive from the real filtered total instead of a
  page-size heuristic, the page number is directly editable (jumps preserve
  the active filters), the disabled state reads "of M", and totals beyond the
  scan ceiling render as an "M+" estimate.
- **hermes bridge: `compression.in_place: true` is withdrawn — rotation mode
  (the vanilla default) is required.** hermes-agent's in-place compaction
  path re-appends the whole compacted window into the same active transcript
  after a preflight cut (its flush bookkeeping resets and the turn's history
  reference is nulled), doubling the conversation; the model then re-answers
  the replayed tail — observed live as a Telegram bot answering yesterday's
  messages. The bridge no longer recommends in-place anywhere; with rotation
  the same re-append lands in the freshly rotated session, where it is
  correct behaviour.
- **mwe-truncate 0.3.0: oversized tool results are snipped on a cut**
  (`snip_tool_chars`, default 4000). The window bounds *turns*, not *weight* —
  browser-tool spam kept the bounded window permanently above the compression
  trigger (fire-abort on every call, one session crash-looping at ~328k
  tokens). Snipping is copy-on-write (the rotated-out archive keeps full
  contents) and never touches the tail from the last user message onward; a
  snip-only pass must save ≥8% or it reports a no-op through the abort
  protocol.

## 1.2.0 — 2026-07-14

### Added

- **Deployment timezone for the ingest classifier** — `recall.ingest_timezone`
  (or the `MWE_INGEST_TIMEZONE` env var) names the users' IANA timezone (e.g.
  `Europe/Rome`). When set, a bare wall-clock time a user speaks ("alle 16") is
  read in that zone and converted to UTC for a fact's validity interval,
  instead of being stamped verbatim as UTC — which drifted every dated
  commitment by the local offset, so deadlines expired late and stale plans
  resurfaced as if still current. Unset keeps the prior UTC-only anchor. The
  DST-aware conversion is delegated to the classifier; no timezone database is
  compiled in.

### Changed

- **Relationships between people now reach the always-on identity core.** A
  statement of who someone is to someone else ("X is Y's partner / parent /
  child") is classified as identity core (`bio` / `high`), extracted
  reciprocally when both are enrolled, and shielded from dedup and
  contradiction retirement — so an agent stops confusing who is who across a
  family or a team.

### Fixed

- **hermes bridge: the per-turn recall block no longer injects
  `suggested_seed`.** A consumer that brings its own model was handed a
  pre-drafted reply inside the user turn, which a weaker model could adopt or
  continue — laundering the ingest classifier's guesses into the agent's
  replies. The bridge now surfaces only the recalled facts.

## 1.1.2 — 2026-07-14

### Fixed

- **An orphaned identity wiki can now be deleted (admin).** Deleting a
  user keeps their wikis (the sender-scrub invariant), but the
  identity-wiki guard refused deletion unconditionally ("remove the
  user/group instead") — a dead end once the user was already gone. The
  refusal is now scoped to *living* principals: an identity wiki whose
  user/group is no longer enrolled is deletable from the dashboard like
  any other wiki, with the same typed-id confirmation and move/tombstone
  dispositions. (#4)

## 1.1.1 — 2026-07-14

### Fixed

- **The binary self-reports its release version again** — the v1.1.0
  artifacts printed `1.0.0` because the workspace version was not bumped
  at release time.
- **Deleting an enrolled user no longer 500s when the identity is bound
  to a consumer.** `consumers.system_user_id` is a plain FK with no `ON
  DELETE` action, so the dashboard's bare `DELETE FROM enrollment_users`
  was rejected by SQLite for any identity registered as a consumer's
  system user. Deletion now goes through `enrollment::remove_user`, one
  transaction that dismantles everything hanging off the identity:
  consumers system-bound to it (registration row, delegation grant,
  web-agent OAuth rows), the user's own OAuth codes/refresh rows (a live
  refresh row would keep minting tokens for a vanished sender), then the
  enrollment row (CASCADE clears credentials, invitations, 2FA, votes).
  The delegation cache is refreshed post-commit so act-as dies on the
  next call. The deleted user's *memory* outlives the identity: their
  wikis stay, and facts they authored are re-pointed at the containing
  wiki's scope principal (the sender-scrub invariant).
- **hermes bridge — the `mwe-truncate` context engine now actually bounds
  the conversation window** (plugin 0.2.0). Its only trigger was hermes's
  `threshold_percent` (0.75 of the model context — a summarization
  default), so on a million-token model the first cut sat at ~786k prompt
  tokens: far beyond per-minute provider token quotas, which a long-lived
  session exhausted first. The window is now counted in recent **user
  turns** (`protect_last_users`, default 5 — cut at a user-message
  boundary, so tool-call pairing holds by construction) with a slack
  (`slack_users`, default 3) that keeps the prompt prefix cache-stable
  between cuts, and the trigger is capped in absolute tokens
  (`threshold_tokens_cap`, default 30k). A no-op fire reports through the
  host's abort protocol instead of rotating the session; pair with
  hermes's `compression.in_place: true` (see the bridge README). The
  `protect_last_n` config key is retired (logged and ignored).

## 1.1.0 — 2026-07-06

*(Section reconstructed after the fact — 1.1.0 shipped without a
changelog entry; the content below is from the release commit.)*

### Added

- **Proactive smart-consumer wiki onboarding** — onboarding is offered
  at connect.
- **Bulk-copy bootstrap** — bulk-copy moves bytes without going through
  the LLM.
- **Bounded log pages** — append-only log pages get a rotate-by-period
  discipline.

## 1.0.0 — 2026-07-06

First public release.

### Changed

- **License: AGPL-3.0-or-later** (was `MIT OR Apache-2.0`), with a
  commercial dual-license available — see [LICENSING.md](LICENSING.md).
  SPDX headers on all first-party sources; contributions now require a
  DCO sign-off plus a relicensing grant ([CONTRIBUTING.md](CONTRIBUTING.md)).
- **Public repository with a fresh history.** The engineering wiki stays
  in the maintainer's private archive; the user/integrator documentation
  ships in [`docs/`](docs/).
- The MCP tool families are declared **stable under semver** from this
  release.

### Added

- **Bridge distribution from the running server** (roadmap 3i). A running
  mwe-mcp is now the distribution point for its own bridges — no repo
  clone, no manual symlinks. A public, anonymous root surface serves a
  slim **front page** (`GET /`: an agent line → the catalog, a human
  sign-in link), the **bridge catalog** (`GET /bridges`,
  `GET /bridges/<consumer>` — each entry with an *agent instructions*
  link to its `install.md`; the install command tailored to the request
  `Host`), and a **self-contained installer**
  (`GET /bridges/<consumer>/install.{sh,ps1,md}`): the bridge's plugin
  files are embedded in the binary and inlined into the script, so one
  `curl … | sh` from inside the hermes checkout lays everything down. The
  **same** catalog + guide are also a dashboard **Bridges** tab
  (`/dashboard/bridges`), and the dashboard home gained a *Connect a
  consumer* card (MCP URL + issue-token + wire-a-consumer). The **token**
  is issued from that card — never from the bridge pages or the
  installer, which only instruct the operator to set it, disable the
  host's built-in memory, and restart. The standalone admin-only
  `/connect` page was **retired** (its onboarding role moved to the home
  + Bridges tab; the `/connect/hooks/*` bundle endpoints remain).

- **The media pipeline** (roadmap group 12). Photos, video, audio and
  documents become memory without betraying the pillars: a media item
  enters as an ordinary described fact whose body carries a bare
  `{{embed=<catalog_id>}}` key, while everything behind the key — kind,
  MIME, size and the **per-media ACL** — is authoritative in the new
  `media_catalog` table (migration 0039), the twin of `fact_index`;
  bytes live once in a global content-addressed store under
  `<workdir>/media/` (sha256, blob-before-row write order). Entry is
  two-phase: `POST /media` (multipart, the same bearer JWT +
  `X-MWE-Act-As` as `/mcp`, idempotent per-owner dedup) mints the
  `c-YYYY-MM-DD-<kind>-NNN.<ext>` id with the closed English kind
  vocabulary, then the new optional `attachments` array on
  `wiki_ingest_message` links it to the turn. Undescribed photos ride
  the existing ingest LLM call as inline image parts (Gemini
  `inlineData`, Anthropic `image` blocks, Ollama `images`; prompt
  v2.27); a consumer-supplied `description` is trusted instead. The
  classifier claims attachments per extraction; markers are rendered by
  code, claimed media widen their catalog ACL to the fact's read set
  (monotone union), and a deterministic fallback files whatever no plan
  claimed — catalogued media is never dead memory. Exit:
  `GET /media/<catalog_id>` (per-media ACL, strong sha256 ETag,
  inline-safe MIME policy) plus the dashboard's cookie-authenticated
  alias with inline `<img>`/`<video>`/`<audio>` rendering of embeds;
  the export archive bundles referenced blobs under `_media/` with a
  catalog manifest; `wiki_lint` ships the `embed_missing` check. The
  marker grammar legalizes embeds inside region bodies (collected on
  the Region event, no more `NestedRegion` warning) so media travel
  with their facts through page reorganizations. The hermes bridge
  grows a standalone `mwe-media` gateway-hook plugin (opt-in):
  Telegram media → fail-closed sender gate → upload → spool →
  `attachments` on the turn's ingest, closing the host's
  native-image-mode memory bypass.

- **The agent-bridge home (`agents-bridges/`) and the hermes bridge.** Host
  adapters are now in-repo deliverables: an authoring guide, a per-bridge
  `bridge.toml` compat manifest (pinned upstream + per-turn-contract
  version, schema-checked), a two-tier smoke harness (offline against a
  recording stub endpoint; live against a real server), and a separate
  non-blocking CI workflow with a weekly upstream-HEAD canary. The
  per-turn contract in `INTEGRATING.md` is stamped **v1**. The first
  bridge ships with it: a zero-fork **hermes-agent plugin pair**
  (`mwe` memory provider — one mechanical ingest per turn, consumer-owned
  window, per-sender act-as pool, one-way mirror of the built-in memory;
  `mwe-truncate` context engine — bounded window, no summarization pass),
  validated live end-to-end.
- **Per-user (addressed) structure proposals** (migration 0032). A
  `recipient_id` column records who a proposal concerns; REM derives it
  from the triggering fact and carries it on the `StructureProposed` /
  `DedupProposed` event payloads. The dashboard tray,
  `structure_proposal_list`, and `pending_attention` scope to "addressed
  to me or unaddressed" for a non-admin (admins see all); apply / confirm
  / revert are gated to the addressee or an admin.
- **Single-use dashboard magic-link.** `GET /dashboard/auth/link` redeems
  a `dashboard_link` token exactly once (compare-and-set on the `jti` in
  `token_blacklist`), sets the sliding session cookie, and redirects to
  the deep-link. `dashboard_link` URLs now target this endpoint and are
  no longer replayable. Together these wire the per-user proposal
  notification flow: REM event → consumer agent → Telegram → single-use
  dashboard link.

### Changed

- **`mwe-mcp serve` provisions the dedicated-user service for you.** The
  dedicated-user gate (roadmap 14b) used to refuse to boot under a login
  account or root and only *print* the `useradd`/`chown`/`chmod` steps.
  Now, on an interactive terminal, it **offers to set the whole thing
  up**: on confirmation it creates the `mwe-mcp` account, installs the
  binary to `/usr/local/bin/mwe-mcp`, relocates (preserving data) or
  creates and locks the workdir at `/home/mwe-mcp/workdir`, installs the
  `mwe-mcp.service` unit (`User=mwe-mcp`, `Restart=on-failure`,
  `ProtectSystem=strict`, boot-enabled), and `enable --now`s it — then
  hands the port to the service and exits. Each privileged step is shown
  and runs under `sudo`. Declining, or a non-interactive host (systemd,
  CI, container, piped stdin), keeps the printed manual steps.
  And on an interactive **`--bypassdedicateduser`** run under a login
  account — the shape for a box dedicated to mwe-mcp (no co-located
  consumer to wall off) — it likewise offers a restart-on-boot service,
  this one `User=<your login user>` with the bypass baked into `ExecStart`
  and no workdir relocation. Both generated units pin `XDG_CACHE_HOME`
  inside the workdir so the bge-m3 weights (~2.2 GB) download succeeds
  under `ProtectSystem=strict`. Net effect: `mwe-mcp serve` takes a fresh
  operator from a login-account refusal to a running, boot-enabled,
  auto-restarting service in one prompt — co-located *or* standalone.
- **`serve` asks where to listen.** `--bind` / `--port` are now optional;
  on a bare interactive `serve` it asks whether to expose the server to
  other machines (`0.0.0.0`, LAN / port-forwardable) or keep it local
  (`127.0.0.1`, the default) and on which port — mwe-mcp is a server
  multiple consumers reach over HTTP, often from other hosts. The choice
  bakes into the systemd unit when the gate provisions one. Passing either
  flag, or a non-interactive host, skips the prompt and uses the loopback
  defaults. When you do expose it, the endpoint is JWT-gated but plain
  HTTP — put TLS in front (reverse proxy / tunnel) and mint `exposed`
  tokens.

### Removed / Breaking

- **The `wiki_type` registry tools and the runtime type-forge were removed.**
  Concretely: the three MCP tools `wiki_type_register` / `wiki_type_list` /
  `wiki_type_describe` (the tool roster drops 23 → 20); the dashboard **Types** page
  and the chat **forge / schema-evolve** verbs; and the emergent *vertical-genre*
  layer. The bundled templates, the registry table, and the structured routing remain;
  the core `_internal.wiki_type_*` functions still exist server-side.

## [0.2.0] — 2026-05-30

First real release. It back-fills the whole feature set built across
Phase B (memory engine + MVP dashboard) and Phase C (REM, structure
proposals, smart wikis) — the surface that turns the Phase A
scaffold into a working product. This is a **documentation-consolidation
milestone**, not a frozen-API 1.0: the public release with stability
guarantees is the Phase E target. The repo is now at Phase D
(first-consumer cutover). For what each capability *is and does*, the
documentation set (now `docs/`) is the single source
of truth; the pointers below link the relevant page.

### Added

- **Filesystem-SSOT memory model.** Memory lives as Obsidian-native
  markdown on disk; the `engine.db` sqlite index is fully
  reconstructible by re-walking the filesystem, so deleting it is a
  recoverable operation rather than data loss
  ([`docs/concepts/memory-model.md`](docs/concepts/memory-model.md)).
- **`wiki_type` registry** with bundled templates and an on-demand
  forge that invents a new template (frontmatter schema + lifecycle
  rules) at apply time
  ([`docs/concepts/memory-model.md`](docs/concepts/memory-model.md)).
- **Block-level ACL** via inline `{{owner=… allow=… sender=…}}…{{/}}`
  markers, with per-sender redaction applied region-by-region at render
  time ([`docs/concepts/identity-and-acl.md`](docs/concepts/identity-and-acl.md)).
- **Multi-user identity.** Users and groups with a single-admin model,
  managed through the dashboard CRUD; one unified JWT shape shared by
  the MCP and dashboard surfaces
  ([`docs/concepts/identity-and-acl.md`](docs/concepts/identity-and-acl.md)).
- **Write-side flow:** `wiki_capture` / `wiki_supersede` / `wiki_forget`
  / `wiki_link`, with jaccard 6-gram dedup against active facts on
  capture ([`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md)).
- **Hybrid recall:** lexical + semantic (embedding cosine) + wikilink
  multi-hop traversal, ACL-filtered
  ([`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md)).
- **`wiki_ingest_message` LLM router:** a single LLM call classifies a
  consumer message into capture / supersede / recall / structural-hint /
  skip and routes it to the write-side flow
  ([`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md)).
- **REM self-reorganization.** A nightly cycle runs lifecycle rules,
  settles overdue structure proposals, and emits dedup / promotion /
  type-forge / archive proposals plus hub regeneration
  ([`docs/architecture/overview.md`](docs/architecture/overview.md)).
- **MCP tool surface over HTTP** — families A–K (identity, capture,
  recall, ingest, structure proposals, audit, smart-wiki admin,
  skills, smart-consumer bootstrap, …). The exact roster lives in
  [`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md); the
  proposal-write actions (apply / confirm / revert) are dashboard-only,
  not on the MCP surface.
- **Smart-wikis + smart-consumer surface:** `wiki_admin_*`
  authoritative writes, the `_briefing.md` channel, cooperative leases,
  an append-only op-log with revert, the `/cite` resolver, and inline
  dashboard comments
  ([`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md)).
- **Built-in dashboard:** identity console, memory MVP (wiki / fact
  browser), agentic chat panel, admin LLM-config editor, and the
  operational-prompt editor
  ([`docs/architecture/overview.md`](docs/architecture/overview.md)).
- **Configurable internal LLM** with all-local / hybrid / all-api
  profiles across Ollama, Anthropic, and Gemini backends, wired per
  function and per backend through config + the dashboard editor
  ([`docs/architecture/runtime-topology.md`](docs/architecture/runtime-topology.md),
  [`docs/protocol/config-schema.md`](docs/protocol/config-schema.md)).

### Changed

- **Documentation consolidated.** The engineering documentation is now
  the single source of truth for what the system is and does; the
  planning corpus is forward-only (roadmap + open questions).
- Rust toolchain pinned to **1.88** (was 1.85).

### Removed / Breaking

- **stdio MCP transport removed** — the server is HTTP-only now
  ([`docs/architecture/runtime-topology.md`](docs/architecture/runtime-topology.md)).
- **Legacy `enrollment.yaml` loader removed** — identity is created and
  managed through the dashboard first-run wizard + CRUD, not a seed file
  ([`docs/concepts/identity-and-acl.md`](docs/concepts/identity-and-acl.md)).
- Internally, the `wiki_type` "family" column was refactored to a
  `companion: bool` marker (the live registry reads `companion` via
  `is_companion()`; the old `family TEXT` column is retired).

## [0.0.1] — 2026-05-17

### Added
- Initial scaffold for Phase A.
- Cargo workspace with three crates:
  - `mwe-core` — headless memory engine (module skeleton).
  - `mwe-mcp-server` — CLI binary `mwe-mcp` with `init`, `serve`, `token-issue`,
    `token-revoke`, `token-list`, `doctor` subcommands (stubs).
  - `mwe-dashboard` — built-in PWA library (router stub).
- Pinned core dependencies: `rmcp` 1.7, `axum` 0.7, `maud` 0.26, `sqlx` 0.8,
  `tokio` 1, `jsonwebtoken` 9, `uuid` 1 (v7), `notify` 6, `clap` 4,
  `reqwest` 0.12 (rustls), `fs2` 0.4, `rust-embed` 8.
- Rust toolchain pinned to 1.85 (edition 2024).
- `rustfmt.toml`, `clippy.toml`, `.cargo/config.toml`, `deny.toml`.
- Dual license `MIT OR Apache-2.0`.
- Full planning corpus copied into `docs/design/` (read-only reference).
- Placeholder `AGENT_INSTRUCTIONS.md` with cardinal rule + decision tree.
