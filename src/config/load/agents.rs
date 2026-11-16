//! `[agents]` and `[agents.kinds.<name>]` (agents spec §9).
//!
//! The tables merge key by key across layers: `user.toml` can set
//! `resume` on a kind a fragment declared. Keys of the later `[agents.*]`
//! tables are not read yet and warn like any unknown key. `[agents.sidebar]`,
//! `[agents.grid]`, `[agents.picker]` and `[agents.new]` are plain tables.

use std::collections::BTreeMap;
use std::time::Duration;

use toml::de::{DeTable, DeValue};

use super::{FileCtx, Loader, closest, entries};

/// One merged `[agents.kinds.<name>]` table; a key no layer set is `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigKind {
    /// `match`: argv words that recognise the kind.
    pub matches: Option<Vec<String>>,
    pub command: Option<String>,
    pub resume: Option<String>,
    pub enabled: Option<bool>,
    /// `worktree`: `ask`, `always` or `never`, over `[agents.new]`.
    pub worktree: Option<String>,
}

/// `[agents.sidebar]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSidebar {
    pub width: u16,
    pub show_on_attach: bool,
    pub min_client_width: u16,
}

/// `[agents.grid]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigGrid {
    pub tile_width: u16,
    pub preview_lines: u16,
}

/// `[agents.picker]`, sizes in percent of the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPicker {
    pub width_pct: u16,
    pub height_pct: u16,
}

/// `[agents.new]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigNew {
    /// `ask`, `always` or `never`.
    pub worktree: String,
    pub worktree_command: Vec<String>,
}

/// The merged `[agents]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigAgents {
    pub enabled: bool,
    pub scan_interval: Duration,
    pub kinds: BTreeMap<String, ConfigKind>,
    pub sidebar: ConfigSidebar,
    pub grid: ConfigGrid,
    pub picker: ConfigPicker,
    pub new: ConfigNew,
}

impl Default for ConfigAgents {
    fn default() -> Self {
        ConfigAgents {
            enabled: true,
            scan_interval: Duration::from_secs(1),
            kinds: BTreeMap::new(),
            sidebar: ConfigSidebar {
                width: 32,
                show_on_attach: false,
                min_client_width: 120,
            },
            grid: ConfigGrid {
                tile_width: 60,
                preview_lines: 12,
            },
            picker: ConfigPicker {
                width_pct: 80,
                height_pct: 70,
            },
            new: ConfigNew {
                worktree: "never".to_string(),
                worktree_command: [
                    "git", "-C", "{dir}", "worktree", "add", "-b", "{branch}", "{path}",
                ]
                .map(String::from)
                .to_vec(),
            },
        }
    }
}

/// The agent tables read so far.
#[derive(Default)]
pub(super) struct Merge {
    enabled: Option<bool>,
    scan_interval: Option<Duration>,
    kinds: BTreeMap<String, ConfigKind>,
    /// The tables over the defaults; a key no layer set keeps its default.
    tables: Option<ConfigAgents>,
}

impl Merge {
    pub(super) fn resolved(&self) -> ConfigAgents {
        let d = self.tables.clone().unwrap_or_default();
        ConfigAgents {
            enabled: self.enabled.unwrap_or(d.enabled),
            scan_interval: self.scan_interval.unwrap_or(d.scan_interval),
            kinds: self.kinds.clone(),
            ..d
        }
    }
}

const KEYS: [&str; 7] = [
    "enabled",
    "scan-interval",
    "kinds",
    "sidebar",
    "grid",
    "picker",
    "new",
];
const KIND_KEYS: [&str; 5] = ["match", "command", "resume", "enabled", "worktree"];
const SIDEBAR_KEYS: [&str; 3] = ["width", "show-on-attach", "min-client-width"];
const GRID_KEYS: [&str; 2] = ["tile-width", "preview-lines"];
const PICKER_KEYS: [&str; 2] = ["width", "height"];
const NEW_KEYS: [&str; 2] = ["worktree", "worktree-command"];

