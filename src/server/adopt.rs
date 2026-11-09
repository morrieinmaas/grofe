//! Starting from tmux's sessions (`start-from-tmux`): a server that starts
//! empty takes its sessions from tmux, so moving to grofe keeps the work
//! that is open there. The result is a [`Snapshot`] the normal restore
//! rebuilds ([`crate::server::restore::restore`]).
//!
//! - **A running tmux server** ([`from_live_tmux`]): its sessions,
//!   windows, layouts and directories, read once with `list-panes -a`.
//!   Its programs keep running in tmux, so grofe starts shells only: a
//!   second `claude --resume` of the same conversation, or a second editor
//!   on the same file, would fight the first.
//! - **Else tmux-resurrect's newest save** ([`from_resurrect`]), as
//!   resurrect writes it (layouts as tmux layout strings) or as canopy's
//!   save strategy does (layouts as JSON). Nothing runs those programs
//!   any more, so a pane saved with a command runs it again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use ulid::Ulid;

use crate::server::layout::{GeometryCell, LayoutGeometry, parse_tmux_layout};
use crate::server::snapshot::{PaneSnap, SNAPSHOT_VERSION, SessionSnap, Snapshot, WindowSnap};
use crate::types::{Dir, PaneId};

/// The `list-panes -a` format: one tab-separated line per pane. The first
/// field is `#{grofe_version}`, empty only on a real tmux, so a `tmux` on
/// `PATH` that is grofe itself is never taken for tmux.
const LIVE_FORMAT: &str = "#{grofe_version}\t#{session_name}\t#{session_path}\t\
    #{window_index}\t#{window_name}\t#{window_active}\t#{window_layout}\t\
    #{window_zoomed_flag}\t#{?automatic-rename,1,0}\t#{pane_id}\t#{pane_active}\t\
    #{pane_current_path}";

/// The sessions of the tmux server `tmux` (from `PATH`) reaches with no
/// `$TMUX`: its default socket under `$TMUX_TMPDIR`. `Err` when none
/// runs, it has no session, or `tmux` is grofe.
pub fn from_live_tmux() -> Result<Snapshot, String> {
    let out = Command::new("tmux")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .args(["list-panes", "-a", "-F", LIVE_FORMAT])
        .output()
        .map_err(|e| format!("tmux: {e}"))?;
    if !out.status.success() {
        return Err("no tmux server running".to_string());
    }
    parse_live(&String::from_utf8_lossy(&out.stdout))
}

#[derive(Default)]
struct LiveWindow {
    index: u32,
    name: String,
    active: bool,
    layout: String,
    zoomed: bool,
    automatic_rename: bool,
    /// `(pane_id without %, active, cwd)` in `list-panes` order.
    panes: Vec<(String, bool, PathBuf)>,
}

fn parse_live(text: &str) -> Result<Snapshot, String> {
    // Sessions in tmux's order, each with its windows in order.
    let mut sessions: Vec<(String, PathBuf, Vec<LiveWindow>)> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [
            grofe,
            sess,
            path,
            widx,
            wname,
            wact,
            layout,
            zoom,
            auto,
            pid,
            pact,
            cwd,
        ] = f[..]
        else {
            continue;
        };
        if !grofe.is_empty() {
            return Err("the tmux on PATH is grofe".to_string());
        }
        if sessions.last().is_none_or(|(n, _, _)| n != sess) {
            sessions.push((sess.to_string(), PathBuf::from(path), Vec::new()));
        }
        let Some((_, _, windows)) = sessions.last_mut() else {
            continue;
        };
        let index: u32 = widx.parse().unwrap_or(0);
        if windows.last().is_none_or(|w| w.index != index) {
            windows.push(LiveWindow {
                index,
                name: wname.to_string(),
                active: wact == "1",
                layout: layout.to_string(),
                zoomed: zoom == "1",
                automatic_rename: auto == "1",
                panes: Vec::new(),
            });
        }
        if let Some(w) = windows.last_mut() {
            let id = pid.trim_start_matches('%').to_string();
            w.panes.push((id, pact == "1", PathBuf::from(cwd)));
        }
    }
    if sessions.is_empty() {
        return Err("tmux has no sessions".to_string());
    }
    let sessions = sessions
        .into_iter()
        .map(|(name, cwd, windows)| {
            let active_window = windows
                .iter()
                .find(|w| w.active)
                .or(windows.first())
                .map_or(0, |w| w.index);
            let windows = windows.into_iter().filter_map(live_window).collect();
            SessionSnap {
                name,
                cwd,
                active_window,
                options: Vec::new(),
                windows,
            }
        })
        .collect();
    Ok(snapshot(sessions))
}

