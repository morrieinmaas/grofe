//! Status sources (TC.4, spec D8, §4.10): `[status.sources.<name>]` runs a
//! command on a schedule, off the redraw path, and keeps its last non-empty
//! output line in `@<name>`, as tmux 3.7b keeps a `#()` job's.
//!
//! Timing is checked against deadlines and baselines rather than fixed
//! sleeps: a run count is taken over a window that starts at the first
//! run, and frame gaps are compared with the same machine's gaps measured
//! just before, so a loaded machine does not fail a correct server.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);
/// A forced frame every 1/60 s.
const FRAME: Duration = Duration::from_micros(16_667);
/// The longest frame interval the acceptance text allows.
const FRAME_LIMIT: Duration = Duration::from_millis(50);

fn write_config(srv: &TestServer, text: &str) {
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("config.toml"), text).expect("write config");
}

/// Writes `config` and loads it with `reload`.
fn load(srv: &TestServer, config: &str) {
    write_config(srv, config);
    ok(srv, &["reload"]);
}

/// A server started with `config`, and one detached session `s` running
/// `cat` (so typed input echoes and forces frames).
fn start(config: &str) -> TestServer {
    let srv = TestServer::start();
    load(&srv, config);
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "cat",
        ],
    );
    srv
}

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args)
        .unwrap_or_else(|e| panic!("{args:?} failed: {e}"))
}

fn display(srv: &TestServer, fmt: &str) -> String {
    ok(srv, &["display-message", "-p", fmt])
        .trim_end_matches('\n')
        .to_string()
}

fn lines(path: &Path) -> usize {
    fs::read_to_string(path).map_or(0, |t| t.lines().count())
}

fn server_pid(srv: &TestServer) -> i32 {
    display(srv, "#{pid}").parse().expect("server pid")
}

fn attach(srv: &TestServer) -> Harness {
    let mut h = Harness::spawn_attach_with_env(srv.command(), "s", &[], 80, 24);
    h.wait_until("the status row", WAIT, |text| {
        text.lines().last().unwrap_or_default().starts_with("[s]")
    });
    h
}

/// Types into `a` at 60 Hz and reads both clients, until `done` holds
/// (checked about every 20 ms) or `max` passes. Returns the longest gap
/// between two reads that brought a client output, over both clients, and
/// how many such reads there were.
fn force_frames(
    a: &mut Harness,
    b: &mut Harness,
    max: Duration,
    mut done: impl FnMut() -> bool,
) -> (Duration, usize) {
    let start = Instant::now();
    let mut next_key = start;
    let mut next_check = start;
    let mut typed = 0usize;
    let mut seen = [a.output().len(), b.output().len()];
    let mut last = [start, start];
    let mut longest = Duration::ZERO;
    let mut frames = 0;
    loop {
        let now = Instant::now();
        if now >= next_key {
            // A line break now and then keeps `cat`'s line short.
            a.send(if typed % 40 == 39 { "\r" } else { "x" });
            typed += 1;
            next_key += FRAME;
        }
        for (i, h) in [&mut *a, &mut *b].into_iter().enumerate() {
            let len = h.output().len();
            if len > seen[i] {
                seen[i] = len;
                longest = longest.max(now - last[i]);
                last[i] = now;
                frames += 1;
            }
        }
        if now >= next_check {
            next_check = now + Duration::from_millis(20);
            if done() {
                break;
            }
        }
        if now - start >= max {
            break;
        }
        sleep(Duration::from_millis(1));
    }
    let end = Instant::now();
    for at in last {
        longest = longest.max(end - at);
    }
    (longest, frames)
}

/// The frame gap to hold a run to: the acceptance limit, or on a machine
/// whose own baseline is already near it, half as much again as that.
fn frame_bound(baseline: Duration) -> Duration {
    FRAME_LIMIT.max(baseline + baseline / 2)
}

