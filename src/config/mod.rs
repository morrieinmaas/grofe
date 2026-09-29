//! Config schema, layering, provenance, show-config (spec §5).
//!
//! The config is TOML, read in layers (spec §5.1): built-in defaults,
//! `config.toml` (or the `--file` layers that replace it, R5),
//! `conf.d/*.toml` in lexical order, then `user.toml`. Later layers win,
//! and every resolved value records the file that set it.
//!
//! Loading needs no server. [`load`] reads every layer and reports every
//! error at once, each with its file, line and key; the caller applies
//! all of a [`Resolved`] or none of it (R9). T3.5 wires it into the server.
//!
//! Files are walked as spanned TOML rather than through a serde derive, so
//! that every key keeps its line and one pass can collect all errors.

mod cli;
mod load;
pub mod provenance;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

pub use cli::show_config;
pub use load::{load, load_one, load_with_file};
pub use provenance::{Layer, Provenance, Source};

use crate::server::input::Key;

/// TOML key (spec §5.2) to tmux option name (R4), for every built-in
/// option. Shared with `import-tmux` (TI.1) and the tmux front end (FE.1).
pub const OPTION_NAMES: &[(&str, &str)] = &[
    ("prefix", "prefix"),
    ("base-index", "base-index"),
    ("pane-base-index", "pane-base-index"),
    ("escape-time-ms", "escape-time"),
    ("repeat-time-ms", "repeat-time"),
    ("history-limit", "history-limit"),
    ("mouse", "mouse"),
    ("remain-on-exit", "remain-on-exit"),
    ("automatic-rename", "automatic-rename"),
    ("automatic-rename-format", "automatic-rename-format"),
    ("allow-rename", "allow-rename"),
    ("set-titles", "set-titles"),
    ("set-titles-string", "set-titles-string"),
    ("set-clipboard", "set-clipboard"),
    ("focus-events", "focus-events"),
    // Top-level keys stay above the sectioned ones: `import-tmux` writes
    // them in this order.
    ("main-pane-width", "main-pane-width"),
    ("status.lines", "status"),
    ("status.position", "status-position"),
    ("status.style", "status-style"),
    ("status.left", "status-left"),
    ("status.right", "status-right"),
    ("status.left-length", "status-left-length"),
    ("status.right-length", "status-right-length"),
    ("status.interval", "status-interval"),
    ("status.window.format", "window-status-format"),
    (
        "status.window.current-format",
        "window-status-current-format",
    ),
    ("pane-border.style", "pane-border-style"),
    ("pane-border.active-style", "pane-active-border-style"),
    ("compat-identity", "compat-identity"),
    ("allow-passthrough", "allow-passthrough"),
];

/// TOML key to tmux option name for every list option (TC.2b). A TOML
/// array of strings sets the whole list, element N from index N.
pub const LIST_NAMES: &[(&str, &str)] = &[
    ("status.rows", "status-format"),
    ("command-alias", "command-alias"),
];

/// The tmux name of a list option's TOML key.
pub fn list_tmux_name(toml_key: &str) -> Option<&'static str> {
    LIST_NAMES
        .iter()
        .find(|(t, _)| *t == toml_key)
        .map(|(_, n)| *n)
}

/// The TOML type an option takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ty {
    /// `true`/`false`, stored as tmux's `on`/`off`.
    Bool,
    Int,
    Str,
}

pub(crate) fn toml_type(toml_key: &str) -> Ty {
    match toml_key {
        "mouse" | "remain-on-exit" | "automatic-rename" | "allow-rename" | "focus-events"
        | "set-titles" => Ty::Bool,
        "base-index"
        | "pane-base-index"
        | "escape-time-ms"
        | "repeat-time-ms"
        | "history-limit"
        | "status.lines"
        | "status.left-length"
        | "status.right-length"
        | "status.interval" => Ty::Int,
        _ => Ty::Str,
    }
}

/// The tmux option name for a TOML key.
pub fn tmux_name(toml_key: &str) -> Option<&'static str> {
    OPTION_NAMES
        .iter()
        .find(|(t, _)| *t == toml_key)
        .map(|(_, n)| *n)
}

/// Where the config lives. [`Env::from_process`] reads it from the
/// environment; tests build one pointing at a tempdir.
#[derive(Debug, Clone)]
pub struct Env {
    /// `$XDG_CONFIG_HOME/grofe`, else `$HOME/.config/grofe` (R5). `None`
    /// when neither is set: only the built-in defaults load.
    pub config_dir: Option<PathBuf>,
    /// `$HOME`, for printing paths with `~`.
    pub home: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Env {
        Env::from_vars(
            std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    /// From the two variables' values; an empty value counts as unset.
    pub fn from_vars(xdg_config_home: Option<&str>, home: Option<&str>) -> Env {
        let xdg = xdg_config_home.filter(|s| !s.is_empty()).map(PathBuf::from);
        let home = home.filter(|s| !s.is_empty()).map(PathBuf::from);
        let config_dir = xdg
            .or_else(|| home.as_ref().map(|h| h.join(".config")))
            .map(|d| d.join("grofe"));
        Env { config_dir, home }
    }
}

/// One config mistake. `Display` is `<file>:<line>: <key>: <message>`,
/// dropping the line when it is 0 (the file as a whole) and the key when
/// it is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub file: PathBuf,
    pub line: usize,
    /// The dotted TOML key, e.g. `status.left` or `keys.root.M-h`.
    pub key: String,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file.display())?;
        if self.line > 0 {
            write!(f, ":{}", self.line)?;
        }
        if !self.key.is_empty() {
            write!(f, ": {}", self.key)?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for ConfigError {}

/// One resolved key binding (spec §5.2), from the last layer that bound
/// its table and key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    pub table: String,
    /// As `input::parse_key` read it; exact identity, not folded (R6).
    pub key: Key,
    /// The command string as written.
    pub command: String,
    /// `command` split by `cmd::split_command_string`. Empty means the
    /// binding is removed in this layer, stock bindings included (R11).
    pub cmds: Vec<Vec<String>>,
    pub desc: Option<String>,
    pub group: Option<String>,
    /// A stable identifier, unique across the merged config.
    pub name: Option<String>,
    pub repeat: bool,
    pub source: Source,
}

/// A fully loaded and validated config.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Every option by TOML key, in tmux's value form (`on`, `50000`,
    /// `C-b`), already normalized by the option store's validation.
    pub options: BTreeMap<&'static str, String>,
    /// Bindings in load order.
    pub keys: Vec<KeyBinding>,
    /// Every list option by TOML key (TC.2b), element N at index N.
    pub lists: BTreeMap<&'static str, Vec<String>>,
    /// Who set each option and list.
    pub provenance: Provenance,
    /// Files skipped by an unmet `requires` (R7), one line each.
    pub warnings: Vec<String>,
}

