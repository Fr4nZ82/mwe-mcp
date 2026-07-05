-- 0005_structure_proposals — forge/promote questionnaires (schemi.md §1.5)
--
-- Drives the structural-change approval flow: `wiki_type` forge,
-- stage-3 promotion, packaged forge+promote. Each row carries the
-- questionnaire, the answers, the consolidated spec, and a 7d revert
-- window. Auto-applied via `timeout_at` when the user does not act.

CREATE TABLE structure_proposals (
    proposal_id     TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,                                 -- "wiki_type_forge" | "wiki_promote" | "dedup_merge" | "bundle"
    context         TEXT NOT NULL,                                 -- JSON: { intent, sample_block, source_wiki_id, … }
    questions       TEXT NOT NULL,                                 -- JSON: [{ id, text, options:[…] }]
    proposed_at     TEXT NOT NULL,
    timeout_at      TEXT NOT NULL,                                 -- proposed_at + 24h (configurable)
    status          TEXT NOT NULL DEFAULT 'pending',               -- pending | applied | reverted | expired
    applied_at      TEXT,
    applied_by      TEXT,                                          -- sender_id (NULL on auto-apply)
    answers         TEXT,                                          -- JSON: { question_id: chosen_option_id }
    spec            TEXT,                                          -- JSON: consolidated spec post-integration
    revert_token    TEXT,                                          -- UUID, valid for 7d after applied_at
    revert_deadline TEXT,                                          -- applied_at + 7d
    reverted_at     TEXT
);

CREATE INDEX idx_struct_status ON structure_proposals(status, timeout_at);
CREATE INDEX idx_struct_token  ON structure_proposals(revert_token) WHERE revert_token IS NOT NULL;
