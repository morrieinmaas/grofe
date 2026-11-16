use std::collections::BTreeSet;

use super::*;
use crate::protocol::Dialect;
use crate::server::model::{BASE_INDEX, HISTORY_LIMIT, PANE_BASE_INDEX};

/// `source-file` globs only the path it is given: the requester's cwd
/// is quoted first, as tmux's `cmd_source_file_quote_for_glob`, so a
/// cwd named `[ab]` reads its own file, not `a/` or `b/`'s.
#[test]
fn source_file_takes_the_cwd_literally() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("[ab]*");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(dir.path().join("a")).unwrap();
    std::fs::write(cwd.join("f.conf"), "set -g @where cwd\n").unwrap();
    std::fs::write(dir.path().join("a/f.conf"), "set -g @where glob\n").unwrap();
    let mut state = crate::server::test_state();
    let ctx = ClientCtx {
        cwd,
        dialect: Dialect::Tmux,
        ..crate::server::test_ctx()
    };
    let mut run = |words: &[&str]| {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        crate::server::dispatch::run(&mut state, &ctx, &argv)
    };
    assert_eq!(run(&["source-file", "f.conf"]), Ok(String::new()));
    assert_eq!(run(&["show-options", "-gv", "@where"]), Ok("cwd\n".into()));
    assert_eq!(
        run(&["source-file", "f*.conf"]).map(|_| ()),
        Ok(()),
        "the given path is still a glob"
    );
}

/// FR.2: a global set at runtime or by the config counts as set, even
/// to the default value; an untouched default does not.
#[test]
fn set_globally_tells_a_user_set_from_the_default() {
    let mut state = crate::server::test_state();
    let ctx = crate::server::test_ctx();
    let mut run = |words: &[&str]| {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        crate::server::dispatch::run(&mut state, &ctx, &argv)
    };
    run(&["set", "-g", "pane-border-lines", "single"]).unwrap();
    let options = &state.model.options;
    assert!(options.set_globally("pane-border-lines"));
    assert!(!options.set_globally("pane-border-style"));
    let mut fresh = Options::new(false);
    fresh.use_tmux_defaults(None);
    assert!(!fresh.set_globally("pane-border-lines"));
    fresh
        .config_base
        .insert("pane-border-lines".into(), "single".into());
    assert!(fresh.set_globally("pane-border-lines"));
}

#[test]
fn scratchpad_history_range_is_0_to_100() {
    let mut o = Options::new(false);
    for ok in ["0", "100"] {
        assert!(
            o.set(Owner::Global, "scratchpad-history", ok, false)
                .is_ok(),
            "{ok}"
        );
        assert_eq!(o.lookup("scratchpad-history", &[]), Some(ok));
    }
    for bad in ["-1", "101", "x"] {
        assert!(
            o.set(Owner::Global, "scratchpad-history", bad, false)
                .is_err(),
            "{bad}"
        );
    }
    assert_eq!(o.lookup("scratchpad-history", &[]), Some("100"));
}

#[test]
fn scratchpad_history_default_is_ten() {
    assert_eq!(
        Options::new(false).lookup("scratchpad-history", &[]),
        Some("10")
    );
}

/// TM.7: mouse keys sort as tmux 3.7b's key codes do (recorded in
/// `tc_0b/display-formats` and `tm_0/list-keys-copy-mode-vi`).
#[test]
fn mouse_keys_sort_in_tmux_s_order() {
    use crate::server::input::parse_key;
    let recorded = [
        "MouseDown1Pane",
        "MouseDown1Status",
        "MouseDown1Border",
        "MouseDown1ScrollbarUp",
        "MouseDown1Control8",
        "MouseDown3Pane",
        "MouseDown3StatusLeft",
        "MouseDrag1Pane",
        "MouseDrag1Border",
        "MouseDragEnd1Pane",
        "WheelDownPane",
        "WheelDownStatus",
        "WheelUpPane",
        "DoubleClick1Pane",
        "TripleClick1Pane",
        "M-x",
        "M-MouseDown3Pane",
        "C-a",
    ];
    let mut keys: Vec<Key> = recorded.iter().map(|k| parse_key(k).unwrap()).collect();
    keys.reverse();
    keys.sort_by_key(tmux_key_order);
    let got: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    assert_eq!(got, recorded);
}

