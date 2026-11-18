//! The multi-machine client (spec ssh-remote §5): `grofe --machines`, or
//! bare `grofe` with enabled machines saved. Every machine, the local
//! server too, is a [`Link`]; a watcher thread per machine keeps its link
//! and a control channel subscribed to agent and session events, and
//! refreshes its [`MachineView`]. Only the selected machine has an attach
//! channel: [`MachinesHook`] opens the picker on `machine-key` and switches
//! the attach ([`HookCtx::switch`]) to the row chosen.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use super::net::{AGENT_FORMAT, local_link, machine_link, parse_agents, sanitize_lines};
use super::{LOCAL, Link as LinkState, Machine, MachineView, Transport};
use super::{last_known, save_last_known};
use crate::client::AttachOptions;
use crate::protocol::ClientCtx;
use crate::remote::connect::Link;
use crate::remote::forward::{self, ForwardHook};
use crate::remote::roam::Roaming;
use crate::remote::session::{self, RemoteAttach, pump_events};
use crate::remote::ssh::Target;
use crate::server::input::{Decoder, Key, KeyCode, parse_key};

/// How often a connected machine's view is read again without an event
/// (notifications send none).
const REFRESH: Duration = Duration::from_secs(3);

/// Failed attempts after a drop before a machine counts as offline rather
/// than reconnecting.
const GIVE_UP: u32 = 3;

/// How long a watcher waits before connecting again.
// ponytail: a fixed retry, not the attach's backoff; a machine down for
// hours is tried every 5 s by a background thread that costs one ssh.
const RETRY: Duration = Duration::from_secs(5);

/// The events that change a view.
const EVENTS: &[&str] = &[
    "agent-changed",
    "session-created",
    "session-closed",
    "notify",
];

/// Where a machine's notifications go when it is not the one on screen:
/// the machine attached to, and the texts waiting for the hook to show.
#[derive(Clone, Default)]
struct Notices {
    current: Arc<AtomicUsize>,
    queue: Arc<Mutex<Vec<String>>>,
}

mod hook;

use hook::MachinesHook;

/// The session a machine is attached to when none is saved for it: what
/// `grofe -r host` attaches to.
const DEFAULT_SESSION: &str = "main";

/// One machine as the client tracks it.
struct Entry {
    /// `None` for the local server.
    machine: Option<Machine>,
    view: MachineView,
    link: Option<Link>,
}

type Shared = Arc<Mutex<Vec<Entry>>>;

fn lock(s: &Shared) -> MutexGuard<'_, Vec<Entry>> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

/// How the client speaks to one kind of server: the attach hello's
/// context and the control hello.
#[derive(Clone)]
pub struct Side {
    pub hello: String,
    pub ctx: ClientCtx,
}

/// Everything a watcher or a switch needs to reach machine `i` again.
#[derive(Clone)]
struct Reach {
    socket: PathBuf,
    machine: Option<Machine>,
    /// This machine's own context for the local server (its cwd and whole
    /// environment, as the local CLI sends), the filtered one elsewhere.
    side: Side,
    /// A machine saved with `quic`: the one roaming remote its attaches
    /// share, made on the first attach, so a relink reuses its token.
    roaming: Arc<OnceLock<Arc<Roaming>>>,
}

impl Reach {
    fn new(socket: &Path, machine: Option<Machine>, side: Side) -> Reach {
        Reach {
            socket: socket.to_path_buf(),
            machine,
            side,
            roaming: Arc::new(OnceLock::new()),
        }
    }

    /// The forwarding hook for an attach here: none for the local server,
    /// which is this machine.
    fn forward(&self) -> Option<ForwardHook> {
        let m = self.machine.as_ref()?;
        let host = Target::parse_host(&m.target).ok()?;
        let option = |name: &str| {
            crate::config::load(&crate::config::Env::from_process())
                .ok()
                .and_then(|r| r.options.get(name).cloned())
        };
        Some(ForwardHook::new(
            host.host_name(),
            &self.side.hello,
            Some(forward::forwards_dir(&crate::paths::Env::from_process())),
            forward::Settings::from_options(option),
        ))
    }

    /// A control link for the watcher: the local proxy, or ssh. Never a
    /// QUIC bootstrap, which would stop the proxy behind a running attach.
    fn connect(&self) -> Result<Link, String> {
        match &self.machine {
            None => local_link(&self.socket),
            Some(m) => machine_link(m),
        }
    }

    fn quic(&self) -> bool {
        self.machine
            .as_ref()
            .is_some_and(|m| m.transport == Some(Transport::Quic))
    }

