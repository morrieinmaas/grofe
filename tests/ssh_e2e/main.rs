//! Container end-to-end scenarios for ssh remote sessions (spec
//! 2026-11-15-ssh-remote.md §6). They need the containers
//! `scripts/ssh-e2e.sh` starts, and skip (pass without running) when
//! `GROFE_E2E_SSH_CONFIG` is not set, so `cargo test` stays fast and green.

/// The scenario's [`harness::E2e`], or skip the test.
macro_rules! e2e_or_skip {
    () => {
        match crate::harness::E2e::get() {
            Some(e) => e,
            None => {
                eprintln!("skipped: GROFE_E2E_SSH_CONFIG is not set (scripts/ssh-e2e.sh)");
                return;
            }
        }
    };
}

mod attach;
mod bootstrap;
mod forward;
mod harness;
mod predict;
#[path = "../acceptance/harness.rs"]
#[allow(dead_code)]
mod pty;
mod quic;
mod reconnect;
mod smoke;