/// TC.7: `list-keys` for one key, `-1` for the first line, `-N` for
/// notes in tmux 3.7b's form (recorded: `C-b c       Create a new
/// window`, keys padded to the widest; without `-T` the prefix table,
/// then the root table, every line after the prefix key).
#[test]
fn list_keys_one_key_first_and_notes() {
    let mut state = crate::server::test_state();
    let ctx = crate::server::test_ctx();
    let mut run = |words: &[&str]| {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        crate::server::dispatch::run(&mut state, &ctx, &argv)
    };
    // The stock keys carry tmux's notes too (TC.10); start without them.
    run(&["unbind-key", "-a", "-T", "prefix"]).expect("unbind");
    run(&["bind-key", "-N", "Kill the active pane", "x", "kill-pane"]).expect("bind");
    run(&["bind-key", "-N", "Next layout", "Space", "next-layout"]).expect("bind");
    run(&["bind-key", "y", "list-sessions"]).expect("bind");
    run(&["bind-key", "-n", "-N", "Root note", "F6", "list-sessions"]).expect("bind");

    let got = run(&["list-keys", "-T", "prefix", "x"]).expect("list-keys x");
    assert_eq!(got.lines().count(), 1, "{got}");
    assert!(got.starts_with("bind-key -T prefix x kill-pane"), "{got}");
    assert_eq!(
        run(&["list-keys", "-T", "prefix", "F12"]),
        Ok(String::new())
    );
    let first = run(&["list-keys", "-1", "-T", "prefix"]).expect("-1");
    assert_eq!(first.lines().count(), 1, "{first}");

    assert_eq!(
        run(&["list-keys", "-N"]),
        Ok(
            "C-b Space Next layout\nC-b x     Kill the active pane\nC-b F6    Root note\n"
                .to_string()
        )
    );
    assert_eq!(
        run(&["list-keys", "-N", "-T", "root"]),
        Ok("F6 Root note\n".to_string())
    );
    assert_eq!(
        run(&["list-keys", "-N", "x"]),
        Ok("C-b x Kill the active pane\n".to_string())
    );
    // No attached client: `-1N` prints its line.
    assert_eq!(
        run(&["list-keys", "-1N", "x"]),
        Ok("C-b x Kill the active pane\n".to_string())
    );
    assert_eq!(run(&["list-keys", "-1N", "y"]), Ok(String::new()));
    assert_eq!(
        run(&["list-keys", "-1N", "NotAKey"]),
        Err("invalid key: NotAKey".to_string())
    );
}

/// `set -F` expands with the server's variables, as `display-message`
/// does: a mouse key's command sees the mouse event (TC.10).
#[tokio::test]
async fn set_f_sees_the_mouse_variables() {
    let mut state = crate::server::test_state();
    let ctx = crate::server::test_ctx();
    let run = |state: &mut State, words: &[&str]| {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        crate::server::dispatch::run(state, &ctx, &argv)
    };
    run(&mut state, &["new-session", "-d", "-s", "a"]).expect("new-session");
    let pane = *state.model.panes.keys().next().expect("a pane");
    let target = crate::server::mouse::MouseTarget {
        client: crate::types::ClientId(1),
        cell: (3, 4),
        pane,
        target: pane.to_string(),
        range: None,
        pane_cell: Some((3, 4)),
        report: (3, 4),
        wheel: false,
        action: None,
        event: crate::server::input::Mouse {
            kind: crate::server::input::MouseKind::Move,
            col: 3,
            row: 4,
            mods: Default::default(),
        },
    };
    crate::server::mouse::MouseState::with_current(&mut state, Some(target), |state| {
        run(state, &["set", "-gF", "@m", "#{mouse_pane} #{mouse_x}"])
    })
    .expect("set -F");
    assert_eq!(
        run(&mut state, &["show-options", "-gv", "@m"]),
        Ok(format!("{pane} 3\n"))
    );
}

/// A server with two windows in one session; returns their panes.
fn two_window_state() -> (State, PaneId, PaneId) {
    let mut state = crate::server::test_state();
    let ctx = crate::server::test_ctx();
    for words in [
        &["new-session", "-d", "-s", "a"][..],
        &["new-window", "-d"][..],
    ] {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        crate::server::dispatch::run(&mut state, &ctx, &argv).expect("setup");
    }
    let mut panes: Vec<PaneId> = state.model.panes.keys().copied().collect();
    panes.sort();
    (state, panes[0], panes[1])
}

