//! TP.11: tmux-yank, unchanged, in a `plugin_rig` holding only it. Its
//! `yank.tmux` detects the clipboard command and binds `y`, `Y`, `M-y`,
//! `!` and `MouseDragEnd1Pane` in `copy-mode-vi` and `copy-mode` to
//! `send-keys -X copy-pipe-and-cancel <command>`; grofe adds nothing.
//!
//! - Case 1: in copy mode, `V` then `y` pipes the line to the stub
//!   clipboard command, and the client's outer pty gets the same text as
//!   OSC 52 (`set-clipboard external`, the default as in tmux, which the
//!   owner's config never changes; a copy goes out under `on` too).
//! - Case 2: a drag with the mouse ends on `MouseDragEnd1Pane`, which
//!   pipes the dragged text the same way.
//! - Case 3: `prefix ]` pastes that copy into a pane that asked for
//!   bracketed paste (mode 2004), wrapped in `ESC[200~`/`ESC[201~`.
//! - Case 4: under `set-clipboard off` the command still runs, and no
//!   OSC 52 reaches the client.
//! - yank's `Y`: its `tmux paste-buffer -p`, run by `copy-pipe-and-cancel`,
//!   puts the selection back into the pane, bracketed.
//! - Detection: which command the plugin binds for each set of clipboard
//!   tools on PATH (pbcopy; xclip, with the primary selection for the
//!   mouse; wl-copy; none, its error bindings).
//!
//! The rig's PATH is its own `bin` (the stubs) and `tools`, a directory of
//! links to the system's own tools without any clipboard command, so no
//! case can reach the host's real clipboard whatever the plugin detects.
//! Each stub records its argv and appends its stdin to `<root>/clip`.
//!
//! Each case matches a golden recorded from tmux 3.7b by the same code
//! (`record_tp11_goldens`, ignored: `GROFE_TP11_TMUX=<absolute path to tmux
//! 3.7b> cargo test --test acceptance record_tp11 -- --ignored`).

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use base64::Engine as _;

use crate::harness::Harness;
use crate::plugin_rig::{PREFIX, Rig, WAIT, read_golden, recording_tmux, wait_until, write_golden};

const TASK: &str = "tp11";
const GOLDEN: &str = "tp_11";

/// Every clipboard tool yank's `clipboard_copy_command` looks for, and the
/// paste halves: never linked into `tools`.
const CLIPBOARD: &[&str] = &[
    "pbcopy",
    "pbpaste",
    "reattach-to-user-namespace",
    "clip.exe",
    "wl-copy",
    "wl-paste",
    "xsel",
    "xclip",
    "putclip",
    "tmux",
];

/// A clipboard stub: stdin appended to `clip`, then one line of argv per
/// call. The line comes last, so a test that waits for it reads all of
/// `clip`.
const STUB: &str =
    "#!/bin/sh\ncat >>'@ROOT@/clip'\nprintf '%s\\n' \"@NAME@ $*\" >>'@ROOT@/calls'\n";

const TEXT: &str = "printf 'alpha line\\nbravo two\\n'; exec sleep 100000";

/// A rig loading tmux-yank with the clipboard stubs in `stubs` and no other
/// clipboard command reachable.
fn rig(real: Option<&Path>, stubs: &[&str]) -> Rig {
    let mut rig = Rig::new(TASK, real, &["tmux-yank"], vec![]);
    let tools = rig.path("tools");
    fs::create_dir_all(&tools).unwrap();
    let mut seen: HashSet<_> = CLIPBOARD.iter().map(|s| s.to_string()).collect();
    for dir in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if seen.insert(name.clone()) {
                symlink(e.path(), tools.join(&name)).unwrap();
            }
        }
    }
    let root = rig.root.display().to_string();
    for name in stubs {
        rig.install(name, &STUB.replace("@NAME@", name).replace("@ROOT@", &root));
    }
    let path = format!("{}:{}", rig.path("bin").display(), tools.display());
    rig.server_env(&[("PATH", path)]);
    rig
}

