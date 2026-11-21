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
    /// Pane id to its `#{pane_mode}`, empty out of a mode.
    pub modes: BTreeMap<String, String>,
    /// Client name to its session, `$N name`.
    pub clients: BTreeMap<String, String>,
    /// Paste buffer name to what tells its content apart (size, time,
    /// sample).
    pub buffers: BTreeMap<String, String>,
}

/// The `-F` of the sessions query: id and name.
pub const SESSIONS_FORMAT: &str = "#{session_id}\t#{session_name}";

/// The `-F` of the windows query: session, window, whether the window is
/// its session's current one, its active pane, layout, flags and name.
pub const WINDOWS_FORMAT: &str = "#{session_id}\t#{window_id}\t#{window_active}\t#{pane_id}\t#{window_layout}\t#{window_visible_layout}\t#{window_flags}\t#{window_name}";

/// The `-F` of the panes query: id and mode.
pub const PANES_FORMAT: &str = "#{pane_id}\t#{pane_mode}";

/// The `-F` of the clients query: name, session id and name.
// ponytail: the control client is not a server client, so this lists
// only the others; `%client-*` never names another control bridge.
pub const CLIENTS_FORMAT: &str = "#{client_name}\t#{session_id} #{session_name}";

/// The `-F` of the buffers query: name, then its content's fingerprint.
pub const BUFFERS_FORMAT: &str =
    "#{buffer_name}\t#{buffer_size}\t#{buffer_created}\t#{buffer_sample}";