fn report(s: crate::server::agents::AgentState, ts: u64) -> crate::server::agents::Report {
    crate::server::agents::Report {
        state: crate::server::agents::ReportState::Set(s),
        kind: Some("claude".into()),
        msg: Some("hi".into()),
        blocked: None,
        session_id: None,
        ts,
    }
}

fn var(state: &State, pane: PaneId, name: &str) -> Option<String> {
    let ctx = FormatCtx::new(&state.model, pane);
    ServerVars::new(state, pane, &ctx).get(name)
}

#[tokio::test]
async fn no_record_empty() {
    let (state, a, _) = two_window_state();
    for name in [
        "agent_kind",
        "agent_state",
        "agent_msg",
        "agent_blocked_kind",
        "agent_since",
        "window_agent_state",
        "session_agent_state",
    ] {
        assert_eq!(var(&state, a, name), Some(String::new()), "{name}");
    }
    assert_eq!(var(&state, a, "agents_total"), Some("0".into()));
}

#[tokio::test]
async fn window_rollup_unseen() {
    use crate::server::agents::AgentState;
    let (mut state, a, b) = two_window_state();
    state.agents.apply(
        a,
        report(AgentState::Done, 1),
        false,
        std::time::Instant::now(),
    );
    assert_eq!(var(&state, a, "agent_state"), Some("done".into()));
    assert_eq!(var(&state, a, "agent_unseen"), Some("1".into()));
    assert_eq!(var(&state, a, "window_agent_state"), Some("unseen".into()));
    assert_eq!(var(&state, b, "window_agent_state"), Some(String::new()));
    assert_eq!(var(&state, b, "session_agent_state"), Some("unseen".into()));
}

#[tokio::test]
async fn counts() {
    use crate::server::agents::AgentState;
    let (mut state, a, b) = two_window_state();
    let now = std::time::Instant::now();
    state
        .agents
        .apply(a, report(AgentState::Blocked, 1), true, now);
    state
        .agents
        .apply(b, report(AgentState::Working, 1), true, now);
    let get = |n: &str| var(&state, a, n);
    assert_eq!(get("agents_total"), Some("2".into()));
    assert_eq!(get("agents_blocked"), Some("1".into()));
    assert_eq!(get("agents_working"), Some("1".into()));
    assert_eq!(get("agents_attention"), Some("1".into()));
    assert_eq!(get("agent_attention"), Some("1".into()));
    assert_eq!(get("session_agent_state"), Some("blocked".into()));
}

fn blocked_error_unseen(state: &mut State, a: PaneId, b: PaneId) {
    use crate::server::agents::AgentState;
    let now = std::time::Instant::now();
    state
        .agents
        .apply(a, report(AgentState::Blocked, 1), true, now);
    state
        .agents
        .apply(b, report(AgentState::Done, 1), false, now);
}

#[tokio::test]
async fn badge_empty_when_quiet() {
    let (state, a, _) = two_window_state();
    assert_eq!(var(&state, a, "agents_badge"), Some(String::new()));
    assert_eq!(var(&state, a, "notifications_unread"), Some("0".into()));
}

#[tokio::test]
async fn badge_counts_and_glyphs() {
    let (mut state, a, b) = two_window_state();
    blocked_error_unseen(&mut state, a, b);
    assert_eq!(var(&state, a, "agents_badge"), Some("⏸1 ✓1".into()));
}

#[tokio::test]
async fn badge_muted_glyph() {
    let (mut state, a, b) = two_window_state();
    state.notify.mute(None);
    assert_eq!(var(&state, a, "agents_badge"), Some("🔕".into()));
    blocked_error_unseen(&mut state, a, b);
    assert_eq!(var(&state, a, "agents_badge"), Some("⏸1 ✓1 🔕".into()));
}

#[tokio::test]
async fn default_window_format_shows_glyph() {
    let (mut state, a, b) = two_window_state();
    blocked_error_unseen(&mut state, a, b);
    let fmt = state
        .model
        .options
        .lookup("window-status-format", &[])
        .unwrap()
        .to_string();
    let out = |pane| {
        let ctx = FormatCtx::new(&state.model, pane);
        expand(&fmt, &ServerVars::new(&state, pane, &ctx))
    };
    assert!(out(a).ends_with('⏸'), "{}", out(a));
    assert!(out(b).ends_with('✓'), "{}", out(b));
}

