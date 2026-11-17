//! Where a remote session lives and how ssh reaches it (spec ssh-remote
//! §2): `grofe -r <host>` and `ssh://[user@]host[:port][/session]` targets,
//! and the `ssh` command lines the client runs.

use std::process::{Command, Stdio};

/// The environment variable naming the ssh program (default `ssh`). Tests
/// point it at a fake ssh that runs the remote command locally.
pub const SSH_ENV: &str = "GROFE_SSH";

/// The exit status of the remote command when no grofe is found there.
pub const MISSING_STATUS: i32 = 127;

/// The line the remote command prints on stderr when no grofe is found.
pub const MISSING_MARKER: &str = "grofe-remote: grofe not found";

/// A remote host as ssh takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `host` or `user@host`: anything ssh accepts, aliases included.
    pub dest: String,
    pub port: Option<u16>,
    /// The session an `ssh://` URL names.
    pub session: Option<String>,
}

impl Target {
    /// `grofe -r <host>`: the host exactly as given.
    pub fn host(dest: &str) -> Target {
        Target {
            dest: dest.to_string(),
            port: None,
            session: None,
        }
    }

    /// The host part of `dest`, without a user, for messages.
    pub fn host_name(&self) -> &str {
        self.dest.rsplit_once('@').map_or(&self.dest, |(_, h)| h)
    }
}

/// `ssh://[user@]host[:port][/session]`; an IPv6 host is in brackets.
pub fn parse_url(url: &str) -> Result<Target, String> {
    let bad = |why: &str| format!("bad ssh URL {url:?}: {why}");
    let rest = url
        .strip_prefix("ssh://")
        .ok_or_else(|| bad("it must start with ssh://"))?;
    let (authority, session) = match rest.split_once('/') {
        Some((a, s)) => (a, (!s.is_empty()).then(|| s.to_string())),
        None => (rest, None),
    };
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) if !u.is_empty() => (Some(u), h),
        Some(_) => return Err(bad("empty user")),
        None => (None, authority),
    };
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let (host, after) = inner
            .split_once(']')
            .ok_or_else(|| bad("unclosed [ in the host"))?;
        match after {
            "" => (host, None),
            _ => match after.strip_prefix(':') {
                Some(p) => (host, Some(p)),
                None => return Err(bad("text after ]")),
            },
        }
    } else {
        match hostport.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        }
    };
    if host.is_empty() {
        return Err(bad("no host"));
    }
    let port = match port {
        None => None,
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 => Some(n),
            _ => return Err(bad(&format!("bad port {p:?}"))),
        },
    };
    let dest = match user {
        Some(u) => format!("{u}@{host}"),
        None => host.to_string(),
    };
    Ok(Target {
        dest,
        port,
        session,
    })
}

/// The ssh program: `$GROFE_SSH`, else `ssh`.
pub fn program() -> String {
    std::env::var(SSH_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ssh".to_string())
}

/// Whether `ssh -G` output shows the user set `ServerAliveInterval`
/// (`ssh -G` prints defaults too; `0` is the unset default).
pub fn keepalive_set(ssh_g: &str) -> bool {
    ssh_g.lines().any(|l| {
        let mut words = l.split_whitespace();
        words
            .next()
            .is_some_and(|k| k.eq_ignore_ascii_case("serveraliveinterval"))
            && words.next().is_some_and(|v| v != "0")
    })
}

/// Options for one ssh run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SshOpts {
    /// Add `ServerAliveInterval=15` and `ServerAliveCountMax=2`.
    pub keepalive: bool,
    /// `BatchMode=yes`: never prompt (a reconnect runs with the tty raw).
    pub batch: bool,
}

/// `ssh -T [-p port] [-o ...] <dest> -- <remote command>`.
pub fn argv(target: &Target, opts: SshOpts, remote: &str) -> Vec<String> {
    let mut out = vec!["-T".to_string()];
    if let Some(p) = target.port {
        out.extend(["-p".to_string(), p.to_string()]);
    }
    if opts.keepalive {
        out.extend(
            [
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
            ]
            .map(String::from),
        );
    }
    if opts.batch {
        out.extend(["-o", "BatchMode=yes"].map(String::from));
    }
    out.extend([target.dest.clone(), "--".to_string(), remote.to_string()]);
    out
}

/// Whether to add the keepalive options for `target`: only when `ssh -G`
/// shows the user set no `ServerAliveInterval`. If `ssh -G` fails, ssh's
/// own run will report why.
pub fn wants_keepalive(target: &Target) -> bool {
    let mut cmd = Command::new(program());
    cmd.arg("-G");
    if let Some(p) = target.port {
        cmd.args(["-p", &p.to_string()]);
    }
    cmd.arg(&target.dest)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    match cmd.output() {
        Ok(out) if out.status.success() => !keepalive_set(&String::from_utf8_lossy(&out.stdout)),
        _ => true,
    }
}

/// `s` as one single-quoted shell word.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The directory, relative to the remote `$HOME`, a bootstrap installs
/// grofe `version` into.
pub fn install_dir(version: &str) -> String {
    format!(".local/share/grofe/bin/{version}")
}

