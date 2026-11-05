//! `pane-set-resume` (T4.2) and `reboot-check` (T4.5), spec §6.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cmd::{PaneSetResume, RebootCheck, Resolve};
use crate::paths::{self, Env};
use crate::protocol::{ClientCtx, Failure};
use crate::server::State;
use crate::server::dispatch::defer_reply;
use crate::server::resume::{self, Set};
use crate::server::snapshot::{self, SNAPSHOT_VERSION, Snapshot};

/// `pane-set-resume [-t pane] [-u] [--expect <reason>] [-- argv...]`: the
/// logic is `server::resume`'s.
pub fn pane_set_resume(
    state: &mut State,
    ctx: &ClientCtx,
    c: PaneSetResume,
) -> Result<String, String> {
    let pane = state.model.resolve_pane(&c.target, ctx)?;
    let what = match (c.unset, c.expect) {
        (true, _) => Set::Clear,
        (false, Some(reason)) => Set::Expect(reason),
        (false, None) => Set::Resume(c.argv),
    };
    resume::set(state, ctx, pane, what)?;
    Ok(String::new())
}

// ---- reboot-check ----

/// One pane's line of the report.
#[derive(Debug, Clone, Serialize)]
struct Entry {
    /// `<session>:<window>.<pane>`.
    target: String,
    uid: String,
    verdict: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

const WILL_RESUME: &str = "will resume";
const WITHOUT: &str = "will restart without its conversation";
const NOT_SAVED: &str = "not saved yet";
const NOT_RESTORED: &str = "will not be restored";
const SHELL: &str = "restores as a shell";

/// The snapshot as `reboot-check` sees it on disk.
enum Disk {
    Missing,
    Unreadable(String),
    Read(Box<Snapshot>),
}

/// Reads `path` without [`crate::server::snapshot::read`]'s renames: a
/// check changes nothing.
fn read_disk(path: &Path) -> Disk {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Disk::Missing,
        Err(e) => return Disk::Unreadable(format!("cannot read the snapshot: {e}")),
    };
    let snap: Snapshot = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(_) => return Disk::Unreadable("the snapshot does not parse".to_string()),
    };
    if snap.version != SNAPSHOT_VERSION {
        return Disk::Unreadable(format!(
            "the snapshot is version {}, not {SNAPSHOT_VERSION}",
            snap.version
        ));
    }
    Disk::Read(Box::new(snap))
}

/// `reboot-check [--json]` (spec §6): every pane's verdict from the
/// snapshot on disk compared with the live model, one line each
/// (`<session>:<window>.<pane> <uid> <verdict> [(<reason>)]`) or a JSON
/// array. The exit status is 0 when every pane that expects a resume will
/// resume, 1 when one will not, 2 when persistence is misconfigured.
pub fn reboot_check(state: &mut State, _ctx: &ClientCtx, c: RebootCheck) -> Result<String, String> {
    let env = Env::from_process();
    let path = paths::snapshot_path(&env, &state.socket);
    let mut problems = Vec::new();
    let restore_on = state
        .model
        .options
        .lookup("restore-on-start", &[])
        .is_none_or(|v| v != "off");
    let restore = restore_on || autostart_unit(&env);
    if !restore {
        problems.push(
            "restore-on-start is off and no autostart unit runs start-server --restore".to_string(),
        );
    }
    if let Some(dir) = path.parent()
        && let Some(why) = unwritable(dir)
    {
        problems.push(why);
    }
    let entries = verdicts(state, &read_disk(&path), restore);
    let out = if c.json {
        let mut text = serde_json::to_string_pretty(&entries).map_err(|e| e.to_string())?;
        text.push('\n');
        text
    } else {
        entries
            .iter()
            .map(|e| match &e.reason {
                Some(r) => format!("{} {} {} ({r})\n", e.target, e.uid, e.verdict),
                None => format!("{} {} {}\n", e.target, e.uid, e.verdict),
            })
            .collect()
    };
    let status = if !problems.is_empty() {
        2
    } else if entries
        .iter()
        .any(|e| e.verdict != WILL_RESUME && e.verdict != SHELL)
    {
        1
    } else {
        0
    };
    if status == 0 {
        return Ok(out);
    }
    let err = problems
        .iter()
        .map(|p| format!("reboot-check: {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    let _ = defer_reply(state).send(Err(Failure {
        err,
        out,
        status: Some(status),
    }));
    Ok(String::new())
}

