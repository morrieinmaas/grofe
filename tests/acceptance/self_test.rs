//! Harness self-test: exercises [`crate::harness::Harness`] against the
//! T0.3 spike binary (`examples/spike.rs`), standing in for `grofe attach`
//! until T2.5a lands it. Moved here from the old `tests/spike.rs`, which
//! duplicated this pty + reconstruction plumbing (M2 addendum).

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::harness::{self, Harness};

const COLS: u16 = 100;
const ROWS: u16 = 30;

/// Path to the built `spike` example, in the same cargo profile as this
/// test binary. `cargo test` alone builds both in `debug`; a local `cargo
/// test --release` puts this test binary under `target/release`, so the
/// example must be built with `--release` too, or the path below would
/// point at a binary that was never built.
fn spike_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let exe = std::env::current_exe().expect("current_exe");
        let profile_dir = exe
            .parent()
            .and_then(|deps| deps.parent())
            .expect("target profile dir")
            .to_path_buf();
        let release = profile_dir
            .file_name()
            .is_some_and(|name| name == "release");

        let mut args = vec!["build", "--example", "spike"];
        if release {
            args.push("--release");
        }
        let status = Command::new(env!("CARGO"))
            .args(&args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build --example spike failed");

        profile_dir.join("examples/spike")
    })
    .clone()
}

fn start(argv: &[&str]) -> Harness {
    let mut cmd = Command::new(spike_bin());
    cmd.args(argv);
    Harness::spawn(cmd, COLS, ROWS)
}

#[test]
fn echo_roundtrip() {
    let mut h = start(&["/bin/sh"]);
    h.send("echo grofe-$((40+2))\r");
    h.wait_for("grofe-42");
    h.send("\x1d");
    h.wait_exit();
}

#[test]
fn cpr_reply() {
    let mut h = start(&["/bin/sh"]);
    h.send(concat!(
        "stty raw -echo; printf '\\033[6n'; x=; ",
        "while c=$(dd bs=1 count=1 2>/dev/null); [ \"$c\" != R ]; do x=\"$x$c\"; done; ",
        "stty sane; case \"$x\" in *\\[*\\;*) echo got-cpr-$((6*7));; esac\r"
    ));
    h.wait_for("got-cpr-42");
    h.send("\x1d");
    h.wait_exit();
}

#[test]
fn nvim_edit() {
    if !harness::require_tool("nvim") {
        return;
    }
    let mut h = start(&["nvim", "-u", "NONE", "-i", "NONE"]);
    h.wait_for("~");
    h.send("ihello-spike\x1b");
    h.wait_for("hello-spike");
    sleep(Duration::from_millis(300));
    h.send(":q!\r");
    h.wait_exit();
}

#[test]
fn nvim_exit_restores_primary_screen() {
    if !harness::require_tool("nvim") {
        return;
    }
    let mut h = start(&["/bin/sh"]);
    h.send("PS1='gp> '\r");
    h.send("echo primary-$((20+1))\r");
    h.wait_for("primary-21");
    h.send("nvim -u NONE -i NONE\r");
    h.wait_until(
        "nvim on the alternate screen",
        harness::DEFAULT_TIMEOUT,
        |text| text.contains('~') && !text.contains("primary-21"),
    );
    h.send("ihello-alt\x1b");
    h.wait_for("hello-alt");
    sleep(Duration::from_millis(300));
    h.send(":q!\r");
    h.wait_until(
        "the primary screen with a fresh prompt",
        harness::DEFAULT_TIMEOUT,
        |text| {
            text.contains("primary-21")
                && !text.contains("hello-alt")
                && text.lines().any(|line| line.trim_end() == "gp>")
        },
    );
    h.send("\x1d");
    h.wait_exit();
}

/// Regression for M2 fix round 1: `Harness::spawn`'s pty wrapper must hand
/// `cmd`'s own args to the target untouched. It used to run
/// `exec "$0" -- "$@"`, which put a literal `--` in front of them; grofe's
/// global parser (`src/main.rs::parse_globals`) treats a leading `--` as
/// end-of-globals, so `grofe -S <sock> list-sessions` spawned that way
/// silently fell back to the default socket instead of failing on the one
/// it was told to use.
#[test]
fn spawn_passes_grofe_args_through_untouched() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tmp.path().join("run");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    let socket = runtime_dir.join("custom.sock");
    let default_socket = runtime_dir.join("grofe").join("default.sock");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
    cmd.env_remove("GROFE")
        .env_remove("GROFE_PANE")
        .env("HOME", tmp.path())
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .arg("-S")
        .arg(&socket)
        .arg("list-sessions");

    // Wide enough that a (long, on macOS) tempdir path never auto-wraps the
    // pty line the error prints on: a wrapped line would split the path
    // across two reconstructed rows and break the `contains` checks below.
    let mut h = Harness::spawn(cmd, 500, 10);
    h.wait_for("no server running on");
    let text = h.text();
    assert!(
        text.contains(&socket.display().to_string()),
        "error should name the -S socket; screen:\n{text}"
    );
    assert!(
        !text.contains(&default_socket.display().to_string()),
        "error should not silently fall back to the default socket; screen:\n{text}"
    );
    h.wait_exit();
}

/// Self-test for [`Harness::resize`] and [`Harness::cursor`]: the spike's
/// `render()` path already exercises them isn't proven elsewhere, so cover
/// them directly against the shell's own reporting (`stty size`, a cursor
/// position report) rather than relying on a TUI's redraw to prove it.
#[test]
fn resize_and_cursor() {
    let mut h = start(&["/bin/sh"]);
    h.send("PS1=''\r");
    h.wait_for("");
    h.send("stty size\r");
    h.wait_for(&format!("{ROWS} {COLS}"));

    // The spike relays the new size to its inner pty from its own loop,
    // some time after `resize` returns, so under load a `stty size` sent
    // straight away can still report the old size. Ask again until the new
    // one shows.
    h.resize(COLS + 10, ROWS + 5);
    let resized = format!("{} {}", ROWS + 5, COLS + 10);
    let deadline = Instant::now() + harness::DEFAULT_TIMEOUT;
    loop {
        h.send("stty size\r");
        let asked = Instant::now();
        while !h.text().contains(&resized) && asked.elapsed() < Duration::from_millis(500) {
            sleep(Duration::from_millis(20));
        }
        if h.text().contains(&resized) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {resized:?}; screen:\n{}",
            h.text()
        );
    }

    h.send("printf foo\r");
    h.wait_for("foo");
    assert!(
        h.text().contains("foo"),
        "text() should see what wait_for saw"
    );
    let (col, _row) = h.cursor();
    assert!(col > 0, "cursor should have advanced past column 0");

    h.send("\x1d");
    h.wait_exit();
}
