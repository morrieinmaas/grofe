//! TP.2: vim-tmux-navigator through grofe's native conditional.
//!
//! canopy's generated `40-canopy-nav.toml` (the copy TP.1's
//! `check-canopy-port.sh` keeps current) is loaded into a test server as a
//! conf.d layer, and the keys are typed into an attached client:
//!
//! - Case 1: from a shell, C-h and C-l move focus.
//! - Cases 2 to 4: with Neovim in front the key reaches Neovim, also when
//!   Neovim runs under a file name the plugin's pattern does not match, and
//!   Neovim at its edge moves focus through its own `tmux` call.
//! - Case 5: zoomed, the default unzooms and moves, as with the plugin on
//!   tmux; the plugin's `@tmux_navigator_disable_when_zoomed` keeps focus.
//! - Case 6: the keys fork nothing, counted from the kernel's usage figures
//!   for the server's reaped children.
//!
//! Neovim runs with a minimal init holding the plugin's own
//! `s:TmuxAwareNavigate` logic (tmux_navigator.vim: `wincmd`, and at the
//! edge `tmux -S <socket from $TMUX> select-pane -t $TMUX_PANE -L`), with a
//! log, and with every XDG directory in the test's temp dir. The `tmux` it
//! calls is a symlink to grofe.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use common::{TestServer, wait_until};

use crate::harness::{Harness, require_tool};

/// canopy's nav fragment, the copy of its `grofe/conf.d/40-canopy-nav.toml`
/// that `check-canopy-port.sh` keeps current.
const NAV: &str = include_str!("../fixtures/canopy-port/conf.d/40-canopy-nav.toml");

const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const WAIT: Duration = Duration::from_secs(10);

/// The pane shell, with a prompt of its own: `sh` as root prompts `# `.
const SH: &str = "exec env PS1='grofe$ ' sh";
const PROMPT: &str = "grofe$ ";

const C_H: &[u8] = b"\x08";
const C_L: &[u8] = b"\x0c";

/// A server with canopy's nav fragment loaded, and one session `main`
/// split side by side, both panes running `left` and `right`: the left
/// pane, the right pane (focused).
fn start(left: &str, right: &str) -> (TestServer, String, String) {
    let srv = TestServer::start();
    let conf = srv.config_home.join("grofe/conf.d");
    fs::create_dir_all(&conf).expect("conf.d");
    fs::write(conf.join("40-canopy-nav.toml"), NAV).expect("write fragment");
    srv.cmd(&["reload"]).expect("reload");
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        left,
    ])
    .expect("new-session");
    let left = srv
        .cmd(&["list-panes", "-t", "main", "-F", "#{pane_id}"])
        .expect("list-panes")
        .trim()
        .to_string();
    let right = srv
        .cmd(&[
            "split-window",
            "-h",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "main",
            right,
        ])
        .expect("split")
        .trim()
        .to_string();
    (srv, left, right)
}

fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_for_output(MOUSE_ON, 0);
    a
}

fn var(srv: &TestServer, target: &str, name: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, name])
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn wait_active(srv: &TestServer, pane: &str, what: &str) {
    let got = wait_until(|| var(srv, pane, "#{pane_active}") == "1", WAIT);
    assert!(got.is_ok(), "{what}: {pane} never became active");
}

/// Types `echo <mark>` into the focused shell and waits for its output in
/// `pane`: every key typed before it has been handled by then, since one
/// client's input is handled in order.
fn fence(a: &mut Harness, srv: &TestServer, pane: &str, mark: &str) {
    a.send(format!("echo {mark}\r"));
    let got = wait_until(|| srv.capture(pane).lines().any(|l| l.trim() == mark), WAIT);
    assert!(got.is_ok(), "{mark} never showed in {pane}");
}

/// A Neovim for one test: its init, log, XDG dirs and the `tmux` symlink.
struct Nvim {
    dir: tempfile::TempDir,
}

