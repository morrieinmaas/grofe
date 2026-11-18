//! The roaming transport on the client (spec ssh-remote §4): `grofe -r host
//! --transport quic`. One ssh run starts `grofe proxy --quic` there and
//! brings back its bootstrap line; the link then runs over QUIC to the host
//! ssh would reach, so a changed network keeps the session (QUIC migration).
//! A link that drops anyway reconnects over QUIC with the same token, and
//! only goes back to ssh for a new bootstrap when that proxy is gone. When
//! UDP gets no answer the client says so and uses ssh instead.

use std::io;
use std::process::Stdio;
use std::sync::Mutex;

use super::bootstrap::Install;
use super::connect::{ConnectError, ConnectOpts, Link, Transport, classify};
use super::quic::{self, Bootstrap};
use super::ssh::{self, SshOpts, Target};

/// What the remote runs for a bootstrap.
const QUIC_ARGS: &str = "proxy --quic";

/// Why a bootstrap brought back no line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// ssh, or the remote grofe being missing or too old (as for the ssh
    /// transport).
    Connect(ConnectError),
    /// A grofe that has `proxy --quic` ran it and it failed there (no free
    /// port in `remote-udp-ports`, a bad `remote-quic-idle`, ...): what it
    /// said. ssh works, so the caller can fall back to it.
    Proxy(String),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Connect(e) => e.fmt(f),
            BootError::Proxy(why) => write!(f, "grofe proxy --quic failed there: {why}"),
        }
    }
}

/// The reason in a failed `proxy --quic`'s stderr: its line that starts
/// with [`quic::server::FAILED`], which only a grofe that knows the flag
/// prints (an old one says `takes no arguments`).
pub fn proxy_failure(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .find_map(|l| l.trim().strip_prefix(quic::server::FAILED))
        .map(|why| why.trim().to_string())
}

/// Runs `ssh <target> -- <remote grofe> proxy --quic` and reads the
/// bootstrap line. The proxy detaches, so ssh exits once the line is out.
/// A remote without grofe, or with one that has no `--quic`, reads as
/// [`ConnectError::Missing`] or [`ConnectError::Incompatible`], as for the
/// ssh transport; a `proxy --quic` that ran and failed is
/// [`BootError::Proxy`].
pub fn bootstrap(target: &Target, batch: bool) -> Result<Bootstrap, BootError> {
    let remote = ssh::remote_grofe(env!("CARGO_PKG_VERSION"), QUIC_ARGS);
    let opts = SshOpts {
        keepalive: ssh::Keepalive::default(),
        batch,
    };
    let out = ssh::command()
        .args(ssh::argv(target, opts, &remote))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            BootError::Connect(ConnectError::Failed(format!(
                "cannot run {}: {e}",
                ssh::program()
            )))
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success()
        && let Some(why) = proxy_failure(&stderr)
    {
        return Err(BootError::Proxy(why));
    }
    let parsed = match stdout.lines().next() {
        Some(line) => quic::parse_bootstrap(line),
        None => Err("no bootstrap line from grofe proxy --quic".to_string()),
    };
    let why = match parsed {
        Ok(b) if out.status.success() => return Ok(b),
        Ok(_) => "grofe proxy --quic failed".to_string(),
        Err(why) => why,
    };
    Err(BootError::Connect(classify(
        ConnectError::Failed(why),
        out.status.code(),
        &stderr,
    )))
}

/// `hostname` from `ssh -G` output: the host ssh connects to, after its
/// config's aliases.
pub fn ssh_g_hostname(ssh_g: &str) -> Option<String> {
    ssh_g.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        let key = words.next()?;
        let value = words.next()?;
        key.eq_ignore_ascii_case("hostname")
            .then(|| value.to_string())
    })
}

