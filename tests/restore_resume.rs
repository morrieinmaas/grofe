//! T4.4: resumes at restore (spec §6). A restored pane with a `resume`
//! runs it directly, no shell, after a deterministic per-pane delay
//! (`fnv1a64(uid) mod restore-stagger-ms`, applied by the server, never
//! written into the argv); a pane without one gets its shell at once.
//! When a process a restore started from a resume exits, or cannot start,
//! the pane respawns a shell in its `cwd` with the same `uid`.
//!
//! The first test is the M4 done-means.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{PROMPT, TestServer, wait_until};
use grofe::server::START_MARKER;
use grofe::server::layout::{GeometryCell, LayoutGeometry};
use grofe::server::restore::stagger_delay;
use grofe::server::snapshot::{PaneSnap, SNAPSHOT_VERSION, SessionSnap, Snapshot, WindowSnap};
use grofe::types::Dir;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(15);

/// A temp dir holding `fake-agent` (on no `PATH` but the ones a test
/// builds) and its transcript dir, which the server's config hands every
/// pane as `FAKE_AGENT_HOME` (a restored session takes the server's
/// environment, and the harness's server has nothing else).
struct Agent {
    _dir: TempDir,
    bin: PathBuf,
    home: PathBuf,
}

impl Agent {
    fn new() -> Agent {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        let home = dir.path().join("agent");
        fs::create_dir_all(&bin).expect("bin dir");
        fs::create_dir_all(&home).expect("agent home");
        let agent = bin.join("fake-agent");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agent"),
            &agent,
        )
        .expect("copy fake-agent");
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).expect("chmod");
        Agent {
            _dir: dir,
            bin,
            home,
        }
    }

    fn path(&self) -> String {
        self.bin.join("fake-agent").display().to_string()
    }

    fn transcript(&self, id: &str) -> String {
        fs::read_to_string(self.home.join(format!("{id}.transcript"))).unwrap_or_default()
    }

    /// The transcript check: launched, then resumed, the same id.
    fn resumed(&self, id: &str) -> bool {
        self.transcript(id) == format!("launch {id}\nresume {id}\n")
    }

    /// Config: `FAKE_AGENT_HOME` for every pane, plus `extra`.
    fn config(&self, srv: &TestServer, extra: &str) {
        let dir = srv.config_home.join("grofe");
        fs::create_dir_all(&dir).expect("mkdir config");
        let text = format!(
            "{extra}\n[environment]\nFAKE_AGENT_HOME = \"{}\"\n",
            self.home.display()
        );
        fs::write(dir.join("config.toml"), text).expect("write config");
    }
}

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

fn fmt(srv: &TestServer, target: &str, format: &str) -> String {
    ok(srv, &["display-message", "-p", "-t", target, format])
        .trim_end_matches('\n')
        .to_string()
}

fn list_panes(srv: &TestServer, format: &str) -> Vec<String> {
    ok(srv, &["list-panes", "-a", "-F", format])
        .lines()
        .map(str::to_string)
        .collect()
}

fn type_line(srv: &TestServer, target: &str, line: &str) {
    ok(srv, &["send-keys", "-t", target, "-l", line]);
    ok(srv, &["send-keys", "-t", target, "Enter"]);
}

fn prompt_shown(screen: &str) -> bool {
    screen
        .lines()
        .any(|l| l.trim_end().ends_with(PROMPT.trim_end()))
}

fn wait_prompt(srv: &TestServer, target: &str) {
    srv.wait_for(|| prompt_shown(&srv.capture(target)), WAIT)
        .unwrap_or_else(|e| panic!("{target} prompt: {e}"));
}

/// The command a pane's `/bin/sh` reports, as tmux reports it too: macOS's
/// `/bin/sh` execs bash, so the process carries bash's name.
// ponytail: assumes macOS's default `/private/var/select/sh` (bash); a Mac
// that points it at dash or zsh reports that name instead.
const SH: &str = if cfg!(target_os = "macos") {
    "bash"
} else {
    "sh"
};

