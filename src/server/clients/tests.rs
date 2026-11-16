use std::path::PathBuf;

#[test]
fn a_read_only_client_drives_a_mode_only_with_a_real_x_flag() {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(read_only_command(&v(&["send-keys", "-X", "cancel"])));
    assert!(read_only_command(&v(&["send-keys", "-RX", "cancel"])));
    assert!(!read_only_command(&v(&["send-keys", "-tX", "ls"])));
    assert!(!read_only_command(&v(&["send-keys", "ls"])));
}
use std::sync::Arc;

use super::*;

fn pane_with_agents() -> (State, PaneId) {
    let mut state = crate::server::test_state();
    crate::server::dispatch::run(
        &mut state,
        &crate::server::test_ctx(),
        &["new-session".into(), "-d".into(), "-s".into(), "m".into()],
    )
    .expect("new-session");
    state.agents_enabled = true;
    state.agent_kinds = crate::server::agents::kinds::builtin();
    let id = *state.model.panes.keys().next().expect("pane");
    (state, id)
}

/// A client whose picker previews a pane redraws when that pane
/// outputs, though it shows another window.
#[tokio::test]
async fn wake_redraws_picker_preview() {
    let (mut state, id) = pane_with_agents();
    let (outbox, _) = Outbox::new();
    let mut entry = state_with_client()
        .0
        .clients
        .remove(&ClientId(1))
        .expect("client");
    entry.outbox = outbox;
    state.clients.insert(ClientId(1), entry);
    let report = crate::server::agents::Report {
        state: crate::server::agents::ReportState::Set(crate::server::agents::AgentState::Idle),
        kind: Some("claude".into()),
        msg: None,
        blocked: None,
        session_id: None,
        ts: 1,
    };
    state.agents.apply(id, report, false, Instant::now());
    crate::server::picker::open(&mut state, ClientId(1)).expect("open");
    assert_eq!(
        crate::server::picker::selected_pane(&state, ClientId(1)),
        Some(id)
    );
    state.clients.get_mut(&ClientId(1)).expect("client").dirty = false;
    state.model.panes[&id].handle.reset_state();
    wake(&mut state);
    assert!(state.clients[&ClientId(1)].dirty);
}

/// An open picker asks for a redraw when a shown time changes, and
/// stops asking once closed or its client is gone.
#[tokio::test]
async fn picker_times_tick() {
    let (mut state, id) = pane_with_agents();
    let (outbox, _) = Outbox::new();
    let mut entry = state_with_client()
        .0
        .clients
        .remove(&ClientId(1))
        .expect("client");
    entry.outbox = outbox;
    state.clients.insert(ClientId(1), entry);
    let report = crate::server::agents::Report {
        state: crate::server::agents::ReportState::Set(crate::server::agents::AgentState::Idle),
        kind: Some("claude".into()),
        msg: None,
        blocked: None,
        session_id: None,
        ts: 1,
    };
    let since = Instant::now();
    state.agents.apply(id, report, false, since);
    assert_eq!(crate::server::picker::deadline(&state), None);
    crate::server::picker::open(&mut state, ClientId(1)).expect("open");
    let at = crate::server::picker::deadline(&state).expect("a deadline");
    assert!(at > since && at <= since + Duration::from_millis(1100));
    state.clients.get_mut(&ClientId(1)).expect("client").dirty = false;
    crate::server::picker::tick(&mut state, since + Duration::from_millis(500));
    assert!(!state.clients[&ClientId(1)].dirty, "not yet");
    crate::server::picker::tick(&mut state, at);
    assert!(state.clients[&ClientId(1)].dirty, "the time changed");
    assert!(crate::server::picker::deadline(&state).is_some_and(|next| next > at));
    crate::server::picker::client_gone(&mut state, ClientId(1));
    assert_eq!(crate::server::picker::deadline(&state), None);
}

/// Two outputs within the interval read the leader's argv once.
#[tokio::test]
async fn scan_throttled_per_pane() {
    let (mut state, id) = pane_with_agents();
    let reads = std::cell::Cell::new(0);
    let mut read = |_: &crate::server::model::Pane| {
        reads.set(reads.get() + 1);
        Some((i32::MAX, vec!["claude".to_string()]))
    };
    let panes = BTreeSet::from([id]);
    state.registry.forget_scan(id);
    let t0 = Instant::now();
    scan_agents_with(&mut state, &panes, t0, &mut read);
    scan_agents_with(
        &mut state,
        &panes,
        t0 + Duration::from_millis(900),
        &mut read,
    );
    assert_eq!(reads.get(), 1);
    scan_agents_with(
        &mut state,
        &panes,
        t0 + Duration::from_millis(1100),
        &mut read,
    );
    assert_eq!(reads.get(), 2);
    assert_eq!(
        state.agents.get(id).and_then(|a| a.kind.as_deref()),
        Some("claude")
    );
}

#[tokio::test]
async fn scan_skips_when_disabled() {
    let (mut state, id) = pane_with_agents();
    state.agents_enabled = false;
    let mut read = |_: &crate::server::model::Pane| -> Option<(i32, Vec<String>)> {
        panic!("read while disabled")
    };
    scan_agents_with(&mut state, &BTreeSet::from([id]), Instant::now(), &mut read);
    assert!(state.agents.get(id).is_none());
}