#[test]
fn tmux_identity_formats_unchanged() {
    let mut tmux = Options::new(false);
    tmux.use_tmux_defaults(None);
    let tmux_format = "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }";
    for name in ["window-status-format", "window-status-current-format"] {
        assert_eq!(tmux.lookup(name, &[]), Some(tmux_format), "{name}");
    }
    assert_eq!(
        tmux.lookup("status-right", &[]),
        Some("\"#{pane_title}\" %H:%M %d-%b-%y")
    );
    let grofe = Options::new(false);
    let right = grofe.lookup("status-right", &[]).unwrap();
    assert!(right.ends_with("#[range=user|agents]#{agents_badge}#[norange]"));
}

#[tokio::test]
async fn agent_since_seconds() {
    use crate::server::agents::AgentState;
    let (mut state, a, _) = two_window_state();
    let Some(then) = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90))
    else {
        return;
    };
    state
        .agents
        .apply(a, report(AgentState::Working, 1), true, then);
    let secs: u64 = var(&state, a, "agent_since").unwrap().parse().unwrap();
    assert!((90..95).contains(&secs), "{secs}");
}

/// FE.1 ruling: under tmux identity each built-in default that differs
/// is tmux 3.7b's (recorded); under grofe identity grofe's stays.
#[test]
fn tmux_identity_takes_tmux_s_defaults() {
    let recorded = [
        ("base-index", "0", "1"),
        ("pane-base-index", "0", "1"),
        ("mouse", "off", "on"),
        ("set-clipboard", "external", "external"),
        ("focus-events", "off", "on"),
        ("escape-time", "10", "25"),
        ("history-limit", "2000", "50000"),
        ("extended-keys", "off", "on"),
        ("renumber-windows", "off", "on"),
        ("mode-keys", "emacs", "vi"),
        ("status-keys", "emacs", "emacs"),
    ];
    let grofe = Options::new(false);
    let mut tmux = Options::new(false);
    tmux.use_tmux_defaults(None);
    for (name, tmux_default, grofe_default) in recorded {
        let get = |o: &Options| o.lookup(name, &[]).map(str::to_string);
        assert_eq!(get(&grofe).as_deref(), Some(grofe_default), "grofe {name}");
        assert_eq!(get(&tmux).as_deref(), Some(tmux_default), "tmux {name}");
    }
    // `set -gu` restores the identity's default.
    tmux.set(Owner::Global, "base-index", "5", false).unwrap();
    tmux.unset(Owner::Global, "base-index").unwrap();
    assert_eq!(tmux.lookup("base-index", &[]), Some("0"));
    // tmux's `$EDITOR` rule for the key tables.
    let mut vi = Options::new(false);
    vi.use_tmux_defaults(Some("/usr/local/bin/nvim"));
    assert_eq!(vi.lookup("status-keys", &[]), Some("vi"));
    assert_eq!(vi.lookup("mode-keys", &[]), Some("vi"));
    // A value the config set keeps winning, with tmux's as its base.
    let mut configured = Options::new(false);
    configured
        .config_base
        .insert("mouse".to_string(), "on".to_string());
    configured.set(Owner::Global, "mouse", "on", false).unwrap();
    configured.use_tmux_defaults(None);
    assert_eq!(configured.lookup("mouse", &[]), Some("on"));
    assert_eq!(configured.config_base["mouse"], "off");
}

/// The stub edit: `bind-key` checks the command in the request's
/// dialect and keeps that dialect on the binding.
#[test]
fn bind_key_keeps_the_requests_dialect() {
    let mut state = crate::server::test_state();
    let bind = |key: &str| BindKey {
        table: "root".to_string(),
        repeat: false,
        note: None,
        key: key.to_string(),
        command: vec!["list-sessions".to_string()],
        group: None,
    };
    let grofe_ctx = crate::server::test_ctx;
    let grofe = grofe_ctx();
    bind_key(&mut state, &grofe, bind("F5")).expect("native bind");
    let f5 = parse_key("F5").expect("key");
    let b = state.registry.key_tables.exact("root", &f5).expect("bound");
    assert_eq!(b.dialect, Dialect::Grofe);
    let tmux = ClientCtx {
        dialect: Dialect::Tmux,
        ..grofe
    };
    // FE.1: a tmux-only form binds in the tmux dialect and keeps it.
    let ls = |key: &str| BindKey {
        command: vec!["ls".to_string()],
        ..bind(key)
    };
    bind_key(&mut state, &tmux, ls("F6")).expect("tmux bind");
    let f6 = parse_key("F6").expect("key");
    let b = state.registry.key_tables.exact("root", &f6).expect("bound");
    assert_eq!(b.dialect, Dialect::Tmux);
    let err = bind_key(&mut state, &grofe_ctx(), ls("F7")).unwrap_err();
    assert_eq!(err, "unknown command: ls");
}

