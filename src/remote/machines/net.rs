//! The networked machine parts (spec ssh-remote §5): a [`Link`] to every
//! machine (the local server through a `grofe proxy` child, saved machines
//! over ssh), their views, and `agent list --all-machines`.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use super::{AgentListing, AgentView, LOCAL, Machine, combined_agent_list};
use crate::remote::connect::{ConnectOpts, Link, Transport, kill_child};
use crate::remote::ssh::Target;

/// How long `agent list --all-machines` waits for one machine.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// The format the views and the last-known cache read: `agent list`'s
/// default.
pub const AGENT_FORMAT: &str = "#{pane_id} #{agent_kind} #{agent_state}";

/// A link to the local server at `socket`: `grofe -S <socket> proxy` as a
/// child on pipes, so the local machine is reached like any other. The
/// proxy starts the server when none runs.
pub fn local_link(socket: &Path) -> Result<Link, String> {
    let exe = std::env::current_exe().map_err(|e| format!("finding grofe: {e}"))?;
    let mut cmd = Command::new(exe);
    // The attach client blocks signals on its threads; a proxy started
    // from a reconnect must not inherit that mask.
    crate::remote::ssh::unblock_signals(&mut cmd);
    let mut child = cmd
        .arg("-S")
        .arg(socket)
        .arg("proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("starting the local proxy: {e}"))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("the local proxy has no stdio".to_string());
    };
    Link::over(LOCAL, stdout, stdin, Some(kill_child(child))).map_err(|e| e.to_string())
}

/// A control link to saved machine `m`, over ssh, that never prompts.
/// Always ssh, also for a machine saved with `quic`: a QUIC bootstrap
/// starts a new proxy there, which stops the one behind a running attach.
pub fn machine_link(m: &Machine) -> Result<Link, String> {
    Link::connect(&control_transport(m)?, ConnectOpts { batch: true }).map_err(|e| e.to_string())
}

/// How a control link reaches saved machine `m`: ssh, whatever it was
/// saved with.
pub fn control_transport(m: &Machine) -> Result<Transport, String> {
    Ok(Transport::Ssh(Target::parse_host(&m.target)?))
}

/// The agents in `agent list` output in [`AGENT_FORMAT`]: the kind, or the
/// pane id when the agent reported none, and the state.
pub fn parse_agents(text: &str) -> Vec<AgentView> {
    text.lines()
        .filter_map(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            match words.as_slice() {
                [_, kind, state] => Some(AgentView {
                    name: kind.to_string(),
                    state: state.to_string(),
                }),
                [pane, state] => Some(AgentView {
                    name: pane.to_string(),
                    state: state.to_string(),
                }),
                _ => None,
            }
        })
        .collect()
}

/// Remote-controlled text made safe to print, line by line
/// ([`crate::remote::ssh::sanitize`]).
pub fn sanitize_lines(text: &str) -> String {
    text.lines()
        .map(|l| crate::remote::ssh::sanitize(l) + "\n")
        .collect()
}

/// `f` on its own thread, waited for at most `timeout`; `None` past it
/// (the thread is left to finish on its own).
fn bounded<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout).ok()
}

/// `agent list --all-machines` as it comes out: the table for stdout and
/// the errors for stderr.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AllAgents {
    pub table: String,
    /// `<machine>: <error>`, sanitized: a server that answered with an
    /// error (a bad flag), or a local server that could not be reached.
    pub errors: Vec<String>,
}

/// What asking one machine came to.
enum Asked {
    /// The command's output, or the error the server answered with.
    Answered(Result<String, String>),
    /// No link in time: the machine is offline.
    Unreachable,
}