    /// A link for the attach: over QUIC through the machine's one roaming
    /// remote when it was saved with `quic` (never prompting or printing),
    /// else as [`Reach::connect`].
    fn attach_link(&self) -> Result<Link, String> {
        match &self.machine {
            Some(m) if self.quic() => {
                let target = Target::parse_host(&m.target)?;
                self.roaming
                    .get_or_init(|| Arc::new(Roaming::unlinked(&target)))
                    .relink()
            }
            _ => self.connect(),
        }
    }

    /// The session to attach to there; `None`: the server's choice.
    fn session(&self) -> Option<String> {
        self.machine
            .as_ref()
            .map(|m| m.session.clone().unwrap_or_else(|| DEFAULT_SESSION.into()))
    }
}

/// Runs the multi-machine client: watchers for the local server and every
/// enabled machine in `machines`, then an attach to the local server with
/// [`MachinesHook`]. `local` is how this machine's own CLI speaks to its
/// server, `remote` how it speaks to another machine's; `opts`, `term` and
/// `reconnect` are as for a remote attach.
pub fn run(
    socket: &Path,
    machines: &[Machine],
    local: Side,
    remote: Side,
    opts: AttachOptions,
    term: String,
    reconnect: bool,
) -> Result<String, String> {
    let state_dir = crate::paths::state_dir(&crate::paths::Env::from_process());
    let reaches = reaches(socket, machines, local, remote);
    let shared = entries(&reaches, &state_dir);
    let hello = reaches[0].side.hello.clone();
    let local = local_link(socket)?;
    let key = machine_key(&local, &hello);
    let target = ensure_session(&local, &hello, None)?;
    {
        let mut entries = lock(&shared);
        entries[0].link = Some(local.clone());
        entries[0].view.link = LinkState::Connected;
    }
    let notices = start_watchers(&reaches, &shared, &local, &state_dir);
    let hook = MachinesHook {
        shared: Arc::clone(&shared),
        reaches: reaches.clone(),
        key,
        decoder: Decoder::new(),
        current: Arc::clone(&notices.current),
        notices: Arc::clone(&notices.queue),
        forward: Arc::new(Mutex::new(None)),
        sent: None,
    };
    let reach = reaches[0].clone();
    let attach = RemoteAttach {
        host: LOCAL.to_string(),
        control_hello: hello,
        ctx: reach.side.ctx.clone(),
        opts: AttachOptions { target, ..opts },
        term,
        reconnect,
        relink: Arc::new(move || reach.connect()),
        // Forwarding lives in MachinesHook, keyed to the machine on screen.
        // Predictive echo follows any link.
        hooks: vec![
            Box::new(hook),
            Box::new(crate::remote::predict_hook::PredictHook::new(
                crate::remote::predict_hook::Mode::from_config(),
            )),
        ],
    };
    session::attach(local, attach)
}

/// The local server first, then every enabled machine.
fn reaches(socket: &Path, machines: &[Machine], local: Side, remote: Side) -> Vec<Reach> {
    std::iter::once(Reach::new(socket, None, local))
        .chain(
            machines
                .iter()
                .filter(|m| m.enabled)
                .map(|m| Reach::new(socket, Some(m.clone()), remote.clone())),
        )
        .collect()
}

/// One entry per machine, each offline with its last known agents until
/// its watcher reaches it.
fn entries(reaches: &[Reach], state_dir: &Path) -> Shared {
    Arc::new(Mutex::new(
        reaches
            .iter()
            .map(|r| Entry {
                machine: r.machine.clone(),
                view: offline_view(r.machine.as_ref(), state_dir),
                link: None,
            })
            .collect(),
    ))
}

/// A watcher thread per machine, the local one starting on `local`; where
/// their notifications go.
fn start_watchers(reaches: &[Reach], shared: &Shared, local: &Link, state_dir: &Path) -> Notices {
    let notices = Notices::default();
    for (i, reach) in reaches.iter().enumerate() {
        let (shared, reach, hello, dir, notices) = (
            Arc::clone(shared),
            reach.clone(),
            reach.side.hello.clone(),
            state_dir.to_path_buf(),
            notices.clone(),
        );
        let first = (i == 0).then(|| local.clone());
        std::thread::spawn(move || watch(shared, i, reach, &hello, &dir, first, notices));
    }
    notices
}

/// The view of a machine not (yet) reached: offline, with its last known
/// agents.
fn offline_view(machine: Option<&Machine>, state_dir: &Path) -> MachineView {
    let name = machine.map_or(LOCAL, |m| m.name.as_str());
    MachineView {
        name: name.to_string(),
        link: LinkState::Offline,
        sessions: Vec::new(),
        agents: last_known(state_dir, name)
            .map(|t| parse_agents(&t))
            .unwrap_or_default(),
        unread: false,
    }
}

