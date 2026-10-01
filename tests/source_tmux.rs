//! `source-file` of a file that is not `*.toml`: read as a tmux config
//! through the tmux front end, as tmux 3.7b reads one (recorded on a
//! private socket, `-f /dev/null`), in grofe's dialect and tmux's alike.
//! tmux's plugins source their own files this way (which-key's
//! `init.tmux`, TP.5).

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use common::TestServer;

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
    assert_eq!(srv.cmd(&["source-file", &file]), Ok("loading\n".into()));
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
    let err = srv.cmd(&["source-file", &file]).unwrap_err();
    assert_eq!(
        err.trim_end(),
        format!("{file}:3: unknown command: frobnicate")
    );
    assert_eq!(opt(&srv, "@a"), "");
    // An unknown flag is a parse error too, as in tmux.
    let file = write(&srv, "flag.conf", "display -p one\nset -Z @m 1\n");
    let err = srv.cmd(&["source-file", &file]).unwrap_err();
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
    let out = srv.run(&["source-file", &file]);
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
    let tmux = bin.join("tmux").display().to_string();
    let file = write(
        &srv,
        "hold.tmux",
        &format!(
            "run-shell 'sleep 0.3; {tmux} -S \"${{TMUX%%,*}}\" set -g @x shell'\nset -gF @y '#{{@x}}'\n"
        ),
    );
    assert_eq!(srv.cmd(&["source-file", &file]), Ok(String::new()));
    assert_eq!(opt(&srv, "@y"), "shell");
}

/// A `*.toml` file is still grofe's config; a missing file with `-q` is
/// nothing, whatever its name.
#[test]
fn toml_stays_grofe_config() {
    let srv = server();
    let file = write(&srv, "x.toml", "status.left = \"sourced\"\n");
    assert_eq!(srv.cmd(&["source-file", &file]), Ok(String::new()));
    assert_eq!(opt(&srv, "status-left"), "sourced");
    let gone = PathBuf::from("/nonexistent/x.tmux").display().to_string();
    assert_eq!(srv.cmd(&["source-file", "-q", &gone]), Ok(String::new()));
}
