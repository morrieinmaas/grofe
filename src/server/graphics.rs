//! Kitty graphics in Unicode-placeholder mode (T3.8, spec §4.9). T3.0
//! lands the seams, all inert until T3.8: the state on
//! [`crate::server::State`], [`on_graphics`] (the actor hands it every
//! framed APC `_G` a pane's scanner produced), [`on_full_redraw`] (called
//! before a client gets a full redraw), and the [`Remap`] the renderer reads.

use std::sync::atomic::AtomicBool;

use crate::server::State;
use crate::types::{ClientId, PaneId};

/// Per-pane image id remap and the image data cache (T3.8).
#[derive(Debug, Default)]
pub struct Graphics {}

impl Graphics {
    /// What the renderer needs to rewrite placeholder cells; `None` while
    /// no pane has an image.
    pub fn remap(&self) -> Option<&Remap> {
        None
    }
}

/// Pane-local image ids to the ids clients see (T3.8).
#[derive(Debug, Default)]
pub struct Remap {}

/// One framed APC `_G ... ESC \` from `pane` (T3.3's scanner).
pub fn on_graphics(_state: &mut State, _pane: PaneId, _bytes: Vec<u8>) {}

/// The answer to one framed APC `_G` from a pane, if it is a query (`a=q`)
/// that wants one, written straight back to the pane by its reader;
/// `graphics_ok` says whether an attached client shows kitty graphics.
/// Runs on the pane's reader task, never the actor (T3.8).
pub fn answer_query(_apc: &[u8], _graphics_ok: &AtomicBool) -> Option<Vec<u8>> {
    None
}

/// Client `id` is about to get a full redraw (attach, or after a dropped
/// frame): what it needs re-sent goes through `clients::queue_raw` now.
pub fn on_full_redraw(_state: &mut State, _id: ClientId) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// T3.0: the stub answers no query, whether or not a client could show
    /// the image.
    #[test]
    fn the_stub_answers_no_query() {
        let query = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
        for ok in [false, true] {
            assert_eq!(answer_query(query, &AtomicBool::new(ok)), None);
        }
    }
}
