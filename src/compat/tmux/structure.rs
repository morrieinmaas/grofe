//! Structure, client and listing rows (FE.2): the structure and client
//! verbs, tmux's layout strings, `list-*` default output.
//!
//! Rows and rewrites only: the schema, the dispatcher and the helpers a
//! translation uses ([`super::Invocation`]) are `mod.rs`'s. The most
//! specific matching row decides, and between rows as specific this
//! file's come before the core rows, so a row here naming a core form
//! takes it over. Rewrites ([`super::Rewrite`]) apply before any row.
//!
//! Every row here passes through: the structure verbs (TC.8) and client
//! verbs (TC.9) have tmux's names and flag letters natively. tmux's layout
//! strings are native too (spec §4.3): `select-layout` reads them and
//! `#{window_layout}` writes them (`server::layout::tmux_layout`). So is
//! the `list-*` output with no `-F`: a tmux-dialect request gets tmux's
//! default templates (`server::dispatch::options::TMUX_LIST_*`), since the
//! command a row builds does not say which dialect asked.

use super::{GrofeTarget, MappingKind, Rewrite, Row};

const fn pass(verb: &'static str, golden_id: &'static str) -> Row {
    Row {
        tmux_form: verb,
        kind: MappingKind::PassThrough,
        target: GrofeTarget::Native(verb),
        flags: &[],
        milestone: "M3.6",
        golden_id,
    }
}

/// This area's rows.
pub(crate) fn rows() -> Vec<Row> {
    vec![
        // `list-*`, with tmux's default output and `-a -s -t -f`.
        pass("list-sessions", "fe_2/lists"),
        pass("list-windows", "fe_2/lists"),
        pass("list-panes", "fe_2/lists"),
        pass("list-clients", "fe_2/lists"),
        // The structure verbs (TC.8), by tmux's names and flags.
        pass("join-pane", "fe_2/structure"),
        pass("move-pane", ""),
        pass("swap-pane", "fe_2/structure"),
        pass("swap-window", "fe_2/structure"),
        pass("break-pane", "fe_2/structure"),
        pass("rotate-window", "fe_2/structure"),
        pass("move-window", "fe_2/structure"),
        pass("last-pane", ""),
        pass("next-layout", ""),
        pass("resize-window", "fe_3/hooks"),
        pass("respawn-window", ""),
        // The client and session verbs (TC.9).
        pass("show-messages", "fe_2/sessions"),
        pass("refresh-client", ""),
        pass("choose-client", ""),
        pass("suspend-client", ""),
        pass("find-window", ""),
    ]
}

/// This area's argument rewrites, applied before any row matches.
pub(crate) fn rewrites() -> Vec<Rewrite> {
    Vec::new()
}
