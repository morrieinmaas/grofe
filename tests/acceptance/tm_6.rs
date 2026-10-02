//! TM.6 acceptance: the mouse in copy mode. SGR 1006 reports are typed
//! into an attached client's pty as the outer terminal sends them. The
//! wheel over a pane that did not ask for the mouse enters copy mode, a
//! drag selects and its release runs `MouseDragEnd1Pane`, a double or
//! triple click copies a word or a line, as tmux 3.7b's stock root and
//! `copy-mode-vi` rows do; panes that asked for the mouse still get it.
//!
//! The goldens (`tests/fixtures/compat/golden/tm_6/`) were recorded from
//! tmux 3.7b with the same reports and are replayed here natively. The
//! tests from T3.2 and TC.10 that exercised the old read-only scrollback
//! view moved here, rewritten for copy mode.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use common::{CMD_TIMEOUT, TestServer, wait_until};

use crate::harness::{Harness, require_tool};

const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const WAIT: Duration = Duration::from_secs(10);

/// The golden recorder's pause after each `keys` step (`replay.rs`).
const KEYS_PAUSE: Duration = Duration::from_millis(200);

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compat")
}

fn golden_text(name: &str) -> String {
    fs::read_to_string(fixtures().join(format!("golden/tm_6/{name}.golden"))).unwrap()
}

// ---- Golden replay -----------------------------------------------------------

/// A scenario line's words, split as the replayer splits them.
fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' if word.is_some() => words.extend(word.take()),
            ' ' | '\t' => {}
            '\'' => {
                let w = word.get_or_insert_default();
                for c in it.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    w.push(c);
                }
            }
            '"' => {
                let w = word.get_or_insert_default();
                while let Some(c) = it.next() {
                    match c {
                        '"' => break,
                        '\\' => match it.next() {
                            Some(n @ ('"' | '\\' | '$' | '`')) => w.push(n),
                            Some(n) => {
                                w.push('\\');
                                w.push(n);
                            }
                            None => w.push('\\'),
                        },
                        c => w.push(c),
                    }
                }
            }
            '\\' => word.get_or_insert_default().extend(it.next()),
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// The bytes a `keys` step types (the recorder's `key_bytes`).
fn key_bytes(words: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for word in words {
        match word.as_str() {
            "Escape" => out.push(0x1b),
            w => out.extend_from_slice(w.as_bytes()),
        }
    }
    out
}

enum Step {
    Run {
        line: String,
        args: Vec<String>,
        want: (String, String, i32),
    },
    Client(u16, u16),
    Keys(Vec<u8>),
}

