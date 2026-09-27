//! pane commands: split, kill, select, resize, respawn, send-keys, capture-pane.

use crate::cmd::Command;
use crate::protocol::ClientCtx;
use crate::server::State;

/// Runs a pane command routed here by `dispatch::route`.
// T1.5b replaces this body.
pub fn run(state: &mut State, ctx: &ClientCtx, cmd: Command) -> Result<String, String> {
    let _ = (state, ctx);
    Err(format!("{}: not implemented yet", verb(&cmd)))
}

fn verb(cmd: &Command) -> &'static str {
    match cmd {
        Command::SplitWindow(_) => "split-window",
        Command::KillPane(_) => "kill-pane",
        Command::SelectPane(_) => "select-pane",
        Command::ResizePane(_) => "resize-pane",
        Command::RespawnPane(_) => "respawn-pane",
        Command::SendKeys(_) => "send-keys",
        Command::CapturePane(_) => "capture-pane",
        _ => "command",
    }
}
