---
name: regenerate-index
description: Hub Writer prompt — regenerate `index.md` of a parent wiki from its children list + most recent active facts (the REM regenerator); also reused by the compiler's hub-page pass (`compiler::compile_hub_page`)
version: 1.4
default_version_at_bootstrap: v1.4
source_of_truth: crates/mwe-core/src/rem.rs (fn regenerate_index)
---

# Prompt: regenerate-index

Prompt for the REM nightly sub-job that rebuilds the `index.md`
of any non-smart wiki whose body has fallen behind the children + facts
state. It renders against the `hub_writer` slot. The dashboard agentic
chat panel is a separate prompt at
[`crates/mwe-dashboard/prompts/agentic-chat-panel.md`](../../mwe-dashboard/prompts/agentic-chat-panel.md);
it resolves its own `LlmFunction::OperatorChat` slot (which falls back to
`hub_writer` only when that slot is unset), so the two are decoupled and
produce very different outputs. See
narrative compiler.

## Runtime contract

Operational specs that ship next to the prompt body so they can't
drift from it. Code is the source of truth; the
narrative compiler
page covers the hub writer's two consumers (this regenerator and the
compiler's hub-page pass, `compiler::compile_hub_page`).

**Call site**: `crates/mwe-core/src/rem.rs::regenerate_index` — invoked
once per non-smart wiki by `run_hub_writer` when (a) the wiki has
at least one child AND at least one active fact, (b) the per-cycle cap
`policy.hub_writer_cap` (default `10`) has not been reached, (c) the
wiki is not a smart wiki (a smart wiki's `index.md` is the smart
consumer's responsibility).

**Placeholders** (substituted at render time by
`mwe_core::prompts::render`):

- `{title}` — the parent wiki's display title from `_meta.md`
- `{wiki_type}` — the parent wiki's `wiki_type` slug (e.g.
  `wiki-user`, `wiki-group`, `wiki-root`)
- `{wiki_id}` — canonical wiki id of the parent
- `{subject}` — whose memory this is, from `wiki::subject_directive`:
  **empty** for an ordinary wiki, and the first-person directive when the
  wiki carries the `is_agent` marker (its index is the opening page of that
  agent's autobiography, not a profile of it). Both consumers pass it; the
  compiler's hub pass resolves it from the page's wiki, since a hub has no
  subject of its own
- `{children}` — markdown bullet list of children as **canonical
  wikilinks** (the link grammar of
  recall-pipeline.md):
  the REM regenerator feeds `- [[<child wiki_id>]]` wiki hops, the
  compiler's Hub Writer consumer feeds `- [[wiki_id/page-slug]]` page
  hops (via `compiler::plan_page_wikilink`) — either way already
  resolvable, to be copied verbatim
- `{snippet}` — the context window the model summarises, `\n\n---\n\n`
  separated. **Consumer-dependent**: the REM regenerator feeds the body
  texts of the top 20 most-recent active facts; the compiler's
  ConceptHub / GroupTheme Hub Writer consumer instead feeds child-page
  blurbs (`- <slug>: <description>`), not facts

**Output schema**: plain markdown body for the new `index.md`. No
frontmatter, no fenced code block delimiters around the response —
the orchestrator writes the response verbatim via
`wiki::atomic_write` to `<wiki_dir>/index.md`. Expected shape: 6-12
lines of prose + a short list of children. Long outputs are silently
trimmed by `max_tokens` (no parse failure path).

**Tool subset**: none. Pure generative call, single shot, no
function-calling.

**Runtime parameters** (from the call site):

| Param | Value | Why |
|---|---|---|
| `temperature` | `0.2` | Mildly deterministic — `index.md` is reference prose, not creative writing; small variance avoids the model collapsing on identical wording cycle after cycle. |
| `max_tokens` | `2000` | The target is 6-12 lines (~200-400 tokens); 2000 is comfortable headroom for a wiki with longer prose in any locale. |
| `think:false` | implicit | Workhorse default for Qwen 3.x; see narrative compiler, runtime section. |

**Upstream filter** (decides when the model sees the prompt at all):
the trigger gate in `run_hub_writer` (children present, at least one
active fact, not smart, cap not reached). The 20-fact bound on
`{snippet}` is applied at render time so the prompt fits the
workhorse's ~4k context budget on a co-resident embedder.

## Prompt

**`{locale}`** — substituted before the prompt reaches the model with the
single-line `LANGUAGE` directive from
`mwe_core::locale::memory_directive_for_wiki`: the target wiki's scope
principal (its owning user, or the language a group's members all
declared) names the language. This slot **writes memory** rather than
answering a live turn, so an undeclared locale resolves to **English**
— not to the "mirror the user's message" clause the conversational
slots fall back to.

```text
Regenerate the `index.md` for the memory wiki below.
Title: {title}
Type: {wiki_type}
Wiki id: {wiki_id}
{subject}

Children (a list of [[wikilinks]] — keep every link EXACTLY as written, character-for-character; group sensibly, never restyle or rewrite a link target):
{children}

Context to summarise (most-recent first):
{snippet}

Output the new index.md body in markdown. No frontmatter. Concise — 6-12 lines.

LANGUAGE: {locale}
```
