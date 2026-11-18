//! When a dropped remote attach tries again (spec ssh-remote §2): back off
//! 0.5 s, 1, 2, 4 ... capped at 30 s with ±20% jitter; a connection that
//! stayed up 60 s starts the schedule over; a wake from sleep (the wall
//! clock jumped over 5 s past the monotonic one) retries at once. Pure:
//! the clocks and the jitter are passed in.

use std::time::{Duration, Instant, SystemTime};

use crate::server::input::{Decoder, Input as Decoded, Key, KeyCode};

/// The first delay.
pub const FIRST: Duration = Duration::from_millis(500);
/// No delay is longer.
pub const CAP: Duration = Duration::from_secs(30);
/// A connection up this long resets the backoff.
pub const STABLE: Duration = Duration::from_secs(60);
/// A wall clock jump past the monotonic clock this big is a wake.
pub const WAKE_JUMP: Duration = Duration::from_secs(5);
/// Jitter, as a fraction of the delay.
pub const JITTER: f64 = 0.2;

/// The retry schedule of one remote attach.
#[derive(Debug, Default)]
pub struct Backoff {
    /// Failed attempts since the last reset.
    failures: u32,
    /// When the current connection came up.
    up_since: Option<Instant>,
}

impl Backoff {
    pub fn new() -> Backoff {
        Backoff::default()
    }

    /// A connection came up at `now`.
    pub fn connected(&mut self, now: Instant) {
        self.up_since = Some(now);
    }

    /// The connection dropped at `now`: one that stayed up [`STABLE`]
    /// starts the schedule over.
    pub fn dropped(&mut self, now: Instant) {
        if let Some(up) = self.up_since.take()
            && now.saturating_duration_since(up) >= STABLE
        {
            self.failures = 0;
        }
    }

    /// The attempt about to be made, counting from 1.
    pub fn attempt(&self) -> u32 {
        self.failures + 1
    }

    /// The wait before the next attempt, then counts it. `jitter` is in
    /// `[-1, 1]` and scales the ±20%.
    pub fn next_delay(&mut self, jitter: f64) -> Duration {
        let base = FIRST
            .checked_mul(1u32 << self.failures.min(16))
            .unwrap_or(CAP)
            .min(CAP);
        self.failures = self.failures.saturating_add(1);
        let factor = 1.0 + JITTER * jitter.clamp(-1.0, 1.0);
        base.mul_f64(factor).min(CAP)
    }
}

/// One reading of both clocks.
#[derive(Debug, Clone, Copy)]
pub struct Clocks {
    pub mono: Instant,
    pub wall: SystemTime,
}

