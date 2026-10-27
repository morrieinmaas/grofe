//! Reading the layers (spec §5.1) into a [`Resolved`], collecting every
//! error with its file, line and key (spec §5.2, §5.4).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use toml::Spanned;
use toml::de::{DeString, DeTable, DeValue};

use super::provenance::tilde;
use super::{
    ConfigCommand, ConfigError, ConfigHook, ConfigSource, Env, KeyBinding, LIST_NAMES, Layer,
    OPTION_NAMES, Provenance, Resolved, Source, Ty, closest, tmux_name, toml_type,
};
pub mod plugins;

use crate::cmd;
use crate::server::dispatch::options::{Options, Owner};
use crate::server::input::{Key, parse_key};
use crate::server::sources::MAX_SECONDS;

/// Loads the machine's config chain: built-in defaults, `config.toml`,
/// `conf.d/*.toml` in lexical order, `user.toml`. All errors, or none.
pub fn load(env: &Env) -> Result<Resolved, Vec<ConfigError>> {
    load_layers(env, None)
}

/// As [`load`], with `files` in place of `config.toml` (R5): each `.toml`
/// is a layer-2 file, loaded in the order given, and `conf.d` and
/// `user.toml` still load after them. A `.lua` file needs M6; any other
/// extension is an error.
pub fn load_with_file(env: &Env, files: &[PathBuf]) -> Result<Resolved, Vec<ConfigError>> {
    load_layers(env, Some(files))
}

/// One file and its includes, for runtime `source-file`: only the values
/// and bindings that file sets, without the defaults or the other layers.
/// Includes resolve against the file's own directory. The values count as
/// [`Layer::Config`]; the caller knows the layer it is loading at.
pub fn load_one(path: &Path, env: &Env) -> Result<Resolved, Vec<ConfigError>> {
    let mut l = Loader::new(env, false);
    l.load_path(path, Layer::Config, false);
    l.check_names();
    l.finish()
}

fn load_layers(env: &Env, files: Option<&[PathBuf]>) -> Result<Resolved, Vec<ConfigError>> {
    let mut l = Loader::new(env, true);
    let dir = env.config_dir.as_deref();
    match files {
        None => {
            if let Some(dir) = dir {
                l.load_path(&dir.join("config.toml"), Layer::Config, true);
            }
        }
        Some(files) => {
            for f in files {
                match f.extension().and_then(|e| e.to_str()) {
                    Some("toml") => l.load_path(f, Layer::Config, false),
                    // ponytail: layer 5 is T6.4's (M6); T6.6 passes its
                    // example through here.
                    Some("lua") => l.error(f, 0, "", "Lua config needs M6".to_string()),
                    _ => l.error(
                        f,
                        0,
                        "",
                        "not a config file: --file takes a .toml file".to_string(),
                    ),
                }
            }
        }
    }
    if let Some(dir) = dir {
        for f in l.conf_d(&dir.join("conf.d")) {
            l.load_path(&f, Layer::ConfD, false);
        }
        l.load_path(&dir.join("user.toml"), Layer::User, true);
    }
    l.check_names();
    let missing = l.plugins.missing_cmds();
    l.errors.extend(missing);
    l.finish()
}

/// Loading state: the merged result so far and every error seen.
struct Loader {
    home: Option<PathBuf>,
    /// An option store used only to validate and normalize values, so a
    /// config value is refused exactly as `set-option` would refuse it.
    store: Options,
    options: BTreeMap<&'static str, String>,
    lists: BTreeMap<&'static str, Vec<String>>,
    provenance: Provenance,
    keys: Vec<KeyBinding>,
    hooks: BTreeMap<String, ConfigHook>,
    sources: BTreeMap<String, ConfigSource>,
    plugins: plugins::Merge,
    user_options: BTreeMap<String, String>,
    environment: Vec<(String, String)>,
    load_commands: Vec<ConfigCommand>,
    /// `environment` so far with each value expanded: what a later
    /// binding's, hook's or load command's `$NAME` reads (TP.0).
    env_expanded: Vec<(String, String)>,
    errors: Vec<ConfigError>,
    warnings: Vec<String>,
    /// The include chain being loaded, for cycle detection.
    stack: Vec<PathBuf>,
}

/// The file being walked and the layer it counts as.
struct FileCtx<'a> {
    path: &'a Path,
    text: &'a str,
    layer: Layer,
}

