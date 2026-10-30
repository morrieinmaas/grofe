//! B4: the rare flags and commands of tmux 3.7b's command table, each
//! against a golden recorded from tmux 3.7b (`golden/b4flags/`): the global
//! CLI flags, the commands grofe lacked, key names (`KP*`, C0 controls,
//! `UserN`, `0xNN`) and the flags tmux accepts that grofe refused.

use crate::tp::check;

#[test]
fn the_global_flags_h_d_u_n_q_t_v_l_behave_as_tmux_s() {
    check("b4flags", "cli", &[]);
}

#[test]
fn lock_previous_layout_server_access_and_prompt_history_answer_as_tmux_does() {
    check("b4flags", "commands", &[]);
}

#[test]
fn keypad_c0_user_and_hex_key_names_send_bind_and_list_as_tmux_does() {
    check("b4flags", "keys", &[]);
}
