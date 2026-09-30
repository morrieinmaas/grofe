//! FE.4: binding and overlay rows (`if-shell`, `command-alias`, popups,
//! menus, `new-pane` and `wait-for` for fzf, the mouse argv forms) against
//! goldens in `golden/fe_4/`, plus the cases a golden cannot drive: keys
//! pressed on an attached client, a real `fzf --tmux`, and Claude Code's
//! agent-teams calls over `-S`.
//!
//! Each golden is compared step by step, as FE.3 compares its own: every
//! step must equal its golden except those a scenario's divergence list
//! names, each with its reason and owner, and a listed step must still
//! differ, so the lists shrink as the gaps close.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use grofe::compat::tmux;
use grofe::protocol::{ClientCtx, Dialect};
use nix::pty::{Winsize, openpty};

use crate::fe_1::grofe_cli;
use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{Cli, parse_scenario, replay, split_words};

// ---- Goldens ---------------------------------------------------------------

/// A transcript split into steps: each step's first line (`$ ...`,
/// `client ...`, `screen ...`) with everything it recorded.
fn steps(transcript: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in transcript.split_inclusive('\n') {
        let starts = line.starts_with("$ ")
            || line.starts_with("$& ")
            || line.starts_with("client ")
            || line == "screen\n"
            || line.starts_with("screen ");
        match out.last_mut() {
            Some(step) if !starts => step.push_str(line),
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// The grofe side of `fe_4/<name>` against its golden, allowing the listed
/// steps (by their first line) to differ, and requiring that they do.
fn check(name: &str, diverges: &[(&str, &str)]) {
    let scenario = fixtures_dir().join(format!("golden/fe_4/{name}.scenario"));
    let parsed = parse_scenario(&fs::read_to_string(&scenario).unwrap()).unwrap();
    let golden = fs::read_to_string(scenario.with_extension("golden")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cli = with_client_ttys(grofe_cli(dir.path()), dir.path());
    let got = replay(&cli, &parsed).normalized(&Normalizer::load());
    let (want, got) = (steps(&golden), steps(&got));
    assert_eq!(
        want.len(),
        got.len(),
        "fe_4/{name}: step count\n{}",
        got.join("")
    );
    let listed: BTreeMap<&str, &str> = diverges.iter().copied().collect();
    let mut wrong = Vec::new();
    let mut stale = Vec::new();
    for (w, g) in want.iter().zip(&got) {
        let head = w.lines().next().unwrap_or_default();
        match (listed.contains_key(head), w == g) {
            (false, false) => wrong.push(format!("--- golden\n{w}+++ grofe\n{g}")),
            (true, true) => stale.push(head.to_string()),
            _ => {}
        }
    }
    for head in listed.keys() {
        assert!(
            want.iter().any(|w| w.lines().next() == Some(head)),
            "fe_4/{name}: listed step {head:?} is not in the scenario"
        );
    }
    assert!(
        wrong.is_empty(),
        "fe_4/{name} differs:\n{}",
        wrong.join("\n")
    );
    assert!(
        stale.is_empty(),
        "fe_4/{name}: listed steps now match: {stale:#?}"
    );
}

/// `cli` behind a wrapper that answers the replayer's attach check,
/// `list-clients -F '#{client_tty}'`, with the ttys of this run's attach
/// clients once the server lists a client, since grofe's `client_tty` is
/// still empty (FE.2's). Every other command reaches grofe as it is.
// ponytail: harness only, the same shim as FE.3's; drop it once FE.2 fills
// client_tty.
fn with_client_ttys(mut cli: Cli, dir: &Path) -> Cli {
    let wrap = dir.join("wrap");
    fs::create_dir_all(&wrap).unwrap();
    let script = wrap.join("tmux");
    let grofe = cli.program.display().to_string();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *'list-clients -F #{{client_tty}}')\n\
             \x20 [ \"$('{grofe}' \"$@\" | wc -l)\" -gt 0 ] || exit 0\n\
             \x20 ps -A -o tty= -o args= | awk -v g='{grofe}' 'index($0, g \" \") && / attach/ {{ print \"/dev/\" $1 }}'\n\
             \x20 exit 0 ;;\n\
             esac\n\
             exec '{grofe}' \"$@\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    cli.program = script;
    cli
}

/// Case 1: vim-tmux-navigator's `if -F` C-h test from the CLI (the
/// pressed binding is [`a_pressed_c_h_binding_behaves_as_in_tmux`]), the
/// shell form, false and empty conditions, `-b`, and a branch of `neww`
/// and `setw`.
#[test]
fn if_format_matches_its_golden() {
    check("if-format", &[]);
}

/// Case 6: `send -M` is tmux's `no mouse target`; `resize-pane -M` and
/// `copy-mode -M` do nothing and exit 0, as in tmux 3.7b.
#[test]
fn mouse_forms_match_their_golden() {
    check(
        "mouse-forms",
        &[
            (
                "$ display-menu -x M -y M A a 'display hi'",
                "refused on purpose (M36-R5): tmux says `no current client` only because no \
                 client is attached; with one it draws the menu at the top left",
            ),
            (
                "$ if -F -t = 1 'display -p yes'",
                "a bare `=` outside a mouse key is `no mouse target` (M36-R3); tmux falls back \
                 to the current pane",
            ),
        ],
    );
}

/// Case 2's argv: fzf 0.74's `--tmux` calls with a script in fzf's place
/// (the golden container has no fzf). [`fzf_tmux_selection_reaches_the_caller`]
/// runs the real one.
#[test]
fn fzf_argv_matches_its_golden() {
    check("fzf-argv", &[]);
}

/// Claude Code's agent-teams sequence, as the tuios shim test pins it.
#[test]
fn claude_teams_match_their_golden() {
    check("claude-teams", &[]);
}

/// `remain-on-exit failed`, which Claude Code sets on each teammate pane.
#[test]
fn remain_on_exit_failed_matches_its_golden() {
    check("remain-failed", &[]);
}

/// `command-alias`, with tmux's six default entries under tmux identity.
#[test]
fn aliases_match_their_golden() {
    check("aliases", &[]);
}

/// Case 5: tmux's columns, key order, stock prefix table and command
/// printing; one binding goes to a client's status line, not the output.
#[test]
fn list_keys_matches_its_golden() {
    check("list-keys", &[]);
}

/// Case 3: a genuine popup's environment.
#[test]
fn popup_env_matches_its_golden() {
    check(
        "popup-env",
        &[(
            "$ run-shell 'cat \"$HOME/popup-env\"'",
            "the popup's env: see popup_env_is_tmux_s for the part that must match",
        )],
    );
}

/// Variables a grofe popup has beyond tmux's: grofe's own (`GROFE`, and
/// `GROFE_PANE`, the caller's pane, TC.6), the test harness's (the XDG
/// dirs and `GROFE_RESTORE_ON_START`, which the server inherits), and
/// `SHLVL`, which macOS's `/bin/sh` sets and the container's dash does not.
const POPUP_ENV_EXTRA: &[&str] = &[
    "GROFE",
    "GROFE_PANE",
    "GROFE_RESTORE_ON_START",
    "SHLVL",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
];

/// Case 3, the part of a popup's environment that must be tmux's: every
/// name tmux sets, `TMUX` naming the server and the session, no
/// `TMUX_PANE`, and nothing else but [`POPUP_ENV_EXTRA`]. Values of the
/// recorder's fixed variables (`PATH`, `PWD`) differ between the container
/// and a host run.
#[test]
fn popup_env_is_tmux_s() {
    let golden = fs::read_to_string(fixtures_dir().join("golden/fe_4/popup-env.golden")).unwrap();
    let keys = |text: &str| -> BTreeSet<String> {
        text.split("$ run-shell 'cat \"$HOME/popup-env\"'\n")
            .nth(1)
            .unwrap_or_default()
            .lines()
            .take_while(|l| !l.starts_with("? "))
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.to_string()))
            .collect()
    };
    let scenario = fixtures_dir().join("golden/fe_4/popup-env.scenario");
    let parsed = parse_scenario(&fs::read_to_string(&scenario).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cli = with_client_ttys(grofe_cli(dir.path()), dir.path());
    let got = replay(&cli, &parsed).normalized(&Normalizer::load());
    let (got_keys, want) = (keys(&got), keys(&golden));
    let missing: Vec<&String> = want.difference(&got_keys).collect();
    let extra: Vec<&String> = got_keys
        .difference(&want)
        .filter(|k| !POPUP_ENV_EXTRA.contains(&k.as_str()))
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "missing {missing:?}, extra {extra:?}\n{got}"
    );
    assert!(got.contains("TMUX=<SOCKET>,<PID>,0\n"), "{got}");
    assert!(!got_keys.contains("TMUX_PANE"), "{got}");
}

/// Case 4: grofe draws `-b double` as tmux does; `-s` and `-S` are
/// ignored with a log line (the screen's text is tmux's, colours aside).
#[test]
fn popup_styles_match_their_golden() {
    check("popup-styles", &[]);
}

/// A menu opened while a popup is open does nothing and exits 0.
#[test]
fn menu_over_popup_matches_its_golden() {
    check("menu-over-popup", &[]);
}

// ---- The miss log ----------------------------------------------------------

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| s.to_string()).collect()
}

