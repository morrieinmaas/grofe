//! set-option/show-options scopes, persisted and user options.

use crate::cmd::Command;
use crate::protocol::ClientCtx;
use crate::server::State;

/// Runs an option or format command routed here by `dispatch::route`:
/// `set-option`, `show-options`, `display-message`, and `list-sessions`,
/// `list-windows` or `list-panes` given `-F`.
// T1.5c replaces this body.
pub fn run(state: &mut State, ctx: &ClientCtx, cmd: Command) -> Result<String, String> {
    let _ = (state, ctx);
    Err(format!("{}: not implemented yet", verb(&cmd)))
}

fn verb(cmd: &Command) -> &'static str {
    match cmd {
        Command::SetOption(_) => "set-option",
        Command::ShowOptions(_) => "show-options",
        Command::DisplayMessage(_) => "display-message",
        Command::ListSessions(_) => "list-sessions -F",
        Command::ListWindows(_) => "list-windows -F",
        Command::ListPanes(_) => "list-panes -F",
        _ => "command",
    }
}
