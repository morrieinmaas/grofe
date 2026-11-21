//! `grofe -r <host>`, `attach ssh://...` and `--machines`: the binary's
//! side of `grofe::remote`, which runs a command on a remote server through
//! the proxy (spec ssh-remote §2) the way the local CLI runs it here.

use std::io::{self, IsTerminal};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use grofe::client;
use grofe::paths::Env;
use grofe::protocol::{ATTACH_CHECK, ClientCtx, Dialect, Hello, Reply, encode_hello};
use grofe::remote::bootstrap::Install;
use grofe::remote::machines::Machine;
use grofe::remote::machines::client::Side;
use grofe::remote::ssh::Target;
use grofe::remote::{forward, session};

use super::{
    FEATURES, Globals, OPEN_SETTINGS, attach_argv_for_existing, attaches, connect_or_start,
    dialect, fail, print_reply, request, request_with, server_left, session_gone,
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
        (None, None) => stray_remote_flag(&globals.remote).map(|flag| {
            fail(&format!(
                "grofe: {flag} needs -r <host> or --machine <name>"
            ))
        }),
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
    let machines = wants_machines(globals, rest)?;
    if !rest.is_empty() {
        return Some(fail("grofe --machines: takes no command"));
    }
    if !io::stdin().is_terminal() {
        return Some(fail("open terminal failed: not a terminal"));
    }
    let (local, remote) = match sides() {
        Ok(s) => s,
        Err(e) => return Some(fail(&e)),
    };
    let opts = client::AttachOptions {
        features: FEATURES.get().cloned().unwrap_or_default(),
        ..Default::default()
    };
    Some(print_attach(grofe::remote::machines::client::run(
        &globals.socket(),
        &machines,
        local,
        remote,
        opts,
        std::env::var("TERM").unwrap_or_else(|_| "unknown".to_string()),
        local_option("remote-reconnect").is_none_or(|v| v == "on"),
    )))
}

/// The saved machines when this call is the multi-machine client:
/// `--machines`, or bare `grofe` on a terminal outside a pane with an
/// enabled machine saved. The file is read only for those two.
fn wants_machines(globals: &Globals, rest: &[String]) -> Option<Vec<Machine>> {
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
    (globals.remote.machines || bare).then_some(machines)
}

/// How the multi-machine client speaks to this machine's server (what the
/// local CLI sends: the cwd and the whole environment) and to another
/// machine's (the filtered remote context).
fn sides() -> Result<(Side, Side), String> {
    let local_ctx = ClientCtx {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        pane: None,
        env: std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect(),
        client: None,
        dialect: dialect(),
        request: Default::default(),
    };
    let local = Side {
        hello: encode_hello(&Hello::Control(local_ctx.clone())).map_err(|e| e.to_string())?,
        ctx: local_ctx,
    };
    let remote = Side {
        hello: remote_hello()?,
        ctx: remote_ctx(),
    };
    Ok((local, remote))
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
    // QUIC only for an attach: a one-shot over ssh starts no proxy, so it
    // neither stops the one behind a running QUIC attach (one per instance)
    // nor leaves a detached one behind.
    let quic = match transport.as_str() {
        "ssh" => false,
        "quic" => attaches_remotely(argv),
        other => return fail(&format!("grofe: unknown transport {other:?} (ssh or quic)")),
    };
    let connected = if quic {
        grofe::remote::roam::Roaming::connect(target, &flags.install, &release_url)
            .map(|(roaming, link)| (link, Some(std::sync::Arc::new(roaming))))
    } else {
        grofe::remote::bootstrap::connect(target, &flags.install, &release_url)
            .map(|link| (link, None))
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
        _ => match remote.request(argv) {
            Ok(reply) => print_reply(clean(reply)),
            Err(e) => fail_remote(&e),
        },
    }
}