fn miss_log(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("state/grofe/compat-miss.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.split_once(' ').map_or(l, |(_, what)| what).to_string())
        .collect()
}

/// Case 6: `send -M` fails with tmux's text, and one line in the log.
#[test]
fn send_m_fails_with_tmux_s_text_and_a_log_line() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    let (status, _, err) = cli.run(&words(&["new-session", "-d", "-s", "main"]));
    assert!(status.success(), "{err}");
    let before = miss_log(dir.path()).len();
    let (status, out, err) = cli.run(&words(&["send", "-M"]));
    assert_eq!((status.code(), out.as_str()), (Some(1), ""));
    assert_eq!(err, "no mouse target\n");
    assert_eq!(miss_log(dir.path())[before..], ["send-keys -M".to_string()]);
    let _ = cli.run(&words(&["kill-server"]));
}

/// Case 4: a style flag grofe draws no differently is ignored, with
/// exactly one log line naming it (the `popup-styles` golden has the
/// screen, and tmux's error for an unknown `-b`).
#[test]
fn popup_style_flags_are_ignored_with_one_log_line() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    let (status, _, err) = cli.run(&words(&["new-session", "-d", "-s", "main"]));
    assert!(status.success(), "{err}");
    let before = miss_log(dir.path()).len();
    // No client: the popup fails on the server, after translating, as
    // tmux's does.
    let (_, _, err) = cli.run(&words(&[
        "display-popup",
        "-S",
        "fg=red",
        "-S",
        "fg=blue",
        "-b",
        "double",
        "-E",
        "true",
    ]));
    assert_eq!(err, "no current client\n");
    assert_eq!(
        miss_log(dir.path())[before..],
        ["display-popup -S: cosmetic, ignored".to_string()],
        "one line for the flag, however often it is given"
    );
    let _ = cli.run(&words(&["kill-server"]));
}