/// Waits until `target`'s foreground process is `fake-agent` run with
/// `args`, told by its argv (`/bin/sh <agent> <args>`): a script's name is
/// its interpreter's on macOS (`bash`, as tmux reports it too), and so is
/// the shell's that runs it.
fn wait_agent(srv: &TestServer, agent: &Agent, target: &str, args: &str) {
    let want = format!(" {} {args}", agent.path());
    srv.wait_for(
        || {
            srv.cmd(&[
                "display-message",
                "-p",
                "-t",
                target,
                "#{pane_current_argv}",
            ])
            .is_ok_and(|c| c.trim_end().ends_with(&want))
        },
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{target} runs fake-agent {args}: {e}"));
}

/// The snapshot's resumes by pane `uid`.
fn saved_resumes(srv: &TestServer) -> BTreeMap<String, Option<Vec<String>>> {
    let Ok(text) = fs::read_to_string(srv.snapshot_path()) else {
        return BTreeMap::new();
    };
    let Ok(snap) = serde_json::from_str::<Snapshot>(&text) else {
        return BTreeMap::new();
    };
    snap.sessions
        .iter()
        .flat_map(|s| &s.windows)
        .flat_map(|w| &w.panes)
        .map(|p| (p.uid.clone(), p.resume.clone()))
        .collect()
}

// ---- snapshots written by hand ----

fn cell(x: u16, y: u16, cols: u16, rows: u16, uid: &str) -> LayoutGeometry<String> {
    LayoutGeometry {
        x,
        y,
        cols,
        rows,
        cell: GeometryCell::Pane(uid.to_string()),
    }
}

fn pane(uid: &str, cwd: &Path, resume: Option<Vec<String>>) -> PaneSnap {
    PaneSnap {
        uid: uid.to_string(),
        cwd: cwd.to_path_buf(),
        argv: None,
        resume,
        options: Vec::new(),
    }
}

/// One session `main` with one window of `panes` side by side, 20 columns
/// each.
fn snapshot_of(home: &Path, panes: Vec<PaneSnap>) -> Snapshot {
    let n = panes.len() as u16;
    let cols = n * 21 - 1;
    let layout = if n == 1 {
        cell(0, 0, cols, 24, &panes[0].uid)
    } else {
        LayoutGeometry {
            x: 0,
            y: 0,
            cols,
            rows: 24,
            cell: GeometryCell::Split {
                dir: Dir::Horizontal,
                children: panes
                    .iter()
                    .enumerate()
                    .map(|(i, p)| cell(i as u16 * 21, 0, 20, 24, &p.uid))
                    .collect(),
            },
        }
    };
    Snapshot {
        version: SNAPSHOT_VERSION,
        written_at: "2026-11-05T10:00:00Z".to_string(),
        grofe_version: "0.0.0".to_string(),
        options: Vec::new(),
        sessions: vec![SessionSnap {
            name: "main".to_string(),
            cwd: home.to_path_buf(),
            active_window: 1,
            options: Vec::new(),
            windows: vec![WindowSnap {
                index: 1,
                name: "w".to_string(),
                automatic_rename: false,
                cols,
                rows: 24,
                layout,
                active_pane: panes[0].uid.clone(),
                zoomed: false,
                options: Vec::new(),
                panes,
            }],
        }],
    }
}

fn write_snapshot(srv: &TestServer, snap: &Snapshot) {
    let path = srv.snapshot_path();
    std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o700)
        .create(path.parent().expect("parent"))
        .expect("mkdir state");
    fs::write(&path, serde_json::to_string_pretty(snap).expect("json")).expect("write snapshot");
}

fn kill(srv: &TestServer) {
    ok(srv, &["kill-server"]);
    wait_until(|| !srv.is_running(), WAIT).expect("the server goes");
}

/// Kills whatever runs, writes `snap` and restores it.
fn restore(srv: &TestServer, snap: &Snapshot) {
    if srv.is_running() {
        kill(srv);
    }
    write_snapshot(srv, snap);
    ok(srv, &["start-server", "--restore"]);
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| s.to_string()).collect()
}

/// The log of the last server start only.
fn last_start_log(srv: &TestServer) -> String {
    let log = srv.log();
    match log.rfind(START_MARKER) {
        Some(at) => log[at..].to_string(),
        None => log,
    }
}

/// `(uid, offset in ms from the restore's start)` for every process the
/// last restore started, in log order: `what` is `resume` or `shell`.
fn started(log: &str, what: &str) -> Vec<(String, u64)> {
    let marker = format!(": {what} started at +");
    log.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("grofe server: restore: pane ")?;
            let (uid, rest) = rest.split_once(&marker)?;
            let ms = rest.split_once(" ms")?.0.parse().ok()?;
            Some((uid.to_string(), ms))
        })
        .collect()
}