impl Nvim {
    /// `vsplit`: open two windows side by side, with the right one focused
    /// when `focus_right`, else the left one.
    fn new(vsplit: bool, focus_right: bool) -> Nvim {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for d in ["bin", "config", "data", "state", "cache"] {
            fs::create_dir_all(root.join(d)).expect("nvim dir");
        }
        symlink(env!("CARGO_BIN_EXE_grofe"), root.join("bin/tmux")).expect("tmux symlink");
        let log = root.join("log");
        let mut init = format!(
            r#"set nocompatible
let g:log = '{log}'
function! Nav(dir) abort
  let nr = winnr()
  execute 'wincmd ' . a:dir
  if nr != winnr()
    call writefile(['vim ' . a:dir], g:log, 'a')
    return
  endif
  let args = 'select-pane -t ' . shellescape($TMUX_PANE) . ' -' . tr(a:dir, 'phjkl', 'lLDUR')
  let out = system('tmux -S ' . split($TMUX, ',')[0] . ' ' . args)
  call writefile(['tmux ' . args . ' ' . v:shell_error . trim(out)], g:log, 'a')
endfunction
nnoremap <silent> <C-h> <Cmd>call Nav('h')<CR>
nnoremap <silent> <C-l> <Cmd>call Nav('l')<CR>
"#,
            log = log.display()
        );
        // The split waits for VimEnter, when the screen has its size.
        let split = match (vsplit, focus_right) {
            (false, _) => "",
            (true, true) => "vsplit | wincmd l | ",
            (true, false) => "vsplit | wincmd h | ",
        };
        init.push_str(&format!(
            "autocmd VimEnter * {split}call writefile(['ready'], g:log, 'a')\n"
        ));
        fs::write(root.join("init.vim"), init).expect("init.vim");
        Nvim { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// `env`'s assignments: the `tmux` symlink first on PATH, and every XDG
    /// directory Neovim reads or writes in this test's dir.
    fn env(&self, runtime: Option<&str>) -> String {
        let path = std::env::var("PATH").unwrap_or_default();
        let mut env = format!(
            "PATH='{}:{path}' XDG_CONFIG_HOME='{}' XDG_DATA_HOME='{}' XDG_STATE_HOME='{}' XDG_CACHE_HOME='{}'",
            self.path("bin").display(),
            self.path("config").display(),
            self.path("data").display(),
            self.path("state").display(),
            self.path("cache").display(),
        );
        if let Some(runtime) = runtime {
            env.push_str(&format!(" VIMRUNTIME='{runtime}'"));
        }
        env
    }

    fn args(&self) -> String {
        format!(
            "-u '{}' --noplugin -i NONE -n",
            self.path("init.vim").display()
        )
    }

    /// The pane command: Neovim itself, as `nvim`.
    fn command(&self) -> String {
        format!("exec env {} nvim {}", self.env(None), self.args())
    }

    /// Neovim as typed at a shell prompt, which gets the foreground back
    /// when it exits: a script that execs it, since a line holding the
    /// whole environment could pass the tty's line length limit.
    fn typed(&self) -> String {
        let script = self.path("run-nvim");
        fs::write(&script, format!("#!/bin/sh\n{}\n", self.command())).expect("run-nvim");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        format!("'{}'", script.display())
    }

    /// The pane command: a copy of Neovim's binary named `editor-bin`, exec'd
    /// by a wrapper as `nvim`. Its process name is `editor-bin`, which the
    /// plugin's pattern does not match; its argv[0] is `nvim`.
    fn wrapped_command(&self) -> String {
        let real = which("nvim").expect("nvim on PATH");
        let real = fs::canonicalize(&real).unwrap_or(real);
        let copy = self.path("editor-bin");
        fs::copy(&real, &copy).expect("copy nvim");
        let wrapper = self.path("wrapper");
        fs::write(
            &wrapper,
            format!(
                "#!/usr/bin/env bash\nexec -a nvim '{}' {}\n",
                copy.display(),
                self.args()
            ),
        )
        .expect("wrapper");
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("chmod");
        // A copy outside its install finds no runtime of its own.
        format!(
            "exec env {} '{}'",
            self.env(Some(&vimruntime())),
            wrapper.display()
        )
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.path("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn wait_log(&self, what: &str, done: impl Fn(&[String]) -> bool) {
        let got = wait_until(|| done(&self.log()), WAIT);
        assert!(got.is_ok(), "{what}: the log is {:?}", self.log());
    }

    /// Up, with its mappings, and sampled as the pane's foreground process.
    fn wait_ready(&self, srv: &TestServer, pane: &str) {
        self.wait_log("VimEnter", |l| l.iter().any(|l| l == "ready"));
        let sampled = wait_until(
            || var(srv, pane, "#{pane_current_argv}").starts_with("nvim "),
            WAIT,
        );
        assert!(
            sampled.is_ok(),
            "argv {:?}",
            var(srv, pane, "#{pane_current_argv}")
        );
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

/// The installed Neovim's `$VIMRUNTIME`.
fn vimruntime() -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("vimruntime");
    let status = Command::new("nvim")
        .args(["--clean", "--headless", "-i", "NONE", "-n", "-c"])
        .arg(format!(
            "call writefile([$VIMRUNTIME], '{}')",
            out.display()
        ))
        .args(["-c", "qa!"])
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .status()
        .expect("run nvim");
    assert!(status.success(), "nvim --headless: {status}");
    fs::read_to_string(out)
        .expect("VIMRUNTIME")
        .trim()
        .to_string()
}

/// The fragment's match on `#{pane_foreground_argv}`, with `argv` in its
/// place.
fn nav_match(argv: &str) -> String {
    let start = NAV.find("#{m/ri:").expect("the nav match");
    let end = NAV[start..]
        .find(",#{pane_foreground_argv}}")
        .expect("the nav match's string")
        + start;
    format!("{},{argv}}}", &NAV[start..end])
}

/// The pattern is the plugin's `vim_pattern`, tested on argv[0] alone, of
/// any member of the foreground group (one argv per line).
#[test]
fn the_match_is_the_plugins_pattern_on_argv0() {
    let (srv, left, _) = start(SH, SH);
    let yes = [
        "nvim",
        "nvim -u NONE file.txt",
        "/usr/local/bin/nvim",
        ".nvim-wrapped",
        "nvim-wrapped",
        "vim",
        "vi",
        "view",
        "vimdiff",
        "nvimdiff",
        "gvim",
        "lvim",
        "vimx",
        "fzf",
        "fzf --height 40%",
        "NVIM",
        "sh -c cat | fzf\ncat\nfzf",
        "zsh\nnvim .",
    ];
    let no = [
        "sh",
        "-zsh",
        "man vim",
        "less nvim.log",
        "git commit",
        "nvim-qt",
        "fzf-tmux",
        "",
        "sh -c less vim.txt\nless vim.txt",
    ];
    for argv in yes {
        assert_eq!(var(&srv, &left, &nav_match(argv)), "1", "{argv:?}");
    }
    for argv in no {
        assert_eq!(var(&srv, &left, &nav_match(argv)), "0", "{argv:?}");
    }
}

/// Case 1: from a shell, C-h moves focus left and C-l moves it back.
#[test]
fn case_1_from_a_shell_the_keys_move_focus() {
    let (srv, left, right) = start(SH, SH);
    let mut a = attach(&srv);
    a.wait_for(PROMPT);
    assert_eq!(var(&srv, &right, "#{pane_active}"), "1");
    a.send(C_H);
    wait_active(&srv, &left, "C-h");
    a.send(C_L);
    wait_active(&srv, &right, "C-l");
}

/// Case 2: in Neovim's right window, C-h reaches Neovim, which moves to its
/// left window; at that edge the next C-h is Neovim's own `tmux
/// select-pane -L`, which moves focus to the left pane.
#[test]
fn case_2_in_nvim_the_key_reaches_nvim_which_calls_tmux_at_its_edge() {
    if !require_tool("nvim") {
        return;
    }
    let nvim = Nvim::new(true, true);
    let (srv, left, right) = start(SH, &nvim.command());
    let mut a = attach(&srv);
    nvim.wait_ready(&srv, &right);

    a.send(C_H);
    nvim.wait_log("the first C-h", |l| l.last().is_some_and(|l| l == "vim h"));
    assert_eq!(var(&srv, &right, "#{pane_active}"), "1", "focus stayed");

    a.send(C_H);
    let call = format!("tmux select-pane -t '{right}' -L 0");
    nvim.wait_log("the second C-h", |l| l.last().is_some_and(|l| *l == call));
    wait_active(&srv, &left, "Neovim's select-pane -L");
}

/// Case 3: Neovim exec'd as `nvim` from a binary named `editor-bin`. Its
/// process name does not match; its argv[0] does, so the key still reaches
/// it.
#[test]
fn case_3_nvim_under_a_wrapper_whose_process_name_differs_still_gets_the_key() {
    if !require_tool("nvim") || !require_tool("bash") {
        return;
    }
    let nvim = Nvim::new(false, false);
    let (srv, left, right) = start(SH, &nvim.wrapped_command());
    let mut a = attach(&srv);
    nvim.wait_ready(&srv, &right);
    assert_eq!(var(&srv, &right, "#{pane_current_command}"), "editor-bin");
    assert_eq!(
        var(
            &srv,
            &right,
            "#{m/ri:^(view|l?n?vim?x?|fzf)$,#{pane_current_command}}"
        ),
        "0",
        "the process name is one the plugin's pattern misses"
    );

    a.send(C_H);
    let call = format!("tmux select-pane -t '{right}' -L 0");
    nvim.wait_log("C-h", |l| l.last().is_some_and(|l| *l == call));
    wait_active(&srv, &left, "Neovim's select-pane -L");
}

/// Case 4: Neovim in the leftmost pane, in its own leftmost window. C-h
/// reaches it, it finds no window to its left and calls `tmux select-pane
/// -L`, which grofe runs as a normal pane move: from the leftmost pane that
/// wraps to the rightmost, as tmux 3.7b's does.
#[test]
fn case_4_nvim_at_its_left_edge_moves_focus_through_its_own_tmux_call() {
    if !require_tool("nvim") {
        return;
    }
    let nvim = Nvim::new(true, false);
    let (srv, left, right) = start(&nvim.command(), SH);
    srv.cmd(&["select-pane", "-t", &left]).expect("select");
    let mut a = attach(&srv);
    nvim.wait_ready(&srv, &left);

    a.send(C_H);
    let call = format!("tmux select-pane -t '{left}' -L 0");
    nvim.wait_log("C-h", |l| {
        l.iter().filter(|l| l.starts_with("vim ")).count() == 0
            && l.last().is_some_and(|l| *l == call)
    });
    wait_active(&srv, &right, "the wrapped select-pane -L");
}

/// Case 5: zoomed, C-h unzooms and moves by default, which is the plugin's
/// default on tmux (select-pane without -Z unzooms). With the plugin's
/// `@tmux_navigator_disable_when_zoomed` set to 1, nothing moves.
#[test]
fn case_5_zoomed_c_h_moves_by_default_and_not_when_the_plugin_says_so() {
    let (srv, left, right) = start(SH, SH);
    let mut a = attach(&srv);
    a.wait_for(PROMPT);

    srv.cmd(&["resize-pane", "-Z", "-t", &right]).expect("zoom");
    assert_eq!(var(&srv, &right, "#{window_zoomed_flag}"), "1");
    a.send(C_H);
    wait_active(&srv, &left, "zoomed C-h, by default");
    assert_eq!(var(&srv, &left, "#{window_zoomed_flag}"), "0", "unzoomed");

    srv.cmd(&["set", "-g", "@tmux_navigator_disable_when_zoomed", "1"])
        .expect("set");
    srv.cmd(&["select-pane", "-t", &right]).expect("select");
    srv.cmd(&["resize-pane", "-Z", "-t", &right]).expect("zoom");
    a.send(C_H);
    a.send(C_L);
    fence(&mut a, &srv, &right, "tp2-zoomed");
    assert_eq!(var(&srv, &right, "#{pane_active}"), "1", "focus stayed");
    assert_eq!(
        var(&srv, &right, "#{window_zoomed_flag}"),
        "1",
        "still zoomed"
    );

    // Unzoomed, the option does not stop a move.
    srv.cmd(&["resize-pane", "-Z", "-t", &right])
        .expect("unzoom");
    a.send(C_H);
    wait_active(&srv, &left, "unzoomed C-h, with the option set");
}

/// C-h the moment Neovim is up reaches it, and the moment it has exited
/// moves focus: the conditional reads the foreground group at the keypress,
/// not from the 1 s sample. Nothing here sleeps.
#[test]
fn a_key_right_after_nvim_starts_or_quits_sees_the_process_in_front() {
    if !require_tool("nvim") {
        return;
    }
    let nvim = Nvim::new(false, false);
    let (srv, left, right) = start(SH, SH);
    let mut a = attach(&srv);
    a.wait_for(PROMPT);

    // The quoted `''` keeps the typed line from reading `nvim-gone`.
    a.send(format!("{}; echo nvim-go''ne\r", nvim.typed()));
    nvim.wait_log("VimEnter", |l| l.iter().any(|l| l == "ready"));
    a.send(C_H);
    let call = format!("tmux select-pane -t '{right}' -L 0");
    nvim.wait_log("C-h as Neovim starts", |l| {
        l.last().is_some_and(|l| *l == call)
    });
    wait_active(&srv, &left, "Neovim's select-pane -L");

    srv.cmd(&["select-pane", "-t", &right]).expect("select");
    a.send(":qa!\r");
    let gone = wait_until(
        || srv.capture(&right).lines().any(|l| l.trim() == "nvim-gone"),
        WAIT,
    );
    assert!(gone.is_ok(), "the shell never got the foreground back");
    a.send(C_H);
    wait_active(&srv, &left, "C-h as Neovim exits");
    assert_eq!(
        nvim.log().iter().filter(|l| l.starts_with("tmux ")).count(),
        1,
        "the second C-h went to Neovim: {:?}",
        nvim.log()
    );
}

/// `sh -c 'cat | fzf'`: the group's leader is `sh` and its first member
/// `cat`, and a member whose argv[0] is `fzf` (a `cat` under that name)
/// still makes the key go to the pane.
#[test]
fn a_pipeline_matches_on_a_member_that_is_not_the_leader() {
    let bin = tempfile::tempdir().expect("tempdir");
    let cat = which("cat").expect("cat on PATH");
    symlink(&cat, bin.path().join("fzf")).expect("fzf symlink");
    let path = std::env::var("PATH").unwrap_or_default();
    let pipeline = format!(
        "exec env PATH='{}:{path}' sh -c 'cat | fzf'",
        bin.path().display()
    );
    let (srv, left, right) = start(SH, &pipeline);
    let mut a = attach(&srv);
    let up = wait_until(
        || {
            var(&srv, &right, "#{pane_foreground_argv}")
                .lines()
                .any(|l| l == "fzf")
        },
        WAIT,
    );
    assert!(
        up.is_ok(),
        "{:?}",
        var(&srv, &right, "#{pane_foreground_argv}")
    );
    assert!(var(&srv, &right, "#{pane_current_argv}").starts_with("sh "));

    a.send(C_H);
    a.send("fence-mark\r");
    let echoed = wait_until(|| srv.capture(&right).contains("fence-mark"), WAIT);
    assert!(
        echoed.is_ok(),
        "C-h moved focus, so the fence went to {left}"
    );
    assert_eq!(var(&srv, &right, "#{pane_active}"), "1", "focus stayed");
}

/// A figure the kernel adds to each time `pid` reaps a child, and never
/// otherwise: the total elapsed time of its terminated children on macOS,
/// their minor faults on Linux. Every process that ran adds to both.
#[cfg(target_os = "macos")]
fn reaped_children(pid: i32) -> u64 {
    use nix::libc;
    // SAFETY: an all-zero rusage_info_v2 is a valid value.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live rusage_info_v2, which RUSAGE_INFO_V2 fills.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            (&raw mut info).cast::<libc::rusage_info_t>(),
        )
    };
    assert_eq!(rc, 0, "proc_pid_rusage({pid})");
    info.ri_child_elapsed_abstime
}

#[cfg(target_os = "linux")]
fn reaped_children(pid: i32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
    // After the `(comm)` field: state is field 3, cminflt field 11.
    let rest = &stat[stat.rfind(')').expect("comm") + 2..];
    rest.split(' ')
        .nth(8)
        .expect("cminflt")
        .parse()
        .expect("number")
}

/// Case 6: every nav key is the format form, and pressing them forks
/// nothing. A shell-form binding pressed afterwards moves the same count,
/// which shows the count sees a fork.
#[test]
fn case_6_the_keys_spawn_no_process() {
    let (srv, left, right) = start(SH, SH);
    let keys = srv.cmd(&["list-keys", "-T", "root"]).expect("list-keys");
    for key in ["C-h", "C-j", "C-k", "C-l", "C-\\\\"] {
        let line = keys
            .lines()
            .find(|l| l.starts_with(&format!("bind-key -T root {key} ")))
            .unwrap_or_else(|| panic!("{key} is not bound:\n{keys}"));
        assert!(
            line.contains(&format!("bind-key -T root {key} if-shell -F "))
                && line.contains("\"if-shell -F '"),
            "{key} is not the format form: {line}"
        );
    }
    srv.cmd(&[
        "bind-key",
        "-n",
        "C-g",
        "if-shell",
        "true",
        "set -g @probe ran",
    ])
    .expect("bind C-g");
    let pid: i32 = var(&srv, &left, "#{pid}").parse().expect("server pid");
    let mut a = attach(&srv);
    a.wait_for(PROMPT);

    let before = reaped_children(pid);
    for _ in 0..3 {
        a.send(C_H);
        wait_active(&srv, &left, "C-h");
        a.send(C_L);
        wait_active(&srv, &right, "C-l");
    }
    assert_eq!(reaped_children(pid), before, "a nav key forked");

    a.send(b"\x07");
    let ran = wait_until(|| var(&srv, &left, "#{@probe}") == "ran", WAIT);
    assert!(ran.is_ok(), "the shell-form probe never ran");
    assert!(
        reaped_children(pid) > before,
        "the count missed the probe's fork"
    );
}
