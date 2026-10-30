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
/// `<path>:6: unknown command: frobnicate`. tmux collects such errors and
/// prints them on stdout when the file is done; the request fails.
#[test]
fn a_parse_error_runs_nothing_and_names_its_line() {
    let srv = server();
    let file = write(&srv, "bad.conf", "set -g @a 1\n\nfrobnicate\n");
    let out = tmux_run(&srv, &["source-file", &file]);
    assert!(!out.status.success());
    assert_eq!(
        out.stdout.trim_end(),
        format!("{file}:3: unknown command: frobnicate")
    );
    assert_eq!(out.stderr, "");
    assert_eq!(opt(&srv, "@a"), "");
    // An unknown flag is a parse error too, as in tmux.
    let file = write(&srv, "flag.conf", "display -p one\nset -Z @m 1\n");
    let out = tmux_run(&srv, &["source-file", &file]);
    assert!(!out.status.success());
    assert!(
        out.stdout.starts_with(&format!("{file}:2: ")),
        "{}",
        out.stdout
    );
    assert!(!out.stdout.contains("one"), "{}", out.stdout);
}

/// Valid tmux that grofe cannot do (`detach-client -E true`) fails its own command
/// and the rest of its line; the other lines run, and the request fails
/// with each error named by file and line.
#[test]
fn a_command_grofe_lacks_fails_only_its_own_line() {
    let srv = server();
    let file = write(
        &srv,
        "lack.conf",
        "set -g @a 1\n\
         detach-client -E true\n\
         set -g @b 2\n\
         set -g @before 1 ; detach-client -E true ; set -g @skipped 1\n\
         set -g @c 3\n",
    );
    let out = tmux_run(&srv, &["source-file", &file]);
    assert!(!out.status.success());
    for (name, want) in [("@a", "1"), ("@b", "2"), ("@before", "1"), ("@c", "3")] {
        assert_eq!(opt(&srv, name), want, "{name}");
    }
    assert_eq!(opt(&srv, "@skipped"), "");
    assert!(
        out.stderr.contains(&format!("{file}:2: ")),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains(&format!("{file}:4: ")),
        "{}",
        out.stderr
    );
    assert!(
        !out.stderr.contains(&format!("{file}:3: ")),
        "{}",
        out.stderr
    );
}

/// A file sourced from a file is the same: its parse error runs none of
/// its lines and fails that `source-file`, the outer file's lines go on.
#[test]
fn a_nested_file_fails_alone() {
    let srv = server();
    let inner = write(&srv, "inner.conf", "set -g @inner 1\nfrobnicate\n");
    let lack = write(
        &srv,
        "lack-inner.conf",
        "set -g @i1 1\ndetach-client -E true\nset -g @i2 2\n",
    );
    let outer = write(
        &srv,
        "outer.conf",
        &format!("set -g @o1 1\nsource-file {inner}\nsource-file {lack}\nset -g @o2 2\n"),
    );
    let out = tmux_run(&srv, &["source-file", &outer]);
    assert!(!out.status.success());
    assert_eq!(opt(&srv, "@inner"), "");
    for (name, want) in [("@o1", "1"), ("@o2", "2"), ("@i1", "1"), ("@i2", "2")] {
        assert_eq!(opt(&srv, name), want, "{name}");
    }
    assert!(
        out.stdout
            .contains(&format!("{inner}:2: unknown command: frobnicate")),
        "{}",
        out.stdout
    );
    assert!(
        out.stderr.contains(&format!("{lack}:2: ")),
        "{}",
        out.stderr
    );
}

/// `%if`/`%elif`/`%else`/`%endif` choose lines by a format expanded for
/// the requester's pane; `NAME=value` and `%hidden NAME=value` set the
/// global environment, and a later line reads `$NAME`.
#[test]
fn conditionals_and_assignments_in_a_file() {
    let srv = server();
    let file = write(
        &srv,
        "cond.conf",
        "%if \"#{==:#{session_name},main}\"\n\
         set -g @in main\n\
         %else\n\
         set -g @in other\n\
         %endif\n\
         %if #{==:a b,a b}\n\
         set -g @spaced yes\n\
         %endif\n\
         FOO=bar\n\
         %hidden HID=h\n\
         set -g @foo \"$FOO$HID\"\n\
         %if 0\n\
         NEVER=1\n\
         set -Z\n\
         %endif\n",
    );
    tmux(&srv, &["source-file", &file]).expect("source");
    assert_eq!(opt(&srv, "@in"), "main");
    assert_eq!(opt(&srv, "@spaced"), "yes");
    assert_eq!(opt(&srv, "@foo"), "barh");
    assert_eq!(
        tmux(&srv, &["show-environment", "-g", "FOO"]).unwrap(),
        "FOO=bar\n"
    );
    assert!(tmux(&srv, &["show-environment", "-g", "NEVER"]).is_err());
}

