//! Status sources (spec D8, §4.10): commands the server runs on a schedule,
//! off the redraw path, whose last non-empty output line is kept in an
//! option the status line reads, as tmux 3.7b keeps a `#()` job's.
//!
//! A source is registered under an option name: `@<name>` for one the
//! config declares (`[status.sources.<name>]`), `@src-<hash>` for an inline
//! `#(cmd)` the tmux front end registers through [`register`]. Entries are
//! keyed by the command string, so two registrations of one command share
//! one schedule, one run and one value.
//!
//! One scheduler task sleeps until the earliest due source and then sends
//! [`tick`] to the actor. The tick starts each due source that has no run
//! in flight; a run spawns `/bin/sh -c <cmd>` under the pane spawn lock in
//! its own process group, off the actor, and reports back through
//! `State.jobs`. A run still going at its timeout has its process group
//! killed.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, watch};

use crate::config::Resolved;
use crate::event::Event;
use crate::server::dispatch::options::Owner;
use crate::server::{Job, State, clients, hooks, pane};

/// How long a run may take when its registration names no timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of one line kept, in bytes; the rest of a longer line is
/// dropped.
pub const MAX_LINE: usize = 4096;

/// The most output read from one run, in bytes. The pipe is then closed,
/// so a command that spews output gets `SIGPIPE` and costs the server at
/// most this much.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// The longest `interval` or `timeout` a config may give, in seconds:
/// tmux's `INT_MAX`.
pub const MAX_SECONDS: u64 = i32::MAX as u64;

/// How often a source parked by `status-interval 0` looks again.
// ponytail: a tick a second while parked; a hook on `status-interval`
// changes would need an edit to the option store.
const PARKED: Duration = Duration::from_secs(1);

/// An inline source not requested (registered again) for this long is
/// dropped, as tmux 3.7b drops a `#()` job unused for an hour.
pub const INLINE_EXPIRY: Duration = Duration::from_secs(3600);

/// The registered sources and their schedules; their values live in the
/// option store. Lives on `State.sources`.
#[derive(Default)]
pub struct Sources {
    /// By command string.
    entries: BTreeMap<String, Entry>,
    /// Numbers each run, so a run that outlives its entry's removal and
    /// re-registration cannot mark the new entry idle.
    runs: u64,
    /// The earliest due time, watched by the scheduler task. `None` until
    /// the first registration starts it.
    due: Option<watch::Sender<Option<Instant>>>,
}

struct Entry {
    /// Option names (`batt` for `@batt`, `src-<hash>`) that get the value.
    keys: BTreeMap<String, Registration>,
    /// When the next run is due; `None` means never (a deadline too far
    /// for `Instant`).
    next: Option<Instant>,
    /// The run in flight, by number.
    running: Option<u64>,
    /// A run has been started.
    started: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Registration {
    /// `None` follows the global `status-interval`.
    interval: Option<Duration>,
    timeout: Duration,
    /// Declared by the config, so `configure` replaces it on reload.
    config: bool,
    /// For an inline source, when it was last registered (requested); one
    /// not requested for [`INLINE_EXPIRY`] is dropped. `None` for a named
    /// source, which never expires.
    requested: Option<Instant>,
}

impl Entry {
    /// The shortest interval among the registrations; `None` when that is
    /// `status-interval` and it is 0 (off).
    fn interval(&self, status_interval: Duration) -> Option<Duration> {
        self.keys
            .values()
            .map(|r| r.interval.unwrap_or(status_interval))
            .min()
            .filter(|d| !d.is_zero())
    }

    /// The longest timeout among the registrations.
    fn timeout(&self) -> Duration {
        self.keys
            .values()
            .map(|r| r.timeout)
            .max()
            .unwrap_or(DEFAULT_TIMEOUT)
    }
}

impl Sources {
    /// Adds `key` (an option name without `@`) for `cmd`, moving it off any
    /// other command. A new command is due at once; an existing one keeps
    /// its schedule.
    fn add(&mut self, key: &str, cmd: &str, reg: Registration, now: Instant) {
        self.remove_key_except(key, cmd);
        let entry = self.entries.entry(cmd.to_string()).or_insert(Entry {
            keys: BTreeMap::new(),
            next: Some(now),
            running: None,
            started: false,
        });
        entry.keys.insert(key.to_string(), reg);
    }

