// SPDX-License-Identifier: AGPL-3.0-or-later
//! Authentication primitives — password hashing, session cookie
//! issuance/verification, and the request-scoped extractors that the
//! routes use to learn "who is calling".
//!
//! Split into focused submodules:
//! - [`password`] — Argon2id PHC encoding + verification.
//! - [`session`] — session cookie lifecycle (issue, verify, refresh,
//!   clear) and the [`session::SessionUser`] extractor + admin guard.

pub mod password;
pub mod session;

pub use session::{AdminUser, SessionUser};