fn load(name: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    for line in golden_text(name).lines() {
        if let Some(args) = line.strip_prefix("$ ") {
            let args = split_words(args)
                .into_iter()
                .map(|a| {
                    a.replace("{fixtures}", &fixtures().display().to_string())
                        .replace("{grofe}", env!("CARGO_BIN_EXE_grofe"))
                })
                .collect();
            steps.push(Step::Run {
                line: line.to_string(),
                args,
                want: (String::new(), String::new(), 0),
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("client ") {
            let size = rest.split(' ').next().unwrap();
            let (c, r) = size.split_once('x').unwrap();
            steps.push(Step::Client(c.parse().unwrap(), r.parse().unwrap()));
            continue;
        }
        if let Some(rest) = line.strip_prefix("keys ") {
            steps.push(Step::Keys(key_bytes(&split_words(rest))));
            continue;
        }
        let Some(Step::Run { want, .. }) = steps.last_mut() else {
            panic!("{name}: output before a command: {line}");
        };
        if let Some(status) = line.strip_prefix("? ") {
            want.2 = status.parse().unwrap();
        } else if let Some(err) = line.strip_prefix("! ") {
            want.1.push_str(err);
            want.1.push('\n');
        } else {
            want.0.push_str(line);
            want.0.push('\n');
        }
    }
    steps
}

/// Commands that only read, which a replay may run again.
fn reads_only(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("display" | "show-buffer" | "list-buffers" | "run-shell")
    )
}

/// Runs golden `name` on a fresh server, comparing each command with tmux.
/// Typed reports reach the server in order but after the CLI's commands
/// may, and a click's copy waits for the click timer: the reading
/// commands after a report are retried until they show what tmux's did.
fn replay(name: &str) {
    let srv = TestServer::start();
    let mut client: Option<Harness> = None;
    let mut typed = false;
    for step in load(name) {
        match step {
            Step::Run { line, args, want } => {
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                let run = || {
                    let out = srv.run(&argv);
                    let mut stdout = out.stdout;
                    if !stdout.is_empty() && !stdout.ends_with('\n') {
                        stdout.push('\n');
                    }
                    (stdout, out.stderr, out.status.code().unwrap_or(-1))
                };
                if typed && reads_only(&args) {
                    let _ = wait_until(|| run() == want, CMD_TIMEOUT);
                } else {
                    typed = false;
                }
                assert_eq!(run(), want, "{name}: {line}");
            }
            Step::Client(cols, rows) => {
                let a = Harness::spawn_attach_with_env(srv.command(), "main", &[], cols, rows);
                wait_until(
                    || !srv.cmd(&["list-clients"]).unwrap_or_default().is_empty(),
                    CMD_TIMEOUT,
                )
                .expect("client attached");
                client = Some(a);
            }
            Step::Keys(bytes) => {
                let a = client.as_mut().expect("keys before a client");
                a.send(&bytes);
                let _ = a.text();
                // The recorder's pause after each step, so presses the
                // golden keeps apart are no double click here either.
                std::thread::sleep(KEYS_PAUSE);
                typed = true;
            }
        }
    }
}

/// Case 1, as tmux: the first tick enters copy mode at the bottom, each
/// later one scrolls five lines, and scrolling past the bottom leaves.
#[test]
fn the_wheel_matches_the_golden() {
    replay("wheel");
}

/// Case 3: a drag from (2,3) to (20,5) outside copy mode, one inside it,
/// and one backwards from a scrolled view; each release copies.
#[test]
fn drags_match_the_golden() {
    replay("drag");
}

/// Case 4: double clicks on each kind of column, outside and inside copy
/// mode, copy the word tmux copies.
#[test]
fn double_clicks_match_the_golden() {
    replay("double");
}

/// Case 5: triple clicks copy the whole line, outside and inside copy mode.
#[test]
fn triple_clicks_match_the_golden() {
    replay("triple");
}

/// Case 6: a click in copy mode leaves the cursor where it was and starts
/// no selection: tmux 3.7b's `copy-mode-vi` `MouseDown1Pane` is
/// `select-pane` alone (the brief's "moves the cursor" is not what tmux
/// does).
#[test]
fn a_click_in_copy_mode_matches_the_golden() {
    replay("click");
}

// ---- Through an attached client ----------------------------------------------

/// 200 lines of history, then a pane that shows every byte it gets.
const HISTORY: &str = "seq 1 200; stty -echo; exec cat -v";

/// A server with one session `main` running `cmd`, sized for an 80x24
/// client with a status line.
fn start(cmd: &str) -> TestServer {
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
        cmd,
    ])
    .expect("new-session");
    srv
}

fn attach(srv: &TestServer, cols: u16, rows: u16) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], cols, rows);
    a.wait_for_output(MOUSE_ON, 0);
    a
}

/// An SGR 1006 report as the outer terminal sends it: `x` and `y` are
/// 1-based client cells.
fn sgr(button: u8, x: u16, y: u16, release: bool) -> String {
    let end = if release { 'm' } else { 'M' };
    format!("\x1b[<{button};{x};{y}{end}")
}

const WHEEL_UP: u8 = 64;
const WHEEL_DOWN: u8 = 65;
const DRAG: u8 = 32;

fn top_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

/// The `[offset/history]` indicator anywhere on screen.
fn indicator(text: &str) -> Option<String> {
    text.match_indices('[').find_map(|(i, _)| {
        let rest = &text[i + 1..];
        let end = rest.find(']')?;
        let (a, b) = rest[..end].split_once('/')?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
        (digits(a) && digits(b)).then(|| format!("[{a}/{b}]"))
    })
}

