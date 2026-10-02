//! TM.7: copy mode's parity suite (plan M5). One scenario per row of
//! `docs/copy-mode.md`'s keymap (TM.0's 95 bindings), each in
//! `golden/tm_7/copy_mode/`, plus the front end's own rows in
//! `golden/tm_7/` (`forms`, `buffers`, `options`, `pane-path`). Each was
//! recorded from tmux 3.7b in TC.0b's container and is replayed here on
//! grofe through the `tmux` symlink, byte for byte after normalization.
//!
//! Every scenario starts from the same fixture (TM.1's 500 lines of
//! history in 80x24) with `copy-mode.conf` sourced, so tmux-yank's and the
//! navigator's rows are the ones under test, sends the row's key (with
//! `send-keys` to the pane, or typed on an attached client for the
//! prompts and the mouse) and reads back the cursor or the buffer.
//!
//! Each run also checks that the server left nothing in
//! `compat-miss.log`, that no real tmux ran, and that the user's real tmux
//! and grofe socket and state directories and `~/.config/canopy` are as
//! they were.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::fe_1::grofe_cli;
use crate::fe_3::exact_pids;
use crate::fe_4::with_client_ttys;
use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{Cli, parse_scenario, replay};
use crate::scenarios::{socket_dirs, tripwire};
use crate::tp::misses;

/// The clipboard tools a copy row runs. On the grofe side each is a stub
/// earlier on `PATH` that keeps what it read, so no scenario reaches the
/// machine's real clipboard; the container has none of them.
const CLIPBOARD_TOOLS: &[&str] = &["pbcopy", "pbpaste", "wl-copy", "xclip", "xsel"];

/// grofe as `tmux` ([`grofe_cli`]) with the clipboard tools stubbed and
/// the attach check FE.4's.
fn copy_cli(dir: &Path) -> Cli {
    let cli = grofe_cli(dir);
    let bin = dir.join("bin");
    for tool in CLIPBOARD_TOOLS {
        let path = bin.join(tool);
        fs::write(&path, format!("#!/bin/sh\ncat >> \"$HOME/{tool}.out\"\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    with_client_ttys(cli, dir)
}

/// The names, sizes and times under `~/.config/canopy`, which no scenario
/// may change.
fn canopy_config() -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return String::new();
    };
    let mut out = Vec::new();
    let mut dirs = vec![PathBuf::from(home).join(".config/canopy")];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = fs::symlink_metadata(&path) {
                if meta.is_dir() {
                    dirs.push(path.clone());
                }
                out.push(format!(
                    "{} {} {}",
                    path.display(),
                    meta.len(),
                    meta.mtime()
                ));
            }
        }
    }
    out.sort();
    out.join("\n")
}

/// The scenario at `golden/<rel>.scenario` on grofe: its normalized
/// transcript, or why the run itself failed.
fn run(rel: &str) -> Result<String, String> {
    let scenario = fixtures_dir().join(format!("golden/{rel}.scenario"));
    let text = fs::read_to_string(&scenario).map_err(|e| format!("{rel}: {e}"))?;
    let steps = parse_scenario(&text).map_err(|e| format!("{rel}: {e}"))?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let cli = copy_cli(dir.path());
    let host = (socket_dirs(), canopy_config());
    let got = exact_pids(replay(&cli, &steps)).normalized(&Normalizer::load());
    tripwire(&cli)?;
    let missed = misses(dir.path());
    if !missed.is_empty() {
        return Err(format!("{rel}: compat-miss.log is not empty: {missed:#?}"));
    }
    if host != (socket_dirs(), canopy_config()) {
        return Err(format!(
            "{rel}: the real tmux or grofe directories or ~/.config/canopy changed"
        ));
    }
    Ok(got)
}

/// `got` against the golden text `want`, with the first differing line.
fn compare(rel: &str, want: &str, got: &str) -> Result<(), String> {
    if want == got {
        return Ok(());
    }
    let at = want
        .lines()
        .zip(got.lines())
        .position(|(w, g)| w != g)
        .unwrap_or_else(|| want.lines().count().min(got.lines().count()));
    Err(format!(
        "{rel} differs from its golden at line {}:\n--- tmux 3.7b\n{want}+++ grofe\n{got}",
        at + 1
    ))
}

fn golden(rel: &str) -> String {
    fs::read_to_string(fixtures_dir().join(format!("golden/{rel}.golden")))
        .unwrap_or_else(|e| panic!("{rel}: no golden ({e}); record it"))
}

/// Replays `golden/<rel>.scenario` on grofe and compares with its golden.
fn check(rel: &str) {
    let got = run(rel).unwrap_or_else(|e| panic!("{e}"));
    if let Err(e) = compare(rel, &golden(rel), &got) {
        panic!("{e}");
    }
}

macro_rules! rows {
    ($($name:ident => $file:literal),* $(,)?) => {
        /// Every keymap row's scenario, by file name.
        const ROWS: &[&str] = &[$($file),*];
        $(
            #[test]
            fn $name() {
                check(concat!("tm_7/copy_mode/", $file));
            }
        )*
    };
}

