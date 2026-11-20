//! Memory budget for the server (plan 2026-11-19-footprint, D7). Ignored by
//! default, since each case builds a 46 MB file and drives a real server;
//! CI runs them with `cargo test --release --test footprint -- --ignored`.
//!
//! Footprint is macOS `phys_footprint` (`vmmap --summary`) or Linux `RssAnon`
//! of the server process, in MB. Budgets are checked, never loosened to pass.

mod common;

use std::fs;
use std::io::Write;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::time::Duration;

use common::TestServer;

const IDLE_PANES: usize = 21;
const WAIT: Duration = Duration::from_secs(120);

fn server_pid(srv: &TestServer) -> u32 {
    srv.cmd(&["display-message", "-p", "#{pid}"])
        .expect("display-message #{pid}")
        .trim()
        .parse()
        .expect("server pid")
}

/// The server's physical footprint in MB.
#[cfg(target_os = "macos")]
fn footprint_mb(pid: u32) -> f64 {
    let out = Command::new("vmmap")
        .args(["--summary", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .expect("run vmmap");
    let text = String::from_utf8_lossy(&out.stdout);
    let value = text
        .lines()
        .find_map(|l| l.strip_prefix("Physical footprint:"))
        .expect("vmmap prints a Physical footprint line")
        .trim();
    let (num, unit) = value.split_at(value.len() - 1);
    let num: f64 = num.parse().expect("footprint number");
    match unit {
        "K" => num / 1024.0,
        "M" => num,
        "G" => num * 1024.0,
        "B" => num / (1024.0 * 1024.0),
        other => panic!("unknown vmmap unit {other:?} in {value:?}"),
    }
}

/// The server's `RssAnon` in MB.
#[cfg(target_os = "linux")]
fn footprint_mb(pid: u32) -> f64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("read /proc status");
    let kb: f64 = status
        .lines()
        .find_map(|l| l.strip_prefix("RssAnon:"))
        .expect("RssAnon line")
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .expect("RssAnon number");
    kb / 1024.0
}

/// A ~46 MB file of 400,000 coloured lines, as the footprint script writes.
fn write_big_file(path: &std::path::Path) {
    let mut out = std::io::BufWriter::new(fs::File::create(path).expect("create big file"));
    let pad = "x".repeat(100);
    for n in 1..=400_000u32 {
        writeln!(out, "\x1b[3{}m line {n} {pad}\x1b[0m", n % 8).expect("write big file");
    }
    out.flush().expect("flush big file");
}

/// A server whose one pane `cat`s the big file, run to its end.
fn footprint_after_cat(limit: Option<u32>) -> f64 {
    let srv = TestServer::start();
    if let Some(limit) = limit {
        srv.cmd(&["set", "-g", "history-limit", &limit.to_string()])
            .expect("set history-limit");
    }
    let big = srv.home.join("big.txt");
    write_big_file(&big);
    let script = format!("cat '{}'; echo FOOTPRINT-DONE; sleep 600", big.display());
    srv.cmd(&[
        "new-session",
        "-d",
        "-x",
        "80",
        "-y",
        "24",
        "-s",
        "fp",
        &script,
    ])
    .expect("new-session");
    srv.wait_for(|| srv.capture("fp").contains("FOOTPRINT-DONE"), WAIT)
        .expect("cat finished");
    std::thread::sleep(Duration::from_secs(2));
    footprint_mb(server_pid(&srv))
}

fn assert_under(what: &str, mb: f64, budget: f64) {
    eprintln!("{what}: {mb:.1} MB (budget {budget} MB)");
    assert!(
        mb <= budget,
        "{what}: {mb:.1} MB is over the {budget} MB budget"
    );
}

#[test]
#[ignore = "memory budget; run in CI with --ignored"]
fn idle_21_panes_under_10mb() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-x", "80", "-y", "24", "-s", "fp"])
        .expect("new-session");
    for _ in 1..IDLE_PANES {
        srv.cmd(&["new-window", "-d", "-t", "fp"])
            .expect("new-window");
    }
    std::thread::sleep(Duration::from_secs(2));
    assert_under("21 idle panes", footprint_mb(server_pid(&srv)), 10.0);
}

#[test]
#[ignore = "memory budget; run in CI with --ignored"]
fn after_46mb_default_under_20mb() {
    assert_under(
        "after 46 MB, default limit",
        footprint_after_cat(None),
        20.0,
    );
}

#[test]
#[ignore = "memory budget; run in CI with --ignored"]
fn after_46mb_limit_50000_under_60mb() {
    assert_under(
        "after 46 MB, history-limit 50000",
        footprint_after_cat(Some(50_000)),
        60.0,
    );
}
