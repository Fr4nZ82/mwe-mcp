// SPDX-License-Identifier: AGPL-3.0-or-later
//! Agentic tool registry — wiring between the dashboard's chat panel
//! (running an LLM loop with function calling) and the whitelisted
//! subset of `mwe-core`'s `_internal.*` operations.
//!
//! Per the [agentic-chat design note](../../../wiki/design-notes/agentic-chat.md),
//! the chat panel is an *operative tool on the memory itself*. It composes
//! `_internal.*` directly because it lives in-process — no MCP, no
//! JWT, no consumer-delegation gymnastics. ACLs still apply: every
//! dispatch carries the connected `SessionUser`'s `SenderContext` and
//! the same row-level filters that `wiki_recall` / `wiki_search` use
//! when an external consumer agent comes in.
//!
//! ## Scope of the registry
//!
//! - The read-only tools `wiki_recall`, `wiki_list_pages`,
//!   `wiki_get_meta` form the backbone, exercised end-to-end without
//!   giving the model write authority.
//! - The **structure-proposal triad**: `structure_proposal_list`
//!   (read-only), `structure_proposal_get` (read-only), and the first
//!   write tool, `structure_proposal_apply`. The write path is gated
//!   in the system prompt: the model is told to apply only when the
//!   user explicitly confirms in the chat panel turn — the safety is
//!   the operator's authorship of the prompt, not a whitelist veto.
//! - The **batch-fact pair**: read-only `wiki_facts_for`
//!   (filtered listing by wiki, topic, `fact_type`, date range) and
//!   the write tool `wiki_forget` (per-fact tombstone). The chat
//!   uses them together for "sposta/cancella tutti i fatti X" flows:
//!   `wiki_facts_for` to find the set, then a loop of `wiki_forget`
//!   calls after the user confirms. ACL-filtered through the
//!   connected user's `SenderContext`.
//! - **Single-fact correction**: the write tool
//!   `wiki_supersede` lets the chat replace an existing fact with a
//!   corrected body in-place. The flow is `wiki_recall` to surface the
//!   candidate, then `wiki_supersede` after the user confirms both
//!   which fact and the corrected wording. The old row is tombstoned
//!   with `superseded_by` pointing at the new row; owner, ACL,
//!   `fact_type`, topics, and the validity window carry over from the
//!   original so the chat does not need to re-elicit them.
//! - **Hierarchical wiki move**: the write tool
//!   `wiki_change_scope` lets the chat re-parent a wiki (and its
//!   subtree) — moving it under a different parent or promoting it to
//!   the root. The directly moved wiki's `_meta.md.parent_wiki_id` is
//!   rewritten, the on-disk directory is renamed, and
//!   `fact_index.source_path` is rebased for every affected row.
//!   `wiki_id` stays stable per the [memory model](../../../wiki/concepts/memory-model.md)
//!   invariant, so `wiki_id`-based cross-links never need rewriting (no-op
//!   today).
//! - **Single-fact move**: the write tool `wiki_move_fact` relocates
//!   **one** fact on the operator's instruction ("sposta questo fatto su
//!   salute") — to another page of the same wiki
//!   ([`mwe_core::promote::apply_paragraph_to_file_direct`]) or into another
//!   wiki ([`mwe_core::promote::apply_fact_refile_direct`], always landing on
//!   the destination wiki's `index.md`). It reuses the same act-first engine
//!   the REM cross-wiki refile sweep and the comment-apply `move` op use, so
//!   the dashboard, the dream, and the chat all mint the same born-applied +
//!   revertible receipt. Owner-or-admin gated; a smart source or destination
//!   is refused (smart wikis carry wiki-level governance). Write tool.

use std::sync::Arc;

use mwe_core::capture;
use mwe_core::embedder::Embedder;
use mwe_core::fact_index::FactFilters;
use mwe_core::llm::Tool;
use mwe_core::promote;
use mwe_core::proposals::{self, ConfirmError, ListFilters, ProposalStatus};
use mwe_core::recall::{self, SenderContext};
use mwe_core::types::{FactId, WikiId};
use mwe_core::wiki::WikiTree;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;

/// Hard ceiling on agentic loop iterations. Past this the dashboard
/// stops the conversation and surfaces a "tool budget exhausted"
/// message to the user so the LLM cannot loop indefinitely on bad
/// inputs.
pub const MAX_AGENTIC_ITERATIONS: usize = 8;

/// Default `top_k` for the agentic `wiki_recall` tool when the model
/// does not specify one. Tuned for chat-panel readability — a handful
/// of hits, not a wall of text.
const DEFAULT_RECALL_TOP_K: u32 = 5;

/// Whitelisted `_internal.*` operations exposed to the chat panel LLM.
///
/// Each variant carries the matching tool name on the wire
/// (`Self::name`), the JSON Schema describing its arguments
/// ([`tool_descriptors`]), and the per-variant dispatch logic
/// ([`dispatch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgenticTool {
    /// Semantic recall over `fact_index`, ACL-filtered to the
    /// connected user.
    WikiRecall,
    /// List the active pages of a single wiki.
    WikiListPages,
    /// Return the `_meta.md` view of a single wiki.
    WikiGetMeta,
    /// Look up ONE fact by its exact `fact_id`, ACL-filtered. The read
    /// tool to VERIFY an id the operator pasted before any
    /// forget/supersede/move by id — `wiki_facts_for` cannot do this (it
    /// has no `fact_id` filter), so without this tool the model is
    /// tempted to improvise and act on the wrong fact. Returns the fact's
    /// body + status (active / superseded / tombstoned), or "not found"
    /// when the id does not exist or the operator may not read it.
    WikiGetFact,
    /// List pending structure proposals.
    StructureProposalList,
    /// Fetch the full row of a single proposal (context, questions,
    /// status) so the model can summarise the decision for the user.
    StructureProposalGet,
    /// Apply a pending structure proposal with the user-supplied
    /// `answers` payload. The first write tool exposed to the chat
    /// panel; the system prompt requires the model to confirm the
    /// user's intent in the current turn before calling this.
    StructureProposalApply,
    /// Filtered listing of facts the connected user can see — by
    /// wiki, topic, `fact_type`, date range. Read-only. The chat
    /// pairs it with [`Self::WikiForget`] for the "cancella tutti i
    /// fatti X" pattern.
    WikiFactsFor,
    /// Tombstone a single fact (DB tombstone; the filesystem marker
    /// stays on disk for `wiki_lint` to flag later). Write tool — the
    /// chat batches multiple `wiki_forget` calls after the user
    /// confirms the candidate list. A smart-wiki target is refused
    /// (smart wikis are the consumer's — their section rows are not
    /// fact-governed, and the consumer's next push would undo the
    /// tombstone).
    WikiForget,
    /// Replace an existing fact with a corrected body, in-place.
    /// Mirrors [`mwe_core::capture::wiki_supersede`]: the old row is
    /// tombstoned with `superseded_by` pointing at the freshly
    /// captured row, which inherits the original's owner, ACL,
    /// `fact_type`, topics, and validity window. A smart-wiki target is
    /// refused (a supersede would write a marker-wrapped fact into the
    /// consumer's plain-markdown page). Write tool — paired
    /// with [`Self::WikiRecall`] for the single-fact correction flow.
    WikiSupersede,
    /// Move a wiki (and its subtree) under a different parent, or
    /// promote it to the root. Mirrors
    /// [`mwe_core::scope::wiki_change_scope`]: directory is renamed
    /// on disk, the moved wiki's `_meta.md.parent_wiki_id` is
    /// rewritten, parent `children` lists are kept in sync, and
    /// `fact_index.source_path` is rebased for every affected row.
    /// `wiki_id` stays stable, so existing `[[wiki_id]]` /
    /// `[[wiki_id/page]]` cross-links continue to resolve. A smart
    /// source wiki is refused — its wiki-level read audience derives
    /// from its position in the tree (the scope principal), so a
    /// re-parent would change effective read access on the next
    /// reindex/push; [`mwe_core::scope::wiki_change_scope`] carries the
    /// same guard for every other caller. Write tool.
    WikiChangeScope,
    /// Move **one** fact to a different page of the same wiki, or into
    /// another wiki, following the operator's instruction ("sposta questo
    /// fatto su salute"). Reuses the same engine the REM cross-wiki refile
    /// sweep + the comment-apply `move` op use:
    /// [`mwe_core::promote::apply_paragraph_to_file_direct`] for a same-wiki
    /// page→page move, [`mwe_core::promote::apply_fact_refile_direct`] for a
    /// cross-wiki move (which always lands on the destination wiki's
    /// `index.md` — the compilation plan keys pages by bare slug forest-wide,
    /// so a named cross-wiki page would collide; the dest wiki re-homes it on
    /// its next compile). Owner-or-admin gated, standard-wikis only (a smart
    /// source or dest is refused), act-first + born-applied receipt. Write
    /// tool.
    WikiMoveFact,
    /// Delete **one page** of a standard wiki. Facts the operator *sent* are
    /// tombstoned; a foreign-authored fact is *evacuated* intact via the same
    /// refile engine to its sender's home wiki when one exists, falling back
    /// to its owner's home wiki — a fact whose sender and owner both lack one
    /// is tombstoned. **Admin-only**: deleting structure is the operator's
    /// act (see [identity and ACL](../../../wiki/concepts/identity-and-acl.md)); a smart wiki is refused
    /// (wiki-level governance). Act-first: the whole deletion is wrapped in
    /// ONE born-applied `bundle` receipt
    /// ([`mwe_core::page::delete_page_direct`]), undoable from the dashboard.
    /// Write tool.
    WikiDeletePage,
    /// Open a **forget request** for ONE fact the signed-in user does NOT author
    /// — the non-sender owner's path ([`mwe_core::votes::open_forget_request`];
    /// the write-authority model, [identity and ACL](../../../wiki/concepts/identity-and-acl.md)). The signed-in user must be the fact's `owner`
    /// (subject) or a member of an owning group; a **sender** is refused (they
    /// delete directly via [`Self::WikiForget`]). Propose-first: the fact stays
    /// active while the fact's audience votes ([`Self::StructureProposalVote`]) —
    /// a NO-majority blocks it, silence is consent (it is then forgotten). If the
    /// requester is the fact's only reader the forget applies immediately. A
    /// smart-wiki target is refused (forget votes are per-fact governance;
    /// smart governance is wiki-level). Write tool.
    WikiRequestForget,
    /// Undo a previously-applied structure proposal — the inverse of
    /// [`Self::StructureProposalApply`]. Reuses the
    /// [`mwe_core::proposals::revert_proposal`] chassis (no new revert
    /// logic): the dispatcher selects the `RevertAuth` from the row's
    /// status (`applied` → token fetched server-side,
    /// `applied_pending_confirm` → caller) exactly as the form route
    /// does, then hands off to the per-kind inverse (auto-promotion,
    /// dedup merge, type forge). Revert is only available inside the
    /// 7-day window from when the proposal was applied. Write tool.
    StructureProposalRevert,
    /// Confirm an auto-applied structure proposal so it sticks — the
    /// counterpart of [`Self::StructureProposalRevert`] on the
    /// `applied_pending_confirm -> applied` edge. Thin shim over the
    /// [`mwe_core::proposals::confirm_proposal`] chassis (no new confirm
    /// logic): it promotes a proposal the nightly auto-apply sweep landed
    /// on the user's behalf to permanent `applied`, minting the
    /// `revert_token` and opening the 7-day revert window. The chassis
    /// gates by recipient/admin internally
    /// ([`mwe_core::proposals::ConfirmError::NotAuthorized`]), so — unlike
    /// revert's Token path — the dispatcher needs no separate recipient
    /// pre-check. Restores the "confirm a sweep auto-apply" action the
    /// retired pending-confirms form tray used to carry, now living in the
    /// chat as the single operational surface. Write tool.
    StructureProposalConfirm,
    /// Cast a vote on a pending **fact-forget request** — the audience-facing
    /// half of the non-sender owner's forget vote (the write-authority model). The
    /// dashboard chat acts as the signed-in member, so this votes **as
    /// them** ([`mwe_core::votes::cast_vote`] with `sender_ctx.sender_id`):
    /// the engine checks the caller is in the request's eligible set, records
    /// the (final) vote, and tallies — more than half of the **eligible set**
    /// (the fact's audience minus the requester) voting NO
    /// blocks the request (the fact stays); silence past the window is consent
    /// (the fact is then forgotten); an all-voted quorum with no NO-majority
    /// applies it early. A member reaches it from the `pending_votes` recall
    /// reminder's dashboard link. Write tool.
    StructureProposalVote,
}

impl AgenticTool {
    /// Stable on-the-wire name. Must match the JSON Schema below.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WikiRecall => "wiki_recall",
            Self::WikiListPages => "wiki_list_pages",
            Self::WikiGetMeta => "wiki_get_meta",
            Self::WikiGetFact => "wiki_get_fact",
            Self::StructureProposalList => "structure_proposal_list",
            Self::StructureProposalGet => "structure_proposal_get",
            Self::StructureProposalApply => "structure_proposal_apply",
            Self::WikiFactsFor => "wiki_facts_for",
            Self::WikiForget => "wiki_forget",
            Self::WikiSupersede => "wiki_supersede",
            Self::WikiChangeScope => "wiki_change_scope",
            Self::WikiMoveFact => "wiki_move_fact",
            Self::WikiDeletePage => "wiki_delete_page",
            Self::WikiRequestForget => "wiki_request_forget",
            Self::StructureProposalRevert => "structure_proposal_revert",
            Self::StructureProposalConfirm => "structure_proposal_confirm",
            Self::StructureProposalVote => "structure_proposal_vote",
        }
    }

    /// Reverse mapping used by the dispatcher when the model produces
    /// a `tool_call` whose name has to be matched back to a variant.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "wiki_recall" => Some(Self::WikiRecall),
            "wiki_list_pages" => Some(Self::WikiListPages),
            "wiki_get_meta" => Some(Self::WikiGetMeta),
            "wiki_get_fact" => Some(Self::WikiGetFact),
            "structure_proposal_list" => Some(Self::StructureProposalList),
            "structure_proposal_get" => Some(Self::StructureProposalGet),
            "structure_proposal_apply" => Some(Self::StructureProposalApply),
            "wiki_facts_for" => Some(Self::WikiFactsFor),
            "wiki_forget" => Some(Self::WikiForget),
            "wiki_supersede" => Some(Self::WikiSupersede),
            "wiki_change_scope" => Some(Self::WikiChangeScope),
            "wiki_move_fact" => Some(Self::WikiMoveFact),
            "wiki_delete_page" => Some(Self::WikiDeletePage),
            "wiki_request_forget" => Some(Self::WikiRequestForget),
            "structure_proposal_revert" => Some(Self::StructureProposalRevert),
            "structure_proposal_confirm" => Some(Self::StructureProposalConfirm),
            "structure_proposal_vote" => Some(Self::StructureProposalVote),
            _ => None,
        }
    }
}

/// Build the list of [`Tool`] descriptors the LLM receives on every
/// chat turn — name + description + JSON Schema for arguments.
///
/// The descriptions are written carefully: the model picks between
/// tools based on them, so they tell it both *what* the tool does and
/// *when* to use it. Split across `read_only_tool_descriptors` and
/// `proposal_tool_descriptors` so each helper stays under the clippy
/// length cap and so future write tools land in their own block.
#[must_use]
pub fn tool_descriptors() -> Vec<Tool> {
    let mut out = read_only_tool_descriptors();
    out.extend(proposal_tool_descriptors());
    out.extend(batch_fact_tool_descriptors());
    out.extend(correction_tool_descriptors());
    out.extend(scope_tool_descriptors());
    out.extend(move_fact_tool_descriptors());
    out.extend(delete_page_tool_descriptors());
    out.extend(request_forget_tool_descriptors());
    out.extend(revert_tool_descriptors());
    out.extend(confirm_tool_descriptors());
    out.extend(vote_tool_descriptors());
    out
}