impl FileCtx<'_> {
    fn line(&self, offset: usize) -> usize {
        let end = offset.min(self.text.len());
        self.text.as_bytes()[..end]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1
    }

    fn source(&self, line: usize) -> Source {
        Source {
            layer: self.layer,
            file: Some(self.path.to_path_buf()),
            line,
        }
    }
}

type Entry<'i> = (&'i Spanned<DeString<'i>>, &'i Spanned<DeValue<'i>>);

/// A table's entries in file order.
fn entries<'i>(t: &'i DeTable<'i>) -> Vec<Entry<'i>> {
    let mut v: Vec<Entry<'i>> = t.iter().collect();
    v.sort_by_key(|(k, _)| k.span().start);
    v
}

impl Loader {
    /// A loader; `defaults` seeds every option with its built-in value.
    fn new(env: &Env, defaults: bool) -> Loader {
        let store = Options::new(false);
        let mut options = BTreeMap::new();
        let mut provenance = Provenance::default();
        for (toml, tmux) in OPTION_NAMES.iter().filter(|_| defaults) {
            if let Some(e) = store.get(Owner::Global, tmux) {
                options.insert(*toml, e.value.clone());
                provenance.set(toml, Source::defaults());
            }
        }
        let mut lists = BTreeMap::new();
        for (toml, tmux) in LIST_NAMES.iter().filter(|_| defaults) {
            if let Some(l) = store.list(Owner::Global, tmux) {
                lists.insert(*toml, l.items.values().cloned().collect());
                provenance.set(toml, Source::defaults());
            }
        }
        Loader {
            home: env.home.clone(),
            store,
            options,
            lists,
            provenance,
            keys: Vec::new(),
            hooks: BTreeMap::new(),
            sources: BTreeMap::new(),
            plugins: plugins::Merge::default(),
            user_options: BTreeMap::new(),
            environment: Vec::new(),
            load_commands: Vec::new(),
            env_expanded: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            stack: Vec::new(),
        }
    }

    fn finish(self) -> Result<Resolved, Vec<ConfigError>> {
        if !self.errors.is_empty() {
            return Err(self.errors);
        }
        Ok(Resolved {
            options: self.options,
            lists: self.lists,
            keys: self.keys,
            hooks: self.hooks,
            sources: self.sources,
            plugins: self.plugins.resolved(),
            user_options: self.user_options,
            environment: self.environment,
            load_commands: self.load_commands,
            provenance: self.provenance,
            warnings: self.warnings,
        })
    }

    fn error(&mut self, file: &Path, line: usize, key: &str, message: String) {
        self.errors.push(ConfigError {
            file: file.to_path_buf(),
            line,
            key: key.to_string(),
            message,
        });
    }