    /// Drops `key` from every command but `keep`.
    fn remove_key_except(&mut self, key: &str, keep: &str) {
        for (cmd, e) in self.entries.iter_mut() {
            if cmd != keep {
                e.keys.remove(key);
            }
        }
    }

    /// Drops every registration the config made.
    fn remove_config(&mut self) {
        for e in self.entries.values_mut() {
            e.keys.retain(|_, r| !r.config);
        }
    }

    /// Drops the commands nothing registers any more. Called after a batch
    /// of changes, so a command removed and added back in one batch keeps
    /// its schedule and its run in flight.
    fn prune(&mut self) {
        self.entries.retain(|_, e| !e.keys.is_empty());
    }

    /// The commands due at `now` that have no run in flight, each with its
    /// timeout and a new run number, marked running. Every due entry moves
    /// to its next time, whether it starts or is skipped for a run still
    /// going (one run in flight). Missed times are skipped, not caught up.
    fn due(&mut self, now: Instant, status_interval: Duration) -> Vec<(String, Duration, u64)> {
        let mut start = Vec::new();
        for (cmd, e) in self.entries.iter_mut() {
            let Some(at) = e.next.filter(|at| *at <= now) else {
                continue;
            };
            let Some(iv) = e.interval(status_interval) else {
                // `status-interval` is 0: after its first run the source
                // waits, looking again every [`PARKED`], so a later
                // positive `status-interval` resumes it.
                e.next = now.checked_add(PARKED);
                if !e.started {
                    e.started = true;
                    self.runs += 1;
                    e.running = Some(self.runs);
                    start.push((cmd.clone(), e.timeout(), self.runs));
                }
                continue;
            };
            // A deadline past what `Instant` holds means never.
            e.next = at
                .checked_add(iv)
                .filter(|next| *next > now)
                .or_else(|| now.checked_add(iv));
            if e.running.is_none() {
                e.started = true;
                self.runs += 1;
                e.running = Some(self.runs);
                start.push((cmd.clone(), e.timeout(), self.runs));
            }
        }
        start
    }

    /// The earliest time an entry is due or an inline source expires.
    fn earliest(&self) -> Option<Instant> {
        let expiries = self
            .entries
            .values()
            .flat_map(|e| e.keys.values())
            .filter_map(|r| r.requested?.checked_add(INLINE_EXPIRY));
        self.entries
            .values()
            .filter_map(|e| e.next)
            .chain(expiries)
            .min()
    }

    /// Drops the inline sources not requested for [`INLINE_EXPIRY`].
    fn expire(&mut self, now: Instant) {
        for e in self.entries.values_mut() {
            e.keys.retain(|_, r| {
                r.requested
                    .and_then(|t| t.checked_add(INLINE_EXPIRY))
                    .is_none_or(|until| until > now)
            });
        }
        self.prune();
    }

