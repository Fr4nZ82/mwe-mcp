// SPDX-License-Identifier: AGPL-3.0-or-later
//! JSON Schema definitions for the MCP tool surface.
//!
//! Hand-rolled as `serde_json::json!` literals instead of derived via
//! `schemars` because:
//! - `Tool::input_schema` is `Arc<JsonObject>`; we already serialise
//!   inputs through `serde_json::Value` in the dispatcher.
//! - The schemas closely follow the wire shape documented in
//!   [tool-reference.md](../../../../docs/protocol/tool-reference.md);
//!   a literal block keeps the schema next to the spec text.
//! - Tests assert the schema is non-empty + lists the right tool name
//!   — they would still pass under a macro-derived schema, so the
//!   hand-rolled version costs nothing in coverage.
//!
//! Every schema is wrapped in `materialize()` into the `Arc<JsonObject>`
//! shape rmcp wants.

use std::sync::Arc;

use mwe_core::jwt::ConsumerProfile;
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::json;

fn materialize(name: &'static str, description: &'static str, schema: serde_json::Value) -> Tool {
    let obj: JsonObject = serde_json::from_value(schema).expect("schema literal must be an object");
    Tool::new(name, description, Arc::new(obj))
}

fn read_only(t: Tool) -> Tool {
    t.with_annotations(ToolAnnotations::new().read_only(true))
}

/// Mark a tool as destructive (irreversible side effect) in its
/// advertised annotations. `wiki_forget` tombstones a fact (or opens a
/// vote whose silent resolution does), so it carries this hint.
fn destructive(t: Tool) -> Tool {
    t.with_annotations(ToolAnnotations::new().destructive(true))
}

/// The tool surface, in the order documented in
/// [tool-reference.md](../../../../docs/protocol/tool-reference.md).
///
/// Families A through K:
/// - A — `wiki_ingest_message`
/// - B — `events_poll`, `events_ack` (structural-change notices ride
///   here as `structure_applied` events; the whole `structure_proposal_*`
///   family was removed — the dashboard is the undo surface and calls
///   `mwe-core::proposals` directly without going through MCP)
/// - D — `wiki_read`, `wiki_search`, `wiki_navigate`
/// - E — `tool_log_search`, `wiki_lint`
/// - F — `consumer_register`, `wiki_ingest_external`
/// - G — `dashboard_link`
/// - H — `wiki_admin_push`, `wiki_admin_pull`, `wiki_admin_signpost`,
///   `wiki_admin_notify` (smart-wiki authoritative writes for smart
///   consumers, project signposts into the owner's standard memory, +
///   briefing inbox open to any reader).
/// - I — `skill_list`, `skill_fetch` (server-served skill catalog;
///   bundled skills are public, custom skills are owner-scoped).
/// - K — `smart_bootstrap`, `recall_core_global` (atomic primitives for
///   the Claude Code hook bundle: deterministic smart-wiki resume on
///   session start + canonical transversal recall on every user prompt.
///   Both gated on `consumer_class=smart`).
/// - L — `wiki_forget` (authority-routed forget: the caller deletes a fact
///   they authored, or opens an audience vote to forget one they own but did
///   not author; voting itself stays dashboard-only).
#[must_use]
pub fn all_tools() -> Vec<Tool> {
    vec![
        wiki_ingest_message(),
        events_poll(),
        events_ack(),
        wiki_read(),
        wiki_search(),
        wiki_navigate(),
        tool_log_search(),
        wiki_lint(),
        consumer_register(),
        wiki_ingest_external(),
        dashboard_link(),
        wiki_admin_push(),
        wiki_admin_pull(),
        wiki_admin_signpost(),
        wiki_admin_notify(),
        wiki_admin_lease_acquire(),
        wiki_admin_lease_release(),
        skill_list(),
        skill_fetch(),
        smart_bootstrap(),
        recall_core_global(),
        wiki_forget(),
        wiki_forget_bulk(),
    ]
}

/// Tools a [`ConsumerProfile::Web`] smart consumer (the claude.ai web app over
/// the `webagentoauth` flow) is shown. A **whitelist**, not a blacklist: a newly
/// added tool stays hidden from the reduced web surface until it is listed here,
/// which is the safe default.
///
/// It **keeps the full smart-wiki management surface** — `smart_bootstrap`
/// (discover the wiki it owns + its briefing), `wiki_admin_pull` (read its whole
/// wiki), `wiki_admin_push` (write it) — because those are **server-side** reads /
/// writes, not local-file operations; a web consumer with no local copy still needs
/// to discover, read, and write its server-side wiki. It excludes only what a
/// bridge-less web client genuinely cannot use: the `wiki_admin_lease_*` pair
/// (multi-device coordination of a *local* working copy), the event-drain loop
/// (`events_*`), registration / dashboard-link / ops plumbing, and the skill catalog
/// (claude.ai loads skills through its own upload, not MCP `skill_fetch`).
const WEB_TOOLS: &[&str] = &[
    "wiki_search",
    "wiki_navigate",
    "wiki_read",
    "wiki_ingest_message",
    "wiki_ingest_external",
    "wiki_admin_notify",
    "recall_core_global",
    "smart_bootstrap",
    "wiki_admin_push",
    "wiki_admin_pull",
];

/// The tool catalog for a given connection profile.
///
/// [`ConsumerProfile::Local`] (the default for every non-web consumer) gets the
/// full [`all_tools`]; `Web` gets the trimmed [`WEB_TOOLS`] whitelist. Call-time
/// authorization is unchanged — this only shapes what `tools/list` advertises,
/// to cut routing noise for a bridge-less web client.
#[must_use]
pub fn tools_for(profile: ConsumerProfile) -> Vec<Tool> {
    match profile {
        ConsumerProfile::Local => all_tools(),
        ConsumerProfile::Web => all_tools()
            .into_iter()
            .filter(|t| WEB_TOOLS.iter().any(|n| *n == t.name.as_ref()))
            .collect(),
    }
}