// ---- Keys pressed on an attached client ------------------------------------

/// An attach client on a pty, for pressing keys.
struct Attached {
    child: Child,
    master: File,
}

impl Attached {
    fn spawn(cli: &Cli, session: &str) -> Attached {
        let size = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(&size, None).expect("openpty");
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
        let child = cmd.spawn().expect("spawn attach");
        drop(pty.slave);
        let master = File::from(pty.master);
        let mut reader = master.try_clone().expect("dup pty master");
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        });
        wait_until("a client attached", || {
            !cli.run(&words(&["list-clients"])).1.is_empty()
        });
        Attached { child, master }
    }

    fn press(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("write keys");
        // Past escape-time, so each press stands alone.
        thread::sleep(Duration::from_millis(150));
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn out(cli: &Cli, w: &[&str]) -> String {
    let (status, out, err) = cli.run(&words(w));
    assert!(status.success(), "{w:?}: {err}");
    out
}

/// Case 1, pressed: the owner's C-h binding, bound in the tmux dialect.
/// The right pane runs a process named nvim and gets the key;
/// from the middle pane (a plain cat) the focus moves left. tmux 3.7b does
/// the same (recorded on `-L grofe-fe4 -f /dev/null`, and the CLI form is
/// the `if-format` golden).
#[test]
fn a_pressed_c_h_binding_behaves_as_in_tmux() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    out(
        &cli,
        &[
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            "80",
            "-y",
            "24",
            "cat",
        ],
    );
    out(&cli, &["split-window", "-d", "-h", "-t", "main", "cat"]);
    // A process named nvim: a copy of grofe, waiting on a channel. A
    // symlink would not do (macOS names the process after its target), nor
    // a copy of a system binary (macOS kills it).
    let nvim = dir.path().join("home/nvim");
    fs::copy(env!("CARGO_BIN_EXE_grofe"), &nvim).unwrap();
    out(
        &cli,
        &[
            "split-window",
            "-d",
            "-h",
            "-t",
            "main:1.2",
            &format!("exec '{}' wait-for fe4-nvim", nvim.display()),
        ],
    );
    wait_until("the nvim pane", || {
        out(
            &cli,
            &["display", "-p", "-t", "main:1.3", "#{pane_current_command}"],
        ) == "nvim\n"
    });
    out(
        &cli,
        &[
            "bind",
            "-n",
            "C-h",
            "if",
            "-F",
            "#{m/r:^n?vim$,#{pane_current_command}}",
            "send C-h",
            "select-pane -L",
        ],
    );
    let mut client = Attached::spawn(&cli, "main");
    let active = || out(&cli, &["display", "-p", "-t", "main", "#{pane_index}"]);

    out(&cli, &["select-pane", "-t", "main:1.3"]);
    client.press(b"\x08");
    wait_until("C-h in the nvim pane", || {
        out(&cli, &["capture-pane", "-p", "-t", "main:1.3"]).contains("^H")
    });
    assert_eq!(active(), "3\n", "nvim keeps the focus");

    out(&cli, &["select-pane", "-t", "main:1.2"]);
    client.press(b"\x08");
    wait_until("the focus moved left", || active() == "1\n");
    assert!(
        !out(&cli, &["capture-pane", "-p", "-t", "main:1.2"]).contains("^H"),
        "a plain pane never sees the key"
    );
    drop(client);
    let _ = cli.run(&words(&["kill-server"]));
}

