---
title: Extensions (gated)
status: gated
---

# 9. Extensions (gated)

Opt-in advanced extensions, prioritized **only after** the base system (the areas above) has been
production-stable for at least ~6 months across two or more independent consumers. This is a
deferred-gate phase, not active development — nothing here starts before the gate is met. Several
candidates already have partial groundwork (the multi-hop wikilink library, the dashboard skeleton,
the multi-user ACL architecture).

## Candidates

- **9a** — Knowledge-graph tier: predicate-typed relation queries with a dedicated storage table,
  on top of the multi-hop wikilink groundwork.
- **9b** — RL-learned memory policies derived from long-run operational supersede / archive /
  promote patterns.
- **9c** — Cross-deployment federation: bidirectional sync of shared wiki namespaces with conflict
  resolution.
- **9d** — Voice / multimodal native ingestion beyond the shipped media pipeline
  ([media-pipeline.md](../design-notes/media-pipeline.md) — photos get vision + facts, video is
  caption-only, audio is transcript-is-the-fact): server-side STT for hosts that do not
  transcribe, video thumbnails + real video understanding, an MCP base64 attach tool
  (`wiki_attach_media`) for stdio smart agents, and bridge re-send of media on recall (outbound
  media in chat). Also keep in view from the same family: **recognition as a shared cross-agent
  capability** — enrollment galleries/encodings living in the memory, ACL-governed per person, so
  different agents' recognizers resolve to the same people; the v1 `attachments.description` seam
  already accepts a host-side recognizer's output, and names resolve to principals through the
  existing enrollment.
- **9e** — External calendar sync (Google / Outlook / iCal) toward dated commitments
  (the reminder surface of [group 8](8_reminders.md)).
- **9f** — Standalone GUI: decouple the dashboard from the binary, add a visual graph view, complete
  the memory explorer.
- **9g** — Plugin system: hook points for custom REM plugins.
- **9h** — Native multi-tenancy isolation (schema separation or row-level security, per-tenant
  config and billing).
- **9j** — Document-ingest extensions beyond the shipped pipeline
  ([document-ingest.md](../design-notes/document-ingest.md) — disposition dial, async job,
  media/inline sources): **import-as-pages** (a document too big to consult as one blob becoming
  *its own pages* in its own container with scoped search; named consumer: the oversized appliance
  manual), server-side PDF/binary text extraction (the `text` trusted seam covers it meanwhile),
  the `url`/`file`/`git` sources (today `501`), and a dashboard job-status view beyond the
  `document_ingested` completion notice.
- **9i** — Local 9B recall navigator: once recall-as-navigation ships with a well-annotated wiki, a
  small local model can act as the branch selector behind the deterministic ACL + keyword
  pre-filter — a cost, latency, and privacy opt-in.

## Open decision (when the gate opens)

- **Standalone GUI scope.** Complete the existing built-in dashboard (detach, graph view) as
  finishing a partial feature, vs deliver an entirely separate decoupled web service. Default to
  finishing the existing dashboard unless a deployment topology actually requires a decoupled
  service — the skeleton is most of the way there and a rewrite is unjustified absent a concrete
  driver.
