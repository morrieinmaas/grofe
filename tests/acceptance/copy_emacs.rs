//! The stock emacs `copy-mode` table under tmux identity. tmux 3.7b makes
//! `mode-keys` `emacs` unless `$VISUAL` or `$EDITOR` names a vi, so a tmux
//! user who never set `mode-keys` gets this table on `prefix [`. The rows
//! themselves are checked against tmux 3.7b's `list-keys -T copy-mode`
//! (`golden/b2b/emacs-keys`) and their keys against its other `b2b/emacs-*`
//! goldens; these tests check, through an attached client and with nothing
//! configured, that the stock defaults reach the table: `C-Space` starts a
//! selection, `M-w` copies it into a buffer, `C-g` clears it, `q`, `C-c` and
//! a lone Escape leave, and `C-s` and `C-r` open the search prompts.
//!
//! The server is started through a `tmux` symlink on a private `-S`
//! socket, in the harness's own directories.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::process::Command;

use common::{CMD_TIMEOUT, TestServer, run_bounded, wait_until};

use crate::harness::Harness;

const C_B: &str = "\x02";
/// Out of the mode `selection_present` and `search_present` are empty.
const STATE: &str = "#{pane_in_mode},#{?selection_present,1,0},#{?search_present,1,0}";

/// `tmux -S <socket>` through a symlink to grofe, with the harness's
/// environment and `editor` as `$EDITOR` (no `$VISUAL`).
fn tmux_cmd(srv: &TestServer, editor: Option<&str>) -> Command {
    let link = srv.home.join("tmux-link/tmux");
    if !link.exists() {
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    }
    let mut cmd = Command::new(&link);
    for (k, v) in srv.command().get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.env_remove("VISUAL")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    match editor {
        Some(e) => cmd.env("EDITOR", e),
        None => cmd.env_remove("EDITOR"),
    };
    cmd.current_dir(&srv.home).arg("-S").arg(&srv.socket);
    cmd
}

fn tmux(srv: &TestServer, args: &[&str]) -> String {
    let mut cmd = tmux_cmd(srv, None);
    cmd.args(args);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "tmux {args:?}: {}", out.stderr);
    out.stdout
}

fn state(srv: &TestServer) -> String {
    tmux(srv, &["display", "-p", "-t", "main", STATE])
        .trim()
        .to_string()
}

fn wait_state(srv: &TestServer, want: &str) {
    let ok = wait_until(|| state(srv) == want, CMD_TIMEOUT);
    assert!(ok.is_ok(), "want {want}, have {}", state(srv));
}

/// A tmux-identity server started with `editor` as `$EDITOR`, the copy
/// fixture printed in session `main`, and a client attached to it.
fn start(editor: Option<&str>) -> (TestServer, Harness) {
    let srv = TestServer::unstarted();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/copy/fixture.txt");
    let pane = format!("cat {}; exec sleep 100000", fixture.display());
    let mut cmd = tmux_cmd(&srv, editor);
    cmd.args([
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "24",
        &pane,
    ]);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "new-session: {}", out.stderr);
    let ready = wait_until(
        || srv.capture("main").contains("last line of text"),
        CMD_TIMEOUT,
    );
    assert!(ready.is_ok(), "the fixture never printed");
    let mut attach = tmux_cmd(&srv, editor);
    attach
        .args(["attach", "-t", "main"])
        .env("TERM", "xterm-256color");
    let mut a = Harness::spawn(attach, 80, 25);
    a.wait_for("last line of text");
    (srv, a)
}

fn enter(srv: &TestServer, a: &mut Harness) {
    a.send(format!("{C_B}["));
    wait_state(srv, "1,0,0");
}

#[test]
fn stock_defaults_select_the_emacs_table() {
    let (srv, mut a) = start(None);
    assert_eq!(tmux(&srv, &["show", "-gwv", "mode-keys"]), "emacs\n");
    let rows = tmux(&srv, &["list-keys", "-T", "copy-mode"]);
    assert_eq!(rows.lines().count(), 75, "{rows}");
    enter(&srv, &mut a);
    // C-Space is unbound in copy-mode-vi: a selection proves the table.
    // As in tmux, a selection is present once the cursor leaves its start.
    a.send("\x10\x00\x06");
    wait_state(&srv, "1,1,0");
}

#[test]
fn c_space_then_m_w_copies_the_selection_into_a_buffer() {
    let (srv, mut a) = start(None);
    enter(&srv, &mut a);
    // C-p up to "last line of text", C-a to its start, C-Space, four C-f,
    // M-w.
    a.send("\x10\x01\x00\x06\x06\x06\x06");
    let x = wait_until(
        || tmux(&srv, &["display", "-p", "-t", "main", "#{copy_cursor_x}"]) == "4\n",
        CMD_TIMEOUT,
    );
    assert!(x.is_ok(), "C-f did not move the cursor");
    wait_state(&srv, "1,1,0");
    a.send("\x1bw");
    wait_state(&srv, "0,0,0");
    assert_eq!(tmux(&srv, &["show-buffer"]), "last");
}

#[test]
fn c_g_clears_and_q_c_c_and_escape_leave() {
    let (srv, mut a) = start(None);
    enter(&srv, &mut a);
    a.send("\x10\x00\x06");
    wait_state(&srv, "1,1,0");
    a.send("\x07");
    wait_state(&srv, "1,0,0");
    a.send("q");
    wait_state(&srv, "0,0,0");
    enter(&srv, &mut a);
    a.send("\x03");
    wait_state(&srv, "0,0,0");
    enter(&srv, &mut a);
    a.send("\x1b");
    wait_state(&srv, "0,0,0");
    assert!(tmux(&srv, &["list-buffers"]).is_empty());
}

#[test]
fn c_s_and_c_r_open_the_search_prompts() {
    let (srv, mut a) = start(None);
    enter(&srv, &mut a);
    a.send("\x12");
    a.wait_for("(search up)");
    a.send("foo\r");
    wait_state(&srv, "1,0,1");
    a.send("q");
    wait_state(&srv, "0,0,0");
    enter(&srv, &mut a);
    a.send("\x13");
    a.wait_for("(search down)");
    a.send("\x1b");
    a.wait_until("the prompt closed", CMD_TIMEOUT, |t| {
        !t.contains("(search down)")
    });
}

#[test]
fn a_vi_editor_selects_the_vi_table_as_in_tmux() {
    let (srv, mut a) = start(Some("/usr/bin/nvim"));
    assert_eq!(tmux(&srv, &["show", "-gwv", "mode-keys"]), "vi\n");
    enter(&srv, &mut a);
    a.send("k l");
    wait_state(&srv, "1,1,0");
}
