//! T4.2: `pane-set-resume` (spec §6). `argv[0]` is resolved to an absolute
//! path from the caller's `PATH` (a CLI inside a pane) or the target pane's
//! (its foreground process's, then its spawn environment's), never the
//! server's; the resume is bound to the process it was set for and cleared
//! when that process is gone; `--expect` records an expectation the same
//! way; `-u` clears.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use common::{PROMPT, TestServer};
use tempfile::TempDir;

/// How often the server checks bindings (`resume::SAMPLE`), plus the 1 s
/// the plan allows on top of one sample.
const CLEARED_WITHIN: Duration = Duration::from_millis(2_000);

/// Long enough to cover several samples: what "keeps it" waits for.
const SEVERAL_SAMPLES: Duration = Duration::from_millis(2_500);

const WAIT: Duration = Duration::from_secs(10);

/// A server started as under `env -i PATH=/usr/bin:/bin`, a temp dir
/// holding `fake-agent` (on nobody's `PATH` but the ones a test builds)
/// and the agent's transcript dir, with a session `main` whose panes have
/// `FAKE_AGENT_HOME`.
struct Fixture {
    srv: TestServer,
    _dir: TempDir,
    bin: PathBuf,
    agent_home: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let srv = TestServer::builder().path_env("/usr/bin:/bin").start();
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        let agent_home = dir.path().join("agent");
        fs::create_dir_all(&bin).expect("bin dir");
        fs::create_dir_all(&agent_home).expect("agent home");
        let agent = bin.join("fake-agent");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agent"),
            &agent,
        )
        .expect("copy fake-agent");
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).expect("chmod");
        let home = format!("FAKE_AGENT_HOME={}", agent_home.display());
        srv.cmd(&[
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            "120",
            "-y",
            "30",
            "-e",
            &home,
        ])
        .expect("new-session");
        let f = Fixture {
            srv,
            _dir: dir,
            bin,
            agent_home,
        };
        f.wait_prompt("main");
        f
    }

    fn agent(&self) -> String {
        self.bin.join("fake-agent").display().to_string()
    }

    fn wait_prompt(&self, target: &str) {
        self.srv
            .wait_for(|| prompt_shown(&self.srv.capture(target)), WAIT)
            .unwrap_or_else(|e| panic!("{target} prompt: {e}"));
    }

    fn fmt(&self, target: &str, format: &str) -> String {
        self.srv
            .cmd(&["display-message", "-p", "-t", target, format])
            .expect("display-message")
            .trim_end_matches('\n')
            .to_string()
    }

    fn resume(&self, target: &str) -> String {
        self.fmt(target, "#{pane_resume}")
    }

    fn expected(&self, target: &str) -> String {
        self.fmt(target, "#{pane_resume_expected}")
    }

    fn type_line(&self, target: &str, line: &str) {
        self.srv
            .cmd(&["send-keys", "-t", target, "-l", line])
            .expect("send-keys -l");
        self.srv
            .cmd(&["send-keys", "-t", target, "Enter"])
            .expect("send-keys Enter");
    }

    fn transcript(&self, id: &str) -> String {
        fs::read_to_string(self.agent_home.join(format!("{id}.transcript"))).unwrap_or_default()
    }

    /// Waits until the pane's foreground process is no longer its shell.
    fn wait_foreground(&self, target: &str, command: &str) {
        self.srv
            .wait_for(
                || self.fmt(target, "#{pane_current_command}") == command,
                WAIT,
            )
            .unwrap_or_else(|e| {
                panic!(
                    "{target} runs {command}: {e}; it runs {}",
                    self.fmt(target, "#{pane_current_command}")
                )
            });
    }

    /// Starts `fake-agent --session-id <id>` from the pane's shell with the
    /// temp dir on its `PATH` (the shell's own `PATH` stays the server's),
    /// and waits for it to be the foreground process.
    fn launch_agent(&self, target: &str, id: &str) {
        self.type_line(
            target,
            &format!(
                "PATH={}:$PATH fake-agent --session-id {id}",
                self.bin.display()
            ),
        );
        self.srv
            .wait_for(
                || self.transcript(id).contains(&format!("launch {id}")),
                WAIT,
            )
            .expect("the agent launched");
        self.wait_foreground(target, "fake-agent");
    }

    fn pane_id(&self, target: &str) -> String {
        self.fmt(target, "#{pane_id}")
    }
}

