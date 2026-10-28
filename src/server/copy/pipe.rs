//! Copy targets (spec §4.11, TM.5): `copy-pipe` and its forms, as tmux
//! 3.7b's `window_copy_copy_pipe`. Each is its copy command (TM.2's
//! `copy-selection`, `copy-end-of-line` or `copy-line` form, so it stores
//! a buffer and goes to the clipboard the same way) plus a shell command,
//! or the `copy-command` option when it has none, run with the selection
//! on stdin.
//!
//! The command runs in a task of its own, never waited on by the actor,
//! through `/bin/sh -c` under the spawn lock, with the session's
//! environment as `run-shell` gets it (`hooks::shell_env`) and no pane of
//! its own. Its stdout and stderr go to `/dev/null`, as tmux's
//! `JOB_NOWAIT` jobs. The selection only ever reaches it on stdin; the
//! command itself is the binding's argument, format-expanded against the
//! pane before the copy, as tmux expands it (`copy-command` is not).
//!
//! As recorded from tmux 3.7b: an empty selection still runs the command,
//! with nothing on stdin, and three positionals make the command do
//! nothing.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::AsyncWriteExt;

use super::select::{self, Copied};
use super::{Action, Ctx, Mode};
use crate::cmd;
use crate::server::dispatch::options::{FormatCtx, ServerVars};
use crate::server::{State, hooks, pane};
use crate::types::PaneId;

/// Each pipe command and the copy command it copies as.
const PIPES: &[(&str, &str)] = &[
    ("copy-pipe", "copy-selection"),
    ("copy-pipe-no-clear", "copy-selection-no-clear"),
    ("copy-pipe-and-cancel", "copy-selection-and-cancel"),
    ("copy-pipe-end-of-line", "copy-end-of-line"),
    (
        "copy-pipe-end-of-line-and-cancel",
        "copy-end-of-line-and-cancel",
    ),
    ("copy-pipe-line", "copy-line"),
    ("copy-pipe-line-and-cancel", "copy-line-and-cancel"),
];

/// The pipes that only run a command with the selection on stdin: no
/// buffer, no clipboard and no flags (tmux's `window_copy_cmd_pipe*`).
const PLAIN_PIPES: &[&str] = &["pipe", "pipe-no-clear", "pipe-and-cancel"];

/// How long a pipe command may run before its process group is killed.
// ponytail: tmux never kills a copy-pipe job. A clipboard tool that stays
// to serve the selection (`xclip`, `wl-copy`) forks and lets the shell
// exit first, so only a hung command reaches this.
pub const PIPE_TIMEOUT: Duration = Duration::from_secs(60);

/// A pipe command, or `None` if `name` is not one (or its arguments do
/// not fit, which tmux ignores). The copy is queued with its command for
/// [`run_pipes`]; an empty selection queues the command alone.
pub fn command(mode: &mut Mode, name: &str, args: &[String], ctx: &Ctx<'_>) -> Option<Action> {
    if PLAIN_PIPES.contains(&name) {
        return plain(mode, name, args);
    }
    let copy = PIPES.iter().find(|(pipe, _)| *pipe == name)?.1;
    let (flags, rest) = select::parse(args, "CP", 2)?;
    let mut copy_args = Vec::new();
    if !flags.is_empty() {
        copy_args.push(format!("-{flags}"));
    }
    copy_args.push("--".to_string());
    copy_args.extend(rest.get(1).cloned());
    let before = mode.sel.copied.len();
    let action = select::command(mode, copy, &copy_args, ctx)?;
    // A copy too large to make runs no command either.
    if mode.sel.error.is_some() {
        return Some(action);
    }
    let pipe = rest.first().cloned().unwrap_or_default();
    match mode.sel.copied.get_mut(before) {
        Some(copied) => copied.pipe = Some(pipe),
        None => mode.sel.copied.push(Copied {
            data: Vec::new(),
            append: false,
            set_paste: false,
            set_clip: false,
            pipe: Some(pipe),
        }),
    }
    Some(action)
}

/// `pipe`, `pipe-no-clear` and `pipe-and-cancel`: the selection (or the
/// search match under the cursor) goes to the command, or to `copy-command`
/// when it is empty; nothing is stored. An empty selection still runs it.
fn plain(mode: &mut Mode, name: &str, args: &[String]) -> Option<Action> {
    let (_, rest) = select::parse(args, "", 1)?;
    match select::get_selection(mode) {
        Ok(data) => mode.sel.copied.push(Copied {
            data: data.unwrap_or_default(),
            append: false,
            set_paste: false,
            set_clip: false,
            pipe: Some(rest.first().cloned().unwrap_or_default()),
        }),
        Err(e) => mode.sel.error = Some(e),
    }
    Some(match name {
        "pipe-no-clear" => Action::Nothing,
        "pipe" => {
            select::clear_selection(mode);
            Action::Redraw
        }
        _ => {
            select::clear_selection(mode);
            Action::Cancel
        }
    })
}

