//! Typing held back while a file paste is on its way: what of it still goes
//! through (the prefix, so prefix+d detaches; Esc or C-c, which cancels the
//! upload or, before it, gives the paste up as text) and how much is kept
//! for after the pasted text.

use std::time::{Duration, Instant};

use crate::server::input::{Decoder, Input as Decoded, Key, KeyCode, parse_prefix};

/// The most typing held for after a paste; more is dropped (as the remote
/// client's own offline hold does).
pub const MAX_QUEUED: usize = 64 * 1024;

/// What becomes of one read typed while a paste is held.
#[derive(Debug, PartialEq, Eq)]
pub enum Typed {
    /// Kept, for after the pasted text.
    Queued,
    /// On to the server now: the prefix, or the key after it.
    Through,
    /// Esc or C-c: stop the upload, or before it give the paste up.
    Cancel,
    /// Over [`MAX_QUEUED`]: dropped. `first` is true the first time, for one
    /// message.
    Dropped { first: bool },
}

/// The keys typed while a paste is held.
#[derive(Debug)]
pub struct Held {
    pub bytes: Vec<u8>,
    /// The paste's own text, for when it is given up.
    pub original: Vec<u8>,
    prefix: Option<Key>,
    /// The read before was the prefix: this one is its command.
    armed: bool,
    overflowed: bool,
    decoder: Decoder,
}

impl Held {
    pub fn new(original: Vec<u8>, prefix: Option<Key>) -> Held {
        Held {
            bytes: Vec::new(),
            original,
            prefix,
            armed: false,
            overflowed: false,
            decoder: Decoder::new(),
        }
    }

    /// The default prefix, before the server says what the session's is.
    pub fn default_prefix() -> Option<Key> {
        parse_prefix("C-b").ok().flatten()
    }

    /// One read typed while the paste is held.
    // ponytail: a read is taken whole: the prefix and the next key arrive
    // as reads of their own when typed, and a read holding more than that
    // goes through or waits as one.
    pub fn take(&mut self, bytes: &[u8]) -> Typed {
        let keys: Vec<Key> = self
            .decoder
            .feed(bytes, Instant::now(), Duration::ZERO)
            .into_iter()
            .filter_map(|i| match i {
                Decoded::Key(k) => Some(k),
                _ => None,
            })
            .collect();
        if self.armed {
            self.armed = false;
            return Typed::Through;
        }
        if let (Some(first), Some(prefix)) = (keys.first(), self.prefix)
            && first.matches(&prefix)
        {
            self.armed = keys.len() == 1;
            return Typed::Through;
        }
        let ctrl_c = parse_prefix("C-c").ok().flatten();
        if keys
            .iter()
            .any(|k| k.code == KeyCode::Escape || ctrl_c.is_some_and(|c| k.matches(&c)))
        {
            return Typed::Cancel;
        }
        if self.bytes.len() + bytes.len() > MAX_QUEUED {
            let first = !self.overflowed;
            self.overflowed = true;
            return Typed::Dropped { first };
        }
        self.bytes.extend_from_slice(bytes);
        Typed::Queued
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefix_and_its_key_go_through_and_the_rest_waits() {
        let mut h = Held::new(Vec::new(), Held::default_prefix());
        assert_eq!(h.take(b"ls"), Typed::Queued);
        assert_eq!(h.take(b"\x02"), Typed::Through);
        assert_eq!(h.take(b"d"), Typed::Through);
        assert_eq!(h.take(b"x"), Typed::Queued);
        // Both in one read, and in the kitty keyboard protocol.
        assert_eq!(h.take(b"\x02d"), Typed::Through);
        assert_eq!(h.take(b"\x1b[98;5u"), Typed::Through);
        assert_eq!(h.take(b"d"), Typed::Through);
        assert_eq!(h.bytes, b"lsx");
    }

    #[test]
    fn esc_or_c_c_cancels() {
        let mut h = Held::new(Vec::new(), Held::default_prefix());
        assert_eq!(h.take(b"\x03"), Typed::Cancel);
        assert_eq!(h.take(b"\x1b"), Typed::Cancel);
        assert_eq!(h.take(b"\x1b[99;5u"), Typed::Cancel);
    }

    #[test]
    fn typing_past_the_cap_is_dropped_with_one_message() {
        let mut h = Held::new(Vec::new(), None);
        let big = vec![b'x'; MAX_QUEUED];
        assert_eq!(h.take(&big), Typed::Queued);
        assert_eq!(h.take(b"y"), Typed::Dropped { first: true });
        assert_eq!(h.take(b"z"), Typed::Dropped { first: false });
        assert_eq!(h.bytes.len(), MAX_QUEUED);
    }
}