/// A hello's tty names the client only if it is a device path: else
/// it is taken as none and the client is `client-N`.
#[tokio::test]
async fn only_a_device_path_names_a_client() {
    assert!(usable_tty("/dev/ttys003"));
    assert!(usable_tty("/dev/pts/4"));
    for bad in [
        "ttys003",
        "/tmp/x",
        "/dev/pts/1\n/dev/pts/2",
        "/dev/\x1b[31mred",
        &format!("/dev/{}", "a".repeat(libc::PATH_MAX as usize)),
    ] {
        assert!(!usable_tty(bad), "{bad:?}");
    }
    let mut state = crate::server::test_state();
    crate::server::dispatch::run(
        &mut state,
        &crate::server::test_ctx(),
        &[
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            "main".to_string(),
        ],
    )
    .expect("new-session");
    let ctx = ClientCtx {
        dialect: crate::protocol::Dialect::Tmux,
        ..crate::server::test_ctx()
    };
    for (tty, name, kept) in [
        ("/dev/ttys009", "/dev/ttys009", Some("/dev/ttys009")),
        ("/dev/tty\x07s", "client-", None),
    ] {
        let accepted = attach(
            &mut state,
            AttachRequest {
                ctx: ctx.clone(),
                size: (80, 24),
                term: "xterm".to_string(),
                caps: Caps::default(),
                pid: None,
                target: Some("main".to_string()),
                detach_others: false,
                nest: true,
                tty: Some(tty.to_string()),
                created: false,
                extras: Default::default(),
            },
        )
        .expect("attach");
        let c = &state.clients[&accepted.id];
        assert!(c.name.starts_with(name), "{}", c.name);
        assert_eq!(c.tty.as_deref(), kept);
    }
}

/// As tmux 3.7b's `show-messages` printed them (recorded on `-L
/// grofe-fe2 -f /dev/null`).
#[test]
fn commands_print_as_tmux_logs_them() {
    let print = |words: &[&str]| {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        tmux_print(&argv).unwrap()
    };
    assert_eq!(
        print(&["set", "-sg", "escape-time", "25"]),
        "set-option -gs escape-time 25"
    );
    assert_eq!(
        print(&["set", "-as", "terminal-features", "*:RGB"]),
        "set-option -as terminal-features *:RGB"
    );
    assert_eq!(
        print(&["ls", "-f", "#{==:#{session_name},b}"]),
        "list-sessions -f \"#{==:#{session_name},b}\""
    );
    assert_eq!(print(&["has", "-t", "=ma"]), "has-session -t =ma");
    assert_eq!(
        print(&["new", "-d", "-s", "b", "-x", "80", "-y", "24"]),
        "new-session -d -s b -x 80 -y 24"
    );
    assert_eq!(print(&["show-messages"]), "show-messages");
    assert_eq!(print(&["show-messages", "-JT"]), "show-messages -JT");
}

#[test]
fn a_cli_request_is_logged_command_by_command_with_its_error() {
    let mut state = crate::server::test_state();
    let ctx = ClientCtx {
        dialect: crate::protocol::Dialect::Tmux,
        ..crate::server::test_ctx()
    };
    let argv: Vec<String> = ["splitw", "-h", ";", "has", "-t", "=ma", ";", "ls"]
        .map(String::from)
        .to_vec();
    let mut log = plan_command_log(&state, "client-7", &ctx, &argv, 3);
    // Two commands fire; the second fails, so the third never does.
    log_fired(&mut state, &mut log);
    log_fired(&mut state, &mut log);
    log_error(&mut state, "client-7", "can't find session: ma");
    let lines: Vec<&str> = messages(&state).map(|(_, l)| l.as_str()).collect();
    assert_eq!(
        lines,
        [
            "client-7 command: split-window -h",
            "client-7 command: has-session -t =ma",
            "client-7 message: can't find session: ma",
        ]
    );
    // A plan whose parts do not add up to the request logs nothing.
    assert!(plan_command_log(&state, "c", &ctx, &argv, 4).is_empty());
}

/// A server state with no sessions and one client, as the actor
/// would hold it.
fn state_with_client() -> (State, ClientId) {
    let mut state = crate::server::test_state();
    let id = ClientId(1);
    let (outbox, _) = Outbox::new();
    state.clients.insert(
        id,
        ClientEntry {
            name: client_name(id),
            tty: None,
            pid: None,
            session: SessionId(0),
            ctx: ClientCtx {
                cwd: PathBuf::from("/"),
                pane: None,
                env: Vec::new(),
                client: Some(id),
                dialect: crate::protocol::Dialect::Grofe,
                request: Default::default(),
            },
            size: (80, 24),
            pending_size: None,
            term: "xterm-256color".to_string(),
            caps: Caps::default(),
            activity: Instant::now(),
            activity_time: SystemTime::now(),
            decoder: Decoder::new(),
            keys: KeyState::new(),
            focused: true,
            mouse_prev: None,
            mouse_last: None,
            message: None,
            message_ignores_keys: false,
            status_rows: Vec::new(),
            status_at: 0,
            held: VecDeque::new(),
            held_bytes: 0,
            holds: 0,
            held_full: false,
            viewport: (0, 0),
            pan: None,
            suspended: false,
            suspend_tx: mpsc::channel(1).0,
            flags: ClientFlags::default(),
            mouse_modes: None,
            dirty: false,
            toast_until: std::cell::Cell::new(None),
            outbox,
            exit: None,
        },
    );
    (state, id)
}

/// Timer wakes the actor would take over `window` of idle time with one
/// attached client: each is a `deadline` that falls inside it.
fn idle_wakes(state: &mut State, window: Duration) -> usize {
    let start = Instant::now();
    tick(state, start);
    let mut wakes = 0;
    while let Some(due) = deadline(state).filter(|d| *d <= start + window) {
        wakes += 1;
        tick(state, due);
        assert!(wakes < 100, "runaway timer");
    }
    wakes
}

fn set_session_option(state: &mut State, name: &str, value: &str) {
    let session = state.clients[&ClientId(1)].session;
    state
        .model
        .options
        .set(Owner::Session(session), name, value, false)
        .unwrap();
}

/// The client whose toast expires is redrawn when the deadline passes,
/// though the toast itself is already gone by then.
#[test]
fn toast_deadline_wakes() {
    let (mut state, id) = state_with_client();
    let now = Instant::now();
    tick(&mut state, now);
    state.clients.get_mut(&id).expect("client").dirty = false;
    state.clients[&id]
        .toast_until
        .set(Some(now + Duration::from_secs(5)));
    tick(&mut state, now + Duration::from_secs(1));
    assert_ne!(
        state.registry.last_render,
        Some(now + Duration::from_secs(1))
    );
    tick(&mut state, now + Duration::from_secs(5));
    // Marked dirty and rendered in the same tick.
    assert_eq!(
        state.registry.last_render,
        Some(now + Duration::from_secs(5))
    );
    assert_eq!(state.clients[&id].toast_until.get(), None);
}

