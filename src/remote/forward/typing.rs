//! Typing held back while a file paste is on its way: what of it still goes
//! through (the prefix, so prefix+d detaches; Esc or C-c, which cancels the
//! upload or, before it, gives the paste up as text) and how much is kept
//! for after the pasted text.
//!
//! The keys are read by the server's own [`Decoder`], with the bytes each
//! one came from ([`Decoder::with_spans`]), so its rules hold here too: an
//! incomplete sequence waits for more bytes or the escape time, a lone Esc
//! is Escape only then, Esc before a key is Meta, and a bracketed paste is
//! text however many reads it takes (or until the decoder's paste timeout).

use std::time::{Duration, Instant};

use crate::server::input::{Decoder, Input as Decoded, Key, KeyCode, parse_prefix};

/// The most typing held for after a paste; more is dropped (as the remote
/// client's own offline hold does). A bracketed paste is kept or dropped
/// whole.
pub const MAX_QUEUED: usize = 64 * 1024;

/// What becomes of part of the typing held; what is kept for after the
/// pasted text is not listed.
#[derive(Debug, PartialEq, Eq)]
pub enum Typed {
    /// On to the server now: the prefix, or the key after it.
    Through(Vec<u8>),
    /// Esc or C-c: stop the upload, or before it give the paste up. `rest`
    /// is what was typed after it, to be typed as usual.
    Cancel { rest: Vec<u8> },
    /// Over [`MAX_QUEUED`]: dropped. `first` is true the first time, for one
    /// message.
    Dropped { first: bool },
}

/// Where a paste the decoder passes on in pieces stands.
#[derive(Debug, Default)]
enum Paste {
    #[default]
    None,
    /// Open: its pieces so far.
    Open(Vec<u8>),
    /// Dropped for the cap while still open: its later pieces, and what the
    /// decoder still holds of it, go too, through its end.
    Dropping,
}

/// The keys typed while a paste is held.
#[derive(Debug)]
pub struct Held {
    bytes: Vec<u8>,
    /// The paste's id ([`super::FilePaste::id`]).
    pub id: u64,
    /// The paste's own text, for when it is given up.
    pub original: Vec<u8>,
    prefix: Option<Key>,
    /// How long an incomplete sequence waits ([`super::HOLD_TIMEOUT`]).
    escape_time: Duration,
    /// The key before was the prefix: this one is its command.
    armed: bool,
    overflowed: bool,
    decoder: Decoder,
    /// A bracketed paste the decoder passes on in pieces.
    paste: Paste,
}

impl Held {
    pub fn new(id: u64, original: Vec<u8>, prefix: Option<Key>, escape_time: Duration) -> Held {
        Held {
            bytes: Vec::new(),
            id,
            original,
            prefix,
            escape_time,
            armed: false,
            overflowed: false,
            decoder: Decoder::with_spans(),
            paste: Paste::None,
        }
    }

    /// The default prefix, before the server says what the session's is.
    pub fn default_prefix() -> Option<Key> {
        parse_prefix("C-b").ok().flatten()
    }

    /// One read typed at `now` while the paste is held: what goes through
    /// or cancels, in order; the rest is kept.
    pub fn take(&mut self, bytes: &[u8], now: Instant) -> Vec<Typed> {
        let inputs = self.decoder.feed_spans(bytes, now, self.escape_time);
        self.handle(inputs)
    }

    /// Time passing: a sequence or a paste that waited long enough is
    /// decided.
    pub fn tick(&mut self, now: Instant) -> Vec<Typed> {
        let inputs = self.decoder.flush_spans(now, self.escape_time);
        self.handle(inputs)
    }

    /// Everything held, as typed, for after the paste: kept keys, then what
    /// still waits (a sequence, an open paste: kept whole, or dropped whole
    /// past [`MAX_QUEUED`]); and whether something was dropped there.
    pub fn into_bytes(mut self) -> (Vec<u8>, bool) {
        let mut out = std::mem::take(&mut self.bytes);
        let waiting = self.decoder.take_waiting();
        if self.overflowed {
            // Everything after the first drop goes too.
            return (out, false);
        }
        let unit = match std::mem::take(&mut self.paste) {
            // The dropped paste's rest goes with it.
            Paste::Dropping => return (out, false),
            Paste::Open(mut pieces) => {
                pieces.extend(waiting);
                pieces
            }
            Paste::None => waiting,
        };
        if out.len() + unit.len() > MAX_QUEUED {
            return (out, true);
        }
        out.extend(unit);
        (out, false)
    }

    #[cfg(test)]
    pub fn kept(&self) -> &[u8] {
        &self.bytes
    }

