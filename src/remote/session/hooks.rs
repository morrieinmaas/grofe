//! The seams remote features plug into: [`Hook`], [`Prompt`], the
//! [`HookCtx`] they reach the connection through, and [`Switch`]. See the
//! parent module's docs for when each runs.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use super::Conn;
use crate::protocol::Frame;
use crate::remote::connect::Link;

/// What to do with typed input: bytes for the server, bytes for the tty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Input {
    pub send: Vec<u8>,
    pub draw: Vec<u8>,
}

impl Input {
    /// Send `bytes` as they are, draw nothing.
    pub fn send(bytes: Vec<u8>) -> Input {
        Input {
            send: bytes,
            draw: Vec::new(),
        }
    }
}

/// A seam for a remote feature (forwarding, predictive echo, machines).
/// Every method has a no-op default.
pub trait Hook: Send {
    /// Events to subscribe to on every connection.
    fn events(&self) -> Vec<String> {
        Vec::new()
    }

    /// Typed input on its way to the server.
    fn on_input(&mut self, _cx: &mut HookCtx<'_>, bytes: Vec<u8>) -> Input {
        Input::send(bytes)
    }

    /// Server output about to be written to the tty; bytes to write before it.
    fn before_output(&mut self, _cx: &mut HookCtx<'_>, _bytes: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// Server output just written to the tty; extra bytes to write after it.
    fn on_output(&mut self, _cx: &mut HookCtx<'_>, _bytes: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// An event line from the control channel (the whole JSON object).
    fn on_event(&mut self, _cx: &mut HookCtx<'_>, _event: &serde_json::Value) {}

    /// About every [`TICK`](super::TICK).
    fn tick(&mut self, _cx: &mut HookCtx<'_>, _now: Instant) {}
}

/// A one-line prompt on the bottom row (reverse video). While it is up
/// every input goes to [`Prompt::key`].
pub trait Prompt: Send {
    /// The line to show.
    fn text(&self) -> String;

    /// Input while the prompt is up: `Some` closes it, sending and drawing
    /// what it carries (and the screen is redrawn); `None` keeps it up
    /// (and its text is drawn again).
    fn key(&mut self, cx: &mut HookCtx<'_>, bytes: &[u8]) -> Option<Input>;

    /// The prompt went without an answer: replaced by another, or taken
    /// down by a dropped or switched link. Its owner may raise it again.
    fn dropped(&mut self) {}

    /// Who raised it, for [`HookCtx::close_prompt_tagged`].
    fn tag(&self) -> Option<&'static str> {
        None
    }

    /// Bytes that draw the prompt as an overlay on a `cols` x `rows`
    /// screen instead of the bottom row (the machine picker's box).
    fn overlay(&self, _cols: u16, _rows: u16) -> Option<Vec<u8>> {
        None
    }
}

/// Puts `new` in `slot`; one it replaces is told ([`Prompt::dropped`]).
fn replace_prompt(slot: &mut Option<Box<dyn Prompt>>, new: Box<dyn Prompt>) {
    if let Some(mut old) = slot.replace(new) {
        old.dropped();
    }
}

/// Takes the prompt in `slot` away if it is `tag`'s; whether it did.
fn take_tagged(slot: &mut Option<Box<dyn Prompt>>, tag: &str) -> bool {
    if slot.as_ref().is_some_and(|p| p.tag() == Some(tag)) {
        *slot = None;
        return true;
    }
    false
}

/// Takes down the prompt up, if any, telling it ([`Prompt::dropped`]).
pub(super) fn drop_overlay(overlay: &mut Option<Box<dyn Prompt>>) {
    if let Some(mut p) = overlay.take() {
        p.dropped();
    }
}

/// Another link to move the attach to ([`HookCtx::switch`]).
pub struct Switch {
    /// Makes (or hands over) the link to attach on, and gets the session
    /// ready there. It runs on the switch's own thread, so a slow machine
    /// never stalls the client.
    pub connect: Box<dyn FnOnce() -> Result<Link, String> + Send>,
    /// The host, for the bar.
    pub host: String,
    /// The session to attach to there (`None`: the server picks).
    pub target: Option<String>,
    /// The attach hello's context there, and the control hello for its
    /// requests: this machine's own for the local server, the filtered
    /// remote one for another.
    pub ctx: crate::protocol::ClientCtx,
    pub hello: String,
    /// Makes a new link to that host for a reconnect; it must never prompt.
    pub relink: Arc<dyn Fn() -> Result<Link, String> + Send + Sync>,
}

/// What a hook can reach.
pub struct HookCtx<'a> {
    pub(super) link: &'a Link,
    pub(super) conn: &'a Conn,
    pub(super) host: &'a str,
    pub(super) hello: &'a str,
    pub(super) tty: &'a str,
    pub(super) prompt: &'a mut Option<Box<dyn Prompt>>,
    pub(super) draw: Vec<u8>,
    pub(super) generation: u64,
    pub(super) switch: &'a mut Option<Switch>,
}

impl HookCtx<'_> {
    /// The live link: open channels (`tcp`, `upload`), read the RTT.
    pub fn link(&self) -> &Link {
        self.link
    }

    /// The remote host, for messages.
    pub fn host(&self) -> &str {
        self.host
    }

    /// Which connection this is: it changes with every reconnect, when
    /// the server forgets this client and draws its screen anew.
    pub fn connection(&self) -> u64 {
        self.generation
    }

    /// The smoothed round trip time ([`Link::rtt`]).
    pub fn rtt(&self) -> Option<Duration> {
        self.link.rtt()
    }

    /// A control request on its own channel; its output or error.
    pub fn request(&self, argv: &[String]) -> Result<String, String> {
        self.link.request(self.hello, argv)
    }

    /// A control request on its own thread, so a slow link does not stall
    /// the main loop; its output or error arrives on the receiver.
    pub fn request_bg(&self, argv: Vec<String>) -> Receiver<Result<String, String>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (link, hello) = (self.link.clone(), self.hello.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(link.request(&hello, &argv));
        });
        rx
    }

