//! TC.13 cases 3 and 4, the checks with no tmux golden: the golden
//! container has no fzf and no Neovim, and what they check is grofe
//! driving the real program. Each asserts structure, and says what.
//!
//! - Case 3, `fzf --tmux`: athome's tsess picker runs in a pane of an
//!   attached pty client (the acceptance harness's shape: `new-pane`'s
//!   floating pane needs a client), opens through a real fzf's `--tmux`,
//!   is typed into, and its selection switches the client's session.
//!   Skipped, loudly, without an fzf that has `--tmux`. Where tsess's own
//!   `PATH` would find a real tmux ([`TSESS_PATH_TMUX`]), tsess's picker
//!   call runs from a shell line instead of the script, and says so.
//! - Case 4, vim-tmux-navigator: a headless Neovim in the right pane runs
//!   TP.2's copy of the plugin's `s:TmuxAwareNavigate` edge call, `tmux -S
//!   <socket from $TMUX> select-pane -t $TMUX_PANE -L`, plus the plugin's
//!   zoom probe, through the `tmux` symlink; the focus moves left.
//!   Skipped, loudly, without `nvim`.

use std::fs::{self, File};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};

use super::{Checkouts, TSESS_PATH_TMUX, tc13_cli};
use crate::replay::Cli;

const WAIT: Duration = Duration::from_secs(10);

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| s.to_string()).collect()
}