/// A blocked agent pane's frame takes `agent-blocked-border-style`,
/// active or not; a working one keeps the normal border styles.
#[tokio::test]
async fn blocked_pane_border_style() {
    use crate::server::agents::{AgentState, Report, ReportState};
    let (mut state, pane) = pane_with_agents();
    let id = ClientId(1);
    let entry = state_with_client().0.clients.remove(&id).expect("client");
    state.clients.insert(id, entry);
    state
        .model
        .options
        .set(
            Owner::Global,
            "agent-blocked-border-style",
            "fg=yellow",
            false,
        )
        .unwrap();
    let report = |s| Report {
        state: ReportState::Set(s),
        kind: None,
        msg: None,
        blocked: None,
        session_id: None,
        ts: 1,
    };
    let look = |state: &State| {
        let c = &state.clients[&id];
        let w = state.model.windows.values().next().expect("window");
        pane_looks(state, c, w, &[pane]).panes[&pane].clone()
    };
    let plain = look(&state);
    let now = Instant::now();
    let r = report(AgentState::Working);
    state.agents.apply(pane, r, false, now);
    assert_eq!(look(&state).border, plain.border);
    let r = Report {
        ts: 2,
        ..report(AgentState::Blocked)
    };
    state.agents.apply(pane, r, false, now);
    let blocked = look(&state);
    assert_ne!(blocked.border, plain.border);
    assert_eq!(blocked.active_border, blocked.border);
}

/// `lock-after-time` fires once per idle spell, as tmux's one-shot
/// session timer: a client that cannot be locked (no `lock-command`,
/// no session, another popup open, suspended) does not leave the
/// deadline in the past for the actor to spin on.
#[test]
fn a_lock_that_cannot_happen_does_not_wake_the_server_again() {
    let (mut state, _) = state_with_client();
    set_session_option(&mut state, "status-right", "x");
    set_session_option(&mut state, "lock-after-time", "1");
    set_session_option(&mut state, "lock-command", "");
    assert_eq!(idle_wakes(&mut state, Duration::from_secs(3)), 1);
}

/// G2: with a clock in the status line, an idle server wakes only for
/// the clock (once per `status-interval`), not every 500 ms for names.
#[test]
fn an_idle_server_wakes_only_for_the_status_clock() {
    let (mut state, _) = state_with_client();
    set_session_option(&mut state, "status-interval", "1");
    let wakes = idle_wakes(&mut state, Duration::from_millis(3500));
    assert!((3..=4).contains(&wakes), "{wakes} wakes in 3.5 s");
}

/// G2: a status line with nothing time-dependent in it needs no timer,
/// so an idle server does not wake at all.
#[test]
fn an_idle_server_with_a_static_status_line_never_wakes() {
    let (mut state, _) = state_with_client();
    set_session_option(&mut state, "status-interval", "1");
    set_session_option(&mut state, "status-right", "#{session_name}");
    assert_eq!(idle_wakes(&mut state, Duration::from_secs(3)), 0);
}

/// A timed mute wakes the server once, when it ends, and redraws; a
/// permanent one needs no timer.
#[test]
fn a_timed_mute_wakes_once_at_its_end() {
    let (mut state, id) = state_with_client();
    let now = Instant::now();
    state.notify.mute(None);
    assert_eq!(idle_wakes(&mut state, Duration::from_secs(3)), 0);
    state.notify.mute(Some(now + Duration::from_secs(1)));
    assert_eq!(idle_wakes(&mut state, Duration::from_secs(3)), 1);
    assert!(!state.notify.muted(now + Duration::from_secs(2)));
    assert!(state.notify.mute_deadline().is_none());
    assert!(state.clients.contains_key(&id));
}

/// G2: a clock put into a static status line starts ticking again, and
/// a pane change arms exactly one name check that does not re-arm.
#[test]
fn the_status_clock_follows_the_formats_and_names_arm_once() {
    let (mut state, _) = state_with_client();
    set_session_option(&mut state, "status-interval", "1");
    set_session_option(&mut state, "status-right", "x");
    assert_eq!(idle_wakes(&mut state, Duration::from_secs(3)), 0);
    set_session_option(&mut state, "status-right", "%H:%M");
    let now = Instant::now();
    tick(&mut state, now);
    assert!(deadline(&state).is_some_and(|d| d <= now + Duration::from_secs(1)));

    let (mut state, _) = state_with_client();
    set_session_option(&mut state, "status-right", "#{session_name}");
    let now = Instant::now();
    arm_names(&mut state, now);
    assert_eq!(deadline(&state), Some(now + NAME_INTERVAL));
    tick(&mut state, now + NAME_INTERVAL);
    assert_eq!(deadline(&state), None);
}

/// F1: tmux checks window names whether or not a client is attached.
#[test]
fn names_are_checked_with_no_client_attached() {
    let (mut state, _) = state_with_client();
    state.clients.clear();
    let now = Instant::now();
    arm_names(&mut state, now);
    assert_eq!(deadline(&state), Some(now + NAME_INTERVAL));
    // One check, and no timer after it: a detached server idles at 0 wakes.
    tick(&mut state, now + NAME_INTERVAL);
    assert_eq!(deadline(&state), None);
}

#[test]
fn only_formats_free_of_time_are_static() {
    for ok in [
        "",
        "plain",
        "[#{session_name}] ",
        "#I:#W#{?window_flags,#{window_flags}, }",
        "#[fg=red]#{=21:window_name}",
    ] {
        assert!(format_is_static(ok), "{ok}");
    }
    for bad in [
        "%H:%M",
        "100%",
        "#(date)",
        "#{t:session_created}",
        "#{@clock}",
        "#{pane_current_command}",
        "#{client_activity}",
        "#{S:#{window_name}}",
    ] {
        assert!(!format_is_static(bad), "{bad}");
    }
}

