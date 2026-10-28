//! Copy-mode and buffer rows (TM.7): `copy-mode` and its flags,
//! `send-keys -X` with `-F` and `-N`, `command-prompt -T search`, and the
//! buffer verbs, `load-buffer` and `save-buffer` included.
//!
//! Rows only: the schema and the dispatcher are `mod.rs`'s. Every verb here has a native verb of the same
//! name and flag letters (TM.1 to TM.6), so each row passes through, and
//! the native commands answer as tmux 3.7b does (recorded, `golden/tm_7/`).
//! TP.11's three rows (`send-keys -X`, `copy-mode`, `paste-buffer`, for
//! tmux-yank) are part of this set. A `copy-pipe*` command string inside
//! `bind -T copy-mode-vi` needs no
//! row of its own: the binding keeps its dialect (spec §4.5) and runs
//! through the `send-keys -X` row each time. The copy options
//! (`mode-keys`, `set-clipboard`, `copy-command`, `word-separators`,
//! `wrap-search`, `buffer-limit`) are in `mod.rs`'s one option-name table.

use super::{FlagMark, GrofeTarget, MappingKind, Row};

const SEMANTIC: &[FlagMark] = &[FlagMark::Semantic];
const SEMANTIC_2: &[FlagMark] = &[FlagMark::Semantic, FlagMark::Semantic];

const fn pass(
    tmux_form: &'static str,
    verb: &'static str,
    flags: &'static [FlagMark],
    golden_id: &'static str,
) -> Row {
    Row {
        tmux_form,
        kind: MappingKind::PassThrough,
        target: GrofeTarget::Native(verb),
        flags,
        milestone: "M5",
        golden_id,
    }
}

/// This area's rows.
pub(crate) fn rows() -> Vec<Row> {
    vec![
        // Copy mode (TM.1, TM.6, B1d): `-u` a page up, `-d` a page down,
        // `-e` leave at the bottom, `-H` no position indicator, `-M` a drag
        // from a mouse key (and nothing outside one, as tmux), `-q` leave
        // every mode, `-s` copy another pane, `-S` a scrollbar drag.
        pass("copy-mode", "copy-mode", &[], "tm_7/forms"),
        pass("copy-mode -u", "copy-mode", SEMANTIC, "tm_7/forms"),
        pass("copy-mode -e", "copy-mode", SEMANTIC, "tm_7/forms"),
        pass("copy-mode -H", "copy-mode", SEMANTIC, "tm_7/forms"),
        pass("copy-mode -M", "copy-mode", SEMANTIC, "tm_7/forms"),
        pass("copy-mode -d", "copy-mode", SEMANTIC, "b1d/copy-flags"),
        pass("copy-mode -q", "copy-mode", SEMANTIC, "b1d/copy-flags"),
        pass("copy-mode -s", "copy-mode", SEMANTIC, "b1d/copy-flags"),
        pass("copy-mode -S", "copy-mode", SEMANTIC, "b1d/copy-flags"),
        // Copy-mode commands (TM.1 to TM.5): `-F` expands a search's
        // string, `-N` repeats.
        pass("send-keys -X", "send-keys", SEMANTIC, "tm_7/forms"),
        pass("send-keys -F -X", "send-keys", SEMANTIC_2, "tm_7/forms"),
        pass("send-keys -N -X", "send-keys", SEMANTIC_2, "tm_7/forms"),
        // The search prompt's history type (TM.3).
        pass(
            "command-prompt -T",
            "command-prompt",
            SEMANTIC,
            "tm_7/copy_mode/slash",
        ),
        // Buffers (TM.4), with tmux's default output.
        pass("set-buffer", "set-buffer", &[], "tm_7/buffers"),
        pass("show-buffer", "show-buffer", &[], "tm_7/buffers"),
        pass("list-buffers", "list-buffers", &[], "tm_7/buffers"),
        pass("delete-buffer", "delete-buffer", &[], "tm_7/buffers"),
        pass("paste-buffer", "paste-buffer", &[], "tm_7/buffers"),
        pass("choose-buffer", "choose-buffer", &[], "tm_7/choose-buffer"),
        pass("load-buffer", "load-buffer", &[], "tm_7/load-save"),
        pass("save-buffer", "save-buffer", &[], "tm_7/load-save"),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::{dispatch_command, row_for, table};
    use crate::cmd::Command;
    use crate::protocol::{ClientCtx, Dialect};

    fn one(words: &[&str]) -> Result<Vec<Command>, String> {
        let ctx = ClientCtx {
            cwd: PathBuf::from("/"),
            pane: None,
            env: Vec::new(),
            client: None,
            dialect: Dialect::Tmux,
        };
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        dispatch_command(&argv, &ctx).map_err(|e| e.0)
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    /// Every form the stock `copy-mode-vi` table, tmux-yank and the
    /// navigator send maps onto its native command.
    #[test]
    fn the_copy_forms_reach_their_native_commands() {
        for words in [
            &["send", "-X", "copy-pipe-and-cancel", "pbcopy"][..],
            &[
                "send-keys",
                "-FX",
                "search-backward",
                "--",
                "#{copy_cursor_word}",
            ],
            &["send", "-X", "-N", "5", "scroll-up"],
            &["copy-mode", "-u"],
            &["copy-mode", "-e", "-t", "%0"],
            &["copy-mode", "-H"],
            &["copy-mode", "-M"],
            &["copy-mode", "-d", "-e"],
            &["copy-mode", "-q", "-t", "%0"],
            &["copy-mode", "-s", "%1", "-t", "%0"],
            &["copy-mode", "-S"],
            &[
                "command-prompt",
                "-T",
                "search",
                "-p",
                "(search up)",
                "send -X search-backward -- '%%'",
            ],
            &["setb", "-b", "a", "x"],
            &["showb"],
            &["lsb", "-F", "#{buffer_name}"],
            &["deleteb", "-b", "a"],
            &["pasteb", "-p", "-r"],
            &["choose-buffer", "-Z"],
            &["loadb", "-w", "-b", "x", "-t", "c", "-"],
            &["saveb", "-a", "-b", "x", "out.txt"],
        ] {
            let got = one(words).unwrap_or_else(|e| panic!("{words:?}: {e}"));
            assert_eq!(got.len(), 1, "{words:?}");
            let row = row_for(&argv(words)).unwrap();
            assert_eq!(row.milestone, "M5", "{words:?}: {}", row.tmux_form);
        }
    }

    /// No M5 refusal is left: every M5 row maps or refuses for good.
    #[test]
    fn no_copy_form_waits_for_m5_any_more() {
        for words in [
            &["send", "-X", "cancel"][..],
            &["copy-mode"],
            &["set-buffer", "x"],
            &["paste-buffer"],
            &["choose-buffer"],
            &["list-buffers"],
            &["show-buffer"],
            &["delete-buffer"],
        ] {
            let row = row_for(&argv(words)).unwrap();
            assert!(
                !matches!(row.target, super::GrofeTarget::Unsupported(_)),
                "{words:?} still refused by {}",
                row.tmux_form
            );
        }
    }

    /// Each row's golden exists.
    #[test]
    fn every_row_names_a_recorded_golden() {
        let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compat/golden");
        for row in table().iter().filter(|r| r.golden_id.starts_with("tm_7/")) {
            let path = golden.join(format!("{}.golden", row.golden_id));
            assert!(path.exists(), "{}: no {}", row.tmux_form, path.display());
        }
    }
}
