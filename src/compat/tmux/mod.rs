//! The tmux front end (spec D1, §4.5; FE.1): the table that translates
//! tmux 3.7b's argv forms into native [`Command`]s, or into tmux's own
//! error text. FE.1 lands the row schema, the dispatcher and the area
//! registration here; FE.2, FE.3 and FE.4 each fill one area file
//! (`structure`, `formats`, `bindings`).
//!
//! The stub edit leaves it rejecting every tmux form, on the CLI
//! ([`cli`]) and in the server ([`dispatch`]).

mod bindings;
mod formats;
mod structure;

use std::fmt;
use std::process::ExitCode;

use crate::cmd::Command;
use crate::protocol::ClientCtx;

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

/// The CLI run as `tmux` (`args` follow `argv[0]`). The stub refuses every
/// form before it looks at a socket, a directory or the environment.
pub fn cli(_args: &[String]) -> ExitCode {
    eprintln!(
        "grofe: run as tmux, but the tmux front end is not implemented yet; run grofe instead"
    );
    ExitCode::FAILURE
}