fn pane_var(srv: &TestServer, target: &str, var: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, var])
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Case 1 on screen (moved from T3.2's `wheel_in_a_shell_scrolls_history`,
/// where one tick opened the view five lines up): the first tick shows
/// `[0/178]`, the second scrolls to `[5/178]` with line 174 on top, and
/// scrolling back down leaves copy mode; the pane gets nothing.
#[test]
fn the_wheel_in_a_shell_shows_the_indicator_and_history() {
    let srv = start(HISTORY);
    let mut a = attach(&srv, 80, 24);
    // 200 lines and the cursor's empty line on 23 rows: 178 in history.
    a.wait_until("179 on top", WAIT, |t| top_line(t).trim_end() == "179");

    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.wait_until("copy mode at the bottom", WAIT, |t| {
        indicator(t).as_deref() == Some("[0/178]")
    });
    assert_eq!(pane_var(&srv, "main", "#{pane_in_mode}"), "1");
    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.wait_until("five lines up", WAIT, |t| {
        top_line(t).starts_with("174") && indicator(t).as_deref() == Some("[5/178]")
    });
    let text = a.text();
    assert!(top_line(&text).trim_end().ends_with("[5/178]"), "{text}");

    a.send(sgr(WHEEL_DOWN, 10, 5, false));
    a.wait_until("copy mode left", WAIT, |t| {
        top_line(t).trim_end() == "179" && indicator(t).is_none()
    });
    assert_eq!(pane_var(&srv, "main", "#{pane_in_mode}"), "0");

    a.send("z\r");
    a.wait_until("the z line", WAIT, |t| {
        t.lines().any(|l| l.trim_end() == "z")
    });
    let pane = srv.capture("main");
    assert!(!pane.contains("^["), "the pane got bytes:\n{pane}");
}

/// Case 2: Neovim asked for the mouse, so it gets the wheel and the pane
/// does not enter copy mode.
#[test]
fn the_wheel_in_neovim_scrolls_neovim() {
    if !require_tool("nvim") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("lines.txt");
    let body: String = (1..=200).map(|n| format!("{n}\n")).collect();
    fs::write(&file, body).expect("write lines");
    let srv = start(&format!(
        "exec nvim --clean -c 'set mouse=a' '{}'",
        file.display()
    ));
    let mut a = attach(&srv, 80, 24);
    a.wait_until("nvim showing line 1", WAIT, |t| {
        top_line(t).trim_end() == "1" && t.lines().any(|l| l.trim_end() == "21")
    });

    a.send(sgr(WHEEL_DOWN, 10, 5, false));
    a.wait_until("nvim scrolled", WAIT, |t| {
        top_line(t).trim_end().parse::<u32>().is_ok_and(|n| n > 1)
    });
    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.wait_until("nvim back at the top", WAIT, |t| {
        top_line(t).trim_end() == "1"
    });
    assert_eq!(pane_var(&srv, "main", "#{pane_in_mode}"), "0");
    assert_eq!(indicator(&a.text()), None, "{}", a.text());
}

/// The pane `cat`s the copy fixture and waits, on a session sized for an
/// 80x25 client, as the goldens' panes do.
fn fixture_server() -> TestServer {
    let srv = TestServer::start();
    let cmd = format!(
        "cat '{}'; exec sleep 100000",
        fixtures().join("../copy/fixture.txt").display()
    );
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "24",
        &cmd,
    ])
    .expect("new-session");
    srv
}

/// Case 3's binding: `MouseDragEnd1Pane` runs whatever it is bound to. A
/// stub copy command gets the dragged text, which is also the newest
/// buffer, the same text tmux's drag golden copied.
#[test]
fn a_drag_release_runs_its_binding_with_the_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("copied");
    let srv = fixture_server();
    srv.cmd(&[
        "bind-key",
        "-T",
        "copy-mode-vi",
        "MouseDragEnd1Pane",
        "send-keys",
        "-X",
        "copy-pipe-and-cancel",
        &format!("cat > '{}'", out.display()),
    ])
    .expect("bind");
    let mut a = attach(&srv, 80, 25);
    a.wait_for("stray close");
    a.send(sgr(0, 3, 4, false));
    a.send(sgr(DRAG, 5, 4, false));
    a.send(sgr(DRAG, 21, 6, false));
    a.send(sgr(0, 21, 6, true));
    let want = "matched ( open bracket and ] stray close\n\
                words    with    wide    gaps    between    them\n\
                漢字 wide 文字 cells ";
    wait_until(|| fs::read_to_string(&out).is_ok_and(|t| t == want), WAIT)
        .unwrap_or_else(|e| panic!("{e}: {:?}", fs::read_to_string(&out)));
    assert_eq!(srv.cmd(&["show-buffer"]).as_deref(), Ok(want));
    assert_eq!(pane_var(&srv, "main", "#{pane_in_mode}"), "0");
}

