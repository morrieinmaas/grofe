//! Plugin supervision (T6.3, spec §7.4): `[plugins.<name>]` processes the
//! server starts, restarts and stops.
//!
//! Each enabled plugin has one supervisor task, off the actor, that spawns
//! it under the pane spawn lock in its own process group with stdin from
//! `/dev/null` and stdout and stderr through one pipe the task copies to
//! `<state>/plugins/<name>.log` (rotated at 1 MiB). The task owns the
//! restart policy: backoff from 1 s doubling to 60 s, reset after 60 s
//! alive, and five crashes within 60 s disable the plugin. What
//! `list-plugins` shows is a [`Status`] the task updates; the actor only
//! creates, replaces and stops tasks, so a plugin never delays a command.
//!
//! A plugin is stopped as the server exits does: SIGTERM to its group,
//! SIGKILL once [`Timing::grace`] has passed. Once a plugin's leader exits
//! by itself the rest of its group is killed, so no plugin leaves a stray
//! child behind.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::libc;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::compat::Identity;
use crate::config::{ConfigPlugin, Resolved, Restart};
use crate::paths::{self, Env};
use crate::server::{State, hooks, pane};

/// A plugin's log is rotated to `<name>.log.1` once it would pass this.
const LOG_LIMIT: u64 = 1024 * 1024;

/// Crashes within [`Timing::window`] that disable a plugin.
const MAX_CRASHES: usize = 5;

/// The durations supervision runs on. A server reads
/// [`Plugins::TIME_SCALE_ENV`] so tests can run the real policy in a
/// fraction of the time; every duration scales by it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Timing {
    /// The first restart delay.
    initial: Duration,
    /// The longest restart delay.
    max: Duration,
    /// A run this long resets the delay to `initial`.
    reset_after: Duration,
    /// The span five crashes must fall in to disable a plugin.
    window: Duration,
    /// SIGTERM to SIGKILL.
    grace: Duration,
}

impl Timing {
    fn scaled(scale: f64) -> Timing {
        let secs = |s: f64| Duration::from_secs_f64(s * scale);
        Timing {
            initial: secs(1.0),
            max: secs(60.0),
            reset_after: secs(60.0),
            window: secs(60.0),
            grace: secs(2.0),
        }
    }

    fn from_env() -> Timing {
        let scale = std::env::var(Plugins::TIME_SCALE_ENV)
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s > 0.0 && *s <= 1.0)
            .unwrap_or(1.0);
        Timing::scaled(scale)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    /// Waiting to start or to restart.
    Backoff,
    /// Its restart policy leaves it down.
    Exited,
    /// Five crashes within the window.
    Disabled,
    /// `enabled = false`.
    Off,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Running => "running",
            Phase::Backoff => "backoff",
            Phase::Exited => "exited",
            Phase::Disabled => "disabled",
            Phase::Off => "off",
        }
    }
}

/// What `list-plugins` shows; the supervisor task keeps it current.
#[derive(Debug, Clone, Copy)]
struct Status {
    /// Which supervisor task may change this status; stopping or
    /// relaunching a plugin moves it on, so a task being stopped cannot
    /// overwrite what the actor set after it.
    generation: u64,
    phase: Phase,
    pid: Option<u32>,
    restarts: u32,
    since: SystemTime,
}

type SharedStatus = Arc<Mutex<Status>>;

fn set(status: &SharedStatus, f: impl FnOnce(&mut Status)) {
    let mut s = status.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut s);
}

fn enter(status: &SharedStatus, phase: Phase, pid: Option<u32>) {
    set(status, |s| {
        s.phase = phase;
        s.pid = pid;
        s.since = SystemTime::now();
    });
}

/// The plugins of a server. Lives on `State.plugins`.
pub struct Plugins {
    /// The merged config's plugins, as of the last config application.
    desired: BTreeMap<String, ConfigPlugin>,
    /// Plugins start once the server has applied its config (and, from M4,
    /// restored); a config applied before that only records `desired`.
    started: bool,
    entries: BTreeMap<String, Entry>,
    /// Plugins dropped from the config, whose process may still be going
    /// down: a plugin of the same name waits for it and shares its log.
    retired: BTreeMap<String, Retired>,
    timing: Timing,
    /// `<state>/plugins`.
    dir: PathBuf,
}

