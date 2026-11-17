//! Predictive local echo in the remote client (spec ssh-remote §4): a
//! [`Hook`] that runs the [`Predictor`] on the attach stream, and reports
//! the link's round trip time to the server for `#{client_rtt}`.
//!
//! `predictive-echo` is read from the remote server on every connection
//! and followed through `option-changed`: `on` predicts always, `off`
//! never (and keeps no mirror), `auto` while the smoothed RTT is 50 ms or
//! more. The `pane-echo` hint of this client's active pane turns
//! prediction off while that pane's pty has echo off. Every request runs
//! off the main thread ([`HookCtx::request_bg`]), so a slow link never
//! stalls typing.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use super::predict::Predictor;
use super::session::{Hook, HookCtx, Input};
use crate::client;

/// `auto` predicts from this smoothed RTT up.
pub const AUTO_RTT: Duration = Duration::from_millis(50);

/// The RTT is reported again only after this long...
const REPORT_EVERY: Duration = Duration::from_secs(2);

/// ...and only when it moved by more than this many percent.
const REPORT_CHANGE_PCT: u64 = 10;

/// `predictive-echo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    On,
    Off,
}

impl Mode {
    /// The option's value; anything else is the default, `auto`.
    pub fn parse(value: &str) -> Mode {
        match value.trim() {
            "on" => Mode::On,
            "off" => Mode::Off,
            _ => Mode::Auto,
        }
    }

    /// Whether to predict at this smoothed RTT (none yet measured).
    pub fn wants(self, rtt: Option<Duration>) -> bool {
        match self {
            Mode::On => true,
            Mode::Off => false,
            Mode::Auto => rtt.is_some_and(|r| r >= AUTO_RTT),
        }
    }
}

/// The RTT in milliseconds to report now, if any: the first one, then one
/// that moved by more than [`REPORT_CHANGE_PCT`] percent, at most every
/// [`REPORT_EVERY`].
pub fn rtt_report(rtt: Duration, last: Option<(u64, Instant)>, now: Instant) -> Option<u64> {
    let ms = u64::try_from(rtt.as_millis()).unwrap_or(u64::MAX);
    let Some((sent, at)) = last else {
        return Some(ms);
    };
    let moved = ms.abs_diff(sent).saturating_mul(100) > sent.saturating_mul(REPORT_CHANGE_PCT);
    (moved && now.saturating_duration_since(at) >= REPORT_EVERY).then_some(ms)
}

/// The predictive echo hook.
pub struct PredictHook {
    mode: Mode,
    /// Absent while `off`.
    predictor: Option<Predictor>,
    /// The terminal size the predictor has.
    size: (u16, u16),
    /// Whether the predictor is enabled, as last told.
    on: bool,
    /// The echo hint as last told.
    echo_hint: bool,
    /// [`HookCtx::connection`] the state below belongs to.
    connection: Option<u64>,
    /// `pane-echo` per pane id, as the server last said.
    echo: HashMap<String, bool>,
    /// This client's active pane, once asked.
    active: Option<String>,
    mode_rx: Option<Receiver<Result<String, String>>>,
    active_rx: Option<Receiver<Result<String, String>>>,
    /// The RTT last reported and when.
    reported: Option<(u64, Instant)>,
}

impl Default for PredictHook {
    fn default() -> Self {
        Self::new()
    }
}

impl PredictHook {
    pub fn new() -> PredictHook {
        PredictHook {
            mode: Mode::Auto,
            predictor: None,
            size: (0, 0),
            on: false,
            echo_hint: true,
            connection: None,
            echo: HashMap::new(),
            active: None,
            mode_rx: None,
            active_rx: None,
            reported: None,
        }
    }

    /// Whether the active pane echoes (unknown counts as yes).
    fn active_echoes(&self) -> bool {
        self.active
            .as_ref()
            .and_then(|p| self.echo.get(p))
            .copied()
            .unwrap_or(true)
    }

