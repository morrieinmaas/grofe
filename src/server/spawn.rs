//! Running a string under `/bin/sh -c`, the way tmux's `job_run` does, for
//! `run-shell`, `pipe-pane`, `copy-pipe`, `#()` sources and prompts.

use std::io;
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::server::pane;

/// `/bin/sh -c script` with `argv[0]` `sh`, as tmux's `job_run`, and stderr
/// discarded. `env` replaces the environment when given and is inherited
/// when `None`. The caller sets stdio, the directory and process group, then
/// starts it with [`spawn`].
pub(crate) fn sh(script: &str, env: Option<Vec<(String, String)>>) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg0("sh").arg("-c").arg(script).stderr(Stdio::null());
    if let Some(env) = env {
        cmd.env_clear().envs(env);
    }
    cmd
}

/// Starts `cmd` holding the spawn lock, so no fork inherits a pty master
/// between its `openpty` and its `FD_CLOEXEC`.
pub(crate) fn spawn(cmd: &mut Command) -> io::Result<Child> {
    let _lock = pane::spawn_lock();
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn out(mut cmd: Command) -> String {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped());
        let child = spawn(&mut cmd).expect("spawn");
        let out = child.wait_with_output().await.expect("wait");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[tokio::test]
    async fn the_shell_is_called_sh_and_stderr_goes_nowhere() {
        assert_eq!(out(sh("echo $0; echo oops >&2", None)).await, "sh\n");
    }

    #[tokio::test]
    async fn given_env_replaces_the_environment_and_none_inherits_it() {
        let only = vec![("ONLY".to_string(), "x".to_string())];
        let got = out(sh("echo \"$ONLY${HOME:-none}\"", Some(only))).await;
        assert_eq!(got, "xnone\n");
        let inherited = out(sh("echo ${HOME:+set}", None)).await;
        assert_eq!(inherited, "set\n");
    }
}
