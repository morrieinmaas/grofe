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

mod b1c;
mod b1e;
mod b4_lock;
mod copy_emacs;
mod f1;
mod float_mouse;
mod g3_graphics;
mod harness;
mod layouts;
mod m2_gate;
mod m2_robust;
mod plugin_rig;
mod self_test;
mod stale_build;
mod t2_5a;
mod t2_5b;
mod t3_0;
mod t3_2;
mod t3_3;
mod t3_4;
mod t3_6;
mod t3_8;
mod tc_10;
mod tc_12;
mod tc_6;
mod tc_7;
mod tc_9;
mod tm_1;
mod tm_2;
mod tm_3;
mod tm_5;
mod tm_6;
mod tp_11;
mod tp_12;
mod tp_2;
mod tp_7;
mod tp_8;
mod tp_9;