fn grouped_bind(key: &str, group: Option<&str>) -> BindKey {
    BindKey {
        table: "prefix".to_string(),
        repeat: false,
        note: Some("A note".to_string()),
        key: key.to_string(),
        command: vec!["list-sessions".to_string()],
        group: group.map(str::to_string),
    }
}

#[test]
fn bind_g_stored() {
    let mut state = crate::server::test_state();
    let ctx = crate::server::test_ctx();
    bind_key(&mut state, &ctx, grouped_bind("F5", Some("agents"))).expect("bind");
    let k = parse_key("F5").expect("key");
    let b = state
        .registry
        .key_tables
        .exact("prefix", &k)
        .expect("bound");
    assert_eq!(b.group.as_deref(), Some("agents"));
}

#[test]
fn list_keys_n_shows_group() {
    let mut state = crate::server::test_state();
    let ctx = crate::server::test_ctx();
    let argv: Vec<String> = ["unbind-key", "-a", "-T", "prefix"]
        .map(String::from)
        .to_vec();
    crate::server::dispatch::run(&mut state, &ctx, &argv).expect("unbind");
    bind_key(&mut state, &ctx, grouped_bind("F5", Some("agents"))).expect("bind");
    let got = list_keys(
        &mut state,
        &ctx,
        ListKeys {
            table: Some("prefix".to_string()),
            first: false,
            notes: true,
            key: None,
            all: false,
            format: None,
            prefix: None,
            sort: Default::default(),
        },
    )
    .expect("list");
    assert_eq!(got, "F5 [agents] A note\n");
}

#[test]
fn tmux_dialect_list_keys_unchanged() {
    let mut state = crate::server::test_state();
    let grofe = crate::server::test_ctx();
    let tmux = ClientCtx {
        dialect: Dialect::Tmux,
        ..crate::server::test_ctx()
    };
    let list = |state: &mut State| {
        list_keys(
            state,
            &tmux,
            ListKeys {
                table: Some("prefix".to_string()),
                first: false,
                notes: true,
                key: Some("F5".to_string()),
                all: false,
                format: None,
                prefix: Some("C-b".to_string()),
                sort: Default::default(),
            },
        )
        .expect("list")
    };
    bind_key(&mut state, &grofe, grouped_bind("F5", None)).expect("bind");
    let plain = list(&mut state);
    bind_key(&mut state, &grofe, grouped_bind("F5", Some("agents"))).expect("bind");
    assert_eq!(list(&mut state), plain);
    assert!(!plain.contains("agents"), "{plain}");
}

/// TP.12: `#{start_time}` is when the server started, in Unix seconds,
/// without a pane: continuum reads it to tell a fresh server (restore
/// once) from one that has been running.
#[test]
fn start_time_is_the_server_start() {
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let model = Model::new(false);
    let got = FormatCtx::new(&model, PaneId(99)).get("start_time");
    let secs: u64 = got.expect("start_time set").parse().expect("a number");
    assert!(secs >= before && secs <= before + 5, "{secs} vs {before}");
}

/// TC.3: `#{version}` follows the model's identity (TC.1 sets it);
/// `#{grofe_version}` does not. Neither needs a pane.
#[test]
fn version_follows_the_identity() {
    let mut model = Model::new(false);
    let grofe = env!("CARGO_PKG_VERSION");
    let get = |m: &Model, name: &str| FormatCtx::new(m, PaneId(99)).get(name);
    assert_eq!(get(&model, "version").as_deref(), Some(grofe));
    model.identity = crate::compat::Identity::Tmux;
    assert_eq!(get(&model, "version").as_deref(), Some("3.7b"));
    assert_eq!(get(&model, "grofe_version").as_deref(), Some(grofe));
}