/// Case 7: a `tmux bind` whose command uses `neww`, `setw` and `ls` runs
/// each command, in the tmux dialect, when pressed.
#[test]
fn a_pressed_tmux_binding_with_neww_setw_and_ls_runs() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    out(
        &cli,
        &["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"],
    );
    out(&cli, &["bind", "y", "neww; setw automatic-rename off; ls"]);
    let mut client = Attached::spawn(&cli, "main");
    client.press(b"\x02y");
    wait_until("the new window", || {
        out(
            &cli,
            &["list-windows", "-t", "main", "-F", "#{window_index}"],
        ) == "1\n2\n"
    });
    wait_until("setw on the new window", || {
        out(&cli, &["show", "-wv", "-t", "main:2", "automatic-rename"]) == "off\n"
    });
    assert_eq!(
        out(&cli, &["show", "-wv", "-t", "main:1", "automatic-rename"]),
        "",
        "setw ran after neww, on the new window only"
    );
    drop(client);
    let _ = cli.run(&words(&["kill-server"]));
}

/// Case 8: a binding from a native config opens a `command-prompt` whose
/// template is native, on a server with tmux identity: the template keeps
/// the native dialect (spec §4.5), so its `-s` is grofe's session scope,
/// where tmux's `-s` would be the server table.
#[test]
fn a_native_config_prompt_template_keeps_its_dialect() {
    let dir = tempfile::tempdir().unwrap();
    let mut cli = grofe_cli(dir.path());
    let config = dir.path().join("native.toml");
    let defaults =
        fs::read_to_string(fixtures_dir().join("golden/fe_1/grofe-defaults.toml")).unwrap();
    fs::write(
        &config,
        format!(
            "{defaults}\n[keys.prefix]\ny = \"command-prompt -p name \\\"set-option -s @picked '%%'\\\"\"\n"
        ),
    )
    .unwrap();
    let at = cli.prefix.iter().position(|a| a == "-f").unwrap();
    cli.prefix[at + 1] = config.display().to_string();
    out(
        &cli,
        &["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"],
    );
    assert_eq!(out(&cli, &["display", "-p", "#{version}"]), "3.7b\n");
    let mut client = Attached::spawn(&cli, "main");
    client.press(b"\x02y");
    client.press(b"hello\r");
    wait_until("the prompt's command", || {
        cli.run(&words(&["show", "-v", "-t", "main", "@picked"])).1 == "hello\n"
    });
    assert_eq!(
        cli.run(&words(&["show", "-sv", "@picked"])).1,
        "",
        "not tmux's server table"
    );
    drop(client);
    let _ = cli.run(&words(&["kill-server"]));
}

/// A tmux binding's `command-prompt -I -p` template and `confirm-before -p`
/// command are tmux dialect, stored as sent: `neww`, `renamew` and `%%`
/// run as tmux runs them, and only `y` confirms.
#[test]
fn a_tmux_binding_s_prompt_and_confirm_run_in_tmux_s_dialect() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    out(
        &cli,
        &["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"],
    );
    out(
        &cli,
        &[
            "bind",
            "y",
            "command-prompt -I pre -p name 'neww -n \"%%\"'",
        ],
    );
    out(
        &cli,
        &[
            "bind",
            "u",
            "confirm-before -p 'rename? ' 'renamew -t :1 confirmed'",
        ],
    );
    let mut client = Attached::spawn(&cli, "main");
    let names = || {
        out(
            &cli,
            &["list-windows", "-t", "main", "-F", "#{window_name}"],
        )
    };
    client.press(b"\x02y");
    client.press(b"-x\r");
    wait_until("the prompt's window", || {
        names().lines().any(|n| n == "pre-x")
    });
    client.press(b"\x02u");
    client.press(b"n");
    thread::sleep(Duration::from_millis(300));
    assert!(!names().contains("confirmed"), "n cancels: {}", names());
    client.press(b"\x02u");
    client.press(b"y");
    wait_until("the confirmed rename", || {
        names().starts_with("confirmed\n")
    });
    drop(client);
    let _ = cli.run(&words(&["kill-server"]));
}

