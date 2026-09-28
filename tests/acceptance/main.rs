//! Acceptance suite (spec 9): a client attached in a pty, driven by
//! `expectrl`, its output reconstructed through `alacritty_terminal` and
//! asserted on as a real outer terminal would see it.
//!
//! `harness` (T2.4) is the shared pty + reconstruction API; every scenario
//! module below is one milestone task's file, added with its own `mod` line
//! here. `self_test` (T2.4) proves the harness itself works, against the
//! T0.3 spike binary, since `grofe attach` does not exist until T2.5a; from
//! T2.5a on, scenario modules drive `grofe attach` instead (via
//! `env!("CARGO_BIN_EXE_grofe")`).

mod harness;
mod self_test;
mod t2_5a;
mod t2_5b;
