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
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use cli::show_config;
pub use load::agents::{
    ConfigAgents, ConfigGrid, ConfigKind, ConfigNew, ConfigPicker, ConfigSidebar,
};
pub use load::plugins::{ConfigPlugin, Restart};
pub use load::{load, load_one, load_with_file};
pub use provenance::{Layer, Provenance, Source};

use crate::server::input::Key;
use crate::server::notify::NotifyConfig;

/// Every built-in option as `(TOML key, tmux option name)` (spec §5.2, R4),
/// from the option table, the one list of them. Shared with `import-tmux`
/// (TI.1) and the tmux front end (FE.1).
pub fn option_names() -> impl Iterator<Item = (&'static str, &'static str)> {
    crate::server::dispatch::options::toml_names()
}

/// TOML key to tmux option name for every list option (TC.2b). A TOML
/// array of strings sets the whole list, element N from index N.
pub const LIST_NAMES: &[(&str, &str)] = &[
    ("status.rows", "status-format"),
    ("command-alias", "command-alias"),
    ("update-environment", "update-environment"),
    ("terminal-features", "terminal-features"),
    ("terminal-overrides", "terminal-overrides"),
    ("codepoint-widths", "codepoint-widths"),
    ("pane-colours", "pane-colours"),
    ("user-keys", "user-keys"),
    ("copy-hint-patterns", "copy-hint-patterns"),
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
    tmux_name(toml_key)
        .and_then(crate::server::dispatch::options::toml_ty_of)
        .unwrap_or(Ty::Str)
}

/// An option value (tmux form) as the TOML literal a config would write.
pub(crate) fn literal(toml: &str, value: &str) -> String {
    match (toml_type(toml), toml, value) {
        (Ty::Bool, _, v) => (v == "on").to_string(),
        (Ty::Int, "status.lines", "off") => "0".to_string(),
        (Ty::Int, "status.lines", "on") => "1".to_string(),
        (Ty::Int, _, v) => v.to_string(),
        (Ty::Str, _, v) => crate::util::quote::toml_str(v),
    }
}

/// The TOML type of an option, for the config reference.
pub fn toml_type_name(toml_key: &str) -> &'static str {
    match toml_type(toml_key) {
        Ty::Bool => "boolean",
        Ty::Int => "integer",
        Ty::Str => "string",
    }
}

/// The tmux option name for a TOML key.
pub fn tmux_name(toml_key: &str) -> Option<&'static str> {
    option_names().find(|(t, _)| *t == toml_key).map(|(_, n)| n)
}

/// Where the config lives. [`Env::from_process`] reads it from the
/// environment; tests build one pointing at a tempdir.
#[derive(Debug, Clone)]
pub struct Env {
    /// `$XDG_CONFIG_HOME/grofe`, else `$HOME/.config/grofe` (R5), or the
    /// folder of the `--config` file. `None` when neither is set: only the
    /// built-in defaults load.
    pub config_dir: Option<PathBuf>,
    /// The file read in place of `config.toml`'s layer: `<config_dir>/config.toml`,
    /// or the `--config` / `$GROFE_CONFIG` file.
    pub config_file: Option<PathBuf>,
    /// `$HOME`, for printing paths with `~`.
    pub home: Option<PathBuf>,
}

/// The absolute `--config` file of this process, set once at start.
static PROCESS_CONFIG: OnceLock<PathBuf> = OnceLock::new();

/// Makes `file` (absolute) this process's config file for every
/// [`Env::from_process`] after it. Not an environment variable: it must
/// not reach the panes.
pub fn set_process_config(file: PathBuf) {
    let _ = PROCESS_CONFIG.set(file);
}

/// The file [`set_process_config`] set.
pub fn process_config() -> Option<PathBuf> {
    PROCESS_CONFIG.get().cloned()
}

impl Env {
    /// With the file [`set_process_config`] set, else `$GROFE_CONFIG`.
    pub fn from_process() -> Env {
        Env::from_process_with(process_config())
    }

    /// With `config`, an absolute file, in place of `config.toml`; else
    /// `$GROFE_CONFIG`. The file's folder is then the config dir.
    pub fn from_process_with(config: Option<PathBuf>) -> Env {
        let mut env = Env::from_vars(
            std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        );
        if let Some(file) = config.or_else(env_config_file) {
            env.config_dir = file.parent().map(Path::to_path_buf);
            env.config_file = Some(file);
        }
        env
    }