/// The rig started, `mode-keys vi` and `mouse on`, window `text` showing
/// two lines, and a client on it.
fn ready(rig: &Rig) -> Harness {
    rig.start("main");
    rig.t(&["set-option", "-g", "mode-keys", "vi"]);
    rig.t(&["set-option", "-g", "mouse", "on"]);
    rig.t(&["new-window", "-t", "main", "-n", "text", TEXT]);
    let shown = wait_until(
        || {
            rig.t(&["capture-pane", "-p", "-t", "main:text"])
                .contains("bravo two")
        },
        WAIT,
    );
    assert!(shown.is_ok(), "the pane never showed its text");
    let mut a = rig.attach("main");
    a.wait_for("bravo two");
    a
}

/// `#{pane_in_mode}` of the client's current pane.
fn in_mode(rig: &Rig) -> String {
    rig.t(&["display-message", "-p", "-t", "main", "#{pane_in_mode}"])
        .trim()
        .to_string()
}

fn wait_mode(rig: &Rig, want: &str) {
    let got = wait_until(|| in_mode(rig) == want, WAIT);
    assert!(got.is_ok(), "pane_in_mode never became {want}");
}

/// Copy mode in the current pane, its top line selected.
fn select_top_line(rig: &Rig, a: &mut Harness) {
    a.send(format!("{PREFIX}["));
    wait_mode(rig, "1");
    a.send("g");
    a.send("V");
}

/// Case 1's copy: the top line selected, `y`.
fn yank_line(rig: &Rig, a: &mut Harness) {
    select_top_line(rig, a);
    a.send("y");
    rig.wait_lines("calls", 1, "the clipboard command");
    wait_mode(rig, "0");
}

/// Case 2's copy: a left drag from the first cell of `alpha` to the
/// fifth of `bravo`, released there.
fn drag(rig: &Rig, a: &mut Harness) {
    let sgr = |b: u8, x: u16, y: u16, end: char| format!("\x1b[<{b};{x};{y}{end}");
    a.send(sgr(0, 1, 1, 'M'));
    a.send(sgr(32, 3, 1, 'M'));
    // The release waits for the drag's redraw, as a hand's release comes
    // in a read of its own. tmux 3.7b sends no OSC 52 for a copy made while
    // its pane still has a redraw pending (recorded: with the motion and
    // the release in one read, the copy is piped but no OSC 52 goes out).
    let n = a.output().len();
    a.send(sgr(32, 5, 2, 'M'));
    a.wait_for_output(b"bravo", n);
    a.send(sgr(0, 5, 2, 'm'));
    rig.wait_lines("calls", 1, "the clipboard command");
    wait_mode(rig, "0");
}

/// Something every client redraws after the copy: once it is on screen,
/// an OSC 52 the copy sent has reached the client too.
fn fence(rig: &Rig, a: &mut Harness) {
    rig.t(&["rename-window", "-t", "main:text", "fenced"]);
    a.wait_for("fenced");
}

/// Each OSC 52 in `out`, as `<param>;<data decoded>`, one per line.
fn osc52(out: &[u8]) -> String {
    let mut got = String::new();
    let mut rest = out;
    while let Some(i) = rest.windows(5).position(|w| w == b"\x1b]52;") {
        rest = &rest[i + 5..];
        let end = rest
            .iter()
            .position(|&c| c == 0x07 || c == 0x1b)
            .unwrap_or(rest.len());
        let body = String::from_utf8_lossy(&rest[..end]).to_string();
        let (param, data) = body.split_once(';').unwrap_or((&body, ""));
        let text = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map(|d| String::from_utf8_lossy(&d).to_string())
            .unwrap_or_else(|_| format!("<not base64: {data}>"));
        let term = if rest.get(end) == Some(&0x07) {
            "BEL"
        } else {
            "ST"
        };
        got.push_str(&format!("{param};{text:?} {term}\n"));
        rest = &rest[end..];
    }
    got
}