fn wiki_ingest_message() -> Tool {
    materialize(
        "wiki_ingest_message",
        "Conversational entry point: ingest a raw user message, classify intent (capture / recall / structural / skip), and return a snippet + seed for the consumer agent's reply.",
        json!({
            "type": "object",
            "required": ["text"],
            "additionalProperties": false,
            "properties": {
                "text": { "type": "string", "description": "Raw user message body." },
                "sender_id": { "type": "string", "description": "Optional override of the token's sender_id (must match)." },
                "author": {
                    "type": "string",
                    "enum": ["user", "assistant"],
                    "default": "user",
                    "description": "Who wrote `text`. Default `user`. Set `assistant` ONLY to feed back the agent's OWN prior reply for extraction (agent-authored memory): the server then keeps just the durable sediment the agent synthesised (an episode/decision, advice tied to the user) and attributes it with `sender = <the calling agent>` instead of the user, scoped to `sender_id`/`X-MWE-Act-As` (the user the agent was talking to)."
                },
                "recent_messages": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["role", "text"],
                        "additionalProperties": false,
                        "properties": {
                            "role": { "type": "string", "enum": ["user", "assistant"] },
                            "text": { "type": "string" },
                            "timestamp": { "type": "string" }
                        }
                    }
                },
                "context_hint": {
                    "type": "string",
                    "enum": ["conversation", "dashboard_command", "import"],
                    "default": "conversation"
                },
                "metadata": {
                    "type": "object",
                    "description": "Free-form. The dispatcher honours `disambig_choice` for the second-turn commit, `locale` (BCP-47 tag, explicit LANGUAGE directive — overrides the per-user `enrollment_users.locale` default), `occurred_at` (the turn's semantic clock for backlog replays) and `authored_refs` (provenance breadcrumbs from a preceding wiki_admin_push).",
                    "properties": {
                        "disambig_choice": { "type": "string" },
                        "locale": { "type": "string", "description": "BCP-47 tag (`it-IT`, `en-US`, ...). Wins over `enrollment_users.locale`." },
                        "occurred_at": { "type": "string", "description": "ISO-8601/RFC-3339 instant the message was originally uttered (backlog replay / import). Relative dates, validity windows and the due-soon horizon resolve against it instead of the server clock; operational timestamps stay wall-clock. A malformed value is rejected." },
                        "authored_refs": { "type": "array", "items": { "type": "string" }, "description": "Smart consumers only: plain `[[wiki_id/page]]` wikilinks for project-wiki pages this turn just authored (echo `wiki_admin_push`'s `authored_refs`). Lets personal memory record a reference to that page instead of duplicating its body. Blank entries / non-strings are ignored." }
                    }
                },
                "attachments": {
                    "type": "array",
                    "description": "Media riding this turn. Bytes travel out of band: upload each file via `POST /media` (multipart, same bearer JWT + X-MWE-Act-As as /mcp) first, then pass the minted catalog_id here. Every id must be readable by the effective sender; the catalog row's kind is authoritative. The described fact carries the `{{embed=…}}` marker; the server renders it — never write marker syntax in text.",
                    "items": {
                        "type": "object",
                        "required": ["catalog_id"],
                        "additionalProperties": false,
                        "properties": {
                            "catalog_id": { "type": "string", "description": "The `c-YYYY-MM-DD-<kind>-NNN.<ext>` key returned by POST /media." },
                            "kind": { "type": "string", "enum": ["photo", "video", "audio", "doc"], "description": "Consumer bookkeeping only — the catalog row's kind wins." },
                            "caption": { "type": "string", "description": "The user's caption for this media, when one rode the message." },
                            "description": { "type": "string", "description": "Consumer-supplied description (own vision / host-side recognizer). When present the server trusts it and does not run its own vision pass." }
                        }
                    }
                },
                "promote": { "type": "string", "enum": ["always", "never"], "description": "Paste-into-chat backstop override: `always` forces / `never` forbids promoting this turn's text to the media rail as a verbatim cited document. Absent = an oversized document-shaped user turn (non-guest, not dashboard_command) is promoted automatically; the response then carries `document_promoted` and the turn is ingested as a bounded excerpt plus the attachment link." }
            }
        }),
    )
}

fn events_poll() -> Tool {
    read_only(materialize(
        "events_poll",
        "Drain pending events for a registered consumer. Filters by `since` timestamp and `kinds` whitelist; caps at 50 per call.",
        json!({
            "type": "object",
            "required": ["consumer_id"],
            "additionalProperties": false,
            "properties": {
                "consumer_id": { "type": "string" },
                "since": { "type": "string", "description": "ISO-8601 timestamp; only events newer than this are returned." },
                "kinds": { "type": "array", "items": { "type": "string" } },
                "top_k": { "type": "integer", "minimum": 1, "maximum": 50, "default": 20 }
            }
        }),
    ))
}

fn events_ack() -> Tool {
    materialize(
        "events_ack",
        "Acknowledge delivery of the listed event ids — the events become invisible to subsequent `events_poll` calls for this consumer.",
        json!({
            "type": "object",
            "required": ["consumer_id", "event_ids"],
            "additionalProperties": false,
            "properties": {
                "consumer_id": { "type": "string" },
                "event_ids": { "type": "array", "items": { "type": "integer" }, "minItems": 1 }
            }
        }),
    )
}

