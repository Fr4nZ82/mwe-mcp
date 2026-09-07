# mwe-mcp — working rules for this repository

mwe-mcp is persistent, structured memory (Markdown wikis) for any AI agent,
served over MCP. Rust server, AGPL-3.0-or-later.

Work happens **directly on `main`, in this directory**. Never create a branch
without asking. Pick the right way, not the easy one.

## The code is the source of truth, and the guide is not a second copy

**Behaviour is answered from the code path, never from prose.** When you state
how the engine behaves, open the function. If you are reporting something you
read in a comment rather than verified, say so in that sentence — they are two
different kinds of claim, and mixing them is how five false sentences reached
the owner in two days, each read back to him as the state of the product.

**Where the durable explanation goes.** The *why* of a design lives in the doc
comment beside the thing it explains, where a reader arrives with the code
already open and where a rename drags it along. History and decisions live in
`planning/logs.md`, dated, one entry each. Neither is a second description of
the system.

**`docs/` is the guide for people, and only that.** Two halves — `operator/`
for whoever installs and runs the server, `user/` for whoever the memory is
about — describing what a person **sees and does**: the screens, their labels,
the buttons, what happens after you press one, in the dashboard's own words. It
is not an engineering wiki, and no page in it explains how the engine works
inside; that answer comes from the code, and a second copy of it is the thing
that goes stale first. Where the guide must state a number — a token's
lifetime, a retention window, the percentage the budget warns at — it is taken
from `config.rs` and the module that reads it, not from another document. Where
the dashboard already has a word for a thing, the guide repeats that word: one
name per thing across the dashboard, the skills and the guide. The dashboard
serves this same folder: `crates/mwe-dashboard/src/routes/guide.rs` embeds it
into the binary and puts it under **Guide** in the top bar, so a page is read
where it is verified and there is no second copy to keep in step.

**The rule that keeps it true, and the whole reason the folder is allowed to
exist:** *before every release, every page of the guide is re-verified on the
running dashboard, and a page nobody can verify is deleted, not kept.* It is
written at the top of `docs/README.md`, because it binds whoever writes there
next. A page nobody maintains is worse than a page that does not exist, since
it has the air of knowing something — which is what those five sentences were
made of.

**Prompts and skills are not prose.** `crates/mwe-core/prompts/` and
`crates/mwe-core/skills/` are read by a model at runtime and handed to the
agents that use the product: a wrong sentence there is a **live bug**, fixed
with the code, always, in the same commit.

## "Wiki" means the memory, and nothing else

A **memory wiki** is the Markdown memory the product maintains at runtime under
`<workdir>/wikis/`. That is the product, and it is the only thing in this
repository that the word names.

## A change leaves no trace of what it removed

**Write what is. Never what was.** This binds every comment, every prompt,
every test message — the whole tracked surface.

When you delete or replace something, the sentences that described it go with
it. Do not annotate them, do not date them, do not keep them "for context":
- ❌ *"it used to key on `index.md` until 2026-08-03"* → say what it keys on.
- ❌ *"a leftover of the page listing deleted on 2026-08-15"* → delete the clause.
- ❌ *"no longer / any more / retired / legacy / before the X rule"* → these
  words are the smell. A reader who never saw the old thing cannot use them,
  and a reader who did does not need them.

Where a fence has no obvious reason, give it a **present-tense** one — *"a
standard wiki has no `index.md`, and nothing may coin one"* — not the story of
how it got there.

**The history has exactly two homes**, and neither is the tracked surface:
`planning/logs.md` (the dated decision log) and `planning/archive/`. Anything
worth keeping goes there, in full, once. `CHANGELOG.md` is the one file on the
tracked surface whose subject *is* history — a release entry stays as written.

**Why this is a rule and not a preference** (founder, 2026-08-20): *«se
facciamo sempre così, cioè che ogni cosa che si modifica lascia sue tracce, si
crea confusione e tanto testo inutile da leggere»*. Every change that leaves a
trace makes the next reader pay for a decision they were not part of, and the
traces accumulate faster than anybody prunes them.

