# mwe-mcp — working rules for this repository

mwe-mcp is persistent, structured memory (Markdown wikis) for any AI agent,
served over MCP. Rust server, AGPL-3.0-or-later.

Work happens **directly on `main`, in this directory**. Never create a branch
without asking. Pick the right way, not the easy one.

## The two things called "wiki" — never conflate them

- **Engineering wiki = [`docs/`](docs/index.md)** — the SSOT for whoever works
  on the code, kept **in lockstep with the code**. It is documentation.
- **Memory wiki** — the Markdown memory the product maintains at runtime for
  the agent that uses it, under `<workdir>/wikis/`, outside this repo. That one
  **is the product**.

Always write the qualified term. Never a bare "wiki".

## The code is the source of truth. `docs/` is a release deliverable

**Never treat a `docs/` page as authoritative.** It describes the current state
of the code, but it carries **no guarantee between releases** — verify against
the code before trusting any page. That is not a precaution, it is the contract.

`docs/` is brought true **before a release**, deliberately, over the areas the
release touched (the commits since the last tag say which). Between releases you
may update a page along with the change that motivates it — that is welcome and
much cheaper than reconstructing it later — but it is **not required**, and a
page that lags is not a defect.

**Prompts and skills are not documentation.** `crates/mwe-core/prompts/` and
`crates/mwe-core/skills/` are read by a model at runtime and by the agents that
use the product: a wrong sentence there is a **live bug**, fixed with the code,
always, in the same commit.

`docs/` pages describe **only the current state**. No history, no "this used to
work like X" — a page is rewritten or deleted, never annotated with its past.
The one exception is a sentence explaining why something is *absent*, when a
reader would otherwise re-add it.

## Finishing is a loop, not a moment

Green tests mean the code runs. They say nothing about what the change left
behind, and that residue — a symbol nobody calls, a comment describing the
previous design, a sentence in a prompt the model still obeys — is what the next
reader picks up and believes. **The compiler cannot see any of it.**

So when you think you are done, you are not. Run this pass, fix everything it
finds, then **run it again** — a fix creates new residue. Stop after a pass that
finds nothing. A *first* pass that finds nothing means it was not a pass.

1. **Dead names.** For every name the change removed or renamed, grep the old
   name across `crates/`, `docs/` — and any local-only directory, which a
   repo-root grep may skip. It must come back **empty**, except where a
   deliberate line explains an absence.
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
  the service manager still reports it as active. Sanity check: a correct binary
  is ~34 MB, a featureless one ~28 MB.

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
  `frodo`, `famiglia`). A measurement is written from real rows, so real names
  travel with it unless you rename them as you write.

## Local-only material

Some working material lives on disk but is deliberately **not** in the repo
(see `.gitignore`), so a fresh clone will not have it:

- **`planning/`** — `roadmap.md`, `logs.md` and the per-area cards: future work
  and the dated decision log. A roadmap entry is a **suspicion, not a state**:
  open the code before proposing it as work. Every design decision gets one
  dated line in `planning/logs.md`.
- **`GLOSSARIO.md`** — the product's vocabulary, derived from the code, one
  entry per term with what it *is* and what it *is not*. When a thing has a name
  there, that is the name — repeated verbatim, never paraphrased.

**Nothing that begins with "measure", "check in production" or "repair that
row" is work.** It is not a card and it goes nowhere; it comes back by itself
when there is something honest to measure.
