//! `[plugins.<name>]` (spec §7.4, T6.3): processes the server supervises.
//!
//! A plugin's table merges key by key across layers, `env` per variable
//! too, so `user.toml` can set `enabled = false` on a plugin a fragment
//! declares. `cmd` must be present once merged.

use std::collections::BTreeMap;
use std::path::Path;

use toml::de::DeValue;

use super::{ConfigError, FileCtx, Loader, Source, closest, entries};

/// What a plugin's exit leads to (`restart`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Restart {
    /// After any exit.
    #[default]
    Always,
    /// After a non-zero exit or a signal.
    OnFailure,
    /// Never.
    Never,
}

impl Restart {
    pub fn name(self) -> &'static str {
        match self {
            Restart::Always => "always",
            Restart::OnFailure => "on-failure",
            Restart::Never => "never",
        }
    }

    fn parse(s: &str) -> Option<Restart> {
        [Restart::Always, Restart::OnFailure, Restart::Never]
            .into_iter()
            .find(|r| r.name() == s)
    }
}

/// One merged `[plugins.<name>]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPlugin {
    /// The argv, with a relative path resolved against the file that set it.
    pub cmd: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
    pub restart: Restart,
    /// Who set each key: `cmd`, `enabled`, `restart`, `env.<NAME>`; a key
    /// left at its default has none.
    pub sources: BTreeMap<String, Source>,
}

impl ConfigPlugin {
    /// Whether a running plugin must restart to follow `new`: its `cmd`,
    /// `env` or `restart` changed. `enabled` is the caller's.
    pub fn runs_differently(&self, new: &ConfigPlugin) -> bool {
        self.cmd != new.cmd || self.env != new.env || self.restart != new.restart
    }
}

/// The plugin tables read so far.
#[derive(Default)]
pub(super) struct Merge {
    tables: BTreeMap<String, Partial>,
}

struct Partial {
    cmd: Option<Vec<String>>,
    env: BTreeMap<String, String>,
    enabled: Option<bool>,
    restart: Option<Restart>,
    sources: BTreeMap<String, Source>,
    /// Where the plugin was first mentioned, for a missing `cmd`.
    first: Source,
}

impl Merge {
    /// The plugins whose `cmd` is set.
    pub(super) fn resolved(&self) -> BTreeMap<String, ConfigPlugin> {
        self.tables
            .iter()
            .filter_map(|(name, p)| {
                let plugin = ConfigPlugin {
                    cmd: p.cmd.clone()?,
                    env: p.env.clone(),
                    enabled: p.enabled.unwrap_or(true),
                    restart: p.restart.unwrap_or_default(),
                    sources: p.sources.clone(),
                };
                Some((name.clone(), plugin))
            })
            .collect()
    }

    /// One error for each plugin still without a `cmd`.
    pub(super) fn missing_cmds(&self) -> Vec<ConfigError> {
        self.tables
            .iter()
            .filter(|(_, p)| p.cmd.is_none())
            .map(|(name, p)| ConfigError {
                file: p.first.file.clone().unwrap_or_default(),
                line: p.first.line,
                key: format!("plugins.{name}.cmd"),
                message: "missing cmd".to_string(),
            })
            .collect()
    }
}

impl Loader {
    /// `[plugins]`: a table of plugin tables.
    pub(super) fn plugins(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        let DeValue::Table(t) = v else {
            let msg = format!("expected a table of plugins, found {}", v.type_str());
            return self.error(f.path, line, "plugins", msg);
        };
        for (k, v) in entries(t) {
            let name = k.get_ref().to_string();
            let key = format!("plugins.{name}");
            let line = f.line(k.span().start);
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if !valid {
                let msg = "a plugin name is letters, digits, . - and _".to_string();
                self.error(f.path, line, &key, msg);
                continue;
            }
            let DeValue::Table(fields) = v.get_ref() else {
                let msg = format!("expected a table, found {}", v.get_ref().type_str());
                self.error(f.path, line, &key, msg);
                continue;
            };
            self.plugin(f, &name, line, fields);
        }
    }

