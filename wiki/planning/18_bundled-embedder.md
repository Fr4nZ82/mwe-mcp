---
title: Bundled local embedder (Candle) + configurable embedding backend
status: gated
---

# 18. Bundled local embedder

**Landed (18a–18e, 18g).** The binary bundles a local **Candle** (CPU) bge-m3
embedder — kernels compiled in, no external native runtime — so a from-scratch
`all-api` + bundled deployment needs **zero external services** for embeddings;
the release artifacts are built `--features local-embedder`, so the shipped
default *is* the bundled embedder. The embedder stays **configurable**
(`backend: ollama | bundled | openai`) through the typed `embedding:` config seam
+ the admin dashboard editor, and a reindex-identity guard catches an embedder
change at serve startup. Current state lives in the per-area pages —
[config-schema §embedding](../protocol/config-schema.md#embedding),
[dashboard §embedding](../design-notes/dashboard.md),
[reindex-pipeline §embedder-identity guard](../design-notes/reindex-pipeline.md#embedder-identity-guard-roadmap-18g).
(bge-m3 on CPU is ~80 ms/short message; the bge-m3-Ollama → bge-m3-bundled move is
vector-identical, so it needs no reindex.)

## Remaining work

- [ ] 18f — **GPU opt-in build** *(optional, deferred — maintainer 2026-06-22)* —
  a Candle CUDA feature flag; CPU stays the default shipped artifact. Document the
  Blackwell `sm_120` / CUDA-toolkit caveat (bundling the engine does not by itself
  grant GPU acceleration). **Off the critical path:** CPU bge-m3 is ~80 ms per
  short message, comfortably inside the per-turn budget, so the default CPU
  artifact is enough to ship; revisited only if a GPU embedder is concretely
  wanted.

## Open decision (rides 18f)

- **GPU artifact-publishing shape** — a single CPU-default artifact vs. two
  published artifacts (CPU / CUDA). The runtime half is resolved (the dashboard
  offers `gpu` only on a CUDA build, otherwise disabled with the reason); the
  publishing half defers to when the GPU build is wired (18f) and the
  release-artifact story firms up.
