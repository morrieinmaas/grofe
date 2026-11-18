//! The attach loop while it is, or is going, offline: a drop, the backoff
//! and the reconnect bar, a reconnect or switch attempt and its result, and
//! following the session the client switched to.

use std::sync::Arc;
use std::time::Instant;

use super::{
    Conn, Failure, Kind, Loop, RemoteAttach, State, Switch, draw_bottom_row, handshake,
    hook_events, post,
};
use crate::client::Out;
use crate::remote::reconnect::{Clocks, DetachKey, bar_text, woke};

impl Loop {
    /// While offline: starts the next attempt when it is due (at once after
    /// a sleep, `clocks` being the previous look) and keeps the bar drawn.
    pub(super) fn offline_step(&mut self, a: &RemoteAttach, clocks: Clocks, now: Clocks) {
        if let State::Offline { next } = &mut self.state {
            if attempt_due(next, clocks, now) {
                *next = None;
                self.attempt(a);
            }
            self.draw_bar(&a.host);
        }
    }

    /// Closes the attach channel and attaches over `to.link` on its own
    /// thread; until that is up the bar shows, as for a reconnect. Later
    /// reconnects go to the new host.
    pub(super) fn switch_to(&mut self, a: &mut RemoteAttach, to: Switch) {
        if let State::Online(c) = &self.state {
            c.attach.close("");
        }
        // The old connection's event channel closes with it (Conn's drop).
        super::hooks::drop_overlay(&mut self.overlay);
        self.generation += 1;
        let generation = self.generation;
        a.host = to.host;
        a.relink = to.relink;
        a.control_hello = to.hello.clone();
        self.params.ctx = to.ctx;
        self.params.hello = to.hello;
        // The hooks may want other events there (a new forwarding hook).
        self.params.events = hook_events(&a.hooks);
        self.params.opts.target = to.target;
        self.params.opts.created = false;
        self.params.opts.detach_others = false;
        self.params.opts.extras.cwd = None;
        self.attempt = self.backoff.attempt();
        self.state = State::Offline { next: None };
        self.held = Some(Vec::new());
        self.bar.clear();
        let params = self.params.clone();
        let out = self.out_tx.clone();
        std::thread::spawn(move || {
            let result = match (to.connect)() {
                Ok(link) => handshake(link, &params, generation, &out),
                Err(e) => Err(Failure::Retry(e)),
            };
            post(&out, generation, Kind::Connected(result.map(Box::new)));
        });
    }

    /// The attach channel ended: a dropped link goes offline to reconnect,
    /// anything else (or `remote-reconnect off`) ends the client.
    pub(super) fn lost(
        &mut self,
        a: &mut RemoteAttach,
        why: String,
    ) -> Option<Result<String, String>> {
        let State::Online(c) = &self.state else {
            return None;
        };
        // The link itself ended: a drop. A link still up means the
        // server closed the attach (it exited): as a local client.
        let dropped = c.link.conn().is_closed() || c.link.dropped().is_some();
        if !a.reconnect || !dropped {
            return Some(Err(if why.is_empty() {
                "lost server".to_string()
            } else {
                format!("lost server: {}", crate::remote::ssh::sanitize(&why))
            }));
        }
        let now = Instant::now();
        self.backoff.dropped(now);
        super::hooks::drop_overlay(&mut self.overlay);
        self.detach = DetachKey::new(c.prefix);
        let session = c.session.clone();
        self.params.opts.target = session.or(self.params.opts.target.take());
        // Counted before the delay is: the first retry is attempt 1.
        self.attempt = self.backoff.attempt();
        self.state = State::Offline {
            next: Some(now + self.backoff.next_delay(self.jitter.sample())),
        };
        self.bar.clear();
        self.draw_bar(&a.host);
        None
    }

