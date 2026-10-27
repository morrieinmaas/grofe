//! TM.1: copy mode's core against tmux 3.7b's goldens
//! (`tests/fixtures/compat/golden/tm_1/`), over the fixture in
//! `tests/fixtures/copy/` printed into an 80x24 pane with 500 lines of
//! history. Each golden's commands run natively here, in order, and each
//! one's stdout, stderr and exit status must equal what tmux printed. A
//! `client` step attaches a pty client (T2.4's harness) and a `keys` step
//! types on it, as the recorder does.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::path::PathBuf;

use common::{CMD_TIMEOUT, TestServer, wait_until};

use crate::harness::Harness;

const READOUT: &str = "#{copy_cursor_x},#{copy_cursor_y},#{scroll_position}";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compat")
}

fn golden_text(name: &str) -> String {
    fs::read_to_string(fixtures().join(format!("golden/tm_1/{name}.golden"))).unwrap()
}

/// The fixture as a pane command: 500 lines of history in 80x24.
fn fixture_cmd() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/copy/fixture.txt");
    format!("cat {}; exec sleep 100000", path.display())
}

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
            "Space" => out.push(b' '),
            "Escape" => out.push(0x1b),
            "Enter" => out.push(b'\r'),
            "Tab" => out.push(b'\t'),
            "BSpace" => out.push(0x7f),
            w => match w.strip_prefix("C-").map(str::as_bytes) {
                Some([c]) => out.push(c.to_ascii_lowercase() - b'a' + 1),
                _ => out.extend_from_slice(w.as_bytes()),
            },
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

/// Runs golden `name` on a fresh server, comparing each command with tmux.
fn replay(name: &str) {
    let srv = TestServer::start();
    let mut client: Option<Harness> = None;
    // Typed keys reach the server in order but after the CLI's commands
    // may: the first command after them is retried until it shows what
    // tmux's did, instead of sleeping.
    let mut typed = false;
    for step in load(name) {
        match step {
            Step::Run { line, args, want } => {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let run = || {
                    let out = srv.run(&args);
                    let mut stdout = out.stdout;
                    if !stdout.is_empty() && !stdout.ends_with('\n') {
                        stdout.push('\n');
                    }
                    (stdout, out.stderr, out.status.code().unwrap_or(-1))
                };
                if std::mem::take(&mut typed) {
                    let _ = wait_until(|| run() == want, CMD_TIMEOUT);
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
                typed = true;
            }
        }
    }
}

/// Entering and leaving: the starting cursor, `-u`, `-e`, a second
/// `copy-mode`, `send -X` and `-N` outside the mode with tmux's errors,
/// `#{pane_in_mode}` 1 only in the mode, `q` and `C-c`.
#[test]
fn entry_equals_the_golden() {
    replay("entry");
}

/// Vertical motions, scrolling and paging rows.
#[test]
fn vertical_motions_equal_the_golden() {
    replay("vertical");
}

/// Horizontal and word motion rows.
#[test]
fn horizontal_motions_equal_the_golden() {
    replay("horizontal");
}

/// Brackets, paragraphs, goto-line, the mark and toggle-position.
#[test]
fn jumps_equal_the_golden() {
    replay("jumps");
}

/// Output in copy mode, `refresh-from-pane` and `q`.
#[test]
fn output_equals_the_golden() {
    replay("output");
}

/// The navigator's rows, bound as canopy binds them.
#[test]
fn navigator_rows_equal_the_golden() {
    replay("navigator");
}

/// Tabs and colour: a tab steps whole; `#{copy_cursor_word}`; `-F`
/// expands only a search's string.
#[test]
fn tabs_equal_the_golden() {
    replay("tabs");
}

/// Through a pty client: `prefix [`, `prefix PPage`, the digits' count
/// prompt, `:`'s goto-line prompt.
#[test]
fn keys_through_a_client_equal_the_golden() {
    replay("counts");
}

/// OSC 133 marks (G1): `A` and `C` flag a line, nothing else does;
/// `next-prompt` and `previous-prompt`, with `-o`, jump between them, over
/// history and screen, with their argument errors and a selection.
#[test]
fn prompt_marks_and_jumps_equal_the_golden() {
    replay("prompts");
}

/// The same marks through resizes that reflow the lines and change the
/// height: a mark stays on a split line's first piece and is lost when the
/// line is joined into the one before.
#[test]
fn prompt_marks_through_resizes_equal_the_golden() {
    replay("prompts-resize");
}

/// The marks of lines that fall off a small `history-limit` go with them,
/// the others stay on their lines, also through a reflow that overflows
/// the limit.
#[test]
fn prompt_marks_follow_a_history_limit_as_the_golden() {
    replay("prompts-limit");
}

/// What clears a mark: whole-line clears, not partial ones; ED 2 moves the
/// used lines into history and ED 3 drops the history.
#[test]
fn what_clears_prompt_marks_equals_the_golden() {
    replay("prompts-clear");
}

fn readout(srv: &TestServer) -> String {
    srv.cmd(&["display-message", "-p", "-t", "main", READOUT])
        .unwrap()
        .trim()
        .to_string()
}

fn pane_var(srv: &TestServer, var: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", "main", var])
        .unwrap()
        .trim()
        .to_string()
}

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
        "24",
        &fixture_cmd(),
    ])
    .expect("new-session");
    wait_until(|| pane_var(&srv, "#{history_size}") == "500", CMD_TIMEOUT).expect("fixture");
    srv
}