/// Each live pane's verdict, sessions by name, windows and panes in order.
fn verdicts(state: &State, disk: &Disk, restore: bool) -> Vec<Entry> {
    let model = &state.model;
    let saved: HashMap<&str, &Option<Vec<String>>> = match disk {
        Disk::Read(snap) => snap
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.panes)
            .map(|p| (p.uid.as_str(), &p.resume))
            .collect(),
        _ => HashMap::new(),
    };
    // A pane whose saved resume differs from its live one is "not saved
    // yet" only while the writer still has the change to write; otherwise
    // the disk itself disagrees (edited, or the writer frozen).
    let pending = snapshot::pending_panes(model);
    let mut sessions: Vec<_> = model.sessions.values().collect();
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = Vec::new();
    for s in sessions {
        for wid in &s.windows {
            let Some(w) = model.windows.get(wid) else {
                continue;
            };
            let window_index = model.index_in(s.id, w.id).unwrap_or(w.index);
            let base = model.pane_base_index(w.id);
            for (at, pid) in w.pane_ids().iter().enumerate() {
                let Some(p) = model.panes.get(pid) else {
                    continue;
                };
                let (verdict, reason) = if !restore {
                    (NOT_RESTORED, None)
                } else if let Some(expect) = &p.expect {
                    // Expectations are never saved: a restore can only
                    // start a shell.
                    (WITHOUT, Some(expect.clone()))
                } else if let Some(resume) = &p.resume {
                    match (disk, saved.get(p.uid.as_str())) {
                        (Disk::Missing, _) => (NOT_SAVED, Some("no snapshot".to_string())),
                        (Disk::Unreadable(why), _) => (NOT_SAVED, Some(why.clone())),
                        (_, None) => (NOT_SAVED, Some("not in the snapshot".to_string())),
                        (_, Some(Some(r))) if r == resume => (WILL_RESUME, None),
                        _ if pending.contains(&p.uid) => {
                            (NOT_SAVED, Some("changed since the last write".to_string()))
                        }
                        (_, Some(Some(_))) => (
                            WITHOUT,
                            Some("the snapshot holds an older resume".to_string()),
                        ),
                        (_, Some(None)) => {
                            (WITHOUT, Some("the snapshot holds no resume".to_string()))
                        }
                    }
                } else {
                    (SHELL, None)
                };
                out.push(Entry {
                    target: format!("{}:{window_index}.{}", s.name, at as u32 + base),
                    uid: p.uid.clone(),
                    verdict,
                    reason,
                });
            }
        }
    }
    out
}

/// `None` when the state directory `dir` (or, before the writer made it,
/// its nearest existing ancestor) is writable, else why not.
fn unwritable(dir: &Path) -> Option<String> {
    use nix::unistd::{AccessFlags, access};
    let mut at = dir;
    while !at.exists() {
        at = at.parent()?;
    }
    match access(at, AccessFlags::W_OK | AccessFlags::X_OK) {
        Ok(()) => None,
        Err(e) => Some(format!(
            "the state directory {} is not writable: {e}",
            at.display()
        )),
    }
}

/// Whether a login autostart unit runs `start-server --restore`: a
/// systemd user unit or a launchd agent.
// ponytail: whether the unit is enabled (or loaded) is not checked, only
// that one exists whose command restores.
fn autostart_unit(env: &Env) -> bool {
    let home = env
        .home
        .as_deref()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    let mut systemd = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        systemd.push(Path::new(&xdg).join("systemd/user"));
    }
    if let Some(home) = &home {
        systemd.push(home.join(".config/systemd/user"));
    }
    let launchd = home.map(|h| h.join("Library/LaunchAgents"));
    systemd.iter().any(|d| {
        unit_files(d).any(|text| {
            text.lines().any(|l| {
                let l = l.trim_start();
                (l.starts_with("ExecStart=") || l.starts_with("ExecStart ="))
                    && restores(l.split_whitespace())
            })
        })
    }) || launchd.is_some_and(|d| {
        unit_files(&d).any(|text| {
            // Each `<string>` of `ProgramArguments`, or one `sh -c` string.
            restores(text.split(|c: char| c.is_whitespace() || c == '<' || c == '>'))
        })
    })
}

/// Whether `words` hold `start-server` followed later by `--restore`.
fn restores<'a>(mut words: impl Iterator<Item = &'a str>) -> bool {
    words.any(|w| w == "start-server") && words.any(|w| w == "--restore")
}

/// The text of each small regular file in `dir`.
fn unit_files(dir: &Path) -> impl Iterator<Item = String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let path = e.ok()?.path();
            let meta = fs::metadata(&path).ok()?;
            (meta.is_file() && meta.len() <= 64 * 1024).then_some(path)
        })
        .filter_map(|p| fs::read_to_string(p).ok())
}