/// A drag held on the pane's top row scrolls the view up while it stays
/// there (tmux's 50 ms drag timer), so the selection reaches lines that
/// were above the screen.
#[test]
fn a_drag_held_on_the_top_row_scrolls_up() {
    let srv = fixture_server();
    let mut a = attach(&srv, 80, 25);
    a.wait_for("stray close");
    a.send(sgr(0, 3, 6, false));
    a.send(sgr(DRAG, 4, 6, false));
    a.send(sgr(DRAG, 4, 1, false));
    wait_until(
        || {
            pane_var(&srv, "main", "#{scroll_position}")
                .parse::<u32>()
                .is_ok_and(|n| n >= 3)
        },
        WAIT,
    )
    .expect("the view scrolled up");
    a.send(sgr(0, 4, 1, true));
    wait_until(|| pane_var(&srv, "main", "#{pane_in_mode}") == "0", WAIT)
        .expect("the release copied and left");
    let copied = srv.cmd(&["show-buffer"]).expect("a buffer");
    assert!(copied.lines().count() >= 8, "{copied:?}");
}

/// Two panes side by side; the right one runs `cmd` and is active.
fn side_by_side(left: &str, right: &str) -> (TestServer, String, String) {
    let srv = start(left);
    let left_id = pane_var(&srv, "main", "#{pane_id}");
    srv.cmd(&["split-window", "-h", "-t", "main", right])
        .expect("split");
    let right_id = pane_var(&srv, "main", "#{pane_id}");
    assert_ne!(left_id, right_id);
    (srv, left_id, right_id)
}

/// Case 6 with two panes (moved from T3.2's
/// `a_click_inside_the_view_focuses_and_keeps_it`): a click in a pane in
/// copy mode focuses it and keeps the mode, its view and its cursor; the
/// pane gets no bytes.
#[test]
fn a_click_in_copy_mode_focuses_and_keeps_it() {
    let (srv, left, right) = side_by_side(HISTORY, "cat");
    let mut a = attach(&srv, 80, 24);
    a.wait_until("both panes drawn", WAIT, |t| t.contains('│'));
    wait_until(|| srv.capture(&left).contains("200"), WAIT).expect("history");
    a.send(sgr(WHEEL_UP, 5, 5, false));
    a.send(sgr(WHEEL_UP, 5, 5, false));
    a.wait_until("copy mode on the left", WAIT, |t| {
        indicator(t).is_some_and(|i| i.starts_with("[5/"))
    });
    // The wheel enters without focusing (tmux's `copy-mode -e`); the
    // second tick is copy-mode-vi's `select-pane`, which focuses.
    assert_eq!(pane_var(&srv, &left, "#{pane_active}"), "1");
    srv.cmd(&["select-pane", "-t", &right])
        .expect("select right");
    let cursor = pane_var(&srv, &left, "#{copy_cursor_x},#{copy_cursor_y}");
    let drawn = a.output().len();

    a.send(sgr(0, 5, 5, false));
    a.send(sgr(0, 5, 5, true));
    wait_until(
        || pane_var(&srv, &left, "#{pane_active}") == "1",
        Duration::from_secs(5),
    )
    .unwrap_or_else(|e| panic!("the click did not focus {left}: {e}"));
    wait_until(|| a.output().len() > drawn, WAIT).expect("a frame after the click");
    a.wait_until("the mode kept", WAIT, |t| {
        indicator(t).is_some_and(|i| i.starts_with("[5/"))
    });
    assert_eq!(
        pane_var(&srv, &left, "#{copy_cursor_x},#{copy_cursor_y}"),
        cursor
    );
    assert_eq!(pane_var(&srv, &left, "#{selection_present}"), "0");
    assert!(!srv.capture(&left).contains("^["), "{}", srv.capture(&left));
}

/// Output arriving in copy mode moves neither the view nor its text (moved
/// from T3.2's `the_view_stays_on_its_lines_while_output_arrives`, where
/// the offset grew with the output): copy mode works on the copy taken on
/// entry, as tmux's does.
#[test]
fn output_does_not_move_copy_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let go = dir.path().join("go");
    let srv = start(&format!(
        "seq 1 200; stty -echo; while [ ! -e '{}' ]; do sleep 0.05; done; \
         echo one; echo two; echo three; exec cat",
        go.display()
    ));
    let mut a = attach(&srv, 80, 24);
    a.wait_until("179 on top", WAIT, |t| top_line(t).trim_end() == "179");
    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.wait_until("five lines up", WAIT, |t| {
        indicator(t).as_deref() == Some("[5/178]")
    });
    fs::write(&go, "").expect("start the output");
    wait_until(|| pane_var(&srv, "main", "#{history_size}") == "181", WAIT)
        .expect("the output arrived");
    // The view is the copy's: it neither scrolls nor grows.
    assert_eq!(
        pane_var(&srv, "main", "#{scroll_position}/#{copy_position_limit}"),
        "5/178"
    );
    let text = a.text();
    assert!(top_line(&text).starts_with("174"), "{text}");
    assert_eq!(indicator(&text).as_deref(), Some("[5/178]"), "{text}");
    assert!(!text.contains("three"), "{text}");
}

