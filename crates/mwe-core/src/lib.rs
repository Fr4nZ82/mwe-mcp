// SPDX-License-Identifier: AGPL-3.0-or-later
//! mwe-core — headless memory engine.
//!
//! Library crate consumed by `mwe-mcp-server` and `mwe-dashboard`. Hosts all
//! memory logic: the markdown surface + the authoritative sqlite `engine.db`
//! fact store, marker parser, ACL,
//! recall pipeline, REM (nightly), internal `_internal.*` APIs, lockfile,
//! applicative WAL, file watcher, slug pipeline.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod acl;
pub mod archive;
pub mod audit;
pub mod backup;
pub mod briefing;
pub mod bundle;
pub mod capture;
pub mod capture_buffer;
pub mod comment_apply;
pub mod compile_failures;
pub mod compiler;
pub mod config;
pub mod consumers;
pub mod db;
pub mod dedup;
pub mod delegations;
pub mod diagnostics;
pub mod disclosure_audit;
pub mod document;
pub mod dream;
pub mod dream_journal;
pub mod dream_light;
pub mod embedder;
pub mod enrollment;
pub mod env_file;
pub mod error;
pub mod events;
pub mod export;
pub mod fact_index;
pub mod faults;
pub mod housekeeping;
pub mod ingest;
pub mod jwt;
pub mod lint;
pub mod llm;
#[cfg(feature = "local-embedder")]
pub mod local_embedder;
pub mod locale;
pub mod lockfile;
pub mod media;
pub mod meta_annotate;
pub mod model_catalog;
pub mod oauth;
pub mod oauth_server;
pub mod operator_edits;
pub mod page;
pub mod parser;
pub mod planner;
pub mod promote;
pub mod prompts;
pub mod proposals;
pub mod recall;
pub mod recall_eval;
pub mod recall_gate;
pub mod recall_log;
pub mod recall_nav;
pub mod recall_trace;
pub mod recent_window;
pub mod recovery;
pub mod reindex;
pub mod rem;
pub mod rem_verdicts;
pub mod reminders;
pub mod render;
pub mod reviewer;
pub mod scope;
pub mod sections;
pub mod signposts;
pub mod skills;
pub mod slug;
pub mod smart;
#[cfg(test)]
mod test_db;
pub mod training_spool;
pub mod types;
pub mod usage;
pub mod votes;
pub mod wal;
pub mod watcher;
pub mod wiki;
pub mod wiki_admin;
pub mod wiki_admin_leases;
pub mod wiki_delete;
pub mod workdir_security;

pub use error::{Error, Result};

/// Crate version (matches `Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