    /// Run `run` of `cmd` ended.
    fn finished(&mut self, cmd: &str, run: u64) {
        if let Some(e) = self.entries.get_mut(cmd)
            && e.running == Some(run)
        {
            e.running = None;
        }
    }
}

/// The option name an inline source of `cmd` stores its value in, without
/// the `@`: `src-` and a 64-bit FNV-1a hash of the command, in hex. Stable
/// across servers and builds, so a format can name it.
pub fn inline_key(cmd: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in cmd.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("src-{hash:016x}")
}

/// Registers a source and returns the option its value lands in: `@<name>`
/// for a named source, `@src-<hash of cmd>` for an inline one (`name` is
/// `None`). `interval` `None` follows the global `status-interval`;
/// `timeout` `None` is [`DEFAULT_TIMEOUT`]. Registering the same name and
/// command again only updates its interval and timeout, and is cheap, so
/// the tmux front end may call it on every expansion of a `#(cmd)`: each
/// call counts as a request, and an inline source not requested for
/// [`INLINE_EXPIRY`] is dropped, as tmux drops an unused job. This is the
/// one function the front end calls for `#(cmd)`; [`unregister`] drops a
/// source at once.
pub fn register(
    state: &mut State,
    name: Option<&str>,
    cmd: &str,
    interval: Option<Duration>,
    timeout: Option<Duration>,
) -> String {
    let now = Instant::now();
    let key = name.map_or_else(|| inline_key(cmd), str::to_string);
    let reg = Registration {
        interval,
        timeout: timeout.unwrap_or(DEFAULT_TIMEOUT),
        config: false,
        requested: name.is_none().then_some(now),
    };
    let same = |r: &Registration| Registration {
        requested: None,
        ..*r
    };
    if let Some(r) = state
        .sources
        .entries
        .get_mut(cmd)
        .and_then(|e| e.keys.get_mut(&key))
        && same(r) == same(&reg)
    {
        // Already registered as asked: only the request time moves.
        r.requested = reg.requested;
        return format!("@{key}");
    }
    state.sources.add(&key, cmd, reg, now);
    state.sources.prune();
    reschedule(state);
    format!("@{key}")
}

/// Drops the source registered under `option` (`@batt`, `@src-<hash>`, as
/// [`register`] returned it; the `@` is optional). Its option keeps its
/// last value. Whether one was registered.
/// Drops every `#(cmd)` source (an `@src-<hash>` key, FE.3) that no
/// option's value reads any more: after a `set` overwrites or unsets the
/// option, an `-a` changes it, or a reload replaces it. So a source runs
/// only while the status line can show it.
pub fn prune_unreferenced(state: &mut State) {
    let keys: BTreeSet<String> = state
        .sources
        .entries
        .values()
        .flat_map(|e| e.keys.keys())
        .filter(|k| k.starts_with("src-"))
        .cloned()
        .collect();
    for key in keys {
        if !state.model.options.mentions(&format!("#{{@{key}}}")) {
            unregister(state, &key);
        }
    }
}

/// How many commands have a source now, for tests.
pub fn running(state: &State) -> usize {
    state.sources.entries.len()
}

pub fn unregister(state: &mut State, option: &str) -> bool {
    let key = option.strip_prefix('@').unwrap_or(option);
    let mut found = false;
    for e in state.sources.entries.values_mut() {
        found |= e.keys.remove(key).is_some();
    }
    state.sources.prune();
    reschedule(state);
    found
}

/// Applies the config's sources (`[status.sources]`), at start and on each
/// `reload`, after its options: the config's registrations are replaced;
/// inline ones stay. A command still registered keeps its schedule and its
/// run in flight. A dropped source's option keeps its last value.
pub fn configure(state: &mut State, config: &Resolved) {
    let now = Instant::now();
    state.sources.remove_config();
    for (name, s) in &config.sources {
        let reg = Registration {
            interval: s.interval.map(Duration::from_secs),
            timeout: s.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs),
            config: true,
            requested: None,
        };
        state.sources.add(name, &s.cmd, reg, now);
    }
    state.sources.prune();
    reschedule(state);
}

/// Tells the scheduler the earliest due time, starting it on first use.
/// Without a tokio runtime (unit tests) nothing is scheduled.
fn reschedule(state: &mut State) {
    let earliest = state.sources.earliest();
    if state.sources.due.is_none() {
        if tokio::runtime::Handle::try_current().is_err() || earliest.is_none() {
            return;
        }
        let (tx, rx) = watch::channel(None);
        tokio::spawn(scheduler(rx, state.jobs.clone()));
        state.sources.due = Some(tx);
    }
    if let Some(due) = &state.sources.due {
        // `send_replace` notifies even an unchanged value, which the
        // scheduler waits for after each tick.
        due.send_replace(earliest);
    }
}