/// Whether any process in group `pgid` is alive (a zombie is not).
#[cfg(target_os = "linux")]
fn group_alive(pgid: i32) -> bool {
    let Ok(dir) = fs::read_dir("/proc") else {
        return false;
    };
    dir.filter_map(|e| e.ok())
        .filter_map(|e| fs::read_to_string(e.path().join("stat")).ok())
        .any(|stat| {
            let Some((_, rest)) = stat.rsplit_once(')') else {
                return false;
            };
            let f: Vec<&str> = rest.split_whitespace().collect();
            // state ppid pgrp ...
            f.len() > 2 && f[0] != "Z" && f[2] == pgid.to_string()
        })
}

#[cfg(not(target_os = "linux"))]
fn group_alive(pgid: i32) -> bool {
    nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None).is_ok()
}

/// The zombie children of `pid`.
fn zombies_of(pid: i32) -> usize {
    let out = Command::new("ps")
        .args(["-A", "-o", "ppid=,stat="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| {
            let mut f = l.split_whitespace();
            f.next() == Some(&pid.to_string()) && f.next().is_some_and(|s| s.starts_with('Z'))
        })
        .count()
}

/// How many file descriptors `pid` has open.
fn open_fds(pid: i32) -> usize {
    if let Ok(dir) = fs::read_dir(format!("/proc/{pid}/fd")) {
        return dir.count();
    }
    let out = Command::new("lsof")
        .args(["-n", "-P", "-p", &pid.to_string()])
        .output()
        .expect("lsof");
    // Skip the header; count only numbered descriptors (not cwd, txt...).
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter(|l| {
            l.split_whitespace()
                .nth(3)
                .is_some_and(|fd| fd.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count()
}

/// Resident memory of `pid`, in KiB.
fn rss_kib(pid: i32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("rss")
}

fn path(srv: &TestServer, name: &str) -> PathBuf {
    srv.home.join(name)
}

/// Acceptance: a 1 s source runs 10 or 11 times in 10 s while frames are
/// forced at 60 Hz on two attached clients.
#[test]
fn a_one_second_source_runs_ten_or_eleven_times_in_ten_seconds() {
    let srv = start("");
    let count = path(&srv, "count");
    let mut a = attach(&srv);
    let mut b = attach(&srv);
    load(
        &srv,
        &format!(
            "[status.sources.count]\ncmd = \"echo x >> '{}'; echo n\"\ninterval = 1\n",
            count.display()
        ),
    );
    let mut first: Option<Instant> = None;
    let (_, frames) = force_frames(&mut a, &mut b, Duration::from_secs(20), || {
        let now = Instant::now();
        if first.is_none() && lines(&count) >= 1 {
            first = Some(now);
        }
        first.is_some_and(|t| now >= t + Duration::from_secs(10))
    });
    let runs = lines(&count);
    eprintln!("{runs} runs, {frames} frame reads");
    assert!(first.is_some(), "the source never ran");
    assert!(frames > 100, "frames were not forced: {frames}");
    assert!((10..=11).contains(&runs), "{runs} runs in 10 s");
}

/// Acceptance: a source that sleeps 30 s never delays a frame beyond 50 ms
/// and its process group is killed at its 2 s timeout.
#[test]
fn a_hung_source_never_delays_a_frame_and_is_killed_at_its_timeout() {
    let srv = start("");
    let pidfile = path(&srv, "pid");
    let mut a = attach(&srv);
    let mut b = attach(&srv);
    let (baseline, _) = force_frames(&mut a, &mut b, Duration::from_secs(1), || false);
    load(
        &srv,
        &format!(
            "[status.sources.hung]\ncmd = \"echo $$ > '{}'; sleep 30; echo done\"\ninterval = 60\ntimeout = 2\n",
            pidfile.display()
        ),
    );
    let mut pgid: Option<(i32, Instant)> = None;
    let mut killed: Option<Instant> = None;
    let (longest, frames) = force_frames(&mut a, &mut b, Duration::from_secs(8), || {
        if pgid.is_none()
            && let Some(p) = fs::read_to_string(&pidfile)
                .ok()
                .and_then(|t| t.trim().parse().ok())
        {
            pgid = Some((p, Instant::now()));
        }
        if let Some((p, _)) = pgid
            && !group_alive(p)
        {
            killed = Some(Instant::now());
        }
        killed.is_some()
    });
    let (_, spawned) = pgid.expect("the source never started");
    let killed = killed.expect("the source's process group outlived its timeout");
    let lived = killed - spawned;
    assert!(
        lived >= Duration::from_millis(1500) && lived < Duration::from_secs(5),
        "killed after {lived:?}, want about 2 s"
    );
    assert!(frames > 50, "frames were not forced: {frames}");
    let bound = frame_bound(baseline);
    eprintln!(
        "longest frame gap {longest:?}, baseline {baseline:?}, killed after {lived:?}, {frames} frame reads"
    );
    assert!(
        longest < bound,
        "a frame waited {longest:?} (baseline {baseline:?}, bound {bound:?})"
    );
    // The run was killed, not finished: no value.
    assert_eq!(display(&srv, "#{@hung}"), "");
}

/// The segment is empty until the first result, then holds the last
/// non-empty line (a blank last line skipped, as tmux 3.7b does). `show -gv` returns it and `-q` hides the not-yet-set error, as
/// tmux 3.7b does for a user option.
#[test]
fn the_value_is_empty_until_the_run_ends_then_its_last_line() {
    let srv = TestServer::start();
    let started = path(&srv, "started");
    let gate = path(&srv, "gate");
    load(
        &srv,
        &format!(
            "[status.sources.batt]\ncmd = \"touch '{}'; while [ ! -e '{}' ]; do sleep 0.05; done; printf 'first\\\\nsecond\\\\n\\\\n'\"\ntimeout = 30\n",
            started.display(),
            gate.display()
        ),
    );
    ok(&srv, &["new-session", "-d", "-s", "s", "cat"]);
    wait_until(|| started.exists(), WAIT).expect("the source started");
    assert_eq!(display(&srv, "[#{@batt}]"), "[]");
    assert_eq!(ok(&srv, &["show-options", "-gqv", "@batt"]), "");
    assert_eq!(
        srv.cmd(&["show-options", "-gv", "@batt"]),
        Err("invalid option: @batt\n".to_string())
    );
    fs::write(&gate, "").expect("open the gate");
    wait_until(|| display(&srv, "#{@batt}") == "second", WAIT).expect("the last line arrives");
    assert_eq!(ok(&srv, &["show-options", "-gv", "@batt"]), "second\n");
    assert_eq!(ok(&srv, &["show-options", "-gqv", "@batt"]), "second\n");
    assert_eq!(ok(&srv, &["show-options", "-g", "@batt"]), "@batt second\n");
}

/// Two sources with one command share one run (checked by the command's
/// own side effect).
#[test]
fn two_registrations_of_one_command_share_one_run() {
    let srv = TestServer::start();
    let count = path(&srv, "count");
    let cmd = format!("echo run >> '{}'; echo hi", count.display());
    load(
        &srv,
        &format!(
            "[status.sources.a]\ncmd = \"{cmd}\"\ninterval = 60\n[status.sources.b]\ncmd = \"{cmd}\"\ninterval = 60\n"
        ),
    );
    ok(&srv, &["new-session", "-d", "-s", "s", "cat"]);
    wait_until(|| display(&srv, "#{@a}/#{@b}") == "hi/hi", WAIT).expect("both values");
    assert_eq!(lines(&count), 1, "the command ran more than once");
}

/// A changed value repaints the status line with no other trigger, and
/// `show -gv` returns the latest one.
#[test]
fn a_changed_value_repaints_the_status_line() {
    let srv = TestServer::start();
    let n = path(&srv, "n");
    let cmd = format!(
        "n=$(( $(cat '{0}' 2>/dev/null || echo 0) + 1 )); echo $n > '{0}'; echo v$n",
        n.display()
    );
    load(
        &srv,
        &format!(
            "[status]\ninterval = 60\nright = \"<#{{@tick}}>\"\n[status.sources.tick]\ncmd = \"{cmd}\"\ninterval = 1\n"
        ),
    );
    ok(&srv, &["new-session", "-d", "-s", "s", "cat"]);
    let mut a = attach(&srv);
    let seen = |a: &mut Harness| {
        let text = a.text();
        let row = text.lines().last().unwrap_or_default().to_string();
        row.split_once('<')
            .and_then(|(_, r)| r.split_once('>'))
            .and_then(|(v, _)| v.strip_prefix('v')?.parse::<u32>().ok())
    };
    let deadline = Instant::now() + WAIT;
    let first = loop {
        if let Some(v) = seen(&mut a) {
            break v;
        }
        assert!(Instant::now() < deadline, "no value on the status line");
        sleep(Duration::from_millis(20));
    };
    let deadline = Instant::now() + WAIT;
    while seen(&mut a).is_none_or(|v| v < first + 2) {
        assert!(
            Instant::now() < deadline,
            "the status line stayed at v{first}"
        );
        sleep(Duration::from_millis(20));
    }
    let last: u32 = ok(&srv, &["show-options", "-gv", "@tick"])
        .trim()
        .strip_prefix('v')
        .and_then(|v| v.parse().ok())
        .expect("a value");
    assert!(last >= first + 2);
}

/// A source gets the server environment, not the server process's: a
/// variable `set-environment -g` added is there, one `-gu` removed is not
/// (though the process still has it), and `TMUX` names this server with
/// session -1 and no `TMUX_PANE`, as tmux 3.7b gives its `#()` jobs.
#[test]
fn a_source_runs_with_the_server_environment() {
    let srv = start("");
    ok(&srv, &["set-environment", "-g", "ADDED", "yes"]);
    ok(&srv, &["set-environment", "-gu", "HOME"]);
    let socket = srv.socket.display().to_string();
    load(
        &srv,
        "[status.sources.env]\ncmd = 'echo \"${ADDED-unset}/${HOME-unset}/${TMUX_PANE-unset}/$TMUX/$GROFE\"'\ninterval = 60\n",
    );
    let pid = server_pid(&srv);
    let want = format!("yes/unset/unset/{socket},{pid},-1/{socket}");
    wait_until(|| display(&srv, "#{@env}") == want, WAIT)
        .unwrap_or_else(|_| panic!("got {:?}, want {want:?}", display(&srv, "#{@env}")));
}

/// Many runs leave no zombie children and no open descriptors behind.
#[test]
fn many_runs_leave_no_zombies_or_open_fds() {
    let srv = start("");
    let runs = path(&srv, "runs");
    let pid = server_pid(&srv);
    // The first child the server reaps sets up its SIGCHLD handling, which
    // stays open: count from after one.
    ok(&srv, &["run-shell", "true"]);
    let baseline = open_fds(pid);
    let mut config = String::new();
    for i in 0..10 {
        config.push_str(&format!(
            "[status.sources.s{i}]\ncmd = \"echo x >> '{}'; echo {i}\"\ninterval = 1\n",
            runs.display()
        ));
    }
    load(&srv, &config);
    wait_until(|| lines(&runs) >= 50, Duration::from_secs(20)).expect("50 runs");
    load(&srv, "");
    wait_until(|| zombies_of(pid) == 0 && open_fds(pid) <= baseline, WAIT).unwrap_or_else(|_| {
        panic!(
            "{} zombies, {} fds (baseline {baseline})",
            zombies_of(pid),
            open_fds(pid)
        )
    });
}

/// A source that spews output without a newline is cut at the line limit
/// and its pipe closed, so the server's memory stays bounded.
#[test]
fn a_source_that_spews_output_stays_bounded() {
    let srv = start("");
    let runs = path(&srv, "runs");
    let pid = server_pid(&srv);
    ok(&srv, &["run-shell", "true"]);
    let before = rss_kib(pid);
    load(
        &srv,
        &format!(
            "[status.sources.spew]\ncmd = \"echo x >> '{}'; yes | tr -d '\\\\n'\"\ninterval = 1\ntimeout = 5\n",
            runs.display()
        ),
    );
    wait_until(|| lines(&runs) >= 3, Duration::from_secs(20)).expect("three runs");
    wait_until(|| display(&srv, "#{@spew}").len() == 4096, WAIT).expect("a capped value");
    assert!(display(&srv, "#{@spew}").bytes().all(|b| b == b'y'));
    let after = rss_kib(pid);
    assert!(
        after < before + 32 * 1024,
        "rss grew from {before} KiB to {after} KiB"
    );
}

/// The schema: `[status.sources.<name>]` loads, shows in `show-config
/// --explain`, and a mistake is reported with its file, line and key.
#[test]
fn the_config_schema_checks_and_explains_sources() {
    let srv = TestServer::unstarted();
    write_config(
        &srv,
        "[status.sources.batt]\ncmd = \"pmset -g batt\"\ninterval = 30\ntimeout = 5\n",
    );
    let out = ok(&srv, &["show-config", "--explain", "status.sources.batt"]);
    let file = srv.config_home.join("grofe/config.toml");
    let file = file.display();
    assert_eq!(
        out,
        format!(
            "status.sources.batt = {{ cmd = \"pmset -g batt\", interval = 30, timeout = 5 }}\n  set by {file}\n"
        )
    );
    assert!(ok(&srv, &["show-config"]).contains("status.sources.batt = "));

    let cases = [
        (
            "[status.sources.b]\ncmd = \"x\"\nintervl = 3\n",
            "status.sources.b: unknown field intervl, did you mean interval?",
        ),
        (
            "[status.sources.b]\ncmd = \"x\"\ninterval = 0\n",
            "status.sources.b: interval: expected at least 1 second, found 0",
        ),
        (
            "[status.sources.b]\ninterval = 1\n",
            "status.sources.b: missing cmd",
        ),
        (
            "[status.sources.b]\ncmd = 3\n",
            "status.sources.b: cmd: expected a string, found integer",
        ),
        (
            "[status.sources.\"a b\"]\ncmd = \"x\"\n",
            "status.sources.a b: a source name is letters, digits, - and _",
        ),
        (
            "[status]\nsources = 1\n",
            "status.sources: expected a table of sources, found integer",
        ),
    ];
    for (text, want) in cases {
        write_config(&srv, text);
        let out = srv.run(&["show-config"]);
        assert!(!out.status.success(), "{text:?} loaded");
        assert!(
            out.stderr.contains(want),
            "{text:?}: {:?} lacks {want:?}",
            out.stderr
        );
    }
}

/// `show-options -v` and `-q` as recorded on tmux 3.7b (`-L grofe-tc4 -f
/// /dev/null`): `-v` prints each value alone and unquoted; `-q` turns an
/// unknown option, or a user option unset at that scope, into an empty
/// answer.
#[test]
fn show_v_and_q_answer_as_tmux() {
    let srv = start("");
    ok(&srv, &["set-option", "-g", "@x", "a b\"c$d"]);
    let show = |args: &[&str]| {
        let mut argv = vec!["show-options"];
        argv.extend_from_slice(args);
        srv.cmd(&argv)
    };
    let yes = |s: &str| Ok(s.to_string());
    let no = |s: &str| Err(format!("invalid option: {s}\n"));
    assert_eq!(show(&["-gv", "@x"]), yes("a b\"c$d\n"));
    assert_eq!(show(&["-g", "@x"]), yes("@x \"a b\\\"c\\$d\"\n"));
    assert_eq!(show(&["-v", "@x"]), no("@x"));
    assert_eq!(show(&["-qv", "@x"]), yes(""));
    assert_eq!(show(&["-gvq", "@x"]), yes("a b\"c$d\n"));
    assert_eq!(show(&["-q", "@nope"]), yes(""));
    assert_eq!(show(&["-gq", "nosuch"]), yes(""));
    assert_eq!(show(&["-gqv", "nosuch"]), yes(""));
    assert_eq!(show(&["-gv", "status-interval"]), yes("15\n"));
    assert_eq!(show(&["-v", "status-interval"]), yes(""));
    assert_eq!(show(&["-gv", "nosuch[0]"]), no("nosuch[0]"));
    assert_eq!(show(&["-gqv", "nosuch[0]"]), yes(""));
    assert_eq!(show(&["-gqv", "@nope[1]"]), yes(""));
}
