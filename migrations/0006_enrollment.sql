-- 0006_enrollment — DB mirror of enrollment.yaml (schemi.md §1.6)
--
-- Rebuilt from `enrollment.yaml` on every `mwe-mcp init` and
-- `enrollment_reload()` call. Read-only at query time; never mutated
-- through SQL.

CREATE TABLE enrollment_users (
    user_id  TEXT PRIMARY KEY,
    aliases  TEXT,                                                 -- JSON array of strings
    profile  TEXT,                                                 -- free prose
    is_admin INTEGER NOT NULL DEFAULT 0                            -- bool 0/1
);

CREATE TABLE enrollment_groups (
    group_id    TEXT PRIMARY KEY,
    description TEXT,
    members     TEXT NOT NULL,                                     -- JSON array of user_id
    scope       TEXT                                               -- free prose
);
