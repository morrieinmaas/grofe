use crate::tp::check;

#[test]
fn hooks_have_scopes_arrays_and_run_now() {
    check("b2a", "hook-scopes", &[]);
}

#[test]
fn a_firing_hook_runs_the_nearest_scope_that_has_commands() {
    check("b2a", "hook-fire", &[]);
}
