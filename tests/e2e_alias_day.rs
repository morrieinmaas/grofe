//! TA.1, stage 3: the owner's workflow through a pty, in the alias-day
//! container (`scripts/e2e-alias-day.sh`). Ignored by default, and a no-op
//! unless `/run/.containerenv` exists and `GROFE_E2E_ALIAS_DAY=1`.
//!
//! A login zsh starts in a pty at 200x50 with `TERM=xterm-256color`, and
//! `tmux` is typed into it as the owner does; athome's login unit has
//! already started the server. The steps are TE.2b's W1 to W12 and W14
//! (`.superpowers/sdd/m35/e2e-athome-amendment.md`, section 4, stage 3),
//! without W13 (M7, dropped by the owner), plus TA.1's W15 (copy mode over a soft wrap)
//! and W16 (tmux-yank's `y`). The keys are the owner's: prefix `C-s`.
//!
//! Each step runs to its end even when an earlier one failed, and writes a
//! row to `$GROFE_E2E_OUT/steps.tsv` and the screen it ended on to
//! `$GROFE_E2E_OUT/snapshots/<id>.txt`; the test fails if any step did.
//! Reuses T2.4's pty harness through a `#[path]` module, unedited.

#[path = "acceptance/harness.rs"]
#[allow(dead_code)]
mod harness;

use std::fs;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use harness::Harness;

const COLS: u16 = 200;
const ROWS: u16 = 50;
const PREFIX: &str = "\x13";
/// Longer than tmux's default repeat-time (500 ms), so the key after a
/// repeatable binding is read in the root table again.
const AFTER_REPEAT: Duration = Duration::from_millis(800);
const WRAP_LINE: &str = r#"printf "w%03d " $(seq 1 50); printf "\n"; exec sleep 100000"#;

type Step = Result<String, String>;

struct Ctx {
    h: Harness,
    tmux: PathBuf,
    home: PathBuf,
    out: PathBuf,
    marks: u32,
    rows: Vec<String>,
    failed: Vec<String>,
}

fn enabled() -> bool {
    Path::new("/run/.containerenv").exists()
        && std::env::var("GROFE_E2E_ALIAS_DAY").as_deref() == Ok("1")
}

impl Ctx {
    /// Runs the tmux CLI (grofe, through the symlink a login shell finds),
    /// bounded, and returns its stdout, or its stderr as the error.
    fn tmux(&mut self, args: &[&str]) -> Result<String, String> {
        self.h.text();
        let out = Command::new("timeout")
            .arg("15")
            .arg(&self.tmux)
            .args(args)
            .env_remove("TMUX")
            .output()
            .map_err(|e| format!("tmux {args:?}: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        } else {
            Err(format!(
                "tmux {args:?} exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim_end()
            ))
        }
    }

    /// The one attached client's view of `fmt`.
    fn client(&mut self, fmt: &str) -> Result<String, String> {
        let out = self.tmux(&["list-clients", "-F", fmt])?;
        let mut lines = out.lines();
        match (lines.next(), lines.next()) {
            (Some(one), None) => Ok(one.to_string()),
            (None, _) => Err("no client is attached".to_string()),
            _ => Err(format!("more than one client:\n{out}")),
        }
    }

    /// `fmt` for the attached client's active pane.
    fn pane(&mut self, fmt: &str) -> Result<String, String> {
        let id = self.client("#{pane_id}")?;
        self.tmux(&["display", "-p", "-t", &id, fmt])
    }

    /// One line per pane of the client's window: `fmt` for each.
    fn panes(&mut self, fmt: &str) -> Result<Vec<String>, String> {
        let window = self.client("#{window_id}")?;
        Ok(self
            .tmux(&["list-panes", "-t", &window, "-F", fmt])?
            .lines()
            .map(str::to_string)
            .collect())
    }

    fn send(&mut self, keys: &str) {
        self.h.send(keys);
    }

    /// The prefix, then `key`, as typed.
    fn prefix(&mut self, key: &str) {
        self.h.send(PREFIX);
        sleep(Duration::from_millis(80));
        self.h.send(key);
        sleep(Duration::from_millis(80));
    }

    /// Polls `probe` until it answers, pumping the pty meanwhile.
    fn wait<T>(
        &mut self,
        what: &str,
        secs: u64,
        mut probe: impl FnMut(&mut Ctx) -> Option<T>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = probe(self) {
                return Ok(v);
            }
            if Instant::now() >= deadline {
                return Err(format!("timed out after {secs} s waiting for {what}"));
            }
            self.h.text();
            sleep(Duration::from_millis(100));
        }
    }

    fn wait_screen(
        &mut self,
        what: &str,
        secs: u64,
        f: impl Fn(&str) -> bool,
    ) -> Result<(), String> {
        self.wait(what, secs, |c| f(&c.h.text()).then_some(()))
    }

