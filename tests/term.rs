//! Tests for inner-`TERM` resolution (D7, T0.4, T3.6): the terminfo-dir
//! lookup in isolation. See the module doc on `grofe::term` and
//! docs/spike-notes.md's "TERM decision" and "T3.6 re-run" sections; the
//! end-to-end evidence for `tmux-256color` is `tests/acceptance/t3_6.rs`.

use std::fs;

use grofe::term;

fn fake_entry(root: &std::path::Path, subdir: &str, term_name: &str) {
    let dir = root.join(subdir);
    fs::create_dir_all(&dir).expect("create fake terminfo dir");
    fs::write(dir.join(term_name), b"").expect("write fake terminfo entry");
}

#[test]
fn entry_found_in_letter_layout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fake_entry(tmp.path(), "s", "screen-256color");
    assert!(term::entry_exists(
        &[tmp.path().to_path_buf()],
        "screen-256color"
    ));
}

#[test]
fn entry_found_in_hex_layout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // 's' is 0x73; macOS's ncurses shards by the two-digit hex byte, not the
    // literal letter.
    fake_entry(tmp.path(), "73", "screen-256color");
    assert!(term::entry_exists(
        &[tmp.path().to_path_buf()],
        "screen-256color"
    ));
}

#[test]
fn tmux_256color_is_the_default_when_present() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fake_entry(tmp.path(), "s", "screen-256color");
    fake_entry(tmp.path(), "t", "tmux-256color");
    assert_eq!(
        term::resolve_from(&[tmp.path().to_path_buf()]),
        "tmux-256color"
    );
}

#[test]
fn a_host_without_tmux_256color_gets_screen_256color() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fake_entry(tmp.path(), "73", "screen-256color");
    fake_entry(tmp.path(), "x", "xterm-256color");
    assert_eq!(
        term::resolve_from(&[tmp.path().to_path_buf()]),
        "screen-256color"
    );
}

#[test]
fn missing_default_falls_back() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Neither tmux-256color nor screen-256color anywhere in the candidate
    // dirs: resolution must skip straight to the last resort.
    fake_entry(tmp.path(), "x", "xterm-256color");
    assert_eq!(
        term::resolve_from(&[tmp.path().to_path_buf()]),
        "xterm-256color"
    );
}

#[test]
fn resolves_on_real_host() {
    // Both CI hosts ship tmux-256color (macOS's system ncurses and Debian's
    // ncurses-base); a regression in the search list (a dropped or mistyped
    // dir) must fail here rather than silently fall through.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        assert!(
            term::entry_exists(&term::search_dirs(), "tmux-256color"),
            "tmux-256color not found in {:?}",
            term::search_dirs()
        );
        assert_eq!(term::resolve_term(), "tmux-256color");
    }
    assert_eq!(term::COLORTERM, "truecolor");
}