/// Whether a line of a capture ends at the shell's prompt (a capture
/// trims the space after it).
fn prompt_shown(capture: &str) -> bool {
    capture
        .lines()
        .any(|l| l.trim_end().ends_with(PROMPT.trim_end()))
}

/// The pid of the one process whose command line holds `needle`.
fn pid_with(needle: &str) -> i32 {
    let ps = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
        .expect("ps");
    let pids: Vec<i32> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| l.contains(needle) && !l.contains("ps -A"))
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect();
    assert_eq!(pids.len(), 1, "one process with {needle}: {pids:?}");
    pids[0]
}

#[test]
fn a_cli_inside_a_pane_resolves_against_its_own_path_and_respawn_runs_it() {
    let f = Fixture::new();
    // The server's PATH and the shell's lack the temp dir; only this CLI's
    // own environment has it.
    f.type_line(
        "main",
        &format!(
            "PATH={}:$PATH {} -S {} pane-set-resume -- fake-agent --resume a",
            f.bin.display(),
            env!("CARGO_BIN_EXE_grofe"),
            f.srv.socket.display(),
        ),
    );
    let want = format!("{} --resume a", f.agent());
    f.srv
        .wait_for(|| f.resume("main") == want, WAIT)
        .unwrap_or_else(|e| panic!("{e}: #{{pane_resume}} is {:?}", f.resume("main")));
    // The CLI has exited; the resume stays, bound to the shell that ran it.
    f.wait_prompt("main");
    thread::sleep(SEVERAL_SAMPLES);
    assert_eq!(f.resume("main"), want);
    f.srv
        .cmd(&["respawn-pane", "-k", "-t", "main"])
        .expect("respawn-pane -k");
    f.srv
        .wait_for(|| f.transcript("a").contains("resume a"), WAIT)
        .unwrap_or_else(|e| panic!("{e}: transcript {:?}", f.transcript("a")));
}

#[test]
fn from_outside_a_pane_the_foreground_process_path_resolves() {
    let f = Fixture::new();
    f.type_line(
        "main",
        &format!("PATH={}:$PATH sleep 1000", f.bin.display()),
    );
    f.wait_foreground("main", "sleep");
    f.srv
        .cmd(&[
            "pane-set-resume",
            "-t",
            "main",
            "--",
            "fake-agent",
            "--resume",
            "a",
        ])
        .expect("pane-set-resume");
    assert_eq!(f.resume("main"), format!("{} --resume a", f.agent()));
}

#[test]
fn from_outside_a_pane_the_spawn_environment_path_resolves() {
    let f = Fixture::new();
    let path = format!("PATH={}:/usr/bin:/bin", f.bin.display());
    let pane = f
        .srv
        .cmd(&[
            "new-window",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "main",
            "-e",
            &path,
        ])
        .expect("new-window -e PATH");
    let pane = pane.trim();
    f.wait_prompt(pane);
    // The foreground process has no PATH of its own.
    f.type_line(pane, "env -i /bin/sleep 1000");
    f.wait_foreground(pane, "sleep");
    f.srv
        .cmd(&[
            "pane-set-resume",
            "-t",
            pane,
            "--",
            "fake-agent",
            "--resume",
            "b",
        ])
        .expect("pane-set-resume");
    assert_eq!(f.resume(pane), format!("{} --resume b", f.agent()));
}

#[test]
fn from_outside_a_pane_an_unresolvable_name_errors_and_keeps_the_old_resume() {
    let f = Fixture::new();
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "--", "/bin/sleep", "7"])
        .expect("an absolute argv[0]");
    assert_eq!(f.resume("main"), "/bin/sleep 7");
    // Neither the shell's PATH nor the spawn environment's has fake-agent,
    // and the server's is never used.
    let err = f
        .srv
        .cmd(&[
            "pane-set-resume",
            "-t",
            "main",
            "--",
            "fake-agent",
            "--resume",
            "a",
        ])
        .expect_err("fake-agent is on no PATH the pane has");
    assert!(err.contains("fake-agent"), "{err}");
    assert_eq!(f.resume("main"), "/bin/sleep 7");
}

#[test]
fn closing_the_agent_clears_its_resume_and_the_shell_stays() {
    let f = Fixture::new();
    f.launch_agent("main", "a");
    let pane = f.pane_id("main");
    f.srv
        .cmd(&[
            "pane-set-resume",
            "-t",
            &pane,
            "--",
            "fake-agent",
            "--resume",
            "a",
        ])
        .expect("pane-set-resume");
    assert_eq!(f.resume(&pane), format!("{} --resume a", f.agent()));
    f.srv.cmd(&["send-keys", "-t", &pane, "C-c"]).expect("C-c");
    f.srv
        .wait_for(|| f.resume(&pane).is_empty(), CLEARED_WITHIN)
        .unwrap_or_else(|e| panic!("{e}: #{{pane_resume}} is {:?}", f.resume(&pane)));
    assert_eq!(f.fmt(&pane, "#{pane_dead}"), "0");
    f.wait_foreground(&pane, "sh");
}