/// tmux-which-key's root menu, as its `init.tmux` sets it up: the menu
/// command built with `set -F` into a `command-alias` entry, bound to a
/// prefix key by the alias's name. Pressed, the menu opens and its item's
/// command runs (in the tmux dialect: `menu`, `set -g`).
#[test]
fn which_key_s_alias_binding_opens_its_menu() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    out(
        &cli,
        &["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"],
    );
    out(
        &cli,
        &["set", "-g", "@wk_cmd_show", "menu -x R -y P -T which-key"],
    );
    out(
        &cli,
        &["set", "-g", "@wk_menu_root", "Run r 'set -g @wk-ran yes'"],
    );
    out(
        &cli,
        &[
            "set",
            "-g",
            "-F",
            "command-alias[201]",
            "show-wk-menu-root=#{@wk_cmd_show} #{@wk_menu_root}",
        ],
    );
    out(
        &cli,
        &["bind-key", "-T", "prefix", "Space", "show-wk-menu-root"],
    );
    let mut client = Attached::spawn(&cli, "main");
    client.press(b"\x02 ");
    client.press(b"r");
    wait_until("the menu item's command", || {
        cli.run(&words(&["show", "-gv", "@wk-ran"])).1 == "yes\n"
    });
    drop(client);
    let _ = cli.run(&words(&["kill-server"]));
}

// ---- fzf --tmux (M36-R1) ---------------------------------------------------

/// The fzf on PATH, if it has `--tmux`.
fn fzf_with_tmux() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let fzf = std::env::split_paths(&path)
        .map(|d| d.join("fzf"))
        .find(|p| p.is_file())?;
    let help = std::process::Command::new(&fzf)
        .arg("--help")
        .output()
        .ok()?;
    String::from_utf8_lossy(&help.stdout)
        .contains("--tmux")
        .then_some(fzf)
}

