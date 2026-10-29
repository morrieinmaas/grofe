//! tmux 3.7b's stock right-click menus on a server with tmux identity
//! (B1d): the window menu on the status line, the session menu on its left
//! end and the pane menu at the pointer, each opened by the button-3 press
//! the terminal sends, and none of them natively.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;

use common::TestServer;
use harness::Harness;

fn server(identity: &str) -> (TestServer, Harness) {
    let srv = TestServer::unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        format!("compat-identity = \"{identity}\"\n"),
    )
    .unwrap();
    srv.cmd(&["start-server"]).expect("start-server");
    for args in [
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-n",
            "one",
            "-x",
            "80",
            "-y",
            "24",
            "cat",
        ][..],
        &["new-window", "-d", "-n", "two", "-t", "s:9", "cat"],
        &["set", "-g", "mouse", "on"],
        &["set", "-g", "status-right", ""],
    ] {
        srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    let mut client = Harness::spawn_attach_with_env(srv.command(), "s", &[], 80, 24);
    // The status line is drawn, and the mouse modes are on.
    client.wait_for(":two");
    (srv, client)
}

/// A right-button press at 0-based client cell `(col, row)`. The button
/// stays down: tmux 3.7b's menu closes on the release outside it
/// (`golden/b3wpc/menu-bound-mouse`).
fn right_click(client: &mut Harness, col: u16, row: u16) {
    let (x, y) = (col + 1, row + 1);
    client.send(format!("\x1b[<2;{x};{y}M"));
}

#[test]
fn right_click_on_the_status_line_opens_the_window_menu() {
    let (_srv, mut client) = server("tmux");
    // The second window's label, "[s] 1:one* 9:two", ends at column 16.
    right_click(&mut client, 14, 23);
    client.wait_for("New At End");
    let screen = client.text();
    assert!(screen.contains("9:two"), "{screen}");
    assert!(screen.contains("Swap Left"), "{screen}");
    client.send("q");
    client.wait_until("the menu gone", harness::DEFAULT_TIMEOUT, |t| {
        !t.contains("New At End")
    });
}

#[test]
fn right_click_on_the_status_left_opens_the_session_menu() {
    let (_srv, mut client) = server("tmux");
    right_click(&mut client, 1, 23);
    client.wait_for("New Session");
    assert!(client.text().contains("Renumber"));
    client.send("q");
}

#[test]
fn right_click_in_a_pane_opens_the_pane_menu_at_the_pointer() {
    let (_srv, mut client) = server("tmux");
    right_click(&mut client, 20, 5);
    client.wait_for("Vertical Split");
    let screen = client.text();
    let row = screen
        .lines()
        .find(|l| l.contains("Vertical Split"))
        .expect("the menu row");
    let col = row.find("Vertical Split").unwrap();
    // The box is centred on the pointer's column; its rows start there.
    assert!((8..=20).contains(&col), "{col}: {screen}");
    client.send("q");
}

#[test]
fn natively_a_right_click_opens_nothing() {
    let (_srv, mut client) = server("grofe");
    right_click(&mut client, 8, 23);
    right_click(&mut client, 20, 5);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let screen = client.text();
    assert!(!screen.contains("New At End"), "{screen}");
    assert!(!screen.contains("Vertical Split"), "{screen}");
}

fn pane_ids(srv: &TestServer) -> String {
    srv.cmd(&["list-panes", "-t", "s:one", "-F", "#{pane_id}"])
        .expect("list-panes")
        .replace('\n', " ")
}

#[test]
fn a_click_on_a_border_clears_the_marked_pane() {
    let (srv, mut client) = server("tmux");
    srv.cmd(&["split-window", "-d", "-h", "-t", "s:one", "cat"])
        .unwrap();
    srv.cmd(&["select-pane", "-m", "-t", "s:one.0"]).unwrap();
    assert_eq!(
        srv.cmd(&["display", "-p", "#{pane_marked_set}"]).unwrap(),
        "1\n"
    );
    // The border column of an 80 column window split in two.
    client.send("\x1b[<0;41;6M\x1b[<0;41;6m");
    harness_wait(&srv, "#{pane_marked_set}", "0\n");
    drop(client);
}

#[test]
fn control_click_swaps_the_clicked_pane_with_the_active_one() {
    let (srv, mut client) = server("tmux");
    srv.cmd(&["split-window", "-h", "-t", "s:one", "cat"])
        .unwrap();
    let before = pane_ids(&srv);
    // The new pane is active; Control with the left button on the first.
    client.send("\x1b[<16;5;6M\x1b[<16;5;6m");
    let want: Vec<&str> = before.split_whitespace().rev().collect();
    let want = format!("{} ", want.join(" "));
    common::wait_until(|| pane_ids(&srv) == want, std::time::Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("{e}: {} -> {}", before, pane_ids(&srv)));
}

fn harness_wait(srv: &TestServer, format: &str, want: &str) {
    common::wait_until(
        || srv.cmd(&["display", "-p", format]).is_ok_and(|v| v == want),
        std::time::Duration::from_secs(5),
    )
    .unwrap_or_else(|e| panic!("{format} never {want:?}: {e}"));
}
