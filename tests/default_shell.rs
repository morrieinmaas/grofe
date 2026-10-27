//! `default-shell`: a new pane with no command starts it, and sees it as
//! `$SHELL`, as tmux starts a pane's shell from the option.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TestServer;

#[test]
fn a_new_pane_starts_the_default_shell_when_it_is_set() {
    let srv = TestServer::start();
    let shell = srv.home.join("marker-shell");
    fs::write(
        &shell,
        "#!/bin/sh\necho \"started $0 with SHELL=$SHELL\"\nexec /bin/sh\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let shell = shell.display().to_string();

    srv.cmd(&["new-session", "-d", "-s", "before"]).unwrap();
    srv.cmd(&["set-option", "-g", "default-shell", &shell])
        .unwrap();
    srv.cmd(&["new-session", "-d", "-s", "after"]).unwrap();
    srv.cmd(&["new-window", "-t", "after"]).unwrap();

    // The pane wraps long lines; read it as one.
    let text = |target: &str| srv.capture(target).replace('\n', "");
    for target in ["after:1", "after:2"] {
        let want = format!("marker-shell with SHELL={shell}");
        srv.wait_for(|| text(target).contains(&want), Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("{target}: {e}\n{}", srv.capture(target)));
    }
    // A pane started before the option was set kept the user's shell.
    assert!(!srv.capture("before:1").contains("marker-shell"));
}

/// As tmux 3.7b's `checkshell` (recorded): a shell that is not an
/// absolute path it may run is refused, and panes keep starting.
#[test]
fn an_unusable_default_shell_is_refused_as_tmux_does() {
    let srv = TestServer::start();
    for bad in ["/nonexistent/shell", "sh"] {
        let err = srv
            .cmd(&["set-option", "-g", "default-shell", bad])
            .unwrap_err();
        assert_eq!(err.trim_end(), format!("not a suitable shell: {bad}"));
    }
    srv.cmd(&["new-session", "-d", "-s", "s"]).unwrap();
    srv.cmd(&["send-keys", "-t", "s", "echo alive-$((6*7))", "Enter"])
        .unwrap();
    srv.wait_for(
        || srv.capture("s").contains("alive-42"),
        Duration::from_secs(5),
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", srv.capture("s")));
}

/// `/bin/sh` under another name in the test's home, so `$0` of a command
/// string tells how the shell was started (its `argv[0]`).
fn named_shell(srv: &TestServer, name: &str) -> String {
    let path = srv.home.join(name);
    std::os::unix::fs::symlink(fs::canonicalize("/bin/sh").unwrap(), &path).unwrap();
    path.display().to_string()
}

/// What `file` holds once its command wrote a whole line.
fn written(file: &Path) -> String {
    common::wait_until(
        || fs::read_to_string(file).is_ok_and(|s| s.ends_with('\n')),
        Duration::from_secs(5),
    )
    .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    fs::read_to_string(file).unwrap()
}

/// The command that records its `$0` and `$SHELL` in a file of its own.
fn record(srv: &TestServer, name: &str) -> (String, PathBuf) {
    let file = srv.home.join(name);
    (format!("echo \"$0 $SHELL\" > {}", file.display()), file)
}

/// tmux 3.7b's `spawn_pane`: a one-word command is `default-shell -c
/// <command>`, `argv[0]` the shell's name and `SHELL` its path, in every
/// verb that starts a pane. A shell that stops being usable is `/bin/sh`.
#[test]
fn a_pane_command_runs_through_the_default_shell() {
    let srv = TestServer::start();
    let shell = named_shell(&srv, "tshell");
    srv.cmd(&["set-option", "-g", "default-shell", &shell])
        .unwrap();
    srv.cmd(&["new-session", "-d", "-s", "s"]).unwrap();
    let want = format!("tshell {shell}\n");

    let (cmd, file) = record(&srv, "new-window");
    srv.cmd(&["new-window", "-d", "-t", "s", &cmd]).unwrap();
    assert_eq!(written(&file), want, "new-window");

    let (cmd, file) = record(&srv, "new-session");
    srv.cmd(&["new-session", "-d", "-s", "two", &cmd]).unwrap();
    assert_eq!(written(&file), want, "new-session");

    let (cmd, file) = record(&srv, "split-window");
    srv.cmd(&["split-window", "-d", "-t", "s", &cmd]).unwrap();
    assert_eq!(written(&file), want, "split-window");

    let (cmd, file) = record(&srv, "new-pane");
    srv.cmd(&["new-pane", "-t", "s", &cmd]).unwrap();
    assert_eq!(written(&file), want, "new-pane");

    let (cmd, file) = record(&srv, "respawn-pane");
    srv.cmd(&["respawn-pane", "-k", "-t", "%0", &cmd]).unwrap();
    assert_eq!(written(&file), want, "respawn-pane");

    let (cmd, file) = record(&srv, "respawn-window");
    srv.cmd(&["respawn-window", "-k", "-t", "s:1", &cmd])
        .unwrap();
    assert_eq!(written(&file), want, "respawn-window");

    // Gone after it was set: tmux's `checkshell` fails at spawn, `/bin/sh`.
    fs::remove_file(&shell).unwrap();
    let (cmd, file) = record(&srv, "fallback");
    srv.cmd(&["new-window", "-d", "-t", "s", &cmd]).unwrap();
    assert_eq!(written(&file), "sh /bin/sh\n", "fallback");
}

/// A respawn keeps the shell the pane started with (tmux's `wp->shell`,
/// "if respawning, use the old one"), and a command of several words is
/// executed directly, not through the shell.
#[test]
fn a_respawn_keeps_its_shell_and_several_words_skip_it() {
    let srv = TestServer::start();
    let first = named_shell(&srv, "first");
    let second = named_shell(&srv, "second");
    srv.cmd(&["set-option", "-g", "default-shell", &first])
        .unwrap();
    srv.cmd(&["new-session", "-d", "-s", "s"]).unwrap();
    let file = srv.home.join("respawned");
    let cmd = format!("echo \"$0 $SHELL\" >> {}", file.display());
    srv.cmd(&["new-window", "-d", "-t", "s", &cmd]).unwrap();
    written(&file);
    srv.cmd(&["set-option", "-g", "default-shell", &second])
        .unwrap();
    srv.cmd(&["respawn-pane", "-k", "-t", "s:2"]).unwrap();
    common::wait_until(
        || fs::read_to_string(&file).is_ok_and(|s| s.lines().count() == 2),
        Duration::from_secs(5),
    )
    .unwrap();
    let both = format!("first {first}\nfirst {first}\n");
    assert_eq!(fs::read_to_string(&file).unwrap(), both);

    // A shell that logs each start tells whether it was used.
    let logger = srv.home.join("logger");
    let log = srv.home.join("logger.log");
    fs::write(
        &logger,
        format!(
            "#!/bin/sh\necho started >> {}\nexec /bin/sh \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&logger, fs::Permissions::from_mode(0o755)).unwrap();
    srv.cmd(&[
        "set-option",
        "-g",
        "default-shell",
        &logger.display().to_string(),
    ])
    .unwrap();
    let done = srv.home.join("words");
    let script = format!("echo ok > {}", done.display());
    srv.cmd(&["new-window", "-d", "-t", "s", "/bin/sh", "-c", &script])
        .unwrap();
    written(&done);
    assert!(!log.exists(), "several words went through default-shell");
    let (one, once) = record(&srv, "one-word");
    srv.cmd(&["new-window", "-d", "-t", "s", &one]).unwrap();
    written(&once);
    assert_eq!(fs::read_to_string(&log).unwrap(), "started\n");
}

/// `default-command` is what a pane runs when it is given none, through
/// `default-shell` (tmux's `spawn_pane`); a command given wins.
#[test]
fn default_command_runs_through_the_default_shell() {
    let srv = TestServer::start();
    let shell = named_shell(&srv, "dshell");
    srv.cmd(&["set-option", "-g", "default-shell", &shell])
        .unwrap();
    let (cmd, file) = record(&srv, "default-command");
    srv.cmd(&["set-option", "-g", "default-command", &cmd])
        .unwrap();
    srv.cmd(&["new-session", "-d", "-s", "s"]).unwrap();
    assert_eq!(written(&file), format!("dshell {shell}\n"));

    let (given, given_file) = record(&srv, "given");
    srv.cmd(&["new-window", "-d", "-t", "s", &given]).unwrap();
    written(&given_file);
    fs::remove_file(&file).unwrap();
    srv.cmd(&["new-window", "-d", "-t", "s"]).unwrap();
    assert_eq!(written(&file), format!("dshell {shell}\n"));
}

/// `run-shell` and `if-shell` do not use `default-shell`: tmux's `job_run`
/// without `JOB_DEFAULTSHELL` runs `/bin/sh` as `sh`.
#[test]
fn run_shell_and_if_shell_use_bin_sh() {
    let srv = TestServer::start();
    let shell = named_shell(&srv, "tshell");
    srv.cmd(&["set-option", "-g", "default-shell", &shell])
        .unwrap();
    srv.cmd(&["new-session", "-d", "-s", "s"]).unwrap();
    assert_eq!(srv.cmd(&["run-shell", "echo \"$0\""]).unwrap(), "sh\n");
    let out = srv
        .cmd(&[
            "if-shell",
            "test \"$0\" = sh",
            "display-message -p yes",
            "display-message -p no",
        ])
        .unwrap();
    assert_eq!(out, "yes\n");
}