/// What a copy left: the stub's calls and stdin, and the OSC 52s the
/// client got. (Case 3 reads the buffer back, by pasting it.)
fn result(rig: &Rig, a: &mut Harness) -> String {
    fence(rig, a);
    let calls = fs::read_to_string(rig.path("calls")).unwrap_or_default();
    let clip = fs::read_to_string(rig.path("clip")).unwrap_or_default();
    format!(
        "--- calls\n{calls}--- clip\n{clip:?}\n--- osc52\n{}",
        osc52(a.output())
    )
}

/// The rows yank bound, one per line with single spaces, the rig's root
/// as `<root>`: `y`, `Y`, `M-y`, `!` and `MouseDragEnd1Pane` in both copy
/// tables, and prefix `y` and `Y`.
fn bindings(rig: &Rig) -> String {
    let root = rig.root.display().to_string();
    let mut out = String::new();
    for line in rig.t(&["list-keys"]).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let (Some(table), Some(key)) = (words.get(2), words.get(3)) else {
            continue;
        };
        let ours = match *table {
            // tmux's stock emacs table also has a bare `MouseDragEnd1Pane`
            // copy, which yank leaves alone when it finds no command;
            // grofe ships no emacs table (docs/copy-mode.md, out of scope),
            // only the rows yank binds there.
            "copy-mode" if line.ends_with(" copy-pipe-and-cancel") => false,
            "copy-mode" | "copy-mode-vi" => {
                ["y", "Y", "M-y", "!", "MouseDragEnd1Pane"].contains(key)
            }
            "prefix" => ["y", "Y"].contains(key),
            _ => false,
        };
        if ours {
            out.push_str(&words.join(" ").replace(&root, "<root>"));
            out.push('\n');
        }
    }
    out
}

fn case_yank(rig: &Rig) -> String {
    let mut a = ready(rig);
    yank_line(rig, &mut a);
    result(rig, &mut a)
}

fn case_drag(rig: &Rig) -> String {
    let mut a = ready(rig);
    drag(rig, &mut a);
    result(rig, &mut a)
}

/// Window `paste`, not selected: it shows `show`, asks for bracketed
/// paste, then writes `asked` and copies what it reads, raw, to `pasted`.
/// (grofe has no `#{bracket_paste_flag}` to wait on, so the pane says
/// when it has asked.)
fn paste_window(rig: &Rig, show: &str) {
    let cmd = format!(
        "printf '{show}'; stty raw -echo; printf '\\033[?2004h'; : >'{}'; exec cat >'{}'",
        rig.path("asked").display(),
        rig.path("pasted").display()
    );
    rig.t(&["new-window", "-d", "-t", "main", "-n", "paste", &cmd]);
    let ready = wait_until(|| rig.path("asked").exists(), WAIT);
    assert!(
        ready.is_ok(),
        "the paste pane never asked for bracketed paste"
    );
}

/// What `paste` read, once a bracketed paste has ended.
fn pasted(rig: &Rig) -> String {
    let read = || fs::read(rig.path("pasted")).unwrap_or_default();
    let done = wait_until(|| read().ends_with(b"\x1b[201~"), WAIT);
    assert!(
        done.is_ok(),
        "the paste never ended: {:?}",
        String::from_utf8_lossy(&read())
    );
    format!("{:?}\n", String::from_utf8_lossy(&read()))
}

/// Case 3: `y` copies in `text`, then `prefix ]` in `paste`.
fn case_paste(rig: &Rig) -> String {
    let mut a = ready(rig);
    paste_window(rig, "");
    yank_line(rig, &mut a);
    rig.t(&["select-window", "-t", "main:paste"]);
    a.wait_for("paste*");
    a.send(format!("{PREFIX}]"));
    pasted(rig)
}

/// yank's `Y`: copy mode in `paste`, its top line selected, `Y` pipes it
/// to `tmux paste-buffer -p`, which pastes it back into the pane, and the
/// clipboard command is never run.
fn case_put(rig: &Rig) -> String {
    let mut a = ready(rig);
    paste_window(rig, "charlie put\\n");
    rig.t(&["select-window", "-t", "main:paste"]);
    a.wait_for("charlie put");
    select_top_line(rig, &mut a);
    a.send("Y");
    let got = pasted(rig);
    wait_mode(rig, "0");
    assert!(!rig.path("calls").exists(), "Y ran the clipboard command");
    got
}

