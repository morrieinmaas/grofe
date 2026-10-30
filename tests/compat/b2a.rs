use crate::tp::check;

#[test]
fn hooks_have_scopes_arrays_and_run_now() {
    check("b2a", "hook-scopes", &[]);
}

#[test]
fn a_firing_hook_runs_the_nearest_scope_that_has_commands() {
    check("b2a", "hook-fire", &[]);
}

#[test]
fn notify_hooks_fire_for_what_changed() {
    check(
        "b2a",
        "hook-events",
        &[(
            "$ rename-window -t nope x",
            "tmux exits 0 here: the command-error hook is the last command of its queue",
        )],
    );
}

#[test]
fn after_hooks_run_in_the_context_tmux_gives_them() {
    check("b2a", "hook-after", &[]);
}

#[test]
fn the_options_grofe_used_to_reject_take_tmux_s_values_and_words() {
    check("b2a", "options", &[]);
}

#[test]
fn options_with_an_effect_do_what_tmux_does() {
    check("b2a", "options-effects", &[]);
}

#[test]
fn destroy_unattached_and_exit_unattached_end_what_tmux_ends() {
    check("b2a", "options-sessions", &[]);
}

#[test]
fn a_popup_takes_the_border_options() {
    check("b2a", "options-popup", &[]);
}

#[test]
fn command_flags_do_what_tmux_does() {
    check("b2a", "flags", &[]);
}

#[test]
fn a_notify_hook_s_own_commands_fire_no_hooks_and_each_change_fires() {
    check("b2a", "hook-quiet", &[]);
}

#[test]
fn new_session_s_own_attach_fires_no_client_attached() {
    check("b2a", "hook-new-attach", &[]);
}

#[test]
fn a_flag_option_refuses_true_and_false_as_tmux_does() {
    check("b2a", "options-flags", &[]);
}

#[test]
fn command_error_prints_with_the_error() {
    check("b2a", "hook-command-error", &[]);
}