/// Case 2: a real `fzf --tmux` run from a pane opens through `new-pane`
/// (a floating pane), is typed into there, and its selection reaches the
/// caller. Skipped when the installed fzf lacks `--tmux`.
#[test]
fn fzf_tmux_selection_reaches_the_caller() {
    let Some(fzf) = fzf_with_tmux() else {
        eprintln!("skipped: no fzf with --tmux on PATH");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    let picked = dir.path().join("picked");
    let script = dir.path().join("pick.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf 'apple\\nbanana\\ncherry\\n' | '{}' --tmux center,50%,50% > '{}.tmp'\n\
             echo $? >> '{}.tmp'\nmv '{}.tmp' '{}'\nexec cat\n",
            fzf.display(),
            picked.display(),
            picked.display(),
            picked.display(),
            picked.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    out(
        &cli,
        &[
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            "120",
            "-y",
            "40",
            &script.display().to_string(),
        ],
    );
    let client = Attached::spawn(&cli, "main");
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
        .lines()
        .find_map(|l| l.strip_prefix("1 ").map(str::to_string))
    };
    wait_until("fzf's floating pane", || floating().is_some());
    let pane = floating().unwrap();
    wait_until("fzf's list", || {
        out(&cli, &["capture-pane", "-p", "-t", &pane]).contains("cherry")
    });
    out(&cli, &["send-keys", "-t", &pane, "-l", "ban"]);
    wait_until("fzf's filter", || {
        !out(&cli, &["capture-pane", "-p", "-t", &pane]).contains("cherry")
    });
    out(&cli, &["send-keys", "-t", &pane, "Enter"]);
    wait_until("the selection", || picked.exists());
    assert_eq!(fs::read_to_string(&picked).unwrap(), "banana\n0\n");
    wait_until("the floating pane closed", || floating().is_none());
    drop(client);
    let _ = cli.run(&words(&["kill-server"]));
}

// ---- Claude Code agent teams -----------------------------------------------

/// Claude Code 2.1's TmuxBackend, argv for argv as tuios's
/// `TestClaudeCodeTeammateSequence` pins it, over `-S $socket` as Claude
/// Code sends it: the ids come back in tmux's shape, the teammate pane
/// never takes the focus, and `respawn-pane -k` replaces the placeholder
/// with the teammate's command.
#[test]
fn claude_code_teammate_sequence_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut cli = grofe_cli(dir.path());
    let socket = cli
        .vars
        .iter()
        .find(|(k, _)| k == "socket")
        .map(|(_, v)| v.clone())
        .unwrap();
    out(
        &cli,
        &[
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            "200",
            "-y",
            "50",
            "cat",
        ],
    );
    cli.prefix = vec!["-S".into(), socket];
    let leader = "%0";
    assert_eq!(
        out(
            &cli,
            &["display-message", "-t", leader, "-p", "#{window_id}"]
        ),
        "@0\n"
    );
    assert_eq!(
        out(&cli, &["list-panes", "-t", "@0", "-F", "#{pane_id}"]),
        "%0\n"
    );
    let teammate = out(
        &cli,
        &[
            "split-window",
            "-d",
            "-t",
            leader,
            "-h",
            "-l",
            "70%",
            "-P",
            "-F",
            "#{pane_id}",
            "--",
            "cat",
        ],
    );
    assert_eq!(teammate, "%1\n");
    let teammate = teammate.trim_end();
    for w in [
        &[
            "set-option",
            "-p",
            "-t",
            teammate,
            "window-style",
            "bg=default,fg=blue",
        ][..],
        &[
            "set-option",
            "-p",
            "-t",
            teammate,
            "pane-border-style",
            "fg=blue",
        ],
        &[
            "set-option",
            "-p",
            "-t",
            teammate,
            "pane-active-border-style",
            "fg=blue",
        ],
        &["select-pane", "-t", teammate, "-T", "researcher"],
        &[
            "set-option",
            "-p",
            "-t",
            teammate,
            "pane-border-format",
            "#[fg=blue,bold] #{pane_title} #[default]",
        ],
        &["select-layout", "-t", "@0", "main-vertical"],
        &["resize-pane", "-t", leader, "-x", "30%"],
    ] {
        out(&cli, w);
    }
    out(
        &cli,
        &[
            "set-option",
            "-p",
            "-t",
            teammate,
            "remain-on-exit",
            "failed",
        ],
    );
    assert_eq!(
        out(
            &cli,
            &[
                "list-panes",
                "-t",
                "@0",
                "-F",
                "#{pane_id} #{pane_active} #{pane_title}"
            ]
        )
        .lines()
        .skip(1)
        .collect::<Vec<_>>(),
        ["%1 0 researcher"],
        "the teammate is named and never took the focus"
    );
    let command = "echo teammate-started; exec cat";
    out(&cli, &["respawn-pane", "-k", "-t", teammate, "--", command]);
    wait_until("the teammate's command", || {
        out(&cli, &["capture-pane", "-p", "-t", teammate]).starts_with("teammate-started")
    });
    out(&cli, &["kill-pane", "-t", teammate]);
    assert_eq!(
        out(&cli, &["list-panes", "-t", "@0", "-F", "#{pane_id}"]),
        "%0\n"
    );
    let _ = cli.run(&words(&["kill-server"]));
}

// ---- Coverage --------------------------------------------------------------

/// The coverage doc's areas FE.4 owns the rows of.
const FE4_AREAS: &[&str] = &["area:Bindings", "area:Overlays", "oos:Mouse argv forms"];

/// Forms in FE.4's areas whose rows are another task's, each with why.
const ELSEWHERE: &[(&str, &str)] = &[
    (
        "bind-key",
        "FE.1's pass-through row; the stored command is checked in its own dialect",
    ),
    (
        "command-prompt",
        "FE.1's pass-through row (M3); `-i` runs on Enter only until M5",
    ),
    ("list-commands", "FE.1's row (M36-R1)"),
    (
        "list-keys",
        "FE.1's pass-through row; its output is TC.7's handler",
    ),
    (
        "copy-mode",
        "M5 (M36-R8), FE.1's refusal; `-M` does nothing, as in tmux",
    ),
    (
        "send-keys",
        "FE.1's rows: `-M` is tmux's text, `-X` is M5's",
    ),
    ("resize-pane", "FE.1's row: `-M` does nothing, as in tmux"),
    (
        "kill-pane",
        "FE.1's pass-through; `-t =` resolves in a mouse key",
    ),
    (
        "select-pane",
        "FE.1's pass-through; `-t =` resolves in a mouse key",
    ),
    (
        "switch-client",
        "FE.1's pass-through; `-t =` resolves in a mouse key (M36-R3)",
    ),
];

/// FE.4-area forms that do not translate yet, each with the owner of the
/// missing piece. Every other one must map or be refused with a reason.
const PENDING: &[(&str, &str)] = &[];

fn ctx() -> ClientCtx {
    ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Tmux,
    }
}