/// Whether `argv` attaches there: `attach-session`, or a `new-session`
/// without `-d`.
fn attaches_remotely(argv: &[String]) -> bool {
    match grofe::cmd::parse::canonical_name(&argv[0]) {
        "attach-session" => true,
        "new-session" => attaches(argv),
        _ => false,
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
    /// One request on a fresh control channel. A server that closes the
    /// channel without replying was exiting as it connected (an empty one
    /// started on demand) and never read the request, so it is sent once
    /// more, on another channel, as nothing can have run twice.
    fn request(&self, argv: &[String]) -> Result<Reply, String> {
        let once = || {
            let chan = self.link.open_control().map_err(|e| e.to_string())?;
            request_with(chan, &self.hello, argv)
        };
        match once() {
            Err(e) if server_left(&e) => once(),
            reply => reply,
        }
    }

    /// `new-session` there: `-A` for an existing session attaches, one
    /// that attaches creates the session detached and attaches to it, and
    /// `-d` is one request.
    fn new_session(self, argv: &[String]) -> ExitCode {
        if !attaches(argv) {
            return match self.request(argv) {
                Ok(reply) => print_reply(clean(reply)),
                Err(e) => fail_remote(&e),
            };
        }
        if !io::stdin().is_terminal() {
            return fail("open terminal failed: not a terminal");
        }
        // One request makes (or, with `-A`, finds) the session and names
        // it, as the local `new_and_attach`: a server started on demand for
        // an earlier, separate request would have no session and exit as
        // that connection closed, refusing the next one in that window.
        let reuse = match grofe::cmd::parse(argv) {
            Ok(grofe::cmd::Command::NewSession(c)) => c.name.filter(|_| c.attach),
            _ => None,
        };
        let mut create = Vec::new();
        if reuse.is_some() {
            create.extend(["list-sessions", "-F", "#{session_name}", ";"].map(String::from));
        }
        create.extend([argv[0].clone(), "-d".to_string()]);
        create.extend(argv[1..].iter().cloned());
        let flags = match grofe::cmd::parse(&create[create.len() - argv.len() - 1..]) {
            Ok(grofe::cmd::Command::NewSession(c)) => c.flags,
            _ => None,
        };
        // `-A` on an existing session bumps its activity: the latest either way.
        create.extend([";", "display-message", "-p", "#{session_id}"].map(String::from));
        let out = match self.request(&create) {
            Ok(Reply::Ok { out, .. }) => out,
            Ok(Reply::Err { err, .. }) => return fail_remote(&err),
            Ok(Reply::Event(_)) => return fail("unexpected event reply"),
            Err(e) => return fail_remote(&e),
        };
        let lines: Vec<&str> = out.lines().collect();
        let id = lines
            .last()
            .map(|l| l.trim().to_string())
            .unwrap_or_default();
        let existed = reuse
            .is_some_and(|name| lines[..lines.len().saturating_sub(1)].contains(&name.as_str()));
        if existed {
            let argv = attach_argv_for_existing(argv, id);
            return print_attach(self.attach(&argv, false));
        }
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
        self.check_attach(argv)?;
        let attach = session::RemoteAttach {
            host: self.target.host_name().to_string(),
            control_hello: self.hello.clone(),
            ctx: remote_ctx(),
            opts: attach_options(argv, &parsed, created),
            term: std::env::var("TERM").unwrap_or_else(|_| "unknown".to_string()),
            reconnect: local_option("remote-reconnect").is_none_or(|v| v == "on"),
            relink: self.relinker(),
            hooks: self.hooks(),
        };
        session::attach(self.link, attach)
    }

    /// The attach check ([`ATTACH_CHECK`]): a refusal comes before the
    /// terminal is touched.
    fn check_attach(&self, argv: &[String]) -> Result<(), String> {
        let check: Vec<String> = [ATTACH_CHECK.to_string(), session::client_tty()]
            .into_iter()
            .chain(argv[1..].iter().cloned())
            .collect();
        match self.request(&check)? {
            Reply::Ok { .. } => Ok(()),
            Reply::Err { err, .. } => Err(err),
            Reply::Event(_) => Err("unexpected event reply".to_string()),
        }
    }

    /// Makes a new link to this host for a reconnect, never prompting: over
    /// the roaming transport when there is one, else ssh in batch mode.
    fn relinker(
        &self,
    ) -> std::sync::Arc<dyn Fn() -> Result<grofe::remote::connect::Link, String> + Send + Sync>
    {
        let target = self.target.clone();
        let roaming = self.roaming.clone();
        std::sync::Arc::new(move || {
            if let Some(r) = &roaming {
                return r.relink();
            }
            grofe::remote::connect::Link::connect(
                &grofe::remote::connect::Transport::Ssh(target.clone()),
                grofe::remote::connect::ConnectOpts { batch: true },
            )
            .map_err(|e| e.to_string())
        })
    }

    /// The remote attach's hooks: forwarding, then predictive echo last,
    /// since it predicts only what the others send on.
    fn hooks(&self) -> Vec<Box<dyn session::Hook>> {
        vec![
            Box::new(forward::ForwardHook::new(
                self.target.host_name(),
                &self.hello,
                Some(forward::forwards_dir(&Env::from_process())),
                forward::Settings::from_options(local_option),
            )),
            Box::new(grofe::remote::predict_hook::PredictHook::new(
                grofe::remote::predict_hook::Mode::from_config(),
            )),
        ]
    }
}

/// The attach options of the parsed `attach-session` `argv`.
fn attach_options(
    argv: &[String],
    parsed: &grofe::cmd::AttachSession,
    created: bool,
) -> client::AttachOptions {
    client::AttachOptions {
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
            open_settings: OPEN_SETTINGS.load(std::sync::atomic::Ordering::Relaxed),
        },
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
        Err(e) => fail_remote(&e),
    }
}

/// Fails with `e`, an error that may carry the remote's text, made safe
/// for the terminal first.
fn fail_remote(e: &str) -> ExitCode {
    fail(&grofe::remote::ssh::sanitize(e))
}