/// Keys in copy mode entered by the wheel (moved from the second half of
/// T3.2's `mouse_off_ignores_the_wheel_and_keys_leave_the_view`, where any
/// key closed the view): `q` leaves and is eaten; a key copy-mode-vi does
/// not bind is dropped and the mode stays, as in tmux.
#[test]
fn keys_in_copy_mode_from_the_wheel_follow_copy_mode_vi() {
    let srv = start(HISTORY);
    let mut a = attach(&srv, 80, 24);
    a.wait_until("179 on top", WAIT, |t| top_line(t).trim_end() == "179");
    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.wait_until("copy mode", WAIT, |t| indicator(t).is_some());
    a.send("x");
    a.send("q");
    a.wait_until("q left copy mode", WAIT, |t| indicator(t).is_none());
    a.send("a\r");
    a.wait_until("the a line", WAIT, |t| {
        t.lines().any(|l| l.trim_end() == "a")
    });
    let pane = srv.capture("main");
    assert!(
        !pane.lines().any(|l| l.contains('q') || l.contains('x')),
        "a key reached the pane:\n{pane}"
    );
}

/// With the status line on top, the wheel enters copy mode in the pane
/// under the pointer, drawn below the status row with its indicator on the
/// pane's first row (moved from TC.10's
/// `the_view_draws_below_a_status_line_on_top`).
#[test]
fn copy_mode_draws_below_a_status_line_on_top() {
    let srv = start("seq 1 200; exec cat");
    srv.cmd(&["set", "-g", "status-position", "top"])
        .expect("top");
    let mut a = attach(&srv, 80, 24);
    a.wait_until("200 shown", WAIT, |t| {
        t.lines().any(|l| l.trim_end() == "200")
    });
    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.wait_until("copy mode", WAIT, |t| {
        let second = t.lines().nth(1).unwrap_or_default();
        second.trim_end().ends_with(']') && second.contains("[5/")
    });
    let text = a.text();
    let first = text.lines().next().unwrap_or_default();
    assert!(first.contains("[main]"), "status row replaced: {first:?}");
}

// ---- Bindings wait for run-shell (TM.6 follow-up) ----------------------------

/// As tmux 3.7b's client queue (the `tm_6/run-shell` golden): a binding's
/// `run-shell` without `-b` (or with `-d`) holds its later commands and
/// the keys typed after it; with `-b` nothing waits.
#[test]
fn bindings_hold_for_run_shell_as_the_golden() {
    replay("run-shell");
}

