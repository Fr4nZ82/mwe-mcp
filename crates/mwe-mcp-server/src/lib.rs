// SPDX-License-Identifier: AGPL-3.0-or-later
//! `mwe-mcp-server` library surface — the dispatcher + auth + state
//! modules backing the `mwe-mcp` binary.
//!
//! Exposed primarily so the integration tests under `tests/` can call
//! the dispatcher directly without spinning up a real transport. The
//! binary itself (`src/main.rs`) re-imports the same modules through
//! the crate.

#![forbid(unsafe_code)]

pub mod backup_scheduler;
pub mod env_loader;
pub mod http_connect;
pub mod http_media;
pub mod http_skills;
pub mod mcp;
pub mod rem_scheduler;
pub mod reminder_scheduler;
pub mod tracing_setup;
