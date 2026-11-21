//! Termination signals: seen in their handler, and the write they cause.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

use super::writer::Io;

/// Set by the handler of SIGTERM, SIGHUP or SIGINT as the signal arrives.
static TERMINATING: AtomicBool = AtomicBool::new(false);

/// The handlers [`on_termination`] replaced (the runtime's), by signal.
static PREVIOUS: [OnceLock<SigAction>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];

/// The signals [`on_termination`] watches, in [`PREVIOUS`] order.
const WATCHED: [Signal; 3] = [Signal::SIGTERM, Signal::SIGHUP, Signal::SIGINT];

/// Whether a termination signal has reached this process. It is set in
/// the signal handler itself: the runtime hands a signal to its listeners
/// only on a later poll, and a pane the same signal killed can reach the
/// actor first. A signal handled before the pane exit it caused is seen
/// here by the time the actor gets that exit.
pub(super) fn terminating() -> bool {
    TERMINATING.load(Ordering::SeqCst)
}

/// Sets [`TERMINATING`], then runs the handler it replaced.
extern "C" fn on_termination(
    sig: nix::libc::c_int,
    info: *mut nix::libc::siginfo_t,
    ctx: *mut std::ffi::c_void,
) {
    TERMINATING.store(true, Ordering::SeqCst);
    for (s, prev) in WATCHED.iter().zip(&PREVIOUS) {
        if *s as nix::libc::c_int == sig
            && let Some(act) = prev.get()
        {
            chain(act, sig, info, ctx);
        }
    }
}

/// Calls the handler `act` holds, the kind its `SA_SIGINFO` flag names
/// (`SigAction::handler` reads the flag); the default and ignore actions
/// are never stored.
fn chain(
    act: &SigAction,
    sig: nix::libc::c_int,
    info: *mut nix::libc::siginfo_t,
    ctx: *mut std::ffi::c_void,
) {
    match act.handler() {
        SigHandler::Handler(f) => f(sig),
        SigHandler::SigAction(f) => f(sig, info, ctx),
        SigHandler::SigDfl | SigHandler::SigIgn => {}
    }
}

/// Puts [`on_termination`] in front of the runtime's handlers of SIGTERM,
/// SIGHUP and SIGINT (installed by `serve` and [`spawn_signals`]).
pub(super) fn watch_termination() {
    for (sig, slot) in WATCHED.into_iter().zip(&PREVIOUS) {
        let new = SigAction::new(
            SigHandler::SigAction(on_termination),
            SaFlags::SA_SIGINFO | SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        // SAFETY: `on_termination` is async-signal safe: an atomic store
        // and the handler it replaced.
        let Ok(old) = (unsafe { sigaction(sig, &new) }) else {
            eprintln!("grofe server: snapshot writer: cannot watch signal {sig}");
            continue;
        };
        if matches!(old.handler(), SigHandler::SigDfl | SigHandler::SigIgn) {
            // Nothing to chain to; the default is not ours to replace.
            // SAFETY: restoring the action just replaced.
            let _ = unsafe { sigaction(sig, &old) };
            continue;
        }
        // ponytail: a signal between the install and this `set` sets the
        // flag but does not run the runtime's handler (its stream sees the
        // signal only on a later poll, as before).
        let _ = slot.set(old);
    }
}

/// SIGTERM, SIGHUP and SIGINT: the last state built is written, and the
/// writer freezes. The actor handles the signal itself as before.
pub(super) fn spawn_signals(io: Arc<Io>) {
    use tokio::signal::unix::{SignalKind, signal};
    let kinds = [
        SignalKind::terminate(),
        SignalKind::hangup(),
        SignalKind::interrupt(),
    ];
    let mut signals = Vec::new();
    for kind in kinds {
        match signal(kind) {
            Ok(s) => signals.push(s),
            Err(e) => eprintln!("grofe server: snapshot writer: signal handler: {e}"),
        }
    }
    if signals.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let any = futures_util::future::select_all(signals.iter_mut().map(|s| Box::pin(s.recv())));
        let _ = any.await;
        io.write_final(io.pre_signal().as_ref());
    });
}

/// Whether a termination signal is seen within `within`.
pub(super) fn wait_terminating(within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while !terminating() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    terminating()
}

#[cfg(test)]
mod tests {
    use super::*;

    static PLAIN: AtomicBool = AtomicBool::new(false);
    static INFO: AtomicBool = AtomicBool::new(false);

    extern "C" fn plain(_: nix::libc::c_int) {
        PLAIN.store(true, Ordering::SeqCst);
    }

    extern "C" fn info(
        _: nix::libc::c_int,
        _: *mut nix::libc::siginfo_t,
        _: *mut std::ffi::c_void,
    ) {
        INFO.store(true, Ordering::SeqCst);
    }

    /// The previous handler runs, of the kind its `SA_SIGINFO` flag names.
    /// Calls `chain` directly: raising a real signal would set the
    /// process-wide termination flag the other tests read.
    #[test]
    fn chain_calls_the_previous_handler_of_either_kind() {
        let none = SigSet::empty();
        let a = SigAction::new(SigHandler::Handler(plain), SaFlags::empty(), none);
        chain(&a, 15, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(PLAIN.load(Ordering::SeqCst) && !INFO.load(Ordering::SeqCst));
        let b = SigAction::new(SigHandler::SigAction(info), SaFlags::SA_SIGINFO, none);
        chain(&b, 15, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(INFO.load(Ordering::SeqCst));
    }
}
