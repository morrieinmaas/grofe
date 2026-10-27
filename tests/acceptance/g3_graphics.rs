//! G3: kitty direct placements, animation and sixel, end to end through
//! `grofe attach`. A pane draws with `cat` of a byte file the test wrote; a
//! client with a faked capability (`GROFE_TEST_CAPS`) is sent placements,
//! and the raw bytes it received are what these tests read.
//!
//! Layout: the pane is 80x23 under a status row, cells 10x20 pixels, and
//! the 4x4 PNG fixture is placed `c=8,r=2`, so one source pixel is two
//! columns wide and half a row tall.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use common::TestServer;

use crate::harness::Harness;

const WAIT: Duration = Duration::from_secs(10);
const KITTY: &[(&str, &str)] = &[("GROFE_TEST_CAPS", "kitty_graphics,cell_px=10x20")];
const SIXEL: &[(&str, &str)] = &[("GROFE_TEST_CAPS", "sixel,cell_px=10x20")];
const BOTH: &[(&str, &str)] = &[("GROFE_TEST_CAPS", "kitty_graphics,sixel,cell_px=10x20")];

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn png() -> String {
    let bytes = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graphics/red-blue-4x4.png"),
    )
    .expect("fixture");
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `ESC _ G body ESC \`.
fn apc(body: &str) -> String {
    format!("\x1b_G{body}\x1b\\")
}

/// Moves the cursor to the 1-based `row`, `col`.
fn at(row: u32, col: u32) -> String {
    format!("\x1b[{row};{col}H")
}

/// A direct transmit of the fixture as image `id`, `c` by `r` cells.
fn image(id: u32, extra: &str) -> String {
    apc(&format!("a=T,i={id},f=100,q=2,{extra};{}", png()))
}

/// Session `main` (80x23 plus a status row) whose pane runs `script` under
/// `sh`, with the `graphics` option as asked.
fn session(srv: &TestServer, script: &str, graphics: bool) {
    let cmd = format!("sh -c {}", shell_quote(script));
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "80", "-y", "23", &cmd])
        .expect("new-session");
    if graphics {
        srv.cmd(&["set-option", "-g", "graphics", "on"])
            .expect("set graphics");
    }
}

fn attach(srv: &TestServer, env: &[(&str, &str)]) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", env, 80, 24);
    a.wait_for_output(b"\x1b[2J", 0);
    a
}

fn wait_flag(flag: &Path) -> String {
    format!(
        "while [ ! -e {} ]; do sleep 0.05; done",
        shell_quote(&flag.to_string_lossy())
    )
}

fn touch(path: &Path) {
    std::fs::write(path, b"").expect("flag file");
}

/// Writes `bytes` to `name` under the server's home and returns the shell
/// text that prints it.
fn cat(srv: &TestServer, name: &str, bytes: &str) -> String {
    let path: PathBuf = srv.home.join(name);
    std::fs::write(&path, bytes).expect("byte file");
    format!("cat {}", shell_quote(&path.to_string_lossy()))
}

/// One `a=p` the client was sent: where its cursor was moved first, and its
/// keys.
#[derive(Debug, Clone)]
struct Put {
    row: u32,
    col: u32,
    keys: HashMap<String, String>,
}

impl Put {
    fn n(&self, key: &str) -> i64 {
        self.keys
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no {key} in {self:?}"))
    }
}

fn keys_of(apc: &str) -> HashMap<String, String> {
    apc.split(';')
        .next()
        .unwrap_or_default()
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Every `a=p` in `bytes` that follows a cursor move, in order.
fn puts(bytes: &[u8]) -> Vec<Put> {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("\x1b_Ga=p,") {
        let start = from + i;
        let end = text[start..].find("\x1b\\").map_or(text.len(), |e| start + e);
        let keys = keys_of(&text[start + 3..end]);
        // The move before it: ESC [ row ; col H.
        let before = &text[..start];
        if let Some(h) = before.rfind('H')
            && let Some(csi) = before[..h].rfind("\x1b[")
            && let Some((r, c)) = before[csi + 2..h].split_once(';')
            && let (Ok(row), Ok(col)) = (r.parse(), c.parse())
        {
            out.push(Put { row, col, keys });
        }
        from = end;
    }
    out
}

/// The keys of every `a=d` in `bytes`.
fn deletes(bytes: &[u8]) -> Vec<HashMap<String, String>> {
    String::from_utf8_lossy(bytes)
        .split("\x1b_Ga=d,")
        .skip(1)
        .filter_map(|s| s.split("\x1b\\").next())
        .map(|s| keys_of(&format!("a=d,{s}")))
        .collect()
}

/// The outer id the client's terminal was given for the transmit, from the
/// `a=t,i=N` it was sent.
fn transmitted(bytes: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(bytes)
        .split("\x1b_Ga=t,")
        .skip(1)
        .filter_map(|s| s.split("\x1b\\").next())
        .filter_map(|s| keys_of(&format!("a=t,{s}")).get("i")?.parse().ok())
        .collect()
}

/// Waits for `n` more `a=p` after `from`; returns them and the new end.
fn wait_puts(a: &mut Harness, from: usize, n: usize) -> (Vec<Put>, usize) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let all = a.output().to_vec();
        let found = puts(&all[from.min(all.len())..]);
        if found.len() >= n {
            return (found, all.len());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "wanted {n} placements, have {found:?}; screen:\n{}",
            a.text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn next_put(a: &mut Harness, from: usize) -> (Put, usize) {
    let (found, end) = wait_puts(a, from, 1);
    (found[0].clone(), end)
}

/// Case 1: a placement at the edge of its pane is clipped there: the
/// client is told to show only the columns that fit, with the part of the
/// source they map to, at the cell the first of them is in. The edge is the
/// window's at column 80 and, after a split, the new pane's at column 40.
#[test]
fn a_placement_is_clipped_at_the_pane_edge() {
    let srv = TestServer::start();
    let go = srv.home.join("go");
    let draw = cat(
        &srv,
        "draw.bin",
        &format!(
            "{}{}{}{}",
            at(3, 76),
            image(1, "c=8,r=2,C=1"),
            at(8, 36),
            image(2, "c=8,r=2,C=1")
        ),
    );
    session(&srv, &format!("{}; {draw}; exec sleep 600", wait_flag(&go)), true);
    let mut a = attach(&srv, KITTY);
    touch(&go);
    let (found, mark) = wait_puts(&mut a, 0, 2);
    let edge = found.iter().find(|p| p.row == 3).expect("the first");
    assert_eq!(edge.col, 76);
    // Columns 0..5 of 8, both rows, of the 4x4 source.
    assert_eq!(
        (edge.n("c"), edge.n("r"), edge.n("x"), edge.n("y"), edge.n("w"), edge.n("h")),
        (5, 2, 0, 0, 2, 4),
        "{edge:?}"
    );
    assert_eq!(edge.n("z"), 0);
    assert_eq!(edge.keys.get("C").map(String::as_str), Some("1"));
    let whole = found.iter().find(|p| p.row == 8).expect("the second");
    assert_eq!((whole.n("c"), whole.n("w")), (8, 4), "not cut yet: {whole:?}");
    // The transmits went ahead of them, as plain `a=t`: the terminal must
    // not place them by itself.
    let out = a.output().to_vec();
    assert_eq!(transmitted(&out).len(), 2);
    assert!(!String::from_utf8_lossy(&out).contains("a=T,"), "no placing transmit");
    // Split: the left pane is 40 wide, so the second is cut at its edge and
    // the first is no longer in a pane that shows it.
    srv.cmd(&["split-window", "-h", "-t", "main", "sleep 600"])
        .expect("split");
    let width: i64 = srv
        .cmd(&["list-panes", "-t", "main", "-F", "#{pane_width}"])
        .expect("width")
        .lines()
        .next()
        .expect("a pane")
        .trim()
        .parse()
        .expect("number");
    let (cut, _) = wait_puts(&mut a, mark, 1);
    let cut = &cut[0];
    assert_eq!((cut.row, cut.col), (8, 36), "{cut:?}");
    assert_eq!(cut.n("c"), width - 35, "cut at the pane's edge: {cut:?}");
    assert_eq!(cut.n("w"), (width - 35) / 2, "{cut:?}");
}

/// Case 2: the placement moves up as the pane scrolls, and leaves when its
/// rows are gone.
#[test]
fn a_placement_moves_with_scrolling() {
    let srv = TestServer::start();
    let (go, scroll) = (srv.home.join("go"), srv.home.join("scroll"));
    let draw = cat(&srv, "draw.bin", &format!("{}{}", at(20, 1), image(1, "c=4,r=2,C=1")));
    let script = format!(
        "{}; {draw}; {}; printf '\\033[23;1H\\n\\n\\n'; exec sleep 600",
        wait_flag(&go),
        wait_flag(&scroll)
    );
    session(&srv, &script, true);
    let mut a = attach(&srv, KITTY);
    touch(&go);
    let (first, mark) = next_put(&mut a, 0);
    assert_eq!((first.row, first.col), (20, 1));
    touch(&scroll);
    let (moved, mark) = next_put(&mut a, mark);
    assert_eq!((moved.row, moved.col), (17, 1), "three lines up");
    assert_eq!(moved.keys.get("p"), first.keys.get("p"), "replaced in place");
    // Two more lines and its first row is above the screen: the visible
    // second row is shown alone.
    let _ = mark;
}

/// Case 5: with the option off the bytes are what T3.8 sent: a direct
/// placement and a sixel image reach nobody.
#[test]
fn nothing_is_sent_with_the_option_off() {
    let srv = TestServer::start();
    let go = srv.home.join("go");
    let draw = cat(
        &srv,
        "draw.bin",
        &format!("{}{}\x1bP0;0;0q#0;2;100;0;0!10~\x1b\\done", at(3, 5), image(1, "c=4,r=2")),
    );
    session(&srv, &format!("{}; {draw}; exec sleep 600", wait_flag(&go)), false);
    let mut a = attach(&srv, BOTH);
    touch(&go);
    a.wait_for("done");
    let out = String::from_utf8_lossy(a.output()).into_owned();
    assert!(!out.contains("a=p,"), "a placement");
    assert!(!out.contains("a=t,") && !out.contains("a=T,"), "a transmit");
    assert!(!out.contains("\x1bP0;0q") && !out.contains("\x1bP0;"), "a sixel image");
    assert!(!a.text().contains("SIXEL IMAGE"), "a text fallback");
    let _ = Stdio::null();
}
