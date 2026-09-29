//! TC.12: `DCS tmux;` unwrap (spec §4.9). Apps that see `$TMUX` wrap
//! sequences in `ESC P tmux; ... ESC \`, every ESC inside doubled; grofe
//! handles the content as if sent bare, within the §4.9 set, and drops
//! anything else.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TestServer, wait_until};

use crate::harness::Harness;

const WAIT: Duration = Duration::from_secs(10);
const KITTY: &[(&str, &str)] = &[("GROFE_TEST_CAPS", "kitty_graphics,cell_px=10x20")];
const PLACEHOLDER: char = '\u{10EEEE}';

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `inner` wrapped as apps wrap it under `$TMUX`.
fn wrap(inner: &[u8]) -> Vec<u8> {
    let mut out = b"\x1bPtmux;".to_vec();
    for &b in inner {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// The fixture icat wrote, with its transmit APC wrapped.
fn icat_fixture(wrapped: bool) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/graphics/icat-placeholder-2x2.bin");
    let raw = std::fs::read(path).expect("fixture");
    if !wrapped {
        return raw;
    }
    let start = find(&raw, b"\x1b_G").expect("the transmit");
    let end = start + find(&raw[start..], b"\x1b\\").expect("its ST") + 2;
    [&raw[..start], &wrap(&raw[start..end]), &raw[end..]].concat()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Writes `parts` into files under the server's home and returns a
/// script that waits for a line of input (sent once a client is
/// attached, see [`attach`]), prints them in order, pausing between them
/// so each lands in its own read, then prints `DONE` and sleeps.
fn cat_parts(srv: &TestServer, parts: &[&[u8]]) -> String {
    let mut script = String::from("read l; ");
    for (i, part) in parts.iter().enumerate() {
        let file: PathBuf = srv.home.join(format!("part{i}"));
        std::fs::write(&file, part).expect("write part");
        if i > 0 {
            script.push_str("sleep 0.3; ");
        }
        script.push_str(&format!("cat {}; ", shell_quote(&file.to_string_lossy())));
    }
    script.push_str("echo DONE; exec sleep 600");
    script
}

/// Session `main` running `script`, sized for an 80x24 client.
fn new_session(srv: &TestServer, script: &str) {
    let cmd = format!("sh -c {}", shell_quote(script));
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        &cmd,
    ])
    .expect("new-session");
}

/// A client attached to `main`, past its first full redraw, that has let
/// the pane's script go on.
fn attach(srv: &TestServer, env: &[(&str, &str)]) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", env, 80, 24);
    a.wait_for_output(b"\x1b[2J", 0);
    a.send("\r");
    a
}

/// Every APC `_G` body in `bytes`, less the client's own probe query.
fn apcs(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split("\x1b_G")
        .skip(1)
        .filter_map(|s| s.split("\x1b\\").next())
        .filter(|s| !s.starts_with("i=31,"))
        .map(str::to_string)
        .collect()
}

/// What a kitty client of a fresh server sees once a pane has printed
/// `parts`: the graphics commands it was sent, and the pane rows.
fn graphics_stream(parts: &[&[u8]]) -> (Vec<String>, Vec<String>) {
    let srv = TestServer::start();
    new_session(&srv, &cat_parts(&srv, parts));
    let mut a = attach(&srv, KITTY);
    a.wait_for("DONE");
    a.wait_until("four placeholder cells", WAIT, |t| {
        t.matches(PLACEHOLDER).count() == 4
    });
    let rows = a.text().lines().take(23).map(str::to_string).collect();
    (apcs(a.output()), rows)
}

/// Case 1: a kitty graphics transmit wrapped for tmux gives the same
/// outer stream as the bare one: the same forwarded commands (after the
/// id remap) and the same placeholder cells.
#[test]
fn a_wrapped_kitty_transmit_matches_the_bare_one() {
    let (bare_apcs, bare_rows) = graphics_stream(&[&icat_fixture(false)]);
    assert!(
        bare_apcs.iter().any(|a| a.starts_with("a=T,")),
        "{bare_apcs:?}"
    );
    let (apcs, rows) = graphics_stream(&[&icat_fixture(true)]);
    assert_eq!(apcs, bare_apcs);
    assert_eq!(rows, bare_rows);
}

/// Case 2: the same wrapped transmit split across three reads, once
/// inside `ESC P tmux;` and once between the two ESCs of a doubled pair,
/// still unwraps to the same stream.
#[test]
fn a_wrapped_transmit_split_across_reads_still_unwraps() {
    let (bare_apcs, bare_rows) = graphics_stream(&[&icat_fixture(false)]);
    let wrapped = icat_fixture(true);
    let intro = find(&wrapped, b"\x1bPtmux;").expect("the wrapper");
    let pair = find(&wrapped, b"\x1b\x1b_G").expect("a doubled ESC");
    let (a, rest) = wrapped.split_at(intro + 4);
    let (b, c) = rest.split_at(pair + 1 - a.len());
    assert_eq!(c[0], 0x1b, "the cut is inside the pair");
    let (apcs, rows) = graphics_stream(&[a, b, c]);
    assert_eq!(apcs, bare_apcs);
    assert_eq!(rows, bare_rows);
}

