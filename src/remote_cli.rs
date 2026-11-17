//! `grofe -r <host>`, `attach ssh://...` and `--machines`: the binary's
//! side of `grofe::remote`, which runs a command on a remote server through
//! the proxy (spec ssh-remote §2) the way the local CLI runs it here.

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use grofe::client;
use grofe::paths::Env;
use grofe::protocol::{ATTACH_CHECK, ClientCtx, Dialect, Hello, Reply, encode_hello};
use grofe::remote::ssh::Target;
use grofe::remote::{forward, session};

use super::{
    FEATURES, Globals, RemoteFlags, attach_argv_for_existing, attaches, dialect, fail, print_reply,
    request_with, session_gone,
};

/// A call for a remote host: `-r <host> [command...]`, or `attach
/// ssh://[user@]host[:port][/session]` (spec ssh-remote §2).
pub(super) fn remote_route(globals: &Globals, rest: &[String]) -> Option<ExitCode> {
    let url = rest
        .first()
        .filter(|v| grofe::cmd::parse::canonical_name(v) == "attach-session")
        .and_then(|_| rest[1..].iter().position(|a| a.starts_with("ssh://")))
        .map(|i| i + 1);
    let host = match (&globals.remote.host, &globals.remote.machine) {
        (Some(_), Some(_)) => return Some(fail("grofe: -r and --machine cannot go together")),
        // `--machine local`: this machine, as without it.
        (None, Some(m)) if m == grofe::remote::machines::LOCAL => None,
        (None, Some(m)) if grofe::remote::machines::saved(m).is_none() => {
            return Some(fail(&format!(
                "grofe: no machine named '{m}' (grofe machine ls)"
            )));
        }
        (host, machine) => host.as_ref().or(machine.as_ref()),
    };
    match (host, url) {
        (None, None) => None,
        (Some(_), Some(_)) => Some(fail("grofe: -r and an ssh:// target cannot go together")),
        (Some(host), None) => {
            // A saved machine's name stands for its target, session and
            // transport; anything else is an ssh target.
            let saved = grofe::remote::machines::saved(host);
            let target = saved.as_ref().map_or(host.as_str(), |m| m.target.as_str());
            let session = saved.as_ref().and_then(|m| m.session.as_deref());
            // Without a command, `-r host` means `new-session -A -s main`.
            let argv = if rest.is_empty() {
                ["new-session", "-A", "-s", session.unwrap_or("main")]
                    .map(String::from)
                    .to_vec()
            } else {
                rest.to_vec()
            };
            let transport = saved.as_ref().and_then(|m| m.transport).map(|t| t.as_str());
            match Target::parse_host(target) {
                Ok(target) => Some(remote_main(&target, &globals.remote, transport, &argv)),
                Err(e) => Some(fail(&format!("grofe: {e}"))),
            }
        }
        (None, Some(i)) => {
            let target = match grofe::remote::ssh::parse_url(&rest[i]) {
                Ok(t) => t,
                Err(e) => return Some(fail(&e)),
            };
            let mut argv: Vec<String> = rest
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, a)| a.clone())
                .collect();
            if let Some(session) = &target.session {
                argv.extend(["-t".to_string(), session.clone()]);
            }
            Some(remote_main(&target, &globals.remote, None, &argv))
        }
    }
}

/// The multi-machine client (spec ssh-remote §5): `grofe --machines`, or
/// bare `grofe` on a terminal outside a pane with enabled machines saved.
pub(super) fn machines_route(globals: &Globals, rest: &[String]) -> Option<ExitCode> {
    if dialect() != Dialect::Grofe
        || globals.remote.host.is_some()
        || (!globals.remote.machines && !rest.is_empty())
    {
        return None;
    }
    let machines = grofe::remote::machines::saved_all();
    let bare = rest.is_empty()
        && std::env::var_os("GROFE").is_none_or(|v| v.is_empty())
        && io::stdin().is_terminal()
        && machines.iter().any(|m| m.enabled);
    if !globals.remote.machines && !bare {
        return None;
    }
    if !rest.is_empty() {
        return Some(fail("grofe --machines: takes no command"));
    }
    if !io::stdin().is_terminal() {
        return Some(fail("open terminal failed: not a terminal"));
    }
    let hello = match remote_hello() {
        Ok(h) => h,
        Err(e) => return Some(fail(&e)),
    };
    let opts = client::AttachOptions {
        features: FEATURES.get().cloned().unwrap_or_default(),
        ..Default::default()
    };
    Some(print_attach(grofe::remote::machines::client::run(
        &globals.socket(),
        &machines,
        hello,
        remote_ctx(),
        opts,
        std::env::var("TERM").unwrap_or_else(|_| "unknown".to_string()),
        local_option("remote-reconnect").is_none_or(|v| v == "on"),
    )))
}