impl Default for Plugins {
    fn default() -> Plugins {
        Plugins {
            desired: BTreeMap::new(),
            started: false,
            entries: BTreeMap::new(),
            retired: BTreeMap::new(),
            timing: Timing::from_env(),
            dir: paths::state_dir(&Env::from_process()).join("plugins"),
        }
    }
}

impl Plugins {
    /// Scales every supervision duration (0 to 1], for tests.
    pub const TIME_SCALE_ENV: &'static str = "GROFE_PLUGIN_TIME_SCALE";
}

struct Entry {
    cfg: ConfigPlugin,
    status: SharedStatus,
    log: Arc<Mutex<Log>>,
    run: Option<Run>,
    /// The last supervisor task that was asked to stop; the next one waits
    /// for it, so two processes of one plugin never overlap.
    stopping: Option<JoinHandle<()>>,
}

struct Retired {
    log: Arc<Mutex<Log>>,
    stopping: Option<JoinHandle<()>>,
}

/// A running supervisor task and the way to stop it.
struct Run {
    stop: oneshot::Sender<()>,
    done: JoinHandle<()>,
}

impl Entry {
    fn new(
        name: &str,
        cfg: ConfigPlugin,
        dir: &std::path::Path,
        retired: Option<Retired>,
    ) -> Entry {
        let (log, stopping) = match retired {
            Some(r) => (r.log, r.stopping),
            None => (
                Arc::new(Mutex::new(Log::new(dir.join(format!("{name}.log"))))),
                None,
            ),
        };
        let phase = if cfg.enabled {
            Phase::Backoff
        } else {
            Phase::Off
        };
        Entry {
            cfg,
            status: Arc::new(Mutex::new(Status {
                generation: 0,
                phase,
                pid: None,
                restarts: 0,
                since: SystemTime::now(),
            })),
            log,
            run: None,
            stopping,
        }
    }

    /// Asks the supervisor task to stop its plugin, and cuts it off from the
    /// status. The task ends once the plugin is gone.
    fn stop(&mut self) {
        set(&self.status, |s| s.generation += 1);
        if let Some(run) = self.run.take() {
            let _ = run.stop.send(());
            self.stopping = Some(run.done);
        }
    }

    /// Starts a supervisor task, once the one it replaces has ended.
    fn launch(&mut self, name: &str, env: Vec<(String, String)>, timing: Timing) {
        let mut generation = 0;
        set(&self.status, |s| {
            s.generation += 1;
            generation = s.generation;
            s.restarts = 0;
            s.phase = Phase::Backoff;
            s.pid = None;
            s.since = SystemTime::now();
        });
        let (stop, stopped) = oneshot::channel();
        let sup = Supervisor {
            name: name.to_string(),
            cfg: self.cfg.clone(),
            env,
            status: self.status.clone(),
            generation,
            log: self.log.clone(),
            timing,
        };
        let done = tokio::spawn(sup.run(stopped, self.stopping.take()));
        self.run = Some(Run { stop, done });
    }
}

/// On the actor, from `apply_config`: records the config's plugins and,
/// once started, starts, stops and restarts to match them.
pub fn configure(state: &mut State, config: &Resolved) {
    state.plugins.desired = config.plugins.clone();
    if state.plugins.started {
        reconcile(state);
    }
}

/// Starts the configured plugins; the server calls it once its config is
/// applied and any restore is done.
pub fn start(state: &mut State) {
    state.plugins.started = true;
    reconcile(state);
}