// The structure_proposal_* tools (the `_apply` / `_confirm` /
// `_revert` writes and the `_list` read) were removed from the MCP
// surface. Structural changes apply directly in REM and reach the
// consumer as `structure_applied` notices over `events_poll` — there
// is no proposal queue for an agent to read or act on. Undoing or
// declassing an applied change requires the full context of the
// dashboard, whose handlers call `mwe-core::proposals` directly; the
// notice payload carries the `dashboard_path` to hand the user.

fn wiki_read() -> Tool {
    read_only(materialize(
        "wiki_read",
        "Read the rendered content of a **specific page** of a wiki for the given sender, with ACL applied. `path` selects the page relative to the wiki directory (default `index.md`, e.g. `recipes/pasta.md`) and must be a safe relative path; an unknown page is `not_found`. Returns `content_rendered_for_sender` plus `redacted_count`, behind the wiki-level visibility gate. For a **standard** wiki, marked regions are redacted per the per-fragment ACL while prose passes; a **smart** wiki is markerless, so the wiki-level gate alone governs and the page passes whole. The `format` and `include_archived` arguments are accepted but not yet honored.",
        json!({
            "type": "object",
            "required": ["wiki_id"],
            "additionalProperties": false,
            "properties": {
                "wiki_id": { "type": "string" },
                "sender_id": { "type": "string" },
                "path": { "type": "string", "description": "Page path relative to the wiki directory (default `index.md`). Safe relative paths only (`[A-Za-z0-9._-]` components, no traversal)." },
                "include_archived": { "type": "boolean", "default": false },
                "format": { "type": "string", "enum": ["markdown", "json_blocks"], "default": "markdown" }
            }
        }),
    ))
}

fn wiki_search() -> Tool {
    read_only(materialize(
        "wiki_search",
        "Semantic search over the **whole corpus the sender can read** (ACL-filtered), including other people's / other entities' pages they have access to. **Use this — not `recall_core_global`, which is owner-scoped — for anything about someone or something other than the caller** (a contact's birthday, a colleague's role). If a hit's snippet omits the exact fact, `wiki_read` the page it points to — the prose holds detail the snippet may miss. This is a **flat** top-K lookup; for a question that needs depth or to follow the structure across pages, use **`wiki_navigate`** instead (it returns these flat hits too). Returns top-K hits. The `scope` field `smart`: set to `true` to keep only smart-wiki hits, `false` to exclude them (matched on each wiki's `smart:` flag in its `_meta.md`). `wiki_types` is applied as a post-filter by resolving each hit's `wiki_type` from its `_meta.md`.",
        json!({
            "type": "object",
            "required": ["query"],
            "additionalProperties": false,
            "properties": {
                "query": { "type": "string" },
                "sender_id": { "type": "string" },
                "top_k": { "type": "integer", "minimum": 1, "maximum": 50, "default": 20 },
                "scope": {
                    "type": "object",
                    "properties": {
                        "owner_ids": { "type": "array", "items": { "type": "string" } },
                        "wiki_types": { "type": "array", "items": { "type": "string" } },
                        "smart": { "type": "boolean", "description": "Keep only hits whose per-wiki smart flag matches (true → smart wikis only, false → standard wikis only), read from each wiki's `smart:` flag in its `_meta.md`." },
                        "valid_at": { "type": "string", "description": "Dated query (ISO-8601 instant): keep only facts whose validity window contains this instant — 'what was true on June 4th?'. Without it, a closed window only down-ranks a hit, never hides it." },
                        "include_archived": { "type": "boolean", "default": false }
                    }
                }
            }
        }),
    ))
}

fn wiki_navigate() -> Tool {
    read_only(materialize(
        "wiki_navigate",
        "**Deep recall** over the whole corpus the sender can read (ACL-filtered): a navigator follows the wiki structure hop by hop — the path it takes to reach an answer becomes the answer's context. Returns `navigated` prose fragments, each with its `(wiki_id, page)` (the path), **plus** the flat top-K hits, so this is a **superset** of `wiki_search`. Costlier and slower than `wiki_search` (one LLM call per hop), so reach for it on a **question that needs depth or context** ('tell me everything about X', 'how does Y relate to Z'); use plain `wiki_search` for a quick one-line lookup. To steer it, pass `topics` (subjects to look up) and `owners` (the people/groups the query is about, as `user:<id>`/`group:<id>`) — you know them from the conversation; if you omit them the server extracts them from `query`. Smart wikis are not funnel-navigated (free markdown, not card/wikilink-structured) but their content still surfaces in the flat hits. If no navigator model is wired, `navigator_available` is `false` and only the flat hits come back.",
        json!({
            "type": "object",
            "required": ["query"],
            "additionalProperties": false,
            "properties": {
                "query": { "type": "string", "description": "What to recall, in natural language." },
                "sender_id": { "type": "string", "description": "Optional override of the token's sender_id (must match)." },
                "top_k": { "type": "integer", "minimum": 1, "maximum": 50, "default": 20, "description": "Cap on the flat hits returned (and the RAG seeds that feed the funnel)." },
                "topics": { "type": "array", "items": { "type": "string" }, "description": "Optional. Salient subjects to look up (free text). Supplying these (or `owners`) skips server-side extraction." },
                "owners": { "type": "array", "items": { "type": "string" }, "description": "Optional. Principals the query is about — `user:<id>` / `group:<id>`. Anchors entry into those entities' wikis." }
            }
        }),
    ))
}

fn tool_log_search() -> Tool {
    read_only(materialize(
        "tool_log_search",
        "Audit log query. Admins see every row; regular callers only see their own sender_id rows.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "sender_id_filter": { "type": "string" },
                "tool_name_filter": { "type": "string" },
                "date_range": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" }
                    }
                },
                "result_status": { "type": "string", "enum": ["success", "error"] },
                "top_k": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
            }
        }),
    ))
}

