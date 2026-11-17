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
    let mut child = Command::new(exe)
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

/// A link to saved machine `m` over ssh that never prompts.
// ponytail: `transport = "quic"` is not followed here yet; every saved
// machine is reached over ssh until the QUIC transport is wired in.
pub fn machine_link(m: &Machine) -> Result<Link, String> {
    let transport = Transport::Ssh(Target::parse_host(&m.target)?);
    Link::connect(&transport, ConnectOpts { batch: true }).map_err(|e| e.to_string())
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

/// `agent list --all-machines`: `argv` (the `agent list ...` words without
/// `--all-machines`) on the local server and on every enabled machine at
/// once, each waited for at most `timeout`. A reachable machine's output is
/// kept as its last known agents; an unreachable one shows those, marked
/// offline. With no local server the local part is empty.
pub fn all_agents(
    socket: &Path,
    machines: &[Machine],
    hello: &str,
    argv: &[String],
    state_dir: &Path,
    timeout: Duration,
) -> String {
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
                let got = bounded(timeout, move || machine_link(&m)?.request(&hello, &argv));
                let _ = tx.send(got);
            });
            (name, rx)
        })
        .collect();
    let mut listings = vec![(
        LOCAL.to_string(),
        AgentListing::Online(local.join().ok().and_then(Result::ok).unwrap_or_default()),
    )];
    for (name, rx) in remotes {
        let listing = match rx.recv().ok().flatten() {
            Some(Ok(text)) => {
                let text = sanitize_lines(&text);
                let _ = super::save_last_known(state_dir, &name, &text);
                AgentListing::Online(text)
            }
            _ => AgentListing::Offline(super::last_known(state_dir, &name)),
        };
        listings.push((name, listing));
    }
    combined_agent_list(&listings)
}

/// `grofe agent list --all-machines [-F format]`: `args` is the whole argv
/// from `agent` on.
pub fn all_agents_cmd(socket: &Path, hello: &str, args: &[String]) -> io::Result<String> {
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
