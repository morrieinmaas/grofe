//! Window alerts, as tmux 3.7b's `alerts.c` (recorded on a private
//! socket): a bell, output (activity) and no output for a while (silence)
//! in a window set its flag and fire its hook.
//!
//! - **Bell**, while `monitor-bell` is on (the default): the window's
//!   `window_bell_flag` is set unless it is its session's current window
//!   and the session is attached, and `alert-bell` fires if `bell-action`
//!   (`any` by default) applies, for every bell.
//! - **Activity**, while `monitor-activity` is on (off by default): the
//!   same with `window_activity_flag` and `activity-action` (`other`), but
//!   only while the flag is clear, so once until the window is selected.
//! - **Silence**, while `monitor-silence` is a number of seconds (0, off,
//!   by default): no output for that long does the same with
//!   `window_silence_flag` and `silence-action` (`other`). Output and a
//!   `set` of `monitor-silence` restart the wait (tmux's `alerts_reset`).
//!
//! A window that becomes its session's current one counts as output in
//! it, as in tmux ([`drain`]): in a session no client shows, that sets the
//! activity flag of the window just selected.
//!
//! An action applies by whether the window is its session's current one:
//! `any`, `none`, `current`, `other`. The hook runs in the window, with
//! `#{hook_session}`, `#{hook_session_name}`, `#{hook_window}` and
//! `#{hook_window_name}` set. Selecting the window clears its flags
//! (`Model::select_window`).
//!
//! When the action applies, `visual-bell`, `visual-activity` and
//! `visual-silence` decide what the session's clients get, as tmux's
//! `alerts_set_message`: `off` rings the terminal, `on` shows "Bell in
//! window 2" (or "in current window") on the status line, `both` does both.
//! A bell rings the terminal here and nowhere else, so `monitor-bell off`
//! and `bell-action none` keep it quiet. The status line draws a flagged
//! window in `window-status-bell-style`, or `window-status-activity-style`
//! for an activity or silence flag (`status.rs`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::time::Instant;

use crate::server::dispatch::options::Owner;
use crate::server::{Job, State, clients, hooks};
use crate::types::{PaneId, SessionId, WindowId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alert {
    Bell,
    Activity,
    Silence,
}

impl Alert {
    fn monitor(self) -> &'static str {
        match self {
            Alert::Bell => "monitor-bell",
            Alert::Activity => "monitor-activity",
            Alert::Silence => "monitor-silence",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Alert::Bell => "bell-action",
            Alert::Activity => "activity-action",
            Alert::Silence => "silence-action",
        }
    }

    /// The word of tmux's message, and the option that picks how it shows.
    fn visual(self) -> (&'static str, &'static str) {
        match self {
            Alert::Bell => ("Bell", "visual-bell"),
            Alert::Activity => ("Activity", "visual-activity"),
            Alert::Silence => ("Silence", "visual-silence"),
        }
    }

    fn hook(self) -> &'static str {
        match self {
            Alert::Bell => "alert-bell",
            Alert::Activity => "alert-activity",
            Alert::Silence => "alert-silence",
        }
    }
}

/// Each window's silence wait: when it ends, and whether a timer is out
/// for it. Lives on `State.alerts`.
#[derive(Default)]
pub struct Alerts {
    silence: BTreeMap<WindowId, Silence>,
    /// Windows with a bell waiting for the alerts pass (tmux's
    /// `alerts_queued`): bells in a window before it runs ring once.
    bell_queued: BTreeSet<WindowId>,
}

struct Silence {
    due: Instant,
    armed: bool,
}

/// A window option for `window`, looked up through its session.
fn option(state: &State, window: WindowId, name: &str) -> Option<String> {
    let session = state.model.windows.get(&window)?.session;
    let chain = [Owner::Window(window), Owner::Session(session)];
    state.model.options.lookup(name, &chain).map(str::to_string)
}

fn monitored(state: &State, window: WindowId, alert: Alert) -> bool {
    option(state, window, alert.monitor()).as_deref() == Some("on")
}

/// tmux's `alerts_action_applies`.
fn applies(action: &str, current: bool) -> bool {
    match action {
        "any" => true,
        "current" => current,
        "other" => !current,
        _ => false,
    }
}

/// A bell rang in `window`. Like tmux's `alerts_queue`, the bells of one
/// burst are one alert: the window is checked once, behind the events
/// already waiting.
pub fn bell(state: &mut State, window: WindowId) {
    if !state.alerts.bell_queued.insert(window) {
        return;
    }
    let job: Job = Box::new(move |state: &mut State| {
        state.alerts.bell_queued.remove(&window);
        if monitored(state, window, Alert::Bell) {
            raise(state, window, Alert::Bell);
        }
    });
    if let Err(e) = state.jobs.try_send(job) {
        // No room to queue it: the alert runs now.
        use tokio::sync::mpsc::error::TrySendError::{Closed, Full};
        let (Full(job) | Closed(job)) = e;
        job(state);
    }
}

/// `panes` printed something: activity in their windows, and their
/// silence waits start again.
pub fn output(state: &mut State, panes: &BTreeSet<PaneId>) {
    let windows: BTreeSet<WindowId> = panes
        .iter()
        .filter_map(|p| state.model.panes.get(p).map(|p| p.window))
        .collect();
    for window in windows {
        touch(state, window);
        if monitored(state, window, Alert::Activity) {
            raise(state, window, Alert::Activity);
        }
        restart_silence(state, window);
    }
}

/// The window saw output or was selected: tmux's `window_update_activity`.
fn touch(state: &mut State, window: WindowId) {
    if let Some(w) = state.model.windows.get_mut(&window) {
        w.activity = std::time::SystemTime::now();
    }
}

