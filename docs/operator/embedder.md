# The embedder

**Nav: Embedding.** `/dashboard/admin/embedding`

One embedder backs recall, the dedup check at capture time, and search. The
page states its own default: Ollama `bge-m3` on localhost.

**Backend** offers three:

- **Ollama (HTTP — local / remote)** — the ordinary choice. Set **Model** (the
  wire model name), **Ollama base URL**, and **Dimensions** (the vector size
  the model emits; `bge-m3` is 1024).
- **Bundled (in-binary Candle / bge-m3)** — no external daemon. It needs a
  binary built with the `local-embedder` feature; a build without it says so
  on this page, and the server refuses to start on that setting. **Device** is
  CPU or GPU (GPU needs a CUDA build), and **Bundled model dir** points at
  local weights — leave it empty and they are downloaded to the cache on first
  use.
- **OpenAI-compatible** — not supported; the server refuses to start on it.

Press **Save embedding settings**. Unlike the model slots, this one applies at
the **next server restart**: the embedder is built once at startup.

## Changing the backend or the model is a reindex

Vectors written by one embedder do not match another. If you change either,
run a full reindex — `mwe-mcp reindex` — otherwise recall degrades quietly.
The server's own guard notices at the next startup and warns.