    /// A new connection: the server forgot this client, so ask again, and
    /// drop pending predictions (its full redraw replaces the screen).
    fn sync_connection(&mut self, cx: &mut HookCtx<'_>) {
        if self.connection == Some(cx.connection()) {
            return;
        }
        self.connection = Some(cx.connection());
        self.echo.clear();
        self.active = None;
        self.reported = None;
        self.mode_rx = Some(cx.request_bg(vec![
            "show-options".into(),
            "-gv".into(),
            "predictive-echo".into(),
        ]));
        self.ask_active(cx);
        if let Some(p) = &mut self.predictor {
            p.resize(usize::from(self.size.0), usize::from(self.size.1));
        }
    }

    fn ask_active(&mut self, cx: &HookCtx<'_>) {
        if self.active_rx.is_none() {
            self.active_rx = Some(cx.request_bg(vec![
                "display-message".into(),
                "-p".into(),
                "-t".into(),
                cx.client().into(),
                "#{pane_id}".into(),
            ]));
        }
    }

    fn set_mode(&mut self, cx: &mut HookCtx<'_>, mode: Mode) {
        if mode == self.mode {
            return;
        }
        let was_off = self.mode == Mode::Off;
        self.mode = mode;
        if mode == Mode::Off {
            if let Some(mut p) = self.predictor.take() {
                cx.draw(&p.set_enabled(false));
            }
            self.on = false;
        } else if was_off {
            // A fresh mirror needs the whole screen.
            let _ = cx.request_bg(vec![
                "refresh-client".into(),
                "-t".into(),
                cx.client().into(),
            ]);
        }
    }

    /// The predictor at the terminal's current size, made when first needed.
    fn predictor(&mut self) -> Option<&mut Predictor> {
        if self.mode == Mode::Off {
            return None;
        }
        let size = client::tty_size();
        let (cols, rows) = (usize::from(size.0), usize::from(size.1));
        let fresh = self.predictor.is_none();
        let resized = size != self.size;
        self.size = size;
        let p = self
            .predictor
            .get_or_insert_with(|| Predictor::new(cols, rows));
        if fresh {
            // Nothing is shown yet, so there is nothing to erase.
            p.set_enabled(false);
            p.set_echo_hint(true);
            self.on = false;
            self.echo_hint = true;
        } else if resized {
            p.resize(cols, rows);
        }
        Some(p)
    }

    /// Follows the answers to the background requests and the RTT.
    fn update(&mut self, cx: &mut HookCtx<'_>) {
        if let Some(rx) = &self.mode_rx
            && let Ok(answer) = rx.try_recv()
        {
            self.mode_rx = None;
            if let Ok(value) = answer {
                self.set_mode(cx, Mode::parse(&value));
            }
        }
        if let Some(rx) = &self.active_rx
            && let Ok(answer) = rx.try_recv()
        {
            self.active_rx = None;
            self.active = answer.ok().map(|s| s.trim().to_string());
        }
        let hint = self.active_echoes();
        let wants = self.mode.wants(cx.rtt());
        if self.predictor().is_none() {
            return;
        }
        let Some(p) = self.predictor.as_mut() else {
            return;
        };
        let mut out = Vec::new();
        if hint != self.echo_hint {
            self.echo_hint = hint;
            out.extend(p.set_echo_hint(hint));
        }
        if wants != self.on {
            self.on = wants;
            out.extend(p.set_enabled(wants));
        }
        cx.draw(&out);
    }

    fn report_rtt(&mut self, cx: &HookCtx<'_>, now: Instant) {
        let Some(rtt) = cx.rtt() else { return };
        if let Some(ms) = rtt_report(rtt, self.reported, now) {
            self.reported = Some((ms, now));
            let _ = cx.request_bg(vec![
                "set-client-info".into(),
                "-t".into(),
                cx.client().into(),
                "rtt".into(),
                ms.to_string(),
            ]);
        }
    }
}

impl Hook for PredictHook {
    fn events(&self) -> Vec<String> {
        [
            "pane-echo",
            "pane-focus",
            "window-focused",
            "client-session-changed",
            "option-changed",
        ]
        .map(String::from)
        .to_vec()
    }

    fn on_input(&mut self, cx: &mut HookCtx<'_>, bytes: Vec<u8>) -> Input {
        self.sync_connection(cx);
        let now = Instant::now();
        let draw = match self.predictor() {
            Some(p) => p.on_input(&bytes, now),
            None => Vec::new(),
        };
        Input { send: bytes, draw }
    }