fn opt(srv: &TestServer, name: &str) -> String {
    srv.cmd(&["show-options", "-gv", name])
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

/// A shell that waits for `flag` to exist.
fn until(flag: &std::path::Path) -> String {
    format!("while [ ! -e '{}' ]; do sleep 0.05; done", flag.display())
}

/// Text typed while a binding's `run-shell` runs reaches the pane only
/// after it, and an `after-*` hook's `run-shell` holds the binding's next
/// command and the client's keys the same way, the hook's commands first
/// (recorded: `HC`). Each shell waits on a file the test creates, so
/// nothing depends on timing.
#[test]
fn a_bindings_shell_and_its_hooks_shell_hold_the_client() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (go, go2) = (dir.path().join("go"), dir.path().join("go2"));
    let srv = start("stty -echo; exec cat");
    srv.cmd(&["set", "-g", "@log", ""]).expect("set");
    let f5 = format!(
        "set -ga @log A ; run-shell \"{}\" ; set -ga @log B",
        until(&go)
    );
    srv.cmd(&["bind-key", "-n", "F5", &f5]).expect("bind F5");
    let mut a = attach(&srv, 80, 24);
    a.send("\x1b[15~z\r");
    wait_until(|| opt(&srv, "@log") == "A", WAIT).expect("the binding started");
    // Whatever the client sent next is behind the shell: a round trip
    // through the server shows it has not run.
    srv.cmd(&["refresh-client"]).ok();
    assert_eq!(opt(&srv, "@log"), "A");
    assert!(
        !srv.capture("main").contains('z'),
        "{}",
        srv.capture("main")
    );
    fs::write(&go, "").expect("go");
    wait_until(|| opt(&srv, "@log") == "AB", WAIT).expect("the rest ran");
    wait_until(|| srv.capture("main").contains('z'), WAIT).expect("then the text");

    srv.cmd(&["set", "-g", "@log", ""]).expect("set");
    let hook = format!("run-shell \"{}\" ; set -ga @log H", until(&go2));
    srv.cmd(&["set-hook", "-g", "window-created", &hook])
        .expect("set-hook");
    srv.cmd(&["bind-key", "-n", "F6", "new-window -d ; set -ga @log C"])
        .expect("bind F6");
    a.send("\x1b[17~w\r");
    wait_until(|| pane_var(&srv, "main", "#{session_windows}") == "2", WAIT).expect("the window");
    srv.cmd(&["refresh-client"]).ok();
    assert_eq!(opt(&srv, "@log"), "");
    assert!(
        !srv.capture("main").contains('w'),
        "{}",
        srv.capture("main")
    );
    fs::write(&go2, "").expect("go2");
    wait_until(|| opt(&srv, "@log") == "HC", WAIT).expect("hook first, then C");
    wait_until(|| srv.capture("main").contains('w'), WAIT).expect("then the text");
}

/// While a binding's `run-shell` holds the client, a prompt (or menu, or
/// popup) its shell opens still takes the keys, as tmux hands an overlay's
/// keys over at once (tmux-sessionx's popup relies on it).
#[test]
fn a_prompt_a_held_binding_opens_still_takes_the_keys() {
    let srv = start("stty -echo; exec cat");
    let f9 = format!(
        "run-shell \"'{}' command-prompt -p go 'set -g @p %%'\" ; set -g @after yes",
        env!("CARGO_BIN_EXE_grofe")
    );
    srv.cmd(&["bind-key", "-n", "F9", &f9]).expect("bind F9");
    let mut a = attach(&srv, 80, 24);
    a.send("\x1b[20~");
    a.wait_until("the prompt", WAIT, |t| t.contains("go"));
    a.send("x\r");
    wait_until(|| opt(&srv, "@p") == "x", WAIT).expect("the prompt got the keys");
    wait_until(|| opt(&srv, "@after") == "yes", WAIT).expect("then the rest ran");
}

// ---- The CLI and copy mode's prompts -----------------------------------------

/// `send-keys -l three` into a pane in copy mode types `t`, whose row is
/// `command-prompt` (jump-to-forward): as tmux 3.7b's (recorded: it waits
/// too), the request waits for that prompt on the attached client, and
/// answers once it is answered, cancelled, or the client goes. With no
/// client it fails at once (`no current client`). The request always
/// gets its reply.
#[test]
fn send_keys_into_copy_mode_answers_once_its_prompt_ends() {
    let srv = start(HISTORY);
    srv.cmd(&["copy-mode", "-t", "main"]).expect("copy-mode");
    let out = srv.run(&["send-keys", "-t", "main", "-l", "t"]);
    assert_eq!(out.stderr, "no current client\n");
    assert_eq!(out.status.code(), Some(1));

    let mut a = attach(&srv, 80, 24);
    let spawn = |args: &[&str]| {
        let mut c = srv.command();
        c.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        c.spawn().expect("spawn")
    };
    let mut cli = spawn(&["send-keys", "-t", "main", "-l", "three"]);
    a.wait_until("the jump prompt", WAIT, |t| t.contains("(jump to forward)"));
    assert!(
        matches!(cli.try_wait(), Ok(None)),
        "answered before the prompt"
    );
    a.send("\x1b");
    let status = common::wait_child(cli, WAIT).expect("the reply after Escape");
    assert!(status.success());

    let cli = spawn(&["send-keys", "-t", "main", "-l", "t"]);
    a.wait_until("the prompt again", WAIT, |t| {
        t.contains("(jump to forward)")
    });
    drop(a);
    let status = common::wait_child(cli, WAIT).expect("the reply once the client went");
    assert!(status.success());
}
