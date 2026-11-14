//! Server-side agent entry point: the one place reports, bells and jumps
//! reach the agent store (spec §2.2 to §2.6, §5.2).

use std::time::Instant;

use crate::config::Resolved;
use crate::protocol::ClientCtx;
use crate::server::agents::{Change, Report, ReportState, kinds};
use crate::server::resume::Binding;
use crate::server::{State, clients, events, hooks, resume};
use crate::types::{ClientId, PaneId};

/// Whether `pane` is the active pane of the current window of an attached
/// client whose outer terminal is focused (a client with no focus report
/// yet counts as focused, spec §2.4).
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
        c.focused
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
    clients::mark_all_dirty(state);
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
    }
    state.agent_kinds = all;
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
fn foreground_binding(state: &State, pane: PaneId) -> Option<Binding> {
    let p = state.model.panes.get(&pane)?;
    Binding::of(p.handle.foreground_group()?)
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
}
