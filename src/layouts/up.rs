//! `layout up`: builds the sessions of a [`Plan`] with ordinary commands.

use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use super::schema::{
    FloatPlan, PanePlan, Plan, Run, SessionPlan, Wait, WaitFor, WindowPlan, session_name_problem,
};
use super::{Server, words};

pub struct UpOptions {
    /// `-d`: never attach.
    pub detach: bool,
    /// `--session`: rename a one-session file, or pick from several.
    pub session: Option<String>,
}

#[derive(Default)]
pub struct UpReport {
    /// What to print.
    pub lines: Vec<String>,
    /// Hooks that failed after the session existed.
    pub warnings: Vec<String>,
    /// The session to attach, if any.
    pub attach: Option<String>,
}

const POLL: Duration = Duration::from_millis(100);

/// Builds every session of `plan` that is not already running.
pub fn up(server: &mut dyn Server, plan: &Plan, opts: &UpOptions) -> Result<UpReport, String> {
    let mut sessions: Vec<SessionPlan> = plan.sessions.clone();
    if let Some(want) = &opts.session {
        if sessions.len() == 1 {
            if let Some(why) = session_name_problem(want) {
                return Err(why.to_string());
            }
            sessions[0].name = want.clone();
        } else {
            sessions.retain(|s| &s.name == want);
            if sessions.is_empty() {
                return Err(format!("no session named {want:?} in this layout"));
            }
        }
    }
    let mut report = UpReport::default();
    let mut created: Vec<String> = Vec::new();
    let mut attach: Option<String> = None;
    for s in &sessions {
        let known = server.sessions()?;
        let id = match known.iter().find(|(_, n)| *n == s.name) {
            Some((id, _)) => {
                report
                    .lines
                    .push(format!("session {} is already running", s.name));
                id.clone()
            }
            None => match build(server, s, &mut report) {
                Ok(id) => {
                    created.push(id.clone());
                    report.lines.push(format!("created session {}", s.name));
                    id
                }
                Err(e) => {
                    for id in &created {
                        let _ = server.run(&words(&["kill-session", "-t", id]));
                    }
                    return Err(e);
                }
            },
        };
        if attach.is_none() && s.attach {
            attach = Some(id);
        }
    }
    if !opts.detach {
        report.attach = attach;
    }
    Ok(report)
}

/// Creates one session; on failure removes what it made.
fn build(
    server: &mut dyn Server,
    s: &SessionPlan,
    report: &mut UpReport,
) -> Result<String, String> {
    if let Some(cmd) = &s.before {
        server
            .host_shell(cmd, &s.root, &s.name)
            .map_err(|e| format!("before hook of {}: {e}", s.name))?;
    }
    let first = &s.windows[0];
    let first_pane = &first.panes[0];
    let mut env = s.env.clone();
    env.extend(first.env.iter().cloned());
    env.extend(first_pane.env.iter().cloned());
    let cwd = first_pane.cwd.display().to_string();
    let mut argv = words(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{session_id} #{window_id} #{pane_id}",
        "-s",
        &s.name,
        "-c",
        &cwd,
    ]);
    if let Some(n) = &first.name {
        argv.extend(["-n".to_string(), n.clone()]);
    }
    if let Run::Exec(c) = &first_pane.run {
        argv.extend(["--".to_string(), c.clone()]);
    }
    let out = server.create_session(&argv)?;
    let ids: Vec<&str> = out.split_whitespace().collect();
    let [sid, wid, pid] = ids.as_slice() else {
        return Err(format!("new-session printed {out:?}"));
    };
    let sid = sid.to_string();
    match populate(server, s, &sid, (wid, pid), report) {
        Ok(()) => Ok(sid),
        Err(e) => {
            let _ = server.run(&words(&["kill-session", "-t", &sid]));
            Err(e)
        }
    }
}