/// The remote command that runs `grofe proxy`: wrapped in `sh -c` so it
/// works whatever the remote login shell is.
///
/// Ruling: the versioned install is preferred over a `grofe` on `PATH`
/// when it exists, because a `PATH` grofe that is incompatible would
/// otherwise win every time after a bootstrap installed the right one.
/// Cost: a user who upgrades the `PATH` grofe keeps using the installed
/// one until it is removed.
pub fn proxy_command(version: &str) -> String {
    let script = format!(
        "b=\"$HOME/{dir}/grofe\"; \
         if [ -x \"$b\" ]; then exec \"$b\" proxy; fi; \
         if command -v grofe >/dev/null 2>&1; then exec grofe proxy; fi; \
         echo {marker} >&2; exit {MISSING_STATUS}",
        dir = install_dir(version),
        marker = shell_quote(MISSING_MARKER),
    );
    format!("sh -c {}", shell_quote(&script))
}

/// The remote command that stores its stdin as the versioned grofe,
/// atomically (a temporary name, then `mv`).
pub fn install_command(version: &str) -> String {
    let script = format!(
        "d=\"$HOME/{dir}\"; mkdir -p \"$d\" && cat > \"$d/grofe.tmp\" && \
         chmod 755 \"$d/grofe.tmp\" && mv \"$d/grofe.tmp\" \"$d/grofe\"",
        dir = install_dir(version),
    );
    format!("sh -c {}", shell_quote(&script))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Target {
        parse_url(s).unwrap()
    }

    #[test]
    fn urls_with_user_port_and_session_parse() {
        assert_eq!(url("ssh://box"), Target::host("box"));
        assert_eq!(
            url("ssh://me@box:2222/work"),
            Target {
                dest: "me@box".into(),
                port: Some(2222),
                session: Some("work".into()),
            }
        );
        assert_eq!(url("ssh://box/").session, None);
        assert_eq!(url("ssh://box/a/b").session.as_deref(), Some("a/b"));
    }

    #[test]
    fn an_ipv6_host_is_in_brackets() {
        let t = url("ssh://me@[::1]:22/s");
        assert_eq!((t.dest.as_str(), t.port), ("me@::1", Some(22)));
        assert_eq!(url("ssh://[fe80::1]").dest, "fe80::1");
        assert!(parse_url("ssh://[::1").is_err());
        assert!(parse_url("ssh://[::1]x").is_err());
    }

    #[test]
    fn bad_urls_are_refused() {
        for bad in [
            "box",
            "ssh://",
            "ssh://box:",
            "ssh://box:0",
            "ssh://box:99999",
            "ssh://box:x",
            "ssh://@box",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_name_drops_the_user() {
        assert_eq!(url("ssh://me@box").host_name(), "box");
        assert_eq!(Target::host("box").host_name(), "box");
    }

    #[test]
    fn keepalive_is_read_from_ssh_g() {
        assert!(!keepalive_set("user me\nserveraliveinterval 0\nport 22\n"));
        assert!(keepalive_set("serveraliveinterval 30\n"));
        assert!(!keepalive_set("port 22\n"));
    }

    #[test]
    fn ssh_argv_adds_keepalives_only_when_asked() {
        let t = url("ssh://me@box:2222");
        let with = argv(
            &t,
            SshOpts {
                keepalive: true,
                batch: false,
            },
            "cmd",
        );
        assert_eq!(
            with,
            [
                "-T",
                "-p",
                "2222",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "me@box",
                "--",
                "cmd"
            ]
        );
        let without = argv(&Target::host("box"), SshOpts::default(), "cmd");
        assert_eq!(without, ["-T", "box", "--", "cmd"]);
        let batch = argv(
            &Target::host("box"),
            SshOpts {
                keepalive: false,
                batch: true,
            },
            "cmd",
        );
        assert_eq!(batch, ["-T", "-o", "BatchMode=yes", "box", "--", "cmd"]);
    }

    #[test]
    fn the_proxy_command_prefers_the_versioned_install_then_path() {
        let cmd = proxy_command("1.2.3");
        assert!(cmd.starts_with("sh -c '"), "{cmd}");
        let versioned = cmd.find(".local/share/grofe/bin/1.2.3/grofe").unwrap();
        let path = cmd.find("exec grofe proxy").unwrap();
        assert!(versioned < path);
        assert!(cmd.contains("exit 127"));
    }

    /// The commands really run under `sh`: no grofe anywhere gives the
    /// marker and 127; an installed one is run.
    #[test]
    fn the_proxy_and_install_commands_run_under_sh() {
        let home = tempfile::tempdir().unwrap();
        let run = |cmd: &str, input: &[u8]| {
            use std::io::Write;
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg(cmd)
                .env("HOME", home.path())
                .env("PATH", "/usr/bin:/bin")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            child.wait_with_output().unwrap()
        };
        let out = run(&proxy_command("9.9.9"), b"");
        assert_eq!(out.status.code(), Some(MISSING_STATUS));
        assert!(String::from_utf8_lossy(&out.stderr).contains(MISSING_MARKER));

        let out = run(
            &install_command("9.9.9"),
            b"#!/bin/sh\necho proxy-ran \"$@\"\n",
        );
        assert!(out.status.success(), "{out:?}");
        let out = run(&proxy_command("9.9.9"), b"");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "proxy-ran proxy\n");
    }

    #[test]
    fn shell_quote_survives_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
