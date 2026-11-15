#![no_main]
//! The command-string splitter plus `parse`: no input may panic, and a
//! split that succeeds must split to the same words once its words are
//! escaped and joined again.

use grofe::cmd::parse::{parse, split_command_string};
use grofe::util::quote::tmux_arg;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(commands) = split_command_string(text) else {
        return;
    };
    for words in &commands {
        let _ = parse(words);
    }
    // A word holding NUL (`\000`) cannot exist in tmux, whose strings are C
    // strings, and `tmux_arg` prints it as the `\0` tmux's parser rejects.
    // A word with `"` and none of ` #';${}%` is single-quoted, where
    // `tmux_arg` (tmux's `args_escape`) still escapes `\\`, control
    // characters and a leading `~`, which a single-quoted word reads
    // literally: it is for display, not for re-reading.
    // A word `;` prints as `\;`, which the splitter drops when it ends a
    // command (as tmux does).
    // A word holding NUL cannot exist in tmux, whose strings are C strings.
    let lossy = |w: &String| {
        w == ";"
            || w.contains('\0')
            || (w.contains('"') && !w.contains([' ', '#', '\'', ';', '$', '{', '}', '%']))
    };
    if commands.iter().flatten().any(lossy) {
        return;
    }
    let joined = commands
        .iter()
        .map(|words| {
            words
                .iter()
                .map(|w| tmux_arg(w))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" ; ");
    assert_eq!(
        split_command_string(&joined).as_ref(),
        Ok(&commands),
        "round trip of {text:?} via {joined:?}"
    );
});