fn wiki_lint() -> Tool {
    read_only(materialize(
        "wiki_lint",
        "Run consistency checks over the corpus. Currently ships marker_malformed / orphan_facts / meta_invalid / embed_missing; the other 4 advertised checks return zero issues for now.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "scope": {
                    "type": "object",
                    "properties": {
                        "wiki_ids": { "type": "array", "items": { "type": "string" } }
                    }
                },
                "checks": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": [
                            "broken_crosslinks",
                            "marker_malformed",
                            "orphan_facts",
                            "meta_invalid",
                            "acl_inconsistent",
                            "embed_missing",
                            "hub_outdated",
                            "superseded_chain"
                        ]
                    }
                }
            }
        }),
    ))
}

fn consumer_register() -> Tool {
    materialize(
        "consumer_register",
        "Idempotent consumer registration. On a fresh registration the returned `consumer_secret` should be persisted; on a refresh it is omitted and only the mutable fields are updated.",
        json!({
            "type": "object",
            "required": ["consumer_id"],
            "additionalProperties": false,
            "properties": {
                "consumer_id": { "type": "string" },
                "display_name": { "type": "string" },
                "callback_url": { "type": "string", "format": "uri" },
                "kinds_subscribed": { "type": "array", "items": { "type": "string" } },
                "metadata": { "type": "object" }
            }
        }),
    )
}

fn wiki_ingest_external() -> Tool {
    materialize(
        "wiki_ingest_external",
        "Ingest a document (long-form content that is not a conversational turn): a media-catalog doc, \
         inline text, or a transcript. Returns an async job receipt; the worker decides the disposition \
         (consult = document page only / dossier = page + selective extraction / dissolve = full \
         extraction) unless `disposition` forces it, and notifies completion via `events_poll` \
         (`document_ingested`). `dry_run` previews the proposal synchronously. file/git/url sources \
         return `not_implemented_phase_c`.",
        json!({
            "type": "object",
            "required": ["source"],
            "additionalProperties": false,
            "properties": {
                "source": {
                    "type": "object",
                    "required": ["type"],
                    "properties": {
                        "type": { "type": "string", "enum": ["media", "inline", "file", "git", "url"] },
                        "catalog_id": { "type": "string", "description": "Required for type=media: an already-uploaded catalog id (POST /media)." },
                        "content": { "type": "string", "description": "Required for type=inline: the document text." },
                        "path": { "type": "string" },
                        "git_ref": { "type": "string" }
                    }
                },
                "text": { "type": "string", "description": "Trusted seam: consumer-supplied extraction of the source bytes. Required for non-textual media (e.g. PDF)." },
                "disposition": { "type": "string", "enum": ["consult", "dossier", "dissolve"], "description": "Forces the dial; absent = the classifier proposes." },
                "format": { "type": "string", "enum": ["prose", "dialogue"], "description": "Forces the segmentation shape; dialogue threads per-utterance timestamps to per-fact validity." },
                "title": { "type": "string", "description": "Title hint (e.g. the original filename)." },
                "occurred_at": { "type": "string", "description": "The document's semantic clock (ISO-8601); relative dates inside the document resolve against it. Defaults to the catalog row's timestamp for media sources." },
                "promote": { "type": "string", "enum": ["always", "never"], "description": "Inline sources only: forces (or forbids) verbatim source promotion. Absent = document-shaped inline text is auto-promoted to the media rail (content-addressed blob + catalog id) so extracted facts cite the preserved original; the response then carries `promoted_catalog_id`." },
                "dry_run": { "type": "boolean", "default": false, "description": "Classify + segment synchronously, write nothing (reports `would_promote`)." },
                "force": { "type": "boolean", "default": false, "description": "Bypass the (document, owner) idempotency check." }
            }
        }),
    )
}

fn wiki_admin_push() -> Tool {
    materialize(
        "wiki_admin_push",
        "Smart-wiki authoritative write (H family). Smart consumers only (`consumer_class=smart`). `mode=create` forges a new wiki under `parent_wiki_id/slug`; `mode=upsert` overwrites pages and applies `deletes` on an existing smart-wiki owned by the caller. No server-side LLM — content is taken verbatim. Optional `mark_processed` rides along the push to mark briefing items as `processed_at=NOW()` atomically with the same transaction (used by the smart consumer to recepire dashboard comments addressed by the push). The response returns `authored_refs`: one `[[wiki_id/page]]` breadcrumb per written page — echo these into the next `wiki_ingest_message` (`metadata.authored_refs`) so personal memory links to the page instead of duplicating its body.",
        json!({
            "type": "object",
            "required": ["mode", "pages"],
            "additionalProperties": false,
            "properties": {
                "mode": { "type": "string", "enum": ["create", "upsert"] },
                "wiki_id": { "type": "string", "description": "Required on `upsert`; forbidden on `create` (derived from parent + slug)." },
                "parent_wiki_id": { "type": "string", "description": "Required on `create`. The new smart-wiki lands as a child of this wiki." },
                "slug": { "type": "string", "description": "Required on `create`. Directory slug for the new wiki." },
                "title": { "type": "string", "description": "Required on `create`. Human display title." },
                "wiki_type": { "type": "string", "description": "Required on create. Free-form tone/label; does NOT determine smart-ness (pass `smart: true`)." },
                "smart": { "type": "boolean", "description": "Set true on create to forge a smart wiki (markerless, content-indexed, owner-administered via wiki_admin_*). Optional; default false." },
                "project_id": { "type": "string", "description": "Optional. Stable opaque id of the project the consumer tracks; stamped into `_meta.md.extra.project_id`." },
                "pages": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["path", "content"],
                        "additionalProperties": false,
                        "properties": {
                            "path": { "type": "string" },
                            "content": { "type": "string" }
                        }
                    }
                },
                "deletes": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Honoured only on `upsert`. `_meta.md` is not deletable."
                },
                "mark_processed": {
                    "type": "array",
                    "items": { "type": "string", "description": "Briefing item id, e.g. \"bi_42\" (the bi_ prefix is optional)." },
                    "description": "Optional list of briefing item ids to mark `processed_at = NOW()` atomically with this push. Each id is validated server-side against the push's target wiki — an id missing or belonging to another wiki yields `400 unknown_briefing_item_id` and the whole push rolls back. Capped at 50 ids per push (`400 too_many_briefing_items` over the cap)."
                },
                "expected_op_log_head": {
                    "type": "integer",
                    "description": "Optimistic concurrency (upsert only): the `op_log_head` the caller last synced to (from a prior push's `op_log_id` or a pull's `op_log_head`). The push is rejected with `409 conflicting_op_log_head` if a newer write op landed since — pull, re-diff, re-push. Pulls/notifies do not bump it. Omit for last-writer-wins."
                },
                "activity": {
                    "type": "string",
                    "description": "One plain-language sentence saying what this push was about, recorded as today's line in the OWNER's project diary — the thing that lets their everyday agent notice this project without being told its name. Write it for someone who has never seen the code: «Fixed a fault that left old content on the screens after an update», not «fixed retry backoff in the job dispatcher (PR #214)». Optional; omit it for a push that carried no real work. Max 250 characters — over the cap the push is REFUSED with the measured length, never truncated. Writing twice in one day replaces that day's line. There is no separate call to make: the server writes the diary itself, and the ack reports what it did under `diary`."
                }
            }
        }),
    )
}

