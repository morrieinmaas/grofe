//! Binding and overlay rows (FE.4): `if-shell`, `command-alias`, popup and
//! menu flags, `list-keys` output, the mouse argv forms.
//!
//! Rows and rewrites only: the schema, the dispatcher and the helpers a
//! translation uses ([`super::Invocation`]) are `mod.rs`'s. The most
//! specific matching row decides, and between rows as specific this
//! file's come before the core rows, so a row here naming a core form
//! takes it over. Rewrites ([`super::Rewrite`]) apply before any row.

use super::{Rewrite, Row};

/// This area's rows.
pub(crate) fn rows() -> Vec<Row> {
    Vec::new()
}

/// This area's argument rewrites, applied before any row matches.
pub(crate) fn rewrites() -> Vec<Rewrite> {
    Vec::new()
}
