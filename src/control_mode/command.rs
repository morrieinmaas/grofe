//! Command lines: each runs on the server, except the client-bound commands
//! the bridge answers itself (`detach-client`, `switch-client`,
//! `refresh-client`) and the ones whose answer a control client gets in its
//! own words (`display-message`, `source-file`).

use std::io::Write;
use std::time::Instant;

use crate::cmd::split_command_string;
use crate::compat::tmux::{Getopt, resolve};

use super::Bridge;
use super::conn::exchange;
use super::scan::scan_uint;

/// The smallest and largest window size, tmux's WINDOW_MINIMUM and
/// WINDOW_MAXIMUM.
const SIZES: std::ops::RangeInclusive<u32> = 1..=10000;

/// `target` as a session target: `display-message -t` reads a bare name as a
/// pane, so a name with no `:` gets one; a `%`, `@` or `$` id stays as it is.
pub(super) fn session_target(target: &str) -> String {
    if target.contains(':') || target.starts_with(['%', '@', '$']) {
        target.to_string()
    } else {
        format!("{target}:")
    }
}

/// tmux words its parser's errors `parse error: ...`; the front end's are
/// bare. A command's argument errors (`command <verb>: too many
/// arguments`) are the parser's too.
fn parse_error_text(err: &str) -> String {
    let parse = ["unknown command: ", "ambiguous command: ", "command "];
    if parse.iter().any(|p| err.starts_with(p)) {
        format!("parse error: {err}")
    } else {
        err.to_string()
    }
}

/// `<w><sep><h>`, `sep` one of `seps`, as `sscanf("%u<sep>%u")` reads it.
fn size(text: &str, seps: &[char]) -> Option<(u32, u32)> {
    let (w, rest) = scan_uint(text)?;
    let rest = rest.strip_prefix(seps)?;
    Some((w, scan_uint(rest)?.0))
}

/// `argv` with its `-t` flag and value dropped (flags only, the command's
/// own getopt words before its positionals).
fn without_target(argv: &[String]) -> Vec<String> {
    let mut out = vec![argv[0].clone()];
    let mut words = argv[1..].iter();
    while let Some(w) = words.next() {
        if w == "--" || !w.starts_with('-') {
            out.push(w.clone());
            out.extend(words.cloned());
            break;
        }
        match w.strip_prefix("-t") {
            Some("") => {
                words.next();
            }
            Some(_) => {}
            None => out.push(w.clone()),
        }
    }
    out
}

/// A config file's error, `<path>:<line>: <message>`, which tmux gives a
/// control client as `%config-error`.
fn is_cause(line: &str) -> bool {
    line.split_once(": ")
        .and_then(|(head, _)| head.rsplit_once(':'))
        .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `argv`'s flags as tmux reads them for its verb.
fn flags(argv: &[String]) -> Getopt<'_> {
    resolve(&argv[0]).map_or(
        Getopt {
            flags: Vec::new(),
            rest: 0,
        },
        |spec| spec.getopt(&argv[1..]),
    )
}

/// What a command line did to the client.
enum Outcome {
    Ok(String),
    Err(String),
    /// `detach-client`.
    Detach,
}