#[test]
fn a_key_press_takes_a_message_down_before_display_time() {
    let (mut state, id) = state_with_client();
    show_message(&mut state, id, "no last window");
    let shown = state.clients[&id].message.clone();
    let (text, until, level) = shown.expect("message shown");
    assert_eq!(level, render::Level::Error);
    let until = until.expect("display-time 750 expires");
    assert_eq!(text, "no last window");
    // A focus report is not a key: the message stays.
    let now = Instant::now();
    input(&mut state, id, b"\x1b[O", now);
    assert!(state.clients[&id].message.is_some());
    // A key press well before the message expires takes it down.
    assert!(now < until);
    input(&mut state, id, b"x", now);
    assert!(state.clients[&id].message.is_none());
    assert!(state.clients[&id].dirty);
}

/// As tmux: with `display-time 0` a message stays until a key is
/// pressed, however long that is.
#[test]
fn display_time_zero_keeps_a_message_until_a_key() {
    let (mut state, id) = state_with_client();
    let session = state.clients[&id].session;
    state
        .model
        .options
        .set(Owner::Session(session), "display-time", "0", false)
        .unwrap();
    show_message(&mut state, id, "held");
    assert_eq!(
        state.clients[&id].message,
        Some(("held".to_string(), None, render::Level::Error))
    );
    let later = Instant::now() + Duration::from_secs(3600);
    tick(&mut state, later);
    assert!(state.clients[&id].message.is_some());
    assert!(deadline(&state).is_none_or(|d| d > Instant::now() + DISPLAY_TIME));
    input(&mut state, id, b"x", later);
    assert!(state.clients[&id].message.is_none());
}

/// TC.9: every status-line message is logged for `show-messages`, as
/// tmux logs `<client> message: <text>`, and the log keeps tmux's
/// `message-limit` newest.
#[test]
fn messages_are_logged_and_capped() {
    let (mut state, id) = state_with_client();
    display_message(&mut state, id, "hi", None, false, render::Level::Info);
    show_message(&mut state, id, "an #[error]");
    let log: Vec<&str> = messages(&state).map(|(_, m)| m.as_str()).collect();
    assert_eq!(
        log,
        ["client-1 message: hi", "client-1 message: an #[error]"]
    );
    for i in 0..MESSAGE_LIMIT {
        display_message(
            &mut state,
            id,
            &i.to_string(),
            None,
            false,
            render::Level::Info,
        );
    }
    assert_eq!(messages(&state).count(), MESSAGE_LIMIT);
    assert_eq!(
        messages(&state).next().map(|(_, m)| m.as_str()),
        Some("client-1 message: 0")
    );
    // `message-limit` is the cap once it is set.
    state
        .model
        .options
        .set(Owner::Global, "message-limit", "3", false)
        .unwrap();
    display_message(&mut state, id, "last", None, false, render::Level::Info);
    assert_eq!(messages(&state).count(), 3);
    assert_eq!(
        messages(&state).next_back().map(|(_, m)| m.as_str()),
        Some("client-1 message: last")
    );
}

fn held_inputs(state: &State, id: ClientId) -> Vec<Input> {
    let held = &state.clients[&id].held;
    held.iter()
        .filter_map(|h| match h {
            Held::Input(input) => Some(input.clone()),
            Held::Run(_) => None,
        })
        .collect()
}

/// TC.10 fix round 1: a held client's input waits in order, holds
/// nest, and past the cap new input is dropped.
#[test]
fn held_input_waits_in_order_until_the_last_release() {
    let (mut state, id) = state_with_client();
    let now = Instant::now();
    hold_input(&mut state, id);
    hold_input(&mut state, id);
    input(&mut state, id, b"ab", now);
    let held = held_inputs(&state, id);
    assert_eq!(
        held,
        vec![
            Input::Key(crate::server::input::parse_key("a").unwrap()),
            Input::Key(crate::server::input::parse_key("b").unwrap()),
        ]
    );
    release_input(&mut state, id);
    assert_eq!(state.clients[&id].held.len(), 2, "one hold left");
    release_input(&mut state, id);
    assert!(state.clients[&id].held.is_empty());
    assert_eq!(state.clients[&id].held_bytes, 0);

    hold_input(&mut state, id);
    state.clients.get_mut(&id).unwrap().held_bytes = HELD_CAP;
    input(&mut state, id, b"c", now);
    assert!(state.clients[&id].held.is_empty(), "dropped past the cap");
}

/// Past the cap, everything that arrives is dropped until the hold is
/// released, not only the input that did not fit: a later small key
/// would otherwise run with a gap before it.
#[test]
fn past_the_cap_held_input_is_dropped_until_release() {
    let (mut state, id) = state_with_client();
    let now = Instant::now();
    let key = |k: &str| Input::Key(crate::server::input::parse_key(k).unwrap());
    hold_input(&mut state, id);
    deliver(&mut state, id, key("a"), now);
    state.clients.get_mut(&id).unwrap().held_bytes = HELD_CAP - 1;
    let big = Input::Paste {
        data: vec![b'x'; 2],
        start: true,
        end: true,
    };
    deliver(&mut state, id, big, now);
    deliver(&mut state, id, key("b"), now);
    assert_eq!(
        held_inputs(&state, id),
        vec![key("a")],
        "b came after a dropped paste"
    );
    release_input(&mut state, id);
    assert!(state.clients[&id].held.is_empty());
    // A new hold holds again.
    hold_input(&mut state, id);
    deliver(&mut state, id, key("c"), now);
    assert_eq!(state.clients[&id].held.len(), 1);
}

/// A job queued in input order (the click timer's `DoubleClick`) waits
/// behind the input a hold kept, as tmux appends it to the client's
/// queue, and runs at once when nothing is held.
#[test]
fn a_job_in_input_order_runs_after_the_held_input() {
    let (mut state, id) = state_with_client();
    let now = Instant::now();
    let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
    let job = |ran: &Arc<std::sync::Mutex<Vec<usize>>>| -> crate::server::Job {
        let ran = Arc::clone(ran);
        Box::new(move |state: &mut State| {
            let held = state.clients[&id].held.len();
            ran.lock().unwrap().push(held);
        })
    };
    hold_input(&mut state, id);
    input(&mut state, id, b"ab", now);
    in_input_order(&mut state, id, job(&ran));
    assert!(ran.lock().unwrap().is_empty(), "held behind a and b");
    release_input(&mut state, id);
    assert_eq!(*ran.lock().unwrap(), vec![0], "ran after both keys");
    in_input_order(&mut state, id, job(&ran));
    assert_eq!(*ran.lock().unwrap(), vec![0, 0]);
}