/// Brings the running plugins in line with `desired` (spec §7.4):
/// a new or newly enabled plugin starts, a removed or disabled one stops,
/// one whose `cmd`, `env` or `restart` changed restarts, the rest stay.
fn reconcile(state: &mut State) {
    let desired = state.plugins.desired.clone();
    let removed: Vec<String> = state
        .plugins
        .entries
        .keys()
        .filter(|n| !desired.contains_key(*n))
        .cloned()
        .collect();
    for name in removed {
        if let Some(mut e) = state.plugins.entries.remove(&name) {
            e.stop();
            let retired = Retired {
                log: e.log.clone(),
                stopping: e.stopping.take(),
            };
            state.plugins.retired.insert(name, retired);
        }
    }
    for (name, cfg) in desired {
        let env = plugin_env(state, &name, &cfg);
        let Plugins {
            entries,
            retired,
            timing,
            dir,
            ..
        } = &mut state.plugins;
        let Some(entry) = entries.get_mut(&name) else {
            let mut entry = Entry::new(&name, cfg.clone(), dir, retired.remove(&name));
            if cfg.enabled {
                entry.launch(&name, env, *timing);
            }
            entries.insert(name, entry);
            continue;
        };
        let was_enabled = entry.cfg.enabled;
        let changed = entry.cfg.runs_differently(&cfg);
        entry.cfg = cfg.clone();
        match (cfg.enabled, was_enabled) {
            (false, true) => {
                entry.stop();
                enter(&entry.status, Phase::Off, None);
            }
            (true, false) => entry.launch(&name, env, *timing),
            (true, true) if changed => {
                entry.stop();
                entry.launch(&name, env, *timing);
            }
            _ => {}
        }
    }
}

/// `plugin-restart <name>` (spec §7.4): stops the plugin as on server exit
/// and starts it again at once, with its backoff and crash count cleared.
pub fn restart(state: &mut State, name: &str) -> Result<(), String> {
    let Some(cfg) = state.plugins.entries.get(name).map(|e| e.cfg.clone()) else {
        return Err(format!("unknown plugin: {name}"));
    };
    if !cfg.enabled {
        return Err(format!("plugin {name} is off (enabled = false)"));
    }
    let env = plugin_env(state, name, &cfg);
    let timing = state.plugins.timing;
    if let Some(entry) = state.plugins.entries.get_mut(name) {
        entry.stop();
        entry.launch(name, env, timing);
    }
    Ok(())
}

/// The `list-plugins` lines: `name`, `pid`, `state`, `restarts` and
/// `since` (UTC), tab separated, `-` for no pid.
pub fn list(state: &State) -> String {
    let mut out = String::new();
    for (name, e) in &state.plugins.entries {
        let s = *e.status.lock().unwrap_or_else(PoisonError::into_inner);
        let pid = s.pid.map_or_else(|| "-".to_string(), |p| p.to_string());
        out.push_str(&format!(
            "{name}\t{pid}\t{}\t{}\t{}\n",
            s.phase.name(),
            s.restarts,
            iso8601(s.since)
        ));
    }
    out
}

/// On server exit: stops every plugin (SIGTERM, then SIGKILL after the
/// grace) and waits until they are gone.
pub async fn shutdown(state: &mut State) {
    let grace = state.plugins.timing.grace;
    let mut done = Vec::new();
    for e in state.plugins.entries.values_mut() {
        e.stop();
        done.extend(e.stopping.take());
    }
    done.extend(
        state
            .plugins
            .retired
            .values_mut()
            .filter_map(|r| r.stopping.take()),
    );
    let all = async {
        for d in done {
            let _ = d.await;
        }
    };
    let _ = tokio::time::timeout(grace + Duration::from_secs(1), all).await;
}

