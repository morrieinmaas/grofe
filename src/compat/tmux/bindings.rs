//! Binding and overlay rows (FE.4): `if-shell`, `command-alias`, popup and
//! menu flags, `list-keys` output, the mouse argv forms.
//!
//! Rows only: the schema, the dispatcher and the helpers a translation
//! uses ([`super::Invocation`]) are `mod.rs`'s. This file's rows come
//! before the core rows in [`super::table`], so a row here takes over a
//! core form.

use super::Row;

/// This area's rows.
pub(crate) fn rows() -> Vec<Row> {
    Vec::new()
}