    /// Types `cmd` into the active pane's shell and waits until it is done:
    /// a marker the typed line cannot show (the shell computes it) appears.
    fn shell(&mut self, cmd: &str) -> Result<(), String> {
        self.marks += 1;
        let n = self.marks;
        self.send(&format!("{cmd}; echo AD-$((1000+{n}))-DONE\r"));
        let want = format!("AD-{}-DONE", 1000 + n);
        self.wait_screen(&format!("{cmd:?} to finish"), 30, |s| s.contains(&want))
    }

    fn snapshot(&mut self, id: &str) -> String {
        let text = self.h.text();
        let rel = format!("snapshots/{id}.txt");
        let _ = fs::write(self.out.join(&rel), &text);
        rel
    }

    /// Runs one step, recording its result whether it passed, failed or
    /// panicked (a harness timeout).
    fn step(&mut self, id: &str, name: &str, f: impl FnOnce(&mut Ctx) -> Step) {
        eprintln!("== {id} {name}");
        let result = catch_unwind(AssertUnwindSafe(|| f(self)))
            .unwrap_or_else(|p| Err(panic_text(p.as_ref())));
        let snap = self.snapshot(id);
        let (verdict, note) = match result {
            Ok(note) => ("pass", note),
            Err(e) => {
                self.failed.push(format!("{id}: {e}"));
                self.recover();
                ("fail", e)
            }
        };
        eprintln!("   {verdict}: {note}");
        let note = note.replace(['\t', '\n'], " ");
        self.rows
            .push(format!("{id}\t{name}\t{verdict}\t{snap}\t{note}"));
        let _ = fs::write(self.out.join("steps.tsv"), self.rows.join("\n") + "\n");
    }
}

impl Ctx {
    /// After a failed step: close whatever it left open (a popup, a menu,
    /// copy mode, a half-typed line), so the next step starts at a prompt.
    fn recover(&mut self) {
        for keys in ["\x1b", "\x1b", "\x03"] {
            self.send(keys);
            sleep(Duration::from_millis(300));
        }
        if self.pane("#{pane_in_mode}").ok().as_deref() == Some("1") {
            let _ = self.tmux(&["send-keys", "-X", "cancel"]);
        }
        // A program still in the foreground (an editor the step started)
        // would take the next step's typing: kill its pane, so the client
        // lands on a shell.
        let cmd = self.pane("#{pane_current_command}").unwrap_or_default();
        if !matches!(cmd.trim_start_matches('-'), "zsh" | "bash" | "sh") {
            let _ = self.tmux(&["kill-pane"]);
            sleep(Duration::from_millis(500));
        }
        let _ = self.shell("true");
    }
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "panicked".to_string()
    }
}

fn ensure(cond: bool, msg: impl FnOnce() -> String) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg()) }
}

/// `#rrggbb` as the truecolor SGR parameters an outer terminal receives.
fn sgr_rgb(hex: &str) -> Result<String, String> {
    let h = hex.trim().trim_start_matches('#');
    let byte = |i: usize| {
        u8::from_str_radix(h.get(i..i + 2).unwrap_or(""), 16)
            .map_err(|_| format!("not a colour: {hex}"))
    };
    Ok(format!("2;{};{};{}", byte(0)?, byte(2)?, byte(4)?))
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A pane's geometry, from `list-panes -F GEOM`.
const GEOM: &str =
    "#{pane_id} #{pane_left} #{pane_top} #{pane_width} #{pane_height} #{pane_active}";

#[derive(Debug, Clone)]
struct Geom {
    id: String,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    active: bool,
}

fn geoms(lines: &[String]) -> Vec<Geom> {
    lines
        .iter()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            Some(Geom {
                id: f.first()?.to_string(),
                left: f.get(1)?.parse().ok()?,
                top: f.get(2)?.parse().ok()?,
                width: f.get(3)?.parse().ok()?,
                height: f.get(4)?.parse().ok()?,
                active: *f.get(5)? == "1",
            })
        })
        .collect()
}

impl Ctx {
    fn geoms(&mut self) -> Result<Vec<Geom>, String> {
        Ok(geoms(&self.panes(GEOM)?))
    }

    fn active(&mut self) -> Result<Geom, String> {
        self.geoms()?
            .into_iter()
            .find(|g| g.active)
            .ok_or_else(|| "no active pane".to_string())
    }

    /// The window is in main-vertical: one pane at the left edge running
    /// the window's full height, the others stacked at its right.
    fn cockpit(&mut self) -> Result<Vec<Geom>, String> {
        let height = self.client("#{window_height}")?.parse::<u32>().unwrap_or(0);
        let g = self.geoms()?;
        let mains: Vec<&Geom> = g.iter().filter(|p| p.left == 0).collect();
        ensure(g.len() == 3, || format!("{} panes, not 3: {g:?}", g.len()))?;
        ensure(mains.len() == 1 && mains[0].height == height, || {
            format!("no full-height left pane (window height {height}): {g:?}")
        })?;
        let right: Vec<&Geom> = g.iter().filter(|p| p.left > 0).collect();
        ensure(
            right.len() == 2 && right[0].left == right[1].left && right[0].top != right[1].top,
            || format!("the right two are not stacked: {g:?}"),
        )?;
        Ok(g)
    }