    /// From the two variables' values; an empty value counts as unset.
    pub fn from_vars(xdg_config_home: Option<&str>, home: Option<&str>) -> Env {
        let xdg = xdg_config_home.filter(|s| !s.is_empty()).map(PathBuf::from);
        let home = home.filter(|s| !s.is_empty()).map(PathBuf::from);
        let config_dir = xdg
            .or_else(|| home.as_ref().map(|h| h.join(".config")))
            .map(|d| d.join("grofe"));
        let config_file = config_dir.as_ref().map(|d| d.join("config.toml"));
        Env {
            config_dir,
            config_file,
            home,
        }
    }
}

/// An absolute config file path: `~/` against `home`, anything else
/// against `cwd` (spec 2026-11-20 AC1.5).
pub fn resolve_config_path(arg: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    match (arg.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => cwd.join(arg),
    }
}

/// `arg` as an absolute config file path against this process's cwd and
/// `$HOME`; an unreadable cwd is an error rather than a relative path.
pub fn resolve_from_process(arg: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd =
        std::env::current_dir().map_err(|e| format!("--config: cannot resolve {arg}: {e}"))?;
    Ok(resolve_config_path(arg, &cwd, home.as_deref()))
}

/// `$GROFE_CONFIG` as an absolute path, when set and not empty. One that
/// names a directory is reported once on stderr and ignored.
pub fn env_config_checked() -> Result<Option<PathBuf>, String> {
    let Some(arg) = std::env::var("GROFE_CONFIG").ok().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let path = resolve_from_process(&arg)?;
    if path.is_dir() {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!(
                "grofe: GROFE_CONFIG: {} is a directory, ignoring it",
                path.display()
            );
        });
        return Ok(None);
    }
    Ok(Some(path))
}

/// [`env_config_checked`], an unresolvable path counting as unset.
pub fn env_config_file() -> Option<PathBuf> {
    env_config_checked().ok().flatten()
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

/// One `[hooks]` entry (TC.5): a grofe event and the command string it
/// runs, from the last layer that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigHook {
    /// The command string as written.
    pub command: String,
    /// `command` split by `cmd::split_command_string`, each command
    /// checked. Empty means the hook is removed in this layer.
    pub cmds: Vec<Vec<String>>,
    pub source: Source,
}

/// One `[status.sources.<name>]` entry (TC.4): a status source whose last
/// non-empty output line lands in `@<name>`, from the last layer that declared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    /// The shell command.
    pub cmd: String,
    /// Seconds between runs; `None` follows `status-interval`.
    pub interval: Option<u64>,
    /// Seconds a run may take before its process group is killed; `None`
    /// is `server::sources::DEFAULT_TIMEOUT`.
    pub timeout: Option<u64>,
    pub source: Source,
}

/// An `[environment]` value (TP.0) with `$NAME`, `${NAME}` and a leading
/// `~` expanded through `get`, as tmux expands a config line's words; an
/// unset name is empty.
pub fn expand_value(value: &str, get: &dyn Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut rest = value;
    if let Some(after) = rest.strip_prefix('~')
        && (after.is_empty() || after.starts_with('/'))
    {
        out.push_str(&get("HOME"));
        rest = after;
    }
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        if chars.peek() == Some(&'{') {
            chars.next();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                name.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if !(c.is_ascii_alphanumeric() || c == '_') {
                    break;
                }
                name.push(c);
                chars.next();
            }
        }
        if name.is_empty() {
            out.push('$');
        } else {
            out.push_str(&get(&name));
        }
    }
    out
}

/// One `load-commands` entry (TP.0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigCommand {
    /// The command string as written.
    pub command: String,
    /// `command` split by `cmd::split_command_string`, each command checked.
    pub cmds: Vec<Vec<String>>,
    pub source: Source,
}