/// The environment a plugin gets: the server's global environment (hidden
/// variables left out), the plugin's `env`, then `GROFE`, `GROFE_PLUGIN`,
/// `GROFE_STATE` (the state directory) and, under tmux identity, `TMUX`,
/// which `env` cannot override (it cannot set `TMUX` at all under grofe
/// identity). A plugin is in no pane, so it gets no `GROFE_PANE` or
/// `TMUX_PANE`.
fn plugin_env(state: &State, name: &str, cfg: &ConfigPlugin) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = hooks::variables(state, None)
        .into_iter()
        .filter(|v| !v.hidden)
        .filter_map(|v| Some((v.name, v.value?)))
        .collect();
    for pane_var in ["TMUX", "TMUX_PANE", "GROFE_PANE"] {
        env.remove(pane_var);
    }
    let socket = state.socket.display().to_string();
    env.extend(cfg.env.clone());
    env.remove("TMUX");
    if state.model.identity == Identity::Tmux {
        env.insert(
            "TMUX".to_string(),
            format!("{socket},{},-1", std::process::id()),
        );
    }
    env.insert("GROFE".to_string(), socket);
    env.insert("GROFE_PLUGIN".to_string(), name.to_string());
    env.insert(
        "GROFE_STATE".to_string(),
        paths::state_dir(&Env::from_process()).display().to_string(),
    );
    env.into_iter().collect()
}

struct Supervisor {
    name: String,
    cfg: ConfigPlugin,
    env: Vec<(String, String)>,
    status: SharedStatus,
    /// The [`Status::generation`] this task may write under.
    generation: u64,
    log: Arc<Mutex<Log>>,
    timing: Timing,
}

/// Whether to go round again after an exit.
enum Next {
    Restart,
    Done,
}

impl Supervisor {
    async fn run(self, mut stop: oneshot::Receiver<()>, after: Option<JoinHandle<()>>) {
        if let Some(prev) = after {
            let _ = prev.await;
        }
        // Stopped while it waited for the previous process to go.
        if !matches!(stop.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
            return;
        }
        let t = self.timing;
        let mut backoff = t.initial;
        let mut crashes: VecDeque<Instant> = VecDeque::new();
        loop {
            let started = Instant::now();
            let exit = match self.spawn() {
                // No pid means it has already been reaped, so it has exited:
                // that is an exit like any other, not a status left at
                // `backoff`.
                Ok((mut child, pump)) if child.id().is_none() => {
                    exit_status(&mut child, pump).await
                }
                Ok((mut child, pump)) => {
                    let pid = child.id().expect("checked by the guard");
                    let guard = Group(Some(Pid::from_raw(pid as i32)));
                    self.enter(Phase::Running, Some(pid));
                    tokio::select! {
                        exit = child.wait() => {
                            // Whatever the plugin left running goes with it.
                            guard.kill_survivors();
                            guard.disarm();
                            let _ = tokio::time::timeout(Duration::from_millis(500), pump).await;
                            exit.map(|s| (s.to_string(), !s.success())).map_err(|e| e.to_string())
                        }
                        _ = &mut stop => {
                            terminate(&mut child, &guard, t.grace).await;
                            guard.disarm();
                            let _ = tokio::time::timeout(Duration::from_millis(500), pump).await;
                            self.note("stopped");
                            return;
                        }
                    }
                }
                Err(e) => Err(format!("cannot start: {e}")),
            };
            let alive = started.elapsed();
            let failed = match &exit {
                Ok((status, failed)) => {
                    self.note(&format!("exited: {status}"));
                    *failed
                }
                Err(msg) => {
                    self.note(msg);
                    true
                }
            };
            match self.after_exit(failed, alive, &mut backoff, &mut crashes) {
                Next::Done => return,
                Next::Restart => {}
            }
            let delay = backoff;
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                _ = &mut stop => return,
            }
            backoff = (backoff * 2).min(t.max);
            self.update(|s| s.restarts += 1);
        }
    }

    /// Applies the restart policy to an exit nobody asked for.
    fn after_exit(
        &self,
        failed: bool,
        alive: Duration,
        backoff: &mut Duration,
        crashes: &mut VecDeque<Instant>,
    ) -> Next {
        let t = self.timing;
        let restarts = match self.cfg.restart {
            Restart::Always => true,
            Restart::OnFailure => failed,
            Restart::Never => false,
        };
        if !restarts {
            self.enter(Phase::Exited, None);
            return Next::Done;
        }
        if alive >= t.reset_after {
            *backoff = t.initial;
        }
        let now = Instant::now();
        crashes.push_back(now);
        while crashes
            .front()
            .is_some_and(|c| now.duration_since(*c) > t.window)
        {
            crashes.pop_front();
        }
        if crashes.len() >= MAX_CRASHES {
            self.enter(Phase::Disabled, None);
            let msg = format!(
                "disabled after {MAX_CRASHES} crashes within {}s",
                t.window.as_secs_f64().round()
            );
            self.note(&msg);
            eprintln!("grofe plugin {}: {msg}", self.name);
            return Next::Done;
        }
        self.enter(Phase::Backoff, None);
        Next::Restart
    }

    /// Starts the plugin: a pipe both its streams write to, and the task
    /// that copies the pipe to the log.
    fn spawn(&self) -> io::Result<(Child, JoinHandle<()>)> {
        let _lock = pane::spawn_lock();
        let (read, write) = io::pipe()?;
        let write_err = write.try_clone()?;
        let fd_limit = pane::nofile_limit();
        let mut cmd = Command::new(&self.cfg.cmd[0]);
        cmd.args(&self.cfg.cmd[1..])
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::from(write))
            .stderr(Stdio::from(write_err))
            .process_group(0)
            .kill_on_drop(true);
        // SAFETY: the closure only makes async-signal-safe libc calls.
        unsafe {
            cmd.pre_exec(move || {
                // std empties the signal mask and restores SIGPIPE; the
                // rest of the dispositions are the server's, so a server
                // started with a signal ignored must not pass that on.
                for sig in 1..32 {
                    if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                        libc::signal(sig, libc::SIG_DFL);
                    }
                }
                // macOS sets close-on-exec on an accepted socket after the
                // fact, so a client connection can be open here.
                pane::cloexec_inherited_fds(fd_limit);
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        let receiver = pipe::Receiver::from_owned_fd(OwnedFd::from(read))?;
        let log = self.log.clone();
        let pump = tokio::spawn(copy_to_log(receiver, log));
        Ok((child, pump))
    }

    /// Changes the status, unless the plugin has been stopped or relaunched
    /// since this task started.
    fn update(&self, f: impl FnOnce(&mut Status)) {
        set(&self.status, |s| {
            if s.generation == self.generation {
                f(s);
            }
        });
    }

    fn enter(&self, phase: Phase, pid: Option<u32>) {
        self.update(|s| {
            s.phase = phase;
            s.pid = pid;
            s.since = SystemTime::now();
        });
    }

    /// A line in the plugin's log, from the server.
    fn note(&self, msg: &str) {
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        log.write(format!("grofe: plugin {}: {msg}\n", self.name).as_bytes());
    }
}