    fn session(&mut self) -> Result<String, String> {
        self.client("#{client_session}")
    }

    fn wait_session(&mut self, want: &str) -> Result<(), String> {
        self.wait(&format!("the client in session {want}"), 10, |c| {
            (c.session().ok().as_deref() == Some(want)).then_some(())
        })
    }

    /// Escape closes a popup or menu; wait until no popup is drawn over the
    /// client by checking the screen no longer contains `marker`.
    fn close_with_escape(&mut self, marker: &str) -> Result<(), String> {
        self.send("\x1b");
        self.wait_screen(&format!("{marker:?} to close"), 15, |s| !s.contains(marker))
    }

    fn no_error_line(&mut self) -> Result<(), String> {
        let text = self.h.text();
        let top: String = text.lines().take(2).collect::<Vec<_>>().join("\n");
        for bad in [
            "returned 1",
            "returned 127",
            "not found",
            "unknown command",
            "error",
        ] {
            ensure(!top.to_lowercase().contains(bad), || {
                format!("the status area shows {bad:?}:\n{top}")
            })?;
        }
        Ok(())
    }
}

fn w1(c: &mut Ctx) -> Step {
    c.send("tmux\r");
    let session = c.wait("a client to attach", 20, |c| c.session().ok())?;
    let time = regex::Regex::new(r"\b\d\d:\d\d\b").unwrap();
    let s = session.clone();
    c.wait_screen("the two-row status line at the top", 20, |text| {
        let top: Vec<&str> = text.lines().take(2).collect();
        top.iter().any(|l| l.contains(&format!(" {s} "))) && top.iter().any(|l| time.is_match(l))
    })?;
    // The status sources settle within one status-interval (5 s).
    sleep(Duration::from_secs(6));
    let text = c.h.text();
    for raw in ["#(", "#{", "#["] {
        ensure(!text.contains(raw), || format!("a raw {raw:?} on screen"))?;
    }
    let lines = c.client("#{status}")?;
    ensure(lines == "2", || format!("status is {lines:?}, not 2 rows"))?;
    let top: Vec<&str> = text.lines().take(2).collect();
    Ok(format!(
        "session {session}; status row 1: {:?}",
        top[0].trim_end()
    ))
}

fn w2(c: &mut Ctx) -> Step {
    let red = sgr_rgb(&c.tmux(&["show", "-gv", "@theme_red"])?)?;
    let blue = sgr_rgb(&c.tmux(&["show", "-gv", "@theme_blue"])?)?;
    let from = c.h.output().len();
    c.send(PREFIX);
    c.wait("client_prefix to be 1", 5, |c| {
        (c.client("#{client_prefix}").ok()? == "1").then_some(())
    })?;
    let at = c.h.output().len();
    c.wait("the prefix colour in the outer stream", 5, |c| {
        let out = c.h.output();
        let seen = &out[from.min(out.len())..];
        String::from_utf8_lossy(seen).contains(&red).then_some(())
    })?;
    // An unbound key in the prefix table goes back to root.
    c.send("\x1b");
    c.wait("client_prefix to be 0", 5, |c| {
        (c.client("#{client_prefix}").ok()? == "0").then_some(())
    })?;
    c.wait("the session colour back in the outer stream", 5, |c| {
        let out = c.h.output();
        String::from_utf8_lossy(&out[at.min(out.len())..])
            .contains(&blue)
            .then_some(())
    })?;
    Ok(format!("prefix pill {red}, back to {blue}"))
}

fn w3(c: &mut Ctx) -> Step {
    c.shell("cd /tmp && clear")?;
    let before = c.geoms()?;
    ensure(before.len() == 1, || {
        format!("{} panes before the splits", before.len())
    })?;
    c.prefix("|");
    c.wait("a pane to the right", 10, |c| {
        (c.geoms().ok()?.len() == 2).then_some(())
    })?;
    let right = c.active()?;
    ensure(right.left > 0 && right.top == before[0].top, || {
        format!("not split right: {right:?}")
    })?;
    c.wait("the new shell's prompt", 15, |c| {
        let path = c.pane("#{pane_current_path}").ok()?;
        (path == "/tmp").then_some(())
    })?;
    sleep(Duration::from_millis(500));
    c.shell("pwd")?;
    let shown = c.tmux(&["capture-pane", "-p", "-t", &right.id])?;
    ensure(shown.lines().any(|l| l.trim_end() == "/tmp"), || {
        format!("pwd in the right pane did not print /tmp:\n{shown}")
    })?;
    c.prefix("-");
    c.wait("a pane below", 10, |c| {
        (c.geoms().ok()?.len() == 3).then_some(())
    })?;
    let below = c.active()?;
    ensure(below.left == right.left && below.top > right.top, || {
        format!("not split below: {below:?}")
    })?;
    c.wait("the new shell in /tmp", 15, |c| {
        (c.pane("#{pane_current_path}").ok()? == "/tmp").then_some(())
    })?;
    sleep(Duration::from_millis(500));
    c.shell("pwd")?;
    let shown = c.tmux(&["capture-pane", "-p", "-t", &below.id])?;
    ensure(shown.lines().any(|l| l.trim_end() == "/tmp"), || {
        format!("pwd in the lower pane did not print /tmp:\n{shown}")
    })?;
    Ok("split right and below, each in /tmp".to_string())
}

