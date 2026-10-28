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
