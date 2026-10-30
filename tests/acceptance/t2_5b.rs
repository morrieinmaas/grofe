//! T2.5b: bindings and routing. The default key table driven through a
//! real `grofe attach` (splits, arrows, detach, send-prefix, errors in the
//! status line), TUIs behind it (Neovim, fzf, less, a kitty-keyboard fake),
//! automatic rename, and the remaining M2 commands through the CLI.

// t2_5a loads the same file; each scenario module keeps its own copy so
// neither depends on the other.
#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use common::{TestServer, wait_until};

use crate::harness::{Harness, require_tool};

const PREFIX: &str = "\x02";

/// A pane that prints a marker and then just waits.
const IDLE: &str = "printf 'idle-pane'; exec sleep 600";

fn attach(srv: &TestServer, target: &str) -> Command {
    let mut cmd = srv.command();
    cmd.args(["attach", "-t", target])
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM");
    cmd
}

fn new_session(srv: &TestServer, name: &str, command: &str) {
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        name,
        "-x",
        "80",
        "-y",
        "23",
        command,
    ])
    .unwrap_or_else(|e| panic!("new-session {name}: {e}"));
}

fn display(srv: &TestServer, target: &str, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, format])
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

fn wait_display(srv: &TestServer, target: &str, format: &str, want: &str) {
    if let Err(e) = wait_until(
        || display(srv, target, format) == want,
        Duration::from_secs(10),
    ) {
        panic!(
            "{format} on {target} never became {want:?} (is {:?}): {e}",
            display(srv, target, format)
        );
    }
}

fn pane_count(srv: &TestServer, target: &str) -> usize {
    srv.cmd(&["list-panes", "-t", target])
        .unwrap_or_default()
        .lines()
        .count()
}

fn wait_panes(srv: &TestServer, target: &str, want: usize) {
    if let Err(e) = wait_until(|| pane_count(srv, target) == want, Duration::from_secs(10)) {
        panic!(
            "{target} never had {want} panes (has {}): {e}",
            pane_count(srv, target)
        );
    }
}

fn window_names(srv: &TestServer, target: &str) -> String {
    srv.cmd(&["list-windows", "-t", target, "-F", "#{window_name}"])
        .unwrap_or_default()
}