## Finishing is a loop, not a moment

Green tests mean the code runs. They say nothing about what the change left
behind, and that residue — a symbol nobody calls, a comment describing the
previous design, a sentence in a prompt the model still obeys — is what the next
reader picks up and believes. **The compiler cannot see any of it.**

So when you think you are done, you are not. Run this pass, fix everything it
finds, then **run it again** — a fix creates new residue. Stop after a pass that
finds nothing. A *first* pass that finds nothing means it was not a pass.

1. **Dead names.** For every name the change removed or renamed, grep the old
   name across `crates/` — and any local-only directory, which a repo-root
   grep may skip. It must come back **empty**: a surviving mention is
   a trace, and traces do not stay (see the section above).
2. **Dead code.** Anything that lost its last caller goes: functions, `pub`
   items, enum variants, config keys, test helpers, whole modules. `clippy` will
   not tell you — it does not flag a `pub` item nobody uses. Before deleting on
   the grounds that *"another path already does this"*, write down the domain of
   both and compare them: if the one you are deleting is coarser, the difference
   is the case nobody will handle.
3. **Stale comments — read the neighbours, not the diff.** The comment that is
   now wrong is usually the one *beside* your change, which the diff never shows
   you. Open each function you touched and read it whole: its doc comment, the
   comments above and below your hunk, the module header. A comment that
   describes what the code used to do is a defect with the same weight as a bug.
4. **The claim sweep.** Take 2–3 distinctive phrasings of the OLD behaviour
   ("lists wikis", "falls back to", "in the same commit") and grep those. Stale
   *sentences* do not contain the symbol, so step 1 never finds them.
5. **Read the whole diff as a stranger**, top to bottom, asking of each hunk:
   would somebody who was not here understand why this is the way it is?

A leftover you have already seen is never left for later.

## Build / test / CI

```
cargo fmt --all                                         # CI rejects a diff
cargo clippy --workspace --all-targets -- -D warnings
cargo test  --workspace --all-targets
```

Always `--workspace`: per-crate runs miss cross-crate breakage. Never merge on
red CI.

⚠️ `--all-targets` **skips the feature-gated examples** in
`crates/mwe-core/examples/`. CI builds them separately, so a green local gate
can still go red there:

```
cargo build --workspace --examples --features local-embedder
```

**CSS / Maud:** after touching `tailwind/*.css` or introducing a utility class
in a template, recompile (needs the standalone `tailwindcss` CLI):

```
tailwindcss -i tailwind/app.css -o crates/mwe-dashboard/assets/tailwind.css --minify
```

Never commit hand-built assets outside this flow — they are embedded via
`rust-embed` from the sources in `tailwind/`.

## The six model slots are mandatory — there is no "without a model"

`ingest`, `operator_chat`, `rem_promotions`, `rem_dedup_semantic`, `cronista`,
`navigator`. **All six.** Without a working model this product does not work,
and onboarding refuses to finish.

The engine is full of `None` arms for an unconfigured slot. They exist so a
half-wired install fails visibly instead of panicking, **and that is all they
are**. They are not modes, not a cheap tier, not a supported configuration.

So: never explain a behaviour by what happens "if no model is configured",
never offer that as a trade-off, and never let it into a design. The owner has
had to correct this ten times — it keeps coming back because the `None` arms
read like alternatives when you meet them in the code, and they are not.

## Hard rules

- Edition **2024**; `#![forbid(unsafe_code)]` in every crate. The toolchain is
  pinned to `stable` (`rust-toolchain.toml`); the **MSRV is the floor** and is
  declared as `rust-version` in `Cargo.toml` — the two are different claims and
  only the second one is a compatibility promise.
- **`rustls-tls`, never OpenSSL.** Dependencies in `[workspace.dependencies]`,
  versions pinned; `cargo deny` enforces licences.
