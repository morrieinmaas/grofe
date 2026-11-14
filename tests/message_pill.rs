//! Messages and the prompt in grofe identity's look (`message-pill`): a
//! message is a pill centred in the status line between its left and right
//! parts, which stay, with an icon for its level; the prompt is a pill on
//! the status line, or with `prompt-position = "centre"` a rounded box a
//! third down the window. tmux identity keeps tmux's full-width bar.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::time::Duration;

use common::TestServer;
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);
const PREFIX: &str = "\x02";
const ROWS: u16 = 10;

/// A server on grofe's built-in defaults with `config` as its
/// `config.toml`, a session `s` running cat, `F5` bound to a command that
/// fails and `F6` to a `display-message`, and a client attached at 60x10.
fn attached(config: &str) -> (TestServer, Harness) {
    let srv = TestServer::builder().frames(true).unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("config.toml"), config).expect("write config");
    srv.cmd(&["start-server"]).expect("start-server");
    let y = (ROWS - 1).to_string();
    let cmds: [&[&str]; 6] = [
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "60",
            "-y",
            &y,
            "exec cat",
        ],
        &["set", "-g", "status-left", "LEFT"],
        &["set", "-g", "status-right", "RIGHT"],
        &["set", "-g", "prefix", "C-b"],
        &["bind", "-n", "F5", "select-window", "-t", ":9"],
        &["bind", "-n", "F6", "display-message", "all saved"],
    ];
    for args in cmds {
        srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    let client = Harness::spawn_attach_with_env(srv.command(), "s", &[], 60, ROWS);
    (srv, client)
}

fn status(t: &str) -> &str {
    t.lines().nth(usize::from(ROWS) - 1).unwrap_or_default()
}

#[test]
fn an_error_is_a_pill_between_the_status_parts() {
    let (_srv, mut h) = attached("");
    h.wait_until("the status line", WAIT, |t| status(t).starts_with("LEFT"));
    h.send("\x1b[15~");
    h.wait_until("the error pill", WAIT, |t| {
        status(t).contains("( ✗ Can't find window")
    });
    let shown = h.text();
    let row = status(&shown);
    assert!(row.starts_with("LEFT"), "{shown}");
    assert!(row.trim_end().ends_with("RIGHT"), "{shown}");
}

#[test]
fn an_error_times_out_without_a_key() {
    let (srv, mut h) = attached("");
    srv.cmd(&["set", "-g", "display-time", "300"])
        .expect("display-time");
    h.wait_until("the status line", WAIT, |t| status(t).starts_with("LEFT"));
    h.send("\x1b[15~");
    h.wait_until("the error pill", WAIT, |t| status(t).contains('✗'));
    h.wait_until("the pill gone", WAIT, |t| !status(t).contains('✗'));
}

#[test]
fn display_message_is_an_info_pill() {
    let (_srv, mut h) = attached("");
    h.wait_until("the status line", WAIT, |t| status(t).starts_with("LEFT"));
    h.send("\x1b[17~");
    h.wait_until("the info pill", WAIT, |t| {
        status(t).contains("( ℹ all saved )")
    });
}

#[test]
fn tmux_identity_keeps_the_full_width_bar() {
    let (_srv, mut h) = attached("compat-identity = \"tmux\"\n");
    h.wait_until("the status line", WAIT, |t| status(t).starts_with("LEFT"));
    h.send("\x1b[15~");
    h.wait_until("the message over the row", WAIT, |t| {
        status(t).starts_with("Can't find window")
    });
    assert!(!h.text().contains('✗'), "{}", h.text());
}

#[test]
fn the_prompt_is_a_pill_on_the_status_line() {
    let (_srv, mut h) = attached("");
    h.wait_until("the status line", WAIT, |t| status(t).starts_with("LEFT"));
    h.send(format!("{PREFIX}:"));
    h.wait_until("the prompt pill", WAIT, |t| status(t).contains("( :"));
    h.send("abc");
    h.wait_until("the typed text", WAIT, |t| status(t).contains("( :abc"));
    let shown = h.text();
    let row = status(&shown);
    assert!(row.starts_with("LEFT"), "{shown}");
    assert!(row.trim_end().ends_with("RIGHT"), "{shown}");
    assert_eq!(h.cursor().1, ROWS - 1);
    h.send("\x1b");
    h.wait_until("the prompt gone", WAIT, |t| !status(t).contains(":abc"));
}

#[test]
fn prompt_position_centre_opens_a_box_a_third_down() {
    let (_srv, mut h) = attached("prompt-position = \"centre\"\n");
    h.wait_until("the status line", WAIT, |t| status(t).starts_with("LEFT"));
    h.send(format!("{PREFIX}:"));
    let top = usize::from(ROWS / 3);
    h.wait_until("the prompt box", WAIT, |t| {
        t.lines().nth(top).is_some_and(|r| r.contains("╭─ ( : ) ─"))
    });
    h.send("abc");
    h.wait_until("the typed text", WAIT, |t| {
        t.lines().nth(top + 1).is_some_and(|r| r.contains("│ abc"))
    });
    let shown = h.text();
    let rows: Vec<&str> = shown.lines().collect();
    assert!(rows[top + 2].contains("╰──"), "{shown}");
    // The box is 60 columns wide, clamped to the 60 column window.
    assert!(rows[top].starts_with('╭'), "{shown}");
    assert!(rows[top].trim_end().ends_with('╮'), "{shown}");
    assert!(!status(&shown).contains(":abc"), "{shown}");
    assert_eq!(h.cursor(), (5, top as u16 + 1));
    h.send("\x1b");
    h.wait_until("the box gone", WAIT, |t| !t.contains("( : )"));
}