fn wiki_admin_pull() -> Tool {
    read_only(materialize(
        "wiki_admin_pull",
        "Dual of `wiki_admin_push` (H family). Smart consumers only. Returns every page of a smart-wiki the caller owns, plus the latest `op_log_head` — stamp it and pass it back as `wiki_admin_push`'s `expected_op_log_head` for optimistic concurrency (the gate is enforced: a stale head yields `409 conflicting_op_log_head`). Narrow with `paths` when you only need a few pages, and pass `shape: true` to get **how each page will retrieve** instead of its bytes: sections, blocks too long to index as one, the share of the page they hold, and a plain-language `note` per bad page — measured from disk, so it answers correctly even though section indexing is queued.",
        json!({
            "type": "object",
            "required": ["wiki_id"],
            "additionalProperties": false,
            "properties": {
                "wiki_id": { "type": "string" },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Wiki-relative page paths (forward slashes) to pull. Omit or leave empty for the whole wiki. A path that does not exist is simply absent from the response."
                },
                "shape": {
                    "type": "boolean",
                    "default": false,
                    "description": "Return `pages[*].shape` (+ a `shape_summary` of how many pages will retrieve badly) instead of `pages[*].content`. This is the post-import report: it never moves the wiki's bytes through your context."
                }
            }
        }),
    ))
}

fn wiki_admin_signpost() -> Tool {
    materialize(
        "wiki_admin_signpost",
        "Tell the owner's standard memory that this project exists (H family, roadmap 48). Smart consumers only, and only for a smart-wiki the caller owns. Writes two kinds of short **signpost** into the owner's reserved `projects.md`: a `description` (what the project is, in plain language) and an `activity` line for one day (what happened that day). A signpost is a POINTER, not a record — it exists so a conversational turn that never names the project can still discover it and dig into the documentation; what was actually done belongs in the project wiki. Caps are enforced server-side (400 chars description, 250 activity) and an over-long field is REFUSED with the measured length, never truncated. Activity lines older than the 5-day window are dropped automatically. Re-writing an unchanged signpost is a no-op, so refreshing on every `wiki_admin_push` is free.",
        json!({
            "type": "object",
            "required": ["wiki_id"],
            "additionalProperties": false,
            "properties": {
                "wiki_id": { "type": "string", "description": "The project (smart) wiki being signposted. Must be owned by the caller." },
                "description": {
                    "type": "string",
                    "description": "What the project is and what it is for, NON-technical, for a reader who has never seen the code. Max 400 characters. Replaces the previous description."
                },
                "activity": {
                    "type": "object",
                    "required": ["day", "text"],
                    "additionalProperties": false,
                    "description": "One day's activity, non-technical and summarised. Re-writing the same day replaces that day's line.",
                    "properties": {
                        "day": { "type": "string", "description": "Calendar day, `YYYY-MM-DD`." },
                        "text": { "type": "string", "description": "What happened that day, in plain language. Max 250 characters. The project name and the date are added by the server." }
                    }
                }
            }
        }),
    )
}

fn wiki_admin_lease_acquire() -> Tool {
    materialize(
        "wiki_admin_lease_acquire",
        "Acquire (or extend) an opt-in cooperative lease on a smart-wiki (H family). Smart consumers only (`consumer_class=smart`). While the lease is active, `wiki_admin_push` from any other consumer (different `consumer_id` or `sender_id`) fails with `423 wiki_locked_by_lease`. Re-acquire by the same caller extends the existing row. TTL default 60s, max 300s. Without a lease, `wiki_admin_push` is still subject to the existing per-wiki rules — the lease is a coordination layer, not a mutex.",
        json!({
            "type": "object",
            "required": ["wiki_id"],
            "additionalProperties": false,
            "properties": {
                "wiki_id": { "type": "string", "description": "Target smart-wiki id (must be owned by the caller)." },
                "ttl_sec": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 300,
                    "description": "Lease lifetime in seconds. Defaults to 60. Server cap is 300."
                }
            }
        }),
    )
}

