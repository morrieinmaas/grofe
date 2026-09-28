//! Passthrough (T3.3, spec §4.9): OSC 52, title, bell, OSC 7/8/9/777, and
//! the byte scanner that frames APC `_G` for graphics. T3.0 lands the
//! seams, all inert until T3.3: the [`Scanner`] each pane's reader runs
//! before the emulator, the state on [`crate::server::State`], and
//! [`on_pane_event`], which the actor hands every title, bell, clipboard
//! and cwd event.

use std::borrow::Cow;

use crate::server::State;
use crate::server::pane::PaneEventKind;
use crate::types::PaneId;

/// What the server keeps for passthrough (T3.3).
#[derive(Debug, Default)]
pub struct Passthrough {}

/// Scans a pane's output ahead of the emulator for what `vte` does not
/// parse (OSC 7/9/777, APC `_G`). One per pane reader; its state carries
/// over between reads, so a sequence split across two reads is still found.
#[derive(Debug, Default)]
pub struct Scanner {}

impl Scanner {
    /// The bytes the emulator should parse, and the events found. The
    /// identity until T3.3.
    pub fn scan<'a>(&mut self, bytes: &'a [u8]) -> (Cow<'a, [u8]>, Vec<PaneEventKind>) {
        (Cow::Borrowed(bytes), Vec::new())
    }
}

/// A `Title`, `Bell`, `Clipboard` or `Cwd` event from `pane` (T3.3).
pub fn on_pane_event(_state: &mut State, _pane: PaneId, _kind: PaneEventKind) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// T3.0: the stub scanner hands every byte to the emulator unchanged
    /// and finds nothing, APC and OSC 7 included.
    #[test]
    fn the_stub_scanner_is_the_identity() {
        let mut s = Scanner::default();
        let input: &[u8] = b"plain \x1b]7;file:///tmp\x07 \x1b_Ga=q;AAAA\x1b\\ \x1b[1m";
        let (out, events) = s.scan(input);
        assert_eq!(&*out, input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert!(events.is_empty());
    }
}