fn w4(c: &mut Ctx) -> Step {
    c.prefix("h");
    c.wait("focus on the left pane", 5, |c| {
        (c.active().ok()?.left == 0).then_some(())
    })?;
    c.prefix("l");
    let right = c.wait("focus on a right pane", 5, |c| {
        c.active().ok().filter(|g| g.left > 0)
    })?;
    // The right column holds two panes; k and j move between them (select-pane
    // wraps at an edge, so the order they are in does not matter).
    c.prefix("k");
    c.wait("focus on the other right pane", 5, |c| {
        c.active().ok().filter(|g| g.left > 0 && g.id != right.id)
    })?;
    c.prefix("j");
    c.wait("focus back on the first right pane", 5, |c| {
        c.active().ok().filter(|g| g.id == right.id)
    })?;
    let w0 = c.active()?.width;
    c.send(PREFIX);
    for _ in 0..3 {
        sleep(Duration::from_millis(120));
        c.send("H");
    }
    let w1 = c.wait("the pane to grow three steps", 5, |c| {
        let w = c.active().ok()?.width;
        (w >= w0 + 15).then_some(w)
    })?;
    sleep(AFTER_REPEAT);
    ensure(w1 == w0 + 15, || {
        format!("width {w0} grew to {w1}, not by 15")
    })?;
    Ok(format!(
        "h l k j moved focus; prefix H H H grew {w0} to {w1}"
    ))
}

fn w5(c: &mut Ctx) -> Step {
    let windows = c.client("#{session_windows}")?.parse::<u32>().unwrap_or(0);
    c.prefix("c");
    c.wait("a new window with three panes", 10, |c| {
        let n = c.client("#{session_windows}").ok()?.parse::<u32>().ok()?;
        (n == windows + 1 && c.geoms().ok()?.len() == 3).then_some(())
    })?;
    sleep(Duration::from_millis(500));
    let g = c.cockpit()?;
    let active = g.iter().find(|p| p.active).cloned();
    ensure(active.as_ref().is_some_and(|a| a.left == 0), || {
        format!("the left pane is not focused: {g:?}")
    })?;
    Ok(format!("cockpit window: {g:?}"))
}

fn w6(c: &mut Ctx) -> Step {
    let windows = c.client("#{session_windows}")?.parse::<u32>().unwrap_or(0);
    c.prefix("C");
    c.wait("a plain new window", 10, |c| {
        let n = c.client("#{session_windows}").ok()?.parse::<u32>().ok()?;
        (n == windows + 1 && c.geoms().ok()?.len() == 1).then_some(())
    })?;
    c.prefix("|");
    c.wait("two panes", 10, |c| {
        (c.geoms().ok()?.len() == 2).then_some(())
    })?;
    c.prefix("-");
    c.wait("three panes", 10, |c| {
        (c.geoms().ok()?.len() == 3).then_some(())
    })?;
    c.prefix("V");
    c.wait("the cockpit reshape", 10, |c| c.cockpit().ok())?;

    let first = c.session()?;
    c.tmux(&["new", "-d", "-s", "second"])?;
    c.prefix(")");
    c.wait_session("second")?;
    sleep(AFTER_REPEAT);
    c.prefix("(");
    c.wait_session(&first)?;
    sleep(AFTER_REPEAT);
    c.prefix("t");
    c.wait_session("second")?;
    c.prefix("t");
    c.wait_session(&first)?;
    Ok(format!(
        "C plain, V cockpit; ) ( t t cycled {first} -> second -> {first} -> second -> {first}"
    ))
}