#[test]
fn a_stopped_agent_keeps_its_resume() {
    let f = Fixture::new();
    f.launch_agent("main", "stopme");
    f.srv
        .cmd(&[
            "pane-set-resume",
            "-t",
            "main",
            "--",
            "fake-agent",
            "--resume",
            "stopme",
        ])
        .expect("pane-set-resume");
    let pid = pid_with("--session-id stopme");
    let pid = nix::unistd::Pid::from_raw(pid);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).expect("SIGSTOP");
    thread::sleep(SEVERAL_SAMPLES);
    let still = f.resume("main");
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT);
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    assert_eq!(still, format!("{} --resume stopme", f.agent()));
}

#[test]
fn a_resume_set_over_the_shell_survives_running_ls() {
    let f = Fixture::new();
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "--", "/bin/sleep", "9"])
        .expect("pane-set-resume");
    f.type_line("main", "ls /");
    f.srv
        .wait_for(|| f.srv.capture("main").contains("usr"), WAIT)
        .expect("ls ran");
    thread::sleep(SEVERAL_SAMPLES);
    assert_eq!(f.resume("main"), "/bin/sleep 9");
}

#[test]
fn an_expectation_shows_without_a_resume_and_clears_with_its_process() {
    let f = Fixture::new();
    f.launch_agent("main", "e");
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "--expect", "no id"])
        .expect("--expect");
    assert_eq!(f.resume("main"), "");
    assert_eq!(f.expected("main"), "no id");
    f.srv.cmd(&["send-keys", "-t", "main", "C-c"]).expect("C-c");
    f.srv
        .wait_for(|| f.expected("main").is_empty(), CLEARED_WITHIN)
        .unwrap_or_else(|e| panic!("{e}: expected {:?}", f.expected("main")));
}

#[test]
fn dash_u_clears() {
    let f = Fixture::new();
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "--", "/bin/sleep", "9"])
        .expect("pane-set-resume");
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "-u"])
        .expect("-u");
    assert_eq!(f.resume("main"), "");
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "--expect", "later"])
        .expect("--expect");
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "-u"])
        .expect("-u");
    assert_eq!(f.expected("main"), "");
}

#[test]
fn a_spawn_command_is_the_resume_bound_to_the_root() {
    let f = Fixture::new();
    let pane = f
        .srv
        .cmd(&[
            "new-window",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "main",
            "/bin/sleep",
            "1000",
        ])
        .expect("new-window");
    let pane = pane.trim();
    assert_eq!(f.resume(pane), "/bin/sleep 1000");
    thread::sleep(SEVERAL_SAMPLES);
    assert_eq!(f.resume(pane), "/bin/sleep 1000");
}

#[test]
fn with_no_server_it_fails_and_starts_nothing() {
    let srv = TestServer::unstarted();
    let out = srv.run(&["pane-set-resume", "--", "/bin/true"]);
    assert!(!out.status.success(), "{out:?}");
    thread::sleep(Duration::from_millis(300));
    assert!(!srv.is_running());
    assert_eq!(srv.server_starts(), 0);
}

#[test]
fn every_set_and_clear_raises_resume_changed() {
    let f = Fixture::new();
    f.srv
        .cmd(&["set-hook", "-g", "resume-changed", "set -g -a @rc x"])
        .expect("set-hook");
    let count = || {
        f.srv
            .cmd(&["show-options", "-gqv", "@rc"])
            .unwrap_or_default()
            .trim()
            .len()
    };
    f.launch_agent("main", "h");
    f.srv
        .cmd(&[
            "pane-set-resume",
            "-t",
            "main",
            "--",
            "fake-agent",
            "--resume",
            "h",
        ])
        .expect("set");
    f.srv
        .wait_for(|| count() == 1, WAIT)
        .expect("raised on set");
    f.srv
        .cmd(&[
            "pane-set-resume",
            "-t",
            "main",
            "--",
            "fake-agent",
            "--resume",
            "h",
        ])
        .expect("the same again");
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "--expect", "no id"])
        .expect("expect");
    f.srv
        .wait_for(|| count() == 2, WAIT)
        .expect("raised on expect");
    f.srv.cmd(&["send-keys", "-t", "main", "C-c"]).expect("C-c");
    f.srv
        .wait_for(|| count() == 3, CLEARED_WITHIN + WAIT)
        .expect("raised when its process went");
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "--", "/bin/sleep", "9"])
        .expect("set");
    f.srv
        .cmd(&["pane-set-resume", "-t", "main", "-u"])
        .expect("-u");
    f.srv
        .wait_for(|| count() == 5, WAIT)
        .expect("raised on set and -u");
    // Nothing that changes nothing raises it.
    thread::sleep(Duration::from_millis(300));
    assert_eq!(count(), 5);
}