    /// A reconnect or switch attempt finished.
    pub(super) fn connected(
        &mut self,
        a: &mut RemoteAttach,
        result: Result<Box<Conn>, Failure>,
    ) -> Option<Result<String, String>> {
        match result {
            Ok(conn) => {
                self.backoff.connected(Instant::now());
                self.detach = DetachKey::new(conn.prefix);
                self.state = State::Online(conn);
                self.bar.clear();
                // Typed during a switch: as if typed now, so the hooks see
                // it too (the machine key may be in it).
                if let Some(held) = self.held.take().filter(|h| !h.is_empty()) {
                    self.input(a, held);
                }
                let _ = self.out_tx.try_send(Out::Wake);
            }
            Err(Failure::Fatal(e)) => return Some(Err(e)),
            Err(Failure::Retry(_)) => {
                // A switch that failed is a reconnect: typing is dropped.
                self.held = None;
                self.attempt = self.backoff.attempt();
                let delay = self.backoff.next_delay(self.jitter.sample());
                self.state = State::Offline {
                    next: Some(Instant::now() + delay),
                };
            }
        }
        None
    }

    /// Re-reads, off the main thread, which session this connection's
    /// client is on, so a reconnect after a `switch-client` goes back to it.
    pub(super) fn follow_session(&self) {
        let State::Online(c) = &self.state else {
            return;
        };
        let (link, hello, tty) = (c.link.clone(), self.params.hello.clone(), c.tty.clone());
        let (out, generation) = (self.out_tx.clone(), self.generation);
        std::thread::spawn(move || {
            if let Some(id) = handshake::attached_session(&link, &hello, &tty) {
                post(&out, generation, Kind::Session(id));
            }
        });
    }

    /// Starts a reconnect attempt on its own thread.
    pub(super) fn attempt(&mut self, a: &RemoteAttach) {
        self.generation += 1;
        let generation = self.generation;
        let relink = Arc::clone(&a.relink);
        let mut params = self.params.clone();
        params.opts.created = false;
        // The remembered session may be gone: then the server picks.
        params.fallback = true;
        params.opts.detach_others = false;
        params.opts.extras.cwd = None;
        let out = self.out_tx.clone();
        std::thread::spawn(move || {
            let result = match relink() {
                Ok(link) => handshake(link, &params, generation, &out),
                Err(e) => Err(Failure::Retry(e)),
            };
            post(&out, generation, Kind::Connected(result.map(Box::new)));
        });
    }

    /// Draws the reconnect bar when its text changed.
    pub(super) fn draw_bar(&mut self, host: &str) {
        let State::Offline { next } = &self.state else {
            return;
        };
        let secs = next.map_or(0, |t| {
            t.saturating_duration_since(Instant::now())
                .as_secs_f64()
                .ceil() as u64
        });
        let text = bar_text(host, self.attempt, secs);
        if text != self.bar {
            draw_bottom_row(&text);
            self.bar = text;
        }
    }
}

/// Whether the next attempt (`next`, none while one is in flight) is due at
/// `now`, `before` being the previous look at the clocks: a wake from sleep
/// (the wall clock jumped past the monotonic one) brings it forward to now.
fn attempt_due(next: &mut Option<Instant>, before: Clocks, now: Clocks) -> bool {
    if woke(before, now) && next.is_some() {
        *next = Some(now.mono);
    }
    next.is_some_and(|t| now.mono >= t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Spec: a resumed wake from sleep (a wall clock jump over 5 s) retries
    /// at once, however long the backoff had left.
    #[test]
    fn a_wake_from_sleep_makes_the_next_attempt_due_at_once() {
        let mono = Instant::now();
        let wall = std::time::SystemTime::now();
        let before = Clocks { mono, wall };
        let later = |m: u64, w: u64| Clocks {
            mono: mono + Duration::from_secs(m),
            wall: wall + Duration::from_secs(w),
        };
        let far = Some(mono + Duration::from_secs(30));
        // No jump: the 30 s wait stands.
        let mut next = far;
        assert!(!attempt_due(&mut next, before, later(1, 1)));
        assert_eq!(next, far);
        // A 10 s jump: due now.
        let mut next = far;
        assert!(attempt_due(&mut next, before, later(1, 11)));
        // While an attempt is in flight a wake starts no second one.
        let mut none = None;
        assert!(!attempt_due(&mut none, before, later(1, 11)));
        // The wait running out is due too.
        let mut next = far;
        assert!(attempt_due(&mut next, before, later(30, 30)));
    }
}
