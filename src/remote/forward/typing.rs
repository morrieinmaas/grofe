//! Typing held back while a file paste is on its way: what of it still goes
//! through (the prefix, so prefix+d detaches; Esc or C-c, which cancels the
//! upload or, before it, gives the paste up as text) and how much is kept
//! for after the pasted text. A read is split at each key, so `ls` then C-c
//! in one read keeps `ls`.

use std::ops::Range;
use std::time::{Duration, Instant};

use crate::server::input::{Decoder, Input as Decoded, Key, KeyCode, parse_prefix};

/// The most typing held for after a paste; more is dropped (as the remote
/// client's own offline hold does).
pub const MAX_QUEUED: usize = 64 * 1024;

/// What becomes of part of a read typed while a paste is held; what is
/// kept for after the pasted text is not listed.
#[derive(Debug, PartialEq, Eq)]
pub enum Typed {
    /// On to the server now: the prefix, or the key after it.
    Through(Vec<u8>),
    /// Esc or C-c: stop the upload, or before it give the paste up. `rest`
    /// is what the read held after it, to be typed as usual.
    Cancel { rest: Vec<u8> },
    /// Over [`MAX_QUEUED`]: dropped. `first` is true the first time, for one
    /// message.
    Dropped { first: bool },
}

/// The keys typed while a paste is held.
#[derive(Debug)]
pub struct Held {
    pub bytes: Vec<u8>,
    /// The paste's id ([`super::FilePaste::id`]).
    pub id: u64,
    /// The paste's own text, for when it is given up.
    pub original: Vec<u8>,
    prefix: Option<Key>,
    /// The key before was the prefix: this one is its command.
    armed: bool,
    overflowed: bool,
    decoder: Decoder,
}

impl Held {
    pub fn new(id: u64, original: Vec<u8>, prefix: Option<Key>) -> Held {
        Held {
            bytes: Vec::new(),
            id,
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

    /// Each key of `bytes` and the bytes it came from. Fed a byte at a time
    /// so the spans are known; an escape still waiting at the end is taken
    /// as the Escape key, as the read is all there is.
    fn keys(&mut self, bytes: &[u8]) -> Vec<(Option<Key>, Range<usize>)> {
        let (t, wait) = (Instant::now(), Duration::from_secs(1));
        let mut spans = Vec::new();
        let mut start = 0;
        for i in 0..bytes.len() {
            for input in self.decoder.feed(&bytes[i..=i], t, wait) {
                spans.push((key_of(input), start..i + 1));
                start = i + 1;
            }
        }
        for input in self.decoder.flush(t + 2 * wait, wait) {
            spans.push((key_of(input), start..bytes.len()));
            start = bytes.len();
        }
        if start < bytes.len() {
            spans.push((None, start..bytes.len()));
        }
        spans
    }

    /// One read typed while the paste is held: what goes through or cancels,
    /// in order; the rest is kept.
    pub fn take(&mut self, bytes: &[u8]) -> Vec<Typed> {
        let ctrl_c = parse_prefix("C-c").ok().flatten();
        let mut out = Vec::new();
        for (key, span) in self.keys(bytes) {
            let part = &bytes[span.clone()];
            let prefix = key.zip(self.prefix).is_some_and(|(k, p)| k.matches(&p));
            let cancel = key.is_some_and(|k| {
                k.code == KeyCode::Escape || ctrl_c.is_some_and(|c| k.matches(&c))
            });
            if self.armed || prefix {
                self.armed = !self.armed && prefix;
                match out.last_mut() {
                    Some(Typed::Through(through)) => through.extend_from_slice(part),
                    _ => out.push(Typed::Through(part.to_vec())),
                }
            } else if cancel {
                out.push(Typed::Cancel {
                    rest: bytes[span.end..].to_vec(),
                });
                return out;
            } else if self.bytes.len() + part.len() > MAX_QUEUED {
                let first = !self.overflowed;
                self.overflowed = true;
                if first {
                    out.push(Typed::Dropped { first });
                }
            } else {
                self.bytes.extend_from_slice(part);
            }
        }
        out
    }
}

fn key_of(input: Decoded) -> Option<Key> {
    match input {
        Decoded::Key(k) => Some(k),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held() -> Held {
        Held::new(0, Vec::new(), Held::default_prefix())
    }

    fn through(b: &[u8]) -> Typed {
        Typed::Through(b.to_vec())
    }

    #[test]
    fn the_prefix_and_its_key_go_through_and_the_rest_waits() {
        let mut h = held();
        assert_eq!(h.take(b"ls"), []);
        assert_eq!(h.take(b"\x02"), [through(b"\x02")]);
        assert_eq!(h.take(b"d"), [through(b"d")]);
        assert_eq!(h.take(b"x"), []);
        // Both in one read, and in the kitty keyboard protocol.
        assert_eq!(h.take(b"\x02d"), [through(b"\x02d")]);
        assert_eq!(h.take(b"\x1b[98;5u"), [through(b"\x1b[98;5u")]);
        assert_eq!(h.take(b"d"), [through(b"d")]);
        assert_eq!(h.bytes, b"lsx");
    }

    #[test]
    fn a_read_is_split_at_the_prefix() {
        let mut h = held();
        // Typing, then prefix+d, then more typing: all in one read.
        assert_eq!(h.take(b"ab\x02dc"), [through(b"\x02d")]);
        assert_eq!(h.bytes, b"abc");
        // An arrow before the prefix stays whole and held.
        assert_eq!(h.take(b"\x1b[A\x02"), [through(b"\x02")]);
        assert_eq!(h.bytes, b"abc\x1b[A");
    }

    #[test]
    fn esc_or_c_c_cancels_and_keeps_what_came_before() {
        let mut h = held();
        assert_eq!(h.take(b"\x03"), [Typed::Cancel { rest: Vec::new() }]);
        assert_eq!(h.take(b"\x1b"), [Typed::Cancel { rest: Vec::new() }]);
        assert_eq!(h.take(b"\x1b[99;5u"), [Typed::Cancel { rest: Vec::new() }]);
        // `ls` then C-c in one read: `ls` is kept, what follows is the rest.
        assert_eq!(
            h.take(b"ls\x03pwd"),
            [Typed::Cancel {
                rest: b"pwd".to_vec()
            }]
        );
        assert_eq!(h.bytes, b"ls");
    }

    #[test]
    fn typing_past_the_cap_is_dropped_with_one_message() {
        let mut h = Held::new(0, Vec::new(), None);
        let big = vec![b'x'; MAX_QUEUED];
        assert_eq!(h.take(&big), []);
        assert_eq!(h.take(b"y"), [Typed::Dropped { first: true }]);
        assert_eq!(h.take(b"z"), []);
        assert_eq!(h.bytes.len(), MAX_QUEUED);
    }
}