/// `args` with a pipe command's command expanded against `pane`, as tmux
/// expands it before copying (so `#{selection_present}` still reads 1);
/// any other command's `args` unchanged. A `--` goes before the expanded
/// text, so text that reads as a flag (a pane title of `-C`) stays the
/// command, as tmux expands after parsing.
pub fn expand(state: &State, pane: PaneId, args: &[String]) -> Vec<String> {
    let mut args = args.to_vec();
    if !args.first().is_some_and(|name| {
        PLAIN_PIPES.contains(&name.as_str()) || PIPES.iter().any(|(pipe, _)| pipe == name)
    }) {
        return args;
    }
    let mut at = 1;
    while let Some(arg) = args.get(at) {
        if arg == "--" {
            at += 1;
            break;
        }
        if !arg.starts_with('-') || arg.len() == 1 {
            break;
        }
        at += 1;
    }
    if let Some(arg) = args.get(at) {
        let vars = FormatCtx::new(&state.model, pane);
        let expanded = cmd::expand(arg, &ServerVars::new(state, pane, &vars));
        args[at] = expanded;
        if args[at - 1] != "--" {
            args.insert(at, "--".to_string());
        }
    }
    args
}

/// Starts the commands `copied` carries, for `pane`'s copy.
pub fn run_pipes(state: &State, pane: PaneId, copied: &[Copied]) {
    for c in copied {
        let Some(command) = &c.pipe else {
            continue;
        };
        let command = match command.as_str() {
            "" => state
                .model
                .options
                .lookup("copy-command", &[])
                .unwrap_or_default()
                .to_string(),
            _ => command.clone(),
        };
        if command.is_empty() {
            continue;
        }
        let env = job_env(state, pane);
        let data = c.data.clone();
        tokio::spawn(async move {
            run(&command, env, data, PIPE_TIMEOUT).await;
        });
    }
}

/// The environment a pipe command of `pane` gets: its session's, as
/// `run-shell`'s, with `GROFE` naming this server and no pane.
fn job_env(state: &State, pane: PaneId) -> Vec<(String, String)> {
    let session = state
        .model
        .panes
        .get(&pane)
        .and_then(|p| state.model.link_session(p.window, None));
    let mut env = hooks::shell_env(state, session);
    env.retain(|(k, _)| !matches!(k.as_str(), "TMUX_PANE" | "GROFE" | "GROFE_PANE"));
    // As a pane: a `grofe` run inside talks to this server.
    env.push(("GROFE".to_string(), state.socket.display().to_string()));
    env
}