/// The host to dial over UDP for `target`: what ssh would connect to
/// (`ssh -G`), else the target's own host part.
pub fn dial_host(target: &Target) -> String {
    let mut cmd = ssh::command();
    cmd.arg("-G");
    if let Some(p) = target.port {
        cmd.args(["-p", &p.to_string()]);
    }
    cmd.arg("--")
        .arg(&target.dest)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    match cmd.output() {
        Ok(out) if out.status.success() => ssh_g_hostname(&String::from_utf8_lossy(&out.stdout)),
        _ => None,
    }
    .unwrap_or_else(|| target.host_name().to_string())
}

/// What one QUIC attempt came to.
enum Attempt {
    Linked(Link),
    /// No answer over UDP in time: blocked, or nothing listens there now.
    Silent(String),
    /// An answer, but no link: the proxy is gone, refused the token, or the
    /// certificate is not the pinned one.
    Refused(String),
}

fn attempt(host: &str, dial: &str, b: &Bootstrap) -> Attempt {
    let mut q = match quic::connect(dial, b) {
        Ok(q) => q,
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            return Attempt::Silent(ssh::sanitize(&e.to_string()));
        }
        Err(e) => return Attempt::Refused(ssh::sanitize(&e.to_string())),
    };
    let (reader, writer) = match q.streams() {
        Ok(s) => s,
        Err(e) => return Attempt::Refused(e.to_string()),
    };
    // The link's end closes the QUIC connection; it owns the runtime too.
    match Link::over(host, reader, writer, Some(Box::new(move || q.close()))) {
        Ok(link) => Attempt::Linked(link),
        Err(e) => Attempt::Refused(ssh::sanitize(&e.to_string())),
    }
}

fn blocked_line(host: &str, why: &str) -> String {
    format!(
        "grofe: {}: {}; using ssh instead",
        ssh::sanitize(host),
        ssh::sanitize(why)
    )
}

/// A roaming remote: what a reconnect needs to come back over QUIC.
pub struct Roaming {
    target: Target,
    dial: String,
    state: Mutex<State>,
}

struct State {
    /// The proxy's line, while it may still be there.
    boot: Option<Bootstrap>,
    /// UDP got no answer while ssh worked: ssh from now on.
    ssh_only: bool,
}

impl Roaming {
    /// The first link: a bootstrap (installing grofe there first when it
    /// has none, as `install` allows), then QUIC. When UDP gets no answer
    /// it prints one line saying so and links over ssh.
    pub fn connect(
        target: &Target,
        install: &Install,
        release_url: &str,
    ) -> Result<(Roaming, Link), String> {
        let first = match bootstrap(target, false) {
            Err(BootError::Connect(ConnectError::Failed(why))) => return Err(why),
            // No grofe there, or an old one: the ssh bootstrap installs it
            // (asking as it does), then the proxy runs from the install.
            Err(BootError::Connect(_)) => {
                drop(super::bootstrap::connect(target, install, release_url)?);
                bootstrap(target, false)
            }
            other => other,
        };
        let boot = match first {
            Ok(b) => b,
            Err(BootError::Proxy(why)) => return Roaming::over_ssh(target, &BootError::Proxy(why)),
            Err(e) => return Err(ssh::sanitize(&e.to_string())),
        };
        let roaming = Roaming {
            target: target.clone(),
            dial: dial_host(target),
            state: Mutex::new(State {
                boot: Some(boot.clone()),
                ssh_only: false,
            }),
        };
        let link = match attempt(target.host_name(), &roaming.dial, &boot) {
            Attempt::Linked(link) => link,
            Attempt::Silent(why) => {
                // ponytail: the proxy that got no client stays until
                // remote-quic-idle or the next `proxy --quic` there.
                eprintln!("{}", blocked_line(target.host_name(), &why));
                roaming.state().ssh_only = true;
                roaming.ssh(false)?
            }
            Attempt::Refused(why) => return Err(why),
        };
        Ok((roaming, link))
    }