fn read_only_tool_descriptors() -> Vec<Tool> {
    vec![
        Tool {
            name: AgenticTool::WikiRecall.name().to_owned(),
            description: "Recall semantically related facts from the user's memory. \
                Use when the user asks about something they have mentioned in the \
                past, or when you need to know what is already stored before \
                modifying the memory. Returns up to `top_k` hits with their \
                fact_id, source wiki, and a body snippet."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language query — what to recall."
                    },
                    "top_k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 25,
                        "description": "Maximum number of hits to return (default 5)."
                    }
                },
                "required": ["query"]
            }),
        },
        Tool {
            name: AgenticTool::WikiListPages.name().to_owned(),
            description: "List the active pages of a single wiki. Use when the user \
                wants to know what is inside a specific wiki, or when you need to \
                pick a `target_page` for a subsequent operation."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "wiki_id": {
                        "type": "string",
                        "description": "Wiki identifier (e.g. \"frodo\", \"famiglia/spesa\")."
                    }
                },
                "required": ["wiki_id"]
            }),
        },
        Tool {
            name: AgenticTool::WikiGetMeta.name().to_owned(),
            description: "Return the metadata block of a single wiki — title, type, \
                slug, owner (derived), parent. Use when the user asks 'what kind of \
                wiki is X' or before suggesting a scope/structure change."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "wiki_id": {
                        "type": "string",
                        "description": "Wiki identifier."
                    }
                },
                "required": ["wiki_id"]
            }),
        },
        Tool {
            name: AgenticTool::WikiGetFact.name().to_owned(),
            description: "Look up ONE fact by its exact fact_id (a UUID). Use this to \
                VERIFY a fact_id the operator gave you before forgetting, superseding, \
                or moving it — `wiki_facts_for` does NOT filter by id, so it cannot \
                confirm a specific fact. Returns the fact's body, wiki, owner, and \
                status (active / superseded / tombstoned), or `{\"found\": false}` when \
                no such fact exists or you may not read it. If it returns not-found, \
                tell the operator — do NOT substitute a different fact."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact_id": {
                        "type": "string",
                        "description": "Exact fact_id (UUIDv7) to look up."
                    }
                },
                "required": ["fact_id"]
            }),
        },
    ]
}

fn proposal_tool_descriptors() -> Vec<Tool> {
    vec![
        Tool {
            name: AgenticTool::StructureProposalList.name().to_owned(),
            description: "List structure proposals (REM-emitted changes awaiting the \
                user's call: forge a new wiki type, promote a paragraph, merge \
                duplicates, …). Use to see what is pending, or to find the \
                proposal_id when the user refers to a proposal by description \
                rather than id."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["pending", "applied", "applied_pending_confirm", "reverted", "expired"],
                        "description": "Lifecycle status filter (default: pending). `applied_pending_confirm` = auto-applied by the nightly sweep, awaiting the user's confirm-or-revert."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Kind filter (e.g. \"wiki_promote\", \"dedup_merge\")."
                    },
                    "top_k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Page size (default 20, max 50)."
                    }
                },
                "required": []
            }),
        },
        Tool {
            name: AgenticTool::StructureProposalGet.name().to_owned(),
            description: "Fetch the full context + questions + status of a single \
                proposal by id. Use before applying so you can summarise the \
                decision and show the user what they are confirming."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "proposal_id": {
                        "type": "string",
                        "description": "Opaque proposal id (a UUID, e.g. \"3fa85f64-5717-4562-b3fc-2c963f66afa6\")."
                    }
                },
                "required": ["proposal_id"]
            }),
        },
        Tool {
            name: AgenticTool::StructureProposalApply.name().to_owned(),
            description: "Apply a pending structure proposal with the given answers \
                payload (a JSON object whose shape is dictated by the proposal's \
                `questions`). WRITE TOOL: call this only after the user has \
                explicitly confirmed in the current turn — never on a vague \
                request. On success returns the revert_token + 7d revert_deadline \
                so the user can undo."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "proposal_id": {
                        "type": "string",
                        "description": "Opaque proposal id."
                    },
                    "answers": {
                        "type": "object",
                        "description": "Per-kind answers object (e.g. {\"target_page\": \"index.md\"} for wiki_promote, {} for dedup_merge)."
                    }
                },
                "required": ["proposal_id", "answers"]
            }),
        },
    ]
}

fn batch_fact_tool_descriptors() -> Vec<Tool> {
    vec![
        Tool {
            name: AgenticTool::WikiFactsFor.name().to_owned(),
            description: "Filtered listing of facts the connected user can see. \
                Use BEFORE any batch operation (`wiki_forget` and friends) so you \
                can show the user what you are about to act on. Filters are AND-\
                combined; `topics_any` matches any one of the supplied tags. \
                Returns up to `limit` hits (default 50) with their fact_id, \
                wiki_id, body snippet, and owner."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "wiki_id": {
                        "type": "string",
                        "description": "Optional wiki scope (e.g. \"frodo\", \"famiglia/spesa\")."
                    },
                    "fact_type": {
                        "type": "string",
                        "description": "Optional fact_type tag (e.g. \"bio\", \"preference\")."
                    },
                    "topics_any": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Match any of these topic tags. Empty/absent = no topic constraint."
                    },
                    "created_after": {
                        "type": "string",
                        "description": "ISO 8601 lower bound on creation time (inclusive)."
                    },
                    "created_before": {
                        "type": "string",
                        "description": "ISO 8601 upper bound on creation time (exclusive)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "description": "Hard cap on returned rows (default 50, max 100)."
                    }
                },
                "required": []
            }),
        },
        Tool {
            name: AgenticTool::WikiForget.name().to_owned(),
            description: "Tombstone a single fact (DB row marked deleted; \
                filesystem marker is left in place for `wiki_lint` to flag \
                later). WRITE TOOL — call this only after the user has \
                explicitly confirmed in the current turn which fact(s) to \
                forget. For batch deletes call this once per fact_id; do not \
                hand-write a multi-fact wrapper."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact_id": {
                        "type": "string",
                        "description": "Opaque fact id (UUIDv7-ish; produced by `wiki_recall` / `wiki_facts_for`)."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Short audit reason. Recommended values: \"user_request\", \"filesystem_removed\", \"gdpr_erasure\"."
                    }
                },
                "required": ["fact_id", "reason"]
            }),
        },
    ]
}

fn correction_tool_descriptors() -> Vec<Tool> {
    vec![Tool {
        name: AgenticTool::WikiSupersede.name().to_owned(),
        description: "Replace an existing fact with a corrected body, in-place. \
            Keeps the original's owner, ACL, fact_type, topics, and validity \
            window; only the body changes. The old row is tombstoned with `superseded_by` \
            pointing at the new row, so a future `wiki_recall` returns only \
            the corrected text. WRITE TOOL — call this only after the user \
            has explicitly confirmed both which fact to correct (by fact_id \
            surfaced from `wiki_recall`) and the corrected wording in the \
            current turn."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "old_fact_id": {
                    "type": "string",
                    "description": "Opaque fact id of the row being corrected (surfaced by `wiki_recall`)."
                },
                "new_body": {
                    "type": "string",
                    "description": "The corrected prose body — what the original fact should have said. No markers, no markdown frontmatter."
                }
            },
            "required": ["old_fact_id", "new_body"]
        }),
    }]
}

fn scope_tool_descriptors() -> Vec<Tool> {
    vec![Tool {
        name: AgenticTool::WikiChangeScope.name().to_owned(),
        description: "Move a wiki (and its subtree) to a new place in the tree — \
            either under a different parent or promoted to the root. The wiki \
            keeps its stable wiki_id, so existing `[[wiki_id]]` cross-links \
            still resolve afterwards. The wiki's owning principal is derived \
            from its new path, so a move never rewrites an owner. Refuses a \
            smart source wiki (smart wikis are the consumer's — a re-parent \
            would change their derived wiki-level read audience). WRITE TOOL — \
            call this only after the user has explicitly named both the wiki \
            being moved and the new parent (or said \"to the root\") in the \
            current turn. Use `wiki_get_meta` first if you need to verify \
            either id."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "source_wiki_id": {
                    "type": "string",
                    "description": "Stable id of the wiki being moved (e.g. \"alice-acmecorp\")."
                },
                "new_parent_wiki_id": {
                    "type": "string",
                    "description": "Stable id of the new parent wiki. Omit (or pass null) to promote to the root."
                }
            },
            "required": ["source_wiki_id"]
        }),
    }]
}

fn move_fact_tool_descriptors() -> Vec<Tool> {
    vec![Tool {
        name: AgenticTool::WikiMoveFact.name().to_owned(),
        description: "Move ONE fact to a different page of the same wiki, or into \
            another wiki, following the operator's instruction (e.g. 'sposta \
            questo fatto su salute'). Cross-wiki lands on the destination wiki's \
            main page; the destination wiki reorganises it. Act-first, revertable \
            from the dashboard. WRITE TOOL — call this only after the operator has \
            explicitly confirmed in the current turn both which fact (by fact_id \
            surfaced from `wiki_recall` / `wiki_facts_for`) and where it should go. \
            ADMIN-ONLY: a move re-categorises (it neither deletes the fact nor \
            changes who can read it), so it is the operator's structure authority — \
            a non-admin caller is refused. Refuses a smart source or destination \
            wiki (smart wikis are the consumer's). Use `wiki_get_meta` to confirm a \
            destination wiki id when unsure."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "fact_id": {
                    "type": "string",
                    "description": "Opaque fact id of the fact to move (surfaced by `wiki_recall` / `wiki_facts_for`)."
                },
                "dest_wiki_id": {
                    "type": "string",
                    "description": "Destination wiki id for a cross-wiki move. Omit or leave empty to move within the same wiki."
                },
                "dest_page": {
                    "type": "string",
                    "description": "Destination page (e.g. \"salute.md\") for a SAME-WIKI page move. Ignored for a cross-wiki move (which always lands on the destination wiki's index.md)."
                }
            },
            "required": ["fact_id"]
        }),
    }]
}

fn delete_page_tool_descriptors() -> Vec<Tool> {
    vec![Tool {
        name: AgenticTool::WikiDeletePage.name().to_owned(),
        description: "Delete ONE page of a (standard) wiki. Facts the operator SENT are \
            tombstoned; facts contributed by OTHERS are evacuated intact to their author's \
            own wiki when one exists — or to their subject's when the author has no home \
            wiki; a fact whose author and subject both lack a home wiki is tombstoned. \
            Report the tombstoned/evacuated counts the tool returns. The whole deletion is \
            ONE revertable bundle, undoable from the dashboard within the revert window. \
            Act-first. WRITE TOOL — high-risk: call ONLY after \
            the operator has explicitly confirmed in the current turn both which page (by \
            wiki_id + page) and acceptance of deletion. Use `wiki_list_pages` to confirm the \
            page exists and `wiki_facts_for` to show what is on it first. ADMIN-ONLY: deleting a \
            page (structure) is the operator's act — a non-admin is refused. A smart wiki is \
            refused. The `delete_all_facts` override changes the outcome for \
            facts the admin does not own — set it ONLY after spelling out the consequence and \
            getting an explicit informed yes; never default it on."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "wiki_id": {
                    "type": "string",
                    "description": "Wiki id the page belongs to (e.g. \"franz\", \"famiglia\")."
                },
                "page": {
                    "type": "string",
                    "description": "Page file name within the wiki (e.g. \"salute.md\", \"index.md\")."
                },
                "delete_all_facts": {
                    "type": "boolean",
                    "description": "ADMIN-ONLY. Tombstone EVERY fact, including ones the admin did not author, with NO evacuation — destroys others' contributions. Default false (the safe sender-keyed path). Only set after an explicit informed confirmation."
                }
            },
            "required": ["wiki_id", "page"]
        }),
    }]
}

fn request_forget_tool_descriptors() -> Vec<Tool> {
    vec![Tool {
        name: AgenticTool::WikiRequestForget.name().to_owned(),
        description: "Open a REQUEST to forget ONE fact the signed-in user does NOT author. \
            WRITE TOOL. Use this when the user wants a fact about THEM (or about \
            a group they belong to) deleted but they did not write it — the fact's author keeps \
            their contribution unless the audience agrees. The signed-in user must be the fact's \
            OWNER (the subject the fact is about) or a member of an owning group; if they are the \
            fact's AUTHOR (sender) this tool refuses — use `wiki_forget` instead (they delete \
            their own directly). Propose-first: the fact STAYS active while the fact's audience \
            votes (`structure_proposal_vote`) — a NO majority blocks it, silence within the \
            window is consent (it is then forgotten). If the requester is the fact's only reader \
            it is forgotten immediately. Verify the fact first with `wiki_get_fact`. Call only \
            after the user has EXPLICITLY asked to forget this specific fact."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "fact_id": {
                    "type": "string",
                    "description": "Exact fact_id (UUIDv7) of the fact to request forgetting. Verify it with `wiki_get_fact` first."
                }
            },
            "required": ["fact_id"]
        }),
    }]
}

fn revert_tool_descriptors() -> Vec<Tool> {
    vec![Tool {
        name: AgenticTool::StructureProposalRevert.name().to_owned(),
        description: "Undo a previously-applied structure proposal — the inverse of \
            `structure_proposal_apply`. WRITE TOOL. It reverts a revertable proposal \
            (an auto-promotion, a dedup merge, a type forge) within the 7-day window \
            from when it was applied; outside that window, or once a per-kind guard \
            refuses (the change has since been built upon), the tool reports why — \
            relay that to the user. Call this only after the user has EXPLICITLY \
            confirmed the undo in the current turn — never on a vague request. Use \
            `structure_proposal_get` first to summarise what will be undone."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "proposal_id": {
                    "type": "string",
                    "description": "Opaque proposal id (a UUID, e.g. \"3fa85f64-5717-4562-b3fc-2c963f66afa6\") of the applied proposal to undo. Find it via `structure_proposal_list(status=\"applied\")` or `wiki_recall`."
                }
            },
            "required": ["proposal_id"]
        }),
    }]
}

fn confirm_tool_descriptors() -> Vec<Tool> {
    vec![Tool {
        name: AgenticTool::StructureProposalConfirm.name().to_owned(),
        description: "Confirm an auto-applied proposal so it becomes permanent (promotes \
            `applied_pending_confirm` -> `applied` and opens the 7-day revert window). WRITE \
            TOOL. Use when the user wants to KEEP a change the nightly cycle auto-applied on \
            their behalf; the alternative is `structure_proposal_revert` to undo it. Only \
            proposals currently in `applied_pending_confirm` (the auto-apply sweep landed them \
            past the pending timeout and they are awaiting the user's call) can be confirmed, \
            and only before their confirm window expires — past that the auto-revert sweep \
            undoes them. Find the candidates via \
            `structure_proposal_list(status=\"applied_pending_confirm\")` and summarise each \
            before asking which to keep. Call this only after the user has EXPLICITLY confirmed \
            in the current turn — never on a vague request."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "proposal_id": {
                    "type": "string",
                    "description": "Opaque proposal id (a UUID, e.g. \"3fa85f64-5717-4562-b3fc-2c963f66afa6\") of the `applied_pending_confirm` proposal to confirm. Find it via `structure_proposal_list(status=\"applied_pending_confirm\")`."
                }
            },
            "required": ["proposal_id"]
        }),
    }]
}

