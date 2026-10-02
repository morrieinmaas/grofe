//! `copy-mode` (TM.1): enter copy mode in a pane (spec §4.11), with tmux
//! 3.7b's `-u` (a page up) and `-e` (leave at the bottom); in a pane
//! already in it, `-u` pages up again and the view is kept. TM.6 adds the
//! stock mouse rows' `-H` (hide the position) and `-M` (the pane under the
//! mouse key, starting a drag), which only a new mode takes, as in tmux.

use crate::cmd::{CopyMode, Resolve};
use crate::protocol::ClientCtx;
use crate::server::{State, copy, mouse};

pub fn copy_mode(state: &mut State, ctx: &ClientCtx, c: CopyMode) -> Result<String, String> {
    let pane = if c.mouse {
        // As tmux: no mouse key, or none over a pane, does nothing.
        let Some(pane) = mouse::event_pane(state) else {
            return Ok(String::new());
        };
        pane
    } else {
        state.model.resolve_pane(&c.target, ctx)?
    };
    if copy::enter(state, pane, c.scroll_exit) {
        if let Some(mode) = state.copy.get_mut(pane) {
            mode.hide_position = c.hide_position;
        }
        if c.mouse {
            mouse::start_drag(state);
        }
    }
    if c.page_up {
        copy::page_up(state, pane);
    }
    Ok(String::new())
}
