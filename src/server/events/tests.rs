use super::*;
use crate::types::PaneId;

fn changed(value: &str) -> Event {
    Event::OptionChanged {
        scope: "global".into(),
        name: "@x".into(),
        value: Some(value.into()),
    }
}

fn names(n: &[&str]) -> Vec<String> {
    n.iter().map(|s| s.to_string()).collect()
}

#[test]
fn pane_output_only_when_named() {
    let (tx, _rx) = mpsc::channel(1);
    let bare = Subscriber {
        filters: vec![],
        scope: None,
        gone: false,
        tx: tx.clone(),
        dropped: 0,
    };
    let star = Subscriber {
        filters: names(&["*"]),
        scope: None,
        gone: false,
        tx: tx.clone(),
        dropped: 0,
    };
    let named = Subscriber {
        filters: names(&["pane-output"]),
        scope: None,
        gone: false,
        tx,
        dropped: 0,
    };
    assert!(bare.wants("pane-exited") && !bare.wants("pane-output"));
    assert!(star.wants("option-changed") && !star.wants("pane-output"));
    assert!(named.wants("pane-output") && !named.wants("pane-exited"));
}

#[tokio::test]
async fn overflow_sends_one_notice_with_the_count() {
    let mut events = Events::default();
    let (tx, mut rx) = mpsc::channel(2);
    events.subscribe(vec![], None, tx).unwrap();
    for i in 0..5 {
        events.emit(changed(&i.to_string()));
    }
    events.flush(&Model::default());
    assert_eq!(rx.recv().await, Some(changed("0")));
    assert_eq!(rx.recv().await, Some(changed("1")));
    assert!(rx.try_recv().is_err());
    events.emit(changed("5"));
    events.flush(&Model::default());
    assert_eq!(rx.recv().await, Some(Event::EventsDropped { count: 3 }));
    assert_eq!(rx.recv().await, Some(changed("5")));
}

#[test]
fn closed_subscribers_are_removed_on_the_next_flush() {
    let mut events = Events::default();
    let (live_tx, mut live_rx) = mpsc::channel(8);
    events.subscribe(vec![], None, live_tx).unwrap();
    for _ in 0..10 {
        let (tx, rx) = mpsc::channel(8);
        events.subscribe(vec![], None, tx).unwrap();
        drop(rx);
    }
    events.emit(changed("1"));
    events.flush(&Model::default());
    assert_eq!(events.len(), 1);
    assert_eq!(live_rx.try_recv().ok(), Some(changed("1")));
}

#[test]
fn resubscribing_replaces_the_filters() {
    let mut events = Events::default();
    let (tx, mut rx) = mpsc::channel(8);
    events
        .subscribe(names(&["pane-exited"]), None, tx.clone())
        .unwrap();
    events
        .subscribe(names(&["option-changed"]), None, tx)
        .unwrap();
    assert_eq!(events.len(), 1);
    events.emit(Event::PaneExited {
        pane: PaneId(1),
        uid: "u".into(),
        status: None,
    });
    events.emit(changed("1"));
    events.flush(&Model::default());
    assert_eq!(rx.try_recv().ok(), Some(changed("1")));
    assert!(rx.try_recv().is_err());
}

#[test]
fn unknown_names_are_refused() {
    let mut events = Events::default();
    let (tx, _rx) = mpsc::channel(1);
    let err = events.subscribe(names(&["pane-exitted"]), None, tx.clone());
    assert_eq!(err, Err("unknown event: pane-exitted".to_string()));
    assert!(events.subscribe(names(&["window-*"]), None, tx).is_ok());
}

fn exited(pane: u32) -> Event {
    Event::PaneExited {
        pane: PaneId(pane),
        uid: "u".into(),
        status: None,
    }
}

#[test]
fn a_scope_whose_target_has_gone_passes_nothing_more() {
    let model = Model::default();
    let mut events = Events::default();
    let (tx, mut rx) = mpsc::channel(8);
    let scope = Some(Scope::Pane(PaneId(1)));
    events.subscribe(vec![], scope, tx).unwrap();
    // The target is not in the model: its own last events still pass,
    // and the other pane's do not.
    events.emit(exited(1));
    events.emit(exited(2));
    events.flush(&model);
    assert_eq!(rx.try_recv().ok(), Some(exited(1)));
    assert!(rx.try_recv().is_err());
    events.emit(exited(1));
    events.flush(&model);
    assert!(rx.try_recv().is_err());
}

#[test]
fn a_killed_panes_place_decides_the_window_and_session_scopes() {
    let model = Model::default();
    let place = Place {
        window: Some(WindowId(4)),
        session: Some(SessionId(2)),
    };
    let event = exited(9);
    assert!(Scope::Window(WindowId(4)).passes(&event, place, &model));
    assert!(Scope::Session(SessionId(2)).passes(&event, place, &model));
    assert!(Scope::Pane(PaneId(9)).passes(&event, place, &model));
    assert!(!Scope::Window(WindowId(5)).passes(&event, place, &model));
    assert!(!Scope::Session(SessionId(3)).passes(&event, place, &model));
    // Without a place and a model that lacks the pane, it names no window.
    assert!(!Scope::Window(WindowId(4)).passes(&event, Place::default(), &model));
}