fn vote_tool_descriptors() -> Vec<Tool> {
    vec![Tool {
        name: AgenticTool::StructureProposalVote.name().to_owned(),
        description: "Cast the signed-in member's vote on a pending FACT-FORGET request. \
            WRITE TOOL. Someone who is the subject of a fact they did not write \
            asked for it to be forgotten; the fact STAYS active and is put to its eligible \
            voters (the fact's audience minus the requester): more than half of them voting \
            `no` within the 7-day window BLOCKS the request (the fact stays); \
            silence is consent (the fact is then forgotten); once everyone has voted with no NO \
            majority it is forgotten early. Votes are FINAL — one per member. This votes AS the \
            signed-in member (the engine refuses anyone not in the request's eligible set, the \
            requester, or a re-vote); relay the engine's reason on a refusal. The pending \
            requests a member owes a vote on arrive in the `pending_votes` recall reminder — \
            the proposal read tools are scoped to the REQUESTER, so a voter cannot browse the \
            request here; never report it as nonexistent. Call only after the member has \
            EXPLICITLY said how to vote in the current turn — never guess."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "proposal_id": {
                    "type": "string",
                    "description": "Opaque id of the `fact_forget` request to vote on (from the member's `pending_votes` reminder — a voter cannot browse it via `structure_proposal_list`)."
                },
                "vote": {
                    "type": "string",
                    "enum": ["yes", "no"],
                    "description": "`no` to reject the forget (enough NOs block it, the fact stays), `yes` to approve it (same effect as silence, but recorded so an all-voted quorum can forget early)."
                }
            },
            "required": ["proposal_id", "vote"]
        }),
    }]
}

/// Per-request execution context for the dispatcher.
///
/// Built once at the start of an agentic submission and reused
/// across iterations. `sender_ctx` carries the connected user's
/// identity so every `_internal.*` call respects the same ACLs an
/// external consumer agent would face.
pub struct AgenticContext<'a> {
    pub pool: &'a SqlitePool,
    pub tree: &'a WikiTree,
    pub embedder: Arc<dyn Embedder>,
    pub sender_ctx: SenderContext,
    /// Whether the chat operator holds the admin role (0032). Forwarded
    /// to `apply_proposal` / `confirm_proposal` (admins may act on any
    /// proposal by id).
    pub is_admin: bool,
    /// Whether the admin ACL-reveal switch is active for this request
    /// (`crate::reveal`). It lifts the per-recipient scope on the
    /// proposal **read** tools so an admin can review the whole
    /// deployment's in-flight items — see [`proposal_recipient_scope`].
    /// Always `false` for a non-admin (the predicate gates on the role).
    pub reveal: bool,
}

/// Errors the dispatcher can surface back to the LLM as a `Role::Tool`
/// content. Each variant carries a short, model-readable message.
///
/// The dispatcher does **not** raise these errors up the call stack —
/// they are serialised as JSON and handed back to the LLM so the
/// model can retry with corrected arguments, ask the user, or give
/// up gracefully. The chat panel only sees them as ordinary tool
/// results.
#[derive(Debug, thiserror::Error)]
pub enum AgenticToolError {
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("invalid arguments for `{tool}`: {detail}")]
    InvalidArguments { tool: &'static str, detail: String },
    #[error("internal failure invoking `{tool}`: {detail}")]
    InternalFailure { tool: &'static str, detail: String },
}

/// Dispatch a tool call by name. Returns a JSON-stringified result
/// that the agentic loop hands back to the LLM as the `content` of
/// the corresponding `Role::Tool` message.
///
/// Errors are stringified and returned as `Err`; the caller (the
/// agentic loop) decides whether to feed them back to the model as a
/// tool result with `{"error": "..."}` content, or to bail out
/// entirely. Today the loop feeds them back so the model can recover.
pub async fn dispatch(
    name: &str,
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let tool = AgenticTool::from_name(name)
        .ok_or_else(|| AgenticToolError::UnknownTool(name.to_owned()))?;
    match tool {
        AgenticTool::WikiRecall => dispatch_wiki_recall(arguments, ctx).await,
        AgenticTool::WikiListPages => dispatch_wiki_list_pages(arguments, ctx),
        AgenticTool::WikiGetMeta => dispatch_wiki_get_meta(arguments, ctx),
        AgenticTool::WikiGetFact => dispatch_wiki_get_fact(arguments, ctx).await,
        AgenticTool::StructureProposalList => dispatch_proposal_list(arguments, ctx).await,
        AgenticTool::StructureProposalGet => dispatch_proposal_get(arguments, ctx).await,
        AgenticTool::StructureProposalApply => dispatch_proposal_apply(arguments, ctx).await,
        AgenticTool::WikiFactsFor => dispatch_wiki_facts_for(arguments, ctx).await,
        AgenticTool::WikiForget => dispatch_wiki_forget(arguments, ctx).await,
        AgenticTool::WikiSupersede => dispatch_wiki_supersede(arguments, ctx).await,
        AgenticTool::WikiChangeScope => dispatch_wiki_change_scope(arguments, ctx).await,
        AgenticTool::WikiMoveFact => dispatch_wiki_move_fact(arguments, ctx).await,
        AgenticTool::WikiDeletePage => dispatch_wiki_delete_page(arguments, ctx).await,
        AgenticTool::WikiRequestForget => dispatch_wiki_request_forget(arguments, ctx).await,
        AgenticTool::StructureProposalRevert => dispatch_proposal_revert(arguments, ctx).await,
        AgenticTool::StructureProposalConfirm => dispatch_proposal_confirm(arguments, ctx).await,
        AgenticTool::StructureProposalVote => dispatch_proposal_vote(arguments, ctx).await,
    }
}

#[derive(Debug, Deserialize)]
struct WikiRecallArgs {
    query: String,
    #[serde(default)]
    top_k: Option<u32>,
}

#[derive(Debug, Serialize)]
struct RecallHitSnippet {
    fact_id: String,
    wiki_id: String,
    text: String,
    score: f32,
}

async fn dispatch_wiki_recall(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: WikiRecallArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiRecall.name(),
            detail: e.to_string(),
        }
    })?;
    let top_k = args.top_k.unwrap_or(DEFAULT_RECALL_TOP_K) as usize;
    let hits = recall::wiki_recall(
        ctx.pool,
        Arc::clone(&ctx.embedder),
        &args.query,
        &[],
        top_k,
        FactFilters::default(),
        &ctx.sender_ctx,
    )
    .await
    .map_err(|e| AgenticToolError::InternalFailure {
        tool: AgenticTool::WikiRecall.name(),
        detail: e.to_string(),
    })?;
    let snippets: Vec<RecallHitSnippet> = hits
        .into_iter()
        .map(|h| RecallHitSnippet {
            fact_id: h.fact_id.as_str().to_owned(),
            wiki_id: h.wiki_id,
            text: h.text,
            score: h.score,
        })
        .collect();
    serialise_result("hits", &snippets)
}

#[derive(Debug, Deserialize)]
struct WikiIdArgs {
    wiki_id: String,
}

#[derive(Debug, Serialize)]
struct PageInfoSnippet {
    rel_path: String,
    size: u64,
}

fn dispatch_wiki_list_pages(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: WikiIdArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiListPages.name(),
            detail: e.to_string(),
        }
    })?;
    let wiki_id = WikiId::parse(&args.wiki_id).map_err(|e| AgenticToolError::InvalidArguments {
        tool: AgenticTool::WikiListPages.name(),
        detail: format!("invalid wiki_id `{}`: {e}", args.wiki_id),
    })?;
    let pages = mwe_core::wiki::wiki_list_pages(ctx.tree, &wiki_id).map_err(|e| {
        AgenticToolError::InternalFailure {
            tool: AgenticTool::WikiListPages.name(),
            detail: e.to_string(),
        }
    })?;
    let snippets: Vec<PageInfoSnippet> = pages
        .into_iter()
        .map(|p| PageInfoSnippet {
            rel_path: p.rel_path.display().to_string(),
            size: p.size,
        })
        .collect();
    serialise_result("pages", &snippets)
}

fn dispatch_wiki_get_meta(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let tool = AgenticTool::WikiGetMeta.name();
    let args: WikiIdArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool,
            detail: e.to_string(),
        }
    })?;
    let wiki_id = WikiId::parse(&args.wiki_id).map_err(|e| AgenticToolError::InvalidArguments {
        tool,
        detail: format!("invalid wiki_id `{}`: {e}", args.wiki_id),
    })?;
    let meta = mwe_core::wiki::wiki_get_meta(ctx.tree, &wiki_id).map_err(|e| {
        AgenticToolError::InternalFailure {
            tool,
            detail: e.to_string(),
        }
    })?;
    // The owning principal is derived from topology (the root identity
    // wiki's type), not declared in `_meta.md`.
    let owner = ctx
        .tree
        .resolve_scope_principal(&meta)
        .map_or_else(|e| format!("(unresolved: {e})"), |p| p.to_string());
    let payload = json!({
        "wiki_id": meta.wiki_id.as_str(),
        "title": meta.title,
        "wiki_type": meta.wiki_type,
        "slug": meta.slug.as_str(),
        "owner": owner,
        "parent_wiki_id": meta.parent_wiki_id.as_ref().map(WikiId::as_str),
    });
    Ok(payload.to_string())
}

fn serialise_result<T: Serialize>(
    key: &'static str,
    payload: &T,
) -> Result<String, AgenticToolError> {
    let value = json!({ key: payload });
    serde_json::to_string(&value).map_err(|e| AgenticToolError::InternalFailure {
        tool: "serialise_result",
        detail: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Structure-proposal triad
//
// The first write tool exposed to the chat panel is in this section.
// `structure_proposal_apply` mutates state through
// `mwe_core::proposals::apply_proposal`; the safety contract is that
// the model has been instructed (via the agentic system prompt) to
// require an explicit user confirmation in the same turn before
// dispatching the apply.
// ---------------------------------------------------------------------------

/// Hard ceiling on `structure_proposal_list` size, beyond the
/// per-call `top_k` cap of `mwe_core::proposals`. Keeps the JSON
/// result chunk small enough for the chat panel to render even when
/// dozens of proposals are pending.
const MAX_PROPOSAL_LIST_SIZE: i64 = 20;

#[derive(Debug, Default, Deserialize)]
struct ProposalListArgs {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    top_k: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ProposalListSnippet {
    proposal_id: String,
    kind: String,
    status: String,
    emitted_at: String,
    expires_at: String,
    /// Truncated one-line summary of the JSON context so the panel
    /// does not get a wall of structured data for a list view; the
    /// full context comes back from `structure_proposal_get`.
    context_summary: String,
}

const CONTEXT_SUMMARY_CHARS: usize = 120;

/// Recipient filter for the proposal **read** tools
/// (`structure_proposal_list` / `_get`).
///
/// Everyone — admins included — is scoped to their own proposals
/// (those addressed to them plus the unaddressed / admin-fallback bucket)
/// by default. A proposal's `context` carries the underlying fact text,
/// which is per-fragment ACL'd and is **not** re-projected per reader, so
/// listing every recipient's proposals would leak other users' content.
/// The admin ACL-reveal switch — dashboard-wide, explicit, bannered —
/// lifts the scope (`recipient = None`, every recipient), the same posture
/// the facts table and wiki pages already take. `ctx.reveal` is only ever
/// `true` for an admin (`crate::reveal::active` gates on the role). See
/// `wiki/design-notes/redaction-policy.md`.
fn proposal_recipient_scope(ctx: &AgenticContext<'_>) -> Option<String> {
    (!ctx.reveal).then(|| format!("user:{}", ctx.sender_ctx.sender_id))
}

async fn dispatch_proposal_list(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: ProposalListArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::StructureProposalList.name(),
            detail: e.to_string(),
        }
    })?;
    let status = match args.status.as_deref() {
        None | Some("pending") => Some(ProposalStatus::Pending),
        Some("applied") => Some(ProposalStatus::Applied),
        Some("applied_pending_confirm") => Some(ProposalStatus::AppliedPendingConfirm),
        Some("reverted") => Some(ProposalStatus::Reverted),
        Some("expired") => Some(ProposalStatus::Expired),
        Some(other) => {
            return Err(AgenticToolError::InvalidArguments {
                tool: AgenticTool::StructureProposalList.name(),
                detail: format!(
                    "unknown status `{other}`; expected one of \
                     pending|applied|applied_pending_confirm|reverted|expired"
                ),
            });
        },
    };
    let filters = ListFilters {
        status,
        kind: args.kind,
        // Scoped to the operator's own proposals unless admin reveal is on.
        recipient: proposal_recipient_scope(ctx),
        top_k: args.top_k.map(|n| n.clamp(1, MAX_PROPOSAL_LIST_SIZE)),
    };
    let rows = proposals::list(ctx.pool, &filters).await.map_err(|e| {
        AgenticToolError::InternalFailure {
            tool: AgenticTool::StructureProposalList.name(),
            detail: e.to_string(),
        }
    })?;
    let snippets: Vec<ProposalListSnippet> = rows
        .into_iter()
        .map(|row| ProposalListSnippet {
            proposal_id: row.proposal_id,
            kind: row.kind,
            status: row.status.as_str().to_owned(),
            emitted_at: row.emitted_at,
            expires_at: row.expires_at,
            context_summary: truncate_string(&row.context.to_string(), CONTEXT_SUMMARY_CHARS),
        })
        .collect();
    serialise_result("proposals", &snippets)
}

#[derive(Debug, Deserialize)]
struct ProposalIdArgs {
    proposal_id: String,
}

async fn dispatch_proposal_get(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: ProposalIdArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::StructureProposalGet.name(),
            detail: e.to_string(),
        }
    })?;
    let rows = proposals::list(
        ctx.pool,
        &ListFilters {
            status: None,
            kind: None,
            // Scoped to the operator's own proposals unless admin reveal is on.
            recipient: proposal_recipient_scope(ctx),
            top_k: Some(MAX_PROPOSAL_LIST_SIZE),
        },
    )
    .await
    .map_err(|e| AgenticToolError::InternalFailure {
        tool: AgenticTool::StructureProposalGet.name(),
        detail: e.to_string(),
    })?;
    let row = rows
        .into_iter()
        .find(|r| r.proposal_id == args.proposal_id)
        .ok_or_else(|| AgenticToolError::InvalidArguments {
            tool: AgenticTool::StructureProposalGet.name(),
            detail: format!("proposal `{}` not found", args.proposal_id),
        })?;
    let payload = json!({
        "proposal_id": row.proposal_id,
        "kind": row.kind,
        "status": row.status.as_str(),
        "context": row.context,
        "questions": row.questions,
        "emitted_at": row.emitted_at,
        "expires_at": row.expires_at,
        "applied_at": row.applied_at,
        "applied_by": row.applied_by,
        "revert_deadline": row.revert_deadline,
    });
    Ok(payload.to_string())
}

#[derive(Debug, Deserialize)]
struct ProposalApplyArgs {
    proposal_id: String,
    answers: serde_json::Value,
}

