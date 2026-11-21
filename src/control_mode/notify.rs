//! The pure part of the bridge: [`Snapshot`] is a view of the server, and
//! [`diff`] turns two views into the notification lines between them.

use std::collections::{BTreeMap, BTreeSet};

/// One window as the server reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowInfo {
    /// The sessions the window is linked into.
    pub sessions: BTreeSet<String>,
    pub name: String,
    pub layout: String,
    /// `#{window_visible_layout}`; empty when the server has none (a zoomed
    /// window), then the layout stands in.
    pub visible_layout: String,
    pub flags: String,
    pub active_pane: String,
}

impl WindowInfo {
    fn visible(&self) -> &str {
        if self.visible_layout.is_empty() {
            &self.layout
        } else {
            &self.visible_layout
        }
    }
}

/// The sessions and windows of a server, as `$N` and `@N` ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Session id to its name.
    pub sessions: BTreeMap<String, String>,
    /// Session id to its current window.
    pub current: BTreeMap<String, String>,
    pub windows: BTreeMap<String, WindowInfo>,
    /// Every pane id (`%N`) the server has.
    pub panes: BTreeSet<String>,
}

/// The `-F` of the sessions query: id and name.
pub const SESSIONS_FORMAT: &str = "#{session_id}\t#{session_name}";

/// The `-F` of the windows query: session, window, whether the window is
/// its session's current one, its active pane, layout, flags and name.
pub const WINDOWS_FORMAT: &str = "#{session_id}\t#{window_id}\t#{window_active}\t#{pane_id}\t#{window_layout}\t#{window_visible_layout}\t#{window_flags}\t#{window_name}";

/// The `-F` of the panes query.
pub const PANES_FORMAT: &str = "#{pane_id}";

impl Snapshot {
    /// Reads the output of `list-sessions -F` [`SESSIONS_FORMAT`] and
    /// `list-windows -a -F` [`WINDOWS_FORMAT`].
    pub fn parse(sessions: &str, windows: &str) -> Snapshot {
        let mut snap = Snapshot::default();
        for line in sessions.lines() {
            if let Some((id, name)) = line.split_once('\t') {
                snap.sessions.insert(id.to_string(), name.to_string());
            }
        }
        for line in windows.lines() {
            let f: Vec<&str> = line.splitn(8, '\t').collect();
            let [session, window, active, pane, layout, visible, flags, name] = f[..] else {
                continue;
            };
            if active == "1" {
                snap.current.insert(session.to_string(), window.to_string());
            }
            let info = snap.windows.entry(window.to_string()).or_default();
            info.sessions.insert(session.to_string());
            if info.name.is_empty() || active == "1" {
                info.name = name.to_string();
                info.layout = layout.to_string();
                info.visible_layout = visible.to_string();
                info.flags = flags.to_string();
                info.active_pane = pane.to_string();
            }
        }
        snap
    }

