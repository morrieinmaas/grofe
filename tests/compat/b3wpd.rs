//! B3 WP-D: `source-file` with several paths and globs, `-q -n -v -F`, the
//! environment flags `show-environment -s` and `set-environment -r -F`,
//! and the answer to an OSC 52 clipboard query, each against a golden
//! recorded from tmux 3.7b (`golden/b3wpd/`).

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
fn source_file_takes_paths_and_globs_with_q_n_v_and_f() {
    check_with("b3wpd", "source-file", &[], |s| s.to_string(), real_tmpdir);
}

#[test]
fn show_environment_s_and_set_environment_r_and_f_match() {
    check("b3wpd", "environment", &[]);
}

#[test]
fn an_osc_52_query_is_answered_from_the_top_paste_buffer() {
    check("b3wpd", "osc52", &[]);
}