/// Case 3: a wrapped OSC 52 store reaches the client exactly as a bare
/// one does (re-encoded, BEL-terminated, while `set-clipboard` is on), a
/// wrapped notification is cleaned of C1 controls like a bare one, and a
/// wrapped cursor colour is relayed like a bare one (R16).
#[test]
fn a_wrapped_clipboard_store_and_notification_are_reemitted() {
    let srv = TestServer::start();
    let parts = [
        &wrap(b"\x1b]52;c;aGVsbG8K\x1b\\")[..],
        &wrap(b"\x1b]9;x\xc2\x9c\xc2\x9b6n\x07")[..],
        &wrap(b"\x1b]12;#ff0000\x07")[..],
    ];
    new_session(&srv, &cat_parts(&srv, &parts));
    let mut a = attach(&srv, &[]);
    a.wait_for("DONE");
    a.wait_for_output(b"\x1b]52;c;aGVsbG8K\x07", 0);
    a.wait_for_output(b"\x1b]9;x6n\x07", 0);
    a.wait_for_output(b"\x1b]12;#ff0000\x07", 0);
    let out = a.output();
    assert!(find(out, b"\xc2\x9c").is_none() && find(out, b"\xc2\x9b").is_none());
    assert!(find(out, b"tmux;").is_none());
}

/// Case 4: wrapped text, a private CSI, a DCS, an OSC the bare path does
/// not act on, and content behind a CAN reach neither the pane's screen
/// nor the client.
#[test]
fn an_unknown_wrapped_sequence_reaches_neither_screen_nor_client() {
    let srv = TestServer::start();
    let parts = [
        &wrap(b"LEAK")[..],
        &wrap(b"\x1b[?7727h")[..],
        &wrap(b"\x1bPqLEAK\x1b\\")[..],
        &wrap(b"\x1b]4;1;rgb:ff/00/00\x07")[..],
        &wrap(b"\x1b]52;c;aGVsbG8K\x07LEAK")[..],
        &wrap(b"\x18LEAK\x1b[31mX")[..],
        b"shown\r\n",
    ];
    new_session(&srv, &cat_parts(&srv, &parts));
    let mut a = attach(&srv, &[]);
    a.wait_for("DONE");
    let screen = srv.capture("main");
    assert!(screen.contains("shown"), "{screen}");
    assert!(!screen.contains("LEAK"), "{screen}");
    let out = a.output();
    for leak in [
        &b"LEAK"[..],
        b"\x1b[?7727h",
        b"\x1b]4;",
        b"aGVsbG8K",
        b"tmux;",
    ] {
        assert!(
            find(out, leak).is_none(),
            "{:?} reached the client",
            String::from_utf8_lossy(leak)
        );
    }
}

/// Case 5: a wrapper that never ends (the pane exits mid-wrapper) hangs
/// nothing: the pane's exit is seen (`remain-on-exit` is on in tests), the
/// server goes on, and none of the wrapper reached the screen or client.
#[test]
fn an_unterminated_wrapper_is_dropped_when_the_pane_exits() {
    let srv = TestServer::start();
    let open: &[u8] = b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8K\x07LEAK";
    let script = cat_parts(&srv, &[open]).replace("echo DONE; exec sleep 600", "exit 7");
    new_session(&srv, &script);
    let mut a = attach(&srv, &[]);
    let dead = || {
        srv.cmd(&[
            "display-message",
            "-p",
            "-t",
            "main",
            "#{pane_dead} #{pane_dead_status}",
        ])
        .unwrap_or_default()
    };
    if let Err(e) = wait_until(|| dead().trim_end() == "1 7", WAIT) {
        panic!("{e}: pane_dead, status {:?}", dead());
    }
    assert!(srv.is_running());
    assert!(!srv.capture("main").contains("LEAK"));
    let out = a.output();
    assert!(find(out, b"aGVsbG8K").is_none());
    assert!(find(out, b"LEAK").is_none());
}

/// Fix round 1: `allow-passthrough` takes tmux 3.7b's values (window
/// scope, default `off`), so the `set -p allow-passthrough on` icat and
/// canopy send works; it changes nothing about unwrapping.
#[test]
fn allow_passthrough_is_an_option_with_tmux_values() {
    let srv = TestServer::start();
    new_session(&srv, "exec sleep 600");
    let show = |args: &[&str]| srv.cmd(args).map(|s| s.trim_end().to_string());
    assert_eq!(
        show(&["show", "-gw", "allow-passthrough"]),
        Ok("allow-passthrough off".to_string())
    );
    srv.cmd(&["set", "-p", "allow-passthrough", "on"])
        .expect("set -p on");
    assert_eq!(
        show(&["show", "-p", "allow-passthrough"]),
        Ok("allow-passthrough on".to_string())
    );
    srv.cmd(&["set", "allow-passthrough", "all"])
        .expect("set all");
    assert_eq!(
        show(&["show", "-w", "allow-passthrough"]),
        Ok("allow-passthrough all".to_string())
    );
    let err = srv
        .cmd(&["set", "allow-passthrough", "yes"])
        .expect_err("yes is not a value");
    assert!(err.contains("unknown value"), "{err}");
}
