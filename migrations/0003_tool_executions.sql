-- 0003_tool_executions — audit trail (schemi.md §1.3)
--
-- Every MCP-exposed tool invocation is logged here. Queryable via the
-- `tool_log_search` MCP tool. Privacy: never auto-injected into the
-- consumer agent's context (ADR-022).

CREATE TABLE tool_executions (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp      TEXT NOT NULL,
    tool_name      TEXT NOT NULL,
    sender_id      TEXT NOT NULL,
    device_label   TEXT NOT NULL,                     -- from the MCP token
    rate_limit_id  TEXT,
    args_hash      TEXT,                              -- SHA-256, never raw PII
    result_summary TEXT,
    latency_ms     INTEGER,
    cost_estimate  REAL,                              -- EUR (LLM-backed paths only)
    error          TEXT                               -- error class, NULL on success
);

CREATE INDEX idx_tool_ts     ON tool_executions(timestamp);
CREATE INDEX idx_tool_sender ON tool_executions(sender_id, timestamp);
CREATE INDEX idx_tool_name   ON tool_executions(tool_name, timestamp);
CREATE INDEX idx_tool_error  ON tool_executions(error, timestamp) WHERE error IS NOT NULL;