fn w7(c: &mut Ctx) -> Step {
    let user = c.home.join(".config/grofe/user.toml");
    let original = fs::read_to_string(&user).map_err(|e| format!("{}: {e}", user.display()))?;
    let marker = "ALIASDAY-W7-BOUND";
    let edited = original.replacen(
        "[keys.prefix]\n",
        &format!("[keys.prefix]\nF9 = \"display-message {marker}\"\n"),
        1,
    );
    ensure(edited != original, || {
        "user.toml has no [keys.prefix] table".to_string()
    })?;
    let result = (|| {
        fs::write(&user, &edited).map_err(|e| e.to_string())?;
        c.shell("clear")?;
        c.prefix("r");
        c.wait_screen("canopy: reloaded", 15, |s| s.contains("canopy: reloaded"))?;
        c.no_error_line()?;
        c.prefix("\x1b[20~");
        c.wait_screen("the binding added before the reload", 10, |s| {
            s.contains(marker)
        })?;
        Ok::<_, String>(())
    })();
    fs::write(&user, &original).map_err(|e| e.to_string())?;
    sleep(Duration::from_secs(1));
    c.prefix("r");
    c.wait_screen("canopy: reloaded, again", 15, |s| {
        s.contains("canopy: reloaded")
    })?;
    result?;
    Ok(format!(
        "reloaded; prefix F9, bound in user.toml before it, showed {marker}"
    ))
}

fn w8(c: &mut Ctx) -> Step {
    let nested = |c: &mut Ctx, args: &[&str]| -> Result<String, String> {
        let out = Command::new("/usr/local/bin/tmux")
            .args(["-L", "nested"])
            .args(args)
            .env_remove("TMUX")
            .output()
            .map_err(|e| e.to_string())?;
        c.h.text();
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    };
    c.shell("clear")?;
    c.send(
        "env -u TMUX /usr/local/bin/tmux -L nested -f /dev/null new-session -s nest \\; set -g prefix C-s \\; set -g status-position bottom\r",
    );
    c.wait_screen("the nested tmux's status line", 15, |s| {
        s.contains("[nest]")
    })?;
    c.wait("the nested tmux to take C-s as its prefix", 10, |c| {
        (nested(c, &["show", "-gv", "prefix"]).ok()? == "C-s").then_some(())
    })?;
    c.prefix("\x13");
    c.send("c");
    let res = c.wait("a second window in the nested tmux", 10, |c| {
        (nested(c, &["list-windows", "-t", "nest"])
            .ok()?
            .lines()
            .count()
            == 2)
            .then_some(())
    });
    nested(c, &["kill-server"])?;
    // tmux prints "[exited]" when its last session ends and "[server
    // exited]" after kill-server; either way the shell has the pane back.
    c.wait_screen("the nested tmux to exit", 15, |s| s.contains("exited]"))?;
    res?;
    Ok("prefix C-s reached the nested real tmux as its prefix; its c made window 2".to_string())
}

fn w9(c: &mut Ctx) -> Step {
    let mut notes = Vec::new();
    c.shell("clear")?;
    c.prefix("K");
    c.wait_screen("tsess's picker", 20, |s| {
        s.contains("sessions") && s.contains("tsess")
    })?;
    c.close_with_escape("tsess — prefix K")?;
    c.no_error_line()?;
    notes.push("K tsess opened and closed");

    c.prefix("?");
    c.wait_screen("the keys popup", 20, |s| s.contains("keys>"))?;
    // The list opens on the first table by name (copy-mode); a search
    // brings up the prefix table's splits.
    c.send("split-window");
    c.wait_screen("the prefix table's split bindings", 10, |s| {
        s.lines()
            .any(|l| l.contains("[prefix]") && l.contains("split-window"))
    })?;
    c.close_with_escape("keys>")?;
    c.no_error_line()?;
    notes.push("? listed the bindings");

    c.prefix("T");
    c.wait_screen("the theme picker", 20, |s| s.contains("gruvbox"))?;
    c.close_with_escape("gruvbox")?;
    c.no_error_line()?;
    notes.push("T theme picker opened and closed");
    Ok(notes.join("; "))
}

fn w10(c: &mut Ctx) -> Step {
    let mut notes = Vec::new();
    c.shell("clear && echo https://example.com/alias-day")?;

    c.prefix("s");
    c.wait_screen("sessionx", 20, |s| s.contains("second"))?;
    sleep(Duration::from_millis(500));
    c.send("\x1b");
    sleep(Duration::from_secs(1));
    c.no_error_line()?;
    notes.push("s sessionx opened and closed");

    c.prefix(" ");
    c.wait_screen("which-key's menu", 15, |s| {
        s.contains("+Windows") || s.contains("+Panes")
    })?;
    let menu = c.h.text();
    c.close_with_escape(if menu.contains("+Windows") {
        "+Windows"
    } else {
        "+Panes"
    })?;
    c.no_error_line()?;
    notes.push("Space which-key listed its groups");

    // The popup can cover the pane the URL was echoed in, so the list is
    // known by fzf's pointer on the URL's row, not by a second copy.
    let listed = |s: &str| {
        s.lines()
            .any(|l| l.contains('▌') && l.contains("https://example.com/alias-day"))
    };
    c.prefix("u");
    c.wait_screen("fzf-url's list", 20, listed)?;
    c.send("\x1b");
    c.wait_screen("fzf-url to close", 15, |s| !listed(s))?;
    c.no_error_line()?;
    notes.push("u fzf-url listed the URL");

    let n = c.geoms()?.len();
    c.prefix("o");
    c.send("t");
    c.wait("opensessions' sidebar pane", 20, |c| {
        (c.geoms().ok()?.len() == n + 1).then_some(())
    })?;
    c.prefix("o");
    c.send("t");
    c.wait("the sidebar to close", 20, |c| {
        (c.geoms().ok()?.len() == n).then_some(())
    })?;
    c.no_error_line()?;
    notes.push("o t toggled opensessions' sidebar");

    c.prefix("a");
    c.wait_screen("the claude session popup", 20, |s| {
        s.contains("claude stub")
    })?;
    c.send("\r");
    c.wait_screen("the claude popup to close", 15, |s| {
        !s.contains("claude stub")
    })?;
    c.no_error_line()?;
    notes.push("a launched claude (a stub) in its popup");

    c.prefix("A");
    c.wait_screen("the claude session picker", 20, |s| {
        s.contains(" Sessions ") && s.contains("Claude agents")
    })?;
    c.send("\x1b");
    c.wait_screen("the claude picker to close", 15, |s| {
        !s.contains(" Sessions ")
    })?;
    c.no_error_line()?;
    notes.push("A opened the claude picker");
    Ok(notes.join("; "))
}

