//! B1c: the mouse on a pane's border title. With `pane-border-status` on
//! and `mouse` on, the default `pane-border-format` draws `[z]` and `[x]`
//! buttons (`#[range=control|8]` and `control|9`); tmux 3.7b's stock
//! `MouseDown1Control8` zooms the pane and `MouseDown1Control9` asks to
//! kill it. Reports are typed into the attach pty as SGR 1006.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::time::Duration;

use common::{TestServer, wait_until};

use crate::harness::Harness;

const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const WAIT: Duration = Duration::from_secs(10);

fn start() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        "exec sh",
    ])
    .expect("new-session");
    srv.cmd(&["set", "-g", "pane-border-status", "top"])
        .expect("set");
    srv.cmd(&["split-window", "-h", "-t", "main", "exec sh"])
        .expect("split");
    // A pane's title starts as the host name, and a long one (a macOS
    // runner's) takes the room the right-aligned buttons need: tmux's
    // format_draw trims the right part before the left. A short title
    // keeps them on screen.
    let panes = srv
        .cmd(&["list-panes", "-t", "main", "-F", "#{pane_id}"])
        .expect("list-panes");
    for pane in panes.lines() {
        srv.cmd(&["select-pane", "-t", pane, "-T", "sh"])
            .expect("title");
    }
    srv.cmd(&["select-pane", "-t", "main:.1"]).expect("select");
    srv
}

fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_for_output(MOUSE_ON, 0);
    a
}

fn click(a: &mut Harness, x: u16, y: u16) {
    a.send(format!("\x1b[<0;{x};{y}M"));
    a.send(format!("\x1b[<0;{x};{y}m"));
}

/// The 1-based cell of the first `needle` on screen row `row`.
fn col_of(a: &mut Harness, row: usize, needle: &str) -> u16 {
    let text = a.text();
    let line = text.lines().nth(row).unwrap_or_default().to_string();
    let at = line
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} in {line:?}"));
    line[..at].chars().count() as u16 + 1
}

fn zoomed(srv: &TestServer) -> String {
    srv.cmd(&[
        "display-message",
        "-p",
        "-t",
        "main",
        "#{window_zoomed_flag}",
    ])
    .unwrap_or_default()
    .trim()
    .to_string()
}

#[test]
fn the_zoom_button_in_a_pane_title_zooms_the_pane() {
    let srv = start();
    let mut a = attach(&srv);
    a.wait_for("[z]");
    assert_eq!(zoomed(&srv), "0");
    let x = col_of(&mut a, 0, "[z]");
    click(&mut a, x + 1, 1);
    let got = wait_until(|| zoomed(&srv) == "1", WAIT);
    assert!(got.is_ok(), "zoomed: {}", zoomed(&srv));
    // The zoomed pane's title now offers `[u]`, to unzoom.
    a.wait_for("[u]");
    let x = col_of(&mut a, 0, "[u]");
    std::thread::sleep(Duration::from_millis(400));
    click(&mut a, x + 1, 1);
    let got = wait_until(|| zoomed(&srv) == "0", WAIT);
    assert!(got.is_ok(), "zoomed: {}", zoomed(&srv));
}

#[test]
fn the_close_button_asks_before_killing_the_pane() {
    let srv = start();
    let mut a = attach(&srv);
    a.wait_for("[x]");
    let x = col_of(&mut a, 0, "[x]");
    click(&mut a, x + 1, 1);
    a.wait_for("Kill pane 1?");
    a.send("n");
    let panes = srv
        .cmd(&["list-panes", "-t", "main", "-F", "#{pane_id}"])
        .unwrap_or_default();
    assert_eq!(panes.lines().count(), 2, "{panes}");
}

#[test]
fn the_close_buttons_menu_kills_the_pane_it_was_clicked_on() {
    let srv = start();
    let mut a = attach(&srv);
    a.wait_for("[x]");
    let x = col_of(&mut a, 0, "[x]");
    click(&mut a, x + 1, 1);
    a.wait_for("Kill pane 1?");
    a.send("y");
    let got = wait_until(
        || {
            srv.cmd(&["list-panes", "-t", "main", "-F", "#{pane_id}"])
                .unwrap_or_default()
                .lines()
                .count()
                == 1
        },
        WAIT,
    );
    assert!(got.is_ok(), "the pane is still there");
}

#[test]
fn a_new_pane_asking_for_the_background_at_once_is_told_the_window_style() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "60",
        "-y",
        "8",
        "sleep 60",
    ])
    .expect("new-session");
    srv.cmd(&["set", "-g", "window-style", "fg=#102030,bg=#405060"])
        .expect("set");
    // The query goes out the moment the pane starts, before the server's
    // next sync of the pane colours.
    srv.cmd(&[
        "new-window",
        "-t",
        "main",
        "sh -c 'stty raw -echo; printf \"\\033]11;?\\033\\\\\"; \
         dd bs=1 count=25 2>/dev/null | tr -d \"\\033\"; echo; sleep 30'",
    ])
    .expect("new-window");
    let want = "]11;rgb:4040/5050/6060";
    srv.wait_for(|| srv.capture("main:2").contains(want), WAIT)
        .unwrap_or_else(|e| {
            let all = srv.cmd(&[
                "list-panes",
                "-a",
                "-F",
                "#{window_index}.#{pane_index} #{pane_dead} #{pane_current_command}",
            ]);
            panic!("{e}: {all:?} {:?}", srv.capture("main:2"))
        });
}