- ⚠️ **Migrations are never edited** — not even a comment. sqlx checksums them,
  and a change breaks every existing installation. Need a schema change? Add a
  new migration.
- **English across the whole repo surface**: code, comments, commit messages,
  user-visible strings, docs, prompts.
- A production build needs **`--features local-embedder`**. Without it the
  binary starts, runs the migrations, and dies on
  `backend 'bundled' requires a build with the 'local-embedder' feature` — while
  the service manager still reports it as active. Sanity check: the right binary
  is ~38 MB; the Candle stack is around 6 MB of that, so a build that dropped
  the flag comes out that much smaller.

## Commits, pushes and deploys

- Implement, run the gate, update the docs, **commit locally**. Then stop.
- **Push, tag and deploy only when the owner asks.** A deploy costs a heavy
  release build, so they are batched deliberately.
- **Stage explicitly — never `git add -A`.** The owner edits this repo from
  parallel sessions; sweeping their in-progress work into a commit misattributes
  it and buries it under an unrelated subject. Confirm every path in
  `git status --short` is one you touched.
- A red build may not be yours. If the offending code is untracked or absent
  from `HEAD` and outside what you edited, leave it and re-run later.
- ⚠️ **The repository is public and a commit stays in history forever.** No real
  people's names, customer data, credentials or private paths — in commit
  messages, and equally in comments, prompts, examples and test fixtures. Reach
  for the fictional names the repo already uses (`alice`, `bob`, `carol`,
  `frodo`, `famiglia`).

  **There is no exemption, and the two that felt like one are why 24 real
  names sat in six tracked files until 2026-08-20.** A name arriving inside
  something you must not alter is exactly the case the rule is for:

  - **A verbatim quote from the owner is not exempt.** His sentences name his
    family. Substitute the fixture name inside the quote and say once, where it
    is quoted, that the person was renamed — the sentence keeps its authority
    and loses the name.
  - **A measurement is not exempt.** It is written from real rows, so the names
    come along with it. Rename them **as you write**, not afterwards: once the
    paragraph reads as evidence, nobody wants to touch it.

  **The check fires at commit time, and the last of the three nets is the
  push.** Three local hooks read the same private blacklist
  (`$HOME/.config/mwe-scrub-blacklist.txt`, which is never in the repo):
  `pre-commit` scans the lines a commit adds, `commit-msg` scans the message,
  and `pre-push` scans the whole outgoing range. The middle one carries the
  most weight — a message narrates the measurement that justified the change,
  the measurement is written from real rows, and no check on the *files* will
  ever see it. Caught at commit a name costs one word; caught at push it costs
  the rewrite of every commit made since.

  The hooks live in `.git/hooks/`, so a fresh clone has none of them and the
  blacklist is not there to be found either. Until they are installed, run
  the same check by hand while the work is still in the working tree:

  ```
  BL="$HOME/.config/mwe-scrub-blacklist.txt"
  git diff | grep -inEf "$BL"                      # working tree
  git log -p origin/main..HEAD | grep -inEf "$BL"  # already committed
  ```

## Local-only material

Some working material lives on disk but is deliberately **not** in the repo
(see `.gitignore`), so a fresh clone will not have it:

- **`planning/`** — `roadmap.md`, `logs.md` and the per-area cards: future work
  and the dated decision log. A roadmap entry is a **suspicion, not a state**:
  open the code before proposing it as work. Every design decision gets one
  dated line in `planning/logs.md`, which holds **the current week only** and is
  rotated every Monday into `planning/logs/<year>-W<week>.md` — the file says how,
  and a closed week is never rewritten.
- **`GLOSSARIO.md`** — the product's vocabulary, derived from the code, one
  entry per term with what it *is* and what it *is not*. When a thing has a name
  there, that is the name — repeated verbatim, never paraphrased.

**Nothing that begins with "measure", "check in production" or "repair that
row" is work.** It is not a card and it goes nowhere; it comes back by itself
when there is something honest to measure.