/// `machine-key` from the local server; `M-m` when it cannot be read.
fn machine_key(link: &Link, hello: &str) -> Key {
    let argv = ["show-options", "-gv", "machine-key"].map(String::from);
    let name = link.request(hello, &argv).unwrap_or_default();
    parse_key(name.trim())
        .or_else(|_| parse_key("M-m"))
        .unwrap_or(Key::new(
            KeyCode::Char('m'),
            crate::server::input::Mods {
                alt: true,
                ..Default::default()
            },
        ))
}

/// Makes sure `session` exists on the server behind `link` (creating it
/// detached), and returns the attach target. With no session named, a
/// server with none gets `main` and the attach leaves the choice to it.
fn ensure_session(
    link: &Link,
    hello: &str,
    session: Option<&str>,
) -> Result<Option<String>, String> {
    let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match session {
        Some(s) => {
            if link
                .request(hello, &words(&["has-session", "-t", &format!("={s}")]))
                .is_err()
            {
                link.request(hello, &words(&["new-session", "-d", "-s", s]))?;
            }
            Ok(Some(s.to_string()))
        }
        None => {
            let list = link.request(hello, &words(&["list-sessions", "-F", "#{session_id}"]))?;
            if list.trim().is_empty() {
                link.request(hello, &words(&["new-session", "-d", "-s", DEFAULT_SESSION]))?;
            }
            Ok(None)
        }
    }
}

/// Keeps machine `i`'s link and view up to date, for as long as the
/// process runs. `first` is a link already made.
fn watch(
    shared: Shared,
    i: usize,
    reach: Reach,
    hello: &str,
    state_dir: &Path,
    first: Option<Link>,
    notices: Notices,
) {
    let mut first = first;
    // Failed attempts since the machine was last connected; `None` while it
    // never was (offline from the start).
    let mut failed: Option<u32> = None;
    loop {
        let link = match first.take().map(Ok).unwrap_or_else(|| reach.connect()) {
            Ok(link) => link,
            Err(_) => {
                let state = match &mut failed {
                    Some(n) => {
                        *n += 1;
                        link_state(Some(*n))
                    }
                    None => link_state(None),
                };
                lock(&shared)[i].view.link = state;
                std::thread::sleep(RETRY);
                continue;
            }
        };
        failed = Some(0);
        {
            let mut entries = lock(&shared);
            entries[i].link = Some(link.clone());
            entries[i].view.link = LinkState::Connected;
        }
        let (tx, rx) = mpsc::channel();
        if let Ok(chan) = link.open_control() {
            let hello = hello.to_string();
            let events: Vec<String> = EVENTS.iter().map(|e| e.to_string()).collect();
            std::thread::spawn(move || pump_events(chan, &hello, &events, |v| tx.send(v).is_ok()));
        }
        while link.dropped().is_none() {
            refresh(&shared, i, &link, hello, state_dir);
            match rx.recv_timeout(REFRESH) {
                Ok(event) => on_event(&shared, i, &notices, &event),
                Err(RecvTimeoutError::Timeout) => {}
                // The event channel ended: the link is going.
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(REFRESH),
            }
        }
        // Dropped: reconnecting at once, with the last known view kept.
        let mut entries = lock(&shared);
        entries[i].link = None;
        entries[i].view.link = LinkState::Reconnecting;
    }
}

/// A watched machine's state after `failed` attempts in a row since it was
/// last connected (`None`: it never was).
fn link_state(failed: Option<u32>) -> LinkState {
    match failed {
        Some(n) if n < GIVE_UP => LinkState::Reconnecting,
        _ => LinkState::Offline,
    }
}

/// An event from machine `i`. A `notify` from a machine not on screen marks
/// it unread and queues its text for the local terminal; the attached
/// machine's own server already shows its notifications.
fn on_event(shared: &Shared, i: usize, notices: &Notices, event: &serde_json::Value) {
    if event.get("event").and_then(|e| e.as_str()) != Some("notify")
        || notices.current.load(Ordering::SeqCst) == i
    {
        return;
    }
    let name = {
        let mut entries = lock(shared);
        entries[i].view.unread = true;
        entries[i].view.name.clone()
    };
    notices
        .queue
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(notice_text(&name, event));
}

/// A `notify` event from machine `name` as one line for the local
/// terminal, like the server's own: `grofe: <machine>: <kind> <state>:
/// <msg>`, every remote word sanitized.
fn notice_text(name: &str, event: &serde_json::Value) -> String {
    let field = |k: &str| {
        event
            .get(k)
            .and_then(|v| v.as_str())
            .map(crate::remote::ssh::sanitize)
    };
    let mut text = format!("grofe: {}:", crate::remote::ssh::sanitize(name));
    for word in [field("kind"), field("state")].into_iter().flatten() {
        text.push(' ');
        text.push_str(&word);
    }
    if let Some(msg) = field("msg") {
        text.push_str(": ");
        text.push_str(&msg);
    }
    text
}