/// One live window as a snapshot window, its layout's cells keyed by
/// fresh uids through the pane ids the layout string names.
fn live_window(w: LiveWindow) -> Option<WindowSnap> {
    let uids: BTreeMap<String, String> = w
        .panes
        .iter()
        .map(|(id, _, _)| (id.clone(), Ulid::generate().to_string()))
        .collect();
    let layout = if w.layout.starts_with('{') {
        // A JSON layout names each pane's id (`"I":"%3"`).
        json_layout(&w.layout, &|n| {
            let id = n.get("I")?.as_str()?.trim_start_matches('%');
            uids.get(id).cloned()
        })?
    } else {
        let order = layout_pane_ids(&w.layout);
        let geometry = parse_tmux_layout(&w.layout)?;
        keyed(geometry, &|PaneId(n)| {
            order
                .get(n as usize)
                .and_then(|id| uids.get(id))
                .cloned()
                .unwrap_or_default()
        })
    };
    let active_pane = w
        .panes
        .iter()
        .find(|(_, active, _)| *active)
        .or(w.panes.first())
        .and_then(|(id, _, _)| uids.get(id).cloned())
        .unwrap_or_default();
    let panes = w
        .panes
        .iter()
        .map(|(id, _, cwd)| PaneSnap {
            uid: uids.get(id).cloned().unwrap_or_default(),
            cwd: cwd.clone(),
            argv: None,
            resume: None,
            options: Vec::new(),
        })
        .collect();
    Some(WindowSnap {
        index: w.index,
        name: w.name,
        automatic_rename: w.automatic_rename,
        cols: layout.cols,
        rows: layout.rows,
        layout,
        active_pane,
        zoomed: w.zoomed,
        options: Vec::new(),
        panes,
    })
}

/// The pane ids a tmux layout string's cells name, in cell order: each
/// `WxH,X,Y` followed by `,ID` is a pane (one followed by `{` or `[` is a
/// split).
fn layout_pane_ids(layout: &str) -> Vec<String> {
    let body = layout.split_once(',').map_or("", |(_, b)| b).as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let num = |i: &mut usize| {
        let start = *i;
        while *i < body.len() && body[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > start
    };
    while i < body.len() {
        let start = i;
        // WxH,X,Y
        if num(&mut i)
            && body.get(i) == Some(&b'x')
            && {
                i += 1;
                num(&mut i)
            }
            && body.get(i) == Some(&b',')
            && {
                i += 1;
                num(&mut i)
            }
            && body.get(i) == Some(&b',')
            && {
                i += 1;
                num(&mut i)
            }
        {
            if body.get(i) == Some(&b',') {
                let id_start = i + 1;
                i += 1;
                if num(&mut i) {
                    out.push(String::from_utf8_lossy(&body[id_start..i]).into_owned());
                }
            }
            continue;
        }
        i = start + 1;
    }
    out
}

/// `g` with its cells keyed by `uid` of their pane.
fn keyed(g: LayoutGeometry, uid: &impl Fn(PaneId) -> String) -> LayoutGeometry<String> {
    let cell = match g.cell {
        GeometryCell::Pane(id) => GeometryCell::Pane(uid(id)),
        GeometryCell::Split { dir, children } => GeometryCell::Split {
            dir,
            children: children.into_iter().map(|c| keyed(c, uid)).collect(),
        },
    };
    LayoutGeometry {
        x: g.x,
        y: g.y,
        cols: g.cols,
        rows: g.rows,
        cell,
    }
}

fn snapshot(sessions: Vec<SessionSnap>) -> Snapshot {
    Snapshot {
        version: SNAPSHOT_VERSION,
        written_at: String::new(),
        grofe_version: env!("CARGO_PKG_VERSION").to_string(),
        options: Vec::new(),
        sessions,
    }
}

// ---- tmux-resurrect's save ----

/// tmux-resurrect's newest save in `dir`, else its default directories
/// under `home` (`~/.local/share/tmux/resurrect`, `~/.tmux/resurrect`):
/// the file its `last` link names.
pub fn resurrect_last(dir: Option<&Path>, home: &Path) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = match dir {
        Some(d) => vec![d.to_path_buf()],
        None => vec![
            home.join(".local/share/tmux/resurrect"),
            home.join(".tmux/resurrect"),
        ],
    };
    dirs.into_iter()
        .map(|d| d.join("last"))
        .find(|p| p.is_file())
}