    fn plugin(&mut self, f: &FileCtx, name: &str, line: usize, fields: &toml::de::DeTable) {
        let mut seen = Partial {
            cmd: None,
            env: BTreeMap::new(),
            enabled: None,
            restart: None,
            sources: BTreeMap::new(),
            first: f.source(line),
        };
        for (k, v) in entries(fields) {
            let field = k.get_ref().as_ref();
            let at = f.line(k.span().start);
            let key = format!("plugins.{name}.{field}");
            let wrong = |want: &str| format!("expected {want}, found {}", v.get_ref().type_str());
            let result: Result<(), String> = match (field, v.get_ref()) {
                ("cmd", DeValue::Array(a)) => cmd_value(a, f.path).map(|cmd| seen.cmd = Some(cmd)),
                ("cmd", _) => Err(wrong("an array of strings")),
                ("env", DeValue::Table(t)) => {
                    let mut result = Ok(());
                    for (ek, ev) in entries(t) {
                        let var = ek.get_ref().to_string();
                        match ev.get_ref() {
                            DeValue::String(s) if !var.is_empty() && !var.contains('=') => {
                                seen.env.insert(var.clone(), s.to_string());
                                seen.sources.insert(
                                    format!("env.{var}"),
                                    f.source(f.line(ek.span().start)),
                                );
                            }
                            DeValue::String(_) => {
                                result =
                                    Err("a variable name is not empty and has no =".to_string());
                            }
                            other => {
                                let at = f.line(ek.span().start);
                                let msg = format!("expected a string, found {}", other.type_str());
                                self.error(f.path, at, &format!("{key}.{var}"), msg);
                            }
                        }
                    }
                    result
                }
                ("env", _) => Err(wrong("a table of strings")),
                ("enabled", DeValue::Boolean(b)) => {
                    seen.enabled = Some(*b);
                    Ok(())
                }
                ("enabled", _) => Err(wrong("a boolean")),
                ("restart", DeValue::String(s)) => match Restart::parse(s) {
                    Some(r) => {
                        seen.restart = Some(r);
                        Ok(())
                    }
                    None => Err("expected always, on-failure or never".to_string()),
                },
                ("restart", _) => Err(wrong("a string")),
                (other, _) => {
                    let mut msg = "unknown key".to_string();
                    if let Some(s) = closest(other, ["cmd", "env", "enabled", "restart"]) {
                        msg.push_str(&format!(", did you mean {s}?"));
                    }
                    Err(msg)
                }
            };
            match result {
                Ok(()) if field != "env" => {
                    seen.sources.insert(field.to_string(), f.source(at));
                }
                Ok(()) => {}
                Err(msg) => self.error(f.path, at, &key, msg),
            }
        }
        let merged = self
            .plugins
            .tables
            .entry(name.to_string())
            .or_insert_with(|| Partial {
                cmd: None,
                env: BTreeMap::new(),
                enabled: None,
                restart: None,
                sources: BTreeMap::new(),
                first: seen.first.clone(),
            });
        merged.cmd = seen.cmd.or(merged.cmd.take());
        merged.env.extend(seen.env);
        merged.enabled = seen.enabled.or(merged.enabled);
        merged.restart = seen.restart.or(merged.restart);
        merged.sources.extend(seen.sources);
    }
}

/// A `cmd` array: strings, not empty. A path with a `/` in it that is
/// relative resolves against the directory of the file that set it:
/// `cmd[0]` always, a later word only when it names something that exists
/// there, so `["sh", "x/run.sh"]` finds its script and `["echo", "a/b"]`
/// stays as written.
fn cmd_value(a: &[toml::Spanned<DeValue>], file: &Path) -> Result<Vec<String>, String> {
    let mut argv = Vec::new();
    for item in a {
        match item.get_ref() {
            DeValue::String(s) => argv.push(s.to_string()),
            other => {
                return Err(format!(
                    "expected an array of strings, found {} in it",
                    other.type_str()
                ));
            }
        }
    }
    if argv.is_empty() || argv[0].is_empty() {
        return Err("cmd must not be empty".to_string());
    }
    let dir = file.parent().unwrap_or(Path::new(""));
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    for (i, word) in argv.iter_mut().enumerate() {
        if !word.contains('/') || word.starts_with('/') || word.starts_with('-') {
            continue;
        }
        let path = dir.join(&*word);
        if i == 0 || path.exists() {
            *word = path.display().to_string();
        }
    }
    Ok(argv)
}