/// A window linked into two sessions is in both: an event of the window
/// reaches a subscriber scoped to either session, not a third.
#[tokio::test]
async fn a_session_scope_sees_the_events_of_a_window_it_holds() {
    let mut state = crate::server::test_state();
    let ctx = crate::server::test_ctx();
    let run = |state: &mut State, words: &[&str]| {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        crate::server::dispatch::run(state, &ctx, &argv).expect("command")
    };
    for name in ["a", "b", "c"] {
        run(&mut state, &["new-session", "-d", "-s", name, "sleep 30"]);
    }
    let model = &state.model;
    let id = |name: &str| model.session_by_name(name).expect("session");
    let (a, b, c) = (id("a"), id("b"), id("c"));
    let window = model.sessions[&a].active;
    let src = window.to_string();
    run(&mut state, &["link-window", "-d", "-s", &src, "-t", "b:7"]);
    let model = &state.model;
    let place = Place {
        window: Some(window),
        session: Some(a),
    };
    let event = exited(9);
    assert!(Scope::Session(a).passes(&event, place, model));
    assert!(Scope::Session(b).passes(&event, place, model));
    assert!(!Scope::Session(c).passes(&event, place, model));
}

#[test]
fn option_changed_passes_by_its_scope() {
    let model = Model::default();
    let changed = |scope: &str| Event::OptionChanged {
        scope: scope.into(),
        name: "@x".into(),
        value: None,
    };
    let none = Place::default();
    assert!(Scope::Session(SessionId(1)).passes(&changed("$1"), none, &model));
    assert!(!Scope::Session(SessionId(1)).passes(&changed("global"), none, &model));
    assert!(Scope::Window(WindowId(2)).passes(&changed("@2"), none, &model));
    assert!(Scope::Pane(PaneId(3)).passes(&changed("%3"), none, &model));
    assert!(!Scope::Pane(PaneId(3)).passes(&changed("%4"), none, &model));
}

/// `subscribe -c`: the client's session's events, then the next one's
/// after it switches; an event of no place (a global option) passes.
#[test]
fn a_client_scope_follows_its_client() {
    let model = Model::default();
    let place = |s: u32| Place {
        window: Some(WindowId(s)),
        session: Some(SessionId(s)),
    };
    let mut events = Events::default();
    let (tx, mut rx) = mpsc::channel(8);
    let scope = Scope::Client {
        client: ClientId(7),
        session: SessionId(1),
    };
    events.subscribe(vec![], Some(scope), tx).unwrap();
    let global = Event::OptionChanged {
        scope: "global".into(),
        name: "".into(),
        value: None,
    };
    assert!(scope.passes(&global, Place::default(), &model));
    // An open-url from no pane is not the client's.
    let paneless = Event::OpenUrl {
        url: "https://a/".into(),
        pane: None,
    };
    assert!(!scope.passes(&paneless, Place::default(), &model));
    events.emit_at(exited(1), place(1));
    events.emit_at(exited(2), place(2));
    events.flush(&model);
    assert_eq!(rx.try_recv().ok(), Some(exited(1)));
    assert!(rx.try_recv().is_err());
    // Another client moving changes nothing; this one moving does.
    events.client_moved(ClientId(8), SessionId(1));
    events.client_moved(ClientId(7), SessionId(2));
    events.emit_at(exited(1), place(1));
    events.emit_at(exited(2), place(2));
    events.flush(&model);
    assert_eq!(rx.try_recv().ok(), Some(exited(2)));
    assert!(rx.try_recv().is_err());
}

#[test]
fn a_client_scope_reaches_only_while_its_client_is_attached() {
    let model = Model::default();
    let mut events = Events::default();
    let (tx, _rx) = mpsc::channel(8);
    let scope = Scope::Client {
        client: ClientId(7),
        session: SessionId(1),
    };
    events
        .subscribe(names(&["option-changed"]), Some(scope), tx)
        .unwrap();
    let event = Event::OptionChanged {
        scope: "global".into(),
        name: "".into(),
        value: None,
    };
    assert!(events.reaches(&event, &model, |c| c == ClientId(7)));
    assert!(!events.reaches(&event, &model, |_| false));
}

/// Events added after v1.0 reach only a subscription that names them,
/// not a bare one or a pattern (protocol-v1 §8).
#[test]
fn remote_events_need_their_own_name() {
    let (tx, _rx) = mpsc::channel(4);
    let mut events = Events::default();
    events.subscribe(vec![], None, tx).unwrap();
    for name in ["open-url", "ports-changed", "pane-echo"] {
        assert!(!events.named(name), "{name}");
    }
    let (tx, _rx2) = mpsc::channel(4);
    events
        .subscribe(vec!["*".to_string()], None, tx.clone())
        .unwrap();
    let sub = &events.subscribers[1];
    assert!(!sub.wants("open-url") && !sub.wants("ports-changed") && !sub.wants("pane-echo"));
    assert!(sub.wants("pane-exited"));
    events
        .subscribe(vec!["pane-echo".to_string()], None, tx)
        .unwrap();
    assert!(events.named("pane-echo") && !events.named("ports-changed"));
}