impl<W: Write> Bridge<'_, W> {
    /// One command line: each command its own block, in order, stopping
    /// at the first error (tmux 3.7b). `Some(true)` when the client
    /// detached, `None` when the output is gone.
    pub(super) fn command_line(&mut self, text: &str) -> Option<bool> {
        let cmds = match split_command_string(text) {
            Ok(c) => c,
            Err(e) => {
                return self
                    .block(1, &Err(format!("parse error: {e}")))
                    .then_some(false);
            }
        };
        if cmds.is_empty() {
            return self.block(1, &Ok(String::new())).then_some(false);
        }
        // tmux parses the whole line, flags included, before running any of it.
        for argv in &cmds {
            if let Err(e) = resolve(&argv[0]).and_then(|s| s.check_flags(&argv[1..])) {
                return self.block(1, &Err(parse_error_text(&e.0))).then_some(false);
            }
        }
        for argv in cmds {
            let (result, detach) = match self.command(&argv) {
                Outcome::Ok(out) => (Ok(out), false),
                Outcome::Err(e) => (Err(e), false),
                Outcome::Detach => (Ok(String::new()), true),
            };
            // What `refresh-client -A %p:on` released follows its block, as
            // do `source-file`'s `%config-error` lines.
            if !self.block(1, &result) || (result.is_ok() && !self.release_all()) {
                return None;
            }
            for l in std::mem::take(&mut self.after_block) {
                if !self.line(&l) {
                    return None;
                }
            }
            if detach {
                return Some(true);
            }
            if result.is_err() {
                break;
            }
        }
        Some(false)
    }

    fn command(&mut self, argv: &[String]) -> Outcome {
        let verb = resolve(&argv[0]).map_or(argv[0].as_str(), |s| s.name);
        let getopt = flags(argv);
        match verb {
            // With a target it detaches other clients, on the server.
            "detach-client" if !"ast".chars().any(|f| getopt.has(f)) => Outcome::Detach,
            "detach-client" if getopt.has('a') && !getopt.has('s') && !getopt.has('t') => {
                self.detach_others()
            }
            // `-a -t <other>` and `-s` of the attached session take the
            // control client with them; `-s` wins over `-a`.
            "detach-client" if getopt.has('a') || getopt.has('s') => {
                let ours = if getopt.has('s') {
                    self.is_attached(getopt.value('s'))
                } else {
                    getopt.has('a')
                };
                match self.server(argv) {
                    Outcome::Ok(_) if ours => Outcome::Detach,
                    other => other,
                }
            }
            // `-c` switches another client, on the server.
            "switch-client" if !getopt.has('c') => self.switch_client(getopt.value('t')),
            "refresh-client" => self.refresh_client(&getopt),
            // tmux shows a control client its message as `%message`.
            "display-message" if !"pac".chars().any(|f| getopt.has(f)) => {
                self.message(argv, &getopt)
            }
            // A config file's errors are notices after the block, which ends
            // as a success.
            // The block keeps the other errors (a missing file of several).
            // With several files, a missing one does not fail the block.
            "source-file" => match self.server(argv) {
                Outcome::Err(e) if e.lines().any(is_cause) || argv.len() - getopt.rest > 2 => {
                    let (causes, rest): (Vec<&str>, Vec<&str>) =
                        e.lines().partition(|l| is_cause(l));
                    let causes = causes.iter().map(|l| format!("%config-error {l}"));
                    self.after_block.extend(causes);
                    Outcome::Ok(rest.join("\n"))
                }
                other => other,
            },
            _ => self.server(argv),
        }
    }

    /// Whether `target` names the attached session.
    fn is_attached(&self, target: Option<&str>) -> bool {
        let Some(target) = target else {
            return false;
        };
        let query = [
            "display-message",
            "-p",
            "-t",
            &session_target(target),
            "#{session_id}",
        ];
        self.ask(&query)
            .is_ok_and(|id| id.trim_end() == self.attached)
    }

    /// `display-message` without `-p`: its text as `%message`, newlines
    /// written `_` as tmux writes a control line. With `-v` the trace lines
    /// come first, each one line. A `-t` that names nothing runs without it.
    fn message(&self, argv: &[String], getopt: &Getopt<'_>) -> Outcome {
        let mut argv = argv.to_vec();
        argv.insert(1, "-p".to_string());
        let run = |argv: &[String]| exchange(&self.opts.socket, &self.ctx(), argv);
        let mut out = run(&argv);
        // ponytail: tmux expands the format with no target then (empty
        // session and pane values); grofe's server always has one, so the
        // fallback expands in the attached session.
        if matches!(&out, Err(e) if e.starts_with("can't find")) && getopt.has('t') {
            argv = without_target(&argv);
            out = run(&argv);
        }
        let text = match out {
            Ok(text) => text,
            Err(e) => return Outcome::Err(parse_error_text(&e)),
        };
        let mut printed = String::new();
        let mut msg = text.trim_end_matches('\n').to_string();
        if getopt.has('v') {
            // The message is what the same command prints without `-v`.
            let quiet: Vec<String> = argv.iter().filter(|a| *a != "-v").cloned().collect();
            let Ok(quiet) = run(&quiet) else {
                return Outcome::Ok(text);
            };
            let quiet = quiet.trim_end_matches('\n');
            let trace = msg
                .strip_suffix(quiet)
                .unwrap_or(&msg)
                .trim_end_matches('\n');
            for line in trace.split('\n').filter(|l| !l.is_empty()) {
                if line.starts_with("# ") || printed.is_empty() {
                    printed.push_str(line);
                } else {
                    // A trace line that went on past a newline.
                    printed.pop();
                    printed.push('_');
                    printed.push_str(line);
                }
                printed.push('\n');
            }
            msg = quiet.to_string();
        }
        printed.push_str(&format!("%message {}\n", msg.replace('\n', "_")));
        Outcome::Ok(printed)
    }

    fn server(&self, argv: &[String]) -> Outcome {
        match exchange(&self.opts.socket, &self.ctx(), argv) {
            Ok(out) => Outcome::Ok(out),
            Err(e) => Outcome::Err(parse_error_text(&e)),
        }
    }

    /// `switch-client -t <session>`: the client's session changes, and
    /// `%session-changed` says so.
    fn switch_client(&mut self, target: Option<&str>) -> Outcome {
        let Some(target) = target else {
            // ponytail: only an explicit `-t` (not `-n`, `-p`, `-l`).
            return Outcome::Err("switch-client: only -t is supported in control mode".into());
        };
        match self.ask(&[
            "display-message",
            "-p",
            "-t",
            &session_target(target),
            "#{session_id}\t#{session_name}",
        ]) {
            Ok(out) => {
                let Some((id, name)) = out.trim_end().split_once('\t') else {
                    return Outcome::Err(format!("can't find session: {target}"));
                };
                self.attached = id.to_string();
                self.after.push(format!("%session-changed {id} {name}"));
                Outcome::Ok(String::new())
            }
            Err(e) => Outcome::Err(e),
        }
    }

    /// `detach-client -a`: every other client detaches. The bridge is not
    /// a server client, so that is every client the server has.
    fn detach_others(&self) -> Outcome {
        let names = match self.ask(&["list-clients", "-F", "#{client_name}"]) {
            Ok(out) => out,
            Err(e) => return Outcome::Err(e),
        };
        for name in names.lines() {
            if let Err(e) = self.ask(&["detach-client", "-t", name]) {
                return Outcome::Err(e);
            }
        }
        Outcome::Ok(String::new())
    }

    /// `refresh-client`: the flags of control mode are the bridge's own,
    /// taken in tmux's order: `-F`/`-f`, then the first of `-A`, `-B`, `-C`.
    fn refresh_client(&mut self, getopt: &Getopt<'_>) -> Outcome {
        // ponytail: the control client is not a server client, so no `-t`
        // names it.
        if let Some(t) = getopt.value('t') {
            return Outcome::Err(format!("can't find client: {t}"));
        }
        let values = |f: char| {
            getopt
                .flags
                .iter()
                .filter(move |(g, _)| *g == f)
                .filter_map(|(_, v)| *v)
        };
        // `-F` is an alias of `-f`.
        for v in values('F').chain(values('f')) {
            self.flow.set_flags(v);
        }
        // ponytail: `-r` (colour reports), `-l` (clipboard), `-S` and the
        // scroll flags are not control-mode features of the bridge.
        if getopt.has('r') {
            return Outcome::Err("refresh-client -r: not supported yet".into());
        }
        if getopt.has('A') {
            let mut printed = String::new();
            for v in values('A') {
                for l in self.flow.set_pane(v, &self.snap.panes) {
                    printed.push_str(&l);
                    printed.push('\n');
                }
            }
            return Outcome::Ok(printed);
        }
        if getopt.has('B') {
            for v in values('B') {
                self.subs.update(v, Instant::now());
            }
            return Outcome::Ok(String::new());
        }
        match getopt.value('C').map(|size| self.resize(size)) {
            Some(Err(e)) => Outcome::Err(e),
            _ => Outcome::Ok(String::new()),
        }
    }

    /// `-C <w>x<h>` (or `<w>,<h>`) or `-C @<id>:<w>x<h>`: windows resize,
    /// with tmux's errors. `-C @<id>:` alone (no size) changes nothing here.
    fn resize(&self, arg: &str) -> Result<(), String> {
        let (window, (w, h)) = match arg.strip_prefix('@').and_then(scan_uint) {
            Some((id, rest)) if rest.starts_with(':') => match size(&rest[1..], &['x']) {
                Some(wh) => (Some(format!("@{id}")), wh),
                None => return Ok(()),
            },
            _ => (
                None,
                size(arg, &[',', 'x']).ok_or_else(|| "bad size argument".to_string())?,
            ),
        };
        if !SIZES.contains(&w) || !SIZES.contains(&h) {
            return Err("size too small or too big".into());
        }
        let (w, h) = (w.to_string(), h.to_string());
        // tmux keeps a size for a window it does not have yet; the bridge
        // has nothing to resize then.
        let targets: Vec<String> = match window {
            Some(w) if !self.snap.windows.contains_key(&w) => Vec::new(),
            Some(w) => vec![w],
            None => self
                .snap
                .windows_of(&self.attached)
                .iter()
                .map(|w| w.to_string())
                .collect(),
        };
        for t in targets {
            self.ask(&["resize-window", "-t", &t, "-x", &w, "-y", &h])?;
        }
        Ok(())
    }
}