    /// The `.toml` files in `conf.d`, in lexical order. A missing
    /// directory is no fragments.
    fn conf_d(&mut self, dir: &Path) -> Vec<PathBuf> {
        let read = match fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                self.error(dir, 0, "", format!("cannot read directory: {e}"));
                return Vec::new();
            }
        };
        let mut files: Vec<PathBuf> = read
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "toml") && p.is_file())
            .collect();
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        files
    }

    /// Loads one top-level layer file. `optional` files may be absent.
    fn load_path(&mut self, path: &Path, layer: Layer, optional: bool) {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if optional && e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => return self.error(path, 0, "", format!("cannot read: {e}")),
        };
        self.stack.push(canonical(path));
        self.load_text(path, &text, layer);
        self.stack.pop();
    }

    fn load_text(&mut self, path: &Path, text: &str, layer: Layer) {
        let f = FileCtx { path, text, layer };
        let root = match DeTable::parse(text) {
            Ok(root) => root,
            Err(e) => {
                // A fragment for a newer grofe may use syntax this one
                // cannot parse; its leading header still lets it be skipped.
                if let Some((req, _)) = leading_requires(text)
                    && let Ok(r) = Requires::parse(req)
                    && !r.met(env!("CARGO_PKG_VERSION"))
                {
                    return self.skip(path, req);
                }
                let line = e.span().map_or(0, |s| f.line(s.start));
                return self.error(path, line, "", e.message().to_string());
            }
        };
        let root = root.get_ref();
        let entries = entries(root);
        if !self.requires_met(&f, &entries) {
            return;
        }
        let mut includes = Vec::new();
        for (k, v) in entries {
            let line = f.line(k.span().start);
            match k.get_ref().as_ref() {
                "requires" => {}
                "source-file" => self.include_list(&f, line, v.get_ref(), &mut includes),
                "keys" => self.key_tables(&f, line, v.get_ref()),
                "hooks" => self.hooks(&f, line, v.get_ref()),
                "plugins" => self.plugins(&f, line, v.get_ref()),
                "user-options" => self.user_options(&f, line, v.get_ref()),
                "environment" => self.environment(&f, line, v.get_ref()),
                "load-commands" => self.load_commands(&f, line, v.get_ref()),
                _ => self.option_entry(&f, "", k, v),
            }
        }
        // Includes apply after the including file's own keys, in order.
        for (rel, line) in includes {
            self.include(&f, line, &rel);
        }
    }

    /// Whether the file's `requires` header (R7) is met. An unmet one
    /// skips the file with a warning; a malformed one is an error and the
    /// file is still checked.
    fn requires_met(&mut self, f: &FileCtx, entries: &[Entry]) -> bool {
        let Some(at) = entries.iter().position(|(k, _)| k.get_ref() == "requires") else {
            return true;
        };
        let (k, v) = entries[at];
        let line = f.line(k.span().start);
        if at > 0 {
            let msg = "must be at the top of the file, before any other key".to_string();
            self.error(f.path, line, "requires", msg);
            return true;
        }
        let Some(req) = v.get_ref().as_str() else {
            let found = v.get_ref().type_str();
            self.error(
                f.path,
                line,
                "requires",
                format!("expected a string, found {found}"),
            );
            return true;
        };
        match Requires::parse(req) {
            Err(msg) => {
                self.error(f.path, line, "requires", msg);
                true
            }
            Ok(r) if r.met(env!("CARGO_PKG_VERSION")) => true,
            Ok(_) => {
                self.skip(f.path, req);
                false
            }
        }
    }

    /// Skips `path` for an unmet `requires`, with a warning.
    fn skip(&mut self, path: &Path, req: &str) {
        self.warnings.push(format!(
            "{}: skipped: requires {req} (this is grofe {})",
            path.display(),
            env!("CARGO_PKG_VERSION")
        ));
    }

    /// `source-file = "x.toml"` or `source-file = ["a.toml", "b.toml"]`.
    fn include_list(
        &mut self,
        f: &FileCtx,
        line: usize,
        v: &DeValue,
        out: &mut Vec<(String, usize)>,
    ) {
        let bad = |found: &str| format!("expected a path or an array of paths, found {found}");
        match v {
            DeValue::String(s) => out.push((s.to_string(), line)),
            DeValue::Array(a) => {
                for item in a {
                    let item_line = f.line(item.span().start);
                    match item.get_ref() {
                        DeValue::String(s) => out.push((s.to_string(), item_line)),
                        other => {
                            let msg = bad(other.type_str());
                            self.error(f.path, item_line, "source-file", msg);
                        }
                    }
                }
            }
            other => self.error(f.path, line, "source-file", bad(other.type_str())),
        }
    }

    /// Loads `rel`, resolved against the including file's directory (spec
    /// §5.1), at the including file's layer.
    fn include(&mut self, f: &FileCtx, line: usize, rel: &str) {
        let dir = f.path.parent().unwrap_or(Path::new(""));
        let path = dir.join(rel);
        let canon = canonical(&path);
        if self.stack.contains(&canon) {
            let chain: Vec<String> = self
                .stack
                .iter()
                .chain([&canon])
                .map(|p| tilde(p, self.home.as_deref()))
                .collect();
            let msg = format!("include cycle: {}", chain.join(" -> "));
            return self.error(f.path, line, "source-file", msg);
        }
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("cannot read {}: {e}", path.display());
                return self.error(f.path, line, "source-file", msg);
            }
        };
        self.stack.push(canon);
        self.load_text(&path, &text, f.layer);
        self.stack.pop();
    }

    /// An option key under `prefix` (`""` at the top, `status` inside
    /// `[status]`).
    fn option_entry(
        &mut self,
        f: &FileCtx,
        prefix: &str,
        k: &Spanned<DeString>,
        v: &Spanned<DeValue>,
    ) {
        let full = join(prefix, k.get_ref());
        let line = f.line(k.span().start);
        if full == "status.sources" {
            return self.sources(f, line, v.get_ref());
        }
        if let Some(toml) = OPTION_NAMES.iter().map(|(t, _)| *t).find(|t| *t == full) {
            return self.set_option(f, line, toml, v.get_ref());
        }
        if let Some(toml) = LIST_NAMES.iter().map(|(t, _)| *t).find(|t| *t == full) {
            return self.set_list(f, line, toml, v.get_ref());
        }
        match v.get_ref() {
            DeValue::Table(t) if is_section(&full) || !t.is_empty() => {
                for (k, v) in entries(t) {
                    self.option_entry(f, &full, k, v);
                }
            }
            _ => {
                let mut msg = "unknown key".to_string();
                let known = OPTION_NAMES
                    .iter()
                    .chain(LIST_NAMES)
                    .map(|(t, _)| *t)
                    .chain([
                        "requires",
                        "source-file",
                        "keys",
                        "hooks",
                        "plugins",
                        "status.sources",
                        "user-options",
                        "environment",
                        "load-commands",
                    ]);
                if let Some(s) = closest(&full, known) {
                    msg.push_str(&format!(", did you mean {s}?"));
                }
                self.error(f.path, line, &full, msg);
            }
        }
    }

    fn set_option(&mut self, f: &FileCtx, line: usize, toml: &'static str, v: &DeValue) {
        let value = match toml_value(toml, v) {
            Ok(v) => v,
            Err(msg) => return self.error(f.path, line, toml, msg),
        };
        let Some(tmux) = tmux_name(toml) else {
            return;
        };
        if let Err(msg) = self.store.set(Owner::Global, tmux, &value, false) {
            return self.error(f.path, line, toml, msg);
        }
        if let Some(e) = self.store.get(Owner::Global, tmux) {
            self.options.insert(toml, e.value.clone());
        }
        self.provenance.set(toml, f.source(line));
    }

    /// A list option (TC.2b): an array of strings, the whole list.
    fn set_list(&mut self, f: &FileCtx, line: usize, toml: &'static str, v: &DeValue) {
        let bad = |found: &str| format!("expected an array of strings, found {found}");
        let DeValue::Array(a) = v else {
            return self.error(f.path, line, toml, bad(v.type_str()));
        };
        let mut items = Vec::new();
        for item in a {
            match item.get_ref() {
                DeValue::String(s) => items.push(s.to_string()),
                other => {
                    let item_line = f.line(item.span().start);
                    return self.error(f.path, item_line, toml, bad(other.type_str()));
                }
            }
        }
        self.lists.insert(toml, items);
        self.provenance.set(toml, f.source(line));
    }

    /// `[keys.<table>]` sections.
    fn key_tables(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        let DeValue::Table(tables) = v else {
            let msg = format!("expected a table of key tables, found {}", v.type_str());
            return self.error(f.path, line, "keys", msg);
        };
        for (name, t) in entries(tables) {
            let key = format!("keys.{}", name.get_ref());
            match t.get_ref() {
                DeValue::Table(t) => self.key_table(f, name.get_ref(), t),
                other => {
                    let msg = format!("expected a key table, found {}", other.type_str());
                    self.error(f.path, f.line(name.span().start), &key, msg);
                }
            }
        }
    }

    /// `[hooks]`: `<event> = "<command>"` (TC.5). The event must be one of
    /// grofe's (`server::hooks::EVENTS`) and every command must parse, as
    /// `set-hook -g` checks; a later layer replaces an earlier one's, and an
    /// empty string removes it.
    fn hooks(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        let DeValue::Table(t) = v else {
            let msg = format!("expected a table of hooks, found {}", v.type_str());
            return self.error(f.path, line, "hooks", msg);
        };
        for (k, v) in entries(t) {
            let name = k.get_ref().to_string();
            let path = format!("hooks.{name}");
            let line = f.line(k.span().start);
            if crate::server::hooks::event(&name).is_none() {
                let mut msg = "unknown hook".to_string();
                let names = crate::server::hooks::EVENTS.iter().map(|e| e.name);
                if let Some(s) = closest(&name, names) {
                    msg.push_str(&format!(", did you mean {s}?"));
                }
                self.error(f.path, line, &path, msg);
                continue;
            }
            let Some(command) = v.get_ref().as_str() else {
                let msg = format!(
                    "expected a command string, found {}",
                    v.get_ref().type_str()
                );
                self.error(f.path, line, &path, msg);
                continue;
            };
            let vars = &self.env_expanded;
            let checked =
                cmd::parse::with_variables(vars, || crate::server::hooks::check_command(command));
            let cmds = match checked {
                Ok(cmds) => cmds,
                Err(msg) => {
                    self.error(f.path, line, &path, msg);
                    continue;
                }
            };
            let hook = ConfigHook {
                command: command.to_string(),
                cmds,
                source: f.source(line),
            };
            self.hooks.insert(name, hook);
        }
    }

    /// `[user-options]` (TP.0): `<name> = "<value>"` sets the global user
    /// option `@<name>` when the config is applied, as `set -g @<name>`
    /// would; a runtime `set` of it wins after that. A later layer replaces
    /// an earlier one's value.
    fn user_options(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        for (name, value, _) in self.string_table(f, line, v, "user-options") {
            self.user_options.insert(format!("@{name}"), value);
        }
    }

    /// `[environment]` (TP.0): `<NAME> = "<value>"` sets a global
    /// environment variable when the config is applied, as
    /// `set-environment -g` would. `$NAME`, `${NAME}` and a leading `~` in
    /// the value expand then, from the global environment, as tmux expands
    /// them when it reads the line. A later layer replaces an earlier one's
    /// value.
    fn environment(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        for (name, value, line) in self.string_table(f, line, v, "environment") {
            if name.contains('=') {
                let msg = "a variable name has no =".to_string();
                self.error(f.path, line, &format!("environment.{name}"), msg);
                continue;
            }
            let vars = self.env_expanded.clone();
            let get = |n: &str| cmd::parse::with_variables(&vars, || cmd::parse::env_var(n));
            let expanded = super::expand_value(&value, &get);
            self.env_expanded.retain(|(n, _)| *n != name);
            self.env_expanded.push((name.clone(), expanded));
            match self.environment.iter_mut().find(|(n, _)| *n == name) {
                Some(entry) => entry.1 = value,
                None => self.environment.push((name, value)),
            }
        }
    }

    /// A table of string values, `(key, value, line)` in file order; a
    /// value of another type is an error.
    fn string_table(
        &mut self,
        f: &FileCtx,
        line: usize,
        v: &DeValue,
        table: &str,
    ) -> Vec<(String, String, usize)> {
        let DeValue::Table(t) = v else {
            let msg = format!("expected a table, found {}", v.type_str());
            self.error(f.path, line, table, msg);
            return Vec::new();
        };
        let mut out = Vec::new();
        for (k, v) in entries(t) {
            let name = k.get_ref().to_string();
            let line = f.line(k.span().start);
            match v.get_ref() {
                DeValue::String(s) if !name.is_empty() => out.push((name, s.to_string(), line)),
                DeValue::String(_) => {
                    self.error(f.path, line, table, "a name must not be empty".to_string());
                }
                other => {
                    let msg = format!("expected a string, found {}", other.type_str());
                    self.error(f.path, line, &format!("{table}.{name}"), msg);
                }
            }
        }
        out
    }

    /// `load-commands = ["<command>", ...]` (TP.0): command strings run in
    /// order each time the config is applied (server start and `reload`),
    /// as tmux runs a config file's commands each time it reads it, after
    /// the previous layers' ones. Each must parse, as a binding's must.
    fn load_commands(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        let bad = |found: &str| format!("expected an array of command strings, found {found}");
        let DeValue::Array(a) = v else {
            return self.error(f.path, line, "load-commands", bad(v.type_str()));
        };
        for item in a {
            let item_line = f.line(item.span().start);
            let DeValue::String(s) = item.get_ref() else {
                let msg = bad(item.get_ref().type_str());
                self.error(f.path, item_line, "load-commands", msg);
                continue;
            };
            let split = cmd::parse::with_variables(&self.env_expanded, || commands(s));
            let checked = split.and_then(|cmds| {
                let b = Parsed {
                    command: s.to_string(),
                    cmds,
                    ..Parsed::default()
                };
                self.expand_aliases(b)
            });
            match checked {
                Ok(b) if b.cmds.is_empty() => {
                    let msg = "an empty command".to_string();
                    self.error(f.path, item_line, "load-commands", msg);
                }
                Ok(b) => self.load_commands.push(ConfigCommand {
                    command: b.command,
                    cmds: b.cmds,
                    source: f.source(item_line),
                }),
                Err(msg) => self.error(f.path, item_line, "load-commands", msg),
            }
        }
    }

    /// `[status.sources.<name>]` (TC.4): `cmd` (required), `interval` and
    /// `timeout` in whole seconds, 1 to tmux's `INT_MAX`. The value lands in
    /// `@<name>`, so a name is letters, digits, `-` and `_`. A later layer
    /// replaces an earlier one's source of the same name, and an empty
    /// `cmd` removes it.
    fn sources(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        let DeValue::Table(t) = v else {
            let msg = format!("expected a table of sources, found {}", v.type_str());
            return self.error(f.path, line, "status.sources", msg);
        };
        for (k, v) in entries(t) {
            let name = k.get_ref().to_string();
            let path = format!("status.sources.{name}");
            let line = f.line(k.span().start);
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !valid {
                let msg = "a source name is letters, digits, - and _".to_string();
                self.error(f.path, line, &path, msg);
                continue;
            }
            match source(f, v.get_ref()) {
                Err((at, msg)) => self.error(f.path, at.unwrap_or(line), &path, msg),
                Ok((cmd, _, _)) if cmd.trim().is_empty() => {
                    self.sources.remove(&name);
                }
                Ok((cmd, interval, timeout)) => {
                    let s = ConfigSource {
                        cmd,
                        interval,
                        timeout,
                        source: f.source(line),
                    };
                    self.sources.insert(name, s);
                }
            }
        }
    }

    /// One key table of one file. Two entries that `parse_key` maps to the
    /// same key are an error (R6); a key bound in an earlier layer is
    /// overridden.
    fn key_table(&mut self, f: &FileCtx, table: &str, t: &DeTable) {
        let mut seen: Vec<(Key, String, usize)> = Vec::new();
        for (k, v) in entries(t) {
            let spelled = k.get_ref().to_string();
            let path = format!("keys.{table}.{spelled}");
            let line = f.line(k.span().start);
            let key = match parse_key(&spelled) {
                Ok(key) => key,
                Err(msg) => {
                    self.error(f.path, line, &path, msg);
                    continue;
                }
            };
            if let Some((_, first, first_line)) = seen.iter().find(|(s, _, _)| *s == key) {
                let msg = format!(
                    "{key} bound twice in [keys.{table}] (also as \"{first}\" on line {first_line})"
                );
                self.error(f.path, line, &path, msg);
                continue;
            }
            seen.push((key, spelled, line));
            let vars = &self.env_expanded;
            let parsed = cmd::parse::with_variables(vars, || binding(v.get_ref()));
            let b = match parsed.and_then(|b| self.expand_aliases(b)) {
                Ok(b) => b,
                Err(msg) => {
                    self.error(f.path, line, &path, msg);
                    continue;
                }
            };
            self.keys
                .retain(|old| !(old.table == table && old.key == key));
            self.keys.push(KeyBinding {
                table: table.to_string(),
                key,
                command: b.command,
                cmds: b.cmds,
                desc: b.desc,
                group: b.group,
                name: b.name,
                repeat: b.repeat,
                source: f.source(line),
            });
        }
    }

    /// A binding's commands with a `command-alias` verb expanded, as
    /// `bind-key` expands it when the key is bound (a key press runs them
    /// as bound), with the aliases the config has set so far, then checked:
    /// each must parse.
    fn expand_aliases(&self, mut b: Parsed) -> Result<Parsed, String> {
        let aliases = self
            .lists
            .get("command-alias")
            .map_or(&[][..], Vec::as_slice);
        b.cmds = std::mem::take(&mut b.cmds)
            .into_iter()
            .flat_map(|argv| {
                crate::server::dispatch::menu::expand_alias(aliases, &argv)
                    .unwrap_or_else(|| vec![argv])
            })
            .collect();
        for argv in &b.cmds {
            cmd::parse(argv).map_err(|e| e.to_string())?;
        }
        Ok(b)
    }

    /// A `name` is unique across the merged config (R6).
    fn check_names(&mut self) {
        let mut errors = Vec::new();
        for (i, b) in self.keys.iter().enumerate() {
            let Some(name) = &b.name else { continue };
            let Some(first) = self.keys[..i]
                .iter()
                .find(|o| o.name.as_deref() == Some(name))
            else {
                continue;
            };
            let first_file = first
                .source
                .file
                .as_deref()
                .map_or_else(String::new, |p| p.display().to_string());
            errors.push(ConfigError {
                file: b.source.file.clone().unwrap_or_default(),
                line: b.source.line,
                key: format!("keys.{}.{}", b.table, b.key),
                message: format!(
                    "name \"{name}\" is already used by keys.{}.{} in {first_file}:{}",
                    first.table, first.key, first.source.line
                ),
            });
        }
        self.errors.extend(errors);
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Whether `key` is a table that holds options (`status`, `pane-border`).
fn is_section(key: &str) -> bool {
    OPTION_NAMES
        .iter()
        .any(|(t, _)| t.strip_prefix(key).is_some_and(|r| r.starts_with('.')))
}

/// The path with symlinks and `..` resolved, for cycle detection; the path
/// as given if that fails.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A TOML value in the option store's string form.
fn toml_value(toml: &str, v: &DeValue) -> Result<String, String> {
    let wrong = |want: &str| format!("expected {want}, found {}", v.type_str());
    match (toml_type(toml), v) {
        (Ty::Bool, DeValue::Boolean(b)) => Ok(if *b { "on" } else { "off" }.to_string()),
        (Ty::Bool, _) => Err(wrong("a boolean")),
        (Ty::Int, DeValue::Integer(i)) => {
            let n = i64::from_str_radix(i.as_str(), i.radix())
                .map_err(|_| format!("integer out of range: {i}"))?;
            // tmux's `status` is `off`, `on` or a line count from 2.
            Ok(match (toml, n) {
                ("status.lines", 0) => "off".to_string(),
                ("status.lines", 1) => "on".to_string(),
                _ => n.to_string(),
            })
        }
        (Ty::Int, _) => Err(wrong("an integer")),
        (Ty::Str, DeValue::String(s)) => Ok(s.to_string()),
        (Ty::Str, _) => Err(wrong("a string")),
    }
}

/// A binding's fields, before its table and key are attached.
#[derive(Default)]
struct Parsed {
    command: String,
    cmds: Vec<Vec<String>>,
    desc: Option<String>,
    group: Option<String>,
    name: Option<String>,
    repeat: bool,
}

/// A binding's value: a command string, or a `{ cmd, desc, group, name,
/// repeat }` table (spec §5.2).
fn binding(v: &DeValue) -> Result<Parsed, String> {
    let mut b = Parsed::default();
    match v {
        DeValue::String(s) => b.command = s.to_string(),
        DeValue::Table(t) => {
            let mut cmd = None;
            for (k, v) in entries(t) {
                let field = k.get_ref().as_ref();
                let text = || match v.get_ref() {
                    DeValue::String(s) => Ok(s.to_string()),
                    other => Err(format!(
                        "{field}: expected a string, found {}",
                        other.type_str()
                    )),
                };
                match field {
                    "cmd" => cmd = Some(text()?),
                    "desc" => b.desc = Some(text()?),
                    "group" => b.group = Some(text()?),
                    "name" => {
                        let name = text()?;
                        if name.is_empty() {
                            return Err("name: must not be empty".to_string());
                        }
                        b.name = Some(name);
                    }
                    "repeat" => {
                        b.repeat = v.get_ref().as_bool().ok_or_else(|| {
                            format!(
                                "repeat: expected a boolean, found {}",
                                v.get_ref().type_str()
                            )
                        })?;
                    }
                    other => {
                        let mut msg = format!("unknown field {other}");
                        let fields = ["cmd", "desc", "group", "name", "repeat"];
                        if let Some(s) = closest(other, fields) {
                            msg.push_str(&format!(", did you mean {s}?"));
                        }
                        return Err(msg);
                    }
                }
            }
            b.command = cmd.ok_or("missing cmd")?;
        }
        other => {
            return Err(format!(
                "expected a command string or a table, found {}",
                other.type_str()
            ));
        }
    }
    b.cmds = commands(&b.command)?;
    Ok(b)
}

/// A source's `cmd`, `interval` and `timeout`.
type SourceFields = (String, Option<u64>, Option<u64>);

/// A source's `{ cmd, interval, timeout }` table. An error carries the line
/// of the field it is about (`None`: the source's own line).
fn source(f: &FileCtx, v: &DeValue) -> Result<SourceFields, (Option<usize>, String)> {
    let DeValue::Table(t) = v else {
        return Err((None, format!("expected a table, found {}", v.type_str())));
    };
    let (mut cmd, mut interval, mut timeout) = (None, None, None);
    for (k, v) in entries(t) {
        let field = k.get_ref().as_ref();
        let at = Some(f.line(k.span().start));
        let seconds = || match v.get_ref() {
            DeValue::Integer(i) => i64::from_str_radix(i.as_str(), i.radix())
                .ok()
                .and_then(|n| u64::try_from(n).ok())
                .filter(|n| (1..=MAX_SECONDS).contains(n))
                .ok_or_else(|| {
                    let msg = format!("{field}: expected 1 to {MAX_SECONDS} seconds, found {i}");
                    (at, msg)
                }),
            other => Err((
                at,
                format!(
                    "{field}: expected an integer (seconds), found {}",
                    other.type_str()
                ),
            )),
        };
        match field {
            "cmd" => match v.get_ref() {
                DeValue::String(s) => cmd = Some(s.to_string()),
                other => {
                    let msg = format!("cmd: expected a string, found {}", other.type_str());
                    return Err((at, msg));
                }
            },
            "interval" => interval = Some(seconds()?),
            "timeout" => timeout = Some(seconds()?),
            other => {
                let mut msg = format!("unknown field {other}");
                if let Some(s) = closest(other, ["cmd", "interval", "timeout"]) {
                    msg.push_str(&format!(", did you mean {s}?"));
                }
                return Err((at, msg));
            }
        }
    }
    let cmd = cmd.ok_or((None, "missing cmd".to_string()))?;
    Ok((cmd, interval, timeout))
}

/// A binding's command string as a checked sequence; empty unbinds (R11).
fn commands(s: &str) -> Result<Vec<Vec<String>>, String> {
    if s.trim().is_empty() {
        return Ok(Vec::new());
    }
    let cmds = cmd::split_command_string(s).map_err(|e| e.to_string())?;
    if cmds.is_empty() {
        return Err(format!("no command in {s:?}"));
    }
    // Each must parse, checked once aliases are known (`expand_aliases`).
    Ok(cmds)
}

/// The value of a `requires = "..."` line that comes before anything but
/// blank lines and comments, read without a TOML parser, and its line.
/// Only a plain double-quoted value counts.
fn leading_requires(text: &str) -> Option<(&str, usize)> {
    let (i, line) = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i, l.trim()))
        .find(|(_, l)| !l.is_empty() && !l.starts_with('#'))?;
    let rest = line.strip_prefix("requires")?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let (value, tail) = rest.split_once('"')?;
    let tail = tail.trim_start();
    (tail.is_empty() || tail.starts_with('#')).then_some((value, i + 1))
}