fn wait_started(srv: &TestServer, what: &str, n: usize) -> Vec<(String, u64)> {
    srv.wait_for(|| started(&last_start_log(srv), what).len() >= n, WAIT)
        .unwrap_or_else(|e| panic!("{n} {what} starts: {e}; log:\n{}", last_start_log(srv)));
    started(&last_start_log(srv), what)
}

// ---- the done-means ----

/// Three panes in two windows, each running `fake-agent --session-id <id>`
/// from its shell (the temp dir only on that command's `PATH`) and bound
/// from outside with `pane-set-resume -- <its absolute path> --resume <id>`
/// (macOS 27 hides the environment of the `/bin/sh` the script runs
/// under, so no `PATH` of its own can resolve a bare name), under a server
/// started as `env -i PATH=/usr/bin:/bin`; the state is saved; returns
/// `(window index, uid)` per id.
fn three_agents(srv: &TestServer, agent: &Agent) -> Vec<(String, String)> {
    ok(
        srv,
        &["new-session", "-d", "-s", "main", "-x", "120", "-y", "30"],
    );
    ok(srv, &["split-window", "-h", "-t", "main:1"]);
    ok(srv, &["new-window", "-t", "main"]);
    let targets = [("a", "main:1.1"), ("b", "main:1.2"), ("c", "main:2.1")];
    let mut placed = Vec::new();
    for (id, target) in targets {
        wait_prompt(srv, target);
        type_line(
            srv,
            target,
            &format!(
                "PATH={}:$PATH fake-agent --session-id {id}",
                agent.bin.display()
            ),
        );
        wait_agent(srv, agent, target, &format!("--session-id {id}"));
        // Its argv shows before it writes the transcript, and the first run
        // of a freshly copied script can take a few hundred ms on macOS.
        let launched = format!("launch {id}\n");
        srv.wait_for(|| agent.transcript(id) == launched, WAIT)
            .unwrap_or_else(|e| panic!("{id} launched: {e}: {:?}", agent.transcript(id)));
        let pane = fmt(srv, target, "#{pane_id}");
        ok(
            srv,
            &[
                "pane-set-resume",
                "-t",
                &pane,
                "--",
                &agent.path(),
                "--resume",
                id,
            ],
        );
        assert_eq!(
            fmt(srv, target, "#{pane_resume}"),
            format!("{} --resume {id}", agent.path())
        );
        placed.push((
            fmt(srv, target, "#{window_index}"),
            fmt(srv, target, "#{pane_uid}"),
        ));
    }
    let uids: Vec<String> = placed.iter().map(|(_, u)| u.clone()).collect();
    srv.wait_for(
        || {
            let saved = saved_resumes(srv);
            uids.iter()
                .all(|u| saved.get(u).is_some_and(|r| r.is_some()))
        },
        WAIT,
    )
    .expect("the resumes are saved");
    placed
}

fn window_and_uid(srv: &TestServer) -> Vec<String> {
    list_panes(srv, "#{window_index} #{pane_uid}")
}

/// **The M4 done-means.** After a crash (`kill_hard`) and a restart, every
/// transcript reads `launch <id>` then `resume <id>`, each pane is in the
/// window it was in and every `uid` is unchanged, although `fake-agent`
/// sits on no `PATH` the server or its panes have and the server runs under
/// `env -i PATH=/usr/bin:/bin`.
#[test]
fn agent_panes_come_back_running_their_resume_after_a_crash() {
    let agent = Agent::new();
    let srv = TestServer::builder()
        .restore_on_start(true)
        .path_env("/usr/bin:/bin")
        .start();
    agent.config(&srv, "");
    // The config is read at start: restart once so it applies.
    let srv = srv.restart();
    three_agents(&srv, &agent);
    let before = window_and_uid(&srv);
    assert_eq!(before.len(), 3, "{before:?}");
    srv.kill_hard();
    let srv = srv.restart();
    srv.wait_for(|| ["a", "b", "c"].iter().all(|id| agent.resumed(id)), WAIT)
        .unwrap_or_else(|e| {
            panic!(
                "transcripts: {e}: a={:?} b={:?} c={:?}; log:\n{}",
                agent.transcript("a"),
                agent.transcript("b"),
                agent.transcript("c"),
                srv.log()
            )
        });
    assert_eq!(window_and_uid(&srv), before);
    // Each runs its resume directly (no shell), and keeps it for the next save.
    for (i, id) in ["a", "b", "c"].iter().enumerate() {
        let target = ["main:1.1", "main:1.2", "main:2.1"][i];
        wait_agent(&srv, &agent, target, &format!("--resume {id}"));
        assert_eq!(
            fmt(&srv, target, "#{pane_resume}"),
            format!("{} --resume {id}", agent.path())
        );
    }
}