/// TM.1 (`keys_through_a_client`): a held key replayed on release that
/// opens a prompt holds the client again, and the keys held behind it
/// go to that prompt, as when they arrive after it opened. They were
/// left held behind the prompt, which waited for them: copy mode's `2`
/// then `{`, read in one go, lost the `{`.
#[tokio::test]
async fn held_keys_behind_a_replayed_prompt_go_to_it() {
    let (mut state, id) = state_with_client();
    crate::server::register_input_hook(&mut state, Arc::new(crate::server::prompt::PromptHook));
    let ctx = crate::server::test_ctx();
    let run = |state: &mut State, words: &[&str]| {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        crate::server::dispatch::run(state, &ctx, &argv).expect("run")
    };
    run(&mut state, &["new-session", "-d", "-s", "a"]);
    let session = *state.model.sessions.keys().next().expect("a session");
    state.clients.get_mut(&id).expect("client").session = session;
    run(
        &mut state,
        &[
            "bind-key",
            "-n",
            "a",
            "command-prompt -I x { set -g @got '%%' }",
        ],
    );
    hold_input(&mut state, id);
    input(&mut state, id, b"ab\r", Instant::now());
    assert_eq!(state.clients[&id].held.len(), 3, "all held");
    release_input(&mut state, id);
    assert_eq!(state.model.options.lookup("@got", &[]), Some("xb"));
    assert!(!state.prompts.is_open(id));
}

/// tmux 3.7b (recorded): a binding keeps the alias expansion made when
/// it was bound, and a key press does not expand it again. With
/// `a=display-message` and `display-message=set @hit twice`, the key
/// bound to `a` runs `display-message`.
#[tokio::test]
async fn a_bound_alias_chain_expands_once() {
    let (mut state, id) = state_with_client();
    let ctx = crate::server::test_ctx();
    let run = |state: &mut State, words: &[&str]| {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        crate::server::dispatch::run(state, &ctx, &argv).expect("run")
    };
    run(&mut state, &["new-session", "-d", "-s", "a"]);
    let session = *state.model.sessions.keys().next().expect("a session");
    state.clients.get_mut(&id).expect("client").session = session;
    run(
        &mut state,
        &["set", "-ag", "command-alias", "a=display-message"],
    );
    run(
        &mut state,
        &[
            "set",
            "-ag",
            "command-alias",
            "display-message=set -g @hit twice",
        ],
    );
    run(&mut state, &["bind-key", "-n", "F5", "a"]);
    run(&mut state, &["bind-key", "-n", "F6", "set -g @pressed yes"]);
    input(&mut state, id, b"\x1b[15~\x1b[17~", Instant::now());
    assert_eq!(state.model.options.lookup("@pressed", &[]), Some("yes"));
    let hit = state.model.options.lookup("@hit", &[]);
    assert_eq!(hit, None, "expanded a second time");
}

/// TC.11: a click maps onto the status row it lands on, at the bottom
/// by default and at the top with `status-position top`.
#[test]
fn a_status_click_finds_the_range_under_it() {
    let (mut state, id) = state_with_client();
    let net = status::draw(
        "#[range=user|net]NET#[norange]",
        status::Style::default(),
        80,
    );
    let user = Some(RangeKind::User("net".to_string()));
    let c = state.clients.get_mut(&id).unwrap();
    c.status_rows = vec![net.clone()];
    c.status_at = 23;
    assert_eq!(status_range_at(&state, id, 1, 23), user);
    assert_eq!(status_range_at(&state, id, 1, 22), None);
    assert_eq!(status_range_at(&state, id, 40, 23), None);

    let o = &mut state.model.options;
    o.set(Owner::Global, "status", "2", false).unwrap();
    o.set(Owner::Global, "status-position", "top", false)
        .unwrap();
    // TC.10: until the next draw, clicks still map onto the rows as
    // they were drawn, not as the options now place them.
    assert_eq!(status_range_at(&state, id, 1, 23), user);
    assert_eq!(status_at(&state, id, 1, 0), None);
    let c = state.clients.get_mut(&id).unwrap();
    c.status_rows = vec![StatusRow::default(), net];
    c.status_at = 0;
    assert_eq!(status_range_at(&state, id, 1, 1), user);
    assert_eq!(status_range_at(&state, id, 1, 0), None);
    assert_eq!(status_at(&state, id, 1, 0), Some((0, None)));
    assert_eq!(status_range_at(&state, id, 1, 23), None);
    assert_eq!(status_at(&state, id, 1, 2), None);
}

/// The hooks and overlays (prompt, menu, popup, chooser, mouse, copy
/// mode) are registered in priority order, and with
/// nothing open every hook passes every kind of input on and every
/// overlay leaves the screen alone.
#[test]
fn the_builtin_hooks_and_overlays_are_inert() {
    let (mut state, id) = state_with_client();
    crate::server::register_builtins(&mut state);
    assert_eq!(state.registry.input_hooks.len(), 13);
    // TM.6 removed the mouse's scrollback view overlay.
    assert_eq!(state.registry.overlays.len(), 12);
    let now = Instant::now();
    let inputs = [
        Input::Key(crate::server::input::parse_key("C-b").expect("key")),
        Input::Paste {
            data: b"x".to_vec(),
            start: true,
            end: true,
        },
        Input::Focus(false),
        Input::Mouse(crate::server::input::Mouse {
            kind: crate::server::input::MouseKind::WheelUp,
            col: 3,
            row: 4,
            mods: Default::default(),
        }),
    ];
    for hook in state.registry.input_hooks.clone() {
        for input in &inputs {
            assert_eq!(hook.on_input(&mut state, id, input, now), None, "{input:?}");
        }
    }
    let mut screen = screen_with('a');
    let before = screen.clone();
    let cx = overlay_ctx(&state, id).expect("ctx");
    for overlay in state.registry.overlays.iter().rev() {
        overlay.draw(&state, id, &cx, &mut screen);
    }
    assert_eq!(screen, before);
}

