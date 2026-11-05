//! FR.1: framed geometry and its options (spec 2026-11-05-frames.md).
//! With `pane-frames on` and gap `g`, each tiled pane's content is inset
//! by its own frame and the gap: neighbouring content rects are `2 + g`
//! cells apart and the content area is the window inset by `1 + g`.
//! Frames off is tmux's geometry exactly; a window too small to frame its
//! panes falls back to shared borders; the defaults follow the identity,
//! and the TOML config takes the options under either.
//!
//! grofe's built-in `pane-frames` is `off` until FR.2 draws the frames
//! (the option's ponytail), so the framed tests turn it on in the config.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use common::{CMD_TIMEOUT, CmdOutput, TestServer, run_bounded, wait_until};

const WAIT: Duration = Duration::from_secs(10);
const GEOM: &str = "#{pane_left},#{pane_top},#{pane_width}x#{pane_height}";

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

/// Each pane of `target`'s window as `left,top,WxH`, in index order.
fn geometry(srv: &TestServer, target: &str) -> Vec<String> {
    ok(srv, &["list-panes", "-t", target, "-F", GEOM])
        .lines()
        .map(str::to_string)
        .collect()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// A session `f` of one 80x24 window, its panes numbered from 0.
fn session(srv: &TestServer) {
    ok(srv, &["set", "-g", "pane-base-index", "0"]);
    ok(
        srv,
        &["new-session", "-d", "-s", "f", "-x", "80", "-y", "24"],
    );
}

fn write_config(srv: &TestServer, text: &str) {
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("mkdir config");
    fs::write(dir.join("config.toml"), text).expect("write config");
}

/// A server whose config sets `text`, started by the first command.
fn start_with_config(text: &str) -> TestServer {
    let srv = TestServer::unstarted();
    write_config(&srv, text);
    ok(&srv, &["start-server"]);
    srv
}

/// A grofe-identity server with frames on and the default gap (1).
fn framed() -> TestServer {
    start_with_config("pane-frames = true\n")
}

/// What the pane's pty says its size is (`stty size`: rows, then cols).
fn pty_size(srv: &TestServer, pane: &str) -> String {
    ok(srv, &["send-keys", "-t", pane, "clear; stty size", "Enter"]);
    let mut last = String::new();
    let got = wait_until(
        || {
            last = srv.capture(pane);
            last.lines().any(|l| {
                let w: Vec<&str> = l.split_whitespace().collect();
                w.len() == 2 && w.iter().all(|n| n.parse::<u16>().is_ok())
            })
        },
        WAIT,
    );
    got.unwrap_or_else(|e| panic!("stty size in {pane}: {e}: {last:?}"));
    last.lines()
        .find(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            w.len() == 2 && w.iter().all(|n| n.parse::<u16>().is_ok())
        })
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn wait_pty(srv: &TestServer, pane: &str, want: &str) {
    let mut last = String::new();
    let got = wait_until(
        || {
            last = pty_size(srv, pane);
            last == want
        },
        WAIT,
    );
    got.unwrap_or_else(|e| panic!("{pane}'s pty: {e}: got {last:?}, want {want:?}"));
}

/// grofe's defaults (frames on, gap 1): a split leaves a 3-cell
/// separator and 2 cells around the outside, and the pty is the content.
#[test]
fn a_framed_split_insets_the_content() {
    let srv = framed();
    session(&srv);
    ok(&srv, &["split-window", "-h", "-t", "f"]);
    // The inner area 78x22 at (2,2) splits as tmux splits 78 columns,
    // 39 and 38, and each pane gives up 2 to its right and bottom.
    assert_eq!(geometry(&srv, "f"), strings(&["2,2,37x20", "42,2,36x20"]));
    wait_pty(&srv, "f:.0", "20 37");
    wait_pty(&srv, "f:.1", "20 36");
    // `#{window_layout}` describes the content rects.
    let layout = ok(
        &srv,
        &["display-message", "-p", "-t", "f", "#{window_layout}"],
    );
    assert!(
        layout
            .trim_end()
            .ends_with(",76x20,2,2{37x20,2,2,0,36x20,42,2,1}"),
        "{layout}"
    );
    // A content size in cells is a content size.
    ok(&srv, &["split-window", "-v", "-t", "f:.1", "-l", "5"]);
    assert_eq!(
        geometry(&srv, "f"),
        strings(&["2,2,37x20", "42,2,36x12", "42,17,36x5"])
    );
}

/// `resize-pane -L 5` moves the divider 5 cells, frames and all.
#[test]
fn resize_pane_moves_the_framed_divider() {
    let srv = framed();
    session(&srv);
    ok(&srv, &["split-window", "-h", "-t", "f"]);
    ok(&srv, &["resize-pane", "-t", "f:.0", "-L", "5"]);
    assert_eq!(geometry(&srv, "f"), strings(&["2,2,32x20", "37,2,41x20"]));
    wait_pty(&srv, "f:.1", "20 41");
    ok(&srv, &["resize-pane", "-t", "f:.0", "-x", "50"]);
    assert_eq!(geometry(&srv, "f"), strings(&["2,2,50x20", "55,2,23x20"]));
    // A resize past what the frames leave stops where every pane keeps a
    // content cell.
    ok(&srv, &["resize-pane", "-t", "f:.0", "-R", "100"]);
    let g = geometry(&srv, "f");
    assert_eq!(g[1].rsplit(',').next(), Some("1x20"), "{g:?}");
    assert!(g[0].starts_with("2,2,"), "{g:?}");
}

/// Presets lay the panes out in the inner area, a main pane's width in
/// content cells.
#[test]
fn presets_are_framed() {
    let srv = framed();
    session(&srv);
    ok(&srv, &["split-window", "-t", "f"]);
    ok(&srv, &["split-window", "-t", "f"]);
    ok(&srv, &["select-layout", "-t", "f", "even-vertical"]);
    // 22 inner rows less 2 borders: 7, 7 and 6, each less 2.
    assert_eq!(
        geometry(&srv, "f"),
        strings(&["2,2,76x5", "2,10,76x5", "2,18,76x4"])
    );
    ok(&srv, &["set", "-w", "-t", "f", "main-pane-width", "30"]);
    ok(&srv, &["select-layout", "-t", "f", "main-vertical"]);
    let g = geometry(&srv, "f");
    assert_eq!(g[0], "2,2,30x20", "{g:?}");
    assert!(g[1].starts_with("35,2,43x"), "{g:?}");
    assert!(g[2].starts_with("35,"), "{g:?}");
    ok(&srv, &["select-layout", "-t", "f", "even-horizontal"]);
    let g = geometry(&srv, "f");
    assert_eq!(g, strings(&["2,2,24x20", "29,2,23x20", "55,2,23x20"]));
    // The framed layout string applies back as the layout it describes.
    let layout = ok(
        &srv,
        &["display-message", "-p", "-t", "f", "#{window_layout}"],
    );
    ok(&srv, &["select-layout", "-t", "f", "tiled"]);
    ok(&srv, &["select-layout", "-t", "f", layout.trim_end()]);
    assert_eq!(geometry(&srv, "f"), g);
}

/// A zoomed pane fills the window as one framed tile with the same gap.
#[test]
fn a_zoomed_pane_is_one_framed_tile() {
    let srv = framed();
    session(&srv);
    ok(&srv, &["split-window", "-h", "-t", "f"]);
    ok(&srv, &["resize-pane", "-Z", "-t", "f:.1"]);
    let zoomed = ok(
        &srv,
        &[
            "display-message",
            "-p",
            "-t",
            "f:.1",
            &format!("#{{window_zoomed_flag}} {GEOM}"),
        ],
    );
    assert_eq!(zoomed, "1 2,2,76x20\n");
    wait_pty(&srv, "f:.1", "20 76");
    ok(&srv, &["resize-pane", "-Z", "-t", "f:.1"]);
    assert_eq!(geometry(&srv, "f"), strings(&["2,2,37x20", "42,2,36x20"]));
    wait_pty(&srv, "f:.1", "20 36");
}

/// Frames off is today's geometry exactly, and turning frames on and off
/// again moves nothing.
#[test]
fn frames_off_is_tmux_geometry() {
    let srv = TestServer::start();
    ok(&srv, &["set", "-g", "pane-frames", "off"]);
    session(&srv);
    ok(&srv, &["split-window", "-h", "-t", "f"]);
    ok(&srv, &["split-window", "-v", "-t", "f:.1"]);
    let tmux = strings(&["0,0,40x24", "41,0,39x12", "41,13,39x11"]);
    assert_eq!(geometry(&srv, "f"), tmux);
    let layout = ok(
        &srv,
        &["display-message", "-p", "-t", "f", "#{window_layout}"],
    );
    assert!(
        layout
            .trim_end()
            .ends_with(",80x24,0,0{40x24,0,0,0,39x24,41,0[39x12,41,0,1,39x11,41,13,2]}"),
        "{layout}"
    );
    ok(&srv, &["set", "-g", "pane-frames", "on"]);
    let framed = geometry(&srv, "f");
    assert_eq!(framed[0], "2,2,37x20", "{framed:?}");
    wait_pty(&srv, "f:.0", "20 37");
    ok(&srv, &["set", "-g", "pane-gap", "0"]);
    assert_eq!(geometry(&srv, "f")[0], "1,1,38x22");
    ok(&srv, &["set", "-g", "pane-frames", "off"]);
    assert_eq!(geometry(&srv, "f"), tmux);
    wait_pty(&srv, "f:.0", "24 40");
    // A window's own value wins over the global one.
    ok(&srv, &["set", "-w", "-t", "f", "pane-frames", "on"]);
    assert_eq!(geometry(&srv, "f")[0], "1,1,38x22");
    assert_eq!(
        srv.cmd(&["set", "-g", "pane-gap", "9"]),
        Err("value is too large: 9\n".to_string())
    );
}

/// A window too small to frame every pane lays them out with shared
/// borders, and frames them again once it is big enough.
#[test]
fn a_too_small_window_falls_back_to_shared_borders() {
    let srv = framed();
    ok(&srv, &["set", "-g", "pane-base-index", "0"]);
    ok(
        &srv,
        &["new-session", "-d", "-s", "f", "-x", "20", "-y", "10"],
    );
    ok(&srv, &["split-window", "-h", "-t", "f"]);
    ok(&srv, &["split-window", "-h", "-t", "f:.1"]);
    assert_eq!(geometry(&srv, "f")[0], "2,2,7x6");
    ok(&srv, &["resize-window", "-t", "f", "-x", "8", "-y", "10"]);
    let g = geometry(&srv, "f");
    // Shared one-cell borders over the whole window, as tmux lays it out.
    assert_eq!(g, strings(&["0,0,3x10", "4,0,2x10", "7,0,1x10"]));
    ok(&srv, &["resize-window", "-t", "f", "-x", "20", "-y", "10"]);
    assert_eq!(geometry(&srv, "f")[0], "2,2,7x6");
    // A split that would leave a framed pane no content cell is refused.
    assert!(srv.cmd(&["split-window", "-h", "-t", "f:.2"]).is_err());
}

/// The defaults follow the identity: grofe's gap is 1 and its lines
/// rounded, tmux's keeps shared single borders.
#[test]
fn the_defaults_differ_by_identity() {
    let srv = TestServer::start();
    let show = |srv: &TestServer, name: &str| ok(srv, &["show", "-gv", name]);
    // ponytail: `on` once FR.2 lands (the option's comment).
    assert_eq!(show(&srv, "pane-frames"), "off\n");
    assert_eq!(show(&srv, "pane-gap"), "1\n");
    assert_eq!(show(&srv, "pane-frame-title"), "on\n");
    assert_eq!(show(&srv, "pane-border-lines"), "rounded\n");

    let tmux = start_with_config("compat-identity = \"tmux\"\n");
    assert_eq!(show(&tmux, "pane-frames"), "off\n");
    assert_eq!(show(&tmux, "pane-gap"), "0\n");
    assert_eq!(show(&tmux, "pane-frame-title"), "on\n");
    assert_eq!(show(&tmux, "pane-border-lines"), "single\n");
    session(&tmux);
    ok(&tmux, &["split-window", "-h", "-t", "f"]);
    assert_eq!(geometry(&tmux, "f"), strings(&["0,0,40x24", "41,0,39x24"]));
    // `set -gu` goes back to the identity's default.
    ok(&tmux, &["set", "-g", "pane-frames", "on"]);
    ok(&tmux, &["set", "-gu", "pane-frames"]);
    assert_eq!(show(&tmux, "pane-frames"), "off\n");
}

/// The TOML config takes the frame options under tmux identity too.
#[test]
fn the_toml_config_frames_a_tmux_identity_server() {
    let srv = start_with_config(
        "compat-identity = \"tmux\"\npane-frames = true\npane-gap = 2\npane-frame-title = false\n",
    );
    let show = |name: &str| ok(&srv, &["show", "-gv", name]);
    assert_eq!(show("pane-frames"), "on\n");
    assert_eq!(show("pane-gap"), "2\n");
    assert_eq!(show("pane-frame-title"), "off\n");
    session(&srv);
    ok(&srv, &["split-window", "-h", "-t", "f"]);
    // Pad 3: the inner area is 77x21 at (3,3), split 38 and 38.
    assert_eq!(geometry(&srv, "f"), strings(&["3,3,35x18", "42,3,35x18"]));
    wait_pty(&srv, "f:.1", "18 35");
}

/// A `tmux` symlink to the grofe binary in the harness's own `bin/`.
fn tmux_link(srv: &TestServer) -> PathBuf {
    let bin = srv.home.parent().expect("root").join("bin");
    fs::create_dir_all(&bin).unwrap();
    let link = bin.join("tmux");
    if !link.exists() {
        symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    }
    link
}

/// `tmux -S <socket> <args>` through the symlink, isolated as the harness
/// isolates grofe.
fn tmux(srv: &TestServer, args: &[&str]) -> CmdOutput {
    let mut cmd = Command::new(tmux_link(srv));
    let tmpdir = srv.home.parent().expect("root").join("tmux-tmpdir");
    fs::create_dir_all(&tmpdir).unwrap();
    cmd.current_dir(&srv.home)
        .env_remove("GROFE")
        .env_remove("GROFE_PANE")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("HOME", &srv.home)
        .env("XDG_CONFIG_HOME", &srv.config_home)
        .env("XDG_STATE_HOME", &srv.state_home)
        .env("XDG_RUNTIME_DIR", &srv.runtime_dir)
        .env("TMUX_TMPDIR", tmpdir)
        .env("GROFE_RESTORE_ON_START", "0")
        .env("GROFE_REMAIN_ON_EXIT", "1")
        .arg("-S")
        .arg(&srv.socket)
        .args(args);
    run_bounded(cmd, CMD_TIMEOUT)
}

/// Through tmux's CLI the frame options are unknown, as any grofe-only
/// option is, while the TOML config set them.
#[test]
fn tmux_s_cli_does_not_know_the_frame_options() {
    let srv = start_with_config("compat-identity = \"tmux\"\npane-frames = true\n");
    for name in ["pane-frames", "pane-gap", "pane-frame-title"] {
        let out = tmux(&srv, &["show-options", "-g", name]);
        assert!(!out.status.success(), "{name}: {}", out.stdout);
        assert_eq!(out.stderr, format!("invalid option: {name}\n"));
        let out = tmux(&srv, &["set-option", "-g", name, "on"]);
        assert!(!out.status.success(), "{name}");
        assert_eq!(out.stderr, format!("invalid option: {name}\n"));
        // `-q` quiets an unknown name, as in tmux.
        let out = tmux(&srv, &["show-options", "-gqv", name]);
        assert!(out.status.success(), "{name}: {}", out.stderr);
        assert_eq!(out.stdout, "");
    }
    let all = tmux(&srv, &["show-options", "-g"]);
    assert!(all.status.success(), "{}", all.stderr);
    assert!(!all.stdout.contains("pane-frame"), "{}", all.stdout);
    assert!(!all.stdout.contains("pane-gap"), "{}", all.stdout);
    let all = tmux(&srv, &["show-options", "-gw"]);
    assert!(!all.stdout.contains("pane-frame"), "{}", all.stdout);
    // The native CLI still knows them.
    assert_eq!(ok(&srv, &["show", "-gv", "pane-frames"]), "on\n");
}

/// The snapshot keeps a framed window's layout, and restore brings the
/// same content rects back.
#[test]
fn a_framed_layout_survives_a_restore() {
    let srv = framed();
    session(&srv);
    ok(&srv, &["split-window", "-h", "-t", "f"]);
    ok(&srv, &["split-window", "-v", "-t", "f:.1"]);
    ok(&srv, &["resize-pane", "-t", "f:.0", "-R", "7"]);
    let before = geometry(&srv, "f");
    assert_eq!(before[0], "2,2,44x20", "{before:?}");
    ok(&srv, &["kill-server"]);
    wait_until(|| !srv.is_running(), WAIT).expect("the server goes");
    ok(&srv, &["start-server", "--restore"]);
    assert_eq!(geometry(&srv, "f"), before);
}
