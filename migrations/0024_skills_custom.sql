-- Custom (user-scoped) skill catalog.
--
-- A "skill" is a markdown document that documents how a consumer LLM
-- agent should behave in a given mode (always-loaded `core`,
-- transversal `core-globalmemory`, companion-bound `smart-companion`,
-- per-class `standard-conversational`, conversion pattern
-- `wiki-companion-codebase`, plus the future auto-generated
-- `wiki-companion-<owner>-<slug>` produced as a side-effect of
-- `wiki_type_register`).
--
-- Bundled skills ship as files inside the binary
-- (`crates/mwe-core/skills/*.md` embedded via rust-embed) and are
-- the same for every operator. **Custom skills are per-owner** —
-- this table is their durable home.
--
-- The `name` is globally unique (custom names start with
-- `wiki-companion-` per the companion naming rule, so they cannot
-- collide with bundled names). `etag` is the sha256 of the canonical
-- content, used by `skill_fetch` for cache validation. `version`
-- carries the template version that minted the skill, so a future
-- "regenerate when the source template version moves" job can
-- detect drift.
--
-- The table is append-only-by-convention; an upsert from
-- `wiki_type_register` either inserts the row or, when the user
-- explicitly requests `mode: "update"`, overwrites the row in place
-- and bumps `updated_at`. There is no soft-delete: when the
-- corresponding `wiki_types_custom` row is dropped, the matching
-- skill row is dropped too.

CREATE TABLE skills_custom (
    name                   TEXT NOT NULL PRIMARY KEY,
    owner_user             TEXT NOT NULL,
    version                TEXT NOT NULL,
    description            TEXT NOT NULL,
    content                TEXT NOT NULL,
    generated_from_type_id TEXT,
    etag                   TEXT NOT NULL,
    created_at             TEXT NOT NULL,
    updated_at             TEXT NOT NULL
);

CREATE INDEX idx_skills_custom_owner
    ON skills_custom(owner_user);

CREATE INDEX idx_skills_custom_generated_from
    ON skills_custom(generated_from_type_id)
    WHERE generated_from_type_id IS NOT NULL;