fn w11(c: &mut Ctx) -> Step {
    c.prefix("C");
    c.wait("a fresh window", 10, |c| {
        (c.geoms().ok()?.len() == 1).then_some(())
    })?;
    sleep(Duration::from_millis(500));
    c.prefix("|");
    c.wait("two panes", 10, |c| {
        (c.geoms().ok()?.len() == 2).then_some(())
    })?;
    let left = c
        .geoms()?
        .into_iter()
        .find(|g| g.left == 0)
        .ok_or("no left pane")?;
    let right = c.active()?;
    sleep(Duration::from_secs(1));
    c.send("cd /tmp && nvim -O ad-left.txt ad-right.txt\r");
    // Every failure from here on quits nvim, so the next step types into
    // a shell, not into an nvim left in insert mode.
    let result = (|| {
        c.wait("nvim in the right pane", 30, |c| {
            (c.pane("#{pane_current_command}").ok()? == "nvim").then_some(())
        })?;
        c.wait_screen("nvim's two splits", 30, |s| {
            s.contains("ad-left.txt") && s.contains("ad-right.txt")
        })?;
        nvim_navigation(c, &left, &right)
    })();
    if result.is_err() {
        let _ = c.tmux(&["select-pane", "-t", &right.id]);
        nvim_cmd(c, "qa!");
        let _ = c.wait("nvim to quit after the failure", 10, |c| {
            (c.pane("#{pane_current_command}").ok()? != "nvim").then_some(())
        });
    }
    result
}

/// Runs an Ex command in nvim, from any mode. athome's keymaps.lua swaps
/// `;` and `:` in normal mode, so `;` opens the command line. Escape goes
/// first and on its own: within escape-time it and the `;` would be read
/// as M-;.
fn nvim_cmd(c: &mut Ctx, cmd: &str) {
    c.send("\x1b");
    sleep(Duration::from_millis(300));
    c.send(&format!(";{cmd}\r"));
}

fn nvim_navigation(c: &mut Ctx, left: &Geom, right: &Geom) -> Step {
    let ask = |c: &mut Ctx| nvim_cmd(c, "echo 'ADWIN'.winnr()");
    let win = |c: &mut Ctx, want: u32| -> Result<(), String> {
        ask(c);
        let w = format!("ADWIN{want}");
        c.wait_screen(&format!("nvim window {want}"), 10, |s| s.contains(&w))
    };
    // Stage 2 ran nvim once, so its plugins are installed; it answers once
    // its VimEnter setups have run.
    c.wait("nvim to answer a command", 30, |c| {
        ask(c);
        sleep(Duration::from_secs(1));
        c.h.text().contains("ADWIN1").then_some(())
    })?;
    c.send("\x0c");
    sleep(Duration::from_millis(500));
    win(c, 2)?;
    ensure(c.active()?.id == right.id, || {
        "C-l inside nvim moved the tmux pane".to_string()
    })?;
    c.send("\x08");
    sleep(Duration::from_millis(500));
    win(c, 1)?;
    ensure(c.active()?.id == right.id, || {
        "C-h in nvim's middle moved the tmux pane".to_string()
    })?;
    c.send("\x08");
    c.wait("C-h at nvim's left edge to reach the tmux pane", 10, |c| {
        (c.active().ok()?.id == left.id).then_some(())
    })?;
    // Leave nvim: back to its pane, quit.
    c.prefix("l");
    c.wait("the nvim pane", 5, |c| {
        (c.active().ok()?.id == right.id).then_some(())
    })?;
    nvim_cmd(c, "qa!");
    c.wait("nvim to quit", 15, |c| {
        (c.pane("#{pane_current_command}").ok()? != "nvim").then_some(())
    })?;
    Ok(
        "C-l, C-h moved between nvim splits; C-h at the left edge moved to the tmux pane"
            .to_string(),
    )
}

