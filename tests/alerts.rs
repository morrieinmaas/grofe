//! Window alerts and their hooks (`server::alerts`, TP.10), against what
//! tmux 3.7b did on a private socket (`-f /dev/null`, two windows, no
//! client attached, the hooks logging through `run-shell`):
//!
//! ```text
//! alert-bell s=a w=@1 ctx=a:1.0 flags=!       bell in window 1
//! alert-bell s=a w=@1 ctx=a:1.0 flags=!       again: every bell fires
//! alert-bell s=a w=@0 ctx=a:0.0 flags=!*      the current window too (any)
//! alert-activity s=a w=@1 ctx=a:1.0 flags=##! once, not for window 0 (other)
//! alert-silence s=a w=@1 ctx=a:1.0 flags=##!~ once, not for window 0
//! ```
//!
//! and `list-windows` read `0 ##!~*` and `1 ##!~` at the end. The commands
//! go through the tmux front end, as `tmux` naming grofe sends them.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use common::{TestServer, run_bounded, wait_until};

struct Tmux {
    srv: TestServer,
    bin: PathBuf,
    log: PathBuf,
}

impl Tmux {
    fn start() -> Tmux {
        let srv = TestServer::start();
        let bin = srv.home.join("tmux");
        symlink(env!("CARGO_BIN_EXE_grofe"), &bin).unwrap();
        let log = srv.home.join("alerts.log");
        let t = Tmux { srv, bin, log };
        // tmux's own index, as the recording had: grofe's default is 1.
        t.ok(&["set", "-g", "base-index", "0"]);
        t.ok(&["set", "-g", "pane-base-index", "0"]);
        t.ok(&[
            "new-session",
            "-d",
            "-s",
            "a",
            "-x",
            "80",
            "-y",
            "24",
            "sleep 600",
        ]);
        t.ok(&["new-window", "-d", "-t", "a:1", "sleep 600"]);
        t
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let base = self.srv.command();
        let mut cmd = Command::new(&self.bin);
        for (k, v) in base.get_envs() {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        cmd.current_dir(&self.srv.home)
            .arg("-S")
            .arg(&self.srv.socket)
            .args(args);
        let out = run_bounded(cmd, Duration::from_secs(10));
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(out.stderr)
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        self.run(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    fn hook_all(&self) {
        let cmd = format!(
            "run-shell \"echo #{{hook}} s=#{{hook_session_name}} w=#{{hook_window}} \
             ctx=#{{session_name}}:#{{window_index}}.#{{pane_index}} \
             flags=#{{window_flags}} >> {}\"",
            self.log.display()
        );
        for hook in ["alert-bell", "alert-activity", "alert-silence"] {
            self.ok(&["set-hook", "-g", hook, &cmd]);
        }
    }

    fn tty(&self, window: &str) -> String {
        self.ok(&["display", "-p", "-t", window, "#{pane_tty}"])
            .trim()
            .to_string()
    }

    fn lines(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn wait_lines(&self, n: usize) -> Vec<String> {
        let got = wait_until(|| self.lines().len() >= n, Duration::from_secs(10));
        assert!(
            got.is_ok(),
            "{} lines, want {n}: {:?}",
            self.lines().len(),
            self.lines()
        );
        self.lines()
    }

    fn flags(&self) -> String {
        self.ok(&["list-windows", "-F", "#{window_index} #{window_flags}"])
    }
}

fn write_tty(tty: &str, bytes: &[u8]) {
    fs::write(tty, bytes).unwrap();
}

#[test]
fn alert_options_have_tmux_defaults() {
    let t = Tmux::start();
    let window = t.ok(&["show", "-gw"]);
    for line in [
        "monitor-activity off",
        "monitor-bell on",
        "monitor-silence 0",
    ] {
        assert!(window.lines().any(|l| l == line), "{line}:\n{window}");
    }
    let session = t.ok(&["show", "-g"]);
    for line in [
        "activity-action other",
        "bell-action any",
        "silence-action other",
        "visual-activity off",
        "visual-bell off",
        "visual-silence off",
    ] {
        assert!(session.lines().any(|l| l == line), "{line}:\n{session}");
    }
}

#[test]
fn every_bell_fires_alert_bell_in_its_window() {
    let t = Tmux::start();
    t.hook_all();
    let (one, zero) = (t.tty("a:1"), t.tty("a:0"));
    write_tty(&one, b"\x07");
    t.wait_lines(1);
    write_tty(&one, b"\x07");
    t.wait_lines(2);
    write_tty(&zero, b"\x07");
    let lines = t.wait_lines(3);
    assert_eq!(
        lines,
        [
            "alert-bell s=a w=@1 ctx=a:1.0 flags=!",
            "alert-bell s=a w=@1 ctx=a:1.0 flags=!",
            "alert-bell s=a w=@0 ctx=a:0.0 flags=!*",
        ]
    );
    // bell-action none: the flag is set, no hook fires; monitor-bell off:
    // neither.
    t.ok(&["select-window", "-t", "a:1"]);
    t.ok(&["select-window", "-t", "a:0"]);
    t.ok(&["set", "-g", "bell-action", "none"]);
    write_tty(&one, b"\x07");
    let set = wait_until(|| t.flags().contains("1 !-"), Duration::from_secs(10));
    assert!(set.is_ok(), "{}", t.flags());
    t.ok(&["select-window", "-t", "a:1"]);
    t.ok(&["select-window", "-t", "a:0"]);
    t.ok(&["setw", "-g", "monitor-bell", "off"]);
    write_tty(&one, b"\x07");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(t.flags(), "0 *\n1 -\n");
    assert_eq!(t.lines().len(), 3);
}

#[test]
fn activity_alerts_once_and_only_in_another_window() {
    let t = Tmux::start();
    t.hook_all();
    t.ok(&["setw", "-g", "monitor-activity", "on"]);
    let (one, zero) = (t.tty("a:1"), t.tty("a:0"));
    write_tty(&one, b"x");
    t.wait_lines(1);
    write_tty(&one, b"y");
    write_tty(&zero, b"z");
    let both = wait_until(|| t.flags() == "0 ##*\n1 ##\n", Duration::from_secs(10));
    assert!(both.is_ok(), "{}", t.flags());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(t.lines(), ["alert-activity s=a w=@1 ctx=a:1.0 flags=##"]);
    // Selecting a window clears its flags, and counts as activity in it
    // (tmux's `session_set_current`): with no client attached that sets
    // the flag again at once, so later output alerts no more (recorded:
    // `0 ##*`, `1 ##-`, and no second line).
    t.ok(&["select-window", "-t", "a:1"]);
    t.ok(&["select-window", "-t", "a:0"]);
    assert_eq!(t.flags(), "0 ##*\n1 ##-\n");
    write_tty(&one, b"w");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(t.lines().len(), 1, "{:?}", t.lines());
}

#[test]
fn silence_alerts_once_after_monitor_silence_seconds() {
    let t = Tmux::start();
    t.hook_all();
    t.ok(&["setw", "-g", "monitor-silence", "1"]);
    let lines = t.wait_lines(1);
    assert_eq!(lines, ["alert-silence s=a w=@1 ctx=a:1.0 flags=~"]);
    assert_eq!(t.flags(), "0 ~*\n1 ~\n");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(t.lines().len(), 1, "{:?}", t.lines());
    // Output starts the wait again; 0 turns it off.
    t.ok(&["select-window", "-t", "a:1"]);
    t.ok(&["select-window", "-t", "a:0"]);
    write_tty(&t.tty("a:1"), b"x");
    assert_eq!(t.wait_lines(2).len(), 2);
    t.ok(&["setw", "-g", "monitor-silence", "0"]);
    t.ok(&["select-window", "-t", "a:1"]);
    t.ok(&["select-window", "-t", "a:0"]);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(t.lines().len(), 2);
}
