//! Binding and overlay rows (FE.4): `if-shell`, `command-alias`, popup and
//! menu flags, `new-pane` and `wait-for` (fzf's `--tmux`, M36-R1), the mouse
//! argv forms.
//!
//! Rows and rewrites only: the schema, the dispatcher and the helpers a
//! translation uses ([`super::Invocation`]) are `mod.rs`'s. The most
//! specific matching row decides, and between rows as specific this
//! file's come before the core rows, so a row here naming a core form
//! takes it over. Rewrites ([`super::Rewrite`]) apply before any row.
//!
//! Every verb here has a native verb of the same name and flag letters
//! (TC.6, TC.7, TC.9, TC.10), so most rows pass through. A translation
//! only drops the style flags grofe draws no differently, each noted in
//! `compat-miss.log` ([`super::Invocation::ignored`]). Stored commands (an
//! `if-shell` branch, a menu item, a `confirm-before` command) keep the
//! request's dialect, so a tmux branch such as `neww` runs as tmux's.

use super::{FlagMark, GrofeTarget, Invocation, MappingKind, Rewrite, Row, TmuxError};
use crate::cmd::Command;

const UNUSED: &str = "nothing the owner runs sends it (spec 4.5)";

const SEMANTIC: &[FlagMark] = &[FlagMark::Semantic];

const fn row(
    tmux_form: &'static str,
    kind: MappingKind,
    target: GrofeTarget,
    flags: &'static [FlagMark],
    golden_id: &'static str,
) -> Row {
    Row {
        tmux_form,
        kind,
        target,
        flags,
        milestone: "M3.6",
        golden_id,
    }
}

const fn pass(verb: &'static str, golden_id: &'static str) -> Row {
    row(
        verb,
        MappingKind::PassThrough,
        GrofeTarget::Native(verb),
        &[],
        golden_id,
    )
}

/// Refused for good: [`super::Row::refuses`] is true with no milestone.
const fn refuse(tmux_form: &'static str, reason: &'static str) -> Row {
    Row {
        tmux_form,
        kind: MappingKind::Behaviour,
        target: GrofeTarget::Unsupported(reason),
        flags: SEMANTIC,
        milestone: "",
        golden_id: "",
    }
}

/// `inv`'s arguments without the flags in `cosmetic`, each one given noted
/// as ignored; the positionals after them as sent.
fn without_cosmetic(inv: &Invocation<'_>, cosmetic: &[char]) -> Vec<String> {
    let got = inv.getopt();
    let mut args = Vec::new();
    let mut noted = Vec::new();
    for (flag, value) in &got.flags {
        if cosmetic.contains(flag) {
            if !noted.contains(flag) {
                inv.ignored(*flag);
                noted.push(*flag);
            }
            continue;
        }
        args.push(format!("-{flag}"));
        args.extend(value.map(str::to_string));
    }
    let rest = got.rest.min(inv.args.len());
    if rest > 0 && inv.args[rest - 1] == "--" {
        args.push("--".to_string());
    }
    args.extend_from_slice(&inv.args[rest..]);
    args
}

/// `display-popup`: every flag tmux has is native, styles and `-k -N -C`
/// included.
fn display_popup(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    inv.native("display-popup", inv.args)
}

/// `display-menu`: every flag tmux has is native, the border lines, the
/// styles, `-O -M -C` and `-c` included.
fn display_menu(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    inv.native("display-menu", inv.args)
}

/// `new-pane`: the pane's border styles are cosmetic.
fn new_pane(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    inv.native("new-pane", &without_cosmetic(inv, &['s', 'S', 'R']))
}

/// This area's rows.
pub(crate) fn rows() -> Vec<Row> {
    use GrofeTarget::Translate;
    use MappingKind::{Behaviour, PassThrough};
    vec![
        // The conditional (TC.10): the format form on the actor, the shell
        // form off it; the branch runs in the request's dialect. A bare
        // `=` target is the pointer's inside a mouse key (M36-R3).
        row(
            "if-shell",
            Behaviour,
            GrofeTarget::Native("if-shell"),
            &[],
            "fe_4/if-format",
        ),
        row(
            "if-shell -F",
            Behaviour,
            GrofeTarget::Native("if-shell"),
            SEMANTIC,
            "fe_4/if-format",
        ),
        // `command-alias` entries are grofe aliases (TC.7): the native
        // dispatcher expands them before any dialect parses the request.
        row(
            "set-option command-alias",
            Behaviour,
            Translate(|inv| super::option_command(inv, "set-option", false)),
            &[],
            "fe_4/aliases",
        ),
        // Popups and floating panes (TC.6).
        row(
            "display-popup",
            PassThrough,
            Translate(display_popup),
            &[],
            "fe_4/popup-env",
        ),
        row(
            "display-popup -s",
            PassThrough,
            Translate(display_popup),
            SEMANTIC,
            "fe_4/popup-styles",
        ),
        row(
            "display-popup -S",
            PassThrough,
            Translate(display_popup),
            SEMANTIC,
            "fe_4/popup-styles",
        ),
        row(
            "new-pane",
            PassThrough,
            Translate(new_pane),
            &[],
            "tc_6/new-pane-fzf",
        ),
        pass("wait-for", "fe_4/fzf"),
        refuse("wait-for -L", UNUSED),
        refuse("wait-for -U", UNUSED),
        // Menus, prompts and the pane labels (TC.7).
        row(
            "display-menu",
            PassThrough,
            Translate(display_menu),
            &[],
            "fe_4/menu-over-popup",
        ),
        row(
            "display-menu -b",
            PassThrough,
            Translate(display_menu),
            SEMANTIC,
            "",
        ),
        row(
            "display-menu -O",
            PassThrough,
            Translate(display_menu),
            SEMANTIC,
            "",
        ),
        pass("display-panes", ""),
        pass("confirm-before", "tc_7/confirm-before"),
        pass("choose-tree", ""),
        pass("customize-mode", ""),
    ]
}

/// This area's argument rewrites, applied before any row matches.
pub(crate) fn rewrites() -> Vec<Rewrite> {
    Vec::new()
}