fn populate(
    server: &mut dyn Server,
    s: &SessionPlan,
    sid: &str,
    (wid, pid): (&str, &str),
    report: &mut UpReport,
) -> Result<(), String> {
    // The session's environment is set after the session exists, which is
    // too late for the first pane: it is started again with the environment
    // in place, only when there is one. What only the first window or pane
    // should see is taken back out afterwards.
    let first = &s.windows[0];
    let mut env = s.env.clone();
    env.extend(first.env.iter().cloned());
    env.extend(first.panes[0].env.iter().cloned());
    for (k, v) in &env {
        server.run(&words(&["set-environment", "-t", sid, k, v]))?;
    }
    if !env.is_empty() {
        let cwd = first.panes[0].cwd.display().to_string();
        let mut argv = words(&["respawn-pane", "-k", "-t", pid, "-c", &cwd]);
        if let Run::Exec(c) = &first.panes[0].run {
            argv.extend(["--".to_string(), c.clone()]);
        }
        server.run(&argv)?;
    }
    let mut leaked: Vec<&String> = first.env.iter().map(|(k, _)| k).collect();
    leaked.extend(first.panes[0].env.iter().map(|(k, _)| k));
    restore_env(server, s, sid, &leaked)?;

    if let Some(cmd) = &s.on_start
        && let Err(e) = server.host_shell(cmd, &s.root, &s.name)
    {
        report
            .warnings
            .push(format!("on_start hook of {}: {e}", s.name));
    }
    let mut focus_window = None;
    let mut first_window = None;
    for (i, w) in s.windows.iter().enumerate() {
        let (win, first_pane) = if i == 0 {
            (wid.to_string(), pid.to_string())
        } else {
            new_window(server, s, sid, w)?
        };
        start_pane(server, &first_pane, &w.panes[0])?;
        let mut panes = vec![first_pane];
        for p in &w.panes[1..] {
            let prev = panes.last().cloned().unwrap_or_default();
            let id = split(server, s, sid, w, p, &prev)?;
            start_pane(server, &id, p)?;
            panes.push(id);
        }
        if let Some(layout) = &w.layout {
            server
                .run(&words(&["select-layout", "-t", &win, layout]))
                .map_err(|e| format!("window {}: {e}", w.name.as_deref().unwrap_or("")))?;
        }
        let mut focus_pane = None;
        for (id, p) in panes.iter().zip(&w.panes) {
            if p.focus {
                focus_pane = Some(id.clone());
            }
        }
        let mut last = panes.last().cloned().unwrap_or_default();
        for f in &w.floats {
            let id = float(server, s, sid, &last, f)?;
            if f.focus {
                focus_pane = Some(id.clone());
            }
            last = id;
        }
        let target = focus_pane.unwrap_or_else(|| panes[0].clone());
        server.run(&words(&["select-pane", "-t", &target]))?;
        if w.focus {
            focus_window = Some(win.clone());
        }
        first_window.get_or_insert(win);
    }
    if let Some(win) = focus_window.or(first_window) {
        server.run(&words(&["select-window", "-t", &win]))?;
    }
    if let Some(cmd) = &s.after
        && let Err(e) = server.host_shell(cmd, &s.root, &s.name)
    {
        report
            .warnings
            .push(format!("after hook of {}: {e}", s.name));
    }
    if let Some(cmd) = &s.on_stop {
        server.spawn_watch(sid, &s.name, &s.root, cmd)?;
    }
    Ok(())
}

/// Puts the session's environment back for `keys`: the session's own value,
/// else unset.
fn restore_env(
    server: &mut dyn Server,
    s: &SessionPlan,
    sid: &str,
    keys: &[&String],
) -> Result<(), String> {
    for key in keys {
        let value = s
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == *key)
            .map(|(_, v)| v.clone());
        let argv = match value {
            Some(v) => words(&["set-environment", "-t", sid, key, &v]),
            None => words(&["set-environment", "-t", sid, "-u", key]),
        };
        server.run(&argv)?;
    }
    Ok(())
}

/// Sets `env` in the session, runs `spawn`, takes it back out.
fn with_env<T>(
    server: &mut dyn Server,
    s: &SessionPlan,
    sid: &str,
    env: &[(String, String)],
    spawn: impl FnOnce(&mut dyn Server) -> Result<T, String>,
) -> Result<T, String> {
    for (k, v) in env {
        server.run(&words(&["set-environment", "-t", sid, k, v]))?;
    }
    let result = spawn(server);
    let keys: Vec<&String> = env.iter().map(|(k, _)| k).collect();
    restore_env(server, s, sid, &keys)?;
    result
}

fn new_window(
    server: &mut dyn Server,
    s: &SessionPlan,
    sid: &str,
    w: &WindowPlan,
) -> Result<(String, String), String> {
    let p = &w.panes[0];
    let mut env = w.env.clone();
    env.extend(p.env.iter().cloned());
    let target = format!("{sid}:");
    let cwd = p.cwd.display().to_string();
    let mut argv = words(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{window_id} #{pane_id}",
        "-t",
        &target,
        "-c",
        &cwd,
    ]);
    if let Some(n) = &w.name {
        argv.extend(["-n".to_string(), n.clone()]);
    }
    if let Run::Exec(c) = &p.run {
        argv.extend(["--".to_string(), c.clone()]);
    }
    let out = with_env(server, s, sid, &env, |srv| srv.run(&argv))?;
    let ids: Vec<&str> = out.split_whitespace().collect();
    let [win, pane] = ids.as_slice() else {
        return Err(format!("new-window printed {out:?}"));
    };
    Ok((win.to_string(), pane.to_string()))
}