impl Loader {
    /// `[agents]`.
    pub(super) fn agents(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        let DeValue::Table(t) = v else {
            let msg = format!("expected a table, found {}", v.type_str());
            return self.error(f.path, line, "agents", msg);
        };
        for (k, v) in entries(t) {
            let field = k.get_ref().as_ref();
            let at = f.line(k.span().start);
            let key = format!("agents.{field}");
            let wrong = |want: &str| format!("expected {want}, found {}", v.get_ref().type_str());
            let result = match (field, v.get_ref()) {
                ("enabled", DeValue::Boolean(b)) => {
                    self.agents.enabled = Some(*b);
                    Ok(())
                }
                ("enabled", _) => Err(wrong("a boolean")),
                ("scan-interval", DeValue::String(s)) => parse_duration(s)
                    .map(|d| self.agents.scan_interval = Some(d))
                    .ok_or_else(|| "expected a duration like 500ms, 1s or 2m".to_string()),
                ("scan-interval", _) => Err(wrong("a duration string")),
                ("kinds", DeValue::Table(kinds)) => {
                    self.kinds(f, kinds);
                    continue;
                }
                ("kinds", _) => Err(wrong("a table of kinds")),
                ("sidebar" | "grid" | "picker" | "new", DeValue::Table(t)) => {
                    self.agents_table(f, field, t);
                    continue;
                }
                ("sidebar" | "grid" | "picker" | "new", _) => Err(wrong("a table")),
                (other, _) => {
                    self.unknown_key(f, at, &key, other, &KEYS);
                    continue;
                }
            };
            match result {
                Ok(()) => self.provenance.set(&key, f.source(at)),
                Err(msg) => self.error(f.path, at, &key, msg),
            }
        }
    }

    fn kinds(&mut self, f: &FileCtx, t: &DeTable) {
        for (k, v) in entries(t) {
            let name = k.get_ref().to_string();
            let key = format!("agents.kinds.{name}");
            let line = f.line(k.span().start);
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if !valid {
                let msg = "a kind name is letters, digits, . - and _".to_string();
                self.error(f.path, line, &key, msg);
                continue;
            }
            let DeValue::Table(fields) = v.get_ref() else {
                let msg = format!("expected a table, found {}", v.get_ref().type_str());
                self.error(f.path, line, &key, msg);
                continue;
            };
            self.kind(f, &name, fields);
        }
    }

    fn kind(&mut self, f: &FileCtx, name: &str, fields: &DeTable) {
        let mut seen = ConfigKind::default();
        for (k, v) in entries(fields) {
            let field = k.get_ref().as_ref();
            let at = f.line(k.span().start);
            let key = format!("agents.kinds.{name}.{field}");
            let wrong = |want: &str| format!("expected {want}, found {}", v.get_ref().type_str());
            let result = match (field, v.get_ref()) {
                ("match", DeValue::Array(a)) => words(a).map(|w| seen.matches = Some(w)),
                ("match", _) => Err(wrong("an array of strings")),
                ("command", DeValue::String(s)) => {
                    seen.command = Some(s.to_string());
                    Ok(())
                }
                ("command", _) => Err(wrong("a string")),
                ("resume", DeValue::String(s)) => {
                    seen.resume = Some(s.to_string());
                    Ok(())
                }
                ("resume", _) => Err(wrong("a string")),
                ("enabled", DeValue::Boolean(b)) => {
                    seen.enabled = Some(*b);
                    Ok(())
                }
                ("enabled", _) => Err(wrong("a boolean")),
                ("worktree", DeValue::String(s)) => {
                    worktree_mode(s).map(|m| seen.worktree = Some(m))
                }
                ("worktree", _) => Err(wrong("a string")),
                (other, _) => {
                    self.unknown_key(f, at, &key, other, &KIND_KEYS);
                    continue;
                }
            };
            match result {
                Ok(()) => self.provenance.set(&key, f.source(at)),
                Err(msg) => self.error(f.path, at, &key, msg),
            }
        }
        let merged = self.agents.kinds.entry(name.to_string()).or_default();
        merged.matches = seen.matches.or(merged.matches.take());
        merged.command = seen.command.or(merged.command.take());
        merged.resume = seen.resume.or(merged.resume.take());
        merged.enabled = seen.enabled.or(merged.enabled);
        merged.worktree = seen.worktree.or(merged.worktree.take());
    }

