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
// ponytail: no `visual-*` status messages (the bell still reaches the
// terminal, `passthrough`), and the activity and silence status styles
// never apply (`status.rs`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::time::Instant;

use crate::server::dispatch::options::Owner;
use crate::server::{Job, State, clients, hooks};
use crate::types::{PaneId, WindowId};

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

/// A bell rang in `window`.
pub fn bell(state: &mut State, window: WindowId) {
    if monitored(state, window, Alert::Bell) {
        raise(state, window, Alert::Bell);
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
