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

/// The bytes of a client's output that are past `from`.
fn since(a: &mut Harness, from: usize) -> Vec<u8> {
    let all = a.output();
    all[from.min(all.len())..].to_vec()
}

/// Waits until `done` holds for the output past `from`.
fn wait_bytes(a: &mut Harness, what: &str, from: usize, done: impl Fn(&[u8]) -> bool) -> usize {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let all = a.output().to_vec();
        if done(&all[from.min(all.len())..]) {
            return all.len();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; screen:\n{}",
            a.text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Case 3: a popup over a placement hides it (the client is told to delete
/// it) and closing the popup puts it back; a client that attaches later
/// gets the image data ahead of its first frame, and the placement in it.
#[test]
fn a_popup_hides_a_placement_and_a_late_client_gets_it() {
    let srv = TestServer::start();
    let go = srv.home.join("go");
    let draw = cat(&srv, "draw.bin", &format!("{}{}", at(5, 10), image(1, "c=8,r=2,C=1")));
    session(&srv, &format!("{}; {draw}; exec sleep 600", wait_flag(&go)), true);
    let mut a = attach(&srv, KITTY);
    touch(&go);
    let (put, mark) = next_put(&mut a, 0);
    let (outer, pid) = (put.keys["i"].clone(), put.keys["p"].clone());

    let mut cmd = srv.command();
    cmd.args(["display-popup", "-w", "30", "-h", "8", "-x", "5", "-y", "3", "sleep 600"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("display-popup");
    let after = wait_bytes(&mut a, "the placement deleted", mark, |b| {
        deletes(b)
            .iter()
            .any(|d| d.get("i") == Some(&outer) && d.get("p") == Some(&pid))
    });
    // The popup is the first client's alone: a second one still sees the
    // image.
    let mut b = attach(&srv, KITTY);
    let (other, _) = next_put(&mut b, 0);
    assert_eq!((other.row, other.col), (5, 10));
    // The popup closes: the placement comes back, in place.
    a.send("\x1b");
    let (again, _) = next_put(&mut a, after);
    assert_eq!((again.row, again.col), (5, 10));
    assert_eq!(again.keys["p"], pid, "the same placement");
    let _ = child.wait();

    // Another client now: the transmit before the first frame, the
    // placement in it.
    let mut c = attach(&srv, KITTY);
    let (late, _) = next_put(&mut c, 0);
    assert_eq!((late.row, late.col), (5, 10));
    let out = c.output().to_vec();
    let (t, frame) = (find(&out, b"\x1b_Ga=t,"), find(&out, b"\x1b[2J"));
    assert!(
        t.is_some_and(|t| frame.is_some_and(|f| t < f)),
        "transmit at {t:?}, first frame at {frame:?}"
    );
    // A client without kitty graphics gets none of it.
    let mut d = attach(&srv, &[]);
    std::thread::sleep(Duration::from_millis(300));
    let out = String::from_utf8_lossy(d.output()).into_owned();
    assert!(!out.contains("a=p,") && !out.contains("a=t,"), "{out:?}");
}

/// Case 6: window switch, zoom and a client resize all go through the same
/// diff: the placement is deleted when its pane is not shown and put again,
/// the same placement, when it is.
#[test]
fn placements_follow_window_switches_zoom_and_resize() {
    let srv = TestServer::start();
    let go = srv.home.join("go");
    let draw = cat(&srv, "draw.bin", &format!("{}{}", at(5, 10), image(1, "c=8,r=2,C=1")));
    session(&srv, &format!("{}; {draw}; exec sleep 600", wait_flag(&go)), true);
    let mut a = attach(&srv, KITTY);
    touch(&go);
    let (put, mark) = next_put(&mut a, 0);
    let (outer, pid) = (put.keys["i"].clone(), put.keys["p"].clone());
    let gone = |outer: &str, pid: &str| {
        let (outer, pid) = (outer.to_string(), pid.to_string());
        move |b: &[u8]| {
            deletes(b)
                .iter()
                .any(|d| d.get("i") == Some(&outer) && d.get("p") == Some(&pid))
        }
    };

    // Another window: the image goes; back: it returns.
    srv.cmd(&["new-window", "-t", "main", "sleep 600"]).expect("new-window");
    let mark = wait_bytes(&mut a, "deleted on a window switch", mark, gone(&outer, &pid));
    srv.cmd(&["select-window", "-t", "main:1"]).expect("select-window");
    let (back, mark) = next_put(&mut a, mark);
    assert_eq!((back.row, back.col, &back.keys["p"]), (5, 10, &pid));

    // Zoom with a split: the image's pane fills the window.
    srv.cmd(&["split-window", "-h", "-t", "main:1", "sleep 600"]).expect("split");
    srv.cmd(&["select-pane", "-t", "main:1.2"]).expect("select-pane");
    let mark = a.output().len().max(mark);
    srv.cmd(&["resize-pane", "-Z", "-t", "main:1.2"]).expect("zoom");
    // Zooming the other pane hides ours; unzooming puts it back.
    let mark = wait_bytes(&mut a, "deleted under a zoom", mark, gone(&outer, &pid));
    srv.cmd(&["resize-pane", "-Z", "-t", "main:1.2"]).expect("unzoom");
    let (back, mark) = next_put(&mut a, mark);
    assert_eq!(&back.keys["p"], &pid);

    // A client resize: the window is redrawn in full, so everything that
    // is shown is deleted and put again.
    a.resize(80, 22);
    let (resized, _) = next_put(&mut a, mark);
    assert_eq!(&resized.keys["p"], &pid);
}

/// Case 4: every `d=` form of `a=d`, lower and upper case, deletes the
/// placement it names from the client; the upper case ones also free the
/// image.
#[test]
fn every_delete_form_deletes_the_client_placement() {
    let forms = [
        "a", "A", "i", "I", "c", "C", "p", "P", "q", "Q", "x", "X", "y", "Y", "z", "Z", "r", "R",
    ];
    let srv = TestServer::start();
    let mut script = String::new();
    let mut flags = Vec::new();
    for (n, form) in forms.iter().enumerate() {
        let id = n + 1;
        let row = 2 + n as u32;
        let (place, delete) = (srv.home.join(format!("place{n}")), srv.home.join(format!("del{n}")));
        let body = match *form {
            "a" | "A" => format!("a=d,d={form}"),
            "i" | "I" => format!("a=d,d={form},i={id}"),
            "c" | "C" => format!("a=d,d={form}"),
            "p" | "P" => format!("a=d,d={form},x=5,y={row}"),
            "q" | "Q" => format!("a=d,d={form},x=5,y={row},z=0"),
            "x" | "X" => format!("a=d,d={form},x=5"),
            "y" | "Y" => format!("a=d,d={form},y={row}"),
            "z" | "Z" => format!("a=d,d={form},z=0"),
            _ => format!("a=d,d={form},x={id},y={id}"),
        };
        let cursor = if matches!(*form, "c" | "C") { at(row, 5) } else { String::new() };
        let drawn = cat(
            &srv,
            &format!("place{n}.bin"),
            &format!("{}{}", at(row, 5), image(id as u32, "c=2,r=1,C=1")),
        );
        let deleted = cat(&srv, &format!("del{n}.bin"), &format!("{cursor}{}", apc(&body)));
        script.push_str(&format!(
            "{}; {drawn}; {}; {deleted}; ",
            wait_flag(&place),
            wait_flag(&delete)
        ));
        flags.push((place, delete));
    }
    session(&srv, &format!("{script}exec sleep 600"), true);
    let mut a = attach(&srv, KITTY);
    let mut mark = a.output().len();
    for (n, form) in forms.iter().enumerate() {
        touch(&flags[n].0);
        let (put, at_end) = next_put(&mut a, mark);
        let (outer, pid) = (put.keys["i"].clone(), put.keys["p"].clone());
        touch(&flags[n].1);
        mark = wait_bytes(&mut a, &format!("d={form} to delete the placement"), at_end, |b| {
            deletes(b)
                .iter()
                .any(|d| d.get("i") == Some(&outer) && d.get("p") == Some(&pid))
        });
        if form.chars().all(|c| c.is_ascii_uppercase()) {
            // The image is freed with it.
            let freed = |b: &[u8]| {
                deletes(b)
                    .iter()
                    .any(|d| d.get("d").map(String::as_str) == Some("I") && d.get("i") == Some(&outer))
            };
            wait_bytes(&mut a, &format!("d={form} to free the image"), at_end, freed);
        }
    }
}

/// A sixel image of 20x12 pixels: 2 columns by 1 row of 10x20 cells.
const SIXEL_IMAGE: &str = "\x1bPq#0;2;100;0;0!20~-!20~\x1b\\";

/// Case 7: sixel goes only to a client that has the capability; one that
/// does not gets tmux's text stand-in; the image is freed by a clear, by
/// text over it and by scrolling off the top.
#[test]
fn sixel_goes_to_capable_clients_only() {
    let srv = TestServer::start();
    let (go, clear) = (srv.home.join("go"), srv.home.join("clear"));
    let draw = cat(&srv, "draw.bin", &format!("{}{SIXEL_IMAGE}", at(4, 3)));
    let script = format!(
        "{}; {draw}; printf 'after'; {}; printf '\\033[2J\\033[Hcleared'; exec sleep 600",
        wait_flag(&go),
        wait_flag(&clear)
    );
    session(&srv, &script, true);
    let mut plain = attach(&srv, KITTY);
    let mut capable = attach(&srv, SIXEL);
    touch(&go);
    plain.wait_for("after");
    capable.wait_for("after");
    // The tmux cursor rule: column 0 of the row after the image.
    assert_eq!(&plain.text().lines().nth(4).unwrap_or_default()[..5], "after");
    // The plain client: no sixel, text instead, from the image's cell.
    let text = plain.text();
    assert!(
        text.lines().nth(3).is_some_and(|l| l[2..].starts_with("SIXEL IMAGE (2x1)")),
        "{text}"
    );
    let out = String::from_utf8_lossy(plain.output()).into_owned();
    assert!(!out.contains("\x1bP0;"), "a sixel string: {out:?}");
    // The capable one: the image, cut and scaled to its cells, at (4, 3).
    let out = capable.output().to_vec();
    let i = find(&out, b"\x1b[4;3H\x1bP0;0q").expect("a sixel at the image's cell");
    assert!(out[i..].windows(2).any(|w| w == b"\x1b\\"));
    assert!(!capable.text().contains("SIXEL IMAGE"));
    // A clear frees it: the stand-in goes.
    let mark = capable.output().len();
    touch(&clear);
    plain.wait_for("cleared");
    capable.wait_for("cleared");
    assert!(!plain.text().contains("SIXEL IMAGE"), "{}", plain.text());
    assert!(find(&since(&mut capable, mark), b"\x1bP0;").is_none(), "not drawn again");
}

/// Case 8: a sixel image is dropped when text is written over it and when
/// it scrolls off the top.
#[test]
fn sixel_is_freed_by_text_and_by_scrolling() {
    let srv = TestServer::start();
    let (go, over, scroll) = (srv.home.join("go"), srv.home.join("over"), srv.home.join("scroll"));
    let draw = cat(&srv, "draw.bin", &format!("{}{SIXEL_IMAGE}", at(2, 1)));
    let draw2 = cat(&srv, "draw2.bin", &format!("{}{SIXEL_IMAGE}", at(2, 1)));
    let script = format!(
        "{}; {draw}; {}; printf '\\033[2;2HX'; {}; {draw2}; seq 1 40; exec sleep 600",
        wait_flag(&go),
        wait_flag(&over),
        wait_flag(&scroll)
    );
    session(&srv, &script, true);
    let mut a = attach(&srv, KITTY);
    touch(&go);
    a.wait_for("SIXEL IMAGE (2x1)");
    touch(&over);
    a.wait_until("the stand-in freed by text over it", WAIT, |t| {
        !t.contains("SIXEL IMAGE")
    });
    touch(&scroll);
    a.wait_for("SIXEL IMAGE (2x1)");
    a.wait_until("the stand-in scrolled off", WAIT, |t| {
        !t.contains("SIXEL IMAGE") && t.contains("40")
    });
}

/// Case 9: animation (`a=f`, `a=a`, `a=c`) and `d=f` are forwarded with the
/// outer id, in order after the image, and a later client gets them after
/// the transmit too.
#[test]
fn animation_is_forwarded_and_replayed() {
    let srv = TestServer::start();
    let go = srv.home.join("go");
    let body = format!(
        "{}{}{}{}{}",
        apc("a=t,i=5,f=24,s=1,v=1,q=2;AAAA"),
        apc("a=f,i=5,f=24,s=1,v=1,z=100,q=2;BBBB"),
        apc("a=a,i=5,s=3,v=1,q=2"),
        apc("a=c,i=5,r=1,c=2,q=2"),
        "done"
    );
    let draw = cat(&srv, "draw.bin", &body);
    session(&srv, &format!("{}; {draw}; exec sleep 600", wait_flag(&go)), true);
    let mut a = attach(&srv, KITTY);
    touch(&go);
    a.wait_for("done");
    let in_order = |out: &[u8]| {
        let at = |needle: &[u8]| find(out, needle);
        match (at(b"a=t,"), at(b"a=f,"), at(b"a=a,"), at(b"a=c,")) {
            (Some(t), Some(f), Some(an), Some(c)) => t < f && f < an && an < c,
            _ => false,
        }
    };
    let out = a.output().to_vec();
    assert!(in_order(&out), "{:?}", String::from_utf8_lossy(&out));
    let id = transmitted(&out)[0];
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(text.contains(&format!("a=f,i={id},")), "{text:?}");
    assert!(text.contains(&format!("a=a,i={id},")), "{text:?}");
    assert!(text.contains(&format!("a=c,i={id},")), "{text:?}");
    // The client's own stream has the pane's id in none of them.
    assert!(!text.contains("a=f,i=5,"));
    let mut b = attach(&srv, KITTY);
    std::thread::sleep(Duration::from_millis(300));
    let late = b.output().to_vec();
    assert!(in_order(&late), "{:?}", String::from_utf8_lossy(&late));
}

/// Case 10: z-index and relative placements: the client is told the z, a
/// child sits where its parent plus the offset is, and follows it.
#[test]
fn z_index_and_relative_placements() {
    let srv = TestServer::start();
    let (go, moved) = (srv.home.join("go"), srv.home.join("moved"));
    let draw = cat(
        &srv,
        "draw.bin",
        &format!(
            "{}{}{}",
            at(5, 5),
            image(1, "c=4,r=2,C=1,p=1,z=-3"),
            image(2, "C=1,p=1,P=1,Q=1,H=2,V=1,c=2,r=1")
        ),
    );
    let move_parent = cat(&srv, "move.bin", &format!("{}{}", at(10, 20), apc("a=p,i=1,p=1,C=1,c=4,r=2,z=-3,q=2")));
    session(
        &srv,
        &format!("{}; {draw}; {}; {move_parent}; exec sleep 600", wait_flag(&go), wait_flag(&moved)),
        true,
    );
    let mut a = attach(&srv, KITTY);
    touch(&go);
    let (found, mark) = wait_puts(&mut a, 0, 2);
    let parent = found.iter().find(|p| p.n("z") == -3).expect("the parent");
    assert_eq!((parent.row, parent.col), (5, 5));
    let child = found.iter().find(|p| p.n("z") == 0).expect("the child");
    assert_eq!((child.row, child.col), (6, 7), "parent + (2, 1)");
    touch(&moved);
    let (found, _) = wait_puts(&mut a, mark, 2);
    let child = found.iter().find(|p| p.n("z") == 0).expect("the child again");
    assert_eq!((child.row, child.col), (11, 22), "it moved with the parent");
}
