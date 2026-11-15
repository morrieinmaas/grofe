//! Container end-to-end scenarios for ssh remote sessions (spec
//! 2026-11-15-ssh-remote.md §6). They need the containers
//! `scripts/ssh-e2e.sh` starts, and skip (pass without running) when
//! `GROFE_E2E_SSH_CONFIG` is not set, so `cargo test` stays fast and green.

mod harness;
mod smoke;