/// `source-file -n` reads the file, reports its errors and runs nothing
/// (and sets no variable); `-F` expands the path as a format.
#[test]
fn source_file_dash_n_and_dash_f() {
    let srv = server();
    let file = write(&srv, "n.conf", "set -g @n 1\nPARSED=1\n");
    tmux(&srv, &["source-file", "-n", &file]).expect("-n");
    assert_eq!(opt(&srv, "@n"), "");
    assert!(tmux(&srv, &["show-environment", "-g", "PARSED"]).is_err());
    let bad = write(&srv, "nbad.conf", "set -g @n 1\n%endif\n");
    let out = tmux_run(&srv, &["source-file", "-n", &bad]);
    assert!(!out.status.success());
    assert!(out.stdout.contains(":2: syntax error"), "{}", out.stdout);
    write(&srv, "main.conf", "set -g @f 1\n");
    tmux(&srv, &["source-file", "-F", "#{session_name}.conf"]).expect("-F");
    assert_eq!(opt(&srv, "@f"), "1");
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

/// A `run-shell` hold does not reset the nesting: the lines after it run
/// at the depth of the file they are in, so a file that runs a shell and
/// then sources itself still stops at tmux's limit, within the bound.
#[test]
fn nesting_survives_a_run_shell_hold() {
    let srv = server();
    let home = srv.home.display().to_string();
    let me = write(
        &srv,
        "held-self.conf",
        &format!("set -ga @n x\nrun-shell true\nsource-file {home}/held-self.conf\n"),
    );
    let out = tmux_run(&srv, &["source-file", &me]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(out.stderr.trim_end(), "too many nested files");
    assert_eq!(opt(&srv, "@n"), "x".repeat(50));
    assert!(srv.cmd(&["list-sessions"]).is_ok(), "the server is up");
    // The depth unwound: a later, shallow source still works.
    let ok = write(&srv, "ok2.conf", "set -g @ok2 yes\n");
    assert_eq!(tmux(&srv, &["source-file", &ok]), Ok(String::new()));
    assert_eq!(opt(&srv, "@ok2"), "yes");
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

/// Several paths and globs in one `source-file`, as tmux's `glob(3)`: a
/// glob loads its files in sorted order, a path that matches nothing is
/// `path: No such file or directory` unless `-q`, and the other paths still
/// load.
#[test]
fn source_file_takes_several_paths_and_globs() {
    let srv = server();
    write(&srv, "g1.conf", "set -g @order 1\n");
    write(&srv, "g3.conf", "set -ga @order 3\n");
    write(&srv, "g2.conf", "set -ga @order 2\n");
    write(&srv, "one.conf", "set -g @one 1\n");
    write(&srv, "two.conf", "set -g @two 2\n");
    tmux(&srv, &["source-file", "one.conf", "two.conf"]).expect("two paths");
    assert_eq!(
        (opt(&srv, "@one"), opt(&srv, "@two")),
        ("1".into(), "2".into())
    );
    tmux(&srv, &["source-file", "g*.conf"]).expect("glob");
    assert_eq!(opt(&srv, "@order"), "123");

    srv.cmd(&["set-option", "-gu", "@one"]).unwrap();
    let out = tmux_run(&srv, &["source-file", "one.conf", "no*.conf", "two.conf"]);
    assert!(!out.status.success());
    assert_eq!(out.stderr.trim_end(), "No such file or directory: no*.conf");
    assert_eq!(opt(&srv, "@one"), "1");
    assert_eq!(
        tmux(&srv, &["source-file", "-q", "no*.conf", "missing.conf"]),
        Ok(String::new())
    );
    let out = tmux_run(&srv, &["source-file", "missing.conf"]);
    assert_eq!(
        out.stderr.trim_end(),
        "No such file or directory: missing.conf"
    );
}

/// A relative path in a file that was itself sourced is still the
/// requester's cwd's, not the file's directory, as tmux 3.7b (recorded).
#[test]
fn a_nested_relative_path_is_relative_to_the_cwd() {
    let srv = server();
    let dir = srv.home.join("nest");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("inner.conf"), "set -g @inner yes\n").unwrap();
    fs::write(srv.home.join("top.conf"), "set -g @top yes\n").unwrap();
    fs::write(
        dir.join("outer.conf"),
        "source-file inner.conf\nsource-file top.conf\n",
    )
    .unwrap();
    let out = tmux_run(&srv, &["source-file", "nest/outer.conf"]);
    assert!(!out.status.success());
    assert_eq!(
        out.stderr.trim_end(),
        "No such file or directory: inner.conf"
    );
    assert_eq!(
        (opt(&srv, "@inner"), opt(&srv, "@top")),
        (String::new(), "yes".into())
    );
}

/// A `run-shell` in the first file holds the second file too.
#[test]
fn a_hold_in_one_file_holds_the_next_file() {
    let srv = server();
    let bin = srv.home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    symlink(env!("CARGO_BIN_EXE_grofe"), bin.join("tmux")).unwrap();
    let bin_tmux = bin.join("tmux").display().to_string();
    write(
        &srv,
        "h1.conf",
        &format!("run-shell 'sleep 0.3; {bin_tmux} -S \"${{TMUX%%,*}}\" set -g @x shell'\n"),
    );
    write(&srv, "h2.conf", "set -gF @y '#{@x}'\n");
    assert_eq!(tmux(&srv, &["source-file", "h*.conf"]), Ok(String::new()));
    assert_eq!(opt(&srv, "@y"), "shell");
}