fn client_session(srv: &TestServer) -> String {
    srv.cmd(&["list-clients", "-F", "#{session_name}"])
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

fn wait_client_session(srv: &TestServer, want: &str) {
    if let Err(e) = wait_until(|| client_session(srv) == want, Duration::from_secs(10)) {
        panic!(
            "the client never showed {want} (shows {:?}): {e}",
            client_session(srv)
        );
    }
}

// ---- the default key table, through an attached client ----

#[test]
fn prefix_splits_arrows_move_and_prefix_d_detaches() {
    let srv = TestServer::start();
    new_session(&srv, "main", IDLE);
    // Long enough that polling the CLI below never outlasts a repeat.
    srv.cmd(&["set-option", "-t", "main", "repeat-time", "2000"])
        .expect("repeat-time");
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("idle-pane");

    a.send(format!("{PREFIX}%"));
    wait_panes(&srv, "main", 2);
    // The new pane is on the right and active.
    wait_display(&srv, "main", "#{pane_index}", "2");

    a.send(format!("{PREFIX}\x1b[D"));
    wait_display(&srv, "main", "#{pane_index}", "1");
    a.send("\x1b[C");
    // Within repeat-time the arrow repeats without the prefix.
    wait_display(&srv, "main", "#{pane_index}", "2");

    // Still repeating: the prefix drops out of the repeat and is read
    // again as a fresh prefix, so `"` splits.
    a.send(format!("{PREFIX}\""));
    wait_panes(&srv, "main", 3);
    wait_display(&srv, "main", "#{pane_index}", "3");
    a.send(format!("{PREFIX}\x1b[A"));
    wait_display(&srv, "main", "#{pane_index}", "2");

    a.send(format!("{PREFIX}d"));
    a.wait_for("[detached (from session main)]");
    a.wait_exit();
    assert_eq!(srv.cmd(&["list-clients"]).expect("list-clients"), "");
}

#[test]
fn prefix_bindings_walk_windows_and_sessions() {
    let srv = TestServer::start();
    new_session(&srv, "main", IDLE);
    new_session(&srv, "other", IDLE);
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("[main]");

    a.send(format!("{PREFIX}c"));
    wait_display(&srv, "main:", "#{window_index}", "2");
    a.send(format!("{PREFIX}n"));
    wait_display(&srv, "main:", "#{window_index}", "1");
    a.send(format!("{PREFIX}p"));
    wait_display(&srv, "main:", "#{window_index}", "2");
    a.send(format!("{PREFIX}l"));
    wait_display(&srv, "main:", "#{window_index}", "1");
    a.send(format!("{PREFIX}2"));
    wait_display(&srv, "main:", "#{window_index}", "2");

    a.send(format!("{PREFIX})"));
    wait_client_session(&srv, "other");
    a.wait_for("[other]");
    a.send(format!("{PREFIX}L"));
    wait_client_session(&srv, "main");
    a.send(format!("{PREFIX}("));
    wait_client_session(&srv, "other");
}

/// `new-window` from `prefix c` opens in the active pane's cwd (spec §5.3).
#[test]
fn prefix_c_opens_the_window_in_the_active_panes_cwd() {
    let srv = TestServer::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let dir = fs::canonicalize(dir.path()).expect("canonicalize");
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-c",
        &dir.display().to_string(),
        IDLE,
    ])
    .expect("new-session");
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("idle-pane");
    a.send(format!("{PREFIX}c"));
    wait_display(&srv, "main:", "#{window_index}", "2");
    let path = display(&srv, "main:2", "#{pane_current_path}");
    assert_eq!(
        fs::canonicalize(&path).ok().as_deref(),
        Some(dir.as_path()),
        "{path}"
    );
}

/// Reads `n` raw bytes and prints them as hex, so a test sees exactly what
/// reached the pane.
fn hex_reader(push: &str, n: usize) -> String {
    format!(
        "printf '{push}ready\\n'; stty raw -echo; \
         b=$(dd bs=1 count={n} 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
         stty sane; printf 'got:%s:end' \"$b\"; exec sleep 600"
    )
}

#[test]
fn prefix_prefix_sends_the_prefix_to_the_pane() {
    let srv = TestServer::start();
    new_session(&srv, "main", &hex_reader("", 1));
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("ready");
    a.send(format!("{PREFIX}{PREFIX}"));
    a.wait_for("got:02:end");
}

#[test]
fn a_failing_binding_shows_its_error_in_the_status_line() {
    let srv = TestServer::start();
    new_session(&srv, "main", IDLE);
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("[main] 1:");
    // One window: there is no last window.
    a.send(format!("{PREFIX}l"));
    a.wait_for("No last window");
    let screen = a.text();
    let status = screen.lines().nth(23).unwrap_or_default();
    assert!(status.starts_with("No last window"), "status: {status:?}");
    // It goes away after display-time, as in tmux.
    a.wait_until("message cleared", Duration::from_secs(5), |t| {
        !t.contains("No last window") && t.contains("[main] 1:")
    });
}

// ---- TUIs behind the client ----

#[test]
fn neovim_edits_and_writes_a_file() {
    if !require_tool("nvim") {
        return;
    }
    let srv = TestServer::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("note.txt");
    new_session(
        &srv,
        "main",
        &format!("exec nvim --clean '{}'", file.display()),
    );
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("note.txt");
    a.send("ihello from grofe");
    a.wait_for("hello from grofe");
    a.send("\x1b");
    // Past escape-time and nvim's own ttimeout, so `:` is not Meta-:.
    sleep(Duration::from_millis(300));
    a.send(":wq\r");
    if let Err(e) = wait_until(
        || fs::read_to_string(&file).is_ok_and(|s| s == "hello from grofe\n"),
        Duration::from_secs(10),
    ) {
        panic!("{e}: file is {:?}", fs::read_to_string(&file));
    }
}