/// A resume from a spawn command is bound to the pane's root process:
/// when that process exits and the pane stays (`remain-on-exit`), the
/// resume is cleared in the same step that marks the pane dead (the
/// actor's exit handling), and `resume-changed` is raised for the spawn
/// and for the clear.
#[test]
fn a_spawn_command_resume_clears_when_its_process_exits_and_the_pane_stays() {
    let f = Fixture::new();
    f.srv
        .cmd(&["set-option", "-g", "remain-on-exit", "on"])
        .expect("remain-on-exit");
    f.srv
        .cmd(&["set-hook", "-g", "resume-changed", "set -g -a @rc x"])
        .expect("set-hook");
    let count = || {
        f.srv
            .cmd(&["show-options", "-gqv", "@rc"])
            .unwrap_or_default()
            .trim()
            .len()
    };
    let agent = f.agent();
    let pane = f
        .srv
        .cmd(&[
            "new-window",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "main",
            "--",
            &agent,
            "--session-id",
            "x",
        ])
        .expect("new-window");
    let pane = pane.trim().to_string();
    f.srv
        .wait_for(|| f.transcript("x").contains("launch x"), WAIT)
        .expect("the agent launched");
    assert_eq!(f.resume(&pane), format!("{agent} --session-id x"));
    f.srv
        .wait_for(|| count() == 1, WAIT)
        .expect("raised for the spawn");
    f.srv.cmd(&["send-keys", "-t", &pane, "C-c"]).expect("C-c");
    // Dead and resume read in one request, so one actor step: a dead pane
    // never shows the resume of the process that exited.
    let state = || f.fmt(&pane, "#{pane_dead}|#{pane_resume}");
    let started = std::time::Instant::now();
    loop {
        let now = state();
        if now == "1|" {
            break;
        }
        assert_eq!(
            now,
            format!("0|{agent} --session-id x"),
            "dead before cleared"
        );
        assert!(started.elapsed() < WAIT, "the agent never exited: {now}");
        thread::sleep(Duration::from_millis(20));
    }
    f.srv
        .wait_for(|| count() == 2, WAIT)
        .expect("raised for the clear");
    // `respawn-pane <cmd>` binds the new command to the new root, and its
    // exit clears it the same way.
    f.srv
        .cmd(&["respawn-pane", "-t", &pane, "--", "/bin/sleep", "1"])
        .expect("respawn-pane");
    assert_eq!(f.resume(&pane), "/bin/sleep 1");
    f.srv
        .wait_for(|| state() == "1|", WAIT)
        .unwrap_or_else(|e| panic!("{e}: {}", state()));
    f.srv
        .wait_for(|| count() == 4, WAIT)
        .unwrap_or_else(|e| panic!("{e}: {} raised", count()));
}

/// The CLI asking is told by its pid, not its executable: a grofe at
/// another path (an upgrade installed while the server runs) binds to the
/// shell that ran it just the same, not to itself.
#[test]
fn a_cli_from_another_grofe_binary_binds_to_its_shell() {
    let f = Fixture::new();
    let other = f.bin.join("grofe-upgraded");
    fs::copy(env!("CARGO_BIN_EXE_grofe"), &other).expect("copy grofe");
    f.type_line(
        "main",
        &format!(
            "{} -S {} pane-set-resume -- /bin/sleep 5",
            other.display(),
            f.srv.socket.display(),
        ),
    );
    f.srv
        .wait_for(|| f.resume("main") == "/bin/sleep 5", WAIT)
        .unwrap_or_else(|e| panic!("{e}: #{{pane_resume}} is {:?}", f.resume("main")));
    f.wait_prompt("main");
    thread::sleep(SEVERAL_SAMPLES);
    assert_eq!(f.resume("main"), "/bin/sleep 5");
}