fn case_off(rig: &Rig) -> String {
    let mut a = ready(rig);
    rig.t(&["set-option", "-g", "set-clipboard", "off"]);
    yank_line(rig, &mut a);
    result(rig, &mut a)
}

/// What the plugin binds with `stubs` as the only clipboard tools.
fn case_detect(real: Option<&Path>, stubs: &[&str]) -> String {
    let rig = rig(real, stubs);
    rig.start("main");
    // The rig sees the plugin loaded at its first row: wait for its last.
    // With no command it binds `y`, `Y` and `M-y` in both tables, beside the
    // stock vi `MouseDragEnd1Pane` row.
    let rows = if stubs.is_empty() { 7 } else { 12 };
    let all = wait_until(|| bindings(&rig).lines().count() == rows, WAIT);
    assert!(all.is_ok(), "not every row bound:\n{}", bindings(&rig));
    bindings(&rig)
}

/// The detection goldens: name, stubs.
const DETECT: &[(&str, &[&str])] = &[
    ("detect-pbcopy", &["pbcopy", "xclip", "xsel", "wl-copy"]),
    ("detect-wl-copy", &["wl-copy", "xclip", "xsel"]),
    ("detect-xsel", &["xsel", "xclip"]),
    ("detect-xclip", &["xclip"]),
    ("detect-none", &[]),
];

const STUBS: &[&str] = &["pbcopy"];

#[test]
fn case_1_y_pipes_the_line_to_the_clipboard_command_and_sends_osc52() {
    let got = case_yank(&rig(None, STUBS));
    assert!(got.contains("--- calls\npbcopy \n"), "{got}");
    assert!(got.contains("--- osc52\n;\"alpha line"), "{got}");
    assert_eq!(got, read_golden(GOLDEN, "yank"));
}

#[test]
fn case_2_a_drag_end_pipes_the_dragged_text_the_same_way() {
    let got = case_drag(&rig(None, STUBS));
    assert!(got.contains("--- calls\npbcopy \n"), "{got}");
    assert_eq!(got, read_golden(GOLDEN, "drag"));
}

#[test]
fn case_3_prefix_bracket_pastes_the_copy_bracketed() {
    let got = case_paste(&rig(None, STUBS));
    assert!(got.starts_with("\"\\u{1b}[200~alpha line"), "{got}");
    assert_eq!(got, read_golden(GOLDEN, "paste"));
}

#[test]
fn y_upper_puts_the_selection_back_bracketed() {
    let got = case_put(&rig(None, STUBS));
    assert!(got.starts_with("\"\\u{1b}[200~charlie put"), "{got}");
    assert_eq!(got, read_golden(GOLDEN, "put"));
}

#[test]
fn case_4_set_clipboard_off_still_pipes_but_sends_no_osc52() {
    let got = case_off(&rig(None, STUBS));
    assert!(got.contains("--- calls\npbcopy \n"), "{got}");
    assert!(got.ends_with("--- osc52\n"), "{got}");
    assert_eq!(got, read_golden(GOLDEN, "off"));
}

#[test]
fn the_plugin_binds_the_command_it_detects() {
    for (name, stubs) in DETECT {
        assert_eq!(
            case_detect(None, stubs),
            read_golden(GOLDEN, name),
            "{name}"
        );
    }
}

/// The goldens, from real tmux 3.7b.
#[test]
#[ignore]
fn record_tp11_goldens() {
    let tmux = recording_tmux("GROFE_TP11_TMUX");
    let real = Some(tmux.as_path());
    write_golden(GOLDEN, "yank", &case_yank(&rig(real, STUBS)));
    write_golden(GOLDEN, "drag", &case_drag(&rig(real, STUBS)));
    write_golden(GOLDEN, "paste", &case_paste(&rig(real, STUBS)));
    write_golden(GOLDEN, "put", &case_put(&rig(real, STUBS)));
    write_golden(GOLDEN, "off", &case_off(&rig(real, STUBS)));
    for (name, stubs) in DETECT {
        write_golden(GOLDEN, name, &case_detect(real, stubs));
    }
}