#[test]
fn fzf_picks_an_entry() {
    if !require_tool("fzf") {
        return;
    }
    let srv = TestServer::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("picked");
    new_session(
        &srv,
        "main",
        &format!(
            "printf 'a\\nb\\n' | fzf > '{}'; printf 'fzf-done'; exec sleep 600",
            out.display()
        ),
    );
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("2/2");
    a.send("b");
    a.wait_for("1/2");
    a.send("\r");
    a.wait_for("fzf-done");
    assert_eq!(fs::read_to_string(&out).expect("fzf output"), "b\n");
}

#[test]
fn less_jumps_to_the_end_and_quits() {
    if !require_tool("less") {
        return;
    }
    let srv = TestServer::start();
    new_session(
        &srv,
        "main",
        "seq 1 500 | LESS= less; printf 'less-done'; exec sleep 600",
    );
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_until("less shows the start", Duration::from_secs(10), |t| {
        t.lines().next().is_some_and(|l| l.trim_end() == "1")
    });
    a.send("G");
    a.wait_until("less shows the end", Duration::from_secs(10), |t| {
        t.lines().any(|l| l.trim_end() == "500")
    });
    a.send("q");
    a.wait_for("less-done");
}

/// With kitty flags 1 (disambiguate) pushed, Escape reaches the pane as
/// `CSI 27 u` (M2 addendum).
#[test]
fn a_kitty_pane_gets_escape_as_csi_u() {
    let srv = TestServer::start();
    new_session(&srv, "main", &hex_reader("\\033[>1u", 5));
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("ready");
    a.send("\x1b");
    a.wait_for("got:1b5b323775:end");
}

