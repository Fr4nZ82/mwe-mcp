// SPDX-License-Identifier: AGPL-3.0-or-later
//! Server-side HTML rendering — Maud templates organised in submodules.
//!
//! - [`layout`] — the base shell every page shares (`<html>`, nav,
//!   flash slot, footer) plus a few error-page helpers.
//! - [`components`] — small reusable bits (form field, table, badge,
//!   submit button) so individual route templates stay declarative.
//!
//! Pass 4 fills in the real templates; pass 1 stubs them to keep the
//! crate compiling.

pub mod components;
pub mod layout;