#[test]
fn same_value_set_is_a_no_op() {
    let mut o = Options::new(false);
    let s = Owner::Session(SessionId(0));
    assert_eq!(o.set(s, "@x", "a", false), Ok(true));
    assert_eq!(o.set(s, "@x", "a", false), Ok(false));
    assert_eq!(o.set(s, "@x", "b", false), Ok(true));
    // A flag option normalizes before comparing: `yes` is `on`.
    assert_eq!(o.set(Owner::Global, "allow-rename", "on", false), Ok(true));
    assert_eq!(
        o.set(Owner::Global, "allow-rename", "yes", false),
        Ok(false)
    );
}

#[test]
fn persist_flag_is_stored() {
    let mut o = Options::new(false);
    let p = Owner::Pane(PaneId(3));
    o.set(p, "@state", "busy", true).unwrap();
    assert!(o.get(p, "@state").unwrap().persist);
    o.set(p, "@other", "x", false).unwrap();
    assert!(!o.get(p, "@other").unwrap().persist);
}

#[test]
fn lookup_walks_pane_window_session_then_global() {
    let mut o = Options::new(false);
    let chain = [
        Owner::Pane(PaneId(0)),
        Owner::Window(WindowId(0)),
        Owner::Session(SessionId(0)),
    ];
    assert_eq!(o.lookup("@x", &chain), None);
    for (owner, v) in [
        (Owner::Global, "g"),
        (chain[2], "s"),
        (chain[1], "w"),
        (chain[0], "p"),
    ] {
        o.set(owner, "@x", v, false).unwrap();
        assert_eq!(o.lookup("@x", &chain), Some(v));
    }
    for (owner, v) in [(chain[0], "w"), (chain[1], "s"), (chain[2], "g")] {
        assert_eq!(o.unset(owner, "@x"), Ok(true));
        assert_eq!(o.lookup("@x", &chain), Some(v));
    }
}

#[test]
fn builtins_validate_and_reset_to_default() {
    let mut o = Options::new(true);
    assert_eq!(o.lookup(REMAIN_ON_EXIT, &[]), Some("on"));
    assert_eq!(
        o.set(Owner::Global, "no-such", "1", false),
        Err("invalid option: no-such".to_string())
    );
    assert!(
        o.set(Owner::Global, REMAIN_ON_EXIT, "maybe", false)
            .is_err()
    );
    assert!(o.set(Owner::Global, "history-limit", "-3", false).is_err());
    o.set(Owner::Global, "history-limit", "10", false).unwrap();
    o.unset(Owner::Global, "history-limit").unwrap();
    assert_eq!(o.lookup("history-limit", &[]), Some("50000"));
}

#[test]
fn builtin_defaults_match_the_model() {
    let o = Options::new(false);
    assert_eq!(
        o.lookup("history-limit", &[]),
        Some(HISTORY_LIMIT.to_string().as_str())
    );
    assert_eq!(
        o.lookup("base-index", &[]),
        Some(BASE_INDEX.to_string().as_str())
    );
    assert_eq!(
        o.lookup("pane-base-index", &[]),
        Some(PANE_BASE_INDEX.to_string().as_str())
    );
}

#[test]
fn retain_live_drops_values_of_removed_objects() {
    let mut o = Options::new(false);
    o.set(Owner::Pane(PaneId(7)), "@x", "1", false).unwrap();
    o.retain_live(&Model::default());
    let owners: BTreeSet<Owner> = o.values.keys().copied().collect();
    assert_eq!(owners, BTreeSet::from([Owner::Global]));
}

/// The rows T3.0 adds (R4): tmux's names and scopes, spec §5.3's
/// defaults (`set-clipboard` `external` as tmux's, a ruling), and
/// tmux's value checks.
#[test]
fn m3_rows_have_grofe_defaults_and_tmux_scopes() {
    let o = Options::new(false);
    for (name, default, scope) in [
        ("mouse", "on", OptionScope::Session),
        ("set-clipboard", "external", OptionScope::Global),
        ("focus-events", "on", OptionScope::Global),
        ("pane-border-style", "default", OptionScope::Window),
        (
            "pane-active-border-style",
            "#{?pane_in_mode,fg=yellow,#{?synchronize-panes,fg=red,fg=green}}",
            OptionScope::Window,
        ),
        ("status-position", "bottom", OptionScope::Session),
        ("allow-passthrough", "off", OptionScope::Window),
    ] {
        assert_eq!(o.lookup(name, &[]), Some(default), "{name}");
        assert_eq!(builtin(name).map(|b| b.scope), Some(scope), "{name}");
        assert!(builtin_names().any(|n| n == name), "{name}");
    }
    assert_eq!(validate("set-clipboard", "external"), Ok("external".into()));
    assert_eq!(
        validate("set-clipboard", "maybe"),
        Err("unknown value: maybe".to_string())
    );
    assert_eq!(validate("mouse", "yes"), Ok("on".into()));
    assert_eq!(validate("status-position", "top"), Ok("top".into()));
    assert_eq!(
        validate("status-position", "left"),
        Err("unknown value: left".to_string())
    );
    assert_eq!(
        builtin_names().count() + grofe_only_names().count(),
        BUILTINS.len()
    );
}

