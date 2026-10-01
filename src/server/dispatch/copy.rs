//! `copy-mode` (TM.1): enter copy mode in a pane (spec §4.11), with tmux
//! 3.7b's `-u` (a page up) and `-e` (leave at the bottom); in a pane
//! already in it, `-u` pages up again and the view is kept.

use crate::cmd::{CopyMode, Resolve};
use crate::protocol::ClientCtx;
use crate::server::{State, copy};

pub fn copy_mode(state: &mut State, ctx: &ClientCtx, c: CopyMode) -> Result<String, String> {
    let pane = state.model.resolve_pane(&c.target, ctx)?;
    copy::enter(state, pane, c.scroll_exit);
    if c.page_up {
        copy::page_up(state, pane);
    }
    Ok(String::new())
}