/// The local config's value of option `name`, or its default.
fn local_option(name: &str) -> Option<String> {
    grofe::config::load(&grofe::config::Env::from_process())
        .ok()
        .and_then(|r| r.options.get(name).cloned())
}

/// Runs `argv` on the remote: attach-type commands attach through an
/// attach channel, everything else is one control request printed as the
/// local CLI prints it.
fn remote_main(
    target: &Target,
    flags: &RemoteFlags,
    saved_transport: Option<&str>,
    argv: &[String],
) -> ExitCode {
    if argv == ["ports"] {
        return remote_ports(target.host_name());
    }
    let transport = flags
        .transport
        .clone()
        .or_else(|| saved_transport.map(String::from))
        .or_else(|| local_option("remote-transport"))
        .unwrap_or_else(|| "ssh".to_string());
    let release_url = grofe::remote::bootstrap::release_url(local_option("remote-release-url"));
    let connected = match transport.as_str() {
        "ssh" => grofe::remote::bootstrap::connect(target, &flags.install, &release_url)
            .map(|link| (link, None)),
        "quic" => grofe::remote::roam::Roaming::connect(target, &flags.install, &release_url)
            .map(|(roaming, link)| (link, Some(std::sync::Arc::new(roaming)))),
        other => return fail(&format!("grofe: unknown transport {other:?} (ssh or quic)")),
    };
    let (link, roaming) = match connected {
        Ok(c) => c,
        Err(e) => return fail(&format!("grofe: {}", grofe::remote::ssh::sanitize(&e))),
    };
    let hello = match remote_hello() {
        Ok(h) => h,
        Err(e) => return fail(&e),
    };
    let remote = Remote {
        link,
        target: target.clone(),
        hello,
        roaming,
    };
    match grofe::cmd::parse::canonical_name(&argv[0]) {
        "attach-session" => print_attach(remote.attach(argv, false)),
        "new-session" => remote.new_session(argv),
        _ => match remote
            .link
            .open_control()
            .map_err(|e| e.to_string())
            .and_then(|chan| request_with(chan, &remote.hello, argv))
        {
            Ok(reply) => print_reply(reply),
            Err(e) => fail(&e),
        },
    }
}

/// The control hello for a remote server. Nothing local that is
/// meaningless there goes: no pane, no cwd (the server falls back to the
/// remote `$HOME`), and only the terminal's and locale's variables.
// Ruling: the remote sees TERM, COLORTERM, LANG and LC_* only. Cost:
// another variable a remote program wants must be set there.
pub(super) fn remote_hello() -> Result<String, String> {
    encode_hello(&Hello::Control(remote_ctx())).map_err(|e| e.to_string())
}

fn remote_ctx() -> ClientCtx {
    ClientCtx {
        cwd: PathBuf::new(),
        pane: None,
        env: std::env::vars()
            .filter(|(k, _)| {
                matches!(k.as_str(), "TERM" | "COLORTERM" | "LANG") || k.starts_with("LC_")
            })
            .collect(),
        client: None,
        dialect: dialect(),
        request: Default::default(),
    }
}

/// A connected remote host.
struct Remote {
    link: grofe::remote::connect::Link,
    target: Target,
    hello: String,
    /// Set for `--transport quic`: reconnects go through it.
    roaming: Option<std::sync::Arc<grofe::remote::roam::Roaming>>,
}

impl Remote {
    fn request(&self, argv: &[String]) -> Result<Reply, String> {
        let chan = self.link.open_control().map_err(|e| e.to_string())?;
        request_with(chan, &self.hello, argv)
    }

