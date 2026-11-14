//! Pane resumes (spec §6, T4.2): the argv that brings a pane's process
//! back, bound to the process it was set for, and cleared when that
//! process is gone.
//!
//! A resume (or an expectation, `--expect`) is bound to one process: the
//! pane's foreground process group leader when `pane-set-resume` ran, or
//! the pane's root process for a spawn or respawn command
//! ([`bind_root`]). The binding is a pid plus that process's start time,
//! so a later process that reuses the pid does not match. A timer checks
//! the bindings once a [`SAMPLE`] while any exists and clears each whose
//! process has exited; a stopped process still exists and keeps its.
//!
//! `argv[0]` is resolved to an absolute path when it is set, never against
//! the server's own `PATH`: a server started at boot with a minimal `PATH`
//! can still run it. A caller with pane context (a CLI inside a pane) is
//! resolved against the `PATH` its hello carried; any other caller (a
//! plugin, a control client, a CLI outside) against the target pane: its
//! foreground process's own `PATH`, then the pane's spawn environment's.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::protocol::ClientCtx;
use crate::server::model::Pane;
use crate::server::pane::procinfo;
use crate::server::{Job, State, hooks};
use crate::types::PaneId;

/// How often the bindings are checked while any exists.
pub const SAMPLE: Duration = Duration::from_secs(1);

/// The process a resume or expectation belongs to: pid plus start time, so
/// a reused pid does not match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub pid: i32,
    pub start_time: u64,
}

impl Binding {
    /// Process `pid` as it is now; `None` if it has exited.
    pub fn of(pid: i32) -> Option<Binding> {
        Some(Binding {
            pid,
            start_time: procinfo::start_time(pid)?,
        })
    }

    /// Whether the process this binding names still exists (stopped
    /// counts; a zombie does not).
    pub fn alive(self) -> bool {
        procinfo::start_time(self.pid) == Some(self.start_time)
    }
}

/// Called after every spawn and respawn, the pane's new handle in place: a
/// resume from the spawn command binds to the pane's root process. Any
/// expectation was the old process's and goes.
///
/// A spawn command's bare `argv[0]` (`fake-agent`, as typed) is made
/// absolute here, against the `PATH` the root process was spawned with
/// (the pane's spawn environment, as the server gave it: the process's own
/// `/proc` environ can still read empty while it execs), so a restore under
/// a minimal `PATH` can still run it (spec §6). A one-word command is a script for the shell: it is
/// resolved only when it is a plain name (`htop`) found at a plain path, so
/// the script stays the same command; any other script is kept as it is, as
/// is a name the `PATH` does not hold.
pub fn bind_root(pane: &mut Pane) {
    pane.expect = None;
    let root = pane.handle.pid().and_then(|pid| i32::try_from(pid).ok());
    if let (Some(path), Some(resume)) = (pane.handle.spawn_path(), pane.resume.as_mut())
        && (resume.len() > 1 || plain_word(&resume[0]))
        && !resume[0].contains('/')
        && let Some(found) = lookup(&resume[0], path)
        && (resume.len() > 1 || plain_word(&found.display().to_string()))
    {
        resume[0] = found.display().to_string();
    }
    pane.resume_binding = match pane.resume {
        Some(_) => root.and_then(Binding::of),
        None => None,
    };
}

/// After [`bind_root`] at a spawn or respawn, with the actor's state:
/// raises `resume-changed` when the pane's resume or expectation differs
/// from `before` (a new pane had neither), and starts the sampling timer
/// for the new binding.
pub fn spawned(state: &mut State, pane: PaneId, before: (Option<Vec<String>>, Option<String>)) {
    let Some(p) = state.model.panes.get(&pane) else {
        return;
    };
    if (p.resume.as_ref(), p.expect.as_ref()) != (before.0.as_ref(), before.1.as_ref()) {
        raise_changed(state, pane);
    }
    watch(state);
}

/// `pane-set-resume`: what to record for the target pane.
pub enum Set {
    Resume(Vec<String>),
    Expect(String),
    Clear,
}

/// Sets, or with [`Set::Clear`] clears, `pane`'s resume or expectation,
/// for the caller `ctx`. A resume's `argv[0]` is resolved first; when it
/// cannot be, nothing changes.
pub fn set(state: &mut State, ctx: &ClientCtx, pane: PaneId, what: Set) -> Result<(), String> {
    let p = state
        .model
        .panes
        .get(&pane)
        .ok_or_else(|| format!("can't find pane: {pane}"))?;
    let caller = state.cli_io.as_ref().and_then(|io| io.pid);
    let (resume, expect, binding) = match what {
        Set::Clear => (None, None, None),
        Set::Resume(argv) => {
            let (leader, binding) = foreground(p, caller)?;
            let argv = resolve(state, ctx, p, leader, argv)?;
            (Some(argv), None, Some(binding))
        }
        Set::Expect(reason) => {
            let (_, binding) = foreground(p, caller)?;
            (None, Some(reason), Some(binding))
        }
    };
    update(state, pane, resume, expect, binding);
    watch(state);
    Ok(())
}

/// The pane's foreground process group leader, and the binding for it.
/// When that leader is the very CLI asking, `caller` (run from an
/// interactive shell, which gives each command its own group), the binding
/// is its parent instead, the shell that ran it: the CLI is gone the
/// moment it answers.
fn foreground(p: &Pane, caller: Option<i32>) -> Result<(i32, Binding), String> {
    if p.dead.is_some() {
        return Err(format!("pane {} is dead", p.id));
    }
    let leader = p
        .handle
        .foreground_group()
        .ok_or_else(|| format!("pane {} has no foreground process", p.id))?;
    let bound = if caller == Some(leader) {
        procinfo::parent(leader).unwrap_or(leader)
    } else {
        leader
    };
    let binding = Binding::of(bound)
        .ok_or_else(|| format!("pane {}: its foreground process has exited", p.id))?;
    Ok((leader, binding))
}

