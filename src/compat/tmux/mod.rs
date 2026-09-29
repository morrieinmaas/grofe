//! The tmux front end (spec D1, §4.5; FE.1): the table that translates
//! tmux 3.7b's argv forms into native [`Command`]s, or into tmux's own
//! error text. FE.1 lands the row schema, the dispatcher and the area
//! registration here; FE.2, FE.3 and FE.4 each fill one area file
//! (`structure`, `formats`, `bindings`).
//!
//! The stub edit leaves it rejecting every tmux form, on the CLI
//! ([`cli`]) and in the server ([`dispatch`]).
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
}

/// Reads the argv of the CLI run as `tmux` (`args` follow `argv[0]`), or
/// the error to print and exit 1 with. The stub refuses every form, so
/// nothing touches a socket, a directory or a server.
pub fn cli(_args: &[String]) -> Result<TmuxCli, String> {
    Err(
        "grofe: run as tmux, but the tmux front end is not implemented yet; run grofe instead"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_refuses_every_form() {
        for args in [&[][..], &["-V"], &["-L", "x", "new", "-d"], &["ls"]] {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let err = cli(&args).unwrap_err();
            assert!(err.contains("tmux front end is not implemented"), "{err}");
            let err = dispatch(&args, &crate::server::test_ctx()).unwrap_err();
            assert!(
                err.0.starts_with("tmux front end is not implemented"),
                "{err}"
            );
        }
    }
}