/// A command's stdout, or why it failed.
fn out(cli: &Cli, w: &[&str]) -> Result<String, String> {
    let (status, out, err) = cli.run(&words(w));
    if status.success() {
        Ok(out)
    } else {
        Err(format!("{w:?}: {err}"))
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) -> Result<(), String> {
    let deadline = Instant::now() + WAIT;
    while !done() {
        if Instant::now() > deadline {
            return Err(format!("timed out waiting for {what}"));
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// A program on the test process's own `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// An attach client on a pty, its output drained.
struct Attached {
    child: Child,
    _master: File,
}

impl Attached {
    fn spawn(cli: &Cli, session: &str) -> Result<Attached, String> {
        let size = Winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(&size, None).map_err(|e| e.to_string())?;
        let slave = |fd: &OwnedFd| Stdio::from(fd.try_clone().expect("dup pty slave"));
        let mut cmd = cli.command(&words(&["attach", "-t", session]));
        cmd.stdin(slave(&pty.slave))
            .stdout(slave(&pty.slave))
            .stderr(slave(&pty.slave));
        // SAFETY: setsid and ioctl are async-signal-safe; nothing allocates.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid()?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|e| e.to_string())?;
        drop(pty.slave);
        let master = File::from(pty.master);
        let mut reader = master.try_clone().map_err(|e| e.to_string())?;
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        });
        let attached = Attached {
            child,
            _master: master,
        };
        wait_until("a client attached", || {
            out(cli, &["list-clients"]).is_ok_and(|o| !o.is_empty())
        })?;
        Ok(attached)
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Both cases; every failure.
pub fn run(co: &Checkouts) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, result) in [("fzf --tmux", fzf_tsess(co)), ("nav", nav_from_nvim(co))] {
        match result {
            Ok(Some(skip)) => eprintln!("SKIPPED tc_13 {name}: {skip}"),
            Ok(None) => eprintln!("tc_13 {name}: done"),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    failures
}

/// Case 3. `Ok(Some(reason))` when skipped.
fn fzf_tsess(co: &Checkouts) -> Result<Option<String>, String> {
    let Some(fzf) = on_path("fzf") else {
        return Ok(Some("no fzf on PATH".into()));
    };
    let help = std::process::Command::new(&fzf)
        .arg("--help")
        .output()
        .map_err(|e| e.to_string())?;
    if !String::from_utf8_lossy(&help.stdout).contains("--tmux") {
        return Ok(Some(format!("{} has no --tmux", fzf.display())));
    }
    let Some(bash) = on_path("bash") else {
        return Ok(Some("no bash on PATH".into()));
    };
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let cli = tc13_cli(dir.path(), co);
    symlink(&fzf, dir.path().join("bin/fzf")).map_err(|e| e.to_string())?;
    // tsess puts TSESS_PATH_TMUX's directory first on its PATH; a tmux
    // there is a real one, not grofe. Then tsess's own fzf call, as the
    // script writes it, runs from a shell line instead, and the run says so.
    let picker = if Path::new(TSESS_PATH_TMUX).exists() {
        eprintln!(
            "tc_13 fzf --tmux: {TSESS_PATH_TMUX} would shadow grofe on tsess's PATH, so \
             tsess's picker call runs without the script"
        );
        "sel=$(tmux list-sessions -F '#S' | fzf --ansi --tmux center,70%,68% --reverse \
         --prompt='sessions  '); tmux switch-client -t \"$sel\"; echo tsess-exit-$?; exec cat"
            .to_string()
    } else {
        format!(
            "'{}' '{}'; echo tsess-exit-$?; exec cat",
            bash.display(),
            co.athome
                .join("home/dot_local/bin/executable_tsess")
                .display()
        )
    };
    let result = (|| {
        out(
            &cli,
            &["new-session", "-d", "-s", "aux", "-n", "aux", "cat"],
        )?;
        out(
            &cli,
            &[
                "new-session",
                "-d",
                "-s",
                "main",
                "-n",
                "main",
                "-x",
                "120",
                "-y",
                "40",
                "cat",
            ],
        )?;
        let _client = Attached::spawn(&cli, "main")?;
        out(&cli, &["respawn-pane", "-k", "-t", "main:1.1", &picker])?;
        let floating = || {
            out(
                &cli,
                &[
                    "list-panes",
                    "-t",
                    "main",
                    "-F",
                    "#{pane_floating_flag} #{pane_id}",
                ],
            )
            .ok()
            .and_then(|o| {
                o.lines()
                    .find_map(|l| l.strip_prefix("1 ").map(str::to_string))
            })
        };
        wait_until("fzf's floating pane (new-pane)", || floating().is_some())?;
        let pane = floating().ok_or("the floating pane went away")?;
        let screen = || out(&cli, &["capture-pane", "-p", "-t", &pane]).unwrap_or_default();
        wait_until("tsess's session list in fzf", || {
            let s = screen();
            s.contains("sessions") && s.contains("aux")
        })?;
        out(&cli, &["send-keys", "-t", &pane, "-l", "aux"])?;
        wait_until("fzf's filter", || !screen().contains("main"))?;
        out(&cli, &["send-keys", "-t", &pane, "Enter"])?;
        wait_until("the client to switch to aux", || {
            out(&cli, &["list-clients", "-F", "#{client_session}"]).is_ok_and(|o| o == "aux\n")
        })?;
        wait_until("tsess to exit 0", || {
            out(&cli, &["capture-pane", "-p", "-t", "main:1.1"])
                .is_ok_and(|o| o.contains("tsess-exit-0"))
        })?;
        wait_until("the floating pane to close", || floating().is_none())
    })();
    let _ = cli.run(&words(&["kill-server"]));
    result.map(|()| None)
}

/// TP.2's minimal Neovim init: the plugin's edge call and zoom probe, each
/// logged.
fn nav_init(log: &Path) -> String {
    format!(
        r##"set nocompatible
let g:log = '{log}'
function! Nav(dir) abort
  let nr = winnr()
  execute 'wincmd ' . a:dir
  if nr != winnr()
    call writefile(['vim ' . a:dir], g:log, 'a')
    return
  endif
  let socket = split($TMUX, ',')[0]
  let zoomed = trim(system('tmux -S ' . socket . ' display-message -p "#{{window_zoomed_flag}}"'))
  let args = 'select-pane -t ' . shellescape($TMUX_PANE) . ' -' . tr(a:dir, 'phjkl', 'lLDUR')
  let out = system('tmux -S ' . socket . ' ' . args)
  call writefile(['zoomed ' . zoomed, 'tmux ' . args . ' ' . v:shell_error . trim(out)], g:log, 'a')
endfunction
"##,
        log = log.display()
    )
}

/// Case 4. `Ok(Some(reason))` when skipped.
fn nav_from_nvim(co: &Checkouts) -> Result<Option<String>, String> {
    let Some(nvim) = on_path("nvim") else {
        return Ok(Some("no nvim on PATH".into()));
    };
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let cli = tc13_cli(dir.path(), co);
    let root = dir.path().join("nvim");
    for d in ["config", "data", "state", "cache"] {
        fs::create_dir_all(root.join(d)).map_err(|e| e.to_string())?;
    }
    let log = root.join("log");
    fs::write(root.join("init.vim"), nav_init(&log)).map_err(|e| e.to_string())?;
    let xdg = |d: &str| root.join(d).display().to_string();
    let command = format!(
        "exec env XDG_CONFIG_HOME='{}' XDG_DATA_HOME='{}' XDG_STATE_HOME='{}' XDG_CACHE_HOME='{}' \
         '{}' --headless -u '{}' --noplugin -i NONE -n -c \"call Nav('h')\" -c 'qa!'",
        xdg("config"),
        xdg("data"),
        xdg("state"),
        xdg("cache"),
        nvim.display(),
        root.join("init.vim").display(),
    );
    let result = (|| {
        out(
            &cli,
            &[
                "new-session",
                "-d",
                "-s",
                "main",
                "-n",
                "main",
                "-x",
                "120",
                "-y",
                "40",
                "cat",
            ],
        )?;
        let right = out(
            &cli,
            &[
                "split-window",
                "-h",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                "main:1",
                "cat",
            ],
        )?
        .trim()
        .to_string();
        out(&cli, &["respawn-pane", "-k", "-t", &right, &command])?;
        let want = format!("tmux select-pane -t '{right}' -L 0");
        wait_until("Neovim's select-pane call", || {
            fs::read_to_string(&log).is_ok_and(|l| l.lines().any(|l| l == want))
        })?;
        let log = fs::read_to_string(&log).unwrap_or_default();
        if !log.lines().any(|l| l == "zoomed 0") {
            return Err(format!("the zoom probe did not read 0:\n{log}"));
        }
        let active = out(
            &cli,
            &["display-message", "-p", "-t", "main:1", "#{pane_id}"],
        )?;
        if active.trim() != "%0" {
            return Err(format!(
                "focus is on {}, not the left pane %0",
                active.trim()
            ));
        }
        Ok(())
    })();
    let _ = cli.run(&words(&["kill-server"]));
    result.map(|()| None)
}

#[test]
fn nav_init_escapes_its_format_braces() {
    let init = nav_init(Path::new("/x/log"));
    assert!(init.contains("\"#{window_zoomed_flag}\""));
    assert!(init.contains("let g:log = '/x/log'"));
}
