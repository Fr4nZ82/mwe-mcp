---
name: regenerate-index
description: Hub Writer prompt — the prose of a compilation plan's `ConceptHub` page, from its children list. The file name is historical: the REM index regenerator this was written for was retired on 2026-08-03 (a wiki's `index.md` is now assembled without a model), and renaming the file would orphan every operator override.
version: 1.5
default_version_at_bootstrap: v1.5
source_of_truth: crates/mwe-core/src/compiler.rs (fn compile_hub_page)
---

# Prompt: regenerate-index

Prompt for the narrative compiler's **hub page** pass: a plan page that
has no facts of its own but one or more child leaves, rendered as a short
overview citing every child as a `[[wikilink]]`. It renders against the
`hub_writer` slot.

**Historical name.** This was REM's `index.md` regenerator, and the hub
pass borrowed it. Since 2026-08-03 a wiki's `index.md` is its **map** and
is assembled from the pages on disk with no model involved, so the hub page
is the only caller left. The file keeps its name because renaming it would
orphan every operator override at `<workdir>/prompts/regenerate-index.md`. The dashboard agentic
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
page covers the hub writer's one remaining consumer.

**Call site**: `crates/mwe-core/src/compiler.rs::compile_hub_page` —
invoked once per plan page of type `ConceptHub` during the compile pass.

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
the compiler's hub gate in `compiler::compile_page` — a plan node with
**no facts of its own**, with child leaves, typed `ConceptHub` /
`GroupTheme` / `WikiBuffer`. REM's map writer, which once shared this
prompt, calls no model at all any more; the file keeps its historical name
so operator overrides are not orphaned. The 20-fact bound on `{snippet}`
is applied at render time so the prompt fits the workhorse's ~4k context
budget on a co-resident embedder.

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
