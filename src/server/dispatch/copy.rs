//! `copy-mode` (TM.1): enter copy mode in a pane (spec §4.11), with tmux
//! 3.7b's `-u` (a page up) and `-e` (leave at the bottom); in a pane
//! already in it, `-u` pages up again and the view is kept. TM.6 adds the
//! stock mouse rows' `-H` (hide the position) and `-M` (the pane under the
//! mouse key, starting a drag), which only a new mode takes, as in tmux.

use crate::cmd::{CopyMode, Resolve};
use crate::protocol::ClientCtx;
use crate::server::{State, copy, mouse};

pub fn copy_mode(state: &mut State, ctx: &ClientCtx, c: CopyMode) -> Result<String, String> {
    let pane = if c.mouse && !c.cancel {
        // As tmux: no mouse key, or none over a pane, does nothing.
        let Some(pane) = mouse::event_pane(state) else {
            return Ok(String::new());
        };
        pane
    } else {
        state.model.resolve_pane(&c.target, ctx)?
    };
    // `-q` leaves every mode of the pane (`window_pane_reset_mode_all`).
    if c.cancel {
        state.copy.remove(pane);
        crate::server::tree::close_over(state, pane);
        crate::server::clients::mark_all_dirty(state);
        return Ok(String::new());
    }
    let source = match &c.source {
        Some(t) => state.model.resolve_pane(t, ctx)?,
        None => pane,
    };
    if copy::enter(state, pane, source, c.scroll_exit) {
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
    if c.page_down {
        copy::page_down(state, pane, c.scroll_exit);
    }
    // `-S` drags a pane scrollbar's slider to the pointer's row. grofe
    // draws no scrollbar, so no mouse key sends it; run from the CLI, tmux
    // 3.7b has the slider at the top (recorded): the view goes to the top
    // of the history and the pane becomes the window's active one.
    // ponytail: no pointer position, so no other place for the slider.
    if c.slider && state.copy.is_active(pane) {
        if let Some(mode) = state.copy.get_mut(pane) {
            mode.oy = mode.grid.hsize;
        }
        let window = state.model.panes.get(&pane).map(|p| p.window);
        if let Some(w) = window.and_then(|w| state.model.windows.get_mut(&w)) {
            w.set_active(pane);
        }
    }
    Ok(String::new())
}