/// `argv` with `argv[0]` made absolute (see the module docs).
fn resolve(
    state: &State,
    ctx: &ClientCtx,
    p: &Pane,
    leader: i32,
    mut argv: Vec<String>,
) -> Result<Vec<String>, String> {
    let Some(name) = argv.first().cloned() else {
        return Err("pane-set-resume: an empty argv".to_string());
    };
    let found = if name.contains('/') {
        let base = match ctx.pane {
            Some(_) => ctx.cwd.clone(),
            None => p.handle.current_path().unwrap_or_else(|| p.cwd.clone()),
        };
        let path = base.join(&name);
        executable(&path).then_some(path)
    } else {
        search_paths(state, ctx, p, leader)
            .iter()
            .find_map(|path| lookup(&name, path))
    };
    let found = found.ok_or_else(|| match ctx.pane {
        Some(_) => format!("pane-set-resume: {name}: not found on the caller's PATH"),
        None => format!(
            "pane-set-resume: {name}: not found on the PATH of pane {}'s foreground process or its spawn environment",
            p.id
        ),
    })?;
    argv[0] = found.display().to_string();
    Ok(argv)
}

/// The `PATH`s to search, in order.
fn search_paths(state: &State, ctx: &ClientCtx, p: &Pane, leader: i32) -> Vec<String> {
    if ctx.pane.is_some() {
        return ctx
            .env
            .iter()
            .filter(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .collect();
    }
    let session = state.model.windows.get(&p.window).map(|w| w.session);
    // The spawn environment: what the server gave the root process, not
    // its environ read back (macOS 27 hides that of Apple's binaries, the
    // stock shells among them), else what a pane of its session would get
    // now.
    let spawn = p.handle.spawn_path().map(str::to_string).or_else(|| {
        let s = session?;
        let own = state.model.sessions.get(&s)?.env.clone();
        own.into_iter()
            .chain(hooks::pane_env(state, s))
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v)
    });
    procinfo::env_var(leader, "PATH")
        .into_iter()
        .chain(spawn)
        .collect()
}

/// A shell word that means the same quoted or not: nothing the shell
/// would split, expand or treat as syntax.
fn plain_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-,@%:".contains(c))
}

/// `name` in the directories of `path`; relative entries (an empty one is
/// the current directory) are skipped, so the answer is always absolute.
fn lookup(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .map(Path::new)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| executable(candidate))
}

fn executable(path: &Path) -> bool {
    path.is_absolute()
        && std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Records the new state, raising `resume-changed` when it differs.
fn update(
    state: &mut State,
    pane: PaneId,
    resume: Option<Vec<String>>,
    expect: Option<String>,
    binding: Option<Binding>,
) {
    let Some(p) = state.model.panes.get_mut(&pane) else {
        return;
    };
    let changed = p.resume != resume || p.expect != expect;
    p.resume = resume;
    p.expect = expect;
    p.resume_binding = binding;
    if changed {
        raise_changed(state, pane);
    }
}

/// The one place `resume-changed` is raised from: every set and clear
/// here, a clear for a gone process included, passes through it.
fn raise_changed(state: &mut State, pane: PaneId) {
    hooks::resume_changed(state, pane);
}

/// Clears every binding whose process is gone; whether any remain.
pub fn sweep(state: &mut State) -> bool {
    let gone: Vec<PaneId> = state
        .model
        .panes
        .values()
        .filter(|p| p.resume_binding.is_some_and(|b| !b.alive()))
        .map(|p| p.id)
        .collect();
    for pane in gone {
        update(state, pane, None, None, None);
    }
    state
        .model
        .panes
        .values()
        .any(|p| p.resume_binding.is_some())
}

/// `pane`'s process exited: its binding is checked now rather than at the
/// next sample. For the actor's exit handling.
pub fn on_pane_exited(state: &mut State, pane: PaneId) {
    let dead = state
        .model
        .panes
        .get(&pane)
        .and_then(|p| p.resume_binding)
        .is_some_and(|b| !b.alive());
    if dead {
        update(state, pane, None, None, None);
    }
}

/// The sampling timer, while one runs.
// ponytail: one server per process, so one timer per process; `State`
// has no field for it (rule 4: the stub edit adds fields, not T4.2).
static TIMER: Mutex<Option<tokio::task::AbortHandle>> = Mutex::new(None);

/// Starts the sampling timer unless it runs. It stops itself once no
/// binding remains, so an idle server wakes for nothing.
pub fn watch(state: &State) {
    let mut timer = TIMER.lock().unwrap_or_else(PoisonError::into_inner);
    if timer.as_ref().is_some_and(|t| !t.is_finished()) {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let jobs = state.jobs.clone();
    let task = rt.spawn(async move {
        loop {
            tokio::time::sleep(SAMPLE).await;
            let job: Job = Box::new(|state: &mut State| {
                if !sweep(state) {
                    stop();
                }
            });
            if jobs.send(job).await.is_err() {
                break;
            }
        }
    });
    *timer = Some(task.abort_handle());
}

fn stop() {
    let mut timer = TIMER.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(t) = timer.take() {
        t.abort();
    }
}
