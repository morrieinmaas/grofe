//! TP.8: tmux-fzf-url, unchanged, in a `plugin_rig` holding only it, with
//! `@fzf-url-history-limit 2000` as the owner's config sets it. prefix u
//! captures the pane (`capture-pane -J -p -e -S -2000`), extracts its URLs
//! with the plugin's own patterns through its pinned `xre` (0.1.1, the
//! release canopy provisions), and offers them in fzf's tmux popup.
//!
//! - Case 1: a pane showing three URLs (http, https, one with a query
//!   string): the picker lists exactly those three.
//! - Case 2: choosing one runs the plugin's default open command, a stub
//!   `xdg-open` and `open` first on the rig's PATH, with that URL as its
//!   one argument.
//! - Case 3: in a pane with more history than the limit, only the URLs in
//!   the last 2000 lines of history and the screen are listed.
//!
//! Each case matches a golden recorded from tmux 3.7b by the same code
//! (`record_tp8_goldens`, ignored: `GROFE_TP8_TMUX=<absolute path to tmux
//! 3.7b> cargo test --test acceptance record_tp8 -- --ignored`).
//!
//! The plugin sends its open command's output to `/tmp/tmux-<uid>-fzf-url.log`,
//! a fixed path the owner's own tmux-fzf-url writes too, so every case
//! runs only with `GROFE_TEST_PRIVATE_TMP=1` (a container), skips loudly
//! elsewhere, and fails without it under `CI`.
//!
//! `xre` is not vendored, and fetching it means the network, so the test
//! needs one at `$GROFE_TEST_XRE` or on PATH that reports 0.1.1: without
//! one it skips, and under `CI` it fails. Any other version would make the
//! plugin download its own; a `curl` stub on the rig's PATH fails that.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::harness::{Harness, require_tool};
use crate::plugin_rig::{
    PREFIX, Rig, WAIT, host_tool, private_tmp, read_golden, recording_tmux, wait_until,
    write_golden,
};

const TASK: &str = "tp8";
/// Where the plugin sends its open command's output, a fixed path the
/// owner's own tmux-fzf-url writes too.
const FZF_URL_LOG: &str = "/tmp/tmux-<uid>-fzf-url.log";
const GOLDEN: &str = "tp_8";

/// Records its argv, one line for the count and one per argument.
const OPEN_STUB: &str = "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$@\" >>'<root>/opened.log'\n";
const CURL_STUB: &str = "#!/bin/sh\necho \"curl $*\" >>'<root>/curl.log'\nexit 1\n";

/// The pinned xre, if this machine has one.
fn xre() -> Option<PathBuf> {
    let path = std::env::var_os("GROFE_TEST_XRE")
        .map(PathBuf::from)
        .or_else(|| host_tool("xre"))?;
    let out = Command::new(&path).arg("--version").output().ok()?;
    (String::from_utf8_lossy(&out.stdout).trim() == "xre 0.1.1").then_some(path)
}

/// Whether the tools the plugin needs are here; skips (or, under CI,
/// fails) when not.
fn tools() -> Option<PathBuf> {
    if !private_tmp(FZF_URL_LOG) || !require_tool("fzf") {
        return None;
    }
    match xre() {
        Some(x) => Some(x),
        None if std::env::var_os("CI").is_some() => {
            panic!("no xre 0.1.1 at $GROFE_TEST_XRE or on PATH (required under CI)")
        }
        None => {
            eprintln!("skipping: no xre 0.1.1 at $GROFE_TEST_XRE or on PATH");
            None
        }
    }
}