/// A TUI that pushed kitty flags 8 (every key as an escape code) gets a
/// bare `q` as `CSI 113 u`, and so quits on it.
#[test]
fn a_kitty_tui_quits_on_a_bare_q() {
    let srv = TestServer::start();
    let tui = "printf '\\033[>8uready\\n'; stty raw -echo; \
               b=$(dd bs=1 count=6 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
               stty sane; printf '\\033[<u'; \
               if [ \"$b\" = 1b5b31313375 ]; then printf 'tui-quit'; else printf 'bad:%s' \"$b\"; fi; \
               exec sleep 600";
    new_session(&srv, "main", tui);
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("ready");
    a.send("q");
    a.wait_for("tui-quit");
}

// ---- automatic rename ----

/// `automatic-rename` names the window `#{b:pane_current_path}`, and an
/// OSC title from the pane never freezes it (spec §4.5).
#[test]
fn cd_renames_the_window_even_after_an_osc_title() {
    let srv = TestServer::start();
    new_session(&srv, "main", "exec sh");
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("[main] 1:");
    a.send("cd /tmp\r");
    a.wait_for("1:tmp*");
    assert_eq!(window_names(&srv, "main"), "tmp\n");

    a.send("printf '\\033]2;SOME TITLE\\007\\033]0;OTHER\\007'; cd /usr\r");
    a.wait_for("1:usr*");
    assert_eq!(window_names(&srv, "main"), "usr\n");
}

#[test]
fn automatic_rename_works_without_a_client() {
    let srv = TestServer::start();
    new_session(&srv, "main", "exec sh");
    srv.cmd(&["send-keys", "-t", "main", "cd /tmp", "Enter"])
        .expect("send-keys");
    if let Err(e) = wait_until(
        || window_names(&srv, "main") == "tmp\n",
        Duration::from_secs(10),
    ) {
        panic!("{e}: names {:?}", window_names(&srv, "main"));
    }
}

#[test]
fn rename_window_and_new_window_n_stop_automatic_rename() {
    let srv = TestServer::start();
    new_session(&srv, "main", "exec sh");
    srv.cmd(&["rename-window", "-t", "main:1", "mine"])
        .expect("rename-window");
    assert_eq!(
        srv.cmd(&["show-options", "-w", "-t", "main:1", "automatic-rename"])
            .expect("show"),
        "automatic-rename off\n"
    );
    srv.cmd(&["send-keys", "-t", "main", "cd /tmp", "Enter"])
        .expect("send-keys");
    // Past a sample and a rename check.
    sleep(Duration::from_millis(2500));
    assert_eq!(window_names(&srv, "main"), "mine\n");

    srv.cmd(&["new-window", "-t", "main", "-n", "fixed", "exec sh"])
        .expect("new-window");
    assert_eq!(
        srv.cmd(&["show-options", "-w", "-t", "main:2", "automatic-rename"])
            .expect("show"),
        "automatic-rename off\n"
    );
    assert_eq!(window_names(&srv, "main"), "mine\nfixed\n");
}

// ---- the remaining M2 commands, through the CLI ----

#[test]
fn next_previous_and_last_window_walk_the_session() {
    let srv = TestServer::start();
    new_session(&srv, "main", IDLE);
    let index = || display(&srv, "main:", "#{window_index}");

    assert_eq!(
        srv.cmd(&["next-window", "-t", "main"]),
        Err("no next window\n".to_string())
    );
    assert_eq!(
        srv.cmd(&["previous-window", "-t", "main"]),
        Err("no previous window\n".to_string())
    );
    assert_eq!(
        srv.cmd(&["last-window", "-t", "main"]),
        Err("no last window\n".to_string())
    );

    srv.cmd(&["new-window", "-t", "main", IDLE]).expect("neww");
    srv.cmd(&["new-window", "-t", "main", IDLE]).expect("neww");
    assert_eq!(index(), "3");
    // As tmux: next and previous wrap around, last goes back and forth.
    srv.cmd(&["next-window", "-t", "main"]).expect("next");
    assert_eq!(index(), "1");
    srv.cmd(&["next-window", "-t", "main"]).expect("next");
    assert_eq!(index(), "2");
    srv.cmd(&["last-window", "-t", "main"]).expect("last");
    assert_eq!(index(), "1");
    srv.cmd(&["last-window", "-t", "main"]).expect("last");
    assert_eq!(index(), "2");
    srv.cmd(&["previous-window", "-t", "main"])
        .expect("previous");
    assert_eq!(index(), "1");
    srv.cmd(&["previous-window", "-t", "main"])
        .expect("previous");
    assert_eq!(index(), "3");
}

/// Killing the active window makes the last window active, as tmux does
/// (not the neighbour by position).
#[test]
fn killing_the_active_window_falls_back_to_the_last_window() {
    let srv = TestServer::start();
    new_session(&srv, "main", IDLE);
    srv.cmd(&["new-window", "-t", "main", IDLE]).expect("neww");
    srv.cmd(&["new-window", "-t", "main", IDLE]).expect("neww");
    srv.cmd(&["select-window", "-t", "main:1"]).expect("select");
    srv.cmd(&["select-window", "-t", "main:3"]).expect("select");
    srv.cmd(&["kill-window", "-t", "main:3"]).expect("kill");
    assert_eq!(display(&srv, "main:", "#{window_index}"), "1");
}

/// The last window is a stack, as in tmux: with windows 1, 2 and 3,
/// selecting 1 then 3 and killing 1 leaves 2 as the last window (with
/// `renumber-windows off`, so the indexes stay put).
#[test]
fn the_last_window_is_a_stack() {
    let srv = TestServer::start();
    new_session(&srv, "main", IDLE);
    srv.cmd(&["set", "-g", "renumber-windows", "off"])
        .expect("renumber off");
    srv.cmd(&["new-window", "-t", "main", IDLE]).expect("neww");
    srv.cmd(&["new-window", "-t", "main", IDLE]).expect("neww");
    srv.cmd(&["select-window", "-t", "main:1"]).expect("select");
    srv.cmd(&["select-window", "-t", "main:3"]).expect("select");
    srv.cmd(&["kill-window", "-t", "main:1"]).expect("kill");
    srv.cmd(&["last-window", "-t", "main"]).expect("last");
    assert_eq!(display(&srv, "main:", "#{window_index}"), "2");
}

/// When the active window's shell exits, the last window takes over and
/// the one before it is still the last window.
#[test]
fn the_last_window_survives_the_active_windows_shell_exiting() {
    let srv = TestServer::start();
    srv.cmd(&["set-option", "-g", "remain-on-exit", "off"])
        .expect("remain-on-exit");
    new_session(&srv, "main", IDLE);
    srv.cmd(&["new-window", "-t", "main", IDLE]).expect("neww");
    srv.cmd(&["new-window", "-t", "main", "exec sh"])
        .expect("neww");
    srv.cmd(&["select-window", "-t", "main:1"]).expect("select");
    srv.cmd(&["select-window", "-t", "main:3"]).expect("select");
    srv.cmd(&["send-keys", "-t", "main:3", "exit", "Enter"])
        .expect("send-keys");
    if let Err(e) = wait_until(
        || window_names(&srv, "main").lines().count() == 2,
        Duration::from_secs(10),
    ) {
        panic!("{e}: {:?}", window_names(&srv, "main"));
    }
    // By position it would be window 2; the stack gives 1.
    assert_eq!(display(&srv, "main:", "#{window_index}"), "1");
    srv.cmd(&["last-window", "-t", "main"]).expect("last");
    assert_eq!(display(&srv, "main:", "#{window_index}"), "2");
}

#[test]
fn rename_session_checks_the_name_like_new_session() {
    let srv = TestServer::start();
    new_session(&srv, "main", IDLE);
    new_session(&srv, "other", IDLE);
    srv.cmd(&["rename-session", "-t", "main", "work"])
        .expect("rename-session");
    assert_eq!(display(&srv, "work", "#{session_name}"), "work");
    assert_eq!(
        srv.cmd(&["rename-session", "-t", "work", "other"]),
        Err("duplicate session: other\n".to_string())
    );
    assert_eq!(
        srv.cmd(&["rename-session", "-t", "nope", "x"]),
        Err("can't find session: nope\n".to_string())
    );
    assert!(srv.cmd(&["rename-session", "-t", "work", "a:b"]).is_err());
}

#[test]
fn switch_client_moves_the_current_client_between_sessions() {
    let srv = TestServer::start();
    for name in ["b", "main", "c"] {
        new_session(&srv, name, IDLE);
    }
    assert_eq!(
        srv.cmd(&["switch-client", "-n"]),
        Err("no current client\n".to_string())
    );
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("[main]");
    assert_eq!(
        srv.cmd(&["switch-client", "-l"]),
        Err("can't find last session\n".to_string())
    );
    // Sessions in name order, wrapping: b, c, main.
    srv.cmd(&["switch-client", "-n"]).expect("-n");
    wait_client_session(&srv, "b");
    srv.cmd(&["switch-client", "-p"]).expect("-p");
    wait_client_session(&srv, "main");
    srv.cmd(&["switch-client", "-p"]).expect("-p");
    wait_client_session(&srv, "c");
    srv.cmd(&["switch-client", "-l"]).expect("-l");
    wait_client_session(&srv, "main");
    srv.cmd(&["switch-client", "-t", "b"]).expect("-t");
    wait_client_session(&srv, "b");
    a.wait_for("[b]");
    assert_eq!(
        srv.cmd(&["switch-client", "-T", "nosuch"]),
        Err("table nosuch doesn't exist\n".to_string())
    );
    srv.cmd(&["switch-client", "-T", "prefix"]).expect("-T");
    assert_eq!(
        srv.cmd(&["list-clients", "-F", "#{client_key_table}"])
            .expect("lsc"),
        "prefix\n"
    );
}

#[test]
fn send_prefix_types_the_sessions_prefix() {
    let srv = TestServer::start();
    new_session(&srv, "main", &hex_reader("", 1));
    srv.wait_for(
        || srv.capture("main").contains("ready"),
        Duration::from_secs(10),
    )
    .expect("reader ready");
    srv.cmd(&["set-option", "-t", "main", "prefix", "C-a"])
        .expect("set prefix");
    srv.cmd(&["send-prefix", "-t", "main"])
        .expect("send-prefix");
    srv.wait_for(
        || srv.capture("main").contains("got:01:end"),
        Duration::from_secs(10),
    )
    .unwrap_or_else(|e| panic!("{e}: {}", srv.capture("main")));
}

/// `capture-pane -e` keeps colours and attributes as SGR sequences, a reset
/// wherever the style changes back to the default.
#[test]
fn capture_pane_e_keeps_colours_and_attributes() {
    let srv = TestServer::start();
    new_session(
        &srv,
        "main",
        "printf 'plain \\033[31mred\\033[0m \\033[1;4mbu\\033[0m end\\n\\033[7minv\\033[0m'; \
         exec sleep 600",
    );
    srv.wait_for(
        || srv.capture("main").contains("inv"),
        Duration::from_secs(10),
    )
    .expect("pane output");
    let plain = srv
        .cmd(&["capture-pane", "-p", "-t", "main", "-E", "1"])
        .expect("capture");
    assert_eq!(plain, "plain red bu end\ninv\n");
    let styled = srv
        .cmd(&["capture-pane", "-p", "-e", "-t", "main", "-E", "1"])
        .expect("capture -e");
    assert_eq!(
        styled,
        "plain \x1b[31mred\x1b[39m \x1b[1;4mbu\x1b[0m end\n\x1b[7minv\x1b[0m\n"
    );
}

/// A coloured cell in the last column, recorded on tmux 3.7b: trailing
/// blanks go, the SGR before the last cell stays (tmux trims the blank
/// even when it is coloured), and the pen carries into the next line.
/// This pins tmux's shape, not its bytes: grofe writes full `ESC[0;...m`
/// forms where tmux writes the minimal SGR change.
#[test]
fn capture_pane_e_with_a_coloured_cell_in_the_last_column() {
    let srv = TestServer::start();
    new_session(
        &srv,
        "main",
        "printf 'ab\\033[80G\\033[41m \\033[0m\\ncd\\033[80G\\033[31mX\\033[0m\\nef'; \
         exec sleep 600",
    );
    srv.wait_for(
        || srv.capture("main").contains("ef"),
        Duration::from_secs(10),
    )
    .expect("pane output");
    let styled = srv
        .cmd(&["capture-pane", "-p", "-e", "-t", "main", "-E", "2"])
        .expect("capture -e");
    let want = format!(
        "ab{}\x1b[41m\n\x1b[49mcd{}\x1b[31mX\n\x1b[39mef\n",
        " ".repeat(77),
        " ".repeat(77)
    );
    assert_eq!(styled, want);
}

#[test]
fn list_clients_matches_tmux_and_client_vars_reach_formats() {
    let srv = TestServer::start();
    new_session(&srv, "main", IDLE);
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("[main]");
    // tmux 3.7b's default list-clients line, flags included.
    assert_eq!(
        srv.cmd(&["list-clients"]).expect("lsc"),
        "client-1: main [80x24 xterm-256color] (attached,focused,UTF-8)\n"
    );
    // Focus reports from the outer terminal update `focused`.
    a.send("\x1b[O");
    srv.wait_for(
        || {
            srv.cmd(&["list-clients", "-F", "#{client_flags}"])
                .is_ok_and(|s| s == "attached,UTF-8\n")
        },
        Duration::from_secs(5),
    )
    .expect("unfocused");
    // A format from outside any client finds the current client, as tmux.
    assert_eq!(
        display(&srv, "main", "#{client_width}x#{client_height}"),
        "80x24"
    );
}