fn w12(c: &mut Ctx) -> Step {
    sleep(Duration::from_millis(500));
    let before = c.client("#{client_session} #{window_index} #{pane_id}")?;
    c.shell("clear")?;
    c.send("tmux detach\r");
    c.wait_screen("the detach message", 15, |s| s.contains("[detached"))?;
    c.wait("no client attached", 10, |c| {
        c.tmux(&["list-clients"]).ok().filter(|s| s.is_empty())
    })?;
    c.send("tmux attach\r");
    let after = c.wait("the client back", 20, |c| {
        c.client("#{client_session} #{window_index} #{pane_id}")
            .ok()
    })?;
    ensure(after == before, || {
        format!("attached to {after:?}, not {before:?}")
    })?;
    Ok(format!("detached and attached back to {after}"))
}

fn w15(c: &mut Ctx) -> Step {
    c.prefix("C");
    c.wait("a fresh window", 10, |c| {
        (c.geoms().ok()?.len() == 1).then_some(())
    })?;
    let pane = c.active()?.id;
    c.tmux(&["split-window", "-h", "-l", "80", "-t", &pane, WRAP_LINE])?;
    let wrap = c.wait("the 80-column pane", 10, |c| {
        c.active().ok().filter(|g| g.width == 80)
    })?;
    c.wait("the wrapped line", 10, |c| {
        c.tmux(&["capture-pane", "-p", "-t", &wrap.id])
            .ok()?
            .contains("w050")
            .then_some(())
    })?;
    let _ = c.tmux(&["delete-buffer"]);
    // From history-top, row 1 col 40, to row 2 col 20, across the soft wrap
    // at column 80: TM.2's wrap golden, second copy.
    c.prefix("[");
    c.wait("copy mode", 5, |c| {
        (c.pane("#{pane_in_mode}").ok()? == "1").then_some(())
    })?;
    for keys in ["g", "j", "4", "0", "l", " ", "j", "2", "0", "h"] {
        c.send(keys);
        sleep(Duration::from_millis(120));
    }
    let sel =
        c.pane("#{selection_start_x},#{selection_start_y} #{selection_end_x},#{selection_end_y}")?;
    c.send("\r");
    c.wait("copy mode to end", 5, |c| {
        (c.pane("#{pane_in_mode}").ok()? == "0").then_some(())
    })?;
    let got = c.tmux(&["show-buffer"])?;
    let size = c.tmux(&["list-buffers", "-F", "#{buffer_size}"])?;
    let golden =
        fs::read_to_string(golden_dir().join("tm_2/wrap.golden")).map_err(|e| e.to_string())?;
    let want = golden_copy(&golden, 1).ok_or("the golden has no second show-buffer")?;
    ensure(sel == "40,1 20,2", || {
        format!("selection {sel}, golden 40,1 20,2")
    })?;
    ensure(got == want.0, || {
        format!("show-buffer {got:?}, golden {:?}", want.0)
    })?;
    ensure(size.lines().next() == Some(want.1.as_str()), || {
        format!("buffer size {size}, golden {}", want.1)
    })?;
    Ok(format!(
        "selection {sel}; show-buffer matches tm_2/wrap.golden ({} bytes)",
        want.1
    ))
}

fn w16(c: &mut Ctx) -> Step {
    let clip = c.home.join(".clip/wl-copy.log");
    let _ = fs::remove_file(&clip);
    let from = c.h.output().len();
    c.prefix("[");
    c.wait("copy mode", 5, |c| {
        (c.pane("#{pane_in_mode}").ok()? == "1").then_some(())
    })?;
    for keys in ["g", "0", " ", "3", "l"] {
        c.send(keys);
        sleep(Duration::from_millis(120));
    }
    c.send("y");
    c.wait("copy mode to end", 5, |c| {
        (c.pane("#{pane_in_mode}").ok()? == "0").then_some(())
    })?;
    // Real tmux 3.7b sends an empty selection parameter (`52;;`), which
    // the outer terminal reads as its clipboard; grofe matches it.
    let osc = format!("\x1b]52;;{}", base64(b"w001"));
    let found = c.wait("OSC 52 with the selection on the outer pty", 10, |c| {
        let out = c.h.output();
        let seen = String::from_utf8_lossy(&out[from.min(out.len())..]).to_string();
        seen.contains(&osc).then_some(())
    });
    let stub = c.wait("the stub clipboard command's output", 10, |_| {
        fs::read_to_string(&clip).ok()
    });
    let buffer = c.tmux(&["show-buffer"])?;
    ensure(buffer == "w001", || {
        format!("the buffer is {buffer:?}, not \"w001\"")
    })?;
    found?;
    let stub = stub?;
    ensure(stub == "w001", || {
        format!("wl-copy got {stub:?}, not \"w001\"")
    })?;
    Ok(format!(
        "y: buffer w001, OSC 52 {:?} on the pty, wl-copy stub got w001",
        osc.replace('\x1b', "ESC")
    ))
}

