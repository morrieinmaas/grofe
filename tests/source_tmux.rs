//! `source-file` of a file that is not `*.toml`, sent as tmux's (through
//! `tmux` naming grofe, or to a tmux-identity server): read as a tmux
//! config through the tmux front end, as tmux 3.7b reads one (recorded on
//! a private socket, `-f /dev/null`). tmux's plugins source their own
//! files this way (which-key's `init.tmux`, TP.5). A native `source-file`
//! on a grofe server reads any file as TOML, as before.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use common::{CmdOutput, TestServer, run_bounded};

/// `tmux <args>` against `srv`: grofe through a `tmux` symlink, so the
/// request is in tmux's dialect, as a plugin's own `tmux` call is.
fn tmux_run(srv: &TestServer, args: &[&str]) -> CmdOutput {
    let link = srv.home.join("tmux-link/tmux");
    if !link.exists() {
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    }
    let base = srv.command();
    let mut cmd = std::process::Command::new(&link);
    for (k, v) in base.get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.current_dir(&srv.home)
        .arg("-S")
        .arg(&srv.socket)
        .args(args);
    run_bounded(cmd, std::time::Duration::from_secs(20))
}

fn tmux(srv: &TestServer, args: &[&str]) -> Result<String, String> {
    let out = tmux_run(srv, args);
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(out.stderr)
    }
}

fn server() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"])
        .expect("new-session");
    srv
}

fn write(srv: &TestServer, name: &str, text: &str) -> String {
    let path = srv.home.join(name);
    fs::write(&path, text).expect("write");
    path.display().to_string()
}