/// Case 2: `prefix [`, then 50 lines arrive from a background process:
/// the readout and what the client shows are the same before and after.
/// Case 3: `q` goes back to the live pane, which shows the new lines.
#[test]
fn output_arriving_in_copy_mode_does_not_move_the_view() {
    let srv = start();
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 25);
    a.wait_for("last line of text");
    a.send(b"\x02[");
    a.wait_for("[0/500]");
    a.send(b"k");
    wait_until(|| readout(&srv) == "16,22,0", CMD_TIMEOUT).expect("k");
    let before = a.text();
    srv.cmd(&[
        "run-shell",
        "-t",
        "main",
        "seq 1 50 | sed 's/^/new /' > #{pane_tty}",
    ])
    .unwrap();
    wait_until(|| pane_var(&srv, "#{history_size}") == "550", CMD_TIMEOUT)
        .expect("the 50 lines arrive");
    assert_eq!(readout(&srv), "16,22,0");
    assert_eq!(a.text(), before, "the view moved");
    a.send(b"q");
    a.wait_for("new 50");
    assert_eq!(pane_var(&srv, "#{pane_in_mode}"), "0");
}

/// Case 6: `prefix PPage` enters a page up (`-u`), `prefix [` at the
/// bottom; both show the indicator in `mode-style`.
#[test]
fn prefix_ppage_enters_a_page_up() {
    let srv = start();
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 25);
    a.wait_for("last line of text");
    a.send(b"\x02\x1b[5~");
    a.wait_for("[22/500]");
    assert_eq!(pane_var(&srv, "#{pane_in_mode} #{scroll_position}"), "1 22");
    a.send(b"q");
    wait_until(|| pane_var(&srv, "#{pane_in_mode}") == "0", CMD_TIMEOUT).unwrap();
    a.send(b"\x02[");
    a.wait_for("[0/500]");
    assert_eq!(readout(&srv), "0,23,0");
}

/// The stock `#` and `*` rows (`send -FX search-backward --
/// "#{copy_cursor_word}"`) run in the mode; the word they expand is
/// `#{copy_cursor_word}`'s (TM.3 searches for it).
#[test]
fn hash_and_star_expand_the_cursor_word() {
    let srv = start();
    srv.cmd(&["copy-mode", "-t", "main"]).unwrap();
    srv.cmd(&["send-keys", "-t", "main", "-X", "-N", "5", "cursor-up"])
        .unwrap();
    srv.cmd(&["send-keys", "-t", "main", "-X", "start-of-line"])
        .unwrap();
    assert_eq!(pane_var(&srv, "#{copy_cursor_word}"), "CamelCaseWord");
    for key in ["#", "*"] {
        srv.cmd(&["send-keys", "-t", "main", key]).unwrap();
        assert_eq!(pane_var(&srv, "#{pane_in_mode}"), "1", "{key}");
    }
    srv.cmd(&[
        "send-keys",
        "-t",
        "main",
        "-F",
        "-X",
        "search-backward",
        "--",
        "#{copy_cursor_word}",
    ])
    .unwrap();
}

/// tmux frees a pane's modes when it is killed or respawned: so does
/// grofe, and a respawned pane starts outside copy mode.
#[test]
fn respawn_and_kill_leave_copy_mode() {
    let srv = start();
    srv.cmd(&["copy-mode", "-t", "main"]).unwrap();
    srv.cmd(&["respawn-pane", "-k", "-t", "main", "exec sleep 100000"])
        .unwrap();
    assert_eq!(pane_var(&srv, "#{pane_in_mode}"), "0");
    let err = srv
        .cmd(&["send-keys", "-t", "main", "-X", "cursor-up"])
        .unwrap_err();
    assert!(err.contains("not in a mode"), "{err}");
    srv.cmd(&["split-window", "-t", "main", "exec sleep 100000"])
        .unwrap();
    srv.cmd(&["copy-mode", "-t", "main"]).unwrap();
    srv.cmd(&["kill-pane", "-t", "main"]).unwrap();
    assert_eq!(pane_var(&srv, "#{pane_in_mode}"), "0");
}

/// A huge `-N` outside copy mode is capped at `MAX_REPEAT` (or stops at the
/// pane's full input queue) and the server keeps answering (tmux 3.7b stops
/// answering, recorded).
#[test]
fn a_huge_count_does_not_take_the_server_down() {
    let srv = start();
    let out = srv.run(&["send-keys", "-t", "main", "-N", "4294967295", "x"]);
    assert!(
        out.stderr.contains("queue is full") || out.status.success(),
        "{out:?}"
    );
    assert_eq!(pane_var(&srv, "#{pane_in_mode}"), "0");
    srv.cmd(&["copy-mode", "-t", "main"]).unwrap();
    srv.cmd(&["send-keys", "-t", "main", "-N", "4294967295", "k"])
        .unwrap();
    // The echoed x's grew the history; every k ran up to its top.
    let got = pane_var(
        &srv,
        "#{copy_cursor_y} #{scroll_position} #{copy_position_limit}",
    );
    let words: Vec<&str> = got.split(' ').collect();
    assert_eq!(words[0], "0", "{got}");
    assert_eq!(words[1], words[2], "{got}");
}

/// A count of keys that are not only motions runs every repeat, even when
/// the cursor stays put: three copies make three buffers.
#[test]
fn a_count_of_copies_runs_each_one() {
    let srv = start();
    srv.cmd(&[
        "bind-key",
        "-T",
        "copy-mode-vi",
        "y",
        "send-keys",
        "-X",
        "copy-selection-no-clear",
    ])
    .unwrap();
    srv.cmd(&["copy-mode", "-t", "main"]).unwrap();
    srv.cmd(&["send-keys", "-t", "main", "-X", "begin-selection"])
        .unwrap();
    srv.cmd(&["send-keys", "-t", "main", "-N", "3", "y"])
        .unwrap();
    let names = srv.cmd(&["list-buffers", "-F", "#{buffer_name}"]).unwrap();
    assert_eq!(names.lines().count(), 3, "{names}");
}
