---
title: Public release
status: planned
---

# 5. Public release

A stable public release of mwe-mcp as a consumable open-source product, gated on the first consumer
being production-stable for at least a few weeks. The work is breadth across product surfaces;
documentation and examples may begin in parallel during the consumer cutover.

## Steps

- **5a** — Comprehensive documentation: a getting-started tutorial, integration guides, an
  auto-generated API reference, an architecture overview, an FAQ.
- **5b** — Multi-arch Docker image (amd64 + arm64) with a health endpoint, env-var config, and a
  compose example.
- **5c** — Examples bundle: chat-bot starters (Telegram/Discord/Slack), a Claude Code skill bundle,
  a VSCode extension prototype.
- **5d** — Observability stack: a metrics endpoint, optional trace export, structured JSON logging
  with PII redaction, a health endpoint.
- **5e** — Formal testing strategy: a unit-coverage target on the core, plus integration / E2E /
  load suites with a temporary DB and a mock consumer.
- **5f** — **Done (2026-06-21): dashboard-first operator surface** (the group that
  delivered this is complete and its planning page retired; current state lives in
  [build-run.md](../development/build-run.md) + [dashboard.md](../design-notes/dashboard.md)).
  The direction inverted: instead of *growing* the CLI admin surface, the **dashboard is the
  operator surface** and the CLI shrinks to daemon (`serve`) + optional headless bootstrap (`init`)
  + break-glass/ops (`admin-reset`, `token-revoke`, `token-list`, `migrate`, `backup`) + cron
  (`rem run-*`) + dev (`recall eval`) + boot-failure triage (`doctor`). Admin actions while the
  server is up — token console, LLM/embedding/recall config, live diagnostics, "Run REM now",
  "Backup now" — live in the dashboard.
- **5g** — GDPR/privacy tooling: forget-user (dry-run + execute, cascading to the fact index,
  archive, and events), export-user, configurable retention.
- **5h** — *(partial)* Security hardening: token signing-algorithm configurability, token rotation,
  per-token rate limiting, opt-in 2FA; and dashboard security (XSS/CSRF review, per-endpoint rate
  limiting, an action audit log, idle session timeout).
- **5i** — Dashboard i18n (the backend locale rendering is largely ready) plus a language selector.
- **5j** — Cost guardrails: a monthly hard-stop budget, per-feature cost tracking, configurable
  alerts.
- **5k** — Versioning + migration: a semver convention, breaking-change docs, an upgrade path.
- **5l** — *(partial: CHANGELOG + CI shipped)* Repository hygiene: issue/PR templates, CONTRIBUTING,
  CODE_OF_CONDUCT, SECURITY.

## Open decisions

- **Token rotation and per-token rate-limiting scope.** These two together size the security
  effort. For the release MVP, default to the lighter reading — document the existing revocation
  policy and add a generic per-endpoint throttle — and treat refresh-token rotation plus per-token
  quotas as a follow-up unless a consumer demands them.