    fn before_output(&mut self, cx: &mut HookCtx<'_>, _bytes: &[u8]) -> Vec<u8> {
        self.sync_connection(cx);
        self.predictor()
            .map(Predictor::before_output)
            .unwrap_or_default()
    }

    fn on_output(&mut self, _cx: &mut HookCtx<'_>, bytes: &[u8]) -> Vec<u8> {
        match &mut self.predictor {
            Some(p) => p.feed_output(bytes, Instant::now()),
            None => Vec::new(),
        }
    }

    fn on_event(&mut self, cx: &mut HookCtx<'_>, event: &serde_json::Value) {
        self.sync_connection(cx);
        let field = |name: &str| event.get(name).and_then(|v| v.as_str());
        match field("event") {
            Some("pane-echo") => {
                if let (Some(pane), Some(echo)) =
                    (field("pane"), event.get("echo").and_then(|v| v.as_bool()))
                {
                    self.echo.insert(pane.to_string(), echo);
                }
            }
            Some("pane-focus" | "window-focused" | "client-session-changed") => {
                self.ask_active(cx);
            }
            Some("option-changed")
                if field("name") == Some("predictive-echo") && field("scope") == Some("global") =>
            {
                self.set_mode(cx, Mode::parse(field("value").unwrap_or("auto")));
            }
            _ => return,
        }
        self.update(cx);
    }

    fn tick(&mut self, cx: &mut HookCtx<'_>, now: Instant) {
        self.sync_connection(cx);
        self.update(cx);
        self.report_rtt(cx, now);
        if let Some(p) = &mut self.predictor {
            cx.draw(&p.tick(now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_mode_parses_with_auto_as_the_default() {
        assert_eq!(Mode::parse("on"), Mode::On);
        assert_eq!(Mode::parse("off\n"), Mode::Off);
        assert_eq!(Mode::parse("auto"), Mode::Auto);
        assert_eq!(Mode::parse(""), Mode::Auto);
    }

    #[test]
    fn auto_predicts_from_50_ms_of_rtt() {
        assert!(!Mode::Auto.wants(None));
        assert!(!Mode::Auto.wants(Some(49 * MS)));
        assert!(Mode::Auto.wants(Some(50 * MS)));
        assert!(Mode::Auto.wants(Some(300 * MS)));
    }

    #[test]
    fn on_and_off_ignore_the_rtt() {
        assert!(Mode::On.wants(None));
        assert!(Mode::On.wants(Some(MS)));
        assert!(!Mode::Off.wants(Some(500 * MS)));
    }

    #[test]
    fn the_rtt_is_reported_first_then_on_a_big_enough_change() {
        let t0 = Instant::now();
        assert_eq!(rtt_report(42 * MS, None, t0), Some(42));
        let last = Some((100, t0));
        let later = t0 + REPORT_EVERY;
        // 10 % or less is not worth a request.
        assert_eq!(rtt_report(110 * MS, last, later), None);
        assert_eq!(rtt_report(90 * MS, last, later), None);
        assert_eq!(rtt_report(111 * MS, last, later), Some(111));
        assert_eq!(rtt_report(89 * MS, last, later), Some(89));
    }

    #[test]
    fn the_rtt_is_reported_at_most_every_two_seconds() {
        let t0 = Instant::now();
        let last = Some((100, t0));
        assert_eq!(
            rtt_report(300 * MS, last, t0 + Duration::from_secs(1)),
            None
        );
        assert_eq!(rtt_report(300 * MS, last, t0 + REPORT_EVERY), Some(300));
    }

    #[test]
    fn the_active_panes_echo_is_the_hint() {
        let mut h = PredictHook::new();
        assert!(h.active_echoes(), "unknown counts as echoing");
        h.echo.insert("%1".into(), false);
        assert!(h.active_echoes(), "another pane's echo does not count");
        h.active = Some("%1".into());
        assert!(!h.active_echoes());
        h.echo.insert("%1".into(), true);
        assert!(h.active_echoes());
    }
}