/// `agent list --all-machines`: `argv` (the `agent list ...` words without
/// `--all-machines`) on the local server and on every enabled machine at
/// once, each waited for at most `timeout`. A reachable machine's output is
/// kept as its last known agents when it is in the default format (no
/// `-F`, the format the picker reads back); an unreachable one shows those,
/// marked offline. A machine that answered with an error is left out of the
/// table and its error reported. With no local server the local part is
/// empty.
pub fn all_agents(
    socket: &Path,
    machines: &[Machine],
    hello: &str,
    argv: &[String],
    state_dir: &Path,
    timeout: Duration,
) -> AllAgents {
    let local = {
        let (socket, hello, argv) = (socket.to_path_buf(), hello.to_string(), argv.to_vec());
        std::thread::spawn(move || {
            if std::os::unix::net::UnixStream::connect(&socket).is_err() {
                return Ok(String::new());
            }
            local_link(&socket)?.request(&hello, &argv)
        })
    };
    let remotes: Vec<_> = machines
        .iter()
        .filter(|m| m.enabled)
        .map(|m| {
            let (m, hello, argv) = (m.clone(), hello.to_string(), argv.to_vec());
            let name = m.name.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let asked = bounded(timeout, move || match machine_link(&m) {
                    Ok(link) => Asked::Answered(link.request(&hello, &argv)),
                    Err(_) => Asked::Unreachable,
                });
                let _ = tx.send(asked.unwrap_or(Asked::Unreachable));
            });
            (name, rx)
        })
        .collect();
    let cache = !argv.iter().any(|a| a.starts_with("-F"));
    let mut out = AllAgents::default();
    let mut listings = Vec::new();
    match local.join().unwrap_or_else(|_| Err("panicked".to_string())) {
        Ok(text) => listings.push((LOCAL.to_string(), AgentListing::Online(text))),
        Err(e) => out.errors.push(error_line(LOCAL, &e)),
    }
    for (name, rx) in remotes {
        match rx.recv().unwrap_or(Asked::Unreachable) {
            Asked::Answered(Ok(text)) => {
                let text = sanitize_lines(&text);
                if cache {
                    let _ = super::save_last_known(state_dir, &name, &text);
                }
                listings.push((name, AgentListing::Online(text)));
            }
            Asked::Answered(Err(e)) => out.errors.push(error_line(&name, &e)),
            Asked::Unreachable => {
                let last = super::last_known(state_dir, &name);
                listings.push((name, AgentListing::Offline(last)));
            }
        }
    }
    out.table = combined_agent_list(&listings);
    out
}

/// One machine's error, as stderr shows it.
fn error_line(name: &str, err: &str) -> String {
    let err = crate::remote::ssh::sanitize(err.trim_end());
    format!("{}: {err}", crate::remote::ssh::sanitize(name))
}

/// `grofe agent list --all-machines [-F format]`: `args` is the whole argv
/// from `agent` on.
pub fn all_agents_cmd(socket: &Path, hello: &str, args: &[String]) -> io::Result<AllAgents> {
    let argv: Vec<String> = args
        .iter()
        .filter(|a| *a != "--all-machines")
        .cloned()
        .collect();
    let env = crate::config::Env::from_process();
    let machines = match env.config_dir {
        Some(dir) => super::load(&super::machines_path(&dir)).map_err(io::Error::other)?,
        None => Vec::new(),
    };
    let state = crate::paths::state_dir(&crate::paths::Env::from_process());
    Ok(all_agents(
        socket,
        &machines,
        hello,
        &argv,
        &state,
        LIST_TIMEOUT,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_parse_with_and_without_a_kind() {
        let got = parse_agents("%1 claude working\n%2 blocked\n\n");
        assert_eq!(
            got,
            vec![
                AgentView {
                    name: "claude".into(),
                    state: "working".into()
                },
                AgentView {
                    name: "%2".into(),
                    state: "blocked".into()
                },
            ]
        );
    }

    #[test]
    fn control_links_use_ssh_for_a_quic_machine_too() {
        let mut m = Machine {
            name: "q".into(),
            target: "box".into(),
            session: None,
            transport: Some(super::super::Transport::Quic),
            enabled: true,
        };
        assert_eq!(
            control_transport(&m),
            Ok(Transport::Ssh(Target::host("box")))
        );
        m.target = "-oProxyCommand=x".into();
        assert!(control_transport(&m).is_err());
    }

    #[test]
    fn remote_text_loses_its_controls() {
        assert_eq!(
            sanitize_lines("%1 claude\x1b[2Jdone\nok"),
            "%1 claude\\u{1b}[2Jdone\nok\n"
        );
    }

    #[test]
    fn bounded_gives_up_at_the_timeout() {
        let slow = bounded(Duration::from_millis(20), || {
            std::thread::sleep(Duration::from_secs(1));
            1
        });
        assert_eq!(slow, None);
        assert_eq!(bounded(Duration::from_secs(1), || 2), Some(2));
    }
}