/// The sessions in a resurrect save's text: `pane` and `window` lines,
/// tab-separated, a `:` in front of fields that may be empty.
pub fn from_resurrect(text: &str) -> Result<Snapshot, String> {
    struct SavedPane {
        index: u32,
        active: bool,
        cwd: PathBuf,
        command: String,
        full: String,
    }
    struct SavedWindow {
        index: u32,
        name: String,
        active: bool,
        layout: String,
        automatic_rename: bool,
        zoomed: bool,
        panes: Vec<SavedPane>,
    }
    let field = |s: &str| s.strip_prefix(':').unwrap_or(s).to_string();
    let mut sessions: Vec<(String, Vec<SavedWindow>)> = Vec::new();
    let mut panes: BTreeMap<(String, u32), Vec<SavedPane>> = BTreeMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.first() {
            Some(&"pane") if f.len() >= 11 => {
                let key = (f[1].to_string(), f[2].parse().unwrap_or(0));
                panes.entry(key).or_default().push(SavedPane {
                    index: f[5].parse().unwrap_or(0),
                    active: f[8] == "1",
                    cwd: PathBuf::from(field(f[7])),
                    command: f[9].to_string(),
                    full: field(f[10]),
                });
            }
            Some(&"window") if f.len() >= 7 => {
                let sess = f[1].to_string();
                if !sessions.iter().any(|(n, _)| *n == sess) {
                    sessions.push((sess.clone(), Vec::new()));
                }
                let flags = field(f[5]);
                let auto = f.get(7).map(|a| field(a)).unwrap_or_default();
                if let Some((_, windows)) = sessions.iter_mut().find(|(n, _)| *n == sess) {
                    windows.push(SavedWindow {
                        index: f[2].parse().unwrap_or(0),
                        name: field(f[3]),
                        active: f[4] == "1",
                        layout: f[6].to_string(),
                        automatic_rename: auto == "on",
                        zoomed: flags.contains('Z'),
                        panes: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }
    for (sess, windows) in &mut sessions {
        for w in windows.iter_mut() {
            let mut ps = panes.remove(&(sess.clone(), w.index)).unwrap_or_default();
            ps.sort_by_key(|p| p.index);
            w.panes = ps;
        }
    }
    let sessions: Vec<SessionSnap> = sessions
        .into_iter()
        .map(|(name, windows)| {
            let active_window = windows
                .iter()
                .find(|w| w.active)
                .or(windows.first())
                .map_or(0, |w| w.index);
            let cwd = windows
                .iter()
                .find(|w| w.active)
                .and_then(|w| w.panes.iter().find(|p| p.active))
                .map(|p| p.cwd.clone())
                .unwrap_or_default();
            let windows = windows
                .into_iter()
                .filter(|w| !w.panes.is_empty())
                .filter_map(|w| {
                    let uids: Vec<String> = w
                        .panes
                        .iter()
                        .map(|_| Ulid::generate().to_string())
                        .collect();
                    let by_index: BTreeMap<u32, String> = w
                        .panes
                        .iter()
                        .zip(&uids)
                        .map(|(p, u)| (p.index, u.clone()))
                        .collect();
                    let layout = if w.layout.starts_with('{') {
                        json_layout(&w.layout, &|n| {
                            let index = u32::try_from(n.get("i")?.as_u64()?).ok()?;
                            by_index.get(&index).cloned()
                        })?
                    } else {
                        let g = parse_tmux_layout(&w.layout)?;
                        keyed(g, &|PaneId(n)| {
                            uids.get(n as usize).cloned().unwrap_or_default()
                        })
                    };
                    let active_pane = w
                        .panes
                        .iter()
                        .zip(&uids)
                        .find(|(p, _)| p.active)
                        .or(w.panes.iter().zip(&uids).next())
                        .map(|(_, u)| u.clone())
                        .unwrap_or_default();
                    let panes = w
                        .panes
                        .iter()
                        .zip(uids)
                        .map(|(p, uid)| PaneSnap {
                            uid,
                            cwd: p.cwd.clone(),
                            argv: None,
                            resume: saved_command(&p.command, &p.full),
                            options: Vec::new(),
                        })
                        .collect();
                    Some(WindowSnap {
                        index: w.index,
                        name: w.name,
                        automatic_rename: w.automatic_rename,
                        cols: layout.cols,
                        rows: layout.rows,
                        layout,
                        active_pane,
                        zoomed: w.zoomed,
                        options: Vec::new(),
                        panes,
                    })
                })
                .collect();
            SessionSnap {
                name,
                cwd,
                active_window,
                options: Vec::new(),
                windows,
            }
        })
        .collect();
    if sessions.is_empty() {
        return Err("the save has no sessions".to_string());
    }
    Ok(snapshot(sessions))
}

/// What a saved pane runs again: its full command, through `sh -c`, when
/// one was saved for a program that is not a shell.
// ponytail: resurrect's `@resurrect-processes` list is not read; a pane
// with a saved command for any program runs it.
fn saved_command(command: &str, full: &str) -> Option<Vec<String>> {
    const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "nu"];
    if full.trim().is_empty() || SHELLS.contains(&command) {
        return None;
    }
    Some(vec!["sh".to_string(), "-c".to_string(), full.to_string()])
}

/// The uid of the pane a JSON layout's pane node is.
type PaneKey<'a> = dyn Fn(&serde_json::Value) -> Option<String> + 'a;

/// tmux's JSON layout (`{"V":2,"L":node}`, a node `{"t":"h"|"v"|"p",
/// "w","h","x","y","c":[...],"i":pane_index,"I":"%id"}`), as newer tmux
/// prints `#{window_layout}` and resurrect saves it, each pane cell keyed
/// by `uid`; `h` puts its children side by side.
fn json_layout(text: &str, uid: &PaneKey) -> Option<LayoutGeometry<String>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    fn node(n: &serde_json::Value, uid: &PaneKey) -> Option<LayoutGeometry<String>> {
        let num = |k: &str| {
            n.get(k)
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok())
        };
        let cell = match n.get("t")?.as_str()? {
            "p" => GeometryCell::Pane(uid(n)?),
            t => GeometryCell::Split {
                dir: if t == "h" {
                    Dir::Horizontal
                } else {
                    Dir::Vertical
                },
                children: n
                    .get("c")?
                    .as_array()?
                    .iter()
                    .map(|c| node(c, uid))
                    .collect::<Option<Vec<_>>>()?,
            },
        };
        Some(LayoutGeometry {
            x: num("x")?,
            y: num("y")?,
            cols: num("w")?,
            rows: num("h")?,
            cell,
        })
    }
    node(v.get("L")?, uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_pane_ids_are_in_cell_order() {
        assert_eq!(
            layout_pane_ids(
                "b25d,169x36,0,0{84x36,0,0,13,84x36,85,0[84x18,85,0,15,84x17,85,19,2]}"
            ),
            ["13", "15", "2"]
        );
        assert_eq!(layout_pane_ids("1234,80x24,0,0,7"), ["7"]);
    }

    #[test]
    fn a_grofe_answering_as_tmux_is_not_taken_for_tmux() {
        let line = "0.1.0\tmain\t/\t0\tzsh\t1\tbad\t0\t1\t%0\t1\t/";
        assert!(parse_live(line).is_err());
    }

    #[test]
    fn a_canopy_save_restores_layout_cwds_and_saved_commands() {
        let save = "pane\t10\t1\t1\t:*\t1\ttitle\t:/work/a\t1\tclaude\t:claude --resume abc\n\
            pane\t10\t1\t1\t:*\t2\tx\t:/work/b\t0\tzsh\t:\n\
            window\t10\t1\t:grofe\t1\t:*\t{\"V\":2,\"L\":{\"t\":\"h\",\"w\":169,\"h\":36,\"x\":0,\"y\":0,\"c\":[{\"t\":\"p\",\"w\":84,\"h\":36,\"x\":0,\"y\":0,\"i\":1},{\"t\":\"p\",\"w\":84,\"h\":36,\"x\":85,\"y\":0,\"i\":2}]}}\t:\n\
            state\t10\t10\n";
        let snap = from_resurrect(save).expect("parses");
        let s = &snap.sessions[0];
        assert_eq!(s.name, "10");
        let w = &s.windows[0];
        assert_eq!((w.name.as_str(), w.cols, w.rows), ("grofe", 169, 36));
        assert_eq!(w.panes[0].cwd, PathBuf::from("/work/a"));
        assert_eq!(
            w.panes[0].resume.as_deref(),
            Some(
                &[
                    "sh".to_string(),
                    "-c".to_string(),
                    "claude --resume abc".to_string()
                ][..]
            )
        );
        assert_eq!(w.panes[1].resume, None, "a shell runs no saved command");
        assert_eq!(w.active_pane, w.panes[0].uid);
        let GeometryCell::Split { dir, children } = &w.layout.cell else {
            panic!("a split: {:?}", w.layout);
        };
        assert_eq!(*dir, Dir::Horizontal);
        assert_eq!(children[1].cell, GeometryCell::Pane(w.panes[1].uid.clone()));
    }
}