/// Runs `command` under `/bin/sh -c` in a process group of its own with
/// `data` on stdin, then closes it; waits for the shell, killing its group
/// at `timeout`. The exit status, or `None` if it could not run.
pub async fn run(
    command: &str,
    env: Vec<(String, String)>,
    data: Vec<u8>,
    timeout: Duration,
) -> Option<ExitStatus> {
    let child = {
        let _lock = pane::spawn_lock();
        tokio::process::Command::new("/bin/sh")
            // As tmux's `job_run`: `argv[0]` is `sh`, not the path.
            .arg0("sh")
            .arg("-c")
            .arg(command)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
    };
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("grofe copy-pipe {command:?}: {e}");
            return None;
        }
    };
    let group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .map(Pid::from_raw);
    let stdin = child.stdin.take();
    let work = async {
        // A command that exits without reading closes the pipe: the
        // write fails and is dropped, as tmux drops it.
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(&data).await;
        }
        child.wait().await
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(status) => status.ok(),
        Err(_) => {
            // The shell is not reaped yet, so its group id is still its own.
            if let Some(group) = group {
                let _ = killpg(group, Signal::SIGKILL);
            }
            child.wait().await.ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::server::copy::{Grid, run as run_command};

    fn mode(rows: &[&str]) -> Mode {
        Mode::new(Grid::from_text(20, 0, rows), (0, 0), true)
    }

    fn x(m: &mut Mode, words: &[&str]) -> Action {
        let args: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        run_command(m, &args, &Ctx { separators: "" })
    }

    #[test]
    fn a_pipe_queues_its_copy_with_its_command() {
        let mut m = mode(&["abc", "def"]);
        x(&mut m, &["select-line"]);
        assert_eq!(
            x(&mut m, &["copy-pipe-and-cancel", "-P", "cmd"]),
            Action::Cancel
        );
        let copied = std::mem::take(&mut m.sel.copied);
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].data, b"abc\n");
        assert!(!copied[0].set_paste && copied[0].set_clip);
        assert_eq!(copied[0].pipe.as_deref(), Some("cmd"));
    }

    #[test]
    fn an_empty_selection_queues_the_command_alone() {
        let mut m = mode(&["abc"]);
        x(&mut m, &["copy-pipe"]);
        let copied = std::mem::take(&mut m.sel.copied);
        assert_eq!(copied.len(), 1);
        assert!(copied[0].data.is_empty() && !copied[0].set_paste && !copied[0].set_clip);
        assert_eq!(copied[0].pipe.as_deref(), Some(""));
    }

    #[test]
    fn plain_pipes_queue_the_selection_for_a_command_and_store_nothing() {
        let mut m = mode(&["abc", "def"]);
        x(&mut m, &["select-line"]);
        assert_eq!(x(&mut m, &["pipe-no-clear", "cmd"]), Action::Nothing);
        assert!(m.selecting, "kept");
        assert_eq!(x(&mut m, &["pipe", "cmd2"]), Action::Redraw);
        assert!(!m.selecting, "cleared");
        x(&mut m, &["select-line"]);
        assert_eq!(x(&mut m, &["pipe-and-cancel"]), Action::Cancel);
        let copied = std::mem::take(&mut m.sel.copied);
        assert_eq!(copied.len(), 3);
        for (c, pipe) in copied.iter().zip(["cmd", "cmd2", ""]) {
            assert_eq!(c.data, b"abc\n");
            assert!(!c.set_paste && !c.set_clip && !c.append);
            assert_eq!(c.pipe.as_deref(), Some(pipe));
        }
    }

    #[test]
    fn plain_pipes_take_no_flags_and_one_word() {
        let mut m = mode(&["abc"]);
        x(&mut m, &["select-line"]);
        assert_eq!(x(&mut m, &["pipe", "-C", "cmd"]), Action::Nothing);
        assert_eq!(x(&mut m, &["pipe", "a", "b"]), Action::Nothing);
        assert!(m.sel.copied.is_empty() && m.selecting);
        assert_eq!(x(&mut m, &["pipe", "--", "-C"]), Action::Redraw);
        assert_eq!(m.sel.copied[0].pipe.as_deref(), Some("-C"));
    }

    #[test]
    fn three_positionals_do_nothing() {
        let mut m = mode(&["abc"]);
        x(&mut m, &["select-line"]);
        assert_eq!(
            x(&mut m, &["copy-pipe-and-cancel", "a", "b", "c"]),
            Action::Nothing
        );
        assert!(m.sel.copied.is_empty());
        assert!(m.selecting);
    }

    /// As tmux's `window_copy_pipe` (`job_run` with `JOB_NOWAIT`): `/bin/sh`
    /// with `argv[0]` `sh`.
    #[tokio::test]
    async fn the_command_runs_bin_sh_as_sh() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let command = format!("echo \"$0\" > '{}'", out.display());
        let status = run(&command, vec![], Vec::new(), PIPE_TIMEOUT).await;
        assert!(status.is_some_and(|s| s.success()));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "sh\n");
    }

    #[tokio::test]
    async fn the_selection_goes_to_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let command = format!("cat > '{}'", out.display());
        let status = run(&command, vec![], b"$(x) `y`\n".to_vec(), PIPE_TIMEOUT).await;
        assert!(status.is_some_and(|s| s.success()));
        assert_eq!(std::fs::read(&out).unwrap(), b"$(x) `y`\n");
    }

    /// Whether `pid` is gone: no such process, or a zombie. A process
    /// killed with its shell is reparented to pid 1, which in a container
    /// (act's `tail -f /dev/null`) never reaps it, and a zombie still
    /// answers `kill(pid, 0)`.
    fn gone(pid: i32) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        let stat = String::from_utf8_lossy(&out.stdout);
        stat.trim().is_empty() || stat.trim_start().starts_with('Z')
    }

    /// Runs `command` (which writes a background process's pid to the
    /// file `<pid>` names) with 1 MiB on stdin and a 500 ms timeout, and
    /// asserts it is stopped at the timeout and that background process
    /// killed with it. Returns the shell's exit status.
    async fn killed_at_the_timeout(command: &str) -> Option<ExitStatus> {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let command = command.replace("<pid>", &format!("'{}'", pid.display()));
        let started = Instant::now();
        let data = vec![b'x'; 1 << 20];
        let status = run(&command, vec![], data, Duration::from_millis(500)).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: i32 = std::fs::read_to_string(&pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !gone(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let survived = !gone(pid);
        if survived {
            let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
        assert!(!survived, "the background sleep survived: {command}");
        status
    }

    /// A command that never exits, and one it left running, are killed
    /// at the timeout and the shell reaped; a command that reads nothing
    /// does not hold up the write.
    #[tokio::test]
    async fn a_hung_command_is_killed_at_the_timeout() {
        let status = killed_at_the_timeout("sleep 1000 & echo $! > <pid>; wait").await;
        assert!(status.is_some_and(|s| !s.success()), "{status:?}");
    }

    /// The shell exits at once, but what it left running holds stdin open
    /// without reading it, so the write never ends: the group is still
    /// killed at the timeout. (Through fd 3: dash gives a background job
    /// `/dev/null` for stdin first, so its `<&0` would be that.) The shell is not reaped until the write is
    /// done, so its group id cannot have been reused.
    #[tokio::test]
    async fn a_command_the_shell_left_holding_stdin_is_killed() {
        let status =
            killed_at_the_timeout("exec 3<&0; sleep 1000 <&3 & echo $! > <pid>; exit 0").await;
        assert!(
            status.is_some_and(|s| s.success()),
            "the shell's own exit: {status:?}"
        );
    }
}