fn rig(real: Option<&Path>, xre: &Path) -> Rig {
    let rig = Rig::new(
        TASK,
        real,
        &["tmux-fzf-url"],
        vec![("@fzf-url-history-limit".into(), "2000".into())],
    );
    let bin = rig.store().join("plugins/tmux-fzf-url/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::copy(xre, bin.join("xre")).unwrap();
    let root = rig.root.display().to_string();
    for name in ["xdg-open", "open"] {
        rig.install(name, &OPEN_STUB.replace("<root>", &root));
    }
    rig.install("curl", &CURL_STUB.replace("<root>", &root));
    rig
}

/// A fresh window in session `main` showing `text`, its history up to
/// 5000 lines, so the plugin's limit and not the pane's is what cuts.
fn show(rig: &Rig, text: &str) {
    fs::write(rig.path("home/text"), text).unwrap();
    rig.t(&["set-option", "-g", "history-limit", "5000"]);
    rig.t(&["new-window", "-t", "main", "-n", "urls"]);
    rig.t(&[
        "send-keys",
        "-t",
        "main:urls",
        "clear; cat text; echo done-$((1))",
        "Enter",
    ]);
    let shown = wait_until(
        || {
            rig.t(&["capture-pane", "-p", "-t", "main:urls"])
                .lines()
                .any(|l| l == "done-1")
        },
        WAIT,
    );
    assert!(shown.is_ok(), "the pane never showed its text");
}

/// The picker's rows, as `<n> <url>`, once it lists `last`: its rows are
/// `nl`'s, a number and two spaces before each URL, where the pane's own
/// lines have one or none.
fn listed(a: &mut Harness, last: &str) -> String {
    a.wait_for(&format!("  {last}"));
    let mut out = String::new();
    for line in a.text().lines() {
        let words: Vec<&str> = line
            .split_whitespace()
            .filter(|w| w.chars().all(|c| c.is_ascii_digit()) || w.contains("://"))
            .collect();
        if let [n, url] = words[..]
            && url.contains("://")
        {
            out.push_str(&format!("{n} {url}\n"));
        }
    }
    out
}

const THREE: &str = "see http://plain.example/a here\n\
                     and https://secure.example/b too\n\
                     q https://query.example/c?x=1&y=2\n";

/// Cases 1 and 2: the three URLs listed, then the query one chosen.
fn pick_one(rig: &Rig) -> String {
    rig.start("main");
    show(rig, THREE);
    let mut a = rig.attach("main");
    a.send(format!("{PREFIX}u"));
    let list = listed(&mut a, "https://query.example/c?x=1&y=2");
    a.send("query");
    a.wait_for("> query");
    a.send("\r");
    rig.wait_lines("opened.log", 2, "the open command");
    let opened = fs::read_to_string(rig.path("opened.log")).unwrap();
    assert!(
        !rig.path("curl.log").exists(),
        "the plugin tried to fetch xre"
    );
    format!("--- listed\n{list}--- opened\n{opened}")
}

/// Case 3: a URL 2500 lines back, one 1500 back and one on screen.
fn history_limit(rig: &Rig) -> String {
    rig.start("main");
    let mut text = String::from("https://old.example/too-far\n");
    for n in 0..1000 {
        text.push_str(&format!("filler {n}\n"));
    }
    text.push_str("https://mid.example/within\n");
    for n in 0..1500 {
        text.push_str(&format!("filler {n}\n"));
    }
    text.push_str("https://new.example/on-screen\n");
    show(rig, &text);
    let mut a = rig.attach("main");
    a.send(format!("{PREFIX}u"));
    let list = listed(&mut a, "https://new.example/on-screen");
    a.send("\x1b");
    assert!(
        !rig.path("curl.log").exists(),
        "the plugin tried to fetch xre"
    );
    list
}

#[test]
fn case_1_and_2_lists_three_urls_and_opens_the_chosen_one() {
    let Some(xre) = tools() else { return };
    let got = pick_one(&rig(None, &xre));
    assert!(
        got.contains("--- opened\n1\nhttps://query.example/c?x=1&y=2\n"),
        "{got}"
    );
    assert_eq!(got, read_golden(GOLDEN, "pick-one"));
}

#[test]
fn case_3_scans_only_the_history_limit() {
    let Some(xre) = tools() else { return };
    let got = history_limit(&rig(None, &xre));
    assert!(!got.contains("old.example"), "{got}");
    assert_eq!(got, read_golden(GOLDEN, "history-limit"));
}

/// Records the goldens from the real tmux 3.7b at `$GROFE_TP8_TMUX`, each
/// case on its own private socket and directories.
#[test]
#[ignore]
fn record_tp8_goldens() {
    let tmux = recording_tmux("GROFE_TP8_TMUX");
    assert!(
        private_tmp(FZF_URL_LOG),
        "record only where /tmp is private"
    );
    let xre = xre().expect("an xre 0.1.1 at $GROFE_TEST_XRE or on PATH");
    write_golden(GOLDEN, "pick-one", &pick_one(&rig(Some(&tmux), &xre)));
    write_golden(
        GOLDEN,
        "history-limit",
        &history_limit(&rig(Some(&tmux), &xre)),
    );
}