    /// `[agents.sidebar]`, `[agents.grid]`, `[agents.picker]`, `[agents.new]`.
    fn agents_table(&mut self, f: &FileCtx, table: &str, t: &DeTable) {
        let mut cfg = self.agents.tables.take().unwrap_or_default();
        let known: &[&str] = match table {
            "sidebar" => &SIDEBAR_KEYS,
            "grid" => &GRID_KEYS,
            "picker" => &PICKER_KEYS,
            _ => &NEW_KEYS,
        };
        for (k, v) in entries(t) {
            let field = k.get_ref().as_ref();
            let at = f.line(k.span().start);
            let key = format!("agents.{table}.{field}");
            let v = v.get_ref();
            let result = match (table, field) {
                ("sidebar", "width") => num(v, 1, 200).map(|n| cfg.sidebar.width = n),
                ("sidebar", "show-on-attach") => flag(v).map(|b| cfg.sidebar.show_on_attach = b),
                ("sidebar", "min-client-width") => {
                    num(v, 0, 10_000).map(|n| cfg.sidebar.min_client_width = n)
                }
                ("grid", "tile-width") => num(v, 1, 200).map(|n| cfg.grid.tile_width = n),
                ("grid", "preview-lines") => num(v, 1, 200).map(|n| cfg.grid.preview_lines = n),
                ("picker", "width") => percent(v).map(|n| cfg.picker.width_pct = n),
                ("picker", "height") => percent(v).map(|n| cfg.picker.height_pct = n),
                ("new", "worktree") => match v {
                    DeValue::String(s) => worktree_mode(s).map(|m| cfg.new.worktree = m),
                    _ => Err(format!("expected a string, found {}", v.type_str())),
                },
                ("new", "worktree-command") => match v {
                    DeValue::Array(a) if a.is_empty() => {
                        Err("the worktree command must not be empty".to_string())
                    }
                    DeValue::Array(a) => words(a).map(|w| cfg.new.worktree_command = w),
                    _ => Err(format!(
                        "expected an array of strings, found {}",
                        v.type_str()
                    )),
                },
                _ => {
                    self.unknown_key(f, at, &key, field, known);
                    continue;
                }
            };
            match result {
                Ok(()) => self.provenance.set(&key, f.source(at)),
                Err(msg) => self.error(f.path, at, &key, msg),
            }
        }
        self.agents.tables = Some(cfg);
    }

    /// An unknown key is a warning: a later step's table must not break an
    /// older grofe reading the same file.
    fn unknown_key(&mut self, f: &FileCtx, line: usize, key: &str, word: &str, known: &[&str]) {
        let hint = match closest(word, known.iter().copied()) {
            Some(s) => format!("did you mean {s}?"),
            None => format!("did you mean one of {}?", known.join(", ")),
        };
        self.warnings.push(format!(
            "{}:{line}: {key}: unknown key, {hint}",
            f.path.display()
        ));
    }
}

/// `match`: strings, none empty.
fn words(a: &[toml::Spanned<DeValue>]) -> Result<Vec<String>, String> {
    a.iter()
        .map(|item| match item.get_ref() {
            DeValue::String(s) if !s.is_empty() => Ok(s.to_string()),
            DeValue::String(_) => Err("a match word must not be empty".to_string()),
            other => Err(format!(
                "expected an array of strings, found {} in it",
                other.type_str()
            )),
        })
        .collect()
}

fn worktree_mode(s: &str) -> Result<String, String> {
    match s {
        "ask" | "always" | "never" => Ok(s.to_string()),
        _ => Err("expected ask, always or never".to_string()),
    }
}

/// A whole number in `lo..=hi`.
fn num(v: &DeValue, lo: i64, hi: i64) -> Result<u16, String> {
    match v {
        DeValue::Integer(i) => match i.to_string().parse::<i64>() {
            Ok(n) if (lo..=hi).contains(&n) => Ok(n as u16),
            _ => Err(format!("expected a number from {lo} to {hi}")),
        },
        other => Err(format!("expected a number, found {}", other.type_str())),
    }
}

fn flag(v: &DeValue) -> Result<bool, String> {
    match v {
        DeValue::Boolean(b) => Ok(*b),
        other => Err(format!("expected a boolean, found {}", other.type_str())),
    }
}

