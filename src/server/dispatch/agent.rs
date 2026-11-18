//! Server-side agent entry point: the one place reports, bells and jumps
//! reach the agent store (spec §2.2 to §2.6, §5.2).

use std::time::{Duration, Instant};

use crate::cmd::expand;
use crate::config::Resolved;
use crate::protocol::ClientCtx;
use crate::server::agents::{AgentState, Change, Report, ReportState, kinds};
use crate::server::dispatch::options::{FormatCtx, ServerVars};
use crate::server::notify::{Raised, desktop};
use crate::server::resume::Binding;
use crate::server::{State, clients, events, hooks, resume};
use crate::types::{ClientId, PaneId};

/// Whether a client is known to be unfocused: its terminal reported losing
/// focus and `focus-events` is on. With `focus-events` off its focus is
/// unknown and counts as focused (spec §2.4); a client that never reported
/// starts focused.
fn known_unfocused(state: &State, c: &clients::ClientEntry) -> bool {
    !c.focused && state.model.options.lookup("focus-events", &[]) == Some("on")
}

/// Whether `pane` is the active pane of the current window of an attached
/// client whose outer terminal is focused (a client whose focus is unknown
/// counts as focused, spec §2.4).
pub fn is_seen(state: &State, pane: PaneId) -> bool {
    let Some(window) = state.model.panes.get(&pane).map(|p| p.window) else {
        return false;
    };
    if state
        .model
        .windows
        .get(&window)
        .is_none_or(|w| w.active != pane)
    {
        return false;
    }
    state.clients.values().any(|c| {
        !known_unfocused(state, c)
            && state
                .model
                .sessions
                .get(&c.session)
                .is_some_and(|s| s.active == window)
    })
}

/// Applies a report to `pane`'s record: creates it bound to the pane's
/// foreground process, raises `agent-changed`, redraws, and sets the pane's
/// resume when the session id changed.
pub fn apply(state: &mut State, pane: PaneId, r: Report) {
    if !state.agents_enabled || !state.model.panes.contains_key(&pane) {
        return;
    }
    let now = Instant::now();
    // A record whose process is gone is replaced, not updated (spec §2.3).
    let created = r.state != ReportState::Clear
        && state
            .agents
            .get(pane)
            .is_none_or(|a| a.binding.is_some_and(|b| !b.alive()));
    if created {
        let binding = foreground_binding(state, pane);
        let kind = r.kind.clone().unwrap_or_default();
        state.agents.ensure(pane, &kind, binding, now);
    }
    let seen = is_seen(state, pane);
    let before = state.agents.get(pane).map(|a| (a.state, a.since));
    let change = state.agents.apply(pane, r, seen, now);
    // A record this report made had no state before it.
    let change = match (change, created) {
        (Some(c), true) => Some(Change { old: None, ..c }),
        (None, true) => state.agents.get(pane).map(|a| Change {
            old: None,
            new: Some(a.state),
            kind: a.kind.clone(),
            session_changed: false,
        }),
        (c, false) => c,
    };
    let Some(change) = change else {
        return;
    };
    if change.session_changed && change.new.is_some() {
        set_resume(state, pane);
    }
    events::emit_agent_changed(state, pane, &change);
    let worked = match (change.old, before) {
        (Some(AgentState::Working), Some((AgentState::Working, since))) => {
            now.saturating_duration_since(since)
        }
        _ => Duration::ZERO,
    };
    raise(state, pane, &change, worked, now);
    clients::mark_all_dirty(state);
}

/// Offers a state change to the notification store (spec §6.1) and delivers
/// what it raises.
fn raise(state: &mut State, pane: PaneId, change: &Change, worked: Duration, now: Instant) {
    let Some(uid) = state.model.panes.get(&pane).map(|p| p.uid.clone()) else {
        return;
    };
    let msg = state
        .agents
        .get(pane)
        .and_then(|a| a.msg.clone().or_else(|| a.blocked.clone()));
    let configured = change
        .kind
        .as_deref()
        .is_some_and(|k| state.agent_kinds.iter().any(|x| x.name == k));
    let seen = is_seen(state, pane);
    let raised = state.notify.on_change(
        pane,
        &uid,
        change.kind.as_deref(),
        change.old.map(AgentState::as_str),
        change.new.map(AgentState::as_str),
        msg.as_deref(),
        worked,
        seen,
        configured,
        now,
    );
    if let Some(raised) = raised {
        emit_notify(state, raised);
        deliver(state, raised);
        clients::mark_all_dirty(state);
    }
}