/// Copies the plugin's output to its log until the pipe closes.
async fn copy_to_log(mut from: pipe::Receiver, log: Arc<Mutex<Log>>) {
    let mut buf = vec![0u8; 8192];
    while let Ok(n) = from.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
        log.write(&buf[..n]);
    }
}

/// The plugin's process group, killed if its task is dropped before the
/// plugin is reaped (the runtime going down). Disarmed once reaped, as the
/// group id may then be reused.
struct Group(Option<Pid>);

impl Group {
    /// SIGKILLs the group. Only while the leader is unreaped: its zombie
    /// keeps the group id from being handed to anything else.
    fn kill(&self) {
        if let Some(pgid) = self.0 {
            let _ = killpg(pgid, Signal::SIGKILL);
        }
    }

    /// For after the leader was reaped: SIGKILLs what it left behind, and
    /// only while the group still has members. An empty group's id is free
    /// to be reused by an unrelated process, which a blind `killpg` would
    /// hit. Returns whether it signalled.
    fn kill_survivors(&self) -> bool {
        let Some(pgid) = self.0 else { return false };
        killpg(pgid, None::<Signal>).is_ok() && killpg(pgid, Signal::SIGKILL).is_ok()
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The exit of a plugin: its status once reaped, after the log pump has
/// had a moment to drain.
async fn exit_status(child: &mut Child, pump: JoinHandle<()>) -> Result<(String, bool), String> {
    let exit = child.wait().await;
    let _ = tokio::time::timeout(Duration::from_millis(500), pump).await;
    exit.map(|s| (s.to_string(), !s.success()))
        .map_err(|e| e.to_string())
}

/// Stops a plugin as the server exit does: SIGTERM to its group, then
/// SIGKILL once `grace` has passed with the leader or any member still up.
async fn terminate(child: &mut Child, group: &Group, grace: Duration) {
    let Some(pgid) = group.0 else { return };
    let _ = killpg(pgid, Signal::SIGTERM);
    let deadline = tokio::time::Instant::now() + grace;
    let leader_gone = tokio::time::timeout_at(deadline, child.wait())
        .await
        .is_ok();
    if leader_gone {
        // The leader is reaped, but its children may outlive it: they get
        // the rest of the grace, then are killed if still there.
        while tokio::time::Instant::now() < deadline && killpg(pgid, None::<Signal>).is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        group.kill_survivors();
    } else {
        // Still unreaped: its zombie holds the group id.
        group.kill();
        let _ = child.wait().await;
    }
}

/// A plugin's log file, opened on first write and rotated to `<file>.1`
/// when a write would take it past [`LOG_LIMIT`].
struct Log {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl Log {
    fn new(path: PathBuf) -> Log {
        Log {
            path,
            file: None,
            size: 0,
        }
    }

    fn open(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                paths::ensure_private_dir(dir)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&self.path)?;
            self.size = file.metadata()?.len();
            self.file = Some(file);
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("log not open"))
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.open().is_err() {
            return;
        }
        if self.size > 0 && self.size + bytes.len() as u64 > LOG_LIMIT {
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            self.file = None;
            let _ = fs::rename(&self.path, old);
            if self.open().is_err() {
                return;
            }
        }
        let len = bytes.len() as u64;
        if let Ok(f) = self.open()
            && f.write_all(bytes).is_ok()
        {
            self.size += len;
        }
    }
}

/// `t` as `YYYY-MM-DDTHH:MM:SSZ`.
fn iso8601(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_formats_utc() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_793_100_000);
        assert_eq!(iso8601(t), "2026-10-27T11:20:00Z");
    }

    #[test]
    fn the_log_rotates_at_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins").join("p.log");
        let mut log = Log::new(path.clone());
        let chunk = vec![b'x'; 600 * 1024];
        log.write(&chunk);
        log.write(&chunk);
        let old = dir.path().join("plugins").join("p.log.1");
        assert_eq!(fs::metadata(&old).map(|m| m.len()).ok(), Some(600 * 1024));
        assert_eq!(fs::metadata(&path).map(|m| m.len()).ok(), Some(600 * 1024));
    }

    fn supervisor(restart: Restart, timing: Timing) -> Supervisor {
        let cfg = ConfigPlugin {
            cmd: vec!["x".to_string()],
            env: BTreeMap::new(),
            enabled: true,
            restart,
            sources: BTreeMap::new(),
        };
        let status = Arc::new(Mutex::new(Status {
            generation: 0,
            phase: Phase::Running,
            pid: Some(1),
            restarts: 0,
            since: SystemTime::now(),
        }));
        Supervisor {
            name: "t".to_string(),
            cfg,
            env: Vec::new(),
            status,
            generation: 0,
            log: Arc::new(Mutex::new(Log::new(PathBuf::from("/nonexistent/t.log")))),
            timing,
        }
    }

    /// A leader running `script` in its own group, reaped; its group id
    /// and what the script printed.
    async fn reaped_leader(script: &str) -> (Pid, String) {
        use std::process::Stdio;
        let mut leader = Command::new("sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("spawn");
        let pgid = Pid::from_raw(leader.id().expect("pid") as i32);
        let mut out = String::new();
        leader
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut out)
            .await
            .expect("read");
        leader.wait().await.expect("reap");
        (pgid, out)
    }

    /// Whether `pid` is a process still running (a zombie nobody has
    /// reaped yet is not).
    fn running(pid: &str) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .expect("ps");
        let stat = String::from_utf8_lossy(&out.stdout);
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    }

    #[tokio::test]
    async fn survivors_of_a_reaped_leader_are_killed_while_the_group_has_members() {
        let (pgid, out) = reaped_leader("sleep 60 >/dev/null 2>&1 & echo $!").await;
        let sleeper = out.trim();
        assert!(running(sleeper), "the survivor starts up");
        assert!(Group(Some(pgid)).kill_survivors());
        for _ in 0..200 {
            if !running(sleeper) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the survivor {sleeper} is still up");
    }

    #[tokio::test]
    async fn an_empty_group_is_not_signalled() {
        // Nothing is left under that id, which may be anyone's by now.
        let (pgid, _) = reaped_leader("exit 0").await;
        assert!(!Group(Some(pgid)).kill_survivors());
        assert!(!Group(None).kill_survivors());
    }

    #[tokio::test]
    async fn a_plugin_without_a_pid_is_an_exit() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 3"])
            .spawn()
            .expect("spawn");
        child.wait().await.expect("reap");
        assert_eq!(child.id(), None);
        let pump = tokio::spawn(async {});
        let (status, failed) = exit_status(&mut child, pump).await.expect("status");
        assert!(failed, "{status}");
        assert!(status.contains('3'), "{status}");
    }

    fn phase(s: &Supervisor) -> Phase {
        s.status.lock().expect("status").phase
    }

    #[test]
    fn five_crashes_within_the_window_disable_and_four_do_not() {
        let s = supervisor(Restart::Always, Timing::scaled(1.0));
        let (mut backoff, mut crashes) = (s.timing.initial, VecDeque::new());
        for _ in 0..4 {
            let next = s.after_exit(true, Duration::ZERO, &mut backoff, &mut crashes);
            assert!(matches!(next, Next::Restart));
            assert_eq!(phase(&s), Phase::Backoff);
        }
        let next = s.after_exit(true, Duration::ZERO, &mut backoff, &mut crashes);
        assert!(matches!(next, Next::Done));
        assert_eq!(phase(&s), Phase::Disabled);
    }

    #[test]
    fn crashes_outside_the_window_are_forgotten() {
        // A 60 ms window: the first four crashes age out before the fifth.
        let s = supervisor(Restart::Always, Timing::scaled(0.001));
        let (mut backoff, mut crashes) = (s.timing.initial, VecDeque::new());
        for _ in 0..4 {
            s.after_exit(true, Duration::ZERO, &mut backoff, &mut crashes);
        }
        std::thread::sleep(Duration::from_millis(120));
        let next = s.after_exit(true, Duration::ZERO, &mut backoff, &mut crashes);
        assert!(matches!(next, Next::Restart));
        assert_eq!(crashes.len(), 1);
    }

    #[test]
    fn sixty_seconds_alive_resets_the_backoff() {
        let s = supervisor(Restart::Always, Timing::scaled(1.0));
        let (mut backoff, mut crashes) = (Duration::from_secs(32), VecDeque::new());
        s.after_exit(true, Duration::from_secs(59), &mut backoff, &mut crashes);
        assert_eq!(backoff, Duration::from_secs(32));
        s.after_exit(true, Duration::from_secs(60), &mut backoff, &mut crashes);
        assert_eq!(backoff, Duration::from_secs(1));
    }

    #[test]
    fn the_policy_decides_whether_an_exit_restarts() {
        for (restart, failed, restarts) in [
            (Restart::Always, false, true),
            (Restart::OnFailure, false, false),
            (Restart::OnFailure, true, true),
            (Restart::Never, true, false),
        ] {
            let s = supervisor(restart, Timing::scaled(1.0));
            let (mut backoff, mut crashes) = (s.timing.initial, VecDeque::new());
            let next = s.after_exit(failed, Duration::ZERO, &mut backoff, &mut crashes);
            assert_eq!(
                matches!(next, Next::Restart),
                restarts,
                "{restart:?} {failed}"
            );
            if !restarts {
                assert_eq!(phase(&s), Phase::Exited);
            }
        }
    }

    #[test]
    fn timing_scales() {
        let t = Timing::scaled(0.5);
        assert_eq!(t.initial, Duration::from_millis(500));
        assert_eq!(t.max, Duration::from_secs(30));
    }
}