/// Item 13: the config-load hook changes nothing until T3.5 fills it.
#[test]
fn apply_config_at_start_is_a_no_op() {
    let (mut state, _) = state_with_client();
    let options = |s: &State| {
        s.model
            .options
            .at(Owner::Global)
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect::<Vec<_>>()
    };
    let (before, keys) = (options(&state), format!("{:?}", state.registry.key_tables));
    crate::server::dispatch::options::apply_config_at_start(&mut state);
    assert_eq!(options(&state), before);
    assert_eq!(format!("{:?}", state.registry.key_tables), keys);
}

/// Item 11: raw bytes go out ahead of the next frame, in the same
/// frame; with no screen change they go out alone; a dropped frame
/// keeps them for the next one.
#[test]
fn raw_bytes_lead_the_next_frame_and_survive_a_drop() {
    let caps = Caps::default();
    let (mut outbox, mut rx) = Outbox::new();
    outbox.queue_raw(b"RAW1");
    assert!(outbox.offer(screen_with('a'), &caps));
    let first = rx.try_recv().expect("first frame");
    assert!(first.starts_with(b"RAW1\x1b[r"), "{first:?}");
    // Nothing changed on screen: the raw bytes alone.
    outbox.queue_raw(b"RAW2");
    assert!(outbox.offer(screen_with('a'), &caps));
    assert_eq!(rx.try_recv().expect("raw only"), b"RAW2");
    // The slot is full: the frame is dropped, the raw bytes are not.
    assert!(outbox.offer(screen_with('b'), &caps));
    outbox.queue_raw(b"RAW3");
    assert_eq!(outbox.discarded, 0);
    assert!(!outbox.offer(screen_with('c'), &caps));
    assert!(outbox.needs_full_redraw());
    assert!(outbox.discarded > 0, "the dropped diff is counted");
    let _ = rx.try_recv().expect("the 'b' frame");
    assert!(outbox.offer(screen_with('c'), &caps));
    let next = rx.try_recv().expect("redraw");
    assert!(next.starts_with(b"RAW3\x1b[r"), "{next:?}");
}

/// Item 5 (R2): mouse modes follow the client's session's `mouse`
/// option, written once per change, ahead of the frame.
#[test]
fn mouse_modes_follow_the_option() {
    let (mut state, id) = state_with_client();
    let raw = |s: &mut State| std::mem::take(&mut s.clients.get_mut(&id).expect("c").outbox.raw);
    sync_mouse_modes(&mut state);
    assert_eq!(raw(&mut state), MOUSE_ON, "grofe's default is on");
    sync_mouse_modes(&mut state);
    assert_eq!(raw(&mut state), b"", "unchanged: nothing written");
    state
        .model
        .options
        .set(Owner::Global, "mouse", "off", false)
        .expect("set");
    sync_mouse_modes(&mut state);
    assert_eq!(raw(&mut state), MOUSE_OFF);
}

/// R28: focus reports update the client's focus flag either way, but
/// reach hooks and panes only while `focus-events` is on.
#[test]
fn focus_events_off_stops_focus_reports() {
    struct Seen;
    impl InputHook for Seen {
        fn on_input(&self, s: &mut State, _: ClientId, i: &Input, _: Instant) -> Option<Routed> {
            if let Input::Focus(_) = i {
                s.exit = true;
            }
            None
        }
    }
    let (mut state, id) = state_with_client();
    state.registry.input_hooks.push(Arc::new(Seen));
    state
        .model
        .options
        .set(Owner::Global, "focus-events", "off", false)
        .expect("set");
    route(&mut state, id, Input::Focus(false), Instant::now());
    assert!(!state.clients[&id].focused);
    assert!(!state.exit, "no hook saw it");
    state
        .model
        .options
        .set(Owner::Global, "focus-events", "on", false)
        .expect("set");
    route(&mut state, id, Input::Focus(true), Instant::now());
    assert!(state.clients[&id].focused);
    assert!(state.exit, "a hook saw it");
}

/// Fix round 1: every pane's graphics flag follows whether an
/// attached client shows kitty graphics (as `GROFE_TEST_CAPS` makes a
/// client claim it in its hello): on when one attaches, set on a pane
/// spawned meanwhile, off once it leaves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graphics_ok_follows_the_attached_clients() {
    let (mut state, id) = state_with_client();
    let ctx = ClientCtx {
        cwd: std::env::temp_dir(),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: crate::protocol::Dialect::Grofe,
        request: Default::default(),
    };
    let run = |state: &mut State, words: &[&str]| {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        dispatch::run(state, &ctx, &argv).expect("command");
    };
    let flags = |state: &State| {
        state
            .model
            .panes
            .values()
            .map(|p| p.handle.graphics_ok())
            .collect::<Vec<_>>()
    };
    run(&mut state, &["new-session", "-d", "-s", "g", "sleep 30"]);
    changed(&mut state);
    assert_eq!(flags(&state), [false], "a client without kitty graphics");
    if let Some(c) = state.clients.get_mut(&id) {
        c.caps.kitty_graphics = true;
    }
    changed(&mut state);
    assert_eq!(flags(&state), [true]);
    run(&mut state, &["split-window", "-t", "g", "sleep 30"]);
    assert_eq!(flags(&state), [true, true], "a new pane starts on");
    gone(&mut state, id);
    assert_eq!(flags(&state), [false, false], "the client left");
}

/// The sidebar shown on `id` (width 32, so 33 columns), as `agent-sidebar on`.
fn show_sidebar(state: &mut State, id: ClientId) {
    crate::server::sidebar::set(state, id, crate::server::sidebar::SidebarOp::On)
        .expect("sidebar on");
}