/// The `notify` event for a raised notification (protocol v1 §6).
fn emit_notify(state: &mut State, raised: Raised) {
    let Some(n) = state.notify.items().iter().find(|n| n.id == raised.id) else {
        return;
    };
    let session = state
        .model
        .panes
        .get(&n.pane)
        .and_then(|p| state.model.windows.get(&p.window))
        .map(|w| w.session);
    let event = crate::event::Event::Notify {
        pane: n.pane,
        session,
        kind: n.kind.clone(),
        state: n.state.clone(),
        msg: n.msg.clone(),
    };
    state.events.emit(event);
}

/// Sends a raised notification out of the terminal (spec §6.4): `auto` is
/// OSC 9 when some client is known unfocused and the command when no client
/// is attached.
pub fn deliver(state: &mut State, raised: Raised) {
    if !raised.desktop {
        return;
    }
    let Some(n) = state.notify.items().iter().find(|n| n.id == raised.id) else {
        return;
    };
    let (pane, state_name) = (n.pane, n.state.clone());
    let title = format!("grofe: {}", n.kind.as_deref().unwrap_or("agent"));
    let label = pane_label(state, pane);
    let message = match &n.msg {
        Some(m) => format!("{label} {state_name}: {m}"),
        None => format!("{label} {state_name}"),
    };
    let delivery = state.notify.cfg.delivery.as_str();
    let (terminal, command) = match delivery {
        "terminal" => (true, false),
        "command" => (false, true),
        "auto" => (
            state.clients.values().any(|c| known_unfocused(state, c)),
            state.clients.is_empty(),
        ),
        _ => (false, false),
    };
    if terminal {
        let bytes = desktop::osc9(&format!("{title}: {message}"));
        let ids: Vec<ClientId> = state.clients.keys().copied().collect();
        for id in ids {
            clients::queue_raw(state, id, &bytes);
        }
    }
    // A test server (desktop::BUILTIN_OFF_ENV) runs a configured command
    // only, never the desktop's own notifier.
    if command && (state.notify.cfg.command.is_some() || !desktop::builtin_off()) {
        let os = if cfg!(target_os = "macos") {
            desktop::Os::Mac
        } else {
            desktop::Os::Linux
        };
        let cfg = &state.notify.cfg;
        let argv = desktop::argv(
            os,
            &title,
            &message,
            cfg.sound,
            cfg.command.as_deref(),
            &pane.to_string(),
            false,
        );
        state.notify_runner.submit(argv);
    }
}

fn pane_label(state: &State, pane: PaneId) -> String {
    let vars = FormatCtx::new(&state.model, pane);
    expand(
        "#{session_name}:#{window_index}.#{pane_index}",
        &ServerVars::fresh(state, pane, &vars),
    )
}

/// A bell or OSC 9/777 on a pane that never reported (spec §3.3).
pub fn attention(state: &mut State, pane: PaneId, msg: Option<String>) {
    if state.agents_enabled && state.agents.attention(pane, msg) {
        clients::mark_all_dirty(state);
    }
}

/// Reads `[agents]`: the built-in kinds with `[agents.kinds.<name>]` laid
/// over them key by key; `enabled = false` drops a kind.
pub fn configure(state: &mut State, r: &Resolved) {
    state.agents_enabled = r.agents.enabled;
    let mut all = kinds::builtin();
    for (name, c) in &r.agents.kinds {
        if c.enabled == Some(false) {
            all.retain(|k| &k.name != name);
            continue;
        }
        let at = match all.iter().position(|k| &k.name == name) {
            Some(at) => at,
            None => {
                all.push(kinds::Kind {
                    name: name.clone(),
                    matches: vec![name.clone()],
                    command: None,
                    resume: None,
                    worktree: None,
                });
                all.len() - 1
            }
        };
        let kind = &mut all[at];
        if let Some(m) = &c.matches {
            kind.matches = m.clone();
        }
        if c.command.is_some() {
            kind.command = c.command.clone();
        }
        if c.resume.is_some() {
            kind.resume = c.resume.clone();
        }
        if c.worktree.is_some() {
            kind.worktree = c.worktree.clone();
        }
    }
    state.agent_kinds = all;
    super::notify::configure(state, r);
}