fn wiki_admin_lease_release() -> Tool {
    materialize(
        "wiki_admin_lease_release",
        "Release a lease the caller currently holds (H family). Smart consumers only. The lease is one-shot: releasing a lease that has already been released, expired, or that belongs to a different `(sender_id, consumer_id)` pair returns `404 not_found`.",
        json!({
            "type": "object",
            "required": ["lease_id"],
            "additionalProperties": false,
            "properties": {
                "lease_id": { "type": "string", "description": "Server-issued opaque lease id returned by `wiki_admin_lease_acquire`." }
            }
        }),
    )
}

fn wiki_admin_notify() -> Tool {
    materialize(
        "wiki_admin_notify",
        "Append an item to a smart-wiki `_briefing.md` (H family). **Open to any token with read access to the target wiki** (NOT restricted to smart consumers): a standard consumer like openclaw must be able to relay user observations into the briefing for the smart consumer to triage at next session. Rate-limited 50 notify/wiki/h.",
        json!({
            "type": "object",
            "required": ["wiki_id", "topic", "body", "source"],
            "additionalProperties": false,
            "properties": {
                "wiki_id": { "type": "string", "description": "Target smart-wiki id." },
                "topic": { "type": "string", "description": "Short topic line (≤ 200 bytes)." },
                "body": { "type": "string", "description": "Markdown body (≤ 4 KB)." },
                "source": {
                    "type": "object",
                    "required": ["kind", "ref"],
                    "additionalProperties": false,
                    "properties": {
                        "kind": { "type": "string", "enum": ["user", "rem", "consumer", "dashboard"] },
                        "ref": { "type": "string", "description": "Free-form attribution (`user:frodo`, `cc-laptop`, …)." }
                    }
                },
                "kind": {
                    "type": "string",
                    "enum": ["observation", "reasoning", "external"],
                    "description": "Optional three-layer classification. Defaults to `observation` when omitted."
                },
                "target_cite": {
                    "type": "string",
                    "description": "Optional stable handle of the form `wiki://<wiki_id>/<page_path>(#<heading-slug>)?` pointing at the wiki section this item is about. Server-side validated; rendered as an inline `→ <cite>` link in `_briefing.md`."
                },
                "ts": { "type": "string", "description": "Optional ISO 8601 timestamp; defaults to server now." }
            }
        }),
    )
}

fn skill_list() -> Tool {
    read_only(materialize(
        "skill_list",
        "List the bundled skills available to this consumer (I family): `core`, `core-globalmemory`, `smart-consumer`, `smart-onboarding`, `standard-conversational`, `smart-codebase` (shipped with mwe-mcp via rust-embed). Each entry carries `name`, `version`, `description`, `depends_on`, `etag` (content hash), and `source` (`bundled`).",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "consumer_class": {
                    "type": "string",
                    "enum": ["smart", "standard"],
                    "description": "Optional filter hint; today every consumer sees the full bundle. Reserved for future class-aware filtering."
                }
            }
        }),
    ))
}

fn skill_fetch() -> Tool {
    read_only(materialize(
        "skill_fetch",
        "Fetch the full markdown body of a single bundled skill (I family). Returns `{ name, version, description, content, etag, source }`. The `etag` matches the value surfaced by `skill_list` so a consumer can short-circuit on cache hit.",
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "description": "Skill name (no `.md` suffix): `core`, `core-globalmemory`, `smart-consumer`, `smart-onboarding`, `standard-conversational`, `smart-codebase`." },
                "version": { "type": "string", "description": "Optional version pin. Reserved — today the only version on disk is the current one; HTTP `/skills/<name>/<version>.md` is the future plumbing." }
            }
        }),
    ))
}

fn dashboard_link() -> Tool {
    materialize(
        "dashboard_link",
        "Mint a sliding-TTL session token + URL for the built-in dashboard. The consumer agent surfaces it as a button / inline link.",
        json!({
            "type": "object",
            "required": ["intent"],
            "additionalProperties": false,
            "properties": {
                "intent": {
                    "type": "string",
                    "enum": [
                        "home", "modify_wiki", "view_wiki",
                        "answer_proposal", "archive_view", "audit", "costs", "settings"
                    ]
                },
                "sender_id": { "type": "string" },
                "context": {
                    "type": "object",
                    "properties": {
                        "wiki_id": { "type": "string" },
                        "proposal_id": { "type": "string" },
                        "chat_seed": { "type": "string" }
                    },
                    "additionalProperties": true
                },
                "channel": {
                    "type": "string",
                    "enum": ["telegram", "discord", "slack", "browser", "vscode", "voice_fallback"]
                }
            }
        }),
    )
}

fn smart_bootstrap() -> Tool {
    read_only(materialize(
        "smart_bootstrap",
        "Surface the smart consumer's session-start landscape — every smart-family wiki the caller owns, with pending briefing items and last op-log activity (K family). Called at session start — the bundled `claude-code.json` SessionStart hook nudges the model to call it: input is `{}` by default; pass `project_hint` to float a particular wiki to the top. **Inside a project folder, pass `project_id`** (the exact derived id — the recipe is in the bundled `core` skill): the response then carries `first_connect`, which says outright whether this project already has a wiki of yours, and when it has none it names the skill that knows what to do about it. Each returned wiki carries `is_self: true` when it is your own operational wiki — its slug is your `consumer_id` AND it is an agent's memory (the engine-written `is_agent` marker, or the `agent` label for a wiki forged before it) — so you can pick yours when the user owns several agent wikis. Smart-only.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "project_hint": {
                    "type": "string",
                    "description": "Optional substring (case-insensitive) matched against each candidate wiki's `_meta.md.extra.project_id`, slug, and title. Matches float to the top; non-matches are still returned, in last-activity order. Empty/whitespace = no hint."
                },
                "project_id": {
                    "type": "string",
                    "description": "The exact stable id of the project this session is working in, derived from the cwd per the `core` skill. Matched for equality (case-sensitive) against `_meta.md.extra.project_id`. Sets `matches_project_id` on the wiki that carries it, floats it first, and populates the `first_connect` block: `{project_id, wiki_id, wiki_found, hint}` — `hint` is non-null only when this project has no wiki yet."
                },
                "briefing_limit_per_wiki": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "default": 5,
                    "description": "Cap on pending `wiki_briefing_items` rows surfaced per wiki. Hard-clamped server-side."
                }
            }
        }),
    ))
}

