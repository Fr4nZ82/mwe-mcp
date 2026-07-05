// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canonical mwe-core errors.
//!
//! Concrete variants are added as the underlying modules land.

use thiserror::Error;

/// Canonical mwe-core error.
#[derive(Debug, Error)]
pub enum Error {
    /// Underlying I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Database error (sqlx).
    #[error("db error: {0}")]
    Db(#[from] sqlx::Error),

    /// JSON serialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// YAML serialization error.
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// Title produces an empty slug after the derivation pipeline.
    ///
    /// Maps to `400 invalid_title` at the tool boundary. The agent must pass
    /// an explicit `slug` parameter to recover.
    #[error("invalid_title: pipeline produced empty slug from {input:?}")]
    InvalidTitle {
        /// The original title that the pipeline could not turn into a slug.
        input: String,
    },

    /// More than 99 collisions on the same base slug.
    ///
    /// Maps to `409 wiki_collision_cap_exceeded` at the tool boundary. The
    /// agent must pass an explicit `slug` to disambiguate.
    #[error("collision_cap_exceeded: base {base:?} has 99 collisions")]
    CollisionCapExceeded {
        /// The base slug whose collision cap was exceeded.
        base: String,
    },

    /// Generic catch-all error (placeholder).
    #[error("{0}")]
    Other(String),
}

/// `Result` alias with [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