/// Takes the jumping client to `pane` (spec §5.2): the client that ran the
/// command, else the most recently active one. It stays in its session when
/// that holds the pane's window (linked windows included), else switches to
/// the pane's session; then the window and pane are selected.
pub fn jump(state: &mut State, ctx: &ClientCtx, pane: PaneId) -> Result<(), String> {
    let window = state
        .model
        .panes
        .get(&pane)
        .map(|p| p.window)
        .ok_or_else(|| format!("can't find pane: {pane}"))?;
    let client = jumping_client(state, ctx);
    let own = client
        .and_then(|id| state.clients.get(&id))
        .map(|c| c.session);
    let session = state
        .model
        .link_session(window, own)
        .ok_or_else(|| format!("pane {pane} is in no session"))?;
    state.model.select_window_in(session, window);
    super::pane::switch_active(&mut state.model, window, pane, false);
    if let Some(id) = client.filter(|_| own != Some(session)) {
        clients::show_session_with(state, id, session, false);
    }
    clients::mark_all_dirty(state);
    Ok(())
}

fn jumping_client(state: &State, ctx: &ClientCtx) -> Option<ClientId> {
    ctx.client
        .filter(|id| state.clients.contains_key(id))
        .or_else(|| {
            state
                .clients
                .iter()
                .max_by_key(|(_, c)| c.activity)
                .map(|(id, _)| *id)
        })
}

/// The binding for `pane`'s foreground process, if it has one.
///
/// A report typed in a plain shell is made by the short-lived `grofe` (or
/// `tmux`) CLI, which is then the foreground leader; the record binds to the
/// pane's root process (the shell) instead, or the sweep would drop it.
fn foreground_binding(state: &State, pane: PaneId) -> Option<Binding> {
    let p = state.model.panes.get(&pane)?;
    let group = p.handle.foreground_group()?;
    let argv = crate::server::pane::procinfo::leader(group).argv;
    let root = p.handle.pid().and_then(|pid| i32::try_from(pid).ok());
    Binding::of(bind_pid(group, argv.as_deref(), root))
}

/// The process a new record binds to: the foreground group, unless its
/// leader is the reporter itself and the pane's root process is known.
fn bind_pid(group: i32, leader_argv: Option<&[String]>, root: Option<i32>) -> i32 {
    let reporter = leader_argv
        .and_then(|a| a.first())
        .and_then(|a0| a0.rsplit('/').next())
        .is_some_and(|b| b == "grofe" || b == "tmux");
    match root {
        Some(root) if reporter => root,
        _ => group,
    }
}