async fn dispatch_proposal_apply(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: ProposalApplyArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::StructureProposalApply.name(),
            detail: e.to_string(),
        }
    })?;
    // No proposal kind needs an LLM at apply time today (the only one
    // that did — `wiki_type_forge` — has been removed).
    match proposals::apply_proposal(
        ctx.pool,
        ctx.tree,
        &args.proposal_id,
        &args.answers,
        Some(ctx.sender_ctx.sender_id.as_str()),
        ctx.is_admin,
    )
    .await
    {
        Ok(outcome) => {
            let payload = json!({
                "applied": {
                    "proposal_id": outcome.proposal_id,
                    "kind": outcome.kind,
                    "applied_at": outcome.applied_at,
                    "applied_by": outcome.applied_by,
                    "revert_token": outcome.revert_token,
                    "revert_deadline": outcome.revert_deadline,
                }
            });
            Ok(payload.to_string())
        },
        Err(e) => Err(AgenticToolError::InternalFailure {
            tool: AgenticTool::StructureProposalApply.name(),
            detail: e.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Batch-fact pair: wiki_facts_for + wiki_forget
// ---------------------------------------------------------------------------

/// Hard ceiling on `wiki_facts_for` results, beyond the user-supplied
/// `limit`. Keeps the JSON result small enough for chat-panel rendering.
const MAX_FACTS_FOR_RESULTS: usize = 100;
/// Default `limit` when the model does not pass one.
const DEFAULT_FACTS_FOR_LIMIT: usize = 50;

#[derive(Debug, Default, Deserialize)]
struct FactsForArgs {
    #[serde(default)]
    wiki_id: Option<String>,
    #[serde(default)]
    fact_type: Option<String>,
    #[serde(default)]
    topics_any: Vec<String>,
    #[serde(default)]
    created_after: Option<String>,
    #[serde(default)]
    created_before: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct FactSnippet {
    fact_id: String,
    wiki_id: String,
    source_path: String,
    body: String,
    owner: String,
    fact_type: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct WikiGetFactArgs {
    fact_id: String,
}

/// One fact looked up by id, with enough status to drive the next
/// action. Mirrors [`FactSnippet`] plus the supersede / tombstone
/// timestamps so the model can see whether the fact is still active
/// before proposing a write against it.
#[derive(Debug, Serialize)]
struct FactDetail {
    fact_id: String,
    wiki_id: String,
    source_path: String,
    body: String,
    owner: String,
    fact_type: Option<String>,
    topics: Vec<String>,
    created_at: String,
    updated_at: String,
    /// Set when a correction has superseded this fact.
    superseded_at: Option<String>,
    /// Set when the fact has been tombstoned (forgotten).
    deleted_at: Option<String>,
}

/// Look up a single fact by exact id, ACL-projected. The verify-before-act
/// read tool: `wiki_facts_for` has no `fact_id` filter, so without this
/// the model improvises (e.g. `wiki_facts_for(limit=1)`) and acts on the
/// wrong fact. An absent or unreadable id returns `{"found": false}` — it
/// leaks neither content nor existence.
async fn dispatch_wiki_get_fact(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: WikiGetFactArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiGetFact.name(),
            detail: e.to_string(),
        }
    })?;
    let fact_id = FactId::parse(&args.fact_id).map_err(|e| AgenticToolError::InvalidArguments {
        tool: AgenticTool::WikiGetFact.name(),
        detail: format!("invalid fact_id `{}`: {e}", args.fact_id),
    })?;
    let row = mwe_core::fact_index::find_by_id(ctx.pool, &fact_id)
        .await
        .map_err(|e| AgenticToolError::InternalFailure {
            tool: AgenticTool::WikiGetFact.name(),
            detail: e.to_string(),
        })?;
    // ACL invariant (per-fragment ACL): never reveal a fact the operator may
    // not read. An unreadable — or absent — id is reported as plain "not
    // found". Admins bypass, like every other ACL-projected read here.
    let visible = row.as_ref().is_some_and(|r| {
        ctx.is_admin
            || mwe_core::acl::can_read(
                &mwe_core::types::Acl {
                    owner: Some(r.owner_id.clone()),
                    allow: r.allow_ids.clone(),
                },
                &ctx.sender_ctx.sender_id,
                &ctx.sender_ctx.sender_groups,
                r.sender_id.as_ref(),
            )
    });
    let Some(row) = row.filter(|_| visible) else {
        return serialise_result("found", &false);
    };
    let detail = FactDetail {
        fact_id: row.fact_id.as_str().to_owned(),
        wiki_id: row.wiki_id,
        source_path: row.source_path,
        body: row.text,
        owner: row.owner_id.to_string(),
        fact_type: row.fact_type,
        topics: row.topics,
        created_at: row.created_at,
        updated_at: row.updated_at,
        superseded_at: row.superseded_at,
        deleted_at: row.deleted_at,
    };
    serialise_result("fact", &detail)
}

async fn dispatch_wiki_facts_for(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: FactsForArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiFactsFor.name(),
            detail: e.to_string(),
        }
    })?;
    let limit = args
        .limit
        .unwrap_or(DEFAULT_FACTS_FOR_LIMIT)
        .min(MAX_FACTS_FOR_RESULTS);
    let filters = FactFilters {
        wiki_id: args.wiki_id,
        owner_id: None,
        fact_type: args.fact_type,
        created_after: args.created_after,
        created_before: args.created_before,
        topics_any: args.topics_any,
        valid_at: None,
        limit,
        ..Default::default()
    };
    let hits = recall::wiki_facts_for(ctx.pool, filters, &ctx.sender_ctx)
        .await
        .map_err(|e| AgenticToolError::InternalFailure {
            tool: AgenticTool::WikiFactsFor.name(),
            detail: e.to_string(),
        })?;
    let snippets: Vec<FactSnippet> = hits
        .into_iter()
        .map(|h| FactSnippet {
            fact_id: h.fact_id.as_str().to_owned(),
            wiki_id: h.wiki_id,
            source_path: h.source_path,
            body: h.text,
            owner: h.owner_id.to_string(),
            fact_type: h.fact_type,
            created_at: h.created_at,
        })
        .collect();
    serialise_result("facts", &snippets)
}

/// Refuse a standard-wiki management verb whose target wiki is **smart**.
///
/// Smart wikis are the consumer's — governed at the wiki level and written
/// through `wiki_admin_*`; the operator's touchpoint for one is its briefing,
/// so the chat's per-fact / structure verbs must not manage them. The guard
/// keys on the wiki's `smart` meta flag: smart section rows carry
/// `sender_id = NULL` and owner = the scope principal, so no sender/owner
/// gate would catch them.
fn ensure_standard_wiki(
    ctx: &AgenticContext<'_>,
    tool: &'static str,
    wiki_id: &str,
) -> Result<(), AgenticToolError> {
    let id = WikiId::parse(wiki_id).map_err(|e| AgenticToolError::InternalFailure {
        tool,
        detail: format!("wiki_id `{wiki_id}` failed to parse: {e}"),
    })?;
    let handle = match ctx.tree.locate(&id) {
        Ok(h) => h,
        // A row can outlive its wiki directory: a wiki absent from the tree
        // cannot carry a `smart` flag, so pass through and let the underlying
        // engine produce its canonical error for the missing wiki.
        Err(mwe_core::wiki::WikiError::WikiNotFound { .. }) => return Ok(()),
        Err(e) => {
            return Err(AgenticToolError::InternalFailure {
                tool,
                detail: e.to_string(),
            });
        },
    };
    if handle.meta().smart {
        return Err(AgenticToolError::InvalidArguments {
            tool,
            detail: format!(
                "wiki `{wiki_id}` is smart — smart wikis are the consumer's, governed at the \
                 wiki level; the operator's touchpoint for a smart wiki is its briefing"
            ),
        });
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct WikiForgetArgs {
    fact_id: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct WikiForgetReport {
    fact_id: String,
    tombstoned: bool,
    reason: String,
}

async fn dispatch_wiki_forget(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: WikiForgetArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiForget.name(),
            detail: e.to_string(),
        }
    })?;
    let fact_id = FactId::parse(&args.fact_id).map_err(|e| AgenticToolError::InvalidArguments {
        tool: AgenticTool::WikiForget.name(),
        detail: format!("invalid fact_id `{}`: {e}", args.fact_id),
    })?;
    // Sender-direct authority (the write-authority model): only the fact's author
    // (its `sender`) forgets it directly; an admin may forget any fact. A
    // non-sender owner's path is `wiki_request_forget` (a request → audience
    // vote), so refuse here and point them there.
    let row = mwe_core::fact_index::find_by_id(ctx.pool, &fact_id)
        .await
        .map_err(|e| AgenticToolError::InternalFailure {
            tool: AgenticTool::WikiForget.name(),
            detail: e.to_string(),
        })?
        .ok_or_else(|| AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiForget.name(),
            detail: format!("fact_id `{}` not found", args.fact_id),
        })?;
    // Smart section rows are not fact-governed (the consumer's next push would
    // undo the tombstone) — refuse before the sender gate, which cannot catch
    // a smart row (its sender is NULL).
    ensure_standard_wiki(ctx, AgenticTool::WikiForget.name(), &row.wiki_id)?;
    if !mwe_core::acl::can_delete(
        row.sender_id.as_ref(),
        &ctx.sender_ctx.sender_id,
        ctx.is_admin,
    ) {
        return Err(AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiForget.name(),
            detail: "only the fact's author (its sender) can forget it directly; you are not \
                     its sender — if it is about you (or your group), use `wiki_request_forget` \
                     to put it to the fact's audience"
                .to_owned(),
        });
    }
    let outcome = capture::wiki_forget(
        ctx.tree,
        ctx.pool,
        ctx.embedder.clone(),
        &fact_id,
        &args.reason,
    )
    .await
    .map_err(|e| AgenticToolError::InternalFailure {
        tool: AgenticTool::WikiForget.name(),
        detail: e.to_string(),
    })?;
    let report = WikiForgetReport {
        fact_id: args.fact_id,
        tombstoned: outcome.tombstoned,
        reason: args.reason,
    };
    serialise_result("forgot", &report)
}

// ---------------------------------------------------------------------------
// Single-fact correction: wiki_supersede
//
// Inherits owner/ACL/fact_type/topics from the targeted fact; the chat
// only needs to surface a fact_id (via wiki_recall) and a corrected body.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WikiSupersedeArgs {
    old_fact_id: String,
    new_body: String,
}

#[derive(Debug, Serialize)]
struct WikiSupersedeReport {
    old_fact_id: String,
    new_fact_id: String,
    source_path: String,
    region_start: i64,
    region_end: i64,
}

async fn dispatch_wiki_supersede(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: WikiSupersedeArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiSupersede.name(),
            detail: e.to_string(),
        }
    })?;
    let old_fact_id =
        FactId::parse(&args.old_fact_id).map_err(|e| AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiSupersede.name(),
            detail: format!("invalid old_fact_id `{}`: {e}", args.old_fact_id),
        })?;
    let old_row = mwe_core::fact_index::find_by_id(ctx.pool, &old_fact_id)
        .await
        .map_err(|e| AgenticToolError::InternalFailure {
            tool: AgenticTool::WikiSupersede.name(),
            detail: e.to_string(),
        })?
        .ok_or_else(|| AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiSupersede.name(),
            detail: format!("fact_id `{}` not found", args.old_fact_id),
        })?;
    if old_row.superseded_at.is_some() || old_row.deleted_at.is_some() {
        return Err(AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiSupersede.name(),
            detail: format!(
                "fact_id `{}` is already superseded or tombstoned",
                args.old_fact_id
            ),
        });
    }
    // Superseding a smart section row would write a marker-wrapped fact into
    // the consumer's plain-markdown page — refuse before the owner gate (a
    // smart row's owner is the scope principal, which the operator may match).
    ensure_standard_wiki(ctx, AgenticTool::WikiSupersede.name(), &old_row.wiki_id)?;
    // Editing the CONTENT of a fact is an *update* — the subject's act (the
    // write-authority model): the **owner** (subject, or a member of an
    // owning group) or an admin may supersede it directly, the same owner axis the
    // ingest supersede guards (`SupersedeCrossOwner`) and the validity closure
    // use. Only *destroying* a fact keys on `sender` / a vote, not updating it.
    if !(mwe_core::acl::sender_owns(
        &old_row.owner_id,
        &ctx.sender_ctx.sender_id,
        &ctx.sender_ctx.sender_groups,
    ) || ctx.is_admin)
    {
        return Err(AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiSupersede.name(),
            detail: "only the fact's owner (its subject, or an owning-group member) or an \
                     admin can edit it; you are not its owner"
                .to_owned(),
        });
    }
    let req = build_supersede_request(&old_row, args.new_body, ctx)?;
    let outcome = capture::wiki_supersede(
        ctx.tree,
        ctx.pool,
        Arc::clone(&ctx.embedder),
        &old_fact_id,
        req,
    )
    .await
    .map_err(|e| AgenticToolError::InternalFailure {
        tool: AgenticTool::WikiSupersede.name(),
        detail: e.to_string(),
    })?;
    let (source_path, region_start, region_end) = match outcome.action {
        capture::CaptureAction::Superseded {
            source_path,
            region_start,
            region_end,
            ..
        }
        | capture::CaptureAction::Captured {
            source_path,
            region_start,
            region_end,
        } => (source_path, region_start, region_end),
        capture::CaptureAction::Skipped {
            matched_fact_id,
            similarity,
        } => {
            return Err(AgenticToolError::InternalFailure {
                tool: AgenticTool::WikiSupersede.name(),
                detail: format!(
                    "supersede unexpectedly hit dedup (matched {matched_fact_id}, similarity {similarity})",
                ),
            });
        },
    };
    let report = WikiSupersedeReport {
        old_fact_id: args.old_fact_id,
        new_fact_id: outcome.fact_id.as_str().to_owned(),
        source_path,
        region_start,
        region_end,
    };
    serialise_result("superseded", &report)
}

/// Project a stored [`mwe_core::fact_index::FactIndexRow`] back to the
/// [`capture::CaptureRequest`] shape `wiki_supersede` expects, copying
/// owner / ACL / `fact_type` / topics / validity window across so the
/// chat does not have to re-elicit them and a correction never mutates
/// metadata. `new_body` is the corrected prose from the user;
/// the page path is recovered by stripping the wiki's relative
/// directory from the stored `source_path` (which is always POSIX-style
/// per `workdir_relative_source_path`).
fn build_supersede_request(
    old_row: &mwe_core::fact_index::FactIndexRow,
    new_body: String,
    ctx: &AgenticContext<'_>,
) -> Result<capture::CaptureRequest, AgenticToolError> {
    let wiki_id =
        WikiId::parse(&old_row.wiki_id).map_err(|e| AgenticToolError::InternalFailure {
            tool: AgenticTool::WikiSupersede.name(),
            detail: format!("stored wiki_id `{}` failed to parse: {e}", old_row.wiki_id),
        })?;
    let handle = ctx
        .tree
        .locate(&wiki_id)
        .map_err(|e| AgenticToolError::InternalFailure {
            tool: AgenticTool::WikiSupersede.name(),
            detail: e.to_string(),
        })?;
    let rel_dir_posix = handle.rel_dir().to_string_lossy().replace('\\', "/");
    let page_str = old_row
        .source_path
        .strip_prefix(&format!("{rel_dir_posix}/"))
        .ok_or_else(|| AgenticToolError::InternalFailure {
            tool: AgenticTool::WikiSupersede.name(),
            detail: format!(
                "source_path `{}` does not start with wiki rel_dir `{rel_dir_posix}/`",
                old_row.source_path,
            ),
        })?;
    Ok(capture::CaptureRequest {
        authored_refs: Vec::new(),
        wiki_id,
        page: std::path::PathBuf::from(page_str),
        body: new_body,
        owner: old_row.owner_id.clone(),
        allow: old_row.allow_ids.clone(),
        sender: old_row.sender_id.clone(),
        fact_type: old_row.fact_type.clone(),
        topics: old_row.topics.clone(),
        // Supersede is intentional — disable dedup so an identical
        // body is never silently dropped on the floor.
        dedup_threshold: Some(1.01),
        // The validity window is metadata like owner/topics: a body
        // correction must not silently reopen (or close) the claim.
        valid_from: old_row.valid_from.clone(),
        valid_to: old_row.valid_to.clone(),
        // Dashboard supersede carries no placement hints (the operator
        // edits an existing fact in place).
        style: None,
        page_description: None,
        salience: None,
    })
}