/// A state with a real session and one client of `size` attached to it
/// (`sidebar::set` resizes through [`changed`], which drops a client
/// whose session is gone).
fn sidebar_state(size: (u16, u16)) -> (State, ClientId, SessionId) {
    let (mut state, _) = pane_with_agents();
    let session = *state.model.sessions.keys().next().expect("session");
    let id = ClientId(1);
    let mut entry = state_with_client().0.clients.remove(&id).expect("client");
    entry.session = session;
    entry.size = size;
    state.clients.insert(id, entry);
    (state, id, session)
}

/// Agents spec §3.1 and §5.1: with agent records (one blocked and
/// unseen), a read notification whose toast has expired and the sidebar
/// shown, but no picker, grid, live toast or timed mute, nothing arms a
/// timer, so a static status line keeps the server asleep.
#[tokio::test]
async fn agents_with_no_timed_surface_never_wake() {
    use crate::server::agents::{AgentState, Report, ReportState};
    let (mut state, id, _) = sidebar_state((160, 50));
    let pane = *state.model.panes.keys().next().expect("pane");
    let report = Report {
        state: ReportState::Set(AgentState::Blocked),
        kind: Some("claude".into()),
        msg: Some("perm".into()),
        blocked: None,
        session_id: None,
        ts: 1,
    };
    state.agents.apply(pane, report, false, Instant::now());
    let long_ago = Instant::now() - Duration::from_secs(60);
    state.notify.on_change(
        pane,
        "u1",
        Some("claude"),
        Some("working"),
        Some("blocked"),
        Some("perm"),
        Duration::from_secs(60),
        false,
        true,
        long_ago,
    );
    show_sidebar(&mut state, id);
    set_session_option(&mut state, "status-interval", "1");
    set_session_option(&mut state, "status-right", "#{agents_badge}");
    // The one `automatic-rename` check making the panes armed (not an
    // agent timer) runs first.
    idle_wakes(&mut state, NAME_INTERVAL * 2);
    assert!(crate::server::sidebar::reserved(&state, id, 160) > 0);
    assert_eq!(crate::server::picker::deadline(&state), None);
    assert_eq!(crate::server::grid::deadline(&state), None);
    assert_eq!(crate::server::toast::deadline(&state, id), None);
    assert_eq!(state.notify.mute_deadline(), None);
    assert_eq!(idle_wakes(&mut state, Duration::from_secs(3)), 0);
}

/// The scan throttle is kept per pane: one pane's scan does not hold
/// back another's.
#[tokio::test]
async fn scan_throttle_is_per_pane() {
    let (mut state, a) = pane_with_agents();
    crate::server::dispatch::run(
        &mut state,
        &crate::server::test_ctx(),
        &["new-window".into(), "-t".into(), "m".into()],
    )
    .expect("new-window");
    let b = *state.model.panes.keys().find(|p| **p != a).expect("pane b");
    let reads = std::cell::RefCell::new(Vec::new());
    let mut read = |p: &crate::server::model::Pane| {
        reads.borrow_mut().push(p.id);
        None
    };
    // Spawning scanned both; start from no scan.
    state.registry.forget_scan(a);
    state.registry.forget_scan(b);
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    scan_agents_with(&mut state, &BTreeSet::from([a]), t0, &mut read);
    scan_agents_with(&mut state, &BTreeSet::from([b]), t0 + ms(500), &mut read);
    scan_agents_with(
        &mut state,
        &BTreeSet::from([a, b]),
        t0 + ms(1100),
        &mut read,
    );
    assert_eq!(*reads.borrow(), [a, b, a]);
}

#[tokio::test]
async fn window_shrinks_by_reserved() {
    let (mut state, id, session) = sidebar_state((160, 50));
    let status = status_lines(&state.model, session);
    show_sidebar(&mut state, id);
    let reserved = crate::server::sidebar::reserved(&state, id, 160);
    assert_eq!(reserved, 33);
    assert_eq!(window_size(&state, id, session), (127, 50 - status));
    let c = state.clients.get(&id).expect("client");
    assert_eq!(visible_size(&state, id, c), (127, 50 - status));
}

#[tokio::test]
async fn no_sidebar_sizes_unchanged() {
    let (state, id, session) = sidebar_state((160, 50));
    let status = status_lines(&state.model, session);
    assert_eq!(window_size(&state, id, session), (160, 50 - status));
    let c = state.clients.get(&id).expect("client");
    assert_eq!(visible_size(&state, id, c), (160, 50 - status));
}

#[tokio::test]
async fn other_client_cropped() {
    let (mut state, id, _session) = sidebar_state((160, 50));
    show_sidebar(&mut state, id);
    // A client the sidebar is not shown on keeps its whole width.
    let c = state.clients.get(&id).expect("client");
    let other = ClientId(2);
    assert_eq!(visible_size(&state, other, c).0, 160);
    assert_eq!(visible_size(&state, id, c).0, 127);
}

#[tokio::test]
async fn toggle_off_restores() {
    let (mut state, id, session) = sidebar_state((160, 50));
    let status = status_lines(&state.model, session);
    show_sidebar(&mut state, id);
    assert_eq!(window_size(&state, id, session).0, 127);
    crate::server::sidebar::set(&mut state, id, crate::server::sidebar::SidebarOp::Off)
        .expect("sidebar off");
    assert_eq!(window_size(&state, id, session), (160, 50 - status));
}

#[test]
fn widening_keeps_cells_and_leaves_blank_columns() {
    let mut s = Screen::new(2, 2);
    s.cells[2].c = 'x';
    let w = widen(s, 4);
    assert_eq!((w.cols, w.rows), (4, 2));
    assert_eq!(w.cells[4].c, 'x');
    assert_eq!(w.cells[2].c, ' ');
}

#[test]
fn a_new_client_takes_the_size_only_after_the_debounce() {
    let (a, b) = (ClientId(1), ClientId(2));
    let t0 = Instant::now();
    let mut owner = SizeOwner::new(a);
    owner.activity(b, t0);
    assert_eq!(owner.deadline(), Some(t0 + SIZE_DEBOUNCE));
    // Typing on does not push the switch back.
    owner.activity(b, t0 + Duration::from_millis(300));
    assert_eq!(owner.deadline(), Some(t0 + SIZE_DEBOUNCE));
    assert!(!owner.settle(t0 + Duration::from_millis(499)));
    assert_eq!(owner.client, a);
    assert!(owner.settle(t0 + SIZE_DEBOUNCE));
    assert_eq!(owner.client, b);
    assert_eq!(owner.deadline(), None);
}