/// Sleeps until the earliest due time, sends [`tick`], and waits for the
/// tick to publish the next time. Ends when the server drops its sources.
async fn scheduler(mut due: watch::Receiver<Option<Instant>>, jobs: mpsc::Sender<Job>) {
    loop {
        let next = *due.borrow_and_update();
        let Some(at) = next else {
            if due.changed().await.is_err() {
                return;
            }
            continue;
        };
        tokio::select! {
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(at)) => {
                if jobs.send(Box::new(tick)).await.is_err() {
                    return;
                }
                if due.changed().await.is_err() {
                    return;
                }
            }
            changed = due.changed() => if changed.is_err() {
                return;
            },
        }
    }
}

/// On the actor: drops expired inline sources, starts every due source and
/// publishes the next due time.
fn tick(state: &mut State) {
    let status_interval = state
        .model
        .options
        .lookup("status-interval", &[])
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_secs);
    let now = Instant::now();
    state.sources.expire(now);
    for (cmd, timeout, run) in state.sources.due(now, status_interval) {
        spawn_run(state, cmd, timeout, run);
    }
    reschedule(state);
}

/// Starts run `run` of `cmd` off the actor, with the server environment
/// (§4.2) and, as tmux 3.7b gives its `#()` jobs (recorded), `TMUX` with
/// session `-1` and no `TMUX_PANE`.
fn spawn_run(state: &mut State, cmd: String, timeout: Duration, run: u64) {
    let mut env = hooks::shell_env(state, None);
    let socket = state.socket.display().to_string();
    env.retain(|(k, _)| !matches!(k.as_str(), "TMUX" | "TMUX_PANE" | "GROFE" | "GROFE_PANE"));
    env.push((
        "TMUX".to_string(),
        format!("{socket},{},-1", std::process::id()),
    ));
    // As a pane: a `grofe` run inside talks to this server.
    env.push(("GROFE".to_string(), socket));
    let jobs = state.jobs.clone();
    tokio::spawn(async move {
        run_source(&cmd, env, timeout, &jobs).await;
        let _ = jobs
            .send(Box::new(move |s: &mut State| s.sources.finished(&cmd, run)))
            .await;
    });
}

/// Runs `cmd` once, reading its output until it closes, and waits for the
/// shell, killing its process group at `timeout`. Then sends the value
/// ([`LastLine::value`]) to the actor.
async fn run_source(
    cmd: &str,
    env: Vec<(String, String)>,
    timeout: Duration,
    jobs: &mpsc::Sender<Job>,
) {
    let child = {
        let _lock = pane::spawn_lock();
        tokio::process::Command::new("/bin/sh")
            // As tmux's `job_run`: `argv[0]` is `sh`, not the path.
            .arg0("sh")
            .arg("-c")
            .arg(cmd)
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
    };
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("grofe source {cmd:?}: {e}");
            return;
        }
    };
    let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) else {
        return;
    };
    let group = Group(Some(Pid::from_raw(pid)));
    let stdout = child.stdout.take();
    let mut output = LastLine::default();
    let work = async {
        if let Some(out) = stdout {
            output.read(out).await;
        }
        child.wait().await
    };
    if tokio::time::timeout(timeout, work).await.is_err() {
        group.kill();
        let _ = child.wait().await;
    }
    group.disarm();
    if let Some(line) = output.value() {
        let cmd = cmd.to_string();
        let _ = jobs
            .send(Box::new(move |s: &mut State| store(s, &cmd, &line)))
            .await;
    }
}

/// A run's process group, killed if the run is dropped before its shell is
/// reaped (the server exiting mid-run). Disarmed once the shell is reaped,
/// as its group id may then be reused.
struct Group(Option<Pid>);