// ---------------------------------------------------------------------------
// Hierarchical wiki move: wiki_change_scope
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WikiChangeScopeArgs {
    source_wiki_id: String,
    #[serde(default)]
    new_parent_wiki_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WikiChangeScopeReport {
    wiki_id: String,
    old_rel_dir: String,
    new_rel_dir: String,
    new_parent_wiki_id: Option<String>,
    facts_rebased: u64,
}

async fn dispatch_wiki_change_scope(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: WikiChangeScopeArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiChangeScope.name(),
            detail: e.to_string(),
        }
    })?;
    let source_id =
        WikiId::parse(&args.source_wiki_id).map_err(|e| AgenticToolError::InvalidArguments {
            tool: AgenticTool::WikiChangeScope.name(),
            detail: format!("invalid source_wiki_id `{}`: {e}", args.source_wiki_id),
        })?;
    let new_parent_id = match args.new_parent_wiki_id.as_deref() {
        None | Some("") => None,
        Some(raw) => Some(
            WikiId::parse(raw).map_err(|e| AgenticToolError::InvalidArguments {
                tool: AgenticTool::WikiChangeScope.name(),
                detail: format!("invalid new_parent_wiki_id `{raw}`: {e}"),
            })?,
        ),
    };
    // A smart wiki's wiki-level read audience derives from its position in the
    // tree (the scope principal), so a re-parent would change effective read
    // access on the next reindex/push. The core primitive carries the same
    // guard; refusing here keeps the message operator-readable.
    ensure_standard_wiki(ctx, AgenticTool::WikiChangeScope.name(), source_id.as_str())?;
    let outcome =
        mwe_core::scope::wiki_change_scope(ctx.tree, ctx.pool, &source_id, new_parent_id.as_ref())
            .await
            .map_err(|e| AgenticToolError::InternalFailure {
                tool: AgenticTool::WikiChangeScope.name(),
                detail: e.to_string(),
            })?;
    let report = WikiChangeScopeReport {
        wiki_id: outcome.wiki_id.as_str().to_owned(),
        old_rel_dir: outcome.old_rel_dir.to_string_lossy().into_owned(),
        new_rel_dir: outcome.new_rel_dir.to_string_lossy().into_owned(),
        new_parent_wiki_id: outcome.new_parent_wiki_id.map(|id| id.as_str().to_owned()),
        facts_rebased: outcome.facts_rebased,
    };
    serialise_result("scope_changed", &report)
}

// ---------------------------------------------------------------------------
// Single-fact move: wiki_move_fact
//
// Relocate ONE fact on the operator's instruction — same-wiki page→page or
// cross-wiki. Reuses the same act-first engine the REM cross-wiki refile sweep
// and the comment-apply `move` op use, so the dashboard, the dream, and the
// chat all mint the same born-applied + revertible receipt. Admin-only (a move
// re-categorises — it neither destroys the fact nor changes its visibility — so
// it is the operator's / REM's act, not the owner's); a smart source or
// destination is refused (smart wikis carry wiki-level governance — moving facts
// across that boundary corrupts it).
// ---------------------------------------------------------------------------

/// Landing page for a cross-wiki move — always the destination wiki's
/// `index.md`, because the compilation plan keys pages by bare slug
/// forest-wide and a named cross-wiki page would collide; the dest wiki
/// re-homes the fact on its next compile.
const MOVE_FACT_CROSS_WIKI_DEST_PAGE: &str = "index.md";

#[derive(Debug, Deserialize)]
struct WikiMoveFactArgs {
    fact_id: String,
    #[serde(default)]
    dest_wiki_id: Option<String>,
    #[serde(default)]
    dest_page: Option<String>,
}

#[derive(Debug, Serialize)]
struct WikiMoveFactReport {
    fact_id: String,
    /// Born-applied receipt id — the undo anchor (revert from the dashboard).
    proposal_id: String,
    dest_wiki_id: String,
    dest_page: String,
    cross_wiki: bool,
}

/// Load the fact named by `wiki_move_fact`, gate it to the session, and resolve
/// its source wiki + wiki-relative page. Refuses: a missing / already
/// superseded-or-tombstoned fact, a **non-admin** operator (a move is admin-only
/// structure authority — [`enforce_move_admin`]), and a **smart** source wiki (a
/// single-fact move is a standard-wiki operation). Returns
/// `(row, source_wiki_id, source_page)`.
async fn load_and_gate_movable_fact(
    ctx: &AgenticContext<'_>,
    fact_id: &FactId,
) -> Result<(mwe_core::fact_index::FactIndexRow, WikiId, String), AgenticToolError> {
    let tool = AgenticTool::WikiMoveFact.name();
    let row = mwe_core::fact_index::find_by_id(ctx.pool, fact_id)
        .await
        .map_err(|e| AgenticToolError::InternalFailure {
            tool,
            detail: e.to_string(),
        })?
        .ok_or_else(|| AgenticToolError::InvalidArguments {
            tool,
            detail: format!("fact_id `{fact_id}` not found"),
        })?;
    if row.superseded_at.is_some() || row.deleted_at.is_some() {
        return Err(AgenticToolError::InvalidArguments {
            tool,
            detail: format!("fact_id `{fact_id}` is already superseded or tombstoned"),
        });
    }
    enforce_move_admin(ctx, tool)?;

    let source_wiki_id =
        WikiId::parse(&row.wiki_id).map_err(|e| AgenticToolError::InternalFailure {
            tool,
            detail: format!("stored wiki_id `{}` failed to parse: {e}", row.wiki_id),
        })?;
    let source_handle =
        ctx.tree
            .locate(&source_wiki_id)
            .map_err(|e| AgenticToolError::InternalFailure {
                tool,
                detail: e.to_string(),
            })?;
    if source_handle.meta().smart {
        return Err(AgenticToolError::InvalidArguments {
            tool,
            detail: format!(
                "wiki `{}` is smart: its governance is wiki-level, not per-fragment — \
                 a single-fact move is a standard-wiki operation",
                row.wiki_id
            ),
        });
    }
    let source_page = move_fact_wiki_relative_page(&source_handle, &row.source_path);
    Ok((row, source_wiki_id, source_page))
}

async fn dispatch_wiki_move_fact(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let tool = AgenticTool::WikiMoveFact.name();
    let args: WikiMoveFactArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool,
            detail: e.to_string(),
        }
    })?;
    let fact_id = FactId::parse(&args.fact_id).map_err(|e| AgenticToolError::InvalidArguments {
        tool,
        detail: format!("invalid fact_id `{}`: {e}", args.fact_id),
    })?;

    // Load + gate the fact + resolve its (standard) source wiki/page.
    let (row, source_wiki_id, source_page) = load_and_gate_movable_fact(ctx, &fact_id).await?;
    let recipient = proposals::recipient_from_fact(&row.owner_id, row.sender_id.as_ref());
    let reason = "dashboard chat: operator move";

    // Cross-wiki when a dest wiki is named AND differs from the source;
    // otherwise a same-wiki page move.
    let cross_wiki_dest = args
        .dest_wiki_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != row.wiki_id);

    let (applied, dest_wiki_id, dest_page, cross_wiki) = if let Some(dest) = cross_wiki_dest {
        let applied = move_fact_cross_wiki(
            ctx,
            &fact_id,
            &source_wiki_id,
            &source_page,
            dest,
            recipient,
            reason,
        )
        .await?;
        (
            applied,
            dest.to_owned(),
            MOVE_FACT_CROSS_WIKI_DEST_PAGE.to_owned(),
            true,
        )
    } else {
        let dest_page = args.dest_page.as_deref().map_or("", str::trim);
        let applied = move_fact_same_wiki(
            ctx,
            &fact_id,
            &source_wiki_id,
            &source_page,
            dest_page,
            recipient,
            reason,
        )
        .await?;
        (applied, row.wiki_id.clone(), dest_page.to_owned(), false)
    };

    let report = WikiMoveFactReport {
        fact_id: args.fact_id,
        proposal_id: applied.proposal_id,
        dest_wiki_id,
        dest_page,
        cross_wiki,
    };
    serialise_result("moved", &report)
}

#[derive(Debug, Deserialize)]
struct WikiDeletePageArgs {
    wiki_id: String,
    page: String,
    /// Admin-only (the governed delete-page path —
    /// [agentic-chat.md](../../../wiki/design-notes/agentic-chat.md)):
    /// tombstone **every** fact, including ones the
    /// admin did not author, with no evacuation. Refused for a non-admin.
    #[serde(default)]
    delete_all_facts: bool,
}

#[derive(Debug, Serialize)]
struct WikiDeletePageReport {
    wiki_id: String,
    page: String,
    facts_tombstoned: u64,
    facts_evacuated: u64,
    /// The single born-applied `bundle` receipt wrapping the whole deletion,
    /// for `structure_proposal_revert`. `None` when the page had no facts.
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle_proposal_id: Option<String>,
}

/// Delete a page: tombstone the operator's own facts, evacuate foreign-authored
/// ones to their senders' wikis (the governed delete-page path —
/// [agentic-chat.md](../../../wiki/design-notes/agentic-chat.md)). Two-level authority — the
/// operator must be owner-equivalent on the wiki (or admin); the per-fact sender
/// axis governs each fact inside. Smart wikis are refused.
async fn dispatch_wiki_delete_page(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let tool = AgenticTool::WikiDeletePage.name();
    let args: WikiDeletePageArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool,
            detail: e.to_string(),
        }
    })?;
    let wiki_id = WikiId::parse(&args.wiki_id).map_err(|e| AgenticToolError::InvalidArguments {
        tool,
        detail: format!("invalid wiki_id `{}`: {e}", args.wiki_id),
    })?;
    let handle = ctx
        .tree
        .locate(&wiki_id)
        .map_err(|e| AgenticToolError::InvalidArguments {
            tool,
            detail: format!("wiki `{}` not found: {e}", args.wiki_id),
        })?;

    // Smart wikis are governed at the wiki level (single proprietor), not by
    // per-fragment evacuation — refuse here.
    if handle.meta().smart {
        return Err(AgenticToolError::InvalidArguments {
            tool,
            detail: "cannot delete a smart wiki's page here — smart wikis are governed at the wiki level"
                .to_owned(),
        });
    }

    // Deleting structure (a page or a whole wiki) is an admin act: under "ACL
    // lives only in the fact" the wiki/page is recall structure, not access, so
    // only the operator restructures it. The per-fact sender axis still governs
    // how each fact on the page is disposed (the move / tombstone modes).
    if !ctx.is_admin {
        return Err(AgenticToolError::InvalidArguments {
            tool,
            detail: "deleting a page is admin-only (structure is the operator's to change)"
                .to_owned(),
        });
    }
    let policy = mwe_core::page::DeletionPolicy {
        mode: if args.delete_all_facts {
            mwe_core::page::DeletionMode::TombstoneAll
        } else {
            mwe_core::page::DeletionMode::SenderKeyed
        },
    };

    let deleter = mwe_core::types::Principal::User(ctx.sender_ctx.sender_id.clone());
    let outcome = mwe_core::page::delete_page_direct(
        ctx.pool,
        ctx.tree,
        &wiki_id,
        &args.page,
        &deleter,
        "dashboard chat: operator delete page",
        policy,
    )
    .await
    .map_err(|e| AgenticToolError::InternalFailure {
        tool,
        detail: e.to_string(),
    })?;

    let report = WikiDeletePageReport {
        wiki_id: args.wiki_id,
        page: args.page,
        facts_tombstoned: outcome.facts_tombstoned,
        facts_evacuated: outcome.facts_evacuated,
        bundle_proposal_id: outcome.bundle_proposal_id,
    };
    serialise_result("deleted", &report)
}

/// Cross-wiki branch of [`dispatch_wiki_move_fact`]: validate the destination
/// wiki (locates, not smart), then refile the fact onto its `index.md`.
#[allow(
    clippy::too_many_arguments,
    reason = "the cross-wiki branch carries the ctx, fact, source wiki/page, dest wiki, recipient, and reason"
)]
async fn move_fact_cross_wiki(
    ctx: &AgenticContext<'_>,
    fact_id: &FactId,
    source_wiki_id: &WikiId,
    source_page: &str,
    dest: &str,
    recipient: Option<String>,
    reason: &str,
) -> Result<mwe_core::promote::DirectApplied, AgenticToolError> {
    let tool = AgenticTool::WikiMoveFact.name();
    let dest_id = WikiId::parse(dest).map_err(|e| AgenticToolError::InvalidArguments {
        tool,
        detail: format!("invalid dest_wiki_id `{dest}`: {e}"),
    })?;
    let dest_handle =
        ctx.tree
            .locate(&dest_id)
            .map_err(|_| AgenticToolError::InvalidArguments {
                tool,
                detail: format!("destination wiki `{dest}` not found"),
            })?;
    if dest_handle.meta().smart {
        return Err(AgenticToolError::InvalidArguments {
            tool,
            detail: format!(
                "destination wiki `{dest}` is smart (the consumer's container) — refused"
            ),
        });
    }
    promote::apply_fact_refile_direct(
        ctx.pool,
        ctx.tree,
        fact_id,
        source_wiki_id.as_str(),
        source_page,
        dest,
        MOVE_FACT_CROSS_WIKI_DEST_PAGE,
        Some(reason),
        recipient,
    )
    .await
    .map_err(|e| map_direct_promote_error(&e, tool))
}

/// Same-wiki branch of [`dispatch_wiki_move_fact`]: move the fact to another
/// page of the same wiki. Refuses an empty / unchanged destination page.
#[allow(
    clippy::too_many_arguments,
    reason = "the same-wiki branch carries the ctx, fact, source wiki/page, dest page, recipient, and reason"
)]
async fn move_fact_same_wiki(
    ctx: &AgenticContext<'_>,
    fact_id: &FactId,
    source_wiki_id: &WikiId,
    source_page: &str,
    dest_page: &str,
    recipient: Option<String>,
    reason: &str,
) -> Result<mwe_core::promote::DirectApplied, AgenticToolError> {
    let tool = AgenticTool::WikiMoveFact.name();
    if dest_page.is_empty() || dest_page == source_page {
        return Err(AgenticToolError::InvalidArguments {
            tool,
            detail: "no destination given: pass dest_page for a same-wiki move, \
                     or dest_wiki_id for a cross-wiki move"
                .to_owned(),
        });
    }
    let hints = mwe_core::promote::ParagraphToFileHints {
        reason: Some(reason.to_owned()),
        ..mwe_core::promote::ParagraphToFileHints::default()
    };
    promote::apply_paragraph_to_file_direct(
        ctx.pool,
        ctx.tree,
        source_wiki_id.as_str(),
        source_page,
        std::slice::from_ref(fact_id),
        dest_page,
        &hints,
        recipient,
    )
    .await
    .map_err(|e| map_direct_promote_error(&e, tool))
}

/// The fact's page path relative to its wiki directory (what the
/// `paragraph_to_file` / `fact_refile` engines expect), derived from the
/// workdir-relative `source_path` by stripping the wiki's `rel_dir` prefix.
fn move_fact_wiki_relative_page(handle: &mwe_core::wiki::WikiHandle, source_path: &str) -> String {
    let rel_dir = handle.rel_dir().to_string_lossy().replace('\\', "/");
    let prefix = format!("{rel_dir}/");
    source_path
        .strip_prefix(&prefix)
        .unwrap_or(source_path)
        .to_owned()
}

