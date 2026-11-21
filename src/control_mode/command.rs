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

/// `target` as a session target: `display-message -t` reads a bare word as a
/// pane, so a word with no `:` gets one.
pub(super) fn session_target(target: &str) -> String {
    if target.contains(':') {
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
        for argv in cmds {
            let (result, detach) = match self.command(&argv) {
                Outcome::Ok(out) => (Ok(out), false),
                Outcome::Err(e) => (Err(e), false),
                Outcome::Detach => (Ok(String::new()), true),
            };
            // What `refresh-client -A %p:on` released follows its block.
            if !self.block(1, &result) || (result.is_ok() && !self.release_all()) {
                return None;
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
            // `-c` switches another client, on the server.
            "switch-client" if !getopt.has('c') => self.switch_client(argv),
            "refresh-client" => self.refresh_client(&getopt),
            // tmux shows a control client its message as `%message`.
            "display-message" if !getopt.has('p') && !getopt.has('a') => {
                let mut argv = argv.to_vec();
                argv.insert(1, "-p".to_string());
                match self.server(&argv) {
                    Outcome::Ok(out) => {
                        Outcome::Ok(format!("%message {}\n", out.trim_end_matches('\n')))
                    }
                    other => other,
                }
            }
            // A config file's errors are notices after the block, which ends
            // as a success.
            "source-file" => match self.server(argv) {
                Outcome::Err(e) if !e.is_empty() && e.lines().all(is_cause) => {
                    let causes = e.lines().map(|l| format!("%config-error {l}"));
                    self.after.extend(causes);
                    Outcome::Ok(String::new())
                }
                other => other,
            },
            _ => self.server(argv),
        }
    }

    fn server(&self, argv: &[String]) -> Outcome {
        match exchange(&self.opts.socket, &self.ctx(), argv) {
            Ok(out) => Outcome::Ok(out),
            Err(e) => Outcome::Err(parse_error_text(&e)),
        }
    }

    /// `switch-client -t <session>`: the client's session changes, and
    /// `%session-changed` says so.
    fn switch_client(&mut self, argv: &[String]) -> Outcome {
        let Some(at) = argv.iter().position(|a| a == "-t") else {
            // ponytail: only an explicit `-t` (not `-n`, `-p`, `-l`).
            return Outcome::Err("switch-client: only -t is supported in control mode".into());
        };
        let Some(target) = argv.get(at + 1) else {
            return Outcome::Err("switch-client: -t needs a target".into());
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

    /// `refresh-client`: the flags of control mode are the bridge's own
    /// (`-C`, `-f`, `-A`, `-B`).
    fn refresh_client(&mut self, getopt: &Getopt<'_>) -> Outcome {
        let mut printed = String::new();
        for &(flag, value) in &getopt.flags {
            let Some(value) = value else {
                continue;
            };
            match flag {
                'C' => {
                    if let Err(e) = self.resize(value) {
                        return Outcome::Err(e);
                    }
                }
                'f' => self.flow.set_flags(value),
                'A' => match self.flow.set_pane(value) {
                    Ok(lines) => {
                        for l in lines {
                            printed.push_str(&l);
                            printed.push('\n');
                        }
                    }
                    Err(e) => return Outcome::Err(e),
                },
                'B' => self.subs.update(value, Instant::now()),
                // ponytail: `-r` (colour reports), `-l` (clipboard), `-S`
                // and the scroll flags are not control-mode features of the
                // bridge.
                'r' => return Outcome::Err("refresh-client -r: not supported yet".into()),
                _ => {}
            }
        }
        Outcome::Ok(printed)
    }

    /// `-C <w>x<h>` or `-C @<id>:<w>x<h>`: windows resize.
    fn resize(&self, arg: &str) -> Result<(), String> {
        let (window, size) = match arg.split_once(':') {
            Some((w, s)) if w.starts_with('@') => (Some(w), s),
            _ => (None, arg),
        };
        let (w, h) = size
            .split_once('x')
            .or_else(|| size.split_once(','))
            .filter(|(w, h)| w.parse::<u16>().is_ok() && h.parse::<u16>().is_ok())
            .ok_or_else(|| format!("bad size argument: {arg}"))?;
        let targets: Vec<String> = match window {
            Some(w) => vec![w.to_string()],
            None => self
                .snap
                .windows_of(&self.attached)
                .iter()
                .map(|w| w.to_string())
                .collect(),
        };
        for t in targets {
            self.ask(&["resize-window", "-t", &t, "-x", w, "-y", h])?;
        }
        Ok(())
    }
}