fn w14(c: &mut Ctx) -> Step {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| c.home.join(".local/state"));
    let log = state.join("grofe/compat-miss.log");
    let text = fs::read_to_string(&log).unwrap_or_default();
    let _ = fs::write(c.out.join("compat-miss.log"), &text);
    ensure(text.is_empty(), || {
        format!(
            "{} has {} line(s):\n{text}",
            log.display(),
            text.lines().count()
        )
    })?;
    Ok(format!("{} is empty", log.display()))
}

fn golden_dir() -> PathBuf {
    std::env::var_os("GROFE_E2E_GOLDEN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compat/golden")
        })
}

/// The `n`th show-buffer's text in a golden, and the size list-buffers
/// gives right after it.
fn golden_copy(golden: &str, n: usize) -> Option<(String, String)> {
    let lines: Vec<&str> = golden.lines().collect();
    let shows: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| **l == "$ show-buffer")
        .map(|(i, _)| i)
        .collect();
    let at = *shows.get(n)?;
    let mut text = Vec::new();
    let mut i = at + 1;
    while i < lines.len() && !lines[i].starts_with("? ") {
        text.push(lines[i]);
        i += 1;
    }
    let size_at = lines[i..]
        .iter()
        .position(|l| l.starts_with("$ list-buffers"))?
        + i
        + 1;
    let size = lines.get(size_at)?.rsplit(' ').next()?.to_string();
    Some((text.join("\n"), size))
}

#[test]
#[ignore = "runs only in the alias-day container: scripts/e2e-alias-day.sh"]
fn alias_day_workflow() {
    if !enabled() {
        eprintln!("skipping: not in the alias-day container");
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let out = PathBuf::from(std::env::var_os("GROFE_E2E_OUT").expect("GROFE_E2E_OUT"));
    fs::create_dir_all(out.join("snapshots")).unwrap();
    let tmux = home.join(".local/lib/grofe/bin/tmux");

    let mut zsh = Command::new("/usr/bin/zsh");
    zsh.arg("-l")
        .current_dir(&home)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("LANG", "C.UTF-8")
        .env("SHELL", "/usr/bin/zsh")
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env_remove("TMUX")
        .env_remove("GROFE_E2E_ALIAS_DAY");
    let h = Harness::spawn(zsh, COLS, ROWS);
    let mut c = Ctx {
        h,
        tmux,
        home,
        out: out.clone(),
        marks: 0,
        rows: Vec::new(),
        failed: Vec::new(),
    };
    c.send("echo AD-$((40+2))-READY\r");
    c.h.wait_for_timeout("AD-42-READY", Duration::from_secs(60));

    c.step("W1", "tmux from zsh", w1);
    c.step("W2", "prefix, then back", w2);
    c.step("W3", "prefix |, prefix - in the pane's cwd", w3);
    c.step("W4", "prefix h/j/k/l, prefix H three times", w4);
    c.step("W5", "prefix c: the cockpit window", w5);
    c.step("W6", "prefix C, V, t, (, )", w6);
    c.step("W7", "prefix r reloads user.toml", w7);
    c.step("W8", "prefix C-s into a nested real tmux", w8);
    c.step("W9", "prefix K, ?, T", w9);
    c.step("W10", "prefix s, Space, u, o, a, A (the plugins)", w10);
    c.step("W11", "C-h through nvim (vim-tmux-navigator)", w11);
    c.step("W12", "tmux detach, tmux attach", w12);
    c.step("W15", "copy mode over a soft wrap, show-buffer", w15);
    c.step("W16", "tmux-yank's y: OSC 52 and the clipboard stub", w16);
    c.step("W14", "compat-miss.log is empty", w14);

    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join("workflow-failures.txt"))
        .unwrap();
    for f in &c.failed {
        let _ = writeln!(log, "{f}");
    }
    assert!(
        c.failed.is_empty(),
        "failed steps:\n{}",
        c.failed.join("\n")
    );
}

#[test]
fn golden_copy_reads_the_second_copy() {
    let golden = fs::read_to_string(golden_dir().join("tm_2/wrap.golden")).unwrap();
    let (text, size) = golden_copy(&golden, 1).unwrap();
    assert!(
        text.starts_with("w025 ") && text.ends_with(" w"),
        "{text:?}"
    );
    assert_eq!(size, "61");
}

#[test]
fn base64_matches_known_values() {
    assert_eq!(base64(b"w001"), "dzAwMQ==");
    assert_eq!(base64(b"abc"), "YWJj");
    assert_eq!(base64(b"ab"), "YWI=");
}