/// Reads machine `i`'s sessions, agents and unread flag over `link`.
fn refresh(shared: &Shared, i: usize, link: &Link, hello: &str, state_dir: &Path) {
    let ask =
        |w: &[&str]| link.request(hello, &w.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let Ok(agents) = ask(&["agent", "list", "-F", AGENT_FORMAT]) else {
        return;
    };
    let sessions = ask(&["list-sessions", "-F", "#{session_name}"]).unwrap_or_default();
    let unread = ask(&["display-message", "-p", "#{notifications_unread}"])
        .map(|v| !matches!(v.trim(), "" | "0"))
        .unwrap_or(false);
    let (agents, sessions) = (sanitize_lines(&agents), sanitize_lines(&sessions));
    let mut entries = lock(shared);
    let e = &mut entries[i];
    if e.machine.is_some() {
        let _ = save_last_known(state_dir, &e.view.name, &agents);
    }
    e.view.agents = parse_agents(&agents);
    e.view.sessions = sessions.lines().map(str::to_string).collect();
    e.view.unread = unread;
}

fn views(shared: &Shared) -> Vec<MachineView> {
    lock(shared).iter().map(|e| e.view.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_machine_reconnects_before_it_counts_as_offline() {
        assert_eq!(link_state(None), LinkState::Offline, "never reached");
        assert_eq!(link_state(Some(1)), LinkState::Reconnecting);
        assert_eq!(link_state(Some(GIVE_UP - 1)), LinkState::Reconnecting);
        assert_eq!(link_state(Some(GIVE_UP)), LinkState::Offline);
    }

    #[test]
    fn a_notify_becomes_one_sanitized_line() {
        let e = serde_json::json!({
            "event": "notify", "pane": "%1", "session": "$0",
            "kind": "claude", "state": "blocked", "msg": "Bash rm\u{1b}[2J"
        });
        assert_eq!(
            notice_text("box", &e),
            "grofe: box: claude blocked: Bash rm\\u{1b}[2J"
        );
        let bare = serde_json::json!({"event": "notify", "pane": "%1", "state": "done",
            "kind": null, "msg": null, "session": null});
        assert_eq!(notice_text("box", &bare), "grofe: box: done");
    }

    #[test]
    fn a_notify_marks_only_a_machine_not_on_screen() {
        let shared: Shared = Arc::new(Mutex::new(
            (0..2)
                .map(|_| Entry {
                    machine: None,
                    view: offline_view(None, Path::new("/nonexistent")),
                    link: None,
                })
                .collect(),
        ));
        let notices = Notices::default();
        let e = serde_json::json!({"event": "notify", "state": "done"});
        on_event(&shared, 0, &notices, &e);
        assert!(
            !lock(&shared)[0].view.unread,
            "on screen: its server shows it"
        );
        on_event(&shared, 1, &notices, &e);
        assert!(lock(&shared)[1].view.unread);
        assert_eq!(notices.queue.lock().unwrap().len(), 1);
        on_event(
            &shared,
            1,
            &notices,
            &serde_json::json!({"event": "agent-changed"}),
        );
        assert_eq!(notices.queue.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_saved_machine_attaches_to_its_session_or_main() {
        let mut m = Machine {
            name: "box".into(),
            target: "box".into(),
            session: None,
            transport: None,
            enabled: true,
        };
        let side = Side {
            hello: String::new(),
            ctx: ClientCtx {
                cwd: PathBuf::new(),
                pane: None,
                env: Vec::new(),
                client: None,
                dialect: Default::default(),
                request: Default::default(),
            },
        };
        let reach = |m: Option<Machine>| Reach::new(Path::new(""), m, side.clone());
        assert_eq!(reach(None).session(), None);
        assert_eq!(reach(Some(m.clone())).session().as_deref(), Some("main"));
        m.session = Some("dev".into());
        assert_eq!(reach(Some(m)).session().as_deref(), Some("dev"));
    }

    #[test]
    fn an_unreached_machine_shows_its_last_known_agents() {
        let d = tempfile::tempdir().unwrap();
        save_last_known(d.path(), "box", "%1 codex blocked\n").unwrap();
        let m = Machine {
            name: "box".into(),
            target: "box".into(),
            session: None,
            transport: None,
            enabled: true,
        };
        let v = offline_view(Some(&m), d.path());
        assert_eq!(v.link, LinkState::Offline);
        assert_eq!(v.agents.len(), 1);
        assert!(v.wants_attention());
    }
}