/// `line` with its placeholders (`<...>`, `%N`, `@N`) made concrete, as
/// FE.1's corpus replay does, and [`MANY_WORDS`] replaced first.
fn concrete(line: &str) -> String {
    let mut line = line.to_string();
    for (placeholder, words) in MANY_WORDS {
        line = line.replace(placeholder, words);
    }
    let mut out = String::new();
    let mut rest = line.as_str();
    while let Some(start) = rest.find('<') {
        let end = rest[start..].find('>').map(|e| start + e);
        match end {
            Some(end) if !rest[start + 1..end].contains([' ', '<']) && end > start + 1 => {
                out.push_str(&rest[..start]);
                out.push('1');
                rest = &rest[end + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out.replace("%N", "%1").replace("@N", "@1")
}

/// Placeholders that stand for more than one word, as which-key's menu
/// items (`<$@>`), each with the words a real call sends in its place.
const MANY_WORDS: &[(&str, &str)] = &[("<$@>", "Item a 'display-message x'")];

/// A command and the commands it stores: a `bind-key`'s command, an
/// `if-shell`'s branches, a menu's items, a `confirm-before`'s command, a
/// `command-prompt`'s template.
fn with_stored(cmd: Vec<String>, out: &mut Vec<Vec<String>>) {
    let form = tmux::form_of(&cmd).unwrap_or_default();
    let verb = form.split(' ').next().unwrap_or_default();
    let skip_flags = |from: usize, values: &[&str]| {
        let mut i = from;
        while let Some(w) = cmd.get(i) {
            match w.as_str() {
                "--" => return i + 1,
                w if values.contains(&w) => i += 2,
                w if w.starts_with('-') && w.len() > 1 => i += 1,
                _ => break,
            }
        }
        i
    };
    let strings: Vec<String> = match verb {
        "bind-key" => {
            let at = skip_flags(1, &["-T", "-N"]) + 1;
            match cmd.get(at..).unwrap_or_default() {
                [] => Vec::new(),
                [one] => vec![one.clone()],
                many => {
                    for c in grofe::cmd::parse::split_argv(many) {
                        with_stored(c, out);
                    }
                    Vec::new()
                }
            }
        }
        "if-shell" => cmd
            .get(skip_flags(1, &["-t"]) + 1..)
            .unwrap_or_default()
            .to_vec(),
        "confirm-before" => cmd
            .get(skip_flags(1, &["-p", "-c", "-t"])..)
            .unwrap_or_default()
            .to_vec(),
        "command-prompt" => cmd
            .get(skip_flags(1, &["-I", "-p", "-t", "-T"])..)
            .unwrap_or_default()
            .to_vec(),
        "display-menu" => {
            let items = cmd
                .get(
                    skip_flags(
                        1,
                        &["-T", "-x", "-y", "-t", "-c", "-b", "-H", "-s", "-S", "-C"],
                    )..,
                )
                .unwrap_or_default();
            let mut commands = Vec::new();
            let mut rest = items;
            while let Some((name, tail)) = rest.split_first() {
                if name.is_empty() {
                    rest = tail;
                    continue;
                }
                if let [_, command, tail @ ..] = tail {
                    commands.push(command.clone());
                    rest = tail;
                } else {
                    break;
                }
            }
            commands
        }
        _ => Vec::new(),
    };
    out.push(cmd);
    for s in strings {
        for c in grofe::cmd::split_command_string(&s).unwrap_or_default() {
            with_stored(c, out);
        }
    }
}

/// Every command in the six corpus files, and those they store.
fn corpus_commands() -> Vec<Vec<String>> {
    let corpus = fixtures_dir().join("../../compat/corpus");
    let mut out = Vec::new();
    for file in [
        "static-cli",
        "static-conf",
        "container-cli",
        "container-pane",
        "container-job",
        "container-stored",
    ] {
        let text = fs::read_to_string(corpus.join(format!("{file}.argv"))).unwrap();
        let conf = matches!(file, "static-conf" | "container-stored");
        for line in text.lines() {
            let line = concrete(line);
            let cmds = if conf {
                grofe::cmd::split_command_string(&line).unwrap_or_default()
            } else {
                match split_words(&line).map(|w| tmux::cli(&w)) {
                    Ok(Ok(cli)) if !cli.version => grofe::cmd::parse::split_argv(&cli.argv),
                    _ => Vec::new(),
                }
            };
            for cmd in cmds {
                with_stored(cmd, &mut out);
            }
        }
    }
    out
}

/// The coverage doc's forms with the spec row each is in.
fn coverage_forms() -> BTreeMap<String, String> {
    let doc = fs::read_to_string(fixtures_dir().join("../../../docs/compat-coverage.md")).unwrap();
    let forms = doc
        .split("\n### Forms\n")
        .nth(1)
        .and_then(|t| t.split("\n### ").next())
        .unwrap();
    forms
        .lines()
        .filter(|l| l.starts_with("| `"))
        .filter_map(|row| {
            let form = row.split('`').nth(1)?.to_string();
            Some((form, row.split(" | ").nth(4)?.to_string()))
        })
        .collect()
}

/// Every corpus form in FE.4's areas either maps (some corpus command of
/// that form translates) or is refused on purpose, with a reason, by the
/// row that decides it. The owner's TPM plugins run unchanged (owner
/// ruling 2026-09-29): nothing in these areas is left pending. The
/// counts go to the FE.4 report (`--nocapture`).
#[test]
fn every_fe4_corpus_form_maps_or_is_refused_with_a_reason() {
    let areas = coverage_forms();
    let mut results: BTreeMap<String, Result<(), String>> = BTreeMap::new();
    for cmd in corpus_commands() {
        let Some(form) = tmux::form_of(&cmd) else {
            continue;
        };
        if !areas
            .get(&form)
            .is_some_and(|a| FE4_AREAS.contains(&a.as_str()))
        {
            continue;
        }
        let got = tmux::dispatch_command(&cmd, &ctx())
            .map(|_| ())
            .map_err(|e| format!("{cmd:?}: {e}"));
        let refused = tmux::row_for(&cmd).is_some_and(|r| {
            matches!(
                r.target,
                tmux::GrofeTarget::Unsupported(_) | tmux::GrofeTarget::Refuse(_)
            )
        });
        let got = match got {
            Err(_) if refused => Err("refused".to_string()),
            other => other,
        };
        let entry = results.entry(form).or_insert_with(|| got.clone());
        if got.is_ok() || (got == Err("refused".to_string()) && entry.is_err()) {
            *entry = got;
        }
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut pending = Vec::new();
    for (form, got) in &results {
        let verb = form.split(' ').next().unwrap_or_default();
        let standing = match got {
            Ok(()) => "mapped",
            Err(e) if e == "refused" => "refused",
            Err(_) if PENDING.iter().any(|(f, _)| f == form) => "pending",
            Err(e) => {
                pending.push(format!("{form}: {e}"));
                "pending"
            }
        };
        let owner = if ELSEWHERE.iter().any(|(v, _)| *v == verb) {
            "elsewhere"
        } else {
            "fe4"
        };
        *counts.entry(standing).or_default() += 1;
        *counts.entry(owner).or_default() += 1;
    }
    let seen: BTreeSet<&str> = results.keys().map(String::as_str).collect();
    let unseen: Vec<&String> = areas
        .iter()
        .filter(|(f, a)| FE4_AREAS.contains(&a.as_str()) && !seen.contains(f.as_str()))
        .map(|(f, _)| f)
        .collect();
    println!("FE.4 corpus forms: {counts:?}; forms no corpus command reaches: {unseen:?}");
    assert!(results.len() > 40, "only {} FE.4 forms", results.len());
    assert!(pending.is_empty(), "FE.4 forms left pending: {pending:#?}");
    let stale: Vec<&str> = PENDING
        .iter()
        .map(|(f, _)| *f)
        .filter(|f| results.get(*f).is_some_and(|r| r.is_ok()))
        .collect();
    assert!(stale.is_empty(), "PENDING forms that map now: {stale:?}");
}

/// The mouse position letters are refused however the menu or popup
/// otherwise looks, and the ordinary letters (`C`, `P`, `R`) map.
#[test]
fn mouse_positions_are_semantic_and_the_other_letters_map() {
    let run = |w: &[&str]| tmux::dispatch_command(&words(w), &ctx()).map(|_| ());
    for (flag, letter) in [
        ("-x", "M"),
        ("-y", "M"),
        ("-x", "W"),
        ("-y", "W"),
        ("-y", "S"),
    ] {
        for verb in ["display-menu", "display-popup"] {
            let err = run(&[verb, "-T", "t", flag, letter, "A", "a", "x"]).unwrap_err();
            assert!(
                err.0.contains("mouse position"),
                "{verb} {flag} {letter}: {err}"
            );
        }
    }
    for (x, y) in [("C", "C"), ("P", "P"), ("R", "P")] {
        assert!(run(&["display-menu", "-T", "t", "-x", x, "-y", y, "A", "a", "x"]).is_ok());
        assert!(run(&["display-popup", "-x", x, "-y", y, "-E", "true"]).is_ok());
    }
    assert!(
        run(&[
            "display-menu",
            "-O",
            "-b",
            "double",
            "-H",
            "bg=red",
            "A",
            "a",
            "x"
        ])
        .is_ok()
    );
    assert!(
        run(&["wait-for", "-L", "x"])
            .unwrap_err()
            .0
            .contains("not supported")
    );
}
