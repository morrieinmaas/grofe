//! The tmux front end (spec D1, §4.5; FE.1): the table that translates
//! tmux 3.7b's argv forms into native [`Command`]s, or into tmux's own
//! error text. FE.1 lands the row schema, the dispatcher and the area
//! registration here; FE.2, FE.3 and FE.4 each fill one area file
//! (`structure`, `formats`, `bindings`).
//!
//! TC.1 reads the CLI's global flags ([`cli`]); until FE.1 lands, the
//! server's [`dispatch`] still rejects every tmux form.
//!
//! The split with `main.rs`: [`cli`] only reads tmux's argv (its global
//! flags, `-V`, bare `tmux`) into a [`TmuxCli`]; `main.rs` keeps the
//! transport (socket, server start, hello, attach) and sends `argv` with
//! the dialect [`cli`] chose.

mod bindings;
mod formats;
mod structure;

use std::fmt;
use std::path::PathBuf;

use crate::cmd::Command;
use crate::protocol::{ClientCtx, Dialect};

/// A tmux-dialect request that fails, worded as tmux words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxError(pub String);

impl fmt::Display for TmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TmuxError {}

/// Translates one tmux-dialect request (`argv[0]` is the verb) into the
/// native commands it means. The stub rejects every form.
pub fn dispatch(argv: &[String], _ctx: &ClientCtx) -> Result<Vec<Command>, TmuxError> {
    Err(TmuxError(match argv.first() {
        Some(verb) => format!("tmux front end is not implemented: {verb}"),
        None => "tmux front end is not implemented".to_string(),
    }))
}

/// What the CLI run as `tmux` sends: the socket its global flags name, and
/// the request, in the dialect it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxCli {
    /// `-S <path>`.
    pub socket: Option<PathBuf>,
    /// `-L <name>`.
    pub name: Option<String>,
    /// The request, `argv[0]` its verb, handled by `main.rs` as a grofe
    /// argv is (`attach`, `new` without `-d`, a one-shot command).
    pub argv: Vec<String>,
    pub dialect: Dialect,
    /// `-V`: print `tmux <compat level>` and exit, before anything else.
    pub version: bool,
}

/// tmux 3.7b's usage text, printed after a bad global option.
const USAGE: &str = "usage: tmux [-2CDhlNuVv] [-c shell-command] [-f file] [-L socket-name]
            [-S socket-path] [-T features] [command [flags]]";

/// tmux's other global flags, those taking a value first. Valid for tmux,
/// not yet for grofe (FE.1 owns `-f`, `-2`, `-u`).
const OTHER_WITH_VALUE: &[char] = &['c', 'f', 'T'];
const OTHER_FLAGS: &[char] = &['2', 'C', 'D', 'h', 'l', 'N', 'q', 'u', 'U', 'v'];

/// Reads the argv of the CLI run as `tmux` (`args` follow `argv[0]`), or
/// the error to print and exit 1 with. Global flags are read as tmux's
/// `getopt` reads them (`-L x`, `-Lx`, `-V`, `--`); TC.1 knows `-L`,
/// `-S` and `-V`. Sockets stay in grofe's directory (spec D12), so
/// `-L name` never reaches tmux's own `/tmp/tmux-$UID`.
pub fn cli(args: &[String]) -> Result<TmuxCli, String> {
    let mut out = TmuxCli {
        socket: None,
        name: None,
        argv: Vec::new(),
        dialect: Dialect::Tmux,
        version: false,
    };
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        i += 1;
        if arg == "--" {
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) else {
            i -= 1;
            break;
        };
        // Every flag grofe knows either takes the rest of the word (`-L`,
        // `-S`) or ends parsing (`-V`, an error), so only the first letter
        // of a bundle is ever read.
        let mut letters = flags.chars();
        let flag = letters.next().unwrap_or('-');
        match flag {
            'L' | 'S' => {
                let attached = letters.as_str();
                let value = if attached.is_empty() {
                    i += 1;
                    args.get(i - 1).cloned().ok_or_else(|| {
                        format!("tmux: option requires an argument -- {flag}\n{USAGE}")
                    })?
                } else {
                    attached.to_string()
                };
                if flag == 'L' {
                    out.name = Some(value);
                } else {
                    out.socket = Some(PathBuf::from(value));
                }
            }
            // tmux prints its version as soon as getopt meets `-V`.
            'V' => {
                out.version = true;
                return Ok(out);
            }
            c if OTHER_WITH_VALUE.contains(&c) || OTHER_FLAGS.contains(&c) => {
                return Err(format!("tmux front end is not implemented: -{c}"));
            }
            c => return Err(format!("tmux: unknown option -- {c}\n{USAGE}")),
        }
    }
    out.argv = args[i..].to_vec();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_dispatcher_stub_refuses_every_form() {
        for argv in [&["ls"][..], &["new-session", "-d"]] {
            let err = dispatch(&args(argv), &crate::server::test_ctx()).unwrap_err();
            assert!(
                err.0.starts_with("tmux front end is not implemented"),
                "{err}"
            );
        }
    }

    #[test]
    fn cli_reads_sockets_and_leaves_the_request_for_the_transport() {
        let got = cli(&args(&["-L", "work", "new-session", "-d", "-L", "x"])).unwrap();
        assert_eq!(got.name.as_deref(), Some("work"));
        assert_eq!(got.socket, None);
        assert_eq!(got.argv, args(&["new-session", "-d", "-L", "x"]));
        assert_eq!(got.dialect, Dialect::Tmux);
        assert!(!got.version);

        let got = cli(&args(&["-S/tmp/s.sock", "-Lname", "ls"])).unwrap();
        assert_eq!(got.socket, Some(PathBuf::from("/tmp/s.sock")));
        assert_eq!(got.name.as_deref(), Some("name"));
        assert_eq!(got.argv, args(&["ls"]));

        let got = cli(&args(&["--", "-x"])).unwrap();
        assert_eq!(got.argv, args(&["-x"]));
        let got = cli(&args(&[])).unwrap();
        assert!(got.argv.is_empty());
    }

    #[test]
    fn cli_version_wins_as_soon_as_it_is_read() {
        for argv in [
            &["-V"][..],
            &["-V", "-Z"],
            &["-L", "x", "-V"],
            &["-SV", "-V"],
        ] {
            let got = cli(&args(argv)).unwrap();
            assert!(got.version, "{argv:?}");
        }
        // `-V` as `-L`'s value is a socket name.
        assert!(!cli(&args(&["-L", "-V"])).unwrap().version);
    }

    /// The texts are tmux 3.7b's, recorded on a private socket.
    #[test]
    fn cli_errors_are_tmux_s() {
        let err = cli(&args(&["-Z"])).unwrap_err();
        assert!(err.starts_with("tmux: unknown option -- Z\nusage: tmux [-2CDhlNuVv]"));
        let err = cli(&args(&["-L"])).unwrap_err();
        assert!(err.starts_with("tmux: option requires an argument -- L\nusage: tmux"));
        let err = cli(&args(&["-f", "/dev/null", "ls"])).unwrap_err();
        assert_eq!(err, "tmux front end is not implemented: -f");
    }
}