    /// Writes `bytes` to the tty once the hook call returns.
    pub fn draw(&mut self, bytes: &[u8]) {
        self.draw.extend_from_slice(bytes);
    }

    /// This client's name for `-t`/`-c` in requests.
    pub fn client(&self) -> &str {
        self.tty
    }

    /// Asks the server to draw this client's screen anew.
    pub fn redraw(&self) {
        // In the background: a slow link must not stall the main loop.
        drop(self.request_bg(vec!["refresh-client".into(), "-t".into(), self.tty.into()]));
    }

    /// Raises `prompt`, replacing one already up.
    pub fn prompt(&mut self, prompt: Box<dyn Prompt>) {
        replace_prompt(self.prompt, prompt);
    }

    /// Takes the prompt down unanswered, if one is up, and redraws.
    pub fn close_prompt(&mut self) {
        if self.prompt.take().is_some() {
            self.redraw();
        }
    }

    /// [`HookCtx::close_prompt`], only when the prompt up is `tag`'s.
    pub fn close_prompt_tagged(&mut self, tag: &str) {
        if take_tagged(self.prompt, tag) {
            self.redraw();
        }
    }

    /// Sends `bytes` to the server as typed input, outside [`Hook::on_input`]
    /// (a result that arrives later).
    pub fn send(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.conn.send(Frame::Input(bytes));
        }
    }

    /// Whether a prompt is up.
    pub fn prompting(&self) -> bool {
        self.prompt.is_some()
    }

    /// Moves the attach to `to` once the hooks return: the current attach
    /// channel closes (its session keeps running) and `to` attaches.
    pub fn switch(&mut self, to: Switch) {
        *self.switch = Some(to);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A prompt that counts its drops.
    struct Counted {
        tag: Option<&'static str>,
        drops: Arc<AtomicUsize>,
    }

    impl Prompt for Counted {
        fn text(&self) -> String {
            String::new()
        }
        fn key(&mut self, _cx: &mut HookCtx<'_>, _bytes: &[u8]) -> Option<Input> {
            None
        }
        fn dropped(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
        fn tag(&self) -> Option<&'static str> {
            self.tag
        }
    }

    fn counted(tag: Option<&'static str>) -> (Box<dyn Prompt>, Arc<AtomicUsize>) {
        let drops = Arc::new(AtomicUsize::new(0));
        let p = Counted {
            tag,
            drops: Arc::clone(&drops),
        };
        (Box::new(p), drops)
    }

    #[test]
    fn a_replaced_or_lost_prompt_is_told() {
        let mut slot = None;
        let (first, first_drops) = counted(Some("forward"));
        replace_prompt(&mut slot, first);
        let (picker, picker_drops) = counted(None);
        replace_prompt(&mut slot, picker);
        assert_eq!(first_drops.load(Ordering::SeqCst), 1);
        drop_overlay(&mut slot);
        assert_eq!(picker_drops.load(Ordering::SeqCst), 1);
        assert!(slot.is_none());
    }

    #[test]
    fn a_tagged_close_leaves_another_hooks_prompt_up() {
        let mut slot = None;
        let (picker, _) = counted(None);
        replace_prompt(&mut slot, picker);
        assert!(!take_tagged(&mut slot, "forward"));
        assert!(slot.is_some(), "the picker stays");
        let (ours, _) = counted(Some("forward"));
        replace_prompt(&mut slot, ours);
        assert!(take_tagged(&mut slot, "forward"));
        assert!(slot.is_none());
    }
}