impl Resolved {
    /// The value of an option by TOML key, in tmux's form.
    pub fn get(&self, toml_key: &str) -> Option<&str> {
        self.options.get(toml_key).map(String::as_str)
    }

    /// `(tmux name, value, source)` for every option a file set, leaving
    /// out the built-in defaults: what a server applies at start or on
    /// `reload`, so values seeded at server start survive.
    pub fn configured(&self) -> impl Iterator<Item = (&'static str, &str, &Source)> {
        self.options.iter().filter_map(|(k, v)| {
            let src = self.provenance.get(k)?;
            let name = tmux_name(k)?;
            (src.layer != Layer::Defaults).then_some((name, v.as_str(), src))
        })
    }

    /// `(tmux name, elements, source)` for every list a file set, as
    /// [`Resolved::configured`] for scalars.
    pub fn configured_lists(&self) -> impl Iterator<Item = (&'static str, &[String], &Source)> {
        self.lists.iter().filter_map(|(k, v)| {
            let src = self.provenance.get(k)?;
            let name = list_tmux_name(k)?;
            (src.layer != Layer::Defaults).then_some((name, v.as_slice(), src))
        })
    }

    /// `(tmux name, value)` for every option, ready for the option store.
    pub fn tmux_options(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.options
            .iter()
            .filter_map(|(k, v)| tmux_name(k).map(|n| (n, v.as_str())))
    }
}

/// Levenshtein distance, for "did you mean" suggestions.
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The closest of `candidates` to `word`, if it is close enough to be a
/// plausible typo.
pub(crate) fn closest<'a>(
    word: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let limit = (word.chars().count() / 3).max(2);
    candidates
        .into_iter()
        .map(|c| (edit_distance(word, c), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::dispatch::options::builtin_names;

    #[test]
    fn every_builtin_option_has_a_toml_name() {
        for name in builtin_names() {
            assert!(
                OPTION_NAMES.iter().any(|(_, n)| *n == name),
                "built-in option {name} has no OPTION_NAMES row"
            );
        }
        for (toml, tmux) in OPTION_NAMES {
            assert!(
                builtin_names().any(|n| n == *tmux),
                "OPTION_NAMES row {toml} names unknown option {tmux}"
            );
        }
    }

    #[test]
    fn toml_names_are_unique() {
        let all: Vec<&str> = OPTION_NAMES
            .iter()
            .chain(LIST_NAMES)
            .map(|(t, _)| *t)
            .collect();
        for (i, a) in all.iter().enumerate() {
            assert!(all[i + 1..].iter().all(|b| a != b), "{a} listed twice");
        }
    }

    #[test]
    fn every_list_option_has_a_toml_name() {
        let lists: Vec<&str> = crate::server::dispatch::options::list_names().collect();
        let named: Vec<&str> = LIST_NAMES.iter().map(|(_, n)| *n).collect();
        assert_eq!(lists.len(), named.len());
        for name in lists {
            assert!(
                named.contains(&name),
                "list option {name} has no LIST_NAMES row"
            );
        }
    }

    #[test]
    fn edit_distance_counts_edits() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("stauts.left", "status.left"), 2);
        assert_eq!(
            closest("stauts.left", ["status.left", "status.right"]),
            Some("status.left")
        );
        assert_eq!(closest("zzz", ["status.left"]), None);
    }

    #[test]
    fn env_prefers_xdg_config_home() {
        let e = Env::from_vars(Some("/x"), Some("/h"));
        assert_eq!(e.config_dir, Some(PathBuf::from("/x/grofe")));
        let e = Env::from_vars(Some(""), Some("/h"));
        assert_eq!(e.config_dir, Some(PathBuf::from("/h/.config/grofe")));
        assert_eq!(Env::from_vars(None, None).config_dir, None);
    }

    #[test]
    fn error_display_is_file_line_key_message() {
        let e = ConfigError {
            file: PathBuf::from("/c/config.toml"),
            line: 3,
            key: "history-limit".to_string(),
            message: "bad".to_string(),
        };
        assert_eq!(e.to_string(), "/c/config.toml:3: history-limit: bad");
        let e = ConfigError {
            line: 0,
            key: String::new(),
            ..e
        };
        assert_eq!(e.to_string(), "/c/config.toml: bad");
    }
}