/// Admin-only gate for the single-fact move. Re-categorising a fact neither
/// destroys it nor changes its visibility, so it is the operator's (admin's)
/// act — and REM's, server-side — never the per-fact owner's (the structure
/// authority of [the write-authority model](../../../wiki/concepts/identity-and-acl.md)). The
/// owner / sender axes gate `delete` / `edit` / `acl_change`, not `move`.
fn enforce_move_admin(
    ctx: &AgenticContext<'_>,
    tool: &'static str,
) -> Result<(), AgenticToolError> {
    if ctx.is_admin {
        Ok(())
    } else {
        Err(AgenticToolError::InvalidArguments {
            tool,
            detail: "moving a fact is admin-only (re-categorising structure is the operator's to \
                     change, not the owner's)"
                .to_owned(),
        })
    }
}

/// Map a [`mwe_core::promote::DirectPromoteError`] onto an
/// [`AgenticToolError`]: an apply refusal (the fact moved since it was
/// surfaced, a bad path, a smart-wiki guard in the engine) is user-actionable
/// (`InvalidArguments` — the model relays it); a receipt-write miss means the
/// move stands but the undo row failed, which is infrastructure
/// (`InternalFailure`).
fn map_direct_promote_error(
    e: &mwe_core::promote::DirectPromoteError,
    tool: &'static str,
) -> AgenticToolError {
    match e {
        mwe_core::promote::DirectPromoteError::Apply(_) => AgenticToolError::InvalidArguments {
            tool,
            detail: e.to_string(),
        },
        mwe_core::promote::DirectPromoteError::Receipt(_) => AgenticToolError::InternalFailure {
            tool,
            detail: e.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// Structure-proposal revert
//
// The inverse of `structure_proposal_apply`. No new revert logic lives
// here: this dispatcher selects the `RevertAuth` from the row's status
// — exactly as the form route `routes::proposals::revert` does — and
// hands off to the `mwe_core::proposals::revert_proposal` chassis, which
// dispatches to the per-kind inverse (auto-promotion, dedup merge, type
// forge).
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ProposalRevertReport {
    proposal_id: String,
    kind: String,
    reverted_at: String,
    /// The revertable status the row was in before the flip
    /// (`applied` or `applied_pending_confirm`) — lets the chat render
    /// "we undid your apply" vs "we undid the auto-apply we did for you".
    prior_status: String,
}

async fn dispatch_proposal_revert(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: ProposalIdArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::StructureProposalRevert.name(),
            detail: e.to_string(),
        }
    })?;
    // Look up status + token + recipient to (a) gate non-admins to the
    // addressee — keeping revert consistent with the recipient scoping
    // `structure_proposal_list` / `_get` already enforce, so it is not a
    // bypass — and (b) pick the RevertAuth path the chassis expects.
    let (status, revert_token, recipient_id) = fetch_revert_row(ctx, &args.proposal_id).await?;

    // 0032 recipient gate: a non-admin may only revert a proposal
    // addressed to them (or an unaddressed one); admins always pass.
    if !proposals::recipient_can_act(
        recipient_id.as_deref(),
        &ctx.sender_ctx.sender_id,
        ctx.is_admin,
    ) {
        return Err(AgenticToolError::InvalidArguments {
            tool: AgenticTool::StructureProposalRevert.name(),
            detail: format!(
                "proposal `{}` is not addressed to you — only its recipient or an admin can revert it",
                args.proposal_id
            ),
        });
    }

    // Status → RevertAuth, mirroring routes::proposals::revert:
    //   applied                 → Token (fetched server-side)
    //   applied_pending_confirm → Caller { sender, is_admin }
    //   anything else           → not revertable.
    let auth = match status.as_str() {
        "applied" => {
            let token =
                revert_token
                    .as_deref()
                    .ok_or_else(|| AgenticToolError::InternalFailure {
                        tool: AgenticTool::StructureProposalRevert.name(),
                        detail: format!(
                            "proposal `{}` is applied but has no revert token on disk",
                            args.proposal_id
                        ),
                    })?;
            proposals::RevertAuth::Token(token)
        },
        "applied_pending_confirm" => proposals::RevertAuth::Caller {
            sender: &ctx.sender_ctx.sender_id,
            is_admin: ctx.is_admin,
        },
        other => {
            return Err(AgenticToolError::InvalidArguments {
                tool: AgenticTool::StructureProposalRevert.name(),
                detail: format!(
                    "proposal `{}` is not in a revertable status: {other}",
                    args.proposal_id
                ),
            });
        },
    };

    match proposals::revert_proposal(ctx.pool, ctx.tree, &args.proposal_id, auth).await {
        Ok(outcome) => {
            let report = ProposalRevertReport {
                proposal_id: outcome.proposal_id,
                kind: outcome.kind,
                reverted_at: outcome.reverted_at,
                prior_status: outcome.prior_status.as_str().to_owned(),
            };
            serialise_result("reverted", &report)
        },
        Err(e) => Err(map_revert_error(&e)),
    }
}

#[derive(Debug, Deserialize)]
struct WikiRequestForgetArgs {
    fact_id: String,
}

/// Open a forget request for a fact the signed-in user does not author — the
/// non-sender owner's path (the write-authority model).
///
/// Requests **as** `ctx.sender_ctx.sender_id`: the engine enforces that the
/// caller is the fact's owner / an owning-group member (or admin), refuses a
/// sender (who deletes directly via `wiki_forget`), computes the audience
/// electorate, and either opens a pending vote or — when the caller is the only
/// reader — forgets the fact immediately.
async fn dispatch_wiki_request_forget(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let tool = AgenticTool::WikiRequestForget.name();
    let args: WikiRequestForgetArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool,
            detail: e.to_string(),
        }
    })?;
    let fact_id = FactId::parse(&args.fact_id).map_err(|e| AgenticToolError::InvalidArguments {
        tool,
        detail: format!("invalid fact_id `{}`: {e}", args.fact_id),
    })?;
    // Forget votes are per-fact governance; smart governance is wiki-level —
    // refuse a smart target before the vote machinery runs. A missing fact
    // falls through to the engine's canonical FactNotFound.
    if let Some(row) = mwe_core::fact_index::find_by_id(ctx.pool, &fact_id)
        .await
        .map_err(|e| AgenticToolError::InternalFailure {
            tool,
            detail: e.to_string(),
        })?
    {
        ensure_standard_wiki(ctx, tool, &row.wiki_id)?;
    }
    let outcome = mwe_core::votes::open_forget_request(
        ctx.pool,
        ctx.tree,
        &ctx.embedder,
        &fact_id,
        &ctx.sender_ctx.sender_id,
        ctx.is_admin,
    )
    .await
    .map_err(|e| map_forget_request_error(tool, &e))?;

    serialise_result("forget_requested", &outcome)
}

/// Map a [`mwe_core::votes::ForgetRequestError`] to an [`AgenticToolError`]. The
/// caller-actionable refusals (not found, already forgotten, sender-direct,
/// unauthorized) are `InvalidArguments` so the chat relays the reason; DB / emit
/// failures are `InternalFailure`.
fn map_forget_request_error(
    tool: &'static str,
    e: &mwe_core::votes::ForgetRequestError,
) -> AgenticToolError {
    use mwe_core::votes::ForgetRequestError as E;
    match e {
        E::FactNotFound(_)
        | E::AlreadyForgotten(_)
        | E::SenderActsDirectly { .. }
        | E::NotAuthorized { .. } => AgenticToolError::InvalidArguments {
            tool,
            detail: e.to_string(),
        },
        E::Db(_) | E::FactIndex(_) | E::Emit(_) => AgenticToolError::InternalFailure {
            tool,
            detail: e.to_string(),
        },
    }
}

#[derive(Debug, Deserialize)]
struct ProposalVoteArgs {
    proposal_id: String,
    vote: String,
}

/// Cast the signed-in member's vote on a pending `fact_forget` request.
///
/// Votes **as** `ctx.sender_ctx.sender_id` — the verb takes no voter argument,
/// so a member can only ever vote as themselves (the engine refuses anyone
/// outside the request's eligible set, the requester, and a re-vote).
async fn dispatch_proposal_vote(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let tool = AgenticTool::StructureProposalVote.name();
    let args: ProposalVoteArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool,
            detail: e.to_string(),
        }
    })?;
    let vote = match args.vote.as_str() {
        "yes" => mwe_core::votes::Vote::Yes,
        "no" => mwe_core::votes::Vote::No,
        other => {
            return Err(AgenticToolError::InvalidArguments {
                tool,
                detail: format!("vote must be \"yes\" or \"no\", got {other:?}"),
            });
        },
    };

    let outcome = mwe_core::votes::cast_vote(
        ctx.pool,
        ctx.tree,
        &ctx.embedder,
        &args.proposal_id,
        &ctx.sender_ctx.sender_id,
        vote,
    )
    .await
    .map_err(|e| map_vote_error(tool, &e))?;

    serialise_result(
        "voted",
        &json!({ "proposal_id": args.proposal_id, "result": outcome }),
    )
}

/// Map a [`mwe_core::votes::VoteError`] to an [`AgenticToolError`]. The
/// member-actionable refusals (ineligible, closed, already voted, unknown id)
/// are `InvalidArguments` so the chat relays the reason; infrastructure /
/// revert failures are `InternalFailure`.
fn map_vote_error(tool: &'static str, e: &mwe_core::votes::VoteError) -> AgenticToolError {
    use mwe_core::votes::VoteError;
    match e {
        VoteError::NotFound(_)
        | VoteError::NotVotable(_)
        | VoteError::NotEligible { .. }
        | VoteError::Closed(_)
        | VoteError::AlreadyVoted { .. } => AgenticToolError::InvalidArguments {
            tool,
            detail: e.to_string(),
        },
        VoteError::Db(_) | VoteError::Json(_) | VoteError::Apply(_) => {
            AgenticToolError::InternalFailure {
                tool,
                detail: e.to_string(),
            }
        },
    }
}

/// Fetch `(status, revert_token, recipient_id)` for one proposal row, or
/// surface `InvalidArguments` when the id is unknown. Mirrors the
/// server-side `SELECT` the form route does before choosing a
/// `RevertAuth`.
async fn fetch_revert_row(
    ctx: &AgenticContext<'_>,
    proposal_id: &str,
) -> Result<(String, Option<String>, Option<String>), AgenticToolError> {
    let row: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT status, revert_token, recipient_id FROM structure_proposals WHERE proposal_id = ?",
    )
    .bind(proposal_id)
    .fetch_optional(ctx.pool)
    .await
    .map_err(|e| AgenticToolError::InternalFailure {
        tool: AgenticTool::StructureProposalRevert.name(),
        detail: e.to_string(),
    })?;
    row.ok_or_else(|| AgenticToolError::InvalidArguments {
        tool: AgenticTool::StructureProposalRevert.name(),
        detail: format!("proposal `{proposal_id}` not found"),
    })
}

/// Map a [`proposals::RevertError`] to an [`AgenticToolError`]. The
/// user-actionable / refusal variants (a per-kind guard refusing via
/// `HandlerData`) become `InvalidArguments` so the model relays them to
/// the user as an ordinary refusal; the two infrastructure variants
/// become `InternalFailure`. The `Display` text is used verbatim as the
/// detail.
fn map_revert_error(e: &proposals::RevertError) -> AgenticToolError {
    use proposals::RevertError as R;
    let tool = AgenticTool::StructureProposalRevert.name();
    let detail = e.to_string();
    match e {
        R::Db(_) | R::HandlerIo(_) => AgenticToolError::InternalFailure { tool, detail },
        R::NotFound(_)
        | R::NotRevertable { .. }
        | R::InvalidRevertToken(_)
        | R::RevertWindowClosed { .. }
        | R::ConfirmWindowExpired { .. }
        | R::RevertNotAuthorized { .. }
        | R::HandlerData(_)
        | R::InvalidPayload(_)
        | R::UnknownKind(_)
        | R::KindNotYetImplemented(_) => AgenticToolError::InvalidArguments { tool, detail },
    }
}

// ---------------------------------------------------------------------------
// Structure-proposal confirm
//
// The counterpart of `structure_proposal_revert` on the
// `applied_pending_confirm → applied` edge. No new confirm logic lives
// here: this dispatcher is a thin shim over the
// `mwe_core::proposals::confirm_proposal` chassis, which already gates
// by recipient/admin internally (`ConfirmError::NotAuthorized`) — so,
// unlike the revert dispatcher, it does NOT pre-check the recipient or
// SELECT the row first; it calls the chassis and maps the error. This
// restores the "confirm a sweep auto-apply" action the retired
// pending-confirms form tray carried, now in the chat as the single
// operational surface.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ProposalConfirmReport {
    proposal_id: String,
    kind: String,
    /// ISO 8601 deadline of the freshly opened 7-day revert window — the
    /// confirm mints a `revert_token` and starts the window, so the chat
    /// can tell the user undo is still available until then.
    revert_deadline: String,
    /// `UUIDv4` minted at confirmation; echoed so a later
    /// `structure_proposal_revert` can authorise the undo within the
    /// window (the chat normally fetches it server-side, but surfacing it
    /// keeps parity with `structure_proposal_apply`).
    revert_token: String,
}

async fn dispatch_proposal_confirm(
    arguments: &serde_json::Value,
    ctx: &AgenticContext<'_>,
) -> Result<String, AgenticToolError> {
    let args: ProposalIdArgs = serde_json::from_value(arguments.clone()).map_err(|e| {
        AgenticToolError::InvalidArguments {
            tool: AgenticTool::StructureProposalConfirm.name(),
            detail: e.to_string(),
        }
    })?;
    // `confirm_proposal` gates by recipient/admin itself (returns
    // `ConfirmError::NotAuthorized` for a non-addressee), validates the
    // status is `applied_pending_confirm`, and checks the confirm window.
    // So — unlike the revert dispatcher — we just call it and map errors.
    match proposals::confirm_proposal(
        ctx.pool,
        &args.proposal_id,
        &ctx.sender_ctx.sender_id,
        ctx.is_admin,
    )
    .await
    {
        Ok(outcome) => {
            let report = ProposalConfirmReport {
                proposal_id: outcome.proposal_id,
                kind: outcome.kind,
                revert_deadline: outcome.revert_deadline,
                revert_token: outcome.revert_token,
            };
            serialise_result("confirmed", &report)
        },
        Err(e) => Err(map_confirm_error(&e)),
    }
}

/// Map a [`ConfirmError`] to an [`AgenticToolError`]. The user-actionable
/// variants (unknown id, wrong recipient, not in a confirmable status,
/// expired window) become `InvalidArguments` so the model relays them to
/// the user as an ordinary refusal; the infrastructure variant (`Db`)
/// becomes `InternalFailure`. The `Display` text is used verbatim as the
/// detail.
fn map_confirm_error(e: &ConfirmError) -> AgenticToolError {
    let tool = AgenticTool::StructureProposalConfirm.name();
    let detail = e.to_string();
    match e {
        ConfirmError::Db(_) => AgenticToolError::InternalFailure { tool, detail },
        ConfirmError::NotFound(_)
        | ConfirmError::NotPendingConfirm { .. }
        | ConfirmError::ConfirmWindowExpired { .. }
        | ConfirmError::NotAuthorized { .. } => AgenticToolError::InvalidArguments { tool, detail },
    }
}

