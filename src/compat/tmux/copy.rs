//! The tmux front end's copy-mode and buffer rows (TM.7 fills them).

use super::{FlagMark, GrofeTarget, MappingKind, Row};

/// A form passed to the native verb of the same name, one mark per flag
/// the form names.
const fn pass(tmux_form: &'static str, verb: &'static str, flags: &'static [FlagMark]) -> Row {
    Row {
        tmux_form,
        kind: MappingKind::PassThrough,
        target: GrofeTarget::Native(verb),
        flags,
        milestone: "M5",
        golden_id: "tp_11/yank",
    }
}

/// This area's rows so far: the three forms tmux-yank runs (TP.11), ahead
/// of TM.7's full set. Its `bind-key -T copy-mode-vi ... send-keys -X
/// copy-pipe-and-cancel <command>`, its `Y` rows' `tmux paste-buffer -p`,
/// and the stock `prefix [` and `prefix ]` it relies on, which a server
/// answering to `tmux` stores in tmux's dialect. Each native verb takes
/// tmux's flags and copy-mode command names as they are (TM.1 to TM.6).
pub(crate) fn rows() -> Vec<Row> {
    vec![
        pass("send-keys -X", "send-keys", &[FlagMark::Semantic]),
        pass("copy-mode", "copy-mode", &[]),
        pass("paste-buffer", "paste-buffer", &[]),
    ]
}