fn split(
    server: &mut dyn Server,
    s: &SessionPlan,
    sid: &str,
    w: &WindowPlan,
    p: &PanePlan,
    prev: &str,
) -> Result<String, String> {
    let mut env = w.env.clone();
    env.extend(p.env.iter().cloned());
    let cwd = p.cwd.display().to_string();
    let mut argv = words(&[
        "split-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        if w.side_by_side { "-h" } else { "-v" },
        "-t",
        prev,
        "-c",
        &cwd,
    ]);
    if let Some(size) = &p.size {
        argv.extend(["-l".to_string(), size.clone()]);
    }
    if let Run::Exec(c) = &p.run {
        argv.extend(["--".to_string(), c.clone()]);
    }
    let out = with_env(server, s, sid, &env, |srv| srv.run(&argv))?;
    Ok(out.trim().to_string())
}

fn float(
    server: &mut dyn Server,
    s: &SessionPlan,
    sid: &str,
    after: &str,
    f: &FloatPlan,
) -> Result<String, String> {
    let cwd = f.cwd.display().to_string();
    let mut argv = words(&[
        "new-pane",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        after,
        "-c",
        &cwd,
    ]);
    for (flag, v) in [
        ("-x", &f.width),
        ("-y", &f.height),
        ("-X", &f.x),
        ("-Y", &f.y),
    ] {
        if let Some(v) = v {
            argv.extend([flag.to_string(), v.clone()]);
        }
    }
    if let Run::Exec(c) = &f.run {
        argv.extend(["--".to_string(), c.clone()]);
    }
    let out = with_env(server, s, sid, &f.env, |srv| srv.run(&argv))?;
    let id = out.trim().to_string();
    if let Run::Typed(c) = &f.run {
        type_into(server, &id, c)?;
    }
    if let Some(b) = &f.border {
        server.run(&words(&[
            "set-option",
            "-p",
            "-t",
            &id,
            "pane-border-lines",
            b,
        ]))?;
    }
    if let Some(t) = &f.title {
        server.run(&words(&["select-pane", "-t", &id, "-T", t]))?;
    }
    Ok(id)
}

/// A pane the server already made: types its command, then waits.
fn start_pane(server: &mut dyn Server, id: &str, p: &PanePlan) -> Result<(), String> {
    if let Run::Typed(c) = &p.run {
        type_into(server, id, c)?;
    }
    match &p.wait {
        Some(w) => wait_for(server, id, w),
        None => Ok(()),
    }
}

fn type_into(server: &mut dyn Server, id: &str, command: &str) -> Result<(), String> {
    server.run(&words(&["send-keys", "-t", id, "-l", "--", command]))?;
    server.run(&words(&["send-keys", "-t", id, "Enter"]))?;
    Ok(())
}

fn wait_for(server: &mut dyn Server, id: &str, w: &Wait) -> Result<(), String> {
    let deadline = Instant::now() + w.timeout;
    let re = match &w.what {
        WaitFor::Regex(r) => Some(regex::Regex::new(r).map_err(|e| e.to_string())?),
        WaitFor::Path(_) => None,
    };
    loop {
        let met = match (&w.what, &re) {
            (WaitFor::Path(p), _) => p.exists(),
            (_, Some(re)) => {
                let screen = server.run(&words(&["capture-pane", "-p", "-t", id]))?;
                re.is_match(&screen)
            }
            _ => false,
        };
        if met {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let what = match &w.what {
                WaitFor::Regex(r) => format!("/{r}/ on the screen of {id}"),
                WaitFor::Path(p) => format!("{} to exist", p.display()),
            };
            return Err(format!(
                "timed out after {}s waiting for {what}",
                w.timeout.as_secs()
            ));
        }
        sleep(POLL);
    }
}

/// `layout __watch`: waits for the session to end (or its server to go),
/// then runs the `on_stop` hook.
pub fn watch(
    server: &mut dyn Server,
    id: &str,
    name: &str,
    root: &Path,
    command: &str,
) -> Result<(), String> {
    loop {
        match server.sessions() {
            Ok(list) if list.iter().any(|(i, _)| i == id) => sleep(Duration::from_millis(500)),
            _ => break,
        }
    }
    server.host_shell(command, root, name)
}