/// `set-titles`/`set-titles-string` (R15): tmux's own names, scope and
/// `set-titles` default; `set-titles-string`'s default is tmux's own
/// translated into `#{...}` form.
#[test]
fn set_titles_rows_are_session_scoped_and_off_by_default() {
    let o = Options::new(false);
    assert_eq!(o.lookup("set-titles", &[]), Some("off"));
    assert_eq!(
        builtin("set-titles").map(|b| b.scope),
        Some(OptionScope::Session)
    );
    assert_eq!(
        o.lookup("set-titles-string", &[]),
        Some(
            "#{session_name}:#{window_index}:#{window_name} - \"#{pane_title}\" #{session_alerts}"
        )
    );
    assert_eq!(
        builtin("set-titles-string").map(|b| b.scope),
        Some(OptionScope::Session)
    );
    assert_eq!(validate("set-titles", "yes"), Ok("on".into()));
    assert_eq!(
        validate("set-titles", "maybe"),
        Err("bad value: maybe".to_string())
    );
}

/// TC.2b: tmux 3.7b's index grammar (recorded on a private socket).
#[test]
fn list_indices_read_as_tmux_reads_them() {
    for (name, want) in [
        ("status-format", Some(("status-format", None))),
        ("status-format[1]", Some(("status-format", Some(1)))),
        ("status-format[+3]", Some(("status-format", Some(3)))),
        ("status-format[ 5]", Some(("status-format", Some(5)))),
        ("status-format[0x6]", Some(("status-format", Some(0)))),
        (
            "status-format[4294967296]",
            Some(("status-format", Some(0))),
        ),
        (
            "status-format[99999999999]",
            Some(("status-format", Some(1_215_752_191))),
        ),
        (
            "status-format[2147483647]",
            Some(("status-format", Some(i32::MAX as u32))),
        ),
        ("status-format[2147483648]", None),
        ("status-format[-1]", None),
        ("status-format[1a]", None),
        ("status-format[a1]", None),
        ("status-format[1]]", None),
        ("status-format[[1]", None),
        ("status-format[1][2]", None),
        ("[1]", None),
    ] {
        let got = split_index(name);
        match want {
            Some(w) => assert_eq!(got, Ok(w), "{name}"),
            None => assert_eq!(got, Err(format!("ambiguous option: {name}")), "{name}"),
        }
    }
}

/// A session's list hides the global one as a whole, as in tmux.
#[test]
fn a_list_lookup_takes_the_first_owner_with_a_list() {
    let mut o = Options::new(false);
    let s = Owner::Session(SessionId(0));
    let set = |index, value: &str| ListOp::Set {
        index,
        value: value.to_string(),
        append: false,
    };
    o.edit_list(Owner::Global, "status-format", set(1, "g"), false)
        .unwrap();
    let global = o.lookup_list("status-format", &[s]).cloned();
    let rows = global.clone().expect("a list");
    assert_eq!(rows[&0], "");
    assert_eq!(rows[&1], "g");
    assert_eq!(o.edit_list(s, "status-format", set(1, "s"), true), Ok(true));
    assert_eq!(
        o.edit_list(s, "status-format", set(1, "s"), true),
        Ok(false)
    );
    assert_eq!(
        o.lookup_list("status-format", &[s]),
        Some(&BTreeMap::from([(1, "s".to_string())]))
    );
    assert!(o.list(s, "status-format").unwrap().persist);
    assert_eq!(
        o.edit_list(s, "status-format", ListOp::Reset, false),
        Ok(true)
    );
    assert_eq!(o.lookup_list("status-format", &[s]).cloned(), global);
    assert_eq!(
        o.edit_list(Owner::Global, "status-format", ListOp::Reset, false),
        Ok(true)
    );
    assert_eq!(
        o.edit_list(Owner::Global, "nosuch", ListOp::Reset, false),
        Err("invalid option: nosuch".to_string())
    );
    assert!(list_names().all(|n| builtin(n).is_none()));
}

