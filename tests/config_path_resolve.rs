//! T5: `resolve_config_path` (spec 2026-11-20-settings-menu, AC1.5).

use std::path::{Path, PathBuf};

use grofe::config::resolve_config_path;

fn resolve(arg: &str) -> PathBuf {
    resolve_config_path(arg, Path::new("/w"), Some(Path::new("/h")))
}

#[test]
fn relative_joins_cwd() {
    assert_eq!(resolve("cfg/a.toml"), PathBuf::from("/w/cfg/a.toml"));
}

#[test]
fn tilde_joins_home() {
    assert_eq!(resolve("~/a.toml"), PathBuf::from("/h/a.toml"));
}

#[test]
fn absolute_is_unchanged() {
    assert_eq!(resolve("/abs/a.toml"), PathBuf::from("/abs/a.toml"));
}

#[test]
fn unicode_and_spaces_survive() {
    assert_eq!(
        resolve("~/my cfg ü/a.toml"),
        PathBuf::from("/h/my cfg ü/a.toml")
    );
}
