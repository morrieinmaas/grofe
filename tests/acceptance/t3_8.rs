//! T3.8: kitty graphics through Unicode placeholders, end to end through
//! `grofe attach`. A pane's transmit reaches a client that claims kitty
//! graphics (`GROFE_TEST_CAPS`, since the reconstruction cannot answer the
//! probe) with a remapped id and `q=2`; its placeholder cells show that id
//! on the reconstructed screen, through a split and a scroll; a late
//! attach gets the image before its first frame; a client without kitty
//! graphics gets none of it; the pane's `a=q` is answered before DA1.
//!
//! The scripted fake program replays `tests/fixtures/graphics/`; the
//! `kitten` case runs only where `kitten` is installed.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::{Color, Processor, Rgb};
use common::TestServer;

use crate::harness::{Harness, on_path};

const WAIT: Duration = Duration::from_secs(10);
const PLACEHOLDER: char = '\u{10EEEE}';
const KITTY: &[(&str, &str)] = &[("GROFE_TEST_CAPS", "kitty_graphics,cell_px=10x20")];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/graphics")
        .join(name)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Session `main` on `srv`, running `script` under `sh`, sized for an
/// 80x24 client with a status line.
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

/// A client attached to `main` with `env`, past its first full redraw.
fn attach(srv: &TestServer, env: &[(&str, &str)]) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", env, 80, 24);
    a.wait_for_output(b"\x1b[2J", 0);
    a
}

/// Shell text that waits until `flag` exists.
fn wait_flag(flag: &Path) -> String {
    format!(
        "while [ ! -e {} ]; do sleep 0.05; done",
        shell_quote(&flag.to_string_lossy())
    )
}

fn touch(path: &Path) {
    std::fs::write(path, b"").expect("flag file");
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Every APC `_G` body in `bytes`.
fn apcs(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split("\x1b_G")
        .skip(1)
        .filter_map(|s| s.split("\x1b\\").next())
        .map(str::to_string)
        .collect()
}

/// The outer id of the first forwarded transmit in `bytes`, checking it
/// carries `q=2` and the fixture's data.
fn transmit_id(bytes: &[u8]) -> Option<u32> {
    let apc = apcs(bytes)
        .into_iter()
        .find(|a| a.starts_with("a=T,") && a.contains("U=1"))?;
    let (keys, data) = apc.split_once(';')?;
    assert!(keys.split(',').any(|k| k == "q=2"), "{keys}");
    assert!(data.starts_with("iVBORw0KGgo"), "the PNG data: {data}");
    keys.split(',')
        .find_map(|k| k.strip_prefix("i="))
        .and_then(|i| i.parse().ok())
}

/// The ids of every forwarded transmit in `bytes`.
fn transmit_ids(bytes: &[u8]) -> Vec<u32> {
    apcs(bytes)
        .iter()
        .filter(|a| a.starts_with("a=T,"))
        .filter_map(|a| {
            let keys = a.split(';').next()?;
            keys.split(',')
                .find_map(|k| k.strip_prefix("i="))?
                .parse()
                .ok()
        })
        .collect()
}

/// A placeholder cell on the reconstructed screen: where, the id its
/// colour gives, and its diacritics.
#[derive(Debug)]
struct Placeholder {
    row: usize,
    col: usize,
    fg: Color,
    marks: Vec<char>,
}

/// Replays `raw` on an 80x24 terminal and lists its placeholder cells.
fn placeholders(raw: &[u8]) -> Vec<Placeholder> {
    let mut term = Term::new(Config::default(), &TermSize::new(80, 24), VoidListener);
    Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut term, raw);
    let grid = term.grid();
    let mut out = Vec::new();
    for row in 0..grid.screen_lines() {
        for col in 0..grid.columns() {
            let cell = &grid[Line(row as i32)][Column(col)];
            if cell.c == PLACEHOLDER {
                out.push(Placeholder {
                    row,
                    col,
                    fg: cell.fg,
                    marks: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
                });
            }
        }
    }
    out
}

fn id_colour(id: u32) -> Color {
    Color::Spec(Rgb {
        r: (id >> 16) as u8,
        g: (id >> 8) as u8,
        b: id as u8,
    })
}