/// Each case recorded from tmux 3.7b (`set -g @t <value>; show -g @t`)
/// on a private socket.
/// Recorded on tmux 3.7b (`set -g prefix X; show -gv prefix`).
#[test]
fn prefix_is_a_key_name_stored_as_tmux_spells_it() {
    for (given, stored) in [
        ("C-a", "C-a"),
        ("c-B", "C-B"),
        ("M-C-x", "C-M-x"),
        ("^a", "C-a"),
        ("pgup", "PPage"),
        ("none", "None"),
        ("C-S-a", "C-S-a"),
        ("S-Tab", "S-Tab"),
        ("BTab", "BTab"),
    ] {
        assert_eq!(validate("prefix", given), Ok(stored.to_string()), "{given}");
    }
    assert_eq!(validate("prefix", "foo"), Err("bad key: foo".to_string()));
}

#[test]
fn apply_config_copies_agent_surface_tables() {
    let mut state = crate::server::test_state();
    let mut resolved = Resolved::default();
    resolved.agents.sidebar.width = 40;
    resolved.agents.picker.width_pct = 55;
    resolved.agents.grid.tile_width = 33;
    apply_config(&mut state, &resolved);
    assert_eq!(state.agents_sidebar.width, 40);
    assert_eq!(state.agents_picker.width_pct, 55);
    assert_eq!(state.agents_grid.tile_width, 33);
}

/// The pinned tmux text (`tmux-options.txt`) is a second source for tmux's
/// option names and spelled defaults; the options table must agree with it.
/// Its numbers come from the compat tests' own config, so only names and
/// spelled formats are compared, not the `tmux_default` column's numbers.
#[test]
fn the_options_table_agrees_with_the_pinned_tmux_text() {
    use crate::compat::tmux::formats::{Table, tmux_default, tmux_table};
    // Options grofe has that tmux 3.7b has not, without being `GROFE_ONLY`
    // (tmux's CLI would call them invalid; the dialect shows them).
    let not_tmux = [
        "compat-identity",
        "graphics",
        "restore-on-start",
        "restore-stagger-ms",
        "resurrect-dir",
        "start-from-tmux",
    ];
    let pinned: BTreeSet<&str> = [Table::Session, Table::Window, Table::Server]
        .into_iter()
        .flat_map(tmux_table)
        .map(|(n, _)| n.split('[').next().unwrap_or(n))
        .collect();
    let ours: BTreeSet<&str> = builtin_names().chain(list_names()).collect();
    for name in &pinned {
        assert!(ours.contains(name), "tmux has {name}, the table does not");
    }
    for name in &ours {
        assert_eq!(
            pinned.contains(name),
            !not_tmux.contains(name),
            "{name} against the pinned tmux tables"
        );
    }
    for b in BUILTINS
        .iter()
        .filter(|b| b.tmux_default.is_some() && !is_grofe_only(b.name))
    {
        assert!(pinned.contains(b.name), "{} is not tmux's", b.name);
    }
    for name in SPELLED_DEFAULTS {
        let spelled = tmux_default(name).or_else(|| tmux_default(&format!("{name}[0]")));
        assert!(spelled.is_some(), "{name} has no pinned tmux text");
    }
}

/// Every built-in option, scalar or list, says what it does: the
/// generated reference prints it.
#[test]
fn every_builtin_has_a_doc() {
    let missing: Vec<&str> = BUILTINS
        .iter()
        .filter(|b| b.doc.trim().is_empty())
        .map(|b| b.name)
        .chain(
            LISTS
                .iter()
                .filter(|l| l.doc.trim().is_empty())
                .map(|l| l.name),
        )
        .collect();
    assert!(missing.is_empty(), "options with no doc: {missing:?}");
}

/// Every option in the reference has its TOML key, so the book never
/// shows an option the config cannot set.
#[test]
fn reference_lists_every_option_with_its_toml_key() {
    let text = reference_markdown();
    for name in BUILTINS.iter().map(|b| b.name).chain(list_names()) {
        assert!(text.contains(&format!("\n## {name}\n")), "{name} missing");
    }
    assert!(
        !text.contains("TOML key**: `(none)`"),
        "an option has no TOML key"
    );
}