fn truncate_string(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use mwe_core::proposals::kind as proposal_kind;

    use super::*;

    #[test]
    fn agentic_tool_round_trip_via_name() {
        for t in [
            AgenticTool::WikiRecall,
            AgenticTool::WikiListPages,
            AgenticTool::WikiGetMeta,
            AgenticTool::WikiGetFact,
            AgenticTool::StructureProposalList,
            AgenticTool::StructureProposalGet,
            AgenticTool::StructureProposalApply,
            AgenticTool::WikiFactsFor,
            AgenticTool::WikiForget,
            AgenticTool::WikiSupersede,
            AgenticTool::WikiChangeScope,
            AgenticTool::WikiMoveFact,
            AgenticTool::WikiDeletePage,
            AgenticTool::WikiRequestForget,
            AgenticTool::StructureProposalRevert,
            AgenticTool::StructureProposalConfirm,
            AgenticTool::StructureProposalVote,
        ] {
            assert_eq!(AgenticTool::from_name(t.name()), Some(t));
        }
        assert_eq!(AgenticTool::from_name("dream_trigger"), None);
    }

    #[test]
    fn tool_descriptors_match_dispatch_names() {
        for descriptor in tool_descriptors() {
            assert!(
                AgenticTool::from_name(&descriptor.name).is_some(),
                "descriptor `{}` has no matching AgenticTool variant",
                descriptor.name,
            );
        }
    }

    #[test]
    fn tool_descriptors_carry_object_schema_with_required() {
        for descriptor in tool_descriptors() {
            let params = &descriptor.parameters;
            assert_eq!(params["type"], "object", "{}", descriptor.name);
            assert!(params["properties"].is_object(), "{}", descriptor.name);
            assert!(params["required"].is_array(), "{}", descriptor.name);
        }
    }

    /// Every wired tool must be TAUGHT, by name, in the bundled system
    /// prompt. The prompt is the only safety surface — there is no code
    /// confirmation gate — so a wired-but-undocumented write tool would
    /// reach the model with no guardrail (see
    /// [agentic-chat.md](../../../wiki/design-notes/agentic-chat.md)). This
    /// guards against a new tool landing in the registry without a matching
    /// flow block in the prompt.
    #[test]
    fn every_wired_tool_is_named_in_the_bundled_prompt() {
        const BUNDLED_PROMPT: &str = include_str!("../prompts/agentic-chat-panel.md");
        for descriptor in tool_descriptors() {
            assert!(
                BUNDLED_PROMPT.contains(&descriptor.name),
                "tool `{}` is wired but never named in the bundled agentic-chat-panel \
                 prompt — the prompt is the safety surface, teach it before shipping it",
                descriptor.name,
            );
        }
    }

    /// Unknown tool names surface as a typed error the agentic loop
    /// translates into a `{"error": "..."}` tool-result so the model
    /// can recover.
    #[tokio::test]
    async fn dispatch_unknown_tool_returns_unknown_tool_error() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let err = dispatch("not_a_real_tool", &json!({}), &ctx)
            .await
            .expect_err("unknown tool must error");
        assert!(matches!(err, AgenticToolError::UnknownTool(_)), "{err:?}");
    }

    /// `wiki_recall` requires a `query` field; missing it surfaces as
    /// `InvalidArguments` so the model receives a clear hint.
    #[tokio::test]
    async fn dispatch_wiki_recall_rejects_missing_query() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let err = dispatch("wiki_recall", &json!({}), &ctx)
            .await
            .expect_err("missing query must error");
        match err {
            AgenticToolError::InvalidArguments { tool, .. } => {
                assert_eq!(tool, "wiki_recall");
            },
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    /// `wiki_list_pages` rejects syntactically invalid wiki ids
    /// before touching the filesystem.
    #[tokio::test]
    async fn dispatch_wiki_list_pages_rejects_invalid_wiki_id() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let err = dispatch(
            "wiki_list_pages",
            &json!({ "wiki_id": "INVALID--id" }),
            &ctx,
        )
        .await
        .expect_err("invalid wiki_id must error");
        match err {
            AgenticToolError::InvalidArguments { tool, .. } => {
                assert_eq!(tool, "wiki_list_pages");
            },
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    /// Insert a row already in `applied` with a future `revert_deadline` and
    /// the given `recipient_id` — a revertable proposal the chat's
    /// `structure_proposal_revert` verb can target. A `dedup_merge` kind keeps
    /// the row trivial; the recipient gate refuses before the chassis runs, so
    /// the inverse handler is never invoked.
    async fn seed_applied_revertable_proposal(
        pool: &SqlitePool,
        proposal_id: &str,
        recipient: Option<&str>,
    ) {
        let now = chrono::Utc::now();
        let revert_deadline = now + chrono::Duration::days(7);
        sqlx::query(
            "INSERT INTO structure_proposals \
             (proposal_id, kind, context, questions, proposed_at, timeout_at, status, \
              applied_at, applied_by, apply_mode, revert_token, revert_deadline, recipient_id) \
             VALUES (?, ?, ?, '[]', ?, ?, 'applied', ?, 'admin', 'manual', ?, ?, ?)",
        )
        .bind(proposal_id)
        .bind(proposal_kind::DEDUP_MERGE)
        .bind(r#"{"intent":"test"}"#)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind("revert-token-xyz")
        .bind(revert_deadline.to_rfc3339())
        .bind(recipient)
        .execute(pool)
        .await
        .expect("seed applied revertable row");
    }

    /// Recipient gate (0032): a non-admin operator cannot revert a
    /// proposal addressed to a different user. The dispatcher refuses with
    /// `InvalidArguments` BEFORE touching the chassis, so the row is left
    /// untouched. This keeps revert consistent with the recipient scoping
    /// `structure_proposal_list` / `_get` enforce — revert is not a bypass.
    /// (The revert happy-path through the chassis is covered by `mwe_core`'s
    /// own per-kind revert tests.)
    #[tokio::test]
    async fn dispatch_proposal_revert_recipient_gate_blocks_non_addressee() {
        let dir = tempfile::tempdir().unwrap();
        let pool = mwe_core::db::open_or_init(dir.path()).await.unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        // Proposal addressed to frodo; the operator is galadriel.
        seed_applied_revertable_proposal(&pool, "p-frodo-revert", Some("user:frodo")).await;

        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("galadriel"),
            is_admin: false,
            reveal: false,
        };
        let err = dispatch(
            "structure_proposal_revert",
            &json!({ "proposal_id": "p-frodo-revert" }),
            &ctx,
        )
        .await
        .expect_err("non-addressee revert must be refused");
        match err {
            AgenticToolError::InvalidArguments { tool, detail } => {
                assert_eq!(tool, "structure_proposal_revert");
                assert!(
                    detail.contains("not addressed to you"),
                    "detail should explain the recipient gate: {detail}"
                );
            },
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        // A refused revert leaves the row untouched.
        let status: (String,) =
            sqlx::query_as("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-frodo-revert")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status.0, "applied",
            "a refused revert leaves the row applied"
        );
    }

    // ---------- structure_proposal_vote (fact_forget) ----------

    /// Seed a pending `fact_forget` request open for a vote: the eligible set +
    /// fact id live in `context`. Future-dated voting deadline (`timeout_at`) so
    /// the window is open. (The tally/apply engine is covered by `mwe_core`'s own
    /// `votes` tests; here we only assert the verb plumbs `cast_vote` correctly.)
    async fn seed_forget_request(pool: &SqlitePool, proposal_id: &str, eligible: &[&str]) {
        let now = chrono::Utc::now();
        let deadline = now + chrono::Duration::days(7);
        let context = json!({
            "variant": "fact_forget",
            "fact_id": "0190a0c8-0000-7000-8000-000000000001",
            "requester": "franz",
            "eligible_voters": eligible,
        })
        .to_string();
        sqlx::query(
            "INSERT INTO structure_proposals \
             (proposal_id, kind, context, questions, proposed_at, timeout_at, status, recipient_id) \
             VALUES (?, ?, ?, '[]', ?, ?, 'pending', 'user:franz')",
        )
        .bind(proposal_id)
        .bind(proposal_kind::FACT_FORGET)
        .bind(&context)
        .bind(now.to_rfc3339())
        .bind(deadline.to_rfc3339())
        .execute(pool)
        .await
        .expect("seed forget request");
    }

    /// An operator who is not in the request's eligible set is refused with
    /// `InvalidArguments` and casts nothing — the member-facing gate
    /// (`votes::cast_vote`) maps cleanly through the verb.
    #[tokio::test]
    async fn dispatch_proposal_vote_refuses_an_ineligible_member() {
        let dir = tempfile::tempdir().unwrap();
        let pool = mwe_core::db::open_or_init(dir.path()).await.unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        // Eligible = {frodo}; the operator is galadriel.
        seed_forget_request(&pool, "p-ff-1", &["frodo"]).await;

        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("galadriel"),
            is_admin: false,
            reveal: false,
        };
        let err = dispatch(
            "structure_proposal_vote",
            &json!({ "proposal_id": "p-ff-1", "vote": "no" }),
            &ctx,
        )
        .await
        .expect_err("an ineligible member must be refused");
        match err {
            AgenticToolError::InvalidArguments { tool, .. } => {
                assert_eq!(tool, "structure_proposal_vote");
            },
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        let votes: (i64,) =
            sqlx::query_as("SELECT count(*) FROM structure_proposal_votes WHERE proposal_id = ?")
                .bind("p-ff-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(votes.0, 0, "a refused vote records nothing");
    }

    /// An eligible member's vote is recorded as the signed-in user; with a
    /// single eligible member a YES is the all-voted (no NO-majority) resolution
    /// → the forget applies (the proposal flips to `applied`).
    #[tokio::test]
    async fn dispatch_proposal_vote_records_an_eligible_members_vote() {
        let dir = tempfile::tempdir().unwrap();
        let pool = mwe_core::db::open_or_init(dir.path()).await.unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        // The voter must be an enrolled user (the votes table FKs into it).
        sqlx::query("INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('galadriel', '[]', 0)")
            .execute(&pool)
            .await
            .unwrap();
        seed_forget_request(&pool, "p-ff-2", &["galadriel"]).await;

        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("galadriel"),
            is_admin: false,
            reveal: false,
        };
        let out = dispatch(
            "structure_proposal_vote",
            &json!({ "proposal_id": "p-ff-2", "vote": "yes" }),
            &ctx,
        )
        .await
        .expect("eligible YES vote recorded");
        assert!(out.contains("\"voted\""), "report keys the action: {out}");
        assert!(out.contains("applied"), "1-of-1 YES resolves early: {out}");

        // The vote was stored as galadriel, and the forget applied.
        let row: (String, String) = sqlx::query_as(
            "SELECT voter_id, vote FROM structure_proposal_votes WHERE proposal_id = ?",
        )
        .bind("p-ff-2")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row, ("galadriel".to_owned(), "yes".to_owned()));
        let status: (String,) =
            sqlx::query_as("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-ff-2")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status.0, "applied", "all-yes applies the forget");
    }

    /// The `wiki_request_forget` verb refuses a SENDER (the fact's author) and
    /// points them at the direct path — the engine's `SenderActsDirectly`
    /// refusal maps to `InvalidArguments` through the dispatcher.
    #[tokio::test]
    async fn dispatch_wiki_request_forget_refuses_the_sender() {
        let dir = tempfile::tempdir().unwrap();
        let pool = mwe_core::db::open_or_init(dir.path()).await.unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        // A fact franz owns AND authored (sender == franz).
        let fact_id =
            mwe_core::types::FactId::parse("0190a0c8-0000-7000-8000-0000000000aa").unwrap();
        mwe_core::fact_index::insert(
            &pool,
            &mwe_core::fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fact_id.clone(),
                wiki_id: "franz".to_owned(),
                source_path: "wikis/franz/intro.md".to_owned(),
                region_start: Some(0),
                region_end: Some(8),
                text: "franz's own fact".to_owned(),
                embedding: vec![0.1, 0.2, 0.3, 0.4],
                owner_id: "user:franz".parse().unwrap(),
                allow_ids: Vec::new(),
                sender_id: Some("user:franz".parse().unwrap()),
                fact_type: None,
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("franz"),
            is_admin: false,
            reveal: false,
        };
        let err = dispatch(
            "wiki_request_forget",
            &json!({ "fact_id": fact_id.as_str() }),
            &ctx,
        )
        .await
        .expect_err("the sender must be refused (they delete directly)");
        match err {
            AgenticToolError::InvalidArguments { tool, detail } => {
                assert_eq!(tool, "wiki_request_forget");
                assert!(detail.contains("authored"), "explains why: {detail}");
            },
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        // No proposal was opened.
        let n: (i64,) =
            sqlx::query_as("SELECT count(*) FROM structure_proposals WHERE kind = 'fact_forget'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n.0, 0);
    }

    // ---------- structure_proposal_confirm ----------

    /// Insert a row already in `applied_pending_confirm` with a future
    /// `confirm_deadline` and the given `recipient_id` — the state the
    /// nightly auto-apply sweep lands a proposal in, waiting for the user
    /// to confirm or revert. Mirrors `proposals::mark_auto_applied`'s
    /// column shape (which is `pub(crate)`, so unusable from this crate's
    /// tests); a `dedup_merge` kind keeps the row trivial (confirm does
    /// not re-run the handler). Returns the proposal id.
    async fn seed_applied_pending_confirm(
        pool: &SqlitePool,
        proposal_id: &str,
        recipient: Option<&str>,
    ) {
        let now = chrono::Utc::now();
        let confirm_deadline = now + chrono::Duration::hours(24);
        sqlx::query(
            "INSERT INTO structure_proposals \
             (proposal_id, kind, context, questions, proposed_at, timeout_at, status, \
              applied_at, applied_by, apply_mode, confirm_deadline, recipient_id) \
             VALUES (?, ?, ?, ?, ?, ?, 'applied_pending_confirm', ?, NULL, 'auto', ?, ?)",
        )
        .bind(proposal_id)
        .bind(proposal_kind::DEDUP_MERGE)
        .bind(r#"{"intent":"test"}"#)
        .bind("[]")
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(confirm_deadline.to_rfc3339())
        .bind(recipient)
        .execute(pool)
        .await
        .expect("seed applied_pending_confirm row");
    }

    /// Seed a `pending` proposal whose `context` carries identifiable text,
    /// addressed to `recipient`.
    async fn seed_pending_with_context(
        pool: &SqlitePool,
        proposal_id: &str,
        recipient: Option<&str>,
        context_json: &str,
    ) {
        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO structure_proposals \
             (proposal_id, kind, context, questions, proposed_at, timeout_at, status, recipient_id) \
             VALUES (?, ?, ?, '[]', ?, ?, 'pending', ?)",
        )
        .bind(proposal_id)
        .bind(proposal_kind::DEDUP_MERGE)
        .bind(context_json)
        .bind(now.to_rfc3339())
        .bind((now + chrono::Duration::hours(24)).to_rfc3339())
        .bind(recipient)
        .execute(pool)
        .await
        .expect("seed pending row");
    }

    /// `structure_proposal_list` is scoped to the operator's own proposals
    /// by default — even for an admin. Another user's pending (whose
    /// `context` carries that user's fact text) stays invisible, and its
    /// content does not leak; only the admin ACL-reveal flag lifts the
    /// scope. This is the per-fragment ACL holding on the proposal read
    /// path (`proposal_recipient_scope`).
    #[tokio::test]
    async fn proposal_list_hides_other_users_until_reveal() {
        let dir = tempfile::tempdir().unwrap();
        let pool = mwe_core::db::open_or_init(dir.path()).await.unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_pending_with_context(
            &pool,
            "p-bob",
            Some("user:bob"),
            r#"{"intent":"bob-private-secret"}"#,
        )
        .await;

        // Admin without reveal: bob's proposal is not listed and its
        // context text does not leak.
        let scoped = dispatch(
            "structure_proposal_list",
            &json!({ "status": "pending" }),
            &AgenticContext {
                pool: &pool,
                tree: &tree,
                embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
                sender_ctx: SenderContext::user("alice"),
                is_admin: true,
                reveal: false,
            },
        )
        .await
        .expect("list succeeds");
        let v: serde_json::Value = serde_json::from_str(&scoped).unwrap();
        assert!(
            v["proposals"].as_array().is_some_and(Vec::is_empty),
            "admin without reveal must not see bob's pending: {v}"
        );
        assert!(
            !scoped.contains("bob-private-secret"),
            "bob's proposal context must not leak without reveal: {scoped}"
        );

        // Admin WITH reveal: bob's proposal surfaces.
        let revealed = dispatch(
            "structure_proposal_list",
            &json!({ "status": "pending" }),
            &AgenticContext {
                pool: &pool,
                tree: &tree,
                embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
                sender_ctx: SenderContext::user("alice"),
                is_admin: true,
                reveal: true,
            },
        )
        .await
        .expect("list succeeds");
        assert!(
            revealed.contains("p-bob"),
            "admin WITH reveal must see bob's pending: {revealed}"
        );
    }

    /// Happy path: an admin confirms an `applied_pending_confirm`
    /// proposal from the chat. The dispatcher hands off to
    /// `proposals::confirm_proposal`, which promotes the row to `applied`,
    /// mints a `revert_token`, and opens the revert window. Asserts the
    /// `confirmed` payload and that the row is now `applied`.
    #[tokio::test]
    async fn dispatch_proposal_confirm_promotes_pending_confirm() {
        let dir = tempfile::tempdir().unwrap();
        let pool = mwe_core::db::open_or_init(dir.path()).await.unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_applied_pending_confirm(&pool, "p-confirm-me", Some("user:alice")).await;

        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let out = dispatch(
            "structure_proposal_confirm",
            &json!({ "proposal_id": "p-confirm-me" }),
            &ctx,
        )
        .await
        .expect("confirm succeeds");

        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let confirmed = &v["confirmed"];
        assert_eq!(confirmed["proposal_id"], "p-confirm-me");
        assert_eq!(confirmed["kind"], proposal_kind::DEDUP_MERGE);
        assert!(
            confirmed["revert_token"]
                .as_str()
                .is_some_and(|t| !t.is_empty()),
            "a revert_token is minted on confirm: {v}"
        );
        assert!(
            confirmed["revert_deadline"].is_string(),
            "revert_deadline present: {v}"
        );

        let status: (String,) =
            sqlx::query_as("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-confirm-me")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status.0, "applied", "the row is now applied after confirm");
    }

    /// Recipient gate: a non-admin operator cannot confirm a
    /// proposal addressed to a different user. The chassis itself returns
    /// `ConfirmError::NotAuthorized`, which the dispatcher maps to
    /// `InvalidArguments`; the row stays `applied_pending_confirm`.
    #[tokio::test]
    async fn dispatch_proposal_confirm_blocks_non_addressee() {
        let dir = tempfile::tempdir().unwrap();
        let pool = mwe_core::db::open_or_init(dir.path()).await.unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        // Proposal addressed to frodo; the operator is galadriel.
        seed_applied_pending_confirm(&pool, "p-frodo-confirm", Some("user:frodo")).await;

        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("galadriel"),
            is_admin: false,
            reveal: false,
        };
        let err = dispatch(
            "structure_proposal_confirm",
            &json!({ "proposal_id": "p-frodo-confirm" }),
            &ctx,
        )
        .await
        .expect_err("non-addressee confirm must be refused");
        match err {
            AgenticToolError::InvalidArguments { tool, detail } => {
                assert_eq!(tool, "structure_proposal_confirm");
                assert!(
                    detail.contains("not authorized"),
                    "detail should explain the recipient gate: {detail}"
                );
            },
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        let status: (String,) =
            sqlx::query_as("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-frodo-confirm")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status.0, "applied_pending_confirm",
            "the row must survive a refused confirm (no state change)"
        );
    }

    // ---------- wiki_move_fact ----------

    /// Build a workdir with a standard source wiki `alice`, a standard
    /// same-owner destination `salute`, and a SMART wiki `proj`. Returns the
    /// tempdir, pool, and tree (re-opened so all three metas are visible).
    async fn move_fact_tree() -> (tempfile::TempDir, SqlitePool, WikiTree) {
        let dir = tempfile::tempdir().unwrap();
        let pool = mwe_core::db::open_or_init(dir.path()).await.unwrap();
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("alice")).unwrap();
        std::fs::write(
            wikis.join("alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        std::fs::write(wikis.join("alice/index.md"), "# Alice\n").unwrap();
        std::fs::create_dir_all(wikis.join("salute")).unwrap();
        std::fs::write(
            wikis.join("salute/_meta.md"),
            "---\nwiki_id: salute\nwiki_type: wiki-user\nslug: salute\ntitle: Salute\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(wikis.join("proj")).unwrap();
        std::fs::write(
            wikis.join("proj/_meta.md"),
            "---\nwiki_id: proj\nwiki_type: wiki-tech\nslug: proj\ntitle: Project\nacl_default: 'user:alice'\nsmart: true\n---\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        (dir, pool, tree)
    }

    /// Capture a fact into `alice/index.md` via the real capture path so it
    /// lands as a marker on disk (the move engines locate the region by
    /// marker). Owned by `user:alice`.
    async fn capture_alice_fact(
        pool: &SqlitePool,
        tree: &WikiTree,
        embedder: Arc<dyn Embedder>,
        body: &str,
    ) -> FactId {
        use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: std::path::PathBuf::from("index.md"),
            body: body.to_owned(),
            owner: "user:alice".parse().unwrap(),
            allow: vec![],
            sender: None,
            fact_type: None,
            topics: vec![],
            dedup_threshold: Some(1.01),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        match wiki_capture(tree, pool, embedder, req)
            .await
            .unwrap()
            .action
        {
            CaptureAction::Captured { .. } => {
                // re-find by source path to get the id
                let facts =
                    mwe_core::fact_index::find_active_by_source_path(pool, "wikis/alice/index.md")
                        .await
                        .unwrap();
                facts
                    .into_iter()
                    .find(|f| f.text == body)
                    .expect("captured fact present")
                    .fact_id
            },
            other => panic!("expected Captured, got {other:?}"),
        }
    }

    /// Plant a section row in `fact_index` for the smart wiki `proj` the way
    /// the smart content indexer would: `sender_id` NULL (no per-fragment
    /// capturer) and owner = the wiki's scope principal. Minimal on purpose —
    /// the smart guards key on the wiki's `smart` meta flag, not on the row.
    async fn seed_smart_section_row(pool: &SqlitePool, fact_id: &FactId) {
        mwe_core::fact_index::insert(
            pool,
            &mwe_core::fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fact_id.clone(),
                wiki_id: "proj".to_owned(),
                source_path: "wikis/proj/index.md".to_owned(),
                region_start: Some(0),
                region_end: Some(5),
                text: "Project uses CI".to_owned(),
                embedding: vec![0.1, 0.2, 0.3, 0.4],
                owner_id: "user:alice".parse().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
    }

    /// Happy path: the chat moves a fact cross-wiki (alice → salute). The
    /// dispatcher refiles it onto salute's index.md and returns a `moved`
    /// payload with the born-applied receipt id.
    #[tokio::test]
    async fn dispatch_wiki_move_fact_moves_cross_wiki() {
        let (dir, pool, tree) = move_fact_tree().await;
        let embedder: Arc<dyn Embedder> =
            Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4));
        let fid = capture_alice_fact(
            &pool,
            &tree,
            Arc::clone(&embedder),
            "Alice takes lisinopril",
        )
        .await;

        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder,
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let out = dispatch(
            "wiki_move_fact",
            &json!({ "fact_id": fid.as_str(), "dest_wiki_id": "salute" }),
            &ctx,
        )
        .await
        .expect("cross-wiki move succeeds");

        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let moved = &v["moved"];
        assert_eq!(moved["fact_id"], fid.as_str());
        assert_eq!(moved["dest_wiki_id"], "salute");
        assert_eq!(moved["dest_page"], "index.md");
        assert_eq!(moved["cross_wiki"], true);
        assert!(
            moved["proposal_id"].as_str().is_some_and(|s| !s.is_empty()),
            "a born-applied receipt id is returned: {v}"
        );

        // Engine effect: the fact now lives in salute.
        let row = mwe_core::fact_index::find_by_id(&pool, &fid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.wiki_id, "salute");
        assert_eq!(row.source_path, "wikis/salute/index.md");
        drop(dir);
    }

    /// A move INTO a smart wiki is refused (smart wikis are the consumer's).
    #[tokio::test]
    async fn dispatch_wiki_move_fact_refuses_smart_dest() {
        let (dir, pool, tree) = move_fact_tree().await;
        let embedder: Arc<dyn Embedder> =
            Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4));
        let fid = capture_alice_fact(&pool, &tree, Arc::clone(&embedder), "Alice uses Rust").await;

        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder,
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let err = dispatch(
            "wiki_move_fact",
            &json!({ "fact_id": fid.as_str(), "dest_wiki_id": "proj" }),
            &ctx,
        )
        .await
        .expect_err("a smart dest must be refused");
        match err {
            AgenticToolError::InvalidArguments { tool, detail } => {
                assert_eq!(tool, "wiki_move_fact");
                assert!(detail.contains("smart"), "refusal cites smart: {detail}");
            },
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        // The fact stayed in alice.
        let row = mwe_core::fact_index::find_by_id(&pool, &fid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.wiki_id, "alice", "fact untouched after refused move");
        drop(dir);
    }

    /// A move whose SOURCE wiki is smart is refused before any engine call.
    #[tokio::test]
    async fn dispatch_wiki_move_fact_refuses_smart_source() {
        let (dir, pool, tree) = move_fact_tree().await;
        // Seed a fact directly into the smart wiki `proj` (no capture needed —
        // the refusal fires before the engine reads any marker).
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dab").unwrap();
        seed_smart_section_row(&pool, &fid).await;

        let embedder: Arc<dyn Embedder> =
            Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4));
        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder,
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let err = dispatch(
            "wiki_move_fact",
            &json!({ "fact_id": fid.as_str(), "dest_wiki_id": "salute" }),
            &ctx,
        )
        .await
        .expect_err("a smart source must be refused");
        assert!(
            matches!(&err, AgenticToolError::InvalidArguments { detail, .. } if detail.contains("smart")),
            "{err:?}"
        );
        drop(dir);
    }

    /// A non-admin operator cannot move a fact — even the fact's own owner: a
    /// move is admin-only structure authority (the write-authority model).
    #[tokio::test]
    async fn dispatch_wiki_move_fact_refuses_non_admin() {
        let (dir, pool, tree) = move_fact_tree().await;
        let embedder: Arc<dyn Embedder> =
            Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4));
        let fid =
            capture_alice_fact(&pool, &tree, Arc::clone(&embedder), "Alice walks daily").await;

        // alice owns the fact but is not an admin → refused.
        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder,
            sender_ctx: SenderContext::user("alice"),
            is_admin: false,
            reveal: false,
        };
        let err = dispatch(
            "wiki_move_fact",
            &json!({ "fact_id": fid.as_str(), "dest_wiki_id": "salute" }),
            &ctx,
        )
        .await
        .expect_err("a non-admin move must be refused");
        assert!(
            matches!(&err, AgenticToolError::InvalidArguments { detail, .. } if detail.contains("admin-only")),
            "{err:?}"
        );
        // The fact stayed in alice (untouched).
        let row = mwe_core::fact_index::find_by_id(&pool, &fid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.wiki_id, "alice", "fact untouched after refused move");
        drop(dir);
    }

    // ---------- smart-wiki guards (smart wikis are the consumer's) ----------

    /// `wiki_change_scope` refuses a smart source wiki: its wiki-level read
    /// audience derives from its position in the tree, so a re-parent would
    /// change effective read access on the next reindex/push.
    #[tokio::test]
    async fn dispatch_wiki_change_scope_refuses_smart_source() {
        let (dir, pool, tree) = move_fact_tree().await;
        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let err = dispatch(
            "wiki_change_scope",
            &json!({ "source_wiki_id": "proj", "new_parent_wiki_id": "alice" }),
            &ctx,
        )
        .await
        .expect_err("a smart source must be refused");
        assert!(
            matches!(&err, AgenticToolError::InvalidArguments { tool, detail }
                if *tool == "wiki_change_scope" && detail.contains("smart")),
            "{err:?}"
        );
        // Nothing moved on disk.
        assert!(dir.path().join("wikis/proj").exists());
        assert!(!dir.path().join("wikis/alice/proj").exists());
        drop(dir);
    }

    /// `wiki_forget` refuses a fact living in a smart wiki — smart section
    /// rows are not fact-governed (the consumer's next push would undo the
    /// tombstone). Admin context, so only the smart guard can be the refusal.
    #[tokio::test]
    async fn dispatch_wiki_forget_refuses_smart_target() {
        let (dir, pool, tree) = move_fact_tree().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dab").unwrap();
        seed_smart_section_row(&pool, &fid).await;
        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let err = dispatch(
            "wiki_forget",
            &json!({ "fact_id": fid.as_str(), "reason": "user_request" }),
            &ctx,
        )
        .await
        .expect_err("a smart target must be refused");
        assert!(
            matches!(&err, AgenticToolError::InvalidArguments { tool, detail }
                if *tool == "wiki_forget" && detail.contains("smart")),
            "{err:?}"
        );
        // The row is untouched.
        let row = mwe_core::fact_index::find_by_id(&pool, &fid)
            .await
            .unwrap()
            .unwrap();
        assert!(row.deleted_at.is_none(), "smart row survives the refusal");
        drop(dir);
    }

    /// `wiki_supersede` refuses a fact living in a smart wiki — a supersede
    /// would write a marker-wrapped fact into the consumer's plain-markdown
    /// page. Admin context, so only the smart guard can be the refusal.
    #[tokio::test]
    async fn dispatch_wiki_supersede_refuses_smart_target() {
        let (dir, pool, tree) = move_fact_tree().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dab").unwrap();
        seed_smart_section_row(&pool, &fid).await;
        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("alice"),
            is_admin: true,
            reveal: false,
        };
        let err = dispatch(
            "wiki_supersede",
            &json!({ "old_fact_id": fid.as_str(), "new_body": "corrected body" }),
            &ctx,
        )
        .await
        .expect_err("a smart target must be refused");
        assert!(
            matches!(&err, AgenticToolError::InvalidArguments { tool, detail }
                if *tool == "wiki_supersede" && detail.contains("smart")),
            "{err:?}"
        );
        // The row is untouched (not superseded, not tombstoned).
        let row = mwe_core::fact_index::find_by_id(&pool, &fid)
            .await
            .unwrap()
            .unwrap();
        assert!(row.superseded_at.is_none() && row.deleted_at.is_none());
        drop(dir);
    }

    /// `wiki_request_forget` refuses a fact living in a smart wiki — forget
    /// votes are per-fact governance, smart governance is wiki-level. The
    /// operator is the row's owner, so without the guard the engine would
    /// have accepted the request.
    #[tokio::test]
    async fn dispatch_wiki_request_forget_refuses_smart_target() {
        let (dir, pool, tree) = move_fact_tree().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dab").unwrap();
        seed_smart_section_row(&pool, &fid).await;
        let ctx = AgenticContext {
            pool: &pool,
            tree: &tree,
            embedder: Arc::new(mwe_core::embedder::FakeEmbedder::new("fake", 4)),
            sender_ctx: SenderContext::user("alice"),
            is_admin: false,
            reveal: false,
        };
        let err = dispatch(
            "wiki_request_forget",
            &json!({ "fact_id": fid.as_str() }),
            &ctx,
        )
        .await
        .expect_err("a smart target must be refused");
        assert!(
            matches!(&err, AgenticToolError::InvalidArguments { tool, detail }
                if *tool == "wiki_request_forget" && detail.contains("smart")),
            "{err:?}"
        );
        // No forget request was opened.
        let n: (i64,) =
            sqlx::query_as("SELECT count(*) FROM structure_proposals WHERE kind = 'fact_forget'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n.0, 0);
        drop(dir);
    }
}
