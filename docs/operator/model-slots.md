# The six model slots

**Nav: LLM.** `/dashboard/admin/llm-config`

Six jobs inside the memory each need a model, and **all six are required**:
until every one of them has a provider and a model, the dashboard shows a red
line on this page and on Home, and the memory does not work.

| On the page | Slot | What it does |
|---|---|---|
| Conversation & capture | `ingest` | Classifies intent and extracts facts on every message. |
| Operator chat | `operator_chat` | Powers this dashboard's chat panel and its multi-step tool calls. |
| Nightly optimisation | `rem_promotions` | Promotes paragraphs, files and wikis at night. |
| Prose compilation | `cronista` | Writes the readable wiki pages out of the captured facts. |
| Recall navigation | `navigator` | Decides which wikis and pages to open on every turn. |
| Dedup | `rem_dedup_semantic` | The yes/no classifier that decides whether two facts say the same thing. |

Each slot carries its own sizing hint on the page — which of them wants a
strong model and which is happy with a small one.

## 1 · Providers & credentials

Set the credentials **before** the slots: a slot can only use a provider that
already authenticates. Five providers are offered — **Ollama** (local or
remote), **Anthropic**, **Google Gemini**, **OpenAI**, **OpenRouter**. Paste a
key and press **Save**; the cleartext never leaves the server and only the last
four characters are ever rendered back to you.

- **Ollama** needs no key when the daemon is local. For a remote one, set
  **Endpoint** and, if it is authenticated, a Bearer token.
- **Anthropic** takes either an API key or **Claude Code login** — signing the
  requests with your own subscription instead. The page says what that is for:
  personal and test use, while a deployment brings its own keys.

## 2 · Model slots

**Quick profile** fills all six at once — *All local*, *Hybrid*, *All API* —
and you fine-tune afterwards. Beside it the page says how many models the
**Model catalogue** currently knows, with **Refresh catalogue** to fetch the
list again.

Each slot takes a **Provider** and a **Model**. A slot whose provider has no
valid key says so, in place, with a warning line. Under **Advanced** are
**Temperature**, **Max tokens**, **Reasoning** and **Base URL** — leave them
empty and each model's own default applies. Base URL mostly matters for a
remote Ollama.

Press **Save configuration**. Changes take effect on the next request: slots
and keys are applied to the running process as well as written to disk, so
there is no restart. Saving rewrites `mwe-mcp.config.yaml` atomically (keeping
`mwe-mcp.config.yaml.bak`), and comments in that file are not preserved. Keys
set here live in memory; the on-disk copy in `mwe-mcp.env` is what survives a
restart.

## When something does not answer

**[Health](health.md) → Load LLM slot diagnostics** probes every slot against
the live handles and tells you which one is unreachable.