/// `reply` with its error text made safe for the terminal. The output of a
/// command stays as it came (`capture-pane -e` needs its escapes).
fn clean(reply: Reply) -> Reply {
    match reply {
        Reply::Err {
            id,
            err,
            out,
            status,
        } => Reply::Err {
            id,
            err: grofe::remote::ssh::sanitize(&err),
            out,
            status,
        },
        other => other,
    }
}

/// `grofe open <url>`: asks the clients subscribed to `open-url` to open it
/// (a remote client opens it on its own machine). With no server or no
/// subscriber it opens the URL here: the pane's own browser from before
/// `forward-browser`, else `open` or `xdg-open` (`GROFE_OPENER` first). Only
/// http, https and file URLs are opened, others are refused; a path that
/// exists is a `file:` URL.
pub(super) fn open_url(socket: &Path, args: &[String]) -> ExitCode {
    let [url] = args else {
        return fail("grofe open: usage: grofe open <url>");
    };
    // `$BROWSER some/file.html` (as `cargo doc --open` runs it): a path
    // that exists is a `file:` URL.
    let url = &grofe::open::path_url(url).unwrap_or_else(|| url.clone());
    let here = || match grofe::open::open_here(url) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("grofe open: {e}")),
    };
    if let Err(e) = grofe::open::check(url) {
        return fail(&format!("grofe open: refused {url:?}: {e}"));
    }
    let argv = ["open-url".to_string(), url.clone()];
    let reply = UnixStream::connect(socket)
        .map_err(|e| e.to_string())
        .and_then(|stream| request(stream, socket, &argv));
    match reply {
        Ok(Reply::Ok { .. }) => ExitCode::SUCCESS,
        Ok(Reply::Err { err, .. }) if err != grofe::open::NO_SUBSCRIBER => fail(&err),
        _ => here(),
    }
}

/// `grofe proxy`: serves the proxy protocol on stdin and stdout. Channels
/// reach the server socket this CLI would, started on demand as for any
/// session-creating call. With `quic`, the same over QUIC instead
/// (`grofe::remote::quic::server`): this process prints the bootstrap line
/// and exits, a detached one serves.
pub(super) fn proxy_cmd(globals: &Globals, quic: bool) -> ExitCode {
    let (socket, own_dir) = globals.resolve();
    // The detached QUIC proxy works from `/`, so a relative `-S` must not
    // stay relative.
    let socket = std::path::absolute(&socket).unwrap_or(socket);
    let target = socket.clone();
    let connect: grofe::remote::proxy::Connect =
        std::sync::Arc::new(move || connect_or_start(&target, own_dir, true));
    if quic {
        return match grofe::remote::quic::server::proxy_quic(&socket, connect) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&format!("{}{e}", grofe::remote::quic::server::FAILED)),
        };
    }
    match grofe::remote::proxy::run(io::stdin(), io::stdout(), connect) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("proxy: {e}")),
    }
}

/// `-r <host>`, `--install`, `--no-install`, `--install-from <path>`,
/// `--transport <t>`: a call that runs on a remote host (spec ssh-remote §2).
#[derive(Default)]
pub(super) struct RemoteFlags {
    pub(super) host: Option<String>,
    pub(super) install: Install,
    pub(super) transport: Option<String>,
    /// `--machine <name>`: a saved machine, like `-r` (spec ssh-remote §5).
    pub(super) machine: Option<String>,
    /// `--machines`: the multi-machine client.
    pub(super) machines: bool,
}

/// The first remote-only option given (`--transport`, `--install`,
/// `--no-install`, `--install-from`), for a call that runs nowhere remote.
fn stray_remote_flag(flags: &RemoteFlags) -> Option<&'static str> {
    if flags.transport.is_some() {
        return Some("--transport");
    }
    match flags.install {
        Install::Ask => None,
        Install::Yes => Some("--install"),
        Install::No => Some("--no-install"),
        Install::From(_) => Some("--install-from"),
    }
}

/// The global options that belong to a remote call ([`RemoteFlags`]).
pub(super) const REMOTE_FLAGS: &[&str] = &[
    "-r",
    "--install-from",
    "--transport",
    "--machine",
    "--install",
    "--machines",
    "--no-install",
];

/// Reads the remote option at `args[i]` (one of [`REMOTE_FLAGS`]) into
/// `flags`; the index after it (and after its value, if it takes one).
pub(super) fn parse_flag(
    args: &[String],
    i: usize,
    flags: &mut RemoteFlags,
) -> Result<usize, String> {
    let arg = args[i].as_str();
    let value = || {
        args.get(i + 1)
            .cloned()
            .ok_or_else(|| format!("option {arg} requires an argument"))
    };
    match arg {
        "-r" => flags.host = Some(value()?),
        "--machine" => flags.machine = Some(value()?),
        "--install-from" => flags.install = Install::From(PathBuf::from(value()?)),
        "--transport" => flags.transport = Some(value()?),
        "--install" => {
            flags.install = Install::Yes;
            return Ok(i + 1);
        }
        "--machines" => {
            flags.machines = true;
            return Ok(i + 1);
        }
        _ => {
            flags.install = Install::No;
            return Ok(i + 1);
        }
    }
    Ok(i + 2)
}