/// Waits until the reconstructed screen has `n` placeholder cells.
fn wait_placeholders(a: &mut Harness, n: usize) -> Vec<Placeholder> {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let found = placeholders(a.output());
        if found.len() == n {
            return found;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "wanted {n} placeholder cells, have {found:?}; screen:\n{}",
            a.text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Checks a 2x2 grid (or its lower part) in `id`'s colour, each cell's
/// row and column diacritics as icat wrote them (row `first_row` on top).
fn assert_grid(found: &[Placeholder], id: u32, first_row: usize) {
    let (row0, col0) = (found[0].row, found[0].col);
    for p in found {
        assert_eq!(p.fg, id_colour(id), "{p:?}");
        let (r, c) = (p.row - row0 + first_row, p.col - col0);
        let digit = ['\u{0305}', '\u{030D}'];
        assert_eq!(p.marks[..2], [digit[r], digit[c]], "{p:?}");
        // The high byte is 0: its diacritic may be dropped.
        assert!(p.marks[2..].iter().all(|m| *m == digit[0]), "{p:?}");
    }
}

/// Case 5: the scripted fake program's transmit reaches a kitty client
/// with a remapped id and `q=2`, the screen shows its placeholder cells in
/// that colour; they survive a split and a scroll; a second client gets
/// the image before its first frame; a client without kitty graphics gets
/// no image bytes.
#[test]
fn placeholder_images_reach_kitty_clients() {
    let srv = TestServer::start();
    let (show, scroll) = (srv.home.join("show"), srv.home.join("scroll"));
    let script = format!(
        "{}; cat {}; {}; seq 1 9; exec sleep 600",
        wait_flag(&show),
        shell_quote(&fixture("icat-placeholder-2x2.bin").to_string_lossy()),
        wait_flag(&scroll),
    );
    new_session(&srv, &script);
    // Another session's pane sends the same image id first, so `main`'s
    // image gets a different outer id: the remap shows.
    let fix = shell_quote(&fixture("icat-placeholder-2x2.bin").to_string_lossy());
    let other = format!(
        "sh -c {}",
        shell_quote(&format!("cat {fix}; exec sleep 600"))
    );
    srv.cmd(&["new-session", "-d", "-s", "other", &other])
        .expect("new-session other");

    let mut a = attach(&srv, KITTY);
    let first = a.wait_for_output(b"\x1b_Ga=T,", 0);
    let theirs = transmit_id(&a.output()[first - 7..]).expect("the other image");
    let mark = a.output().len();
    touch(&show);
    a.wait_for_output(b"\x1b_Ga=T,", mark);
    let found = wait_placeholders(&mut a, 4);
    let id = transmit_id(&a.output()[mark..]).expect("a forwarded transmit");
    assert_ne!(id, 1, "the fixture's own id");
    assert_ne!(id, theirs, "the other pane's image");
    assert_grid(&found, id, 0);

    // A split halves the pane; a scroll moves the image up one row.
    srv.cmd(&["split-window", "-t", "main", "sleep 600"])
        .expect("split-window");
    touch(&scroll);
    a.wait_for("9");
    let found = wait_placeholders(&mut a, 2);
    assert_grid(&found, id, 1);
    assert_eq!(found[0].row, 0, "scrolled up by one: {found:?}");

    // A late kitty client: the image before its first frame.
    let mut b = Harness::spawn_attach_with_env(srv.command(), "main", KITTY, 80, 24);
    b.wait_for_output(b"\x1b[2J", 0);
    let out = b.output().to_vec();
    let (t, frame) = (find(&out, b"\x1b_Ga=T,"), find(&out, b"\x1b[2J"));
    assert!(
        t.is_some_and(|t| frame.is_some_and(|f| t < f)),
        "transmit at {t:?}, first frame at {frame:?}"
    );
    assert!(
        transmit_ids(&out[..frame.unwrap_or(0)]).contains(&id),
        "{:?}",
        apcs(&out)
    );
    assert_grid(&wait_placeholders(&mut b, 2), id, 1);

    // Case 3: a client without kitty graphics gets no image bytes (only
    // its own probe query goes out).
    let mut c = attach(&srv, &[]);
    c.wait_for("9");
    let sent: Vec<String> = apcs(c.output())
        .into_iter()
        .filter(|a| !a.starts_with("i=31,"))
        .collect();
    assert!(sent.is_empty(), "{sent:?}");
}

/// Fix round 2: a client that stops reading, so its frames are dropped,
/// and then resumes gets each image's data once, not once per full
/// redraw.
#[test]
fn a_stalled_client_gets_each_image_once() {
    let srv = TestServer::start();
    let go = srv.home.join("go");
    let script = format!(
        "cat {}; {}; i=0; while [ $i -lt 300 ]; do printf '%070d\\n' $i; i=$((i+1)); \
         sleep 0.01; done; echo END; exec sleep 600",
        shell_quote(&fixture("icat-placeholder-2x2.bin").to_string_lossy()),
        wait_flag(&go),
    );
    new_session(&srv, &script);
    let mut a = attach(&srv, KITTY);
    a.wait_for_output(b"\x1b_Ga=T,", 0);
    // Stop the client (this test's own `grofe attach`) while the pane
    // prints: its socket fills and the server drops frames for it.
    let pattern = format!("{} attach", srv.socket.display());
    let pgrep = std::process::Command::new("pgrep")
        .args(["-f", &pattern])
        .output()
        .expect("pgrep");
    let pids: Vec<i32> = String::from_utf8_lossy(&pgrep.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert_eq!(pids.len(), 1, "the attach client: {pids:?}");
    let pid = nix::unistd::Pid::from_raw(pids[0]);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).expect("SIGSTOP");
    touch(&go);
    std::thread::sleep(Duration::from_secs(3));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT).expect("SIGCONT");
    a.wait_for("END");
    let ids = transmit_ids(a.output());
    assert_eq!(ids.len(), 1, "{ids:?}");
}

/// Runs a pane script that sends `query` then DA1 and prints, with `od`,
/// what came back once the replies go quiet; returns that text.
fn query_replies(query: &str, env: &[(&str, &str)]) -> String {
    let srv = TestServer::start();
    let go = srv.home.join("go");
    let script = format!(
        "stty -icanon -echo min 0 time 5; {}; printf '\\033_G{query}\\033\\\\\\033[c'; \
         dd bs=1024 count=8 2>/dev/null | od -An -c | tr -d ' \\n'; echo; echo DONE; \
         exec sleep 600",
        wait_flag(&go),
    );
    new_session(&srv, &script);
    let mut a = attach(&srv, env);
    touch(&go);
    a.wait_for("DONE");
    let text = a.text();
    text.lines()
        .find(|l| l.contains("[?"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Case 8: the pane's `a=q` is answered before the emulator's DA1 reply
/// from the same read; an error while no client shows kitty graphics;
/// nothing at all with `q=2`.
#[test]
fn queries_are_answered_before_da1() {
    let q = "i=31,s=1,v=1,a=q,t=d,f=24;AAAA";
    let got = query_replies(q, KITTY);
    let (ok, da1) = (got.find("i=31;OK"), got.find("[?"));
    assert!(ok.is_some_and(|o| da1.is_some_and(|d| o < d)), "{got:?}");
    // DA1 as tmux 3.7b answers it; kitty's tools refuse alacritty's
    // shorter `ESC[?6c`.
    assert!(got.ends_with("033[?1;2;4c"), "{got:?}");

    let got = query_replies(q, &[]);
    let (err, da1) = (got.find("i=31;EINVAL"), got.find("[?"));
    assert!(err.is_some_and(|e| da1.is_some_and(|d| e < d)), "{got:?}");

    let got = query_replies("i=31,s=1,v=1,a=q,t=d,f=24,q=2;AAAA", KITTY);
    assert!(!got.contains("_G"), "{got:?}");
    assert!(got.contains("[?"), "{got:?}");
}

/// Case 6: `kitten icat --unicode-placeholder`, with no
/// `--use-window-size`, runs in a pane (the pane has a pixel size from the
/// client's claimed cell size), is answered, exits 0, and its image shows
/// as in case 5. Skipped where `kitten` is not installed.
#[test]
fn kitten_icat_draws_through_grofe() {
    icat_draws(&[]);
}

/// Case 6 with detection skipped: icat transmits by shared memory, which
/// grofe reads and forwards as inline data.
#[test]
fn kitten_icat_by_shared_memory_draws_through_grofe() {
    icat_draws(&["--transfer-mode=memory"]);
}

fn icat_draws(args: &[&str]) {
    if !on_path("kitten") {
        eprintln!("skipping: kitten not found on PATH");
        return;
    }
    let srv = TestServer::start();
    let go = srv.home.join("go");
    let script = format!(
        "{}; kitten icat --unicode-placeholder --stdin=no {} {}; echo EXIT=$?; exec sleep 600",
        wait_flag(&go),
        args.join(" "),
        shell_quote(&fixture("red-blue-4x4.png").to_string_lossy()),
    );
    new_session(&srv, &script);
    let mut a = attach(&srv, KITTY);
    let mark = a.output().len();
    touch(&go);
    a.wait_for("EXIT=");
    assert!(a.text().contains("EXIT=0"), "{}", a.text());
    let out = a.output()[mark..].to_vec();
    let apc = apcs(&out)
        .into_iter()
        .find(|a| a.contains("U=1") && a.contains("q=2"))
        .unwrap_or_else(|| panic!("no transmit in {:?}", apcs(&out)));
    let (keys, data) = apc.split_once(';').unwrap_or_default();
    assert!(!keys.contains("t="), "forwarded as inline data: {keys}");
    assert!(data.starts_with("iVBORw0KGgo"), "the PNG data: {data}");
    let id: u32 = keys
        .split(',')
        .find_map(|k| k.strip_prefix("i="))
        .and_then(|i| i.parse().ok())
        .expect("an id");
    let found = placeholders(a.output());
    assert!(!found.is_empty(), "no placeholder cells:\n{}", a.text());
    for p in &found {
        assert_eq!(p.fg, id_colour(id), "{p:?}");
    }
}