    fn handle(&mut self, inputs: Vec<(Decoded, Vec<u8>)>) -> Vec<Typed> {
        let ctrl_c = parse_prefix("C-c").ok().flatten();
        let mut out = Vec::new();
        let mut inputs = inputs.into_iter();
        while let Some((input, raw)) = inputs.next() {
            let key = match input {
                Decoded::Paste { start, end, .. } => {
                    self.paste_piece(start, end, raw, &mut out);
                    continue;
                }
                Decoded::Key(k) => k,
                _ => {
                    self.keep(&raw, &mut out);
                    continue;
                }
            };
            let prefix = self.prefix.is_some_and(|p| key.matches(&p));
            let cancel = key.code == KeyCode::Escape || ctrl_c.is_some_and(|c| key.matches(&c));
            if self.armed || prefix {
                self.armed = !self.armed && prefix;
                match out.last_mut() {
                    Some(Typed::Through(through)) => through.extend_from_slice(&raw),
                    _ => out.push(Typed::Through(raw)),
                }
            } else if cancel {
                // What came after it, as typed: decoded keys, then what the
                // decoder still waits on (it starts afresh).
                let mut rest: Vec<u8> = inputs.flat_map(|(_, raw)| raw).collect();
                rest.extend(self.decoder.take_waiting());
                out.push(Typed::Cancel { rest });
                return out;
            } else {
                self.keep(&raw, &mut out);
            }
        }
        out
    }

    /// A piece of a bracketed paste: the whole paste is kept once it ends,
    /// or dropped whole if it would pass the cap.
    fn paste_piece(&mut self, start: bool, end: bool, raw: Vec<u8>, out: &mut Vec<Typed>) {
        if start {
            self.paste = if self.overflowed {
                Paste::Dropping
            } else {
                Paste::Open(Vec::new())
            };
        }
        if let Paste::Open(pieces) = &mut self.paste {
            pieces.extend(raw);
            if self.bytes.len() + pieces.len() > MAX_QUEUED {
                self.paste = Paste::Dropping;
                self.drop_typing(out);
            }
        }
        // The end: an open paste is kept, a dropping one is done.
        if end && let Paste::Open(pieces) = std::mem::take(&mut self.paste) {
            self.bytes.extend(pieces);
        }
    }

    /// Holds `part` for after the paste, unless that passes [`MAX_QUEUED`].
    /// Once anything was dropped, everything after it goes too, so what is
    /// replayed never has a hole in the middle of a command.
    fn keep(&mut self, part: &[u8], out: &mut Vec<Typed>) {
        if self.overflowed || self.bytes.len() + part.len() > MAX_QUEUED {
            self.drop_typing(out);
        } else {
            self.bytes.extend_from_slice(part);
        }
    }

    fn drop_typing(&mut self, out: &mut Vec<Typed>) {
        let first = !self.overflowed;
        self.overflowed = true;
        if first {
            out.push(Typed::Dropped { first });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_millis(30);

    fn held() -> Held {
        Held::new(0, Vec::new(), Held::default_prefix(), WAIT)
    }

    fn through(b: &[u8]) -> Typed {
        Typed::Through(b.to_vec())
    }

    #[test]
    fn the_prefix_and_its_key_go_through_and_the_rest_waits() {
        let mut h = held();
        let now = Instant::now();
        assert_eq!(h.take(b"ls", now), []);
        assert_eq!(h.take(b"\x02", now), [through(b"\x02")]);
        assert_eq!(h.take(b"d", now), [through(b"d")]);
        assert_eq!(h.take(b"x", now), []);
        // Both in one read, and in the kitty keyboard protocol.
        assert_eq!(h.take(b"\x02d", now), [through(b"\x02d")]);
        assert_eq!(h.take(b"\x1b[98;5u", now), [through(b"\x1b[98;5u")]);
        assert_eq!(h.take(b"d", now), [through(b"d")]);
        assert_eq!(h.kept(), b"lsx");
    }

    #[test]
    fn a_read_is_split_at_the_prefix() {
        let mut h = held();
        let now = Instant::now();
        // Typing, then prefix+d, then more typing: all in one read.
        assert_eq!(h.take(b"ab\x02dc", now), [through(b"\x02d")]);
        assert_eq!(h.kept(), b"abc");
        // An arrow before the prefix stays whole and held.
        assert_eq!(h.take(b"\x1b[A\x02", now), [through(b"\x02")]);
        assert_eq!(h.kept(), b"abc\x1b[A");
    }

    #[test]
    fn esc_or_c_c_cancels_and_keeps_what_came_before() {
        let mut h = held();
        let now = Instant::now();
        assert_eq!(h.take(b"\x03", now), [Typed::Cancel { rest: Vec::new() }]);
        assert_eq!(
            h.take(b"\x1b[99;5u", now),
            [Typed::Cancel { rest: Vec::new() }]
        );
        // A lone Esc is Escape once the escape time passes.
        assert_eq!(h.take(b"\x1b", now), []);
        assert_eq!(h.tick(now + WAIT), [Typed::Cancel { rest: Vec::new() }]);
        // `ls` then C-c in one read: `ls` is kept, what follows is the rest.
        assert_eq!(
            h.take(b"ls\x03pwd", now),
            [Typed::Cancel {
                rest: b"pwd".to_vec()
            }]
        );
        assert_eq!(h.kept(), b"ls");
    }

    #[test]
    fn typing_past_the_cap_is_dropped_with_one_message() {
        let mut h = Held::new(0, Vec::new(), None, WAIT);
        let now = Instant::now();
        let big = vec![b'x'; MAX_QUEUED];
        assert_eq!(h.take(&big, now), []);
        assert_eq!(h.take(b"y", now), [Typed::Dropped { first: true }]);
        assert_eq!(h.take(b"z", now), []);
        assert_eq!(h.kept().len(), MAX_QUEUED);
    }
}
