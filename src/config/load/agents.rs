//! `[agents]` and `[agents.kinds.<name>]` (agents spec §9).
//!
//! The tables merge key by key across layers: `user.toml` can set
//! `resume` on a kind a fragment declared. Keys of the later `[agents.*]`
//! tables are not read yet and warn like any unknown key.

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
}

/// The merged `[agents]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigAgents {
    pub enabled: bool,
    pub scan_interval: Duration,
    pub kinds: BTreeMap<String, ConfigKind>,
}

impl Default for ConfigAgents {
    fn default() -> Self {
        ConfigAgents {
            enabled: true,
            scan_interval: Duration::from_secs(1),
            kinds: BTreeMap::new(),
        }
    }
}

/// The agent tables read so far.
#[derive(Default)]
pub(super) struct Merge {
    enabled: Option<bool>,
    scan_interval: Option<Duration>,
    kinds: BTreeMap<String, ConfigKind>,
}

impl Merge {
    pub(super) fn resolved(&self) -> ConfigAgents {
        let d = ConfigAgents::default();
        ConfigAgents {
            enabled: self.enabled.unwrap_or(d.enabled),
            scan_interval: self.scan_interval.unwrap_or(d.scan_interval),
            kinds: self.kinds.clone(),
        }
    }
}

const KEYS: [&str; 3] = ["enabled", "scan-interval", "kinds"];
const KIND_KEYS: [&str; 4] = ["match", "command", "resume", "enabled"];

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
}