#[test]
fn agent_changed_in_names() {
    assert!(NAMES.contains(&"agent-changed"));
    let mut events = Events::default();
    let (tx, _rx) = mpsc::channel(4);
    events
        .subscribe(names(&["agent-changed"]), None, tx)
        .expect("subscribe");
}

#[test]
fn set_hook_agent_changed_accepted() {
    let mut state = crate::server::test_state();
    let ctx = crate::server::test_ctx();
    let argv: Vec<String> = ["set-hook", "-g", "agent-changed", "list-sessions"]
        .iter()
        .map(|w| w.to_string())
        .collect();
    crate::server::dispatch::run(&mut state, &ctx, &argv).expect("set-hook");
}

/// A state with one session `main`, one attached client, and an agent
/// record on its active pane.
fn agent_state() -> (State, PaneId, ClientId) {
    use crate::server::agents::{AgentState, Report, ReportState};
    use crate::server::clients::{AttachRequest, attach};
    let mut state = crate::server::test_state();
    let ctx = crate::server::test_ctx();
    let argv: Vec<String> = ["new-session", "-d", "-s", "main", "sleep 30"]
        .iter()
        .map(|w| w.to_string())
        .collect();
    crate::server::dispatch::run(&mut state, &ctx, &argv).expect("new-session");
    let accepted = attach(
        &mut state,
        AttachRequest {
            ctx: ctx.clone(),
            size: (80, 24),
            term: "xterm".to_string(),
            caps: Default::default(),
            pid: None,
            target: Some("main".to_string()),
            detach_others: false,
            nest: true,
            tty: None,
            created: false,
            extras: Default::default(),
        },
    )
    .expect("attach");
    let session = state.model.session_by_name("main").expect("session");
    let window = state.model.sessions[&session].active;
    let pane = state.model.windows[&window].active;
    let report = Report {
        state: ReportState::Set(AgentState::Done),
        kind: None,
        msg: None,
        blocked: None,
        session_id: None,
        ts: 1,
    };
    state
        .agents
        .apply(pane, report, false, std::time::Instant::now());
    (state, pane, accepted.id)
}

#[tokio::test]
async fn seen_clears_unseen_focused() {
    let (mut state, pane, _) = agent_state();
    assert!(state.agents.get(pane).expect("record").unseen);
    track_focus(&mut state);
    assert!(!state.agents.get(pane).expect("record").unseen);
}

#[tokio::test]
async fn unfocused_client_not_seen() {
    let (mut state, pane, id) = agent_state();
    state.clients.get_mut(&id).expect("client").focused = false;
    track_focus(&mut state);
    assert!(state.agents.get(pane).expect("record").unseen);
}

#[tokio::test]
async fn focus_unknown_counts_seen() {
    // A client that never reported focus starts focused, as in tmux.
    let (mut state, pane, id) = agent_state();
    assert!(state.clients[&id].focused);
    track_focus(&mut state);
    assert!(!state.agents.get(pane).expect("record").unseen);
}

#[tokio::test]
async fn emit_agent_changed_names_the_pane_and_states() {
    use crate::server::agents::{AgentState, Change};
    let (mut state, pane, _) = agent_state();
    let (tx, mut rx) = mpsc::channel(4);
    state
        .events
        .subscribe(names(&["agent-changed"]), None, tx)
        .expect("subscribe");
    let change = Change {
        old: None,
        new: Some(AgentState::Idle),
        kind: None,
        session_changed: false,
    };
    emit_agent_changed(&mut state, pane, &change);
    state.events.flush(&state.model);
    let event = rx.try_recv().expect("event");
    let uid = state.model.panes[&pane].uid.clone();
    assert_eq!(event.name(), "agent-changed");
    assert!(matches!(
        event,
        Event::AgentChanged { pane: p, uid: u, old: None, new: Some(n), .. }
            if p == pane && u == uid && n == "idle"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seen_marks_read() {
    use crate::server::{dispatch, test_ctx, test_state};
    let mut state = test_state();
    let argv = ["new-session", "-d", "-s", "a", "sleep 600"].map(String::from);
    dispatch::run(&mut state, &test_ctx(), &argv).expect("new-session");
    let pane = *state.model.panes.keys().next().expect("pane");
    state.notify.cfg.delivery = "off".into();
    let accepted = crate::server::clients::attach(
        &mut state,
        crate::server::clients::AttachRequest {
            ctx: crate::protocol::ClientCtx {
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
    state.clients.get_mut(&accepted.id).expect("client").focused = false;
    let report = crate::server::agents::Report {
        state: crate::server::agents::ReportState::Set(crate::server::agents::AgentState::Blocked),
        kind: Some("claude".into()),
        msg: Some("perm".into()),
        blocked: None,
        session_id: None,
        ts: 1,
    };
    dispatch::agent::apply(&mut state, pane, report);
    assert!(!state.notify.items()[0].read);
    state.clients.get_mut(&accepted.id).expect("client").focused = true;
    mark_agents_seen(&mut state);
    assert!(state.notify.items()[0].read);
}