fn recall_core_global() -> Tool {
    read_only(materialize(
        "recall_core_global",
        "Canonical \"transversal recall\" wrapper around `wiki_search` (K family). Filters to the caller's own (`owner_user = user:<sender>`) wikis **and** excludes smart wikis (per-wiki smart flag) so project-bound memory does not leak into unrelated work — the contract documented in the bundled skill `core-globalmemory.md`. **Owner-scoped: it only ever searches the caller's OWN memory, so it returns nothing about other people or entities.** For a fact about someone/something other than the caller (a contact's birthday, a colleague's role, a shared address), use `wiki_search` instead. Smart-only.",
        json!({
            "type": "object",
            "required": ["query"],
            "additionalProperties": false,
            "properties": {
                "query": { "type": "string", "description": "Free-form recall query. Trimmed; empty after trim → 400." },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "default": 8,
                    "description": "Cap on hits returned. Server clamps to `[1, 20]` to keep the distillate context-budget-friendly per the skill body."
                }
            }
        }),
    ))
}

fn wiki_forget() -> Tool {
    destructive(materialize(
        "wiki_forget",
        "Forget a fact by id, on behalf of the connected user — **authority-routed** (L family). If you **authored** the fact (you are its `sender`), it is tombstoned immediately and the response is `{ outcome: \"forgotten\", fact_id }`. If you did NOT author it but you **own** it (you are its subject, or a member of an owning group), forgetting it needs an **audience vote** — and that vote is opened **from the dashboard, not by the agent**: the response is `{ outcome: \"request_from_dashboard\", fact_id, detail }`, and you should tell the user to open the forget request there (a `dashboard_link` helps). Forgetting an already-forgotten fact is an idempotent success (`outcome: \"already_forgotten\"`). You are refused (`sender_unauthorized`) if you can neither delete nor request — you are not its author, owner, or an owning-group member. **Opening the request and voting on it are both dashboard-only — there is no consumer path that starts a vote in the background.**",
        json!({
            "type": "object",
            "required": ["fact_id"],
            "additionalProperties": false,
            "properties": {
                "fact_id": { "type": "string", "description": "The fact id to forget (a UUIDv7, e.g. as returned in a `wiki_search` hit's `fact_id`)." },
                "reason": { "type": "string", "description": "Optional free-form note on why the fact is being forgotten (audit only)." }
            }
        }),
    ))
}