rows! {
    enter => "enter", escape => "escape", space => "space", bang => "bang",
    hash => "hash", dollar => "dollar", percent => "percent", star => "star",
    comma => "comma", slash => "slash", digit_0 => "digit-0",
    digit_1 => "digit-1", digit_2 => "digit-2", digit_3 => "digit-3",
    digit_4 => "digit-4", digit_5 => "digit-5", digit_6 => "digit-6",
    digit_7 => "digit-7", digit_8 => "digit-8", digit_9 => "digit-9",
    colon => "colon", semicolon => "semicolon", question => "question",
    upper_a => "upper-a", upper_b => "upper-b", upper_d => "upper-d",
    upper_e => "upper-e", upper_f => "upper-f", upper_g => "upper-g",
    upper_h => "upper-h", upper_j => "upper-j", upper_k => "upper-k",
    upper_l => "upper-l", upper_m => "upper-m", upper_n => "upper-n",
    upper_p => "upper-p", upper_t => "upper-t", upper_v => "upper-v",
    upper_w => "upper-w", upper_x => "upper-x", upper_y => "upper-y",
    caret => "caret", b => "b", e => "e", f => "f", g => "g", h => "h",
    j => "j", k => "k", l => "l", n => "n", o => "o", q => "q", r => "r",
    t => "t", v => "v", w => "w", y => "y", z => "z", lbrace => "lbrace",
    rbrace => "rbrace", bspace => "bspace", home => "home", end => "end",
    npage => "npage", ppage => "ppage", up => "up", down => "down",
    left => "left", right => "right", mousedown1pane => "mousedown1pane",
    mousedrag1pane => "mousedrag1pane",
    mousedragend1pane => "mousedragend1pane",
    wheeldownpane => "wheeldownpane", wheeluppane => "wheeluppane",
    doubleclick1pane => "doubleclick1pane",
    tripleclick1pane => "tripleclick1pane", m_x => "m-x", m_y => "m-y",
    c_lbracket => "c-lbracket", c_backslash => "c-backslash", c_b => "c-b",
    c_c => "c-c", c_d => "c-d", c_e => "c-e", c_f => "c-f", c_h => "c-h",
    c_j => "c-j", c_k => "c-k", c_l => "c-l", c_u => "c-u", c_v => "c-v",
    c_y => "c-y", c_up => "c-up", c_down => "c-down",
}

/// The keymap's scenario ids, in `docs/copy-mode.md`'s order.
fn keymap_ids() -> Vec<String> {
    let doc = fs::read_to_string(fixtures_dir().join("../../../docs/copy-mode.md")).unwrap();
    doc.lines()
        .filter(|l| l.starts_with("| `"))
        .filter_map(|l| l.rsplit('|').nth(1))
        .filter_map(|cell| cell.trim().strip_prefix("`copy_mode/"))
        .map(|id| id.trim_end_matches('`').to_string())
        .collect()
}

/// Every keymap row has its scenario, golden and test, and nothing else is
/// in `copy_mode/`: the 95 rows of TM.0's golden.
#[test]
fn every_keymap_row_has_one_scenario() {
    let mut ids = keymap_ids();
    assert_eq!(ids.len(), 95, "{ids:?}");
    ids.sort();
    let mut rows: Vec<String> = ROWS.iter().map(|r| r.to_string()).collect();
    rows.sort();
    assert_eq!(ids, rows, "the tests and the keymap disagree");
    let dir = fixtures_dir().join("golden/tm_7/copy_mode");
    let mut files: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".scenario").map(str::to_string)
        })
        .collect();
    files.sort();
    assert_eq!(files, rows, "scenario files and the keymap disagree");
    for id in &rows {
        assert!(
            dir.join(format!("{id}.golden")).exists(),
            "{id} has no golden; record it"
        );
    }
}

/// Case 2: the live `copy-mode-vi` table, with copy-mode.conf loaded
/// through the `tmux` symlink, is TM.0's golden exactly.
#[test]
fn list_keys_copy_mode_vi_equals_tm_0_s_golden() {
    check("tm_0/list-keys-copy-mode-vi");
}

/// The front end's own rows: `copy-mode`'s flags and `send -X`, `-FX`
/// and `-N` with tmux's errors outside the mode.
#[test]
fn forms_match_their_golden() {
    check("tm_7/forms");
}

/// The buffer verbs and their default output, through the front end.
#[test]
fn buffers_match_their_golden() {
    check("tm_7/buffers");
}

/// The copy options by tmux name: `mode-keys`, `set-clipboard`,
/// `copy-command`, `word-separators`, `wrap-search`, `buffer-limit`.
#[test]
fn options_match_their_golden() {
    check("tm_7/options");
}

/// `#{pane_path}` is the text an OSC 7 sent, as tmux shows it.
#[test]
fn pane_path_matches_its_golden() {
    check("tm_7/pane-path");
}

/// Case 3: one byte changed in one golden fails that scenario and no
/// other.
#[test]
fn an_edited_golden_fails_only_its_scenario() {
    let names = [
        "tm_7/copy_mode/g",
        "tm_7/copy_mode/q",
        "tm_7/copy_mode/dollar",
    ];
    let edited = names[1];
    let mut failed = Vec::new();
    for rel in names {
        let mut want = golden(rel);
        if rel == edited {
            let at = want.rfind("? 0").expect("an exit status");
            want.replace_range(at..at + 3, "? 1");
        }
        let got = run(rel).unwrap_or_else(|e| panic!("{e}"));
        if compare(rel, &want, &got).is_err() {
            failed.push(rel);
        }
    }
    assert_eq!(failed, [edited]);
}