/// Mutation proof: the snapshot edited so one pane resumes `zz` instead
/// turns that pane's transcript check red, and only that one.
#[test]
fn an_edited_resume_turns_that_panes_transcript_check_red() {
    let agent = Agent::new();
    let srv = TestServer::builder()
        .restore_on_start(true)
        .path_env("/usr/bin:/bin")
        .start();
    agent.config(&srv, "");
    let srv = srv.restart();
    let placed = three_agents(&srv, &agent);
    srv.kill_hard();
    let mutated = placed[1].1.clone();
    let path = agent.path();
    srv.edit_snapshot(|v| {
        for s in v["sessions"].as_array_mut().into_iter().flatten() {
            for w in s["windows"].as_array_mut().into_iter().flatten() {
                for p in w["panes"].as_array_mut().into_iter().flatten() {
                    if p["uid"] == mutated.as_str() {
                        p["resume"] = serde_json::json!([path, "--resume", "zz"]);
                    }
                }
            }
        }
    });
    let srv = srv.restart();
    srv.wait_for(
        || agent.resumed("a") && agent.resumed("c") && agent.transcript("zz") == "resume zz\n",
        WAIT,
    )
    .unwrap_or_else(|e| panic!("restored: {e}; log:\n{}", srv.log()));
    assert!(
        !agent.resumed("b"),
        "the mutated pane's check must be red: {:?}",
        agent.transcript("b")
    );
}

// ---- the stagger ----

/// The delay function's checked-in values: FNV-1a 64 of the `uid`, modulo
/// `restore-stagger-ms`; 0 is no delay at all.
#[test]
fn the_delay_function_returns_the_checked_in_values() {
    let ms = |uid: &str, stagger: u64| stagger_delay(uid, stagger).as_millis() as u64;
    for (uid, at_1000, at_5000) in [
        ("01UIDA", 195, 195),
        ("01UIDB", 406, 3406),
        ("01UIDC", 617, 1617),
        ("01UIDD", 828, 4828),
        ("01UIDE", 39, 3039),
        ("01JC0000000000000000000000", 609, 3609),
        ("01HV9ZK4M1Q2W3E4R5T6Y7U8I9", 118, 4118),
        ("x", 271, 2271),
        ("", 37, 1037),
    ] {
        assert_eq!(ms(uid, 1000), at_1000, "{uid} at 1000");
        assert_eq!(ms(uid, 5000), at_5000, "{uid} at 5000");
        assert_eq!(stagger_delay(uid, 0), Duration::ZERO, "{uid} at 0");
    }
}

/// Four resumes whose delays at 1000 are 195, 406, 617 and 828 ms.
fn four_resumes(home: &Path) -> Snapshot {
    let resume = || Some(words(&["/bin/sleep", "100"]));
    snapshot_of(
        home,
        vec![
            pane("01UIDD", home, resume()),
            pane("01UIDB", home, resume()),
            pane("01UIDC", home, resume()),
            pane("01UIDA", home, resume()),
        ],
    )
}