impl Group {
    fn kill(&self) {
        if let Some(pgid) = self.0 {
            let _ = killpg(pgid, Signal::SIGKILL);
        }
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

/// A run's output as it is read: the last complete non-empty line and the
/// line being read, each cut at [`MAX_LINE`] bytes.
#[derive(Default)]
struct LastLine {
    last: Option<Vec<u8>>,
    current: Vec<u8>,
    /// The output reached its end (not the timeout or [`MAX_OUTPUT`]).
    ended: bool,
}

impl LastLine {
    /// Reads `out` until it ends, fails, or [`MAX_OUTPUT`] bytes have come.
    /// The pipe is closed on return.
    async fn read(&mut self, out: impl AsyncRead + Unpin) {
        let mut out = out.take(MAX_OUTPUT as u64);
        let mut buf = [0u8; 4096];
        loop {
            match out.read(&mut buf).await {
                Ok(0) => {
                    self.ended = out.limit() > 0;
                    return;
                }
                Ok(n) => self.push(&buf[..n]),
                Err(_) => return,
            }
        }
    }

    /// Adds output. A CR ends a line as an LF does, as in tmux 3.7b
    /// (`a\rb\n` gives `b`, `x\r\n\r\n` gives `x`).
    fn push(&mut self, bytes: &[u8]) {
        for chunk in bytes.split_inclusive(|b| matches!(b, b'\n' | b'\r')) {
            let (text, ends) = match chunk.split_last() {
                Some((b'\n' | b'\r', text)) => (text, true),
                _ => (chunk, false),
            };
            let room = MAX_LINE.saturating_sub(self.current.len());
            self.current
                .extend_from_slice(&text[..text.len().min(room)]);
            if ends && !self.current.is_empty() {
                self.last = Some(std::mem::take(&mut self.current));
            }
        }
    }

    /// The value, as tmux 3.7b keeps a `#()` job's (recorded): the last
    /// non-empty line, blank lines skipped, a line of spaces counting.
    /// Text after the last line break (a partial line) is the value when
    /// the output has ended, or when the run was cut off (timeout or
    /// [`MAX_OUTPUT`]) before any complete line: as in tmux, whatever a
    /// finished job left in its buffer shows. Output with no non-empty
    /// text gives an empty value once ended; a run cut off with nothing
    /// read gives `None`, so the old value stays.
    /// ponytail: tmux also shows each line as it arrives; here a run's
    /// value lands when its output ends or it is killed.
    fn value(mut self) -> Option<String> {
        let tail = !self.current.is_empty() && (self.ended || self.last.is_none());
        let line = if tail {
            Some(std::mem::take(&mut self.current))
        } else {
            self.last.take()
        };
        let mut line = match line {
            Some(line) => line,
            None if self.ended => Vec::new(),
            None => return None,
        };
        if let Err(e) = std::str::from_utf8(&line)
            && e.error_len().is_none()
        {
            line.truncate(e.valid_up_to());
        }
        Some(String::from_utf8_lossy(&line).into_owned())
    }
}

/// On the actor: `line` is `cmd`'s latest value. Every option registered
/// for it takes the value; a change repaints every client.
fn store(state: &mut State, cmd: &str, line: &str) {
    let Some(entry) = state.sources.entries.get(cmd) else {
        return;
    };
    let names: Vec<String> = entry.keys.keys().map(|k| format!("@{k}")).collect();
    let mut changed = false;
    for name in names {
        if let Ok(true) = state.model.options.set(Owner::Global, &name, line, false) {
            changed = true;
            state.events.emit(Event::OptionChanged {
                scope: "global".to_string(),
                name,
                value: Some(line.to_string()),
            });
        }
    }
    if changed {
        clients::mark_all_dirty(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Duration = Duration::from_secs(1);

    fn reg(interval: Option<u64>) -> Registration {
        Registration {
            interval: interval.map(Duration::from_secs),
            timeout: DEFAULT_TIMEOUT,
            config: false,
            requested: None,
        }
    }

    /// Steps a clock from `t0` through each due time up to `t0 + span`,
    /// finishing every run at once; how many runs started.
    fn runs_in(s: &mut Sources, t0: Instant, span: Duration, status: Duration) -> usize {
        let mut now = t0;
        let mut started = 0;
        while now <= t0 + span {
            for (cmd, _, run) in s.due(now, status) {
                started += 1;
                s.finished(&cmd, run);
            }
            match s.earliest() {
                Some(next) => now = next,
                None => break,
            }
        }
        started
    }

    #[test]
    fn a_one_second_source_runs_ten_or_eleven_times_in_ten_seconds() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        s.add("n", "echo", reg(Some(1)), t0);
        let n = runs_in(&mut s, t0, 10 * SEC, 15 * SEC);
        assert!((10..=11).contains(&n), "{n} runs");
    }

    #[test]
    fn a_late_tick_does_not_drift_or_catch_up() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        s.add("n", "echo", reg(Some(1)), t0);
        assert_eq!(s.due(t0, SEC).len(), 1);
        s.finished("echo", 1);
        // Woken 300 ms late: runs once, next stays on the second grid.
        assert_eq!(
            s.due(t0 + SEC + 300 * Duration::from_millis(1), SEC).len(),
            1
        );
        assert_eq!(s.earliest(), Some(t0 + 2 * SEC));
        s.finished("echo", 2);
        // Woken 3.5 s late: one run, not three.
        assert_eq!(s.due(t0 + 5 * SEC + SEC / 2, SEC).len(), 1);
        assert_eq!(s.earliest(), Some(t0 + 6 * SEC + SEC / 2));
    }

    #[test]
    fn a_run_in_flight_skips_its_next_tick() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        s.add("n", "sleep 30", reg(Some(1)), t0);
        let first = s.due(t0, SEC);
        assert_eq!(first.len(), 1);
        assert!(s.due(t0 + SEC, SEC).is_empty());
        assert!(s.due(t0 + 2 * SEC, SEC).is_empty());
        s.finished("sleep 30", first[0].2);
        assert_eq!(s.due(t0 + 3 * SEC, SEC).len(), 1);
    }

    #[test]
    fn a_stale_run_does_not_free_a_re_registered_entry() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        s.add("n", "x", reg(Some(1)), t0);
        let old = s.due(t0, SEC)[0].2;
        s.remove_key_except("n", "");
        s.prune();
        s.add("n", "x", reg(Some(1)), t0 + SEC);
        let new = s.due(t0 + SEC, SEC)[0].2;
        s.finished("x", old);
        assert!(s.due(t0 + 2 * SEC, SEC).is_empty(), "run {new} still going");
    }

