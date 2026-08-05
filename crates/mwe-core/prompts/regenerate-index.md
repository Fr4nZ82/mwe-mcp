---
name: regenerate-index
description: Hub Writer prompt — the prose of a compilation plan's `ConceptHub` page, from its children list. The file name is historical: the REM index regenerator this was written for was retired on 2026-08-03 (a wiki's `index.md` is now assembled without a model), and renaming the file would orphan every operator override.
version: 1.7
default_version_at_bootstrap: v1.7
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

⚠️ **Three placeholder NAMES are historical and no longer describe what
they carry.** They were the REM regenerator's, and the hub pass inherited
them; renaming them would break every operator override, so the prompt body
labels them for what they actually are instead.

- `{title}` — the **plan page's** own title (`page.title`), not a wiki's
- `{wiki_type}` — the literal string `"hub"`, always. The compiler passes a
  constant; no wiki type ever reaches it
- `{wiki_id}` — the **plan page's slug** (`page.slug`), not a wiki id
- `{subject}` — whose memory this is, from `wiki::subject_directive`:
  **empty** for an ordinary wiki, and the first-person directive when the
  wiki carries the `is_agent` marker (its index is the opening page of that
  agent's autobiography, not a profile of it). Both consumers pass it; the
  compiler's hub pass resolves it from the page's wiki, since a hub has no
  subject of its own
- `{children}` — markdown bullet list of children as **canonical
  wikilinks** — `- [[wiki_id/page-slug]]`, minted by
  `compiler::plan_page_wikilink` (the link grammar of
  recall-pipeline.md). Already resolvable, to be copied verbatim. A child
  that resolves to no page is omitted rather than rendered as a bare
  `[[wiki_id]]`, which names a map and leads nowhere
- `{snippet}` — the context window the model summarises, `\n\n---\n\n`
  separated: child-page blurbs, `- <slug>: <description>`. **Not facts** — a
  hub has none of its own, which is what makes it a hub

**Output schema** (v1.7): one strict JSON object
`{ "mergedBody": "...", "description": "..." }` — the Cronista's shape minus
`style`, which a hub does not choose (it is overview prose, always `prosa`).
`mergedBody` is the markdown page body, no frontmatter; `description` is the
page's **card**, the one line the recall navigator decides from.

Until v1.6 the reply was bare markdown and the card was the **planner's
literal** `page.description`, so a group's foundation page introduced itself
to the navigator as `Group famiglia` while a person's card said what she
actually needs remembering. A group root has been a door like any other
since 69b, so those two words were the whole basis for opening it or not.

Parsing is **tolerant** (`compiler::parse_cronista`, first `{` to last `}`):
a reply that is not JSON — an operator override still written against v1.6,
or a model that ignored the schema — degrades to *whole reply as the body,
card from the plan*, which is exactly the pre-v1.7 behaviour. There is no
parse-failure path that costs a page.

The compiler wraps the body in a testata (`render_page_file`, `style` forced
to `prosa`) and writes it with `handle.write_page(page.page_path, …)` —
**never** to `<wiki_dir>/index.md`, which since 2026-08-03 is the wiki's map
and is assembled with no model. Expected shape: 6-12 lines of prose. Long
outputs are silently trimmed by `max_tokens`.

**Rail floor** (v1.7): the body's child links are checked against
`{children}` after the call (`compiler::missing_rails`, the same parser the
recall funnel harvests links with), and any child the prose dropped is
appended to the page — a hub whose children are absent has failed at the one
job it has. No rewrite is bought here: this is a single-shot call on the
cheap slot. The gap is reported on `CompileReport::rails_appended`.

**Tool subset**: none. Pure generative call, single shot, no
function-calling.

**Runtime parameters** (from the call site):

| Param | Value | Why |
|---|---|---|
| `temperature` | `0.2` | Mildly deterministic — a hub page is reference prose, not creative writing; small variance avoids the model collapsing on identical wording cycle after cycle. |
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
Write the OVERVIEW page described below. It holds no facts of its own: its whole job is to say what this cluster of pages is about and to send the reader to the right child.
Title: {title}
Type: {wiki_type}
Page id: {wiki_id}
{subject}

Children (a list of [[wikilinks]] — keep every link EXACTLY as written, character-for-character; group sensibly, never restyle or rewrite a link target):
{children}

What each child holds (most-recent first):
{snippet}

EVERY child link above must appear in the body, and the prose around each one must say what a reader will find there: those links are the rails somebody follows to reach the detail, and a link nobody can tell apart from its neighbours is a rail nobody takes.

OUTPUT — one strict JSON object, no prose around it, no code fences, newlines inside strings escaped as \n:
{ "mergedBody": "<the page body in markdown, no frontmatter, concise — 6-12 lines>", "description": "<the page's CARD, one line>" }

DESCRIPTION — the card is not a summary of the body, it is how this page is FOUND:
- Who reads it: the navigator, which is shown a page's name, its keywords and this ONE line, never its prose, and decides from that alone whether to open it. This page is a door like any other.
- Do NOT repeat the page name — it is already shown beside the line — and never answer with the page id or a bare label like "Group X". That is the plan's placeholder, not a card.
- Say what a reader will FIND under here: the subjects this cluster actually covers, in the words somebody LOOKING for them would use, not the category they belong to.
- Make it DISTINGUISH. If the line would sit just as well on a neighbouring page, it is not a card yet.
- Stay at TOPIC level: a card that reports the latest news ages into a lie the next time something happens.

LANGUAGE: {locale}
```
