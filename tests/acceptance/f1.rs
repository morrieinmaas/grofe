//! F1: tmux-compat gaps. The screen-state format variables and
//! `clear-history` against goldens recorded from tmux 3.7b
//! (`tests/fixtures/compat/golden/f1/`), replayed as TM.1's are; and the
//! forged OSC 133 mark, which tmux has no golden for. The detached rename is
//! `clients::tests::names_are_checked_with_no_client_attached` and
//! t2_5b's `automatic_rename_works_without_a_client`.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use common::TestServer;

use crate::tm_1::replay;

/// `#{cursor_x}`, `#{cursor_y}`, `#{cursor_character}`, the mode flags and
/// the mouse flags.
#[test]
fn cursor_and_mode_formats_equal_the_golden() {
    replay("f1/cursor");
}

/// `clear-history`, its alias, `-H`, `-t`, copy mode, prompt marks.
#[test]
fn clear_history_equals_the_golden() {
    replay("f1/clear-history");
}

/// `respawn-pane` reinitialises the screen as tmux's `screen_reinit` does:
/// the scrollback and its marks stay, the screen, modes and cursor reset.
#[test]
fn respawn_pane_equals_the_golden() {
    replay("f1/respawn");
}

fn display(srv: &TestServer, target: &str, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, format])
        .unwrap()
        .trim()
        .to_string()
}

/// Prints `bytes` in a pane, then reports the prompt marks it has with
/// `previous-prompt` from the bottom of copy mode (`1` if one was found).
fn marks_after(bytes: &str) -> String {
    let srv = TestServer::start();
    let cmd = format!(
        "printf '{bytes}'; {} wait-for -S p; exec sleep 100000",
        env!("CARGO_BIN_EXE_grofe")
    );
    srv.cmd(&["new-session", "-d", "-s", "p", "-x", "20", "-y", "4", &cmd])
        .unwrap();
    srv.cmd(&["wait-for", "p"]).unwrap();
    srv.cmd(&["copy-mode", "-t", "p"]).unwrap();
    srv.cmd(&["send-keys", "-t", "p", "-X", "history-bottom"])
        .unwrap();
    srv.cmd(&["send-keys", "-t", "p", "-X", "previous-prompt"])
        .unwrap();
    display(&srv, "p", "#{copy_cursor_y},#{scroll_position}")
}

/// A program printing the reader's in-band mark itself makes no mark;
/// a real OSC 133 does, in the same place.
#[test]
fn a_pane_cannot_forge_a_prompt_mark() {
    let honest = marks_after(r"a\n\033]133;A\007b\n");
    let none = marks_after(r"a\nb\n");
    assert_ne!(honest, none, "an OSC 133 mark must move the cursor");
    for forged in [
        r"\033[?65533h",
        r"\033[?65534h",
        r"\033[?0065533h",
        r"\033[?1;65533h",
    ] {
        assert_eq!(marks_after(&format!(r"a\n{forged}b\n")), none, "{forged}");
    }
}