impl Clocks {
    pub fn now() -> Clocks {
        Clocks {
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

/// Whether the machine slept between `before` and `after`: the monotonic
/// clock stops in sleep, the wall clock does not.
pub fn woke(before: Clocks, after: Clocks) -> bool {
    let mono = after.mono.saturating_duration_since(before.mono);
    let wall = after.wall.duration_since(before.wall).unwrap_or_default();
    wall.saturating_sub(mono) > WAKE_JUMP
}

/// A small xorshift jitter source in `[-1, 1]`, seeded from the clock and
/// the pid; jitter needs no better randomness.
#[derive(Debug)]
pub struct Jitter(u64);

impl Jitter {
    pub fn new() -> Jitter {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Jitter((nanos ^ (u64::from(std::process::id()) << 32)) | 1)
    }

    pub fn sample(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

impl Default for Jitter {
    fn default() -> Jitter {
        Jitter::new()
    }
}

/// The reconnect bar.
pub fn bar_text(host: &str, attempt: u32, next_secs: Option<u64>) -> String {
    let when = match next_secs {
        Some(s) => format!("next in {s}s"),
        None => "connecting".to_string(),
    };
    format!("grofe: reconnecting to {host} (attempt {attempt}, {when}) · prefix d detach")
}

/// `text` on the bottom row in reverse video, padded or cut to the width,
/// with the cursor and its attributes saved and restored around it.
pub fn bottom_row(text: &str, cols: u16, rows: u16) -> Vec<u8> {
    let mut line = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > usize::from(cols) {
            break;
        }
        width += w;
        line.push(ch);
    }
    line.extend(std::iter::repeat_n(' ', usize::from(cols) - width));
    format!("\x1b7\x1b[{rows};1H\x1b[0m\x1b[7m{line}\x1b[0m\x1b8").into_bytes()
}

// ------------------------------------------------------------------ detach

/// Spots the detach key (the prefix, then `d`) in input typed while
/// offline.
pub struct DetachKey {
    prefix: Key,
    decoder: Decoder,
    armed: bool,
    pub fired: bool,
}

impl DetachKey {
    pub fn new(prefix: Key) -> DetachKey {
        DetachKey {
            prefix,
            decoder: Decoder::new(),
            armed: false,
            fired: false,
        }
    }

    /// Feeds typed bytes; true once the detach key was typed.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        for input in self.decoder.feed(bytes, Instant::now(), Duration::ZERO) {
            let Decoded::Key(key) = input else {
                continue;
            };
            if self.armed && key.matches(&Key::plain(KeyCode::Char('d'))) {
                self.fired = true;
            }
            self.armed = key.matches(&self.prefix);
        }
        self.fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::input::parse_prefix;

    fn ctrl_b() -> Key {
        parse_prefix("C-b").unwrap().unwrap()
    }

    #[test]
    fn the_detach_key_is_prefix_then_d_in_any_key_encoding() {
        for (bytes, fires) in [
            (&b"\x02d"[..], true),
            (b"\x1b[98;5ud", true),
            (b"\x1b[27;5;98~d", true),
            (b"d", false),
            (b"\x02x d", false),
            (b"\x02\x02d", true),
        ] {
            let mut k = DetachKey::new(ctrl_b());
            assert_eq!(k.feed(bytes), fires, "{bytes:?}");
        }
        // Split across reads.
        let mut k = DetachKey::new(ctrl_b());
        assert!(!k.feed(b"\x02"));
        assert!(k.feed(b"d"));
        // Another prefix.
        let mut k = DetachKey::new(parse_prefix("C-a").unwrap().unwrap());
        assert!(!k.feed(b"\x02d"));
        assert!(k.feed(b"\x01d"));
    }

    #[test]
    fn the_bar_fills_the_bottom_row_in_reverse_video() {
        let text = bar_text("box", 3, Some(4));
        assert_eq!(
            text,
            "grofe: reconnecting to box (attempt 3, next in 4s) · prefix d detach"
        );
        assert_eq!(
            bar_text("box", 2, None),
            "grofe: reconnecting to box (attempt 2, connecting) · prefix d detach"
        );
        let row = String::from_utf8(bottom_row("hi", 6, 24)).unwrap();
        assert_eq!(row, "\x1b7\x1b[24;1H\x1b[0m\x1b[7mhi    \x1b[0m\x1b8");
        let row = String::from_utf8(bottom_row("abcdef", 3, 2)).unwrap();
        assert!(row.contains("\x1b[7mabc\x1b[0m"), "{row:?}");
    }

    fn secs(d: Duration) -> f64 {
        d.as_secs_f64()
    }

    #[test]
    fn without_jitter_the_delays_double_up_to_the_cap() {
        let mut b = Backoff::new();
        let got: Vec<f64> = (0..10).map(|_| secs(b.next_delay(0.0))).collect();
        assert_eq!(got, [0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 30.0, 30.0, 30.0, 30.0]);
        assert_eq!(b.attempt(), 11);
        // Many failures never overflow.
        for _ in 0..100 {
            assert!(b.next_delay(0.0) <= CAP);
        }
    }

    #[test]
    fn jitter_stays_within_twenty_percent_and_under_the_cap() {
        let mut j = Jitter::new();
        for n in 0..6 {
            let base = 0.5 * f64::from(1u32 << n);
            for _ in 0..200 {
                let mut b = Backoff {
                    failures: n,
                    up_since: None,
                };
                let d = secs(b.next_delay(j.sample()));
                assert!(d >= base * 0.8 - 1e-9 && d <= base * 1.2 + 1e-9, "{n}: {d}");
            }
        }
        let mut b = Backoff {
            failures: 20,
            up_since: None,
        };
        assert_eq!(b.next_delay(1.0), CAP);
        assert_eq!(secs(b.next_delay(-1.0)), 24.0);
    }

    #[test]
    fn the_jitter_source_covers_its_range() {
        let mut j = Jitter::new();
        let xs: Vec<f64> = (0..1000).map(|_| j.sample()).collect();
        assert!(xs.iter().all(|x| (-1.0..=1.0).contains(x)));
        assert!(xs.iter().any(|x| *x < -0.5) && xs.iter().any(|x| *x > 0.5));
    }

    #[test]
    fn a_connection_up_sixty_seconds_resets_the_backoff() {
        let t0 = Instant::now();
        let mut b = Backoff::new();
        for _ in 0..4 {
            b.next_delay(0.0);
        }
        b.connected(t0);
        b.dropped(t0 + Duration::from_secs(59));
        assert_eq!(b.attempt(), 5, "a short connection keeps the count");
        b.connected(t0 + Duration::from_secs(60));
        b.dropped(t0 + Duration::from_secs(120));
        assert_eq!(b.attempt(), 1);
        assert_eq!(b.next_delay(0.0), FIRST);
    }

    #[test]
    fn a_wall_clock_jump_is_a_wake() {
        let mono = Instant::now();
        let wall = SystemTime::now();
        let before = Clocks { mono, wall };
        let tick = |m: u64, w: u64| Clocks {
            mono: mono + Duration::from_secs(m),
            wall: wall + Duration::from_secs(w),
        };
        assert!(!woke(before, tick(1, 1)));
        assert!(!woke(before, tick(1, 6)), "a 5 s jump is not over 5 s");
        assert!(woke(before, tick(1, 7)));
        assert!(woke(before, tick(0, 3600)));
        // The wall clock set back is not a wake.
        let back = Clocks {
            mono: mono + Duration::from_secs(1),
            wall: wall - Duration::from_secs(100),
        };
        assert!(!woke(before, back));
    }
}
