//! The multi-machine client's hook ([`MachinesHook`]) and its picker
//! ([`PickerPrompt`]): the machine key, the switch, the `#{client_machines}`
//! report and the forwarding of the machine on screen.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::super::{Picker, PickerOutcome, client_machines, render_picker};
use super::{Reach, Shared, ensure_session, lock, views};
use crate::remote::forward::ForwardHook;
use crate::remote::session::{Hook, HookCtx, Input, Prompt, Switch};
use crate::server::input::{Decoder, Input as Decoded, Key};

/// The client side of the machines: `machine-key` opens the picker, the
/// `#{client_machines}` text goes to the attached server whenever it
/// changes, and the forwarding hook of the machine on screen runs inside it.
pub(super) struct MachinesHook {
    pub(super) shared: Shared,
    pub(super) reaches: Vec<Reach>,
    pub(super) key: Key,
    /// Typed input as keys, the way the server reads it, so the machine
    /// key matches in the kitty keyboard protocol too.
    pub(super) decoder: Decoder,
    /// The machine attached to.
    pub(super) current: Arc<AtomicUsize>,
    /// Notifications from machines not on screen, to show locally.
    pub(super) notices: Arc<Mutex<Vec<String>>>,
    /// The forwarding hook of the remote machine on screen; replaced on a
    /// switch, which stops the old one's forwards.
    pub(super) forward: Arc<Mutex<Option<ForwardHook>>>,
    /// The text last sent, and to which machine.
    pub(super) sent: Option<(usize, String)>,
}

fn slot(s: &Mutex<Option<ForwardHook>>) -> MutexGuard<'_, Option<ForwardHook>> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

impl Hook for MachinesHook {
    fn events(&self) -> Vec<String> {
        slot(&self.forward)
            .as_ref()
            .map(Hook::events)
            .unwrap_or_default()
    }

    fn on_input(&mut self, cx: &mut HookCtx<'_>, bytes: Vec<u8>) -> Input {
        let keys = self.decoder.feed(&bytes, Instant::now(), Duration::ZERO);
        let machine_key = keys
            .iter()
            .any(|k| matches!(k, Decoded::Key(k) if k.matches(&self.key)));
        if !machine_key {
            return match slot(&self.forward).as_mut() {
                Some(f) => f.on_input(cx, bytes),
                None => Input::send(bytes),
            };
        }
        let current = self.current.load(Ordering::SeqCst);
        cx.prompt(Box::new(PickerPrompt {
            shared: Arc::clone(&self.shared),
            reaches: self.reaches.clone(),
            picker: Picker { selected: current },
            current: Arc::clone(&self.current),
            forward: Arc::clone(&self.forward),
        }));
        // ponytail: other keys typed in the same read as the machine key
        // are dropped, not sent or fed to the picker.
        Input::default()
    }

    fn before_output(&mut self, cx: &mut HookCtx<'_>, bytes: &[u8]) -> Vec<u8> {
        slot(&self.forward)
            .as_mut()
            .map(|f| f.before_output(cx, bytes))
            .unwrap_or_default()
    }

    fn on_output(&mut self, cx: &mut HookCtx<'_>, bytes: &[u8]) -> Vec<u8> {
        slot(&self.forward)
            .as_mut()
            .map(|f| f.on_output(cx, bytes))
            .unwrap_or_default()
    }

    fn on_event(&mut self, cx: &mut HookCtx<'_>, event: &serde_json::Value) {
        if let Some(f) = slot(&self.forward).as_mut() {
            f.on_event(cx, event);
        }
    }

    fn tick(&mut self, cx: &mut HookCtx<'_>, now: Instant) {
        if let Some(f) = slot(&self.forward).as_mut() {
            f.tick(cx, now);
        }
        let waiting = std::mem::take(&mut *self.notices.lock().unwrap_or_else(|p| p.into_inner()));
        for text in waiting {
            // As the server sends its own (OSC 9): the terminal shows it.
            cx.draw(&crate::server::notify::desktop::osc9(&text));
        }
        let current = self.current.load(Ordering::SeqCst);
        let text = client_machines(&views(&self.shared));
        if self.sent.as_ref() == Some(&(current, text.clone())) {
            return;
        }
        let argv = client_info_argv(cx.client(), &text);
        // In the background, so a slow link never stalls the client. An
        // older server has no such verb: the segment stays empty there.
        drop(cx.request_bg(argv));
        self.sent = Some((current, text));
    }
}

/// The request that sets `#{client_machines}` for `client` on the server
/// it is attached to.
fn client_info_argv(client: &str, text: &str) -> Vec<String> {
    ["set-client-info", "-t", client, "machines", text]
        .map(String::from)
        .to_vec()
}

/// The `M-m` picker as a [`Prompt`] drawn as a box.
struct PickerPrompt {
    shared: Shared,
    reaches: Vec<Reach>,
    picker: Picker,
    current: Arc<AtomicUsize>,
    forward: Arc<Mutex<Option<ForwardHook>>>,
}

impl Prompt for PickerPrompt {
    fn text(&self) -> String {
        format!("machines: {}", client_machines(&views(&self.shared)))
    }

    fn overlay(&self, cols: u16, rows: u16) -> Option<Vec<u8>> {
        Some(render_picker(
            &views(&self.shared),
            self.picker.selected,
            cols,
            rows,
        ))
    }

    fn key(&mut self, cx: &mut HookCtx<'_>, bytes: &[u8]) -> Option<Input> {
        let count = self.reaches.len();
        let i = match self.picker.handle(bytes, count)? {
            PickerOutcome::Close => return Some(Input::default()),
            PickerOutcome::Choose(i) => i,
        };
        if i == self.current.load(Ordering::SeqCst) {
            return Some(Input::default());
        }
        // An offline machine cannot be attached to: the picker stays up.
        let watched = lock(&self.shared)[i].link.clone()?;
        let host = lock(&self.shared)[i].view.name.clone();
        let reach = self.reaches[i].clone();
        let target = reach.session();
        let (first, hello, session) = (reach.clone(), reach.side.hello.clone(), target.clone());
        // The old machine's forwards stop here; the new one's start with
        // the attach (its events are subscribed on the new connection).
        *slot(&self.forward) = reach.forward();
        cx.switch(Switch {
            // On the switch's thread: the QUIC link (or the watcher's) and
            // the session there, so a slow machine never stalls the keys.
            connect: Box::new(move || {
                let link = if first.quic() {
                    first.attach_link()?
                } else {
                    watched
                };
                ensure_session(&link, &hello, session.as_deref())?;
                Ok(link)
            }),
            host,
            target,
            ctx: reach.side.ctx.clone(),
            hello: reach.side.hello.clone(),
            relink: Arc::new(move || reach.attach_link()),
        });
        self.current.store(i, Ordering::SeqCst);
        Some(Input::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_machines_goes_out_as_set_client_info() {
        assert_eq!(
            client_info_argv("/dev/grofe-remote/1-2", "local a!"),
            [
                "set-client-info",
                "-t",
                "/dev/grofe-remote/1-2",
                "machines",
                "local a!"
            ]
        );
    }
}