    #[test]
    fn two_registrations_of_one_command_share_one_run() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        s.add("a", "date", reg(Some(2)), t0);
        s.add(&inline_key("date"), "date", reg(None), t0);
        let started = s.due(t0, 5 * SEC);
        assert_eq!(started.len(), 1);
        // The shorter interval wins.
        assert_eq!(s.earliest(), Some(t0 + 2 * SEC));
    }

    #[test]
    fn the_status_interval_applies_and_zero_parks_after_one_run() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        s.add("a", "date", reg(None), t0);
        assert_eq!(s.due(t0, 5 * SEC).len(), 1);
        assert_eq!(s.earliest(), Some(t0 + 5 * SEC));
        let mut s = Sources::default();
        s.add("a", "date", reg(None), t0);
        assert_eq!(s.due(t0, Duration::ZERO).len(), 1);
        s.finished("date", 1);
        // Parked: it looks again every PARKED, without running.
        assert_eq!(s.earliest(), Some(t0 + PARKED));
        assert!(s.due(t0 + PARKED, Duration::ZERO).is_empty());
        // `status-interval` back to 5: it resumes at the next look.
        assert_eq!(s.due(t0 + 2 * PARKED, 5 * SEC).len(), 1);
        assert_eq!(s.earliest(), Some(t0 + 2 * PARKED + 5 * SEC));
    }

    /// Review finding: `at + iv` overflowed on a huge interval and the
    /// server panicked. A deadline past what `Instant` holds is never.
    #[test]
    fn a_huge_interval_does_not_overflow() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        s.add("a", "date", reg(Some(u64::MAX)), t0);
        assert_eq!(s.due(t0, SEC).len(), 1);
        assert_eq!(s.earliest(), None);
        let mut s = Sources::default();
        s.add("a", "date", reg(None), t0);
        assert_eq!(s.due(t0, Duration::from_secs(u64::MAX)).len(), 1);
        assert_eq!(s.earliest(), None);
    }

    #[test]
    fn an_inline_source_expires_after_an_hour_unrequested() {
        let mut state = crate::server::test_state();
        let key = register(&mut state, None, "date", None, None);
        register(&mut state, Some("named"), "uptime", None, None);
        let requested = |state: &State| {
            state.sources.entries["date"].keys[&key[1..]]
                .requested
                .expect("inline")
        };
        let t0 = requested(&state);
        state.sources.expire(t0 + INLINE_EXPIRY - SEC);
        assert!(state.sources.entries.contains_key("date"));
        // A second request moves the expiry.
        register(&mut state, None, "date", None, None);
        let t1 = requested(&state);
        assert!(t1 >= t0);
        state.sources.expire(t1 + INLINE_EXPIRY);
        assert!(!state.sources.entries.contains_key("date"));
        // A named source never expires.
        assert!(state.sources.entries.contains_key("uptime"));
    }

    #[test]
    fn an_inline_expiry_is_a_scheduler_deadline() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        let inline = Registration {
            requested: Some(t0),
            ..reg(Some(60 * 60 * 24))
        };
        s.add("src-x", "x", inline, t0);
        s.due(t0, SEC);
        assert_eq!(s.earliest(), Some(t0 + INLINE_EXPIRY));
    }

    #[test]
    fn unregister_drops_a_source() {
        let mut state = crate::server::test_state();
        let key = register(&mut state, None, "date", None, None);
        assert!(unregister(&mut state, &key));
        assert!(state.sources.entries.is_empty());
        assert!(!unregister(&mut state, &key));
        register(&mut state, Some("batt"), "x", None, None);
        assert!(unregister(&mut state, "batt"));
        assert!(state.sources.entries.is_empty());
    }

    #[test]
    fn a_name_moves_to_its_new_command() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        s.add("a", "one", reg(Some(1)), t0);
        s.add("a", "two", reg(Some(1)), t0);
        s.prune();
        assert_eq!(s.entries.keys().collect::<Vec<_>>(), ["two"]);
    }

    #[test]
    fn configure_replaces_config_sources_and_keeps_inline_ones() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        let config = Registration {
            config: true,
            ..reg(Some(1))
        };
        s.add("batt", "b", config, t0);
        s.add(&inline_key("i"), "i", reg(None), t0);
        s.remove_config();
        s.prune();
        assert_eq!(s.entries.keys().collect::<Vec<_>>(), ["i"]);
    }

    #[test]
    fn reconfiguring_a_command_keeps_its_schedule_and_its_run() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        let config = Registration {
            config: true,
            ..reg(Some(60))
        };
        s.add("a", "x", config, t0);
        assert_eq!(s.due(t0, SEC).len(), 1);
        s.remove_config();
        s.add("a", "x", config, t0 + SEC);
        s.prune();
        assert_eq!(s.earliest(), Some(t0 + 60 * SEC));
        assert!(s.entries["x"].running.is_some());
    }

    #[test]
    fn inline_keys_are_stable_hashes() {
        assert_eq!(inline_key(""), "src-cbf29ce484222325");
        assert_eq!(inline_key("a"), "src-af63dc4c8601ec8c");
        assert_ne!(inline_key("date"), inline_key("date "));
    }

    async fn value(out: &[u8]) -> Option<String> {
        let mut l = LastLine::default();
        l.read(out).await;
        l.value()
    }

    /// The cases recorded on tmux 3.7b's `#()`.
    #[tokio::test]
    async fn the_last_non_empty_line_is_kept_as_tmux_keeps_it() {
        let v = |s: &str| Some(s.to_string());
        assert_eq!(value(b"one\ntwo\n").await, v("two"));
        assert_eq!(value(b"one\ntwo\n\n").await, v("two"));
        assert_eq!(value(b"one\ntwo").await, v("two"));
        assert_eq!(value(b"one\n\n\n").await, v("one"));
        assert_eq!(value(b"\n").await, v(""));
        assert_eq!(value(b"x\n  \n").await, v("  "));
        assert_eq!(value(b"").await, v(""));
        assert_eq!(value(b"crlf\r\n").await, v("crlf"));
        // A CR is a line break too (tmux 3.7b, from the review).
        assert_eq!(value(b"a\rb\n").await, v("b"));
        assert_eq!(value(b"p\rq").await, v("q"));
        assert_eq!(value(b"x\r\n\r\n").await, v("x"));
    }

    #[tokio::test]
    async fn a_line_and_the_output_are_bounded() {
        let long = vec![b'y'; MAX_LINE * 4];
        assert_eq!(value(&long).await.map(|l| l.len()), Some(MAX_LINE));
        // A character cut at the limit is dropped, not replaced.
        let mut cut = vec![b'a'; MAX_LINE - 1];
        cut.extend_from_slice("é".as_bytes());
        assert_eq!(value(&cut).await, Some("a".repeat(MAX_LINE - 1)));
        // Past MAX_OUTPUT the rest is not read: the last complete line
        // before it stays.
        let mut spew = b"keep\n".to_vec();
        spew.extend(std::iter::repeat_n(b'y', MAX_OUTPUT));
        spew.extend_from_slice(b"\nlater\n");
        assert_eq!(value(&spew).await.as_deref(), Some("keep"));
        // Split reads join into one line.
        let mut l = LastLine::default();
        l.push(b"tw");
        l.push(b"o\nth");
        l.push(b"ree\n");
        assert_eq!(l.value().as_deref(), Some("three"));
    }

    #[test]
    fn a_run_cut_off_mid_line_keeps_its_partial_line_or_else_the_old_value() {
        let mut l = LastLine::default();
        l.push(b"partial");
        // Not ended, no complete line: the partial text is all there is.
        assert_eq!(l.value().as_deref(), Some("partial"));
        assert_eq!(LastLine::default().value(), None);
    }

    #[test]
    fn register_names_the_option() {
        let mut state = crate::server::test_state();
        assert_eq!(register(&mut state, Some("batt"), "x", None, None), "@batt");
        let inline = register(&mut state, None, "date", None, None);
        assert_eq!(inline, format!("@{}", inline_key("date")));
        assert_eq!(state.sources.entries.len(), 2);
    }

    /// As tmux's `format_job` (`job_run` with `JOB_NOWAIT`): `/bin/sh` with
    /// `argv[0]` `sh`.
    #[tokio::test]
    async fn a_source_runs_bin_sh_as_sh() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let (jobs, _rx) = mpsc::channel(1);
        let cmd = format!("echo \"$0\" > '{}'", out.display());
        run_source(&cmd, Vec::new(), Duration::from_secs(5), &jobs).await;
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "sh\n");
    }

    #[test]
    fn a_value_lands_in_every_option_of_its_command() {
        let mut state = crate::server::test_state();
        register(&mut state, Some("a"), "same", None, None);
        let inline = register(&mut state, None, "same", None, None);
        store(&mut state, "same", "87%");
        let get = |n: &str| {
            state
                .model
                .options
                .get(Owner::Global, n)
                .map(|e| e.value.clone())
        };
        assert_eq!(get("@a").as_deref(), Some("87%"));
        assert_eq!(get(&inline).as_deref(), Some("87%"));
    }
}