fn wiki_forget_bulk() -> Tool {
    destructive(materialize(
        "wiki_forget_bulk",
        "Bulk-forget the connected user's OWN facts (L family) — a self-delete you may always perform, with no vote. Choose `scope`: \"all\" forgets every fact you authored across all wikis; \"wiki\" forgets those in one wiki (requires `wiki_id`); \"page\" forgets those on one page (requires `wiki_id` + `page`, the page's file name e.g. \"vacanze.md\"). Only facts YOU authored (where you are the `sender`) are ever touched — another author's facts are never reached, even in the same wiki or page, and no vote is opened. Returns `{ outcome: \"forgotten_bulk\", scope, forgotten: <count>, ... }`. DESTRUCTIVE and potentially wide (especially `scope: \"all\"`) — call only after the user has explicitly asked, in the current turn, to forget a whole wiki / page / everything they have said. To forget ONE fact by id, or to request forgetting a fact you own but did not author, use `wiki_forget`.",
        json!({
            "type": "object",
            "required": ["scope"],
            "additionalProperties": false,
            "properties": {
                "scope": { "type": "string", "enum": ["all", "wiki", "page"], "description": "Breadth of the self-delete: \"all\" (everything you authored), \"wiki\" (one wiki — needs wiki_id), or \"page\" (one page — needs wiki_id + page)." },
                "wiki_id": { "type": "string", "description": "Target wiki id. Required for scope \"wiki\" and \"page\"; ignored for \"all\"." },
                "page": { "type": "string", "description": "Target page file name within the wiki (e.g. \"vacanze.md\"; \".md\" is appended if omitted). Required for scope \"page\"." },
                "reason": { "type": "string", "description": "Optional free-form note on why (audit only)." }
            }
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tools_present_with_unique_names() {
        // History: the surface was trimmed from 14 → 11, then grew as
        // families were added — 13, then H closed at 14, I opened at 16,
        // J opened at 19, H extended to 21, and K opened with
        // `smart_bootstrap` + `recall_core_global` → 23. The wiki_type
        // redesign removed the J family (`wiki_type_register` / `_list` /
        // `_describe`) → 20; the act-first conversion removed the last
        // proposal tool (`structure_proposal_list`) → 19. The K tools
        // stay, both gated on `consumer_class=smart`; both designed for
        // the Claude Code hook bundle (`SessionStart` +
        // `UserPromptSubmit`). `wiki_navigate` (deep recall via the funnel)
        // opened in family D → 20. `wiki_forget` (authority-routed forget)
        // opened family L → 21; `wiki_forget_bulk` (bulk self-delete) → 22;
        // `wiki_admin_signpost` (project signposts into the owner's
        // standard memory, roadmap 48) extended H → 23.
        let tools = all_tools();
        assert_eq!(tools.len(), 23);
        let names: std::collections::HashSet<_> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names.len(), 23, "tool names must be unique");
        for expected in [
            "wiki_ingest_message",
            "events_poll",
            "events_ack",
            "wiki_read",
            "wiki_search",
            "wiki_navigate",
            "tool_log_search",
            "wiki_lint",
            "consumer_register",
            "wiki_ingest_external",
            "dashboard_link",
            "wiki_admin_push",
            "wiki_admin_pull",
            "wiki_admin_signpost",
            "wiki_admin_notify",
            "wiki_admin_lease_acquire",
            "wiki_admin_lease_release",
            "skill_list",
            "skill_fetch",
            "smart_bootstrap",
            "recall_core_global",
            "wiki_forget",
            "wiki_forget_bulk",
        ] {
            assert!(names.contains(expected), "missing tool: {expected}");
        }
        // Conversely, the dropped tools must NOT be present.
        for dropped in [
            "structure_proposal_list",
            "structure_proposal_apply",
            "structure_proposal_confirm",
            "structure_proposal_revert",
        ] {
            assert!(!names.contains(dropped), "tool {dropped} must be removed");
        }
    }

    #[test]
    fn web_profile_trims_the_catalog() {
        let local = tools_for(ConsumerProfile::Local);
        assert_eq!(
            local.len(),
            all_tools().len(),
            "local profile = full catalog"
        );

        let web = tools_for(ConsumerProfile::Web);
        let web_names: std::collections::HashSet<_> = web.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(web_names.len(), 10, "web surface is the 10-tool whitelist");
        for kept in [
            "wiki_search",
            "wiki_navigate",
            "wiki_read",
            "wiki_ingest_message",
            "wiki_ingest_external",
            "recall_core_global",
            // The full smart-wiki management surface: discover (bootstrap),
            // read-whole (pull), write (push) its own server-side wiki.
            "smart_bootstrap",
            "wiki_admin_pull",
            "wiki_admin_push",
        ] {
            assert!(web_names.contains(kept), "web must keep {kept}");
        }
        // Hidden: only what a bridge-less web client genuinely can't use — the
        // local-copy lease coordination, the event-drain loop, registration /
        // dashboard / ops plumbing, and the MCP skill catalog (claude.ai uploads
        // its own skill).
        for hidden in [
            "wiki_admin_lease_acquire",
            "wiki_admin_lease_release",
            "events_poll",
            "events_ack",
            "consumer_register",
            "dashboard_link",
            "wiki_lint",
            "tool_log_search",
            "skill_list",
            "skill_fetch",
        ] {
            assert!(!web_names.contains(hidden), "web must hide {hidden}");
        }
    }

    #[test]
    fn every_tool_has_object_schema() {
        for t in all_tools() {
            assert_eq!(
                t.input_schema
                    .get("type")
                    .and_then(serde_json::Value::as_str),
                Some("object"),
                "{}: schema must be type:object",
                t.name
            );
        }
    }

    #[test]
    fn wiki_ingest_message_schema_advertises_optional_attachments_array() {
        // A consumer discovers the media seam via `list_tools`: the
        // `attachments` array is optional (text-only callers unchanged),
        // each item requires only `catalog_id`, and the kind enum is the
        // canonical English vocabulary fixed at minting.
        let tool = all_tools()
            .into_iter()
            .find(|t| t.name.as_ref() == "wiki_ingest_message")
            .expect("wiki_ingest_message present");
        let props = tool
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("properties object");
        let att = props
            .get("attachments")
            .and_then(|v| v.as_object())
            .expect("attachments property present");
        assert_eq!(
            att.get("type").and_then(serde_json::Value::as_str),
            Some("array")
        );
        let item_required: Vec<&str> = att
            .get("items")
            .and_then(|v| v.get("required"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(item_required, vec!["catalog_id"]);
        let kinds: Vec<&str> = att
            .get("items")
            .and_then(|v| v.get("properties"))
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.get("enum"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(kinds, vec!["photo", "video", "audio", "doc"]);
        let required: Vec<&str> = tool
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        assert!(
            !required.contains(&"attachments"),
            "attachments must remain optional for backward compatibility"
        );
    }

    #[test]
    fn wiki_admin_push_schema_advertises_mark_processed_optional_array() {
        // The smart consumer needs to discover the `mark_processed`
        // field via `list_tools` so it can recepire dashboard comments
        // atomically with its push. Optional, so older callers omitting
        // the field still work.
        let tool = all_tools()
            .into_iter()
            .find(|t| t.name.as_ref() == "wiki_admin_push")
            .expect("wiki_admin_push present");
        let props = tool
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("properties object");
        let mp = props
            .get("mark_processed")
            .and_then(|v| v.as_object())
            .expect("mark_processed property present");
        assert_eq!(
            mp.get("type").and_then(serde_json::Value::as_str),
            Some("array")
        );
        let items_type = mp
            .get("items")
            .and_then(|v| v.get("type"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(items_type, Some("string"));
        // Optional: must NOT be in `required`.
        let required: Vec<&str> = tool
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        assert!(
            !required.contains(&"mark_processed"),
            "mark_processed must remain optional for backward compatibility"
        );
    }

    #[test]
    fn wiki_forget_schema_requires_fact_id_and_is_marked_destructive() {
        let tool = all_tools()
            .into_iter()
            .find(|t| t.name.as_ref() == "wiki_forget")
            .expect("wiki_forget present");
        // `fact_id` is the single required arg; `reason` is optional.
        let required: Vec<&str> = tool
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(required, vec!["fact_id"]);
        let props = tool
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("properties object");
        assert!(props.contains_key("fact_id"));
        assert!(props.contains_key("reason"));
        // Tombstoning a fact is irreversible — the advertised annotation
        // must flag it so a host can surface a confirmation.
        assert_eq!(
            tool.annotations.as_ref().and_then(|a| a.destructive_hint),
            Some(true),
            "wiki_forget must carry the destructive annotation"
        );
    }
}
