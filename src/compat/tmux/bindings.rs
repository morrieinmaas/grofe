//! Binding and overlay rows (FE.4): `if-shell`, `command-alias`, popup and
//! menu flags, `new-pane` and `wait-for` (fzf's `--tmux`, M36-R1), the mouse
//! argv forms.
//!
//! Rows only: the schema, the dispatcher and the helpers a
//! translation uses ([`super::Invocation`]) are `mod.rs`'s. The most
//! specific matching row decides, and between rows as specific this
//! file's come before the core rows, so a row here naming a core form
//! takes it over.
//!
//! Every verb here has a native verb of the same name and flag letters
//! (TC.6, TC.7, TC.9, TC.10), so most rows pass through. A translation
//! only drops the style flags grofe draws no differently, each noted in
//! `compat-miss.log` ([`super::Invocation::ignored`]). Stored commands (an
//! `if-shell` branch, a menu item, a `confirm-before` command) keep the
//! request's dialect, so a tmux branch such as `neww` runs as tmux's.

use super::{FlagMark, GrofeTarget, Invocation, Row, TmuxError};
use crate::cmd::Command;

const SEMANTIC: &[FlagMark] = &[FlagMark::Semantic];

#[cfg_attr(not(test), allow(unused_variables))]
const fn row(
    tmux_form: &'static str,
    target: GrofeTarget,
    flags: &'static [FlagMark],
    golden_id: &'static str,
) -> Row {
    Row {
        tmux_form,
        target,
        #[cfg(test)]
        flags,
        milestone: "M3.6",
        #[cfg(test)]
        golden_id,
    }
}

const fn pass(verb: &'static str, golden_id: &'static str) -> Row {
    row(verb, GrofeTarget::Native(verb), &[], golden_id)
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
    vec![
        // The conditional (TC.10): the format form on the actor, the shell
        // form off it; the branch runs in the request's dialect. A bare
        // `=` target is the pointer's inside a mouse key (M36-R3).
        row(
            "if-shell",
            GrofeTarget::Native("if-shell"),
            &[],
            "fe_4/if-format",
        ),
        row(
            "if-shell -F",
            GrofeTarget::Native("if-shell"),
            SEMANTIC,
            "fe_4/if-format",
        ),
        // `command-alias` entries are grofe aliases (TC.7): the native
        // dispatcher expands them before any dialect parses the request.
        row(
            "set-option command-alias",
            Translate(|inv| super::option_command(inv, "set-option", false)),
            &[],
            "fe_4/aliases",
        ),
        // Popups and floating panes (TC.6).
        row(
            "display-popup",
            Translate(display_popup),
            &[],
            "fe_4/popup-env",
        ),
        row(
            "display-popup -s",
            Translate(display_popup),
            SEMANTIC,
            "fe_4/popup-styles",
        ),
        row(
            "display-popup -S",
            Translate(display_popup),
            SEMANTIC,
            "fe_4/popup-styles",
        ),
        row("new-pane", Translate(new_pane), &[], "tc_6/new-pane-fzf"),
        pass("wait-for", "fe_4/fzf"),
        // Menus, prompts and the pane labels (TC.7).
        row(
            "display-menu",
            Translate(display_menu),
            &[],
            "fe_4/menu-over-popup",
        ),
        row("display-menu -b", Translate(display_menu), SEMANTIC, ""),
        row("display-menu -O", Translate(display_menu), SEMANTIC, ""),
        pass("display-panes", ""),
        pass("confirm-before", "tc_7/confirm-before"),
        pass("choose-tree", ""),
        pass("customize-mode", ""),
    ]
}