    /// The windows linked into `session`, in id order.
    pub fn windows_of(&self, session: &str) -> Vec<&str> {
        self.windows
            .iter()
            .filter(|(_, w)| w.sessions.contains(session))
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

/// The notifications between `old` and `new` for a client attached to
/// `attached`, in the order tmux 3.7b sends them for one command.
///
/// A window that leaves with its session sends nothing; one that leaves
/// while a session still holds it sends `%unlinked-window-close`, which is
/// what tmux 3.7b sends for `kill-window` (the window is unlinked before
/// the notice).
// ponytail: `%window-close` (a window still linked in the client's session
// when it closes) is never sent.
pub fn diff(old: &Snapshot, new: &Snapshot, attached: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (session, window) in &new.current {
        if old.current.get(session).is_some_and(|w| w != window) {
            out.push(format!("%session-window-changed {session} {window}"));
        }
    }
    for (id, w) in &new.windows {
        if !old.windows.contains_key(id) {
            let kind = if w.sessions.contains(attached) {
                "window-add"
            } else {
                "unlinked-window-add"
            };
            out.push(format!("%{kind} {id}"));
        }
    }
    let ids = |s: &Snapshot| s.sessions.keys().cloned().collect::<Vec<_>>();
    if ids(old) != ids(new) {
        out.push("%sessions-changed".to_string());
    }
    if let (Some(a), Some(b)) = (old.sessions.get(attached), new.sessions.get(attached))
        && a != b
    {
        out.push(format!("%session-renamed {attached} {b}"));
    }
    for (id, w) in &new.windows {
        let Some(was) = old.windows.get(id) else {
            continue;
        };
        if was.name != w.name {
            let kind = if w.sessions.contains(attached) {
                "window-renamed"
            } else {
                "unlinked-window-renamed"
            };
            out.push(format!("%{kind} {id} {}", w.name));
        }
    }
    for (id, w) in &new.windows {
        if let Some(was) = old.windows.get(id)
            && was.active_pane != w.active_pane
        {
            out.push(format!("%window-pane-changed {id} {}", w.active_pane));
        }
    }
    for (id, w) in &new.windows {
        if let Some(was) = old.windows.get(id)
            && (was.layout != w.layout || was.visible() != w.visible())
            && w.sessions.contains(attached)
        {
            out.push(format!(
                "%layout-change {id} {} {} {}",
                w.layout,
                w.visible(),
                w.flags
            ));
        }
    }
    for (id, was) in &old.windows {
        let kept = was.sessions.iter().any(|s| new.sessions.contains_key(s));
        if !new.windows.contains_key(id) && kept {
            out.push(format!("%unlinked-window-close {id}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(sessions: &[(&str, &str)], windows: &[&str]) -> Snapshot {
        let s: String = sessions
            .iter()
            .map(|(i, n)| format!("{i}\t{n}\n"))
            .collect();
        // A line is `session window active pane layout flags name`; the
        // visible layout is added empty unless `vis` follows the layout.
        let w: String = windows
            .iter()
            .map(|l| {
                let mut f: Vec<&str> = l.split('\t').collect();
                if f.len() == 7 {
                    f.insert(5, "");
                }
                format!("{}\n", f.join("\t"))
            })
            .collect();
        Snapshot::parse(&s, &w)
    }

    #[test]
    fn layout_change_reports_the_visible_layout() {
        let old = snap(&[("$0", "m")], &["$0\t@0\t1\t%0\tL0\t*\tw"]);
        let mut new = snap(&[("$0", "m")], &["$0\t@0\t1\t%0\tL0\t*Z\tw"]);
        // A zoomed window: the visible layout is the one zoomed pane.
        new.windows.get_mut("@0").unwrap().visible_layout = "V0".into();
        assert_eq!(diff(&old, &new, "$0"), ["%layout-change @0 L0 V0 *Z"]);
        let parsed = Snapshot::parse("$0\tm\n", "$0\t@0\t1\t%0\tL\tV\t*\tw\n");
        assert_eq!(parsed.windows["@0"].visible_layout, "V");
        assert_eq!(parsed.windows["@0"].name, "w");
    }

    #[test]
    fn a_new_window_is_added_and_a_new_session_is_unlinked() {
        let old = snap(&[("$0", "m")], &["$0\t@0\t1\t%0\tL0\t*\tw0"]);
        let new = snap(
            &[("$0", "m")],
            &["$0\t@0\t1\t%0\tL0\t*\tw0", "$0\t@1\t0\t%1\tL1\t-\tw1"],
        );
        assert_eq!(diff(&old, &new, "$0"), ["%window-add @1"]);
        let new = snap(
            &[("$0", "m"), ("$1", "two")],
            &["$0\t@0\t1\t%0\tL0\t*\tw0", "$1\t@2\t1\t%2\tL2\t*\tw2"],
        );
        assert_eq!(
            diff(&old, &new, "$0"),
            ["%unlinked-window-add @2", "%sessions-changed"]
        );
    }

    #[test]
    fn current_window_pane_layout_and_name_changes() {
        let old = snap(
            &[("$0", "m")],
            &["$0\t@0\t1\t%0\tL0\t*\ta", "$0\t@1\t0\t%1\tL1\t-\tb"],
        );
        let new = snap(
            &[("$0", "m")],
            &["$0\t@0\t0\t%3\tL9\t-\ta", "$0\t@1\t1\t%1\tL1\t*\tc"],
        );
        assert_eq!(
            diff(&old, &new, "$0"),
            [
                "%session-window-changed $0 @1",
                "%window-renamed @1 c",
                "%window-pane-changed @0 %3",
                "%layout-change @0 L9 L9 -",
            ]
        );
    }

    #[test]
    fn a_killed_window_closes_unlinked_but_a_killed_session_is_quiet() {
        let old = snap(
            &[("$0", "m"), ("$1", "t")],
            &[
                "$0\t@0\t1\t%0\tL\t*\ta",
                "$0\t@1\t0\t%1\tL\t-\tb",
                "$1\t@2\t1\t%2\tL\t*\tc",
            ],
        );
        let no_window = snap(
            &[("$0", "m"), ("$1", "t")],
            &["$0\t@0\t1\t%0\tL\t*\ta", "$1\t@2\t1\t%2\tL\t*\tc"],
        );
        assert_eq!(diff(&old, &no_window, "$0"), ["%unlinked-window-close @1"]);
        let no_session = snap(
            &[("$0", "m")],
            &["$0\t@0\t1\t%0\tL\t*\ta", "$0\t@1\t0\t%1\tL\t-\tb"],
        );
        assert_eq!(diff(&old, &no_session, "$0"), ["%sessions-changed"]);
    }
}