/// `"80%"`: 1 to 100 percent.
fn percent(v: &DeValue) -> Result<u16, String> {
    let DeValue::String(s) = v else {
        return Err(format!(
            "expected a percentage like \"80%\", found {}",
            v.type_str()
        ));
    };
    s.strip_suffix('%')
        .and_then(|n| n.parse::<u16>().ok())
        .filter(|n| (1..=100).contains(n))
        .ok_or_else(|| "expected a percentage like \"80%\", from 1% to 100%".to_string())
}

/// `500ms`, `1s`, `2m`: a positive whole number and a unit.
fn parse_duration(s: &str) -> Option<Duration> {
    let digits = s.find(|c: char| !c.is_ascii_digit())?;
    let n: u64 = s[..digits].parse().ok().filter(|n| *n > 0)?;
    match &s[digits..] {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n.checked_mul(60)?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Layer, Resolved};
    use super::*;
    use crate::config::{ConfigError, Env};
    use std::path::Path;

    /// The fake of the loader seam: one text per layer, in order.
    fn load(layers: &[(Layer, &str)]) -> Result<Resolved, Vec<ConfigError>> {
        let mut l = Loader::new(&Env::from_vars(None, None), true);
        for (i, (layer, text)) in layers.iter().enumerate() {
            l.load_text(Path::new(&format!("/c/{i}.toml")), text, *layer);
        }
        l.finish()
    }

    fn one(text: &str) -> Result<Resolved, Vec<ConfigError>> {
        load(&[(Layer::Config, text)])
    }

    #[test]
    fn defaults_are_enabled_one_second_no_kinds() {
        let r = one("").expect("loads");
        assert_eq!(r.agents, ConfigAgents::default());
        assert!(r.agents.enabled);
        assert_eq!(r.agents.scan_interval, Duration::from_secs(1));
    }

    #[test]
    fn kind_resume_only() {
        let r = one("[agents.kinds.claude]\nresume = \"claude -r {id}\"").expect("loads");
        let k = &r.agents.kinds["claude"];
        assert_eq!(k.resume.as_deref(), Some("claude -r {id}"));
        assert_eq!((&k.matches, &k.command, k.enabled), (&None, &None, None));
    }

    #[test]
    fn all_keys_parse() {
        let r = one("[agents]\nenabled = false\nscan-interval = \"500ms\"\n\
             [agents.kinds.x]\nmatch = [\"x\", \"y\"]\ncommand = \"x\"\nenabled = false")
        .expect("loads");
        assert!(!r.agents.enabled);
        assert_eq!(r.agents.scan_interval, Duration::from_millis(500));
        let k = &r.agents.kinds["x"];
        assert_eq!(k.matches, Some(vec!["x".to_string(), "y".to_string()]));
        assert_eq!(k.enabled, Some(false));
    }

    #[test]
    fn unknown_key_warns() {
        let r = one("[agents]\nfoo = 1").expect("a warning, not an error");
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("agents.foo"), "{}", r.warnings[0]);
        assert!(r.warnings[0].contains("did you mean"), "{}", r.warnings[0]);
        let r = one("[agents]\nenabld = true").expect("loads");
        assert!(r.warnings[0].contains("did you mean enabled?"));
    }

    #[test]
    fn kind_name_validated() {
        let errs = one("[agents.kinds.\"a b\"]\nresume = \"x\"").expect_err("bad name");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "agents.kinds.a b");
        assert_eq!(errs[0].line, 1);
    }

    #[test]
    fn bad_values_are_errors() {
        for text in [
            "[agents]\nenabled = 1",
            "[agents]\nscan-interval = \"fast\"",
            "[agents]\nscan-interval = \"0s\"",
            "[agents]\nscan-interval = 1",
            "[agents.kinds.x]\nmatch = \"x\"",
            "[agents.kinds.x]\nmatch = [1]",
            "[agents.kinds.x]\nresume = 3",
            "[agents.kinds]\nx = 1",
        ] {
            assert!(one(text).is_err(), "{text}");
        }
    }

    #[test]
    fn layers_merge_per_key() {
        let r = load(&[
            (
                Layer::Config,
                "[agents]\nscan-interval = \"2s\"\n[agents.kinds.claude]\n\
                 match = [\"claude\"]\nresume = \"a {id}\"",
            ),
            (
                Layer::User,
                "[agents]\nenabled = false\n[agents.kinds.claude]\nresume = \"b {id}\"",
            ),
        ])
        .expect("loads");
        assert!(!r.agents.enabled);
        assert_eq!(r.agents.scan_interval, Duration::from_secs(2));
        let k = &r.agents.kinds["claude"];
        assert_eq!(k.matches, Some(vec!["claude".to_string()]));
        assert_eq!(k.resume.as_deref(), Some("b {id}"));
        let src = r.provenance.get("agents.kinds.claude.resume").expect("src");
        assert_eq!(src.layer, Layer::User);
    }

    #[test]
    fn defaults() {
        let a = one("").expect("loads").agents;
        assert_eq!(
            (
                a.sidebar.width,
                a.sidebar.show_on_attach,
                a.sidebar.min_client_width
            ),
            (32, false, 120)
        );
        assert_eq!((a.grid.tile_width, a.grid.preview_lines), (60, 12));
        assert_eq!((a.picker.width_pct, a.picker.height_pct), (80, 70));
        assert_eq!(a.new.worktree, "never");
        assert_eq!(a.new.worktree_command[..3], ["git", "-C", "{dir}"]);
        assert_eq!(
            a.new.worktree_command.last().map(String::as_str),
            Some("{path}")
        );
    }

    #[test]
    fn tables_parse_merge_and_record_provenance() {
        let r = load(&[
            (
                Layer::Config,
                "[agents.sidebar]\nwidth = 40\nshow-on-attach = true\n\
                 [agents.picker]\nwidth = \"90%\"\n[agents.grid]\ntile-width = 50",
            ),
            (Layer::User, "[agents.sidebar]\nmin-client-width = 80\n[agents.new]\nworktree-command = [\"wt\", \"{branch}\"]"),
        ])
        .expect("loads");
        let a = &r.agents;
        assert_eq!(
            (
                a.sidebar.width,
                a.sidebar.show_on_attach,
                a.sidebar.min_client_width
            ),
            (40, true, 80)
        );
        assert_eq!((a.picker.width_pct, a.picker.height_pct), (90, 70));
        assert_eq!((a.grid.tile_width, a.grid.preview_lines), (50, 12));
        assert_eq!(a.new.worktree_command, ["wt", "{branch}"]);
        let src = r
            .provenance
            .get("agents.sidebar.min-client-width")
            .expect("src");
        assert_eq!(src.layer, Layer::User);
        assert!(r.provenance.get("agents.sidebar.width").is_some());
    }

    #[test]
    fn worktree_values() {
        for m in ["ask", "always", "never"] {
            let r = one(&format!(
                "[agents.new]\nworktree = \"{m}\"\n[agents.kinds.x]\nworktree = \"{m}\""
            ))
            .expect("loads");
            assert_eq!(r.agents.new.worktree, m);
            assert_eq!(r.agents.kinds["x"].worktree.as_deref(), Some(m));
        }
        assert!(one("[agents.new]\nworktree = \"maybe\"").is_err());
        assert!(one("[agents.kinds.x]\nworktree = \"yes\"").is_err());
        assert!(one("[agents.kinds.x]\nworktree = true").is_err());
    }

    #[test]
    fn bad_width_errors() {
        for text in [
            "[agents.sidebar]\nwidth = 0",
            "[agents.sidebar]\nwidth = 201",
            "[agents.sidebar]\nwidth = \"32\"",
            "[agents.grid]\ntile-width = 0",
            "[agents.grid]\ntile-width = 201",
            "[agents.grid]\npreview-lines = 0",
            "[agents.picker]\nwidth = \"0%\"",
            "[agents.picker]\nheight = \"101%\"",
            "[agents.picker]\nwidth = 80",
            "[agents.new]\nworktree-command = []",
            "[agents.sidebar]\nshow-on-attach = 1",
            "[agents]\nsidebar = 3",
        ] {
            assert!(one(text).is_err(), "{text}");
        }
        let r = one("[agents.sidebar]\nwidth = 200\n[agents.grid]\ntile-width = 1").expect("edges");
        assert_eq!(r.agents.sidebar.width, 200);
    }

    #[test]
    fn unknown_table_key_warns() {
        let r = one("[agents.sidebar]\nwidht = 3").expect("loads");
        assert!(
            r.warnings[0].contains("did you mean width?"),
            "{:?}",
            r.warnings
        );
    }
}