/// Two restores of the same snapshot spawn the resumes in the same order
/// and at the same offsets within 50 ms (from the log's monotonic offsets),
/// and those offsets are the delays.
#[test]
fn two_restores_spawn_the_resumes_in_the_same_order_and_offsets() {
    let srv = TestServer::start();
    let snap = four_resumes(&srv.home);
    restore(&srv, &snap);
    let first = wait_started(&srv, "resume", 4);
    restore(&srv, &snap);
    let second = wait_started(&srv, "resume", 4);
    let order = |s: &[(String, u64)]| s.iter().map(|(u, _)| u.clone()).collect::<Vec<_>>();
    assert_eq!(order(&first), ["01UIDA", "01UIDB", "01UIDC", "01UIDD"]);
    assert_eq!(order(&first), order(&second));
    for ((uid, a), (_, b)) in first.iter().zip(&second) {
        assert!(a.abs_diff(*b) <= 50, "{uid}: {a} ms then {b} ms");
        let want = stagger_delay(uid, 1000).as_millis() as u64;
        assert!(
            *a >= want && *a <= want + 50,
            "{uid} started at {a} ms, its delay is {want} ms"
        );
    }
    // The delay is the server's: nothing of it is in the argv.
    for line in list_panes(&srv, "#{pane_resume}") {
        assert_eq!(line, "/bin/sleep 100");
    }
}

/// With `restore-stagger-ms 0` every resume starts within 100 ms of the
/// restore.
#[test]
fn stagger_zero_starts_every_resume_at_once() {
    let srv = TestServer::start();
    let agent = Agent::new();
    agent.config(&srv, "restore-stagger-ms = 0");
    restore(&srv, &four_resumes(&srv.home));
    let starts = wait_started(&srv, "resume", 4);
    for (uid, ms) in &starts {
        assert!(*ms < 100, "{uid} started at {ms} ms");
    }
}

/// At 1000, a pane without a resume has its shell within 100 ms (it is
/// never delayed), while the resume beside it waits for its delay.
#[test]
fn a_pane_without_a_resume_is_never_delayed() {
    let srv = TestServer::start();
    let home = srv.home.clone();
    let snap = snapshot_of(
        &home,
        vec![
            pane("01UIDD", &home, Some(words(&["/bin/sleep", "100"]))),
            pane("01UIDE", &home, None),
        ],
    );
    restore(&srv, &snap);
    let shells = wait_started(&srv, "shell", 1);
    assert_eq!(shells.len(), 1, "{shells:?}");
    assert_eq!(shells[0].0, "01UIDE");
    assert!(shells[0].1 < 100, "the shell started at {} ms", shells[0].1);
    let resumes = wait_started(&srv, "resume", 1);
    assert_eq!(resumes[0].0, "01UIDD");
    assert!(resumes[0].1 >= 828, "{resumes:?}");
}

/// `send-keys` to a pane inside its delay waits in the pty and reaches
/// the process when it starts.
#[test]
fn input_sent_inside_the_delay_reaches_the_process() {
    let srv = TestServer::start();
    let agent = Agent::new();
    // 01UIDD waits 4828 ms at 5000.
    agent.config(&srv, "restore-stagger-ms = 5000");
    let out = srv.home.join("got");
    let script = format!(
        "read line; echo \"got:$line\" > {}; exec sleep 100",
        out.display()
    );
    let snap = snapshot_of(
        &srv.home,
        vec![pane(
            "01UIDD",
            &srv.home,
            Some(words(&["/bin/sh", "-c", &script])),
        )],
    );
    let t0 = Instant::now();
    restore(&srv, &snap);
    type_line(&srv, "main:1.1", "hello");
    assert!(
        t0.elapsed() < Duration::from_millis(4000),
        "the input must be sent inside the delay"
    );
    assert!(
        started(&last_start_log(&srv), "resume").is_empty(),
        "the resume started before the input was sent"
    );
    srv.wait_for(
        || fs::read_to_string(&out).is_ok_and(|s| s == "got:hello\n"),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("the input reached the process: {e}; log:\n{}", srv.log()));
    // The start's log line comes from the actor once the child is up, and
    // can follow the process's own first write.
    assert_eq!(wait_started(&srv, "resume", 1).len(), 1);
}

// ---- falling back to a shell ----