fn opt(srv: &TestServer, name: &str) -> String {
    srv.cmd(&["show-options", "-gqv", name])
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

/// tmux's syntax: comments, a backslash-newline, quotes across lines, a
/// `{ }` list, `;` on a line, tmux's verb aliases and option names; what
/// `display -p` prints is the request's output.
#[test]
fn a_tmux_file_runs_its_commands() {
    let srv = server();
    let file = write(
        &srv,
        "init.tmux",
        "# a comment\n\
         display -p 'loading'\n\
         set -g @a 1 ; set -g @b \"two \\\n words\" # trailing\n\
         set -g @c 'x\ny'\n\
         setw -g @d on\n\
         bind -T prefix F7 {\n  set -g @e 1\n  set -g @f 2\n}\n\
         set -g status-left 'from tmux'\n",
    );
    assert_eq!(tmux(&srv, &["source-file", &file]), Ok("loading\n".into()));
    assert_eq!(opt(&srv, "@a"), "1");
    assert_eq!(opt(&srv, "@b"), "two  words");
    assert_eq!(opt(&srv, "@c"), "x\ny");
    assert_eq!(opt(&srv, "status-left"), "from tmux");
    let keys = srv.cmd(&["list-keys", "-T", "prefix"]).unwrap();
    assert!(keys.contains("F7"), "{keys}");
}

/// A parse error anywhere (here an unknown command on line 3) runs
/// nothing, and names the file and line, as tmux 3.7b's
/// `<path>:6: unknown command: frobnicate`.
#[test]
fn a_parse_error_runs_nothing_and_names_its_line() {
    let srv = server();
    let file = write(&srv, "bad.conf", "set -g @a 1\n\nfrobnicate\n");
    let err = tmux(&srv, &["source-file", &file]).unwrap_err();
    assert_eq!(
        err.trim_end(),
        format!("{file}:3: unknown command: frobnicate")
    );
    assert_eq!(opt(&srv, "@a"), "");
    // An unknown flag is a parse error too, as in tmux.
    let file = write(&srv, "flag.conf", "display -p one\nset -Z @m 1\n");
    let err = tmux(&srv, &["source-file", &file]).unwrap_err();
    assert!(err.starts_with(&format!("{file}:2: ")), "{err}");
    assert!(!err.contains("one"), "{err}");
}

/// A command that fails while running stops the rest of its line; the
/// next lines run, and the request fails at the end after their output,
/// as tmux 3.7b's `one`, `two`, then `empty value`, exit 1.
#[test]
fn a_failing_command_stops_its_line_only() {
    let srv = server();
    let file = write(
        &srv,
        "run.conf",
        "display -p one\nset -g @m ; set -g @n 1\nset -g @o 1\ndisplay -p two\n",
    );
    let out = tmux_run(&srv, &["source-file", &file]);
    assert!(!out.status.success());
    assert_eq!(out.stdout, "one\ntwo\n");
    assert!(!out.stderr.is_empty());
    assert_eq!(opt(&srv, "@n"), "");
    assert_eq!(opt(&srv, "@o"), "1");
}

/// A `run-shell` holds the lines after it until its shell is done, as
/// tmux's queue waits: which-key's `init.tmux` binds its key from a
/// `run-shell "tmux bind-key ..."` and nothing after it may run first.
#[test]
fn run_shell_holds_the_lines_after_it() {
    let srv = server();
    let bin = srv.home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    symlink(env!("CARGO_BIN_EXE_grofe"), bin.join("tmux")).unwrap();
    let bin_tmux = bin.join("tmux").display().to_string();
    let file = write(
        &srv,
        "hold.tmux",
        &format!(
            "run-shell 'sleep 0.3; {bin_tmux} -S \"${{TMUX%%,*}}\" set -g @x shell'\nset -gF @y '#{{@x}}'\n"
        ),
    );
    assert_eq!(tmux(&srv, &["source-file", &file]), Ok(String::new()));
    assert_eq!(opt(&srv, "@y"), "shell");
}

/// A `*.toml` file is grofe's config in either dialect; a native
/// `source-file` on a grofe server reads a file of any other name as TOML
/// too, as before; a missing file with `-q` is nothing.
#[test]
fn toml_stays_grofe_config() {
    let srv = server();
    let file = write(&srv, "x.toml", "status.left = \"sourced\"\n");
    assert_eq!(tmux(&srv, &["source-file", &file]), Ok(String::new()));
    assert_eq!(opt(&srv, "status-left"), "sourced");
    let plain = write(&srv, "extensionless", "status.left = \"plain\"\n");
    assert_eq!(srv.cmd(&["source-file", &plain]), Ok(String::new()));
    assert_eq!(opt(&srv, "status-left"), "plain");
    let conf = write(&srv, "native.conf", "status.left = \"conf\"\n");
    assert_eq!(srv.cmd(&["source-file", &conf]), Ok(String::new()));
    assert_eq!(opt(&srv, "status-left"), "conf");
    let gone = PathBuf::from("/nonexistent/x.tmux").display().to_string();
    assert_eq!(srv.cmd(&["source-file", "-q", &gone]), Ok(String::new()));
}

/// tmux 3.7b (recorded): a file that sources itself loads 50 times, and
/// the 51st is `too many nested files`, once, exit 1; two files that
/// source each other the same. The server stays up.
#[test]
fn nested_files_stop_at_tmux_s_limit() {
    let srv = server();
    let home = srv.home.display().to_string();
    let me = write(
        &srv,
        "self.conf",
        &format!("set -ga @n x\nsource-file {home}/self.conf\n"),
    );
    let out = tmux_run(&srv, &["source-file", &me]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stderr.trim_end(), "too many nested files");
    assert_eq!(opt(&srv, "@n"), "x".repeat(50));

    write(
        &srv,
        "a.conf",
        &format!("set -ga @m a\nsource-file {home}/b.conf\n"),
    );
    write(
        &srv,
        "b.conf",
        &format!("set -ga @m b\nsource-file {home}/a.conf\n"),
    );
    let out = tmux_run(&srv, &["source-file", &format!("{home}/a.conf")]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stderr.trim_end(), "too many nested files");
    assert_eq!(opt(&srv, "@m"), "ab".repeat(25));
    assert!(srv.cmd(&["list-sessions"]).is_ok(), "the server is up");
    // A later, shallow source still works.
    let ok = write(&srv, "ok.conf", "set -g @ok yes\n");
    assert_eq!(tmux(&srv, &["source-file", &ok]), Ok(String::new()));
    assert_eq!(opt(&srv, "@ok"), "yes");
}

/// An error on a line before a `run-shell` hold, and one after it, are
/// kept: the lines after each still run, and the request fails with both
/// at the end. When only a line before the hold failed, tmux 3.7b prints
/// the error and exits with the `run-shell`'s status, 0 (recorded: `set
/// -g @e`, `run-shell "sleep 0.2"`, `set -g @after 1` gives `empty value`
/// and exit 0); a tmux-dialect request does the same.
#[test]
fn errors_around_a_hold_are_kept() {
    let srv = server();
    let file = write(
        &srv,
        "hold2.conf",
        "display -p one\nset -g @e\nrun-shell 'echo two'\nset -g @z ; set -g @skipped 1\nset -g @after 1\n",
    );
    let out = tmux_run(&srv, &["source-file", &file]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(out.stdout, "one\ntwo\n");
    assert_eq!(out.stderr.trim_end(), "empty value\nempty value");
    assert_eq!(opt(&srv, "@after"), "1");
    assert_eq!(opt(&srv, "@skipped"), "");
    // Only a line before the hold failed: printed, and exit 0.
    let file = write(
        &srv,
        "hold.conf",
        "set -g @e\nrun-shell 'sleep 0.2'\nset -g @after2 1\n",
    );
    let out = tmux_run(&srv, &["source-file", &file]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(out.stderr.trim_end(), "empty value");
    assert_eq!(opt(&srv, "@after2"), "1");
}

/// A native request keeps the failure: on a tmux-identity server (where a
/// native `source-file` reads a tmux file too), the same file exits 1.
#[test]
fn a_native_request_keeps_an_error_before_the_hold() {
    let srv = TestServer::unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), "compat-identity = \"tmux\"\n").unwrap();
    srv.cmd(&["start-server"]).expect("start-server");
    srv.cmd(&["new-session", "-d", "-s", "main"])
        .expect("new-session");
    let file = write(
        &srv,
        "hold.conf",
        "set -g @e\nrun-shell 'sleep 0.2'\nset -g @after2 1\n",
    );
    let out = srv.run(&["source-file", &file]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(out.stderr.trim_end(), "empty value");
    assert_eq!(opt(&srv, "@after2"), "1");
    let _ = srv.cmd(&["kill-server"]);
}

/// which-key on a grofe server reading its native config: its entry point
/// runs through a native `run-shell`, and its own `tmux source-file
/// init.tmux` reaches grofe through the `tmux` name, so it is tmux's and
/// reads the file as tmux syntax: prefix Space is bound to its menu.
#[test]
fn which_key_loads_on_a_native_grofe_server() {
    let srv = server();
    let link = srv.home.join("tmux-link");
    tmux(&srv, &["list-sessions"]).unwrap();
    let copy = srv.home.join("wk");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/canopy-port/plugins/tmux-which-key");
    let cp = std::process::Command::new("cp")
        .arg("-R")
        .arg(&src)
        .arg(&copy)
        .status()
        .unwrap();
    assert!(cp.success());
    let path = format!("{}:/usr/bin:/bin", link.display());
    srv.cmd(&["set-environment", "-g", "PATH", &path]).unwrap();
    srv.cmd(&["set-option", "-g", "@tmux-which-key-disable-autobuild", "1"])
        .unwrap();
    let entry = copy.join("plugin.sh.tmux").display().to_string();
    srv.cmd(&["run-shell", &format!("'{entry}' >/dev/null 2>&1")])
        .unwrap();
    let keys = srv.cmd(&["list-keys", "-T", "prefix"]).unwrap();
    let space = keys
        .lines()
        .find(|l| l.starts_with("bind-key -T prefix Space "))
        .unwrap_or_else(|| panic!("no prefix Space:\n{keys}"));
    assert!(space.contains("display-menu"), "{space}");
}