#[test]
fn the_owner_typing_cancels_a_pending_switch() {
    let (a, b) = (ClientId(1), ClientId(2));
    let t0 = Instant::now();
    let mut owner = SizeOwner::new(a);
    owner.activity(b, t0);
    owner.activity(a, t0 + Duration::from_millis(100));
    assert_eq!(owner.deadline(), None);
    assert!(!owner.settle(t0 + Duration::from_secs(1)));
    assert_eq!(owner.client, a);
}

#[test]
fn viewport_is_zero_when_the_window_fits() {
    assert_eq!(follow((5, 5), Some((70, 20)), (80, 23), (80, 23)), (0, 0));
    assert_eq!(follow((5, 5), Some((10, 5)), (40, 11), (80, 23)), (0, 0));
}

#[test]
fn viewport_moves_just_enough_to_show_the_cursor() {
    // Cursor right and below the visible area.
    assert_eq!(follow((0, 0), Some((64, 19)), (80, 23), (40, 11)), (25, 9));
    // Inside already: no movement.
    assert_eq!(follow((25, 9), Some((30, 12)), (80, 23), (40, 11)), (25, 9));
    // Left of and above it: the viewport starts at the cursor.
    assert_eq!(follow((25, 9), Some((3, 2)), (80, 23), (40, 11)), (3, 2));
    // Never past the window's far edge, even after a shrink.
    assert_eq!(follow((70, 20), None, (80, 23), (40, 11)), (40, 12));
}

fn screen_with(c: char) -> Screen {
    let mut s = Screen::new(4, 2);
    s.cells[0].c = c;
    s
}

#[test]
fn a_full_outbox_drops_the_diff_and_redraws_in_full_once_free() {
    let caps = Caps::default();
    let (mut outbox, mut rx) = Outbox::new();
    assert!(outbox.offer(screen_with('a'), &caps));
    assert!(outbox.ready());
    // The writer has not taken the first frame: the second is dropped.
    assert!(!outbox.offer(screen_with('b'), &caps));
    assert!(!outbox.ready());
    assert!(outbox.prev.is_none());
    let first = rx.try_recv().expect("first frame");
    assert!(
        first.windows(4).any(|w| w == b"\x1b[2J"),
        "full redraw first"
    );
    assert!(outbox.ready());
    // The next offer is a full redraw, not a diff against 'a'.
    assert!(outbox.offer(screen_with('b'), &caps));
    let next = rx.try_recv().expect("redraw");
    assert!(
        next.windows(4).any(|w| w == b"\x1b[2J"),
        "full redraw after a drop"
    );
    assert!(rx.try_recv().is_err());
    // Unchanged: nothing is sent.
    assert!(outbox.offer(screen_with('b'), &caps));
    assert!(rx.try_recv().is_err());
}

/// A removed client takes its sidebar and grid state with it.
#[tokio::test]
async fn remove_drops_sidebar_and_grid_state() {
    let (mut state, id, _session) = sidebar_state((160, 50));
    show_sidebar(&mut state, id);
    crate::server::grid::open(&mut state, id).expect("grid");
    assert!(state.grids.is_open(id));
    remove(&mut state, id, None);
    assert!(!state.grids.is_open(id));
    assert_eq!(crate::server::grid::deadline(&state), None);
    assert!(!crate::server::sidebar::is_focused(&state, id));
    assert!(!state.sidebars.has(id));
}

#[test]
fn a_pill_takes_the_themes_colours_and_ends_else_named_and_round() {
    let mut model = Model::default();
    let chain = [Owner::Global];
    let look = pill_look(&model, &chain, render::Level::Info).unwrap();
    assert_eq!(
        (look.colour.as_str(), look.text.as_str()),
        ("blue", "brightwhite")
    );
    assert_eq!(look.ends, "\u{e0b6}\u{e0b4}");
    let o = &mut model.options;
    for (k, v) in [
        ("@theme_blue", "#4d699b"),
        ("@theme_red", "#cc241d"),
        ("@theme_bg", "#f2ecbc"),
        ("@pill_l", "<"),
        ("@pill_r", ">"),
    ] {
        o.set(Owner::Global, k, v, false).unwrap();
    }
    let look = pill_look(&model, &chain, render::Level::Info).unwrap();
    assert_eq!(
        (look.colour.as_str(), look.text.as_str(), look.ends.as_str()),
        ("#4d699b", "#f2ecbc", "<>")
    );
    let err = pill_look(&model, &chain, render::Level::Error).unwrap();
    assert_eq!(err.colour, "#cc241d");
    // No @theme_yellow: a warning keeps the named colour.
    let warn = pill_look(&model, &chain, render::Level::Warning).unwrap();
    assert_eq!(warn.colour, "yellow");
    // `message-pill-ends` wins over the theme; `message-pill off` is none.
    let set = |m: &mut Model, k, v| m.options.set(Owner::Global, k, v, false).unwrap();
    set(&mut model, "message-pill-ends", "()");
    assert_eq!(
        pill_look(&model, &chain, render::Level::Info).unwrap().ends,
        "()"
    );
    set(&mut model, "message-pill", "off");
    assert!(pill_look(&model, &chain, render::Level::Info).is_none());
}

#[test]
fn a_pill_spans_up_to_status_right() {
    let row = |ranges: Vec<crate::server::status::StatusRange>| StatusRow {
        line: StyledLine::default(),
        ranges,
    };
    let right = |start, end| crate::server::status::StatusRange {
        kind: RangeKind::Right,
        start,
        end,
    };
    assert_eq!(pill_span(&[row(vec![right(60, 80)])], 0, 80), 60);
    // No status-right, or no such line: the whole row.
    assert_eq!(pill_span(&[row(vec![right(80, 80)])], 0, 80), 80);
    assert_eq!(pill_span(&[], 0, 80), 80);
}