    /// `new-session` there: `-A` for an existing session attaches, one
    /// that attaches creates the session detached and attaches to it, and
    /// `-d` is one request.
    fn new_session(self, argv: &[String]) -> ExitCode {
        if let Ok(grofe::cmd::Command::NewSession(c)) = grofe::cmd::parse(argv)
            && let Some(name) = c.name.filter(|_| c.attach)
        {
            let list = ["list-sessions", "-F", "#{session_id} #{session_name}"].map(String::from);
            if let Ok(Reply::Ok { out, .. }) = self.request(&list)
                && let Some((id, _)) = out
                    .lines()
                    .filter_map(|l| l.split_once(' '))
                    .find(|(_, n)| *n == name)
            {
                if !io::stdin().is_terminal() {
                    return fail("open terminal failed: not a terminal");
                }
                let argv = attach_argv_for_existing(argv, id.to_string());
                return print_attach(self.attach(&argv, false));
            }
        }
        if !attaches(argv) {
            return match self.request(argv) {
                Ok(reply) => print_reply(reply),
                Err(e) => fail(&e),
            };
        }
        if !io::stdin().is_terminal() {
            return fail("open terminal failed: not a terminal");
        }
        let mut create = vec![argv[0].clone(), "-d".to_string()];
        create.extend(argv[1..].iter().cloned());
        let flags = match grofe::cmd::parse(&create) {
            Ok(grofe::cmd::Command::NewSession(c)) => c.flags,
            _ => None,
        };
        create.extend([";", "display-message", "-p", "#{session_id}"].map(String::from));
        let id = match self.request(&create) {
            Ok(Reply::Ok { out, .. }) => out.lines().last().unwrap_or_default().trim().to_string(),
            Ok(Reply::Err { err, .. }) => return fail(&err),
            Ok(Reply::Event(_)) => return fail("unexpected event reply"),
            Err(e) => return fail(&e),
        };
        let mut attach = vec!["attach-session".to_string(), "-t".to_string(), id];
        attach.extend(flags.into_iter().flat_map(|f| ["-f".to_string(), f]));
        match self.attach(&attach, true) {
            Err(e) if session_gone(&e) => {
                println!("[exited]");
                ExitCode::SUCCESS
            }
            other => print_attach(other),
        }
    }

    /// `attach-session` there: the attach check over a control channel,
    /// then the remote attach client.
    fn attach(self, argv: &[String], created: bool) -> Result<String, String> {
        let parsed = match grofe::cmd::parse(argv) {
            Ok(grofe::cmd::Command::AttachSession(a)) => a,
            Ok(_) => return Err("attach-session: unexpected command".to_string()),
            Err(e) => return Err(e.to_string()),
        };
        let check: Vec<String> = [ATTACH_CHECK.to_string(), session::client_tty().to_string()]
            .into_iter()
            .chain(argv[1..].iter().cloned())
            .collect();
        match self.request(&check)? {
            Reply::Ok { .. } => {}
            Reply::Err { err, .. } => return Err(err),
            Reply::Event(_) => return Err("unexpected event reply".to_string()),
        }
        let opts = client::AttachOptions {
            target: client::raw_target(&argv[1..]),
            detach_others: parsed.detach_others,
            nest: parsed.nest,
            created,
            features: FEATURES.get().cloned().unwrap_or_default(),
            extras: grofe::protocol::AttachExtras {
                cwd: parsed.cwd.clone(),
                read_only: parsed.read_only,
                flags: parsed.flags.clone(),
                no_env: parsed.no_env,
                kill_parent: parsed.kill_parent,
            },
        };
        let target = self.target.clone();
        let roaming = self.roaming.clone();
        let relink = std::sync::Arc::new(move || {
            if let Some(r) = &roaming {
                return r.relink();
            }
            grofe::remote::connect::Link::connect(
                &grofe::remote::connect::Transport::Ssh(target.clone()),
                grofe::remote::connect::ConnectOpts { batch: true },
            )
            .map_err(|e| e.to_string())
        });
        let attach = session::RemoteAttach {
            host: self.target.host_name().to_string(),
            control_hello: self.hello.clone(),
            ctx: remote_ctx(),
            opts,
            term: std::env::var("TERM").unwrap_or_else(|_| "unknown".to_string()),
            reconnect: local_option("remote-reconnect").is_none_or(|v| v == "on"),
            relink,
            // Predictive echo last: it predicts only what the others send on.
            hooks: vec![
                Box::new(forward::ForwardHook::new(
                    self.target.host_name(),
                    &self.hello,
                    Some(forward::forwards_dir(&Env::from_process())),
                    forward::Settings::from_options(local_option),
                )),
                Box::new(grofe::remote::predict_hook::PredictHook::new()),
            ],
        };
        session::attach(self.link, attach)
    }
}

/// `grofe -r host ports`: the port forwards this machine's clients of
/// `host` hold, from their state files; no connection is made.
fn remote_ports(host: &str) -> ExitCode {
    let dir = forward::forwards_dir(&Env::from_process());
    let forwards = forward::list_forwards(&dir, host);
    if forwards.is_empty() {
        println!("no ports forwarded from {host}");
    } else {
        print!("{}", forward::format_forwards(host, &forwards));
    }
    ExitCode::SUCCESS
}

/// How an attach client's end is reported: `[reason]`, or the error.
fn print_attach(result: Result<String, String>) -> ExitCode {
    match result {
        Ok(reason) => {
            println!("[{reason}]");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}
