---
title: Structural aftercare — post-deploy live-outcome watch
status: in-progress
---

# 38. Structural aftercare

What the compile/supersede machinery leaves behind, observed live (prod audits 2026-07-02 and
2026-07-05). All the build work landed: succession pointers (38a) and the husk-page GC (38b) on
2026-07-02; the 38c repair set (cadence-aware re-open consumption, dangling-`parent_hub` heal,
empty-leaf-with-children→hub normalisation, the `leaf_with_children` / `hub_with_facts` /
`oversized_pages` reviewer nominations, registry staleness GC, the Cartografo container rule)
and the 4j `EmergedIndex` foundation node on 2026-07-05. Current state:
[narrative-compiler.md](../design-notes/narrative-compiler.md#the-reviewer) ·
[rem-cycle.md](../design-notes/rem-cycle.md). Sibling of
[organic forgetting (11)](11_forgetting.md) and
[self-correcting REM (15)](15_self-correcting-rem.md).

The maintainer's standing rulings govern this group: **REM does the reorganisation, not hand
edits** (remediation is DB/plan-first), and **a non-enrolled subject is a topic, not a user**
(no identity semantics outside enrollment).

## Remaining work — 38d, the post-deploy watch

The repair set is in-tree, not yet deployed. Judged on the fulls that follow the next deploy:

- [ ] `morgana/cucina.md` (leaf, 11 facts + 7 children at the 07-05 baseline): the
  `leaf_with_children` nomination re-opens it → the Cartografo re-homes its facts under the
  container rule → the emptied leaf flips to hub.
- [ ] The `famiglia-bruno-battaglia` absorption (maintainer option A): at the first build the
  `EmergedIndex` node takes the `famiglia_bruno_battaglia` slug — the 46 carried facts
  re-attach to the wiki's `index.md` (the frozen index with its 5 zombie markers and the dead
  dossier link is recompiled from the DB), the legacy sibling file is swept as an orphan, the
  shadowed registry entry drops. Then the `oversized` nomination re-opens the pile and the
  Cartografo splits it by content into leaves under the index.
- [ ] The dangling `esami_sangue_mario_2026_06_25` `parent_hub` heals at the first build — to
  the emerged index (its wiki foundation page), no longer to `None`.
- [ ] The stale registry `matteo` entry drops at the first build.
- [ ] The two ping-pong facts (`019f2e61-c5f6…`, `019f2cfc-faef…`, parked as refile
  candidates): with the cadence fix the refile↔review oscillation converges — a full's
  Cartografo re-places them off the agent's identity index. The reviewer's inclusion of the
  agent wiki in the cross-subject check is **correct as is**.
- [ ] Husk-GC backlog (4 candidates examined per full, gated by the 7-day receipt revert
  window) drains as windows expire.
- [ ] Residual cost accepted by design: a page the Cartografo judges coherent while ≥ the
  oversized threshold re-nominates every review (one extra re-judge per full) — the price of
  keeping the gate out of Rust; revisit `OVERSIZED_PAGE_THRESHOLD` only if it shows up in the
  drift pill.
