//! Structure, client and listing rows (FE.2): the structure and client
//! verbs, tmux's layout strings, `list-*` default output.
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