/// A fully loaded and validated config.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    /// Every option by TOML key, in tmux's value form (`on`, `50000`,
    /// `C-b`), already normalized by the option store's validation.
    pub options: BTreeMap<&'static str, String>,
    /// Bindings in load order.
    pub keys: Vec<KeyBinding>,
    /// `[hooks]` by event name (TC.5).
    pub hooks: BTreeMap<String, ConfigHook>,
    /// `[status.sources]` by name (TC.4).
    pub sources: BTreeMap<String, ConfigSource>,
    /// `[plugins.<name>]` (T6.3), merged key by key across the layers.
    pub plugins: BTreeMap<String, ConfigPlugin>,
    /// `[[pack]]`: the plugin manager's specs, in file order.
    pub pack: Vec<crate::pack::spec::Spec>,
    /// `[agents]` and `[agents.kinds.<name>]`, merged key by key.
    pub agents: ConfigAgents,
    /// `[notify]`, key by key over the defaults.
    pub notify: NotifyConfig,
    /// Every list option by TOML key (TC.2b), element N at index N.
    pub lists: BTreeMap<&'static str, Vec<String>>,
    /// `[user-options]` (TP.0): global user options by name, `@` included,
    /// from the last layer that set each.
    pub user_options: BTreeMap<String, String>,
    /// `[environment]` (TP.0): global environment variables, in the order
    /// first set, each with the last layer's value.
    pub environment: Vec<(String, String)>,
    /// `load-commands` (TP.0): command strings run in order each time the
    /// config is applied, every layer's after the one before.
    pub load_commands: Vec<ConfigCommand>,
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
    use crate::server::dispatch::options::{builtin_names, grofe_only_names};

    #[test]
    fn every_builtin_option_has_a_toml_name() {
        for name in builtin_names().chain(grofe_only_names()) {
            assert!(
                option_names().any(|(_, n)| n == name),
                "built-in option {name} has no TOML key"
            );
        }
    }

    /// A TOML type must follow the option's `Kind`: booleans for `Bool`,
    /// integers for `Number`, strings for the rest. A choice option may be
    /// spelled as a boolean or an integer (`remain-on-exit`, `status.lines`).
    #[test]
    fn toml_types_agree_with_option_kinds() {
        use crate::server::dispatch::options::{Kind, kind_of};
        for (toml, tmux) in option_names() {
            let kind = kind_of(tmux).unwrap_or_else(|| panic!("{tmux} has no kind"));
            let ok = match kind {
                Kind::Bool => toml_type(toml) == Ty::Bool,
                Kind::Number { .. } => toml_type(toml) == Ty::Int,
                Kind::Choice(vs) => match toml_type(toml) {
                    Ty::Str => true,
                    Ty::Bool | Ty::Int => vs.contains(&"on") && vs.contains(&"off"),
                },
                _ => toml_type(toml) == Ty::Str,
            };
            assert!(
                ok,
                "{toml} ({tmux}) is {kind:?}, TOML says {:?}",
                toml_type(toml)
            );
        }
    }

    /// Only the two choice options the TOML spells as a boolean or an integer
    /// carry a type override (`remain-on-exit`, `status.lines`).
    #[test]
    fn only_two_options_override_their_toml_type() {
        assert_eq!(toml_type("remain-on-exit"), Ty::Bool);
        assert_eq!(toml_type("status.lines"), Ty::Int);
        assert_eq!(toml_type("set-clipboard"), Ty::Str);
        assert_eq!(toml_type("status.position"), Ty::Str);
    }

    #[test]
    fn the_restore_keys_have_their_defaults() {
        let resolved = load(&Env::from_vars(None, None)).expect("defaults load");
        assert_eq!(resolved.get("restore-on-start"), Some("on"));
        assert_eq!(resolved.get("restore-stagger-ms"), Some("1000"));
    }

    #[test]
    fn toml_names_are_unique() {
        let all: Vec<&str> = option_names()
            .chain(LIST_NAMES.iter().copied())
            .map(|(t, _)| t)
            .collect();
        for (i, a) in all.iter().enumerate() {
            assert!(all[i + 1..].iter().all(|b| a != b), "{a} listed twice");
        }
    }

    #[test]
    fn every_list_option_has_a_toml_name() {
        let lists: Vec<&str> = crate::server::dispatch::options::list_names()
            .chain(crate::server::dispatch::options::grofe_only_list_names())
            .collect();
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

    #[test]
    fn scratchpad_history_toml_range_is_0_to_100() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for ok in ["0", "100"] {
            std::fs::write(&path, format!("scratchpad-history = {ok}\n")).unwrap();
            let resolved = load_with_file(&Env::from_vars(None, None), std::slice::from_ref(&path))
                .expect("in range loads");
            assert_eq!(resolved.get("scratchpad-history"), Some(ok));
        }
        for bad in ["-1", "101"] {
            std::fs::write(&path, format!("scratchpad-history = {bad}\n")).unwrap();
            let errors = load_with_file(&Env::from_vars(None, None), std::slice::from_ref(&path))
                .expect_err("out of range is refused");
            assert!(
                errors.iter().any(|e| e.key == "scratchpad-history"),
                "{bad}: {errors:?}"
            );
        }
    }

    #[test]
    fn scratchpad_history_toml_is_int() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "scratchpad-history = 3\n").unwrap();
        let resolved = load_with_file(&Env::from_vars(None, None), std::slice::from_ref(&path))
            .expect("an integer loads");
        assert_eq!(resolved.get("scratchpad-history"), Some("3"));

        std::fs::write(&path, "scratchpad-history = \"x\"\n").unwrap();
        let errors =
            load_with_file(&Env::from_vars(None, None), &[path]).expect_err("a string is refused");
        assert!(
            errors.iter().any(|e| e.key == "scratchpad-history"),
            "the error names the key: {errors:?}"
        );
    }
}
