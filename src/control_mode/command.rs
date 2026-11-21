//! Command lines: each runs on the server, except the client-bound commands
//! the bridge answers itself (`detach-client`, `switch-client`,
//! `refresh-client`).

use std::io::Write;

use crate::cmd::split_command_string;

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
/// bare.
fn parse_error_text(err: &str) -> String {
    let parse = ["unknown command: ", "ambiguous command: "];
    if parse.iter().any(|p| err.starts_with(p)) {
        format!("parse error: {err}")
    } else {
        err.to_string()
    }
}

/// What a command line did to the client.
pub(super) enum Outcome {
    Ok(String),
    Err(String),
    /// `detach-client`.
    Detach(String),
}

impl<W: Write> Bridge<'_, W> {
    /// One command line: its commands in order, stopping at the first
    /// error. The text printed, or the error.
    pub(super) fn command_line(&mut self, text: &str) -> Outcome {
        let cmds = match split_command_string(text) {
            Ok(c) => c,
            Err(e) => return Outcome::Err(format!("parse error: {e}")),
        };
        let mut printed = String::new();
        for argv in cmds {
            match self.command(&argv) {
                Outcome::Ok(out) => printed.push_str(&out),
                Outcome::Err(e) => return Outcome::Err(format!("{printed}{e}")),
                Outcome::Detach(out) => return Outcome::Detach(format!("{printed}{out}")),
            }
        }
        Outcome::Ok(printed)
    }

    fn command(&mut self, argv: &[String]) -> Outcome {
        let verb = crate::compat::tmux::resolve(&argv[0]).map_or(argv[0].as_str(), |s| s.name);
        match verb {
            "detach-client" => Outcome::Detach(String::new()),
            "switch-client" => self.switch_client(argv),
            "refresh-client" => self.refresh_client(argv),
            _ => match exchange(&self.opts.socket, &self.ctx(), argv) {
                Ok(out) => Outcome::Ok(out),
                Err(e) => Outcome::Err(parse_error_text(&e)),
            },
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
    /// (`-C`, `-f`, `-A`).
    fn refresh_client(&mut self, argv: &[String]) -> Outcome {
        let mut printed = String::new();
        let mut i = 1;
        while i < argv.len() {
            let flag = argv[i].as_str();
            let value = argv.get(i + 1);
            match (flag, value) {
                ("-C", Some(size)) => {
                    if let Err(e) = self.resize(size) {
                        return Outcome::Err(e);
                    }
                }
                ("-f", Some(flags)) => self.flow.set_flags(flags),
                ("-A", Some(arg)) => match self.flow.set_pane(arg) {
                    Ok(lines) => {
                        for l in lines {
                            printed.push_str(&l);
                            printed.push('\n');
                        }
                    }
                    Err(e) => return Outcome::Err(e),
                },
                // ponytail: `-B` (subscriptions), `-r`, `-l`, `-S` and the
                // scroll flags are not control-mode features of this slice.
                ("-B" | "-r", Some(_)) => {
                    return Outcome::Err(format!("refresh-client {flag}: not supported yet"));
                }
                _ => {}
            }
            i += 2;
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