/// Windows that became current count as output in them, as tmux's
/// `session_set_current` updates the window's activity: run at the end of
/// every command sequence and pane wake-up.
pub fn drain(state: &mut State) {
    let selected = std::mem::take(&mut state.model.selected);
    for window in selected {
        touch(state, window);
        if monitored(state, window, Alert::Activity) {
            raise(state, window, Alert::Activity);
        }
        restart_silence(state, window);
    }
}

/// `monitor-silence` was set: every window's wait starts again, as
/// tmux's `alerts_reset_all`.
pub fn silence_option_set(state: &mut State) {
    let windows: Vec<WindowId> = state.model.windows.keys().copied().collect();
    for window in windows {
        restart_silence(state, window);
    }
}

fn silence_secs(state: &State, window: WindowId) -> u64 {
    option(state, window, Alert::Silence.monitor())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn restart_silence(state: &mut State, window: WindowId) {
    let secs = silence_secs(state, window);
    if secs == 0 {
        state.alerts.silence.remove(&window);
        return;
    }
    let due = Instant::now() + Duration::from_secs(secs);
    let wait = state
        .alerts
        .silence
        .entry(window)
        .or_insert(Silence { due, armed: false });
    wait.due = due;
    if !wait.armed {
        wait.armed = true;
        arm(state, window, due);
    }
}

/// One timer per window: it wakes at `due`, and the job checks whether
/// output moved the wait on since.
fn arm(state: &State, window: WindowId, due: Instant) {
    let jobs = state.jobs.clone();
    tokio::spawn(async move {
        tokio::time::sleep_until(due).await;
        let job: Job = Box::new(move |state: &mut State| silence_due(state, window));
        let _ = jobs.send(job).await;
    });
}

fn silence_due(state: &mut State, window: WindowId) {
    let Some(wait) = state.alerts.silence.get_mut(&window) else {
        return;
    };
    if !state.model.windows.contains_key(&window) {
        state.alerts.silence.remove(&window);
        return;
    }
    let due = wait.due;
    if Instant::now() < due {
        arm(state, window, due);
        return;
    }
    state.alerts.silence.remove(&window);
    if silence_secs(state, window) > 0 {
        raise(state, window, Alert::Silence);
    }
}

/// tmux's `alerts_check_bell`, `_activity` and `_silence` for one window.
fn raise(state: &mut State, window: WindowId, alert: Alert) {
    let Some(w) = state.model.windows.get(&window) else {
        return;
    };
    let Some(s) = state.model.sessions.get(&w.session) else {
        return;
    };
    let current = s.active == window;
    let attached = s.attached > 0;
    let set = match alert {
        Alert::Bell => w.bell_flag,
        Alert::Activity => w.activity_flag,
        Alert::Silence => w.silence_flag,
    };
    // Activity and silence alert once until the window is selected.
    if set && alert != Alert::Bell {
        return;
    }
    if !current || !attached {
        if let Some(w) = state.model.windows.get_mut(&window) {
            match alert {
                Alert::Bell => w.bell_flag = true,
                Alert::Activity => w.activity_flag = true,
                Alert::Silence => w.silence_flag = true,
            }
        }
        if !set {
            clients::mark_all_dirty(state);
        }
    }
    let action = option(state, window, alert.action()).unwrap_or_default();
    if applies(&action, current) {
        hooks::window_alert(state, alert.hook(), window);
    }
    // Every session the window is linked into (a session group's members
    // too), each by its own action, as tmux walks the window's winlinks.
    let Some(w) = state.model.windows.get(&window) else {
        return;
    };
    let mut sessions: Vec<SessionId> = w.links.iter().map(|(s, _)| *s).collect();
    if sessions.is_empty() {
        sessions.push(w.session);
    }
    sessions.sort();
    sessions.dedup();
    for session in sessions {
        let current = state
            .model
            .sessions
            .get(&session)
            .is_some_and(|s| s.active == window);
        let chain = [Owner::Window(window), Owner::Session(session)];
        let action = state.model.options.lookup(alert.action(), &chain);
        if applies(action.unwrap_or_default(), current) {
            visual(state, window, session, alert);
        }
    }
}

/// tmux's `alerts_set_message`: what each client of `session` gets for
/// `alert` in `window`, by the session's `visual-*` option.
fn visual(state: &mut State, window: WindowId, session: SessionId, alert: Alert) {
    let Some(w) = state.model.windows.get(&window) else {
        return;
    };
    let (word, option) = alert.visual();
    let how = state
        .model
        .options
        .lookup(option, &[Owner::Session(session)])
        .unwrap_or("off")
        .to_string();
    let current = state
        .model
        .sessions
        .get(&session)
        .is_some_and(|s| s.active == window);
    let index = state.model.index_in(session, window).unwrap_or(w.index);
    let text = if current {
        format!("{word} in current window")
    } else {
        format!("{word} in window {index}")
    };
    let ids: Vec<_> = state
        .clients
        .iter()
        .filter(|(_, c)| c.session == session)
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        if how == "off" || how == "both" {
            clients::queue_raw(state, id, b"\x07");
        }
        if how != "off" {
            clients::show_message_as(state, id, &text, crate::server::render::Level::Warning);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::applies;

    #[test]
    fn actions_apply_by_whether_the_window_is_current() {
        for (action, current, other) in [
            ("any", true, true),
            ("none", false, false),
            ("current", true, false),
            ("other", false, true),
        ] {
            assert_eq!(applies(action, true), current, "{action}");
            assert_eq!(applies(action, false), other, "{action}");
        }
    }
}