/// A parsed `requires = "grofe <op> X.Y[.Z]"` header (R7).
struct Requires {
    op: &'static str,
    version: [u64; 3],
}

impl Requires {
    fn parse(s: &str) -> Result<Requires, String> {
        let bad = || {
            format!(
                "malformed requires {s:?}: expected \"grofe <op> X.Y[.Z]\" with <op> one of >= > <= < ="
            )
        };
        let rest = s.trim().strip_prefix("grofe").ok_or_else(bad)?.trim_start();
        let (op, ver) = [">=", "<=", ">", "<", "="]
            .into_iter()
            .find_map(|op| rest.strip_prefix(op).map(|v| (op, v)))
            .ok_or_else(bad)?;
        let version = parse_version(ver.trim(), 2).ok_or_else(bad)?;
        Ok(Requires { op, version })
    }

    fn met(&self, running: &str) -> bool {
        // Pre-release and build suffixes do not change the comparison.
        let core = running.split(['-', '+']).next().unwrap_or(running);
        let Some(running) = parse_version(core, 1) else {
            return false;
        };
        let ord = running.cmp(&self.version);
        match self.op {
            ">=" => ord.is_ge(),
            ">" => ord.is_gt(),
            "<=" => ord.is_le(),
            "<" => ord.is_lt(),
            _ => ord.is_eq(),
        }
    }
}