/// Killing a restored `fake-agent` leaves a shell in its `cwd` with the
/// same `uid` within 1 s, the geometry unchanged, its resume cleared, the
/// usual `pane-exited` event and one log line.
#[test]
fn a_killed_restored_agent_leaves_a_shell_in_its_cwd() {
    let srv = TestServer::start();
    let agent = Agent::new();
    agent.config(&srv, "restore-stagger-ms = 0");
    // Resolved, as a pane's current path is (macOS's temp dir is under
    // `/var`, a symlink to `/private/var`).
    let cwd = srv
        .home
        .canonicalize()
        .expect("canonical home")
        .join("work");
    fs::create_dir_all(&cwd).expect("mkdir");
    let snap = snapshot_of(
        &srv.home,
        vec![
            pane(
                "01UIDA",
                &cwd,
                Some(vec![agent.path(), "--resume".into(), "a".into()]),
            ),
            pane("01UIDE", &srv.home, None),
        ],
    );
    restore(&srv, &snap);
    srv.wait_for(|| agent.transcript("a") == "resume a\n", WAIT)
        .unwrap_or_else(|e| panic!("resumed: {e}; log:\n{}", srv.log()));
    let geometry = "#{pane_uid} #{pane_left},#{pane_top} #{pane_width}x#{pane_height}";
    let before = list_panes(&srv, geometry);
    let target = "main:1.1";
    assert_eq!(fmt(&srv, target, "#{pane_uid}"), "01UIDA");
    let pid = fmt(&srv, target, "#{pane_pid}");
    let mut sub = srv.subscribe(&["pane-exited"]).expect("subscribe");
    let killed = Instant::now();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid.parse().expect("pid")),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill the agent");
    let event = sub.next(WAIT).expect("pane-exited");
    let event = serde_json::to_string(&event).expect("json");
    assert!(event.contains(r#""uid":"01UIDA""#), "{event}");
    srv.wait_for(
        || {
            srv.cmd(&["display-message", "-p", "-t", target, "#{pane_pid}"])
                .is_ok_and(|p| !p.trim().is_empty() && p.trim() != pid)
        },
        Duration::from_secs(1),
    )
    .unwrap_or_else(|e| panic!("a new process within 1 s: {e}; log:\n{}", srv.log()));
    assert!(killed.elapsed() < Duration::from_secs(1));
    wait_prompt(&srv, target);
    assert_eq!(fmt(&srv, target, "#{pane_uid}"), "01UIDA");
    assert_eq!(fmt(&srv, target, "#{pane_dead}"), "0");
    assert_eq!(fmt(&srv, target, "#{pane_current_command}"), SH);
    assert_eq!(
        fmt(&srv, target, "#{pane_current_path}"),
        cwd.display().to_string()
    );
    assert_eq!(fmt(&srv, target, "#{pane_resume}"), "");
    assert_eq!(list_panes(&srv, geometry), before);
    let log = last_start_log(&srv);
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("pane 01UIDA: resume") && l.contains("exited"))
        .collect();
    assert_eq!(lines.len(), 1, "{log}");
    // The snapshot follows: a shell, no resume.
    srv.wait_for(|| saved_resumes(&srv).get("01UIDA") == Some(&None), WAIT)
        .expect("the cleared resume is saved");
    // A shell is not restored-from-resume: its exit follows remain-on-exit
    // (the harness keeps dead panes).
    type_line(&srv, target, "exit");
    srv.wait_for(|| fmt(&srv, target, "#{pane_dead}") == "1", WAIT)
        .expect("the shell's exit is an ordinary one");
}

/// A resume whose binary was deleted after the snapshot gives a shell with
/// the same `uid` and a log line naming the argv; the rest of the restore
/// goes on.
#[test]
fn a_resume_whose_binary_is_gone_gives_a_shell() {
    let srv = TestServer::start();
    let agent = Agent::new();
    // The default stagger: the failed start is a delayed one (195 ms).
    agent.config(&srv, "");
    let gone = agent.bin.join("deleted-agent").display().to_string();
    let snap = snapshot_of(
        &srv.home,
        vec![
            pane(
                "01UIDA",
                &srv.home,
                Some(vec![gone.clone(), "--resume".into(), "a".into()]),
            ),
            pane(
                "01UIDB",
                &srv.home,
                Some(vec![agent.path(), "--resume".into(), "b".into()]),
            ),
        ],
    );
    restore(&srv, &snap);
    let target = "main:1.1";
    wait_prompt(&srv, target);
    assert_eq!(fmt(&srv, target, "#{pane_uid}"), "01UIDA");
    assert_eq!(fmt(&srv, target, "#{pane_dead}"), "0");
    assert_eq!(fmt(&srv, target, "#{pane_resume}"), "");
    let log = last_start_log(&srv);
    assert!(
        log.lines()
            .any(|l| l.contains("pane 01UIDA") && l.contains(&format!("{gone} --resume a"))),
        "{log}"
    );
    srv.wait_for(|| agent.transcript("b") == "resume b\n", WAIT)
        .expect("the other pane resumed");
    assert_eq!(list_panes(&srv, "#{pane_uid}"), ["01UIDA", "01UIDB"]);
}