/// Sets `pane`'s resume from its kind's template and session id (spec
/// §2.6), bound to the record's process. `argv[0]` is resolved against the
/// pane's own `PATH` as `pane-set-resume` does, and stays bare when that
/// finds nothing.
fn set_resume(state: &mut State, pane: PaneId) {
    let Some(a) = state.agents.get(pane) else {
        return;
    };
    let (Some(kind), Some(id)) = (a.kind.as_deref(), a.session_id.as_deref()) else {
        return;
    };
    let Some(argv) = state
        .agent_kinds
        .iter()
        .find(|k| k.name == kind)
        .and_then(|k| k.resume.as_deref())
        .and_then(|t| kinds::resume_argv(t, id))
    else {
        return;
    };
    let argv = resume::resolve_for_pane(state, pane, argv);
    let binding = a.binding.or_else(|| foreground_binding(state, pane));
    let Some(p) = state
        .model
        .panes
        .get_mut(&pane)
        .filter(|p| p.dead.is_none())
    else {
        return;
    };
    let changed = p.resume.as_ref() != Some(&argv) || p.expect.is_some();
    p.resume = Some(argv);
    p.expect = None;
    p.resume_binding = binding;
    if changed {
        hooks::resume_changed(state, pane);
    }
    resume::watch(state);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use super::*;
    use crate::config::Resolved;
    use crate::event::Event;
    use crate::server::agents::AgentState;
    use crate::server::{dispatch, test_ctx, test_state};

    // The swappable seam: the kinds come from `agents::kinds::builtin()`
    // either way, so SUPERLITE_REAL needs no separate fake here.
    fn state_with_pane() -> (State, PaneId) {
        let mut state = test_state();
        let argv = ["new-session", "-d", "-s", "a", "sleep 600"].map(String::from);
        dispatch::run(&mut state, &test_ctx(), &argv).expect("new-session");
        let pane = *state.model.panes.keys().next().expect("a pane");
        (state, pane)
    }

    fn report(state: ReportState, ts: u64) -> Report {
        Report {
            state,
            kind: Some("claude".into()),
            msg: None,
            blocked: None,
            session_id: None,
            ts,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn report_creates_record() {
        let (mut state, pane) = state_with_pane();
        apply(
            &mut state,
            pane,
            report(ReportState::Set(AgentState::Working), 1),
        );
        let a = state.agents.get(pane).expect("record");
        assert_eq!(a.kind.as_deref(), Some("claude"));
        assert_eq!(a.state, AgentState::Working);
        assert!(a.binding.is_some(), "bound to the foreground process");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn report_sets_resume_from_session_id() {
        let (mut state, pane) = state_with_pane();
        let mut r = report(ReportState::Set(AgentState::Done), 1);
        r.session_id = Some("s1".into());
        apply(&mut state, pane, r);
        let resume = state.model.panes[&pane].resume.clone();
        // argv[0] is absolute when the pane's PATH holds `claude`, else bare.
        let resume = resume.expect("a resume");
        assert!(
            resume[0] == "claude" || resume[0].ends_with("/claude"),
            "{resume:?}"
        );
        assert_eq!(resume[1..], ["--resume", "s1"]);
        assert!(state.model.panes[&pane].resume_binding.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_session_id_no_resume() {
        let (mut state, pane) = state_with_pane();
        let mut r = report(ReportState::Set(AgentState::Done), 1);
        r.session_id = Some("a b;rm".into());
        let before = state.model.panes[&pane].resume.clone();
        apply(&mut state, pane, r);
        assert_eq!(state.model.panes[&pane].resume, before);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_config_ignores_reports() {
        let (mut state, pane) = state_with_pane();
        let mut resolved = Resolved::default();
        resolved.agents.enabled = false;
        configure(&mut state, &resolved);
        apply(
            &mut state,
            pane,
            report(ReportState::Set(AgentState::Working), 1),
        );
        assert!(state.agents.get(pane).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_emits_change() {
        let (mut state, pane) = state_with_pane();
        let (tx, mut rx) = mpsc::channel(4);
        state
            .events
            .subscribe(vec!["agent-changed".to_string()], None, tx)
            .expect("subscribe");
        apply(
            &mut state,
            pane,
            report(ReportState::Set(AgentState::Blocked), 1),
        );
        state.events.flush(&state.model);
        let event = rx.try_recv().expect("event");
        assert!(matches!(
            event,
            Event::AgentChanged { old: None, new: Some(n), .. } if n == "blocked"
        ));
        // The same report again changes nothing.
        apply(
            &mut state,
            pane,
            report(ReportState::Set(AgentState::Blocked), 2),
        );
        state.events.flush(&state.model);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jump_prefers_own_session_for_linked_window() {
        use crate::server::clients::{self, AttachRequest};
        let (mut state, pane) = state_with_pane();
        let ctx = test_ctx();
        let run = |state: &mut State, words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            dispatch::run(state, &ctx, &argv).expect("run")
        };
        run(&mut state, &["new-session", "-d", "-s", "b", "sleep 600"]);
        run(&mut state, &["new-window", "-t", "b", "sleep 600"]);
        let window = state.model.panes[&pane].window;
        let b = state
            .model
            .sessions
            .values()
            .find(|s| s.name == "b")
            .map(|s| s.id)
            .expect("session b");
        run(
            &mut state,
            &["link-window", "-s", &format!("a:{window}"), "-t", "b:9"],
        );
        let first_b = state.model.sessions[&b].windows[0];
        state.model.select_window_in(b, first_b);
        let accepted = clients::attach(
            &mut state,
            AttachRequest {
                ctx: ClientCtx {
                    client: None,
                    ..test_ctx()
                },
                size: (80, 24),
                term: "xterm".into(),
                caps: Default::default(),
                pid: None,
                target: Some("b".into()),
                detach_others: false,
                nest: false,
                tty: None,
                created: false,
                extras: Default::default(),
            },
        )
        .expect("attach");
        let ctx = ClientCtx {
            client: Some(accepted.id),
            ..test_ctx()
        };
        jump(&mut state, &ctx, pane).expect("jump");
        assert_eq!(
            state.clients[&accepted.id].session, b,
            "stays in its own session"
        );
        assert_eq!(state.model.sessions[&b].active, window);
        assert!(is_seen(&state, pane));
        let _ = Arc::new(());
    }

    #[test]
    fn configure_merges_kinds_over_builtins() {
        let mut state = test_state();
        let mut resolved = Resolved::default();
        let kinds = &mut resolved.agents.kinds;
        kinds.entry("claude".into()).or_default().resume = Some("claude -r {id}".into());
        kinds.entry("pi".into()).or_default().enabled = Some(false);
        kinds.entry("mine".into()).or_default().matches = Some(vec!["m".into()]);
        configure(&mut state, &resolved);
        let find = |n: &str| state.agent_kinds.iter().find(|k| k.name == n);
        assert_eq!(
            find("claude").and_then(|k| k.resume.as_deref()),
            Some("claude -r {id}")
        );
        assert!(find("pi").is_none());
        assert_eq!(
            find("mine").map(|k| k.matches.clone()),
            Some(vec!["m".to_string()])
        );
        assert!(find("codex").is_some());
    }

    fn attach_unfocused(state: &mut State) -> crate::server::clients::Accepted {
        use crate::server::clients::{self, AttachRequest};
        let accepted = clients::attach(
            state,
            AttachRequest {
                ctx: ClientCtx {
                    client: None,
                    ..test_ctx()
                },
                size: (80, 24),
                term: "xterm".into(),
                caps: Default::default(),
                pid: None,
                target: Some("a".into()),
                detach_others: false,
                nest: false,
                tty: None,
                created: false,
                extras: Default::default(),
            },
        )
        .expect("attach");
        if let Some(c) = state.clients.get_mut(&accepted.id) {
            c.focused = false;
        }
        accepted
    }

    fn blocked(msg: &str, ts: u64) -> Report {
        let mut r = report(ReportState::Set(AgentState::Blocked), ts);
        r.msg = Some(msg.into());
        r
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn change_raises_and_marks_dirty() {
        let (mut state, pane) = state_with_pane();
        state.notify.cfg.delivery = "off".into();
        let _accepted = attach_unfocused(&mut state);
        let soon = || Instant::now() + std::time::Duration::from_millis(1200);
        crate::server::clients::tick(
            &mut state,
            Instant::now() + std::time::Duration::from_secs(1),
        );
        assert!(crate::server::clients::deadline(&state).is_none_or(|d| d > soon()));
        apply(&mut state, pane, blocked("perm", 1));
        let items = state.notify.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].state, "blocked");
        assert_eq!(items[0].msg.as_deref(), Some("perm"));
        assert!(
            crate::server::clients::deadline(&state).is_some_and(|d| d <= soon()),
            "clients redraw"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_unfocused_sends_osc9() {
        let (mut state, pane) = state_with_pane();
        let mut accepted = attach_unfocused(&mut state);
        state.notify.cfg.delivery = "auto".into();
        apply(&mut state, pane, blocked("perm", 1));
        crate::server::clients::tick(
            &mut state,
            Instant::now() + std::time::Duration::from_secs(1),
        );
        let mut out = Vec::new();
        while let Ok(frame) = accepted.output.try_recv() {
            out.extend(frame);
        }
        let out = String::from_utf8_lossy(&out);
        assert!(
            out.contains("\x1b]9;grofe: claude: a:1.1 blocked: perm\x1b\\"),
            "{out:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_no_client_runs_command() {
        let (mut state, pane) = state_with_pane();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("out");
        state.notify.cfg.delivery = "auto".into();
        state.notify.cfg.command = Some(
            [
                "sh",
                "-c",
                &format!("printf '%s' \"$0\" > {}", file.display()),
                "{message}",
            ]
            .map(String::from)
            .to_vec(),
        );
        apply(&mut state, pane, blocked("perm", 1));
        for _ in 0..100 {
            if file.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let text = std::fs::read_to_string(&file).expect("command ran");
        assert_eq!(text, "a:1.1 blocked: perm");
    }

    /// `auto` (spec §6.4): with a client attached and none known unfocused
    /// nothing leaves the terminal; once one is unfocused OSC 9 does, and the
    /// command never runs while any client is attached.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_with_a_focused_client_delivers_nothing() {
        let (mut state, pane) = state_with_pane();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("ran");
        state.notify.cfg.delivery = "auto".into();
        state.notify.cfg.skip_when_seen = false;
        state.notify.cfg.min_interval = Duration::ZERO;
        state.notify.cfg.command = Some(
            ["sh", "-c", &format!("touch {}", file.display())]
                .map(String::from)
                .to_vec(),
        );
        let mut accepted = attach_unfocused(&mut state);
        if let Some(c) = state.clients.get_mut(&accepted.id) {
            c.focused = true;
        }
        let mut at = Instant::now();
        let mut out = |state: &mut State, rx: &mut mpsc::Receiver<Vec<u8>>| {
            at += Duration::from_secs(1);
            crate::server::clients::tick(state, at);
            String::from_utf8_lossy(&drain(rx)).into_owned()
        };
        apply(&mut state, pane, blocked("focused", 1));
        assert_eq!(state.notify.items().len(), 1, "raised to the tray");
        assert!(!out(&mut state, &mut accepted.output).contains("\x1b]9;"));

        if let Some(c) = state.clients.get_mut(&accepted.id) {
            c.focused = false;
        }
        apply(
            &mut state,
            pane,
            report(ReportState::Set(AgentState::Working), 2),
        );
        apply(&mut state, pane, blocked("unfocused", 3));
        assert!(
            out(&mut state, &mut accepted.output)
                .contains("\x1b]9;grofe: claude: a:1.1 blocked: unfocused")
        );

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!file.exists(), "the command ran with a client attached");
    }

    /// Spec §2.4: with `focus-events off` a focus-out report does not make
    /// the client known unfocused, so its active pane counts as seen and
    /// `auto` sends no OSC 9; with it on, the same client is unfocused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn focus_events_off_counts_an_unfocused_client_as_focused() {
        use crate::server::dispatch::options::Owner;
        let (mut state, pane) = state_with_pane();
        let mut accepted = attach_unfocused(&mut state);
        state.notify.cfg.delivery = "auto".into();
        state.notify.cfg.skip_when_seen = false;
        state
            .model
            .options
            .set(Owner::Global, "focus-events", "off", false)
            .expect("set");
        assert!(is_seen(&state, pane), "unknown focus counts as seen");
        apply(&mut state, pane, blocked("perm", 1));
        assert_eq!(state.notify.items().len(), 1, "raised to the tray");
        crate::server::clients::tick(
            &mut state,
            Instant::now() + std::time::Duration::from_secs(1),
        );
        let out = String::from_utf8_lossy(&drain(&mut accepted.output)).into_owned();
        assert!(!out.contains("\x1b]9;"), "{out:?}");

        state
            .model
            .options
            .set(Owner::Global, "focus-events", "on", false)
            .expect("set");
        assert!(!is_seen(&state, pane), "known unfocused is not seen");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worked_time_from_working_since() {
        let (mut state, pane) = state_with_pane();
        state.notify.cfg.delivery = "off".into();
        state.notify.cfg.done_min_work = std::time::Duration::from_millis(100);
        let set = |s, ts| report(ReportState::Set(s), ts);
        apply(&mut state, pane, set(AgentState::Working, 1));
        apply(&mut state, pane, set(AgentState::Done, 2));
        assert!(state.notify.items().is_empty(), "no work, no done");
        apply(&mut state, pane, set(AgentState::Working, 3));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        apply(&mut state, pane, set(AgentState::Done, 4));
        assert_eq!(state.notify.items().len(), 1);
    }

    #[test]
    fn configure_merges_kind_worktree() {
        let mut state = test_state();
        let mut resolved = Resolved::default();
        resolved
            .agents
            .kinds
            .entry("claude".into())
            .or_default()
            .worktree = Some("always".into());
        configure(&mut state, &resolved);
        let find = |n: &str| state.agent_kinds.iter().find(|k| k.name == n);
        assert_eq!(
            find("claude").and_then(|k| k.worktree.as_deref()),
            Some("always")
        );
        assert_eq!(find("codex").and_then(|k| k.worktree.clone()), None);
    }

    /// Agent text with a format job, two status ranges, ESC, BEL and a C1
    /// control. `x` is a file only a run command would make.
    fn evil(x: &std::path::Path) -> String {
        format!(
            "A\x1b]2;PWN\x07B\u{85}C\x1b[31mD#[range=user|x]E##[range=user|y]F#(touch {})",
            x.display()
        )
    }

    /// What the sanitised text looks like on a surface that draws it raw.
    const SHOWN: &str = "A]2;PWNBC[31mD##[range=user|x]E####[range=user|y]F_(touch";

    fn drain(rx: &mut mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.extend(frame);
        }
        out
    }

    /// No byte of the injected controls reaches the client: no ESC before
    /// the OSC or SGR text, no BEL, no C1 (UTF-8 `C2 85`).
    fn assert_inert(bytes: &[u8], what: &str) {
        let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        let text = String::from_utf8_lossy(bytes);
        assert!(!has(b"\x1b]2;PWN"), "{what}: OSC reached the client");
        assert!(!has(b"PWN\x07"), "{what}: BEL reached the client");
        assert!(!has(b"\x1b[31mD"), "{what}: SGR reached the client");
        assert!(!has("\u{85}".as_bytes()), "{what}: C1 reached the client");
        assert!(!has(b"#(touch"), "{what}: a format job survived");
        assert!(text.contains("PWNBC[31mD"), "{what}: not drawn: {text:?}");
    }

    /// Spec §2.7 at every sink an attached client sees: the status line
    /// (`#{agent_msg}` runs no job and opens no range), the toast, the
    /// picker, the tray, the sidebar and the grid.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_text_is_inert_on_every_surface() {
        let (mut state, p1) = state_with_pane();
        crate::server::register_builtins(&mut state);
        state.notify.cfg.delivery = "off".into();
        let dir = tempfile::tempdir().expect("tempdir");
        let x = dir.path().join("X");
        let run = |state: &mut State, ctx: &ClientCtx, words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            dispatch::run(state, ctx, &argv).unwrap_or_else(|e| panic!("{words:?}: {e}"));
        };
        run(
            &mut state,
            &test_ctx(),
            &["new-window", "-t", "a", "sleep 600"],
        );
        let p2 = *state
            .model
            .panes
            .keys()
            .find(|p| **p != p1)
            .expect("second pane");
        for (option, value) in [("status-left", ""), ("status-right-length", "200")] {
            run(
                &mut state,
                &test_ctx(),
                &["set-option", "-g", option, value],
            );
        }
        let mut accepted = {
            use crate::server::clients::{self, AttachRequest};
            clients::attach(
                &mut state,
                AttachRequest {
                    ctx: ClientCtx {
                        client: None,
                        ..test_ctx()
                    },
                    size: (160, 50),
                    term: "xterm".into(),
                    caps: Default::default(),
                    pid: None,
                    target: Some("a".into()),
                    detach_others: false,
                    nest: false,
                    tty: None,
                    created: false,
                    extras: Default::default(),
                },
            )
            .expect("attach")
        };
        let ctx = ClientCtx {
            client: Some(accepted.id),
            ..test_ctx()
        };
        let mut at = Instant::now();
        let mut frame = |state: &mut State, rx: &mut mpsc::Receiver<Vec<u8>>| {
            at += Duration::from_secs(1);
            clients::tick(state, at);
            drain(rx)
        };
        frame(&mut state, &mut accepted.output);
        // p2 is in view (status line); p1 is not, so it toasts.
        apply(&mut state, p2, blocked(&evil(&x), 1));
        apply(&mut state, p1, blocked(&evil(&x), 1));
        let shown = state.agents.get(p1).and_then(|a| a.msg.clone());
        assert!(shown.is_some_and(|m| m.starts_with(SHOWN)));

        let out = frame(&mut state, &mut accepted.output);
        assert_inert(&out, "toast");
        let toast = crate::server::toast::deadline(&state, accepted.id);
        assert!(toast.is_some(), "a toast is up");

        run(
            &mut state,
            &test_ctx(),
            &["set-option", "-g", "status-right", "#{agent_msg}"],
        );
        clients::mark_all_dirty(&mut state);
        let out = frame(&mut state, &mut accepted.output);
        assert_inert(&out, "status line");
        let (rows, _) = clients::drawn_status(&state, accepted.id).expect("status");
        use crate::server::status::RangeKind;
        let user: Vec<_> = rows
            .iter()
            .flat_map(|r| &r.ranges)
            .filter(|r| matches!(r.kind, RangeKind::User(_)))
            .collect();
        assert!(user.is_empty(), "agent text opened a range: {user:?}");
        let line: String = rows
            .iter()
            .flat_map(|r| &r.line.spans)
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            line.contains("D#[range=user|x]E##[range=user|y]F_(touch"),
            "{line:?}"
        );
        for surface in [
            &["choose-agent"][..],
            &["choose-notification"],
            &["agent-sidebar", "on"],
            &["agent-grid"],
        ] {
            run(&mut state, &ctx, surface);
            let out = frame(&mut state, &mut accepted.output);
            assert_inert(&out, &surface.join(" "));
            // Close it so the next one is not drawn under it.
            let id = accepted.id;
            match surface[0] {
                "choose-agent" => crate::server::picker::client_gone(&mut state, id),
                "choose-notification" => crate::server::tray::client_gone(&mut state, id),
                _ => {}
            }
            clients::mark_all_dirty(&mut state);
            frame(&mut state, &mut accepted.output);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!x.exists(), "a job ran agent text");
    }

    /// Spec §2.7 and §6.4 at the outbound sinks: OSC 9 to an unfocused
    /// client carries no control byte, and the notifier gets the text as
    /// one argv element that no shell reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_text_is_inert_in_osc9_and_notifier_argv() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Short marker path: agent text is capped at 200 chars, and a long
        // temp path would cut the probes off.
        let short = std::path::PathBuf::from(format!(
            "/tmp/g{}{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        std::fs::create_dir_all(&short).expect("short dir");
        let x = short.join("X");
        let (mut state, pane) = state_with_pane();
        let mut accepted = attach_unfocused(&mut state);
        state.notify.cfg.delivery = "auto".into();
        apply(&mut state, pane, blocked(&evil(&x), 1));
        crate::server::clients::tick(&mut state, Instant::now() + Duration::from_secs(1));
        let out = drain(&mut accepted.output);
        let text = String::from_utf8_lossy(&out);
        let start = text.find("\x1b]9;").expect("an OSC 9") + 4;
        let end = start + text[start..].find("\x1b\\").expect("its ST");
        let body = &text[start..end];
        assert!(!body.chars().any(char::is_control), "{body:?}");
        assert!(body.contains(SHOWN), "{body:?}");

        let (mut state, pane) = state_with_pane();
        let file = dir.path().join("argv");
        state.notify.cfg.delivery = "auto".into();
        state.notify.cfg.command = Some(
            [
                "sh",
                "-c",
                &format!("printf '%s' \"$1\" > {}", file.display()),
                "sh",
                "{message}",
            ]
            .map(String::from)
            .to_vec(),
        );
        let msg = format!(
            "{} $(touch {}) `touch {}`",
            evil(&x),
            x.display(),
            x.display()
        );
        apply(&mut state, pane, blocked(&msg, 1));
        // The shell's `>` creates the file before printf writes it: wait on content.
        for _ in 0..100 {
            if std::fs::read_to_string(&file).is_ok_and(|s| !s.is_empty()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let got = std::fs::read_to_string(&file).expect("notifier ran");
        assert!(
            got.starts_with(&format!("a:1.1 blocked: {SHOWN}")),
            "{got:?}"
        );
        assert!(got.contains("$(touch") && got.contains("`touch"), "{got:?}");
        assert!(!got.chars().any(char::is_control), "{got:?}");
        assert!(!x.exists(), "the notifier argv went through a shell");
        let _ = std::fs::remove_dir_all(&short);
    }

    #[test]
    fn a_reporting_cli_binds_to_the_pane_root() {
        let argv = |s: &str| Some(vec![s.to_string(), "agent".to_string()]);
        assert_eq!(bind_pid(50, argv("/usr/bin/grofe").as_deref(), Some(7)), 7);
        assert_eq!(bind_pid(50, argv("tmux").as_deref(), Some(7)), 7);
        assert_eq!(bind_pid(50, argv("claude").as_deref(), Some(7)), 50);
        assert_eq!(bind_pid(50, argv("grofe").as_deref(), None), 50);
        assert_eq!(bind_pid(50, None, Some(7)), 50);
    }
}