/// `key<TAB>value` lines as a map.
pub fn pairs(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(|l| l.split_once('\t').unwrap_or((l, "")))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

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

/// The `%window-add` (or unlinked) lines for the links `window` gained.
fn adds(
    out: &mut Vec<String>,
    id: &str,
    gained: &BTreeSet<String>,
    w: &WindowInfo,
    attached: &str,
) {
    let kind = if w.sessions.contains(attached) {
        "window-add"
    } else {
        "unlinked-window-add"
    };
    out.extend(gained.iter().map(|_| format!("%{kind} {id}")));
}

/// The notifications between `old` and `new` for a client attached to
/// `attached`, in the order tmux 3.7b sends them for one command.
///
/// Each link a window gains or loses is one notice (tmux notifies per
/// winlink), named for whether the window is in the client's session once
/// the command is done: a window unlinked from another session but still in
/// the client's sends `%window-close`, one killed outright
/// `%unlinked-window-close` (tmux unlinks it before the notice). When the
/// client's own session went, it is leaving and gets no window notices.
// ponytail: the notices of one command come in this fixed order, not the
// order the command made them (tmux interleaves a multi-link `kill-window`).
pub fn diff(old: &Snapshot, new: &Snapshot, attached: &str) -> Vec<String> {
    let mut out = Vec::new();
    let empty = BTreeSet::new();
    let gained = |id: &String, w: &WindowInfo| -> BTreeSet<String> {
        let had = old.windows.get(id).map_or(&empty, |o| &o.sessions);
        w.sessions.difference(had).cloned().collect()
    };
    // `link-window` notifies the link before selecting it; `new-window`
    // selects first.
    for (id, w) in &new.windows {
        if old.windows.contains_key(id) {
            adds(&mut out, id, &gained(id, w), w, attached);
        }
    }
    for (session, window) in &new.current {
        if old.current.get(session).is_some_and(|w| w != window) {
            out.push(format!("%session-window-changed {session} {window}"));
        }
    }
    for (id, w) in &new.windows {
        if !old.windows.contains_key(id) {
            adds(&mut out, id, &gained(id, w), w, attached);
        }
    }
    let ids = |s: &Snapshot| s.sessions.keys().cloned().collect::<Vec<_>>();
    if ids(old) != ids(new) {
        out.push("%sessions-changed".to_string());
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
    for (id, name) in &new.sessions {
        if old.sessions.get(id).is_some_and(|was| was != name) {
            out.push(format!("%session-renamed {id} {name}"));
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
    for (pane, mode) in &new.modes {
        if old.modes.get(pane).is_some_and(|was| was != mode) {
            out.push(format!("%pane-mode-changed {pane}"));
        }
    }
    for (name, session) in &new.clients {
        if old.clients.get(name) != Some(session) {
            out.push(format!("%client-session-changed {name} {session}"));
        }
    }
    for name in old.clients.keys() {
        if !new.clients.contains_key(name) {
            out.push(format!("%client-detached {name}"));
        }
    }
    // A buffer set again under its name is freed and added (`paste_set`).
    let names: BTreeSet<&String> = old.buffers.keys().chain(new.buffers.keys()).collect();
    for name in names {
        match (old.buffers.get(name), new.buffers.get(name)) {
            (Some(a), Some(b)) if a == b => {}
            (was, now) => {
                if was.is_some() {
                    out.push(format!("%paste-buffer-deleted {name}"));
                }
                if now.is_some() {
                    out.push(format!("%paste-buffer-changed {name}"));
                }
            }
        }
    }
    if new.sessions.contains_key(attached) {
        for (id, was) in &old.windows {
            let now = new.windows.get(id);
            let kind = if now.is_some_and(|w| w.sessions.contains(attached)) {
                "window-close"
            } else {
                "unlinked-window-close"
            };
            let left = was
                .sessions
                .iter()
                .filter(|s| now.is_none_or(|w| !w.sessions.contains(*s)));
            out.extend(left.map(|_| format!("%{kind} {id}")));
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

    /// Recorded on tmux 3.7b: `link-window` of a window of the attached
    /// session into `two`, `unlink-window` of that link, then `kill-window`.
    #[test]
    fn links_gained_and_lost_add_and_close() {
        let base = [
            "$0\t@0\t1\t%0\tL\t*\ta",
            "$0\t@2\t0\t%2\tL\t-\tc",
            "$1\t@1\t1\t%1\tL\t*\tb",
        ];
        let old = snap(&[("$0", "m"), ("$1", "two")], &base);
        let mut linked = base.to_vec();
        linked[2] = "$1\t@1\t0\t%1\tL\t-\tb";
        linked.push("$1\t@2\t1\t%2\tL\t*\tc");
        let linked = snap(&[("$0", "m"), ("$1", "two")], &linked);
        assert_eq!(
            diff(&old, &linked, "$0"),
            ["%window-add @2", "%session-window-changed $1 @2"]
        );
        assert_eq!(
            diff(&linked, &old, "$0"),
            ["%session-window-changed $1 @1", "%window-close @2"]
        );
        let killed = snap(
            &[("$0", "m"), ("$1", "two")],
            &["$0\t@0\t1\t%0\tL\t*\ta", "$1\t@1\t1\t%1\tL\t*\tb"],
        );
        assert_eq!(
            diff(&linked, &killed, "$0"),
            [
                "%session-window-changed $1 @1",
                "%unlinked-window-close @2",
                "%unlinked-window-close @2",
            ]
        );
    }

    #[test]
    fn modes_clients_and_buffers() {
        let mut old = snap(&[("$0", "m"), ("$1", "two")], &[]);
        old.modes.insert("%0".into(), String::new());
        old.clients.insert("/dev/pts/1".into(), "$1 two".into());
        old.clients.insert("/dev/pts/2".into(), "$1 two".into());
        old.buffers.insert("b1".into(), "5\t1\thello".into());
        old.buffers.insert("b2".into(), "1\t1\tx".into());
        let mut new = old.clone();
        new.modes.insert("%0".into(), "copy-mode".into());
        new.clients.insert("/dev/pts/1".into(), "$0 m".into());
        new.clients.remove("/dev/pts/2");
        new.buffers.insert("b1".into(), "5\t2\tworld".into());
        new.buffers.remove("b2");
        new.buffers.insert("buffer0".into(), "4\t2\tanon".into());
        assert_eq!(
            diff(&old, &new, "$0"),
            [
                "%pane-mode-changed %0",
                "%client-session-changed /dev/pts/1 $0 m",
                "%client-detached /dev/pts/2",
                "%paste-buffer-deleted b1",
                "%paste-buffer-changed b1",
                "%paste-buffer-deleted b2",
                "%paste-buffer-changed buffer0",
            ]
        );
    }

    #[test]
    fn any_session_rename_is_sent() {
        let old = snap(&[("$0", "m"), ("$1", "two")], &[]);
        let new = snap(&[("$0", "m"), ("$1", "deux")], &[]);
        assert_eq!(diff(&old, &new, "$0"), ["%session-renamed $1 deux"]);
    }

    #[test]
    fn a_killed_window_closes_unlinked_and_so_do_a_killed_sessions() {
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
        // Recorded: `kill-session` of another session closes its windows.
        assert_eq!(
            diff(&old, &no_session, "$0"),
            ["%sessions-changed", "%unlinked-window-close @2"]
        );
        // The client's own session went: it leaves, with no window notices.
        assert_eq!(diff(&old, &no_session, "$1"), ["%sessions-changed"]);
    }
}