/// `X.Y[.Z]` (at least `min` parts, at most 3), missing parts as 0.
fn parse_version(s: &str, min: usize) -> Option<[u64; 3]> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < min || parts.len() > 3 {
        return None;
    }
    let mut v = [0; 3];
    for (slot, p) in v.iter_mut().zip(&parts) {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_requires_reads_only_the_first_real_line() {
        assert_eq!(
            leading_requires("# c\n\n  requires = \"grofe >= 2.0\" # x\nmore"),
            Some(("grofe >= 2.0", 3))
        );
        assert_eq!(
            leading_requires("mouse = true\nrequires = \"grofe >= 2.0\""),
            None
        );
        assert_eq!(leading_requires("requires = 'grofe >= 2.0'"), None);
        assert_eq!(leading_requires("requires = \"grofe >= 2.0\" junk"), None);
    }

    #[test]
    fn requires_grammar() {
        let r = Requires::parse("grofe >= 1.2").expect("parses");
        assert_eq!((r.op, r.version), (">=", [1, 2, 0]));
        assert!(Requires::parse("grofe >= 1").is_err());
        assert!(Requires::parse("grofe >> 1.0").is_err());
        assert!(Requires::parse("grofe => 1.0").is_err());
        assert!(Requires::parse("grofe >= 1.0.0.0").is_err());
        assert!(Requires::parse("grofe >= 1.-1").is_err());
        assert!(Requires::parse("grofe >= 1.0").is_ok_and(|r| !r.met("0.9.9")));
        assert!(Requires::parse("grofe >= 1.0").is_ok_and(|r| r.met("1.0.0-rc.1")));
    }
}