/// A resume taken from a spawn command keeps its words, but a bare
/// `argv[0]` is made absolute when it is bound, against the `PATH` the
/// pane was spawned with: a restore under `env -i PATH=/usr/bin:/bin`
/// still finds it.
#[test]
fn a_spawn_command_resume_is_resolved_and_restored() {
    let agent = Agent::new();
    let srv = TestServer::builder()
        .restore_on_start(true)
        .path_env("/usr/bin:/bin")
        .start();
    agent.config(&srv, "restore-stagger-ms = 0");
    let srv = srv.restart();
    ok(
        &srv,
        &["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"],
    );
    let path = format!("PATH={}:/usr/bin:/bin", agent.bin.display());
    let pane = ok(
        &srv,
        &[
            "new-window",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "main",
            "-e",
            &path,
            "--",
            "fake-agent",
            "--resume",
            "s",
        ],
    );
    let pane = pane.trim().to_string();
    srv.wait_for(|| agent.transcript("s") == "resume s\n", WAIT)
        .unwrap_or_else(|e| panic!("launched: {e}; log:\n{}", srv.log()));
    assert_eq!(
        fmt(&srv, &pane, "#{pane_resume}"),
        format!("{} --resume s", agent.path())
    );
    let uid = fmt(&srv, &pane, "#{pane_uid}");
    srv.wait_for(
        || saved_resumes(&srv).get(&uid).is_some_and(|r| r.is_some()),
        WAIT,
    )
    .expect("saved");
    srv.kill_hard();
    let srv = srv.restart();
    srv.wait_for(|| agent.transcript("s") == "resume s\nresume s\n", WAIT)
        .unwrap_or_else(|e| panic!("restored: {e}; log:\n{}", srv.log()));
    assert!(list_panes(&srv, "#{pane_uid}").contains(&uid));
}

/// A one-word spawn command is a script for the shell: a plain name is
/// made absolute as a multi-word `argv[0]` is (else a restore would look it
/// up on the server's `PATH`), and any other script is kept as it is.
#[test]
fn a_one_word_spawn_command_is_resolved_only_when_plain() {
    let agent = Agent::new();
    let srv = TestServer::builder().path_env("/usr/bin:/bin").start();
    let idle = agent.bin.join("idle");
    fs::write(&idle, "#!/bin/sh\nexec sleep 100\n").expect("write idle");
    fs::set_permissions(&idle, fs::Permissions::from_mode(0o755)).expect("chmod");
    ok(
        &srv,
        &["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"],
    );
    let path = format!("PATH={}:/usr/bin:/bin", agent.bin.display());
    let spawn = |command: &str| {
        let pane = ok(
            &srv,
            &[
                "new-window",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                "main",
                "-e",
                &path,
                command,
            ],
        );
        pane.trim().to_string()
    };
    let plain = spawn("idle");
    assert_eq!(
        fmt(&srv, &plain, "#{pane_resume}"),
        idle.display().to_string()
    );
    let script = spawn("idle; true");
    // Shown quoted, as one word.
    assert_eq!(fmt(&srv, &script, "#{pane_resume}"), "\"idle; true\"");
}

/// A server killed while a resume waits out its delay never starts it:
/// nothing is left running after the server.
#[test]
fn kill_server_inside_the_delay_starts_nothing() {
    let srv = TestServer::start();
    let agent = Agent::new();
    // 01UIDD waits 4828 ms at 5000.
    agent.config(&srv, "restore-stagger-ms = 5000");
    let out = srv.home.join("ran");
    let script = format!("echo ran > {}; exec sleep 100", out.display());
    let snap = snapshot_of(
        &srv.home,
        vec![pane(
            "01UIDD",
            &srv.home,
            Some(words(&["/bin/sh", "-c", &script])),
        )],
    );
    restore(&srv, &snap);
    kill(&srv);
    std::thread::sleep(Duration::from_millis(6000));
    assert!(!out.exists(), "the resume ran after kill-server");
}
