//! TC.7: menus, `display-panes`, `confirm-before`, prompt flags and
//! aliases. A menu item's key runs its command; a nested menu opens from an
//! item and one Escape closes it (tmux 3.7b closes the outer menu before
//! the item's command opens the inner one, recorded); `display-panes` then
//! a digit selects that pane; `confirm-before` runs its command only on
//! `y`; a rename prompt shows the old name prefilled; a one-key prompt
//! reports the pressed key's binding; an alias becomes a command. A menu
//! belongs to the client that opened it (D15).

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::process::Stdio;
use std::time::Duration;

use common::{TestServer, wait_until};

use crate::harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

/// A server with one session `main` running `cat`, sized for an 80x24
/// client with a status line.
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
        "cat",
    ])
    .expect("new-session");
    srv
}

fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_until("the status row", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
    a
}

fn last_row(text: &str) -> &str {
    text.lines().last().unwrap_or_default()
}

fn wait_last_row(a: &mut Harness, want: &str) {
    a.wait_until(&format!("last row {want:?}"), WAIT, |text| {
        last_row(text).trim_end() == want
    });
}

fn window_name(srv: &TestServer) -> String {
    srv.cmd(&["display-message", "-p", "-t", "main", "#{window_name}"])
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

fn wait_window_name(srv: &TestServer, want: &str) {
    wait_until(|| window_name(srv) == want, WAIT)
        .unwrap_or_else(|e| panic!("window name {want:?}: {e}, got {:?}", window_name(srv)));
}

fn panes(srv: &TestServer) -> usize {
    srv.cmd(&["list-panes", "-t", "main"])
        .unwrap_or_default()
        .lines()
        .count()
}

fn bind(srv: &TestServer, args: &[&str]) {
    let mut argv = vec!["bind-key"];
    argv.extend_from_slice(args);
    srv.cmd(&argv).expect("bind-key");
}

/// Case 1: an item's key runs its command and closes the menu.
#[test]
fn a_menu_items_key_runs_its_command() {
    let srv = start();
    bind(
        &srv,
        &[
            "m",
            "display-menu",
            "-T",
            "Pick",
            "-x",
            "2",
            "-y",
            "10",
            "One",
            "a",
            "rename-window one",
            "Two",
            "b",
            "rename-window two",
            "Three",
            "c",
            "rename-window three",
        ],
    );
    let mut a = attach(&srv);
    a.send("\x02m");
    a.wait_for("┌─Pick");
    a.wait_for("│ Two   (b) │");
    a.send("b");
    wait_window_name(&srv, "two");
    a.wait_until("the menu to close", WAIT, |text| !text.contains("Pick"));
}

/// Case 2: an item opens a nested menu; one Escape leaves neither.
#[test]
fn a_nested_menu_opens_from_an_item_and_escape_closes_both() {
    let srv = start();
    bind(
        &srv,
        &[
            "m",
            "display-menu",
            "-T",
            "Outer",
            "-x",
            "2",
            "-y",
            "10",
            "Sub",
            "s",
            "display-menu -T Inner -x 30 -y 12 Deep d 'rename-window deep'",
        ],
    );
    let mut a = attach(&srv);
    let name = window_name(&srv);
    a.send("\x02m");
    a.wait_for("┌─Outer");
    a.send("s");
    a.wait_until("the inner menu alone", WAIT, |text| {
        text.contains("┌─Inner") && !text.contains("Outer")
    });
    a.send("\x1b");
    a.wait_until("no menu", WAIT, |text| {
        !text.contains("Inner") && !text.contains("Outer")
    });
    assert_eq!(window_name(&srv), name);
    // The keys reach the pane again.
    a.send("after\r");
    wait_until(|| srv.capture("main").contains("after"), WAIT)
        .unwrap_or_else(|e| panic!("keys after the menu: {e}"));
}

/// From the CLI, `display-menu` answers once the menu closes, as tmux's
/// does.
#[test]
fn display_menu_from_the_cli_waits_for_the_menu() {
    let srv = start();
    let mut a = attach(&srv);
    let mut cli = srv.command();
    cli.args([
        "display-menu",
        "-x",
        "C",
        "-y",
        "C",
        "Only",
        "o",
        "rename-window picked",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let mut cli = cli.spawn().expect("spawn display-menu");
    a.wait_for("│ Only (o) │");
    let waited = cli.try_wait().ok().flatten().is_none();
    a.send("o");
    let status = common::wait_child(cli, WAIT).expect("display-menu exits");
    assert!(waited, "display-menu answered before the menu closed");
    assert!(status.success(), "{status:?}");
    wait_window_name(&srv, "picked");
}

/// Case 3: `display-panes` labels the panes; a digit selects that pane and
/// closes the overlay.
#[test]
fn display_panes_then_a_digit_selects_that_pane() {
    let srv = start();
    srv.cmd(&["split-window", "-h", "-t", "main", "cat"])
        .expect("split");
    srv.cmd(&["split-window", "-v", "-t", "main", "cat"])
        .expect("split");
    srv.cmd(&["select-pane", "-t", "%0"]).expect("select");
    bind(&srv, &["q", "display-panes"]);
    let mut a = attach(&srv);
    a.send("\x02q");
    // Each pane's size shows at its top right while the overlay is up.
    a.wait_for("40x23");
    a.send("2");
    wait_until(
        || {
            srv.cmd(&["display-message", "-p", "-t", "main", "#{pane_index}"])
                .is_ok_and(|i| i == "2\n")
        },
        WAIT,
    )
    .unwrap_or_else(|e| panic!("pane 2 active: {e}"));
    a.wait_until("the labels gone", WAIT, |text| !text.contains("40x23"));
}

/// The labels close by themselves after tmux's `display-panes-time` (one
/// second), and a key that names no pane closes them too.
#[test]
fn display_panes_closes_after_its_time() {
    let srv = start();
    srv.cmd(&["split-window", "-h", "-t", "main", "cat"])
        .expect("split");
    bind(&srv, &["q", "display-panes"]);
    let mut a = attach(&srv);
    a.send("\x02q");
    a.wait_for("40x23");
    a.wait_until("the labels gone", Duration::from_secs(3), |text| {
        !text.contains("40x23")
    });
    a.send("\x02q");
    a.wait_for("40x23");
    a.send("7");
    a.wait_until("the labels gone", WAIT, |text| !text.contains("40x23"));
}

/// Case 4: `confirm-before` runs its command on `y` and on nothing else.
#[test]
fn confirm_before_runs_its_command_only_on_y() {
    let srv = start();
    srv.cmd(&["split-window", "-t", "main", "cat"])
        .expect("split");
    bind(&srv, &["x", "confirm-before", "-p", "kill?", "kill-pane"]);
    let mut a = attach(&srv);
    a.send("\x02x");
    wait_last_row(&mut a, "kill?");
    a.send("n");
    a.wait_until("the prompt gone", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
    assert_eq!(panes(&srv), 2);
    // Enter and a capital Y cancel too, as in tmux 3.7b.
    a.send("\x02x");
    wait_last_row(&mut a, "kill?");
    a.send("Y");
    a.wait_until("the prompt gone", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
    assert_eq!(panes(&srv), 2);
    a.send("\x02x");
    wait_last_row(&mut a, "kill?");
    a.send("y");
    wait_until(|| panes(&srv) == 1, WAIT).unwrap_or_else(|e| panic!("kill-pane: {e}"));

    // Without -p the prompt names the command, as tmux words it.
    bind(&srv, &["X", "confirm-before", "set -g @x 1"]);
    a.send("\x02X");
    wait_last_row(&mut a, "Confirm 'set-option'? (y/n)");
    a.send("\x1b");
}

/// Case 5: a rename prompt starts with the old name.
#[test]
fn a_rename_prompt_shows_the_old_name_prefilled() {
    let srv = start();
    srv.cmd(&["rename-window", "-t", "main", "old"])
        .expect("rename");
    bind(
        &srv,
        &[
            "r",
            "command-prompt",
            "-I",
            "#{window_name}",
            "-p",
            "New name:",
            "rename-window %%",
        ],
    );
    let mut a = attach(&srv);
    a.send("\x02r");
    wait_last_row(&mut a, "New name: old");
    a.send("er\r");
    wait_window_name(&srv, "older");
}

/// Case 6: a one-key prompt (`-k`) reports the pressed key's binding, as
/// tmux's stock `prefix /` does; `-1` answers with the key typed.
#[test]
fn a_one_key_prompt_reports_the_pressed_keys_binding() {
    let srv = start();
    bind(&srv, &["-N", "Kill the active pane", "x", "kill-pane"]);
    bind(
        &srv,
        &[
            "/",
            "command-prompt",
            "-k",
            "-p",
            "key",
            "list-keys -1N \"%%\"",
        ],
    );
    let mut a = attach(&srv);
    a.send("\x02/");
    wait_last_row(&mut a, "key");
    a.send("x");
    wait_last_row(&mut a, "C-b x Kill the active pane");
    assert_eq!(panes(&srv), 1);

    bind(
        &srv,
        &[
            "j",
            "command-prompt",
            "-1",
            "-p",
            "jump:",
            "rename-window 'got %%'",
        ],
    );
    a.send("\x02j");
    wait_last_row(&mut a, "jump:");
    a.send("z");
    wait_window_name(&srv, "got z");
}

/// Case 7: an alias from the config becomes a command, and one set at
/// runtime does too; the rest of the arguments follow the alias's own.
#[test]
fn an_alias_defined_in_config_becomes_a_command() {
    let srv = start();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(
        dir.join("config.toml"),
        "command-alias = [\"mya=rename-window from-config\", \"ren=rename-window\"]\n",
    )
    .expect("config");
    srv.cmd(&["reload"]).expect("reload");
    srv.cmd(&["mya"]).expect("mya");
    assert_eq!(window_name(&srv), "from-config");
    srv.cmd(&["ren", "-t", "main", "with-args"]).expect("ren");
    assert_eq!(window_name(&srv), "with-args");

    srv.cmd(&["set", "-ag", "command-alias", "myb=rename-window runtime"])
        .expect("set");
    srv.cmd(&["myb"]).expect("myb");
    assert_eq!(window_name(&srv), "runtime");

    // In a sequence, and from a binding.
    srv.cmd(&["mya", ";", "ren", "second"]).expect("sequence");
    assert_eq!(window_name(&srv), "second");
    bind(&srv, &["a", "myb"]);
    srv.cmd(&["rename-window", "-t", "main", "x"])
        .expect("rename");
    let mut a = attach(&srv);
    a.send("\x02a");
    wait_window_name(&srv, "runtime");
}

/// Case 8 (D15): a menu opened by client A is drawn on A only, and B's keys
/// still reach the pane.
#[test]
fn a_menu_opened_by_one_client_is_invisible_to_another() {
    let srv = start();
    bind(
        &srv,
        &[
            "m",
            "display-menu",
            "-T",
            "Mine",
            "-x",
            "2",
            "-y",
            "10",
            "One",
            "a",
            "rename-window one",
        ],
    );
    let mut a = attach(&srv);
    let mut b = attach(&srv);
    a.send("\x02m");
    a.wait_for("┌─Mine");
    b.send("typed-in-b\r");
    wait_until(|| srv.capture("main").contains("typed-in-b"), WAIT)
        .unwrap_or_else(|e| panic!("b's keys: {e}"));
    b.wait_for("typed-in-b");
    assert!(!b.text().contains("Mine"), "{}", b.text());
    assert!(a.text().contains("┌─Mine"), "{}", a.text());
    a.send("a");
    wait_window_name(&srv, "one");
}
