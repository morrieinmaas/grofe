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
        }
    }

    /// The default prefix, before the server says what the session's is.
    pub fn default_prefix() -> Option<Key> {
        parse_prefix("C-b").ok().flatten()
    }

    /// Each key of `bytes` and the bytes it came from: the read is cut into
    /// tokens first ([`tokens`]), so every span is exact, and each token is
    /// decoded on its own. A bracketed paste is one token and no key.
    fn keys(&self, bytes: &[u8]) -> Vec<(Option<Key>, Range<usize>)> {
        tokens(bytes)
            .into_iter()
            .map(|span| {
                let tok = &bytes[span.clone()];
                if tok.starts_with(super::PASTE_START) {
                    return (None, span);
                }
                let now = Instant::now();
                let mut d = Decoder::new();
                let mut inputs = d.feed(tok, now, Duration::ZERO);
                inputs.extend(d.flush(now, Duration::ZERO));
                let key = inputs.into_iter().find_map(key_of);
                (key, span)
            })
            .collect()
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

/// `bytes` cut into what a terminal sends as one key each: a CSI sequence
/// (`ESC [` ... a final byte), an SS3 one (`ESC O` and a final byte), a
/// whole bracketed paste, Meta and a byte (`ESC x`), a lone ESC, or one
/// character. A sequence broken by a byte that cannot be in it ends there.
fn tokens(bytes: &[u8]) -> Vec<Range<usize>> {
    let (start, end) = (super::PASTE_START, super::PASTE_END);
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let len = if rest.starts_with(start) {
            rest.windows(end.len())
                .position(|w| w == end)
                .map_or(rest.len(), |p| p + end.len())
        } else if rest[0] == 0x1b {
            match rest.get(1) {
                None | Some(0x1b) => 1,
                Some(b'[') => {
                    // Parameters and intermediates, then a final byte.
                    let body = rest[2..]
                        .iter()
                        .position(|b| !(0x20..=0x3f).contains(b))
                        .unwrap_or(rest.len() - 2);
                    match rest.get(2 + body) {
                        Some(b) if (0x40..=0x7e).contains(b) => 2 + body + 1,
                        _ => 2 + body,
                    }
                }
                Some(b'O') => match rest.get(2) {
                    Some(b) if (0x40..=0x7e).contains(b) => 3,
                    _ => 2,
                },
                Some(_) => 1 + utf8_len(rest[1]).min(rest.len() - 1),
            }
        } else {
            utf8_len(rest[0]).min(rest.len())
        };
        out.push(i..i + len.max(1));
        i += len.max(1);
    }
    out
}

/// The length of the UTF-8 character `lead` starts (1 for a stray byte).
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
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
    fn a_read_is_cut_into_the_keys_a_terminal_sends() {
        let cut =
            |b: &[u8]| -> Vec<Vec<u8>> { tokens(b).into_iter().map(|r| b[r].to_vec()).collect() };
        let v = |parts: &[&[u8]]| parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>();
        assert_eq!(cut(b"a\x1b[A\x1bOP"), v(&[b"a", b"\x1b[A", b"\x1bOP"]));
        assert_eq!(cut(b"\x1bO\x03"), v(&[b"\x1bO", b"\x03"]));
        assert_eq!(
            cut(b"\x1b\x1b[200~x\x1by\x1b[201~z"),
            v(&[b"\x1b", b"\x1b[200~x\x1by\x1b[201~", b"z"])
        );
        assert_eq!(
            cut(b"\x1b[99;5u\x1bx\x1b"),
            v(&[b"\x1b[99;5u", b"\x1bx", b"\x1b"])
        );
        assert_eq!(cut("é".as_bytes()), v(&["é".as_bytes()]));
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