    /// A roaming remote with no link yet: its first [`Roaming::relink`]
    /// bootstraps without prompting, and later ones reuse that token. For
    /// a caller that owns a terminal (the multi-machine client) and must
    /// neither prompt nor print.
    pub fn unlinked(target: &Target) -> Roaming {
        Roaming {
            target: target.clone(),
            dial: dial_host(target),
            state: Mutex::new(State {
                boot: None,
                ssh_only: false,
            }),
        }
    }

    /// The fallback when `proxy --quic` failed there: one line saying why,
    /// then ssh for this session.
    fn over_ssh(target: &Target, why: &BootError) -> Result<(Roaming, Link), String> {
        eprintln!("{}", blocked_line(target.host_name(), &why.to_string()));
        let roaming = Roaming {
            target: target.clone(),
            dial: target.host_name().to_string(),
            state: Mutex::new(State {
                boot: None,
                ssh_only: true,
            }),
        };
        let link = roaming.ssh(false)?;
        Ok((roaming, link))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn ssh(&self, batch: bool) -> Result<Link, String> {
        Link::connect(&Transport::Ssh(self.target.clone()), ConnectOpts { batch })
            .map_err(|e| ssh::sanitize(&e.to_string()))
    }

    /// A new link after a drop; it never prompts. First QUIC with the same
    /// token (the proxy outlives the drop); then, the proxy being gone or
    /// silent, a new bootstrap over ssh and QUIC with that; if UDP is
    /// silent while ssh works, ssh.
    // Ruling: nothing is printed here; the attach owns the terminal and
    // its reconnect bar says what is happening.
    pub fn relink(&self) -> Result<Link, String> {
        let host = self.target.host_name();
        let (boot, ssh_only) = {
            let s = self.state();
            (s.boot.clone(), s.ssh_only)
        };
        if ssh_only {
            return self.ssh(true);
        }
        if let Some(b) = boot
            && let Attempt::Linked(link) = attempt(host, &self.dial, &b)
        {
            return Ok(link);
        }
        let b = match bootstrap(&self.target, true) {
            Ok(b) => b,
            Err(BootError::Proxy(_)) => {
                self.state().ssh_only = true;
                return self.ssh(true);
            }
            Err(e) => return Err(ssh::sanitize(&e.to_string())),
        };
        self.state().boot = Some(b.clone());
        match attempt(host, &self.dial, &b) {
            Attempt::Linked(link) => Ok(link),
            Attempt::Silent(_) => {
                self.state().ssh_only = true;
                self.ssh(true)
            }
            Attempt::Refused(why) => Err(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_proxy_quic_is_told_from_an_old_grofe() {
        let failed = "proxy --quic: no free UDP port in 1-2: Address in use\n";
        assert_eq!(
            proxy_failure(failed).as_deref(),
            Some("no free UDP port in 1-2: Address in use")
        );
        assert_eq!(proxy_failure("grofe proxy: takes no arguments\n"), None);
        // The detached proxy's own log lines are not the failure line.
        assert_eq!(
            proxy_failure("grofe proxy --quic: using the built-in defaults\n"),
            None
        );
        assert_eq!(proxy_failure(""), None);
    }

    #[test]
    fn ssh_g_names_the_host_behind_an_alias() {
        let out = "user me\nhostname box.example.net\nport 22\n";
        assert_eq!(ssh_g_hostname(out).as_deref(), Some("box.example.net"));
        assert_eq!(
            ssh_g_hostname("HostName 10.0.0.2\n").as_deref(),
            Some("10.0.0.2")
        );
        assert_eq!(ssh_g_hostname("hostkeyalias x\nport 22\n"), None);
    }

    #[test]
    fn the_bootstrap_command_runs_proxy_quic() {
        let cmd = ssh::remote_grofe("1.2.3", QUIC_ARGS);
        assert_eq!(cmd.matches("proxy --quic").count(), 2, "{cmd}");
    }
}
