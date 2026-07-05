-- 0052_behaviour_rules_page_rename — unify behaviour-rule storage onto `rules.md`
--
-- Behaviour rules (how the calling agent converses/operates, ingest prompt Part 7b) used to be
-- filed on a dedicated `behaviour_rules.md` page in the agent's own wiki, while every agent wiki
-- already scaffolds an unused `rules.md`. Roadmap 29c reclaims that dead slot: behaviour rules now
-- live on `rules.md` (the agent is never a *sender*, so the engine-policy reader `sender_rules`
-- never runs on an agent wiki — no collision with the user-side engine `rules.md`).
--
-- This re-homes every existing behaviour-rule fact off the old page so the renamed page is the
-- single home — a content migration. `source_path` is the workdir-relative `wikis/<id>/<page>`, so
-- `replace()` rewrites only the basename (`wikis/<agent>/behaviour_rules.md` →
-- `wikis/<agent>/rules.md`). No-op on a fresh database (no such rows).
--
-- NOTE — does NOT touch ownership: an agent-wide rule mis-owned under the legacy soul/operational
-- model (e.g. the legacy "usa claude-code" filed owner=user instead of owner=agent, roadmap 29f)
-- is a per-deployment semantic correction, applied as a runtime step, not generically here.

UPDATE fact_index
   SET source_path = replace(source_path, 'behaviour_rules.md', 'rules.md')
 WHERE source_path LIKE '%behaviour_rules.md';
