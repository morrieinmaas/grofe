//! B4: the rare flags and commands of tmux 3.7b's command table, each
//! against a golden recorded from tmux 3.7b (`golden/b4flags/`): the global
//! CLI flags, the commands grofe lacked, key names (`KP*`, C0 controls,
//! `UserN`, `0xNN`) and the flags tmux accepts that grofe refused.

use crate::tp::{check, check_with};

/// grofe reports a path under the client's real cwd, which on macOS is
/// `/private/var/...` for a tempdir under `/var/...`; the golden, recorded
/// in a container, has the plain path.
fn real_tmpdir(golden: &str) -> String {
    if cfg!(target_os = "macos") {
        golden.replace("<TMPDIR>/home", "/private<TMPDIR>/home")
    } else {
        golden.to_string()
    }
}

#[test]
fn the_global_flags_h_d_u_n_q_t_v_l_behave_as_tmux_s() {
    check("b4flags", "cli", &[]);
}

#[test]
fn lock_previous_layout_server_access_and_prompt_history_answer_as_tmux_does() {
    // tmux leaves uid 0 out of `server-access -l`, so run as root (the
    // Linux CI container) the server's own user is not listed.
    let as_root = nix::unistd::getuid().is_root();
    check_with(
        "b4flags",
        "commands",
        &[],
        |s| s.to_string(),
        |g| {
            if as_root {
                g.replace("<USER> (W)\n", "")
            } else {
                g.to_string()
            }
        },
    );
}

#[test]
fn keypad_c0_user_and_hex_key_names_send_bind_and_list_as_tmux_does() {
    check("b4flags", "keys", &[]);
}

#[test]
fn list_commands_sort_with_o_and_r_as_tmux_does() {
    check("b4flags", "sort", &[]);
}

#[test]
fn the_small_flags_join_pane_p_resize_window_lrud_select_layout_nop_and_the_rest_match() {
    check_with("b4flags", "flags-a", &[], |s| s.to_string(), real_tmpdir);
}

#[test]
fn capture_pane_c_l_f_h_a_p_and_m_match() {
    check("b4flags", "capture", &[]);
}

#[test]
fn split_window_and_new_pane_styles_and_k_m_l_and_respawn_e_match() {
    check("b4flags", "panes", &[]);
}

#[test]
fn client_flags_read_only_ignore_size_and_the_switch_client_flags_match() {
    check("b4flags", "clients", &[]);
}

#[test]
fn no_detach_on_destroy_keeps_a_client_when_its_session_goes() {
    check("b4flags", "clients2", &[]);
}

#[test]
fn a_key_closes_a_pane_kept_with_remain_on_exit_key() {
    check("b4flags", "clients3", &[]);
}
