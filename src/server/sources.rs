//! Status sources (spec D8, §4.10): commands the server runs on a schedule,
//! off the redraw path, whose first output line is kept in an option the
//! status line reads.
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

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::{mpsc, watch};

use crate::config::Resolved;
use crate::event::Event;
use crate::server::dispatch::options::Owner;
use crate::server::{Job, State, clients, hooks, pane};

/// How long a run may take when its registration names no timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of a first line kept, in bytes. A source that prints more
/// without a newline is cut here and its pipe closed, so a chatty command
/// costs the server at most this much.
pub const MAX_LINE: usize = 4096;

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
    /// When the next run is due; `None` means never again (no interval).
    next: Option<Instant>,
    /// The run in flight, by number.
    running: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Registration {
    /// `None` follows the global `status-interval`.
    interval: Option<Duration>,
    timeout: Duration,
    /// Declared by the config, so `configure` replaces it on reload.
    config: bool,
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
            e.next = e.interval(status_interval).map(|iv| {
                let next = at + iv;
                if next > now { next } else { now + iv }
            });
            if e.running.is_none() {
                self.runs += 1;
                e.running = Some(self.runs);
                start.push((cmd.clone(), e.timeout(), self.runs));
            }
        }
        start
    }

    /// The earliest time an entry is due.
    fn earliest(&self) -> Option<Instant> {
        self.entries.values().filter_map(|e| e.next).min()
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
/// command again only updates its interval and timeout. This is the one
/// function the tmux front end calls for `#(cmd)`.
// ponytail: an inline source stays registered until the server exits, even
// once no format uses it; a reference count would need the front end to
// unregister.
pub fn register(
    state: &mut State,
    name: Option<&str>,
    cmd: &str,
    interval: Option<Duration>,
    timeout: Option<Duration>,
) -> String {
    let key = name.map_or_else(|| inline_key(cmd), str::to_string);
    let reg = Registration {
        interval,
        timeout: timeout.unwrap_or(DEFAULT_TIMEOUT),
        config: false,
    };
    state.sources.add(&key, cmd, reg, Instant::now());
    state.sources.prune();
    reschedule(state);
    format!("@{key}")
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

/// On the actor: starts every due source and publishes the next due time.
fn tick(state: &mut State) {
    let status_interval = state
        .model
        .options
        .lookup("status-interval", &[])
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_secs);
    for (cmd, timeout, run) in state.sources.due(Instant::now(), status_interval) {
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

/// Runs `cmd` once: sends its first line to the actor as soon as it is read,
/// then waits for the shell, killing its process group at `timeout`.
async fn run_source(
    cmd: &str,
    env: Vec<(String, String)>,
    timeout: Duration,
    jobs: &mpsc::Sender<Job>,
) {
    let child = {
        let _lock = pane::spawn_lock();
        tokio::process::Command::new("/bin/sh")
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
    let work = async {
        if let Some(out) = stdout
            && let Some(line) = first_line(out).await
        {
            let cmd = cmd.to_string();
            let _ = jobs
                .send(Box::new(move |s: &mut State| store(s, &cmd, &line)))
                .await;
        }
        child.wait().await
    };
    if tokio::time::timeout(timeout, work).await.is_err() {
        group.kill();
        let _ = child.wait().await;
    }
    group.disarm();
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

/// The first line of `out`, without its line ending, cut at [`MAX_LINE`]
/// bytes (on a character boundary). Output with no newline counts as one
/// line; none at all is an empty line. `None` on a read error. The pipe is
/// closed on return, so a command still writing gets `SIGPIPE`.
async fn first_line(out: impl AsyncRead + Unpin) -> Option<String> {
    let mut buf = Vec::new();
    BufReader::new(out.take(MAX_LINE as u64))
        .read_until(b'\n', &mut buf)
        .await
        .ok()?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if let Err(e) = std::str::from_utf8(&buf)
        && e.error_len().is_none()
    {
        buf.truncate(e.valid_up_to());
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
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
    fn the_status_interval_applies_and_zero_runs_once() {
        let mut s = Sources::default();
        let t0 = Instant::now();
        s.add("a", "date", reg(None), t0);
        assert_eq!(s.due(t0, 5 * SEC).len(), 1);
        assert_eq!(s.earliest(), Some(t0 + 5 * SEC));
        let mut s = Sources::default();
        s.add("a", "date", reg(None), t0);
        assert_eq!(s.due(t0, Duration::ZERO).len(), 1);
        assert_eq!(s.earliest(), None);
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

    #[tokio::test]
    async fn the_first_line_is_kept_and_bounded() {
        assert_eq!(first_line(&b"one\ntwo\n"[..]).await.as_deref(), Some("one"));
        assert_eq!(first_line(&b"crlf\r\nx"[..]).await.as_deref(), Some("crlf"));
        assert_eq!(
            first_line(&b"no newline"[..]).await.as_deref(),
            Some("no newline")
        );
        assert_eq!(first_line(&b""[..]).await.as_deref(), Some(""));
        let long = vec![b'y'; MAX_LINE * 4];
        assert_eq!(first_line(&long[..]).await.map(|l| l.len()), Some(MAX_LINE));
        // A character cut at the limit is dropped, not replaced.
        let mut cut = vec![b'a'; MAX_LINE - 1];
        cut.extend_from_slice("é".as_bytes());
        assert_eq!(first_line(&cut[..]).await, Some("a".repeat(MAX_LINE - 1)));
    }

    #[test]
    fn register_names_the_option() {
        let mut state = crate::server::test_state();
        assert_eq!(register(&mut state, Some("batt"), "x", None, None), "@batt");
        let inline = register(&mut state, None, "date", None, None);
        assert_eq!(inline, format!("@{}", inline_key("date")));
        assert_eq!(state.sources.entries.len(), 2);
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
