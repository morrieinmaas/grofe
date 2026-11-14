//! `[notify]` (agents spec §9).
//!
//! Each key overrides the same key of an earlier layer. An unknown key warns
//! like any key a later step may add.

use std::time::Duration;

use toml::de::DeValue;

use super::{FileCtx, Loader, closest, entries};
use crate::server::notify::NotifyConfig;

/// The `[notify]` keys read so far, starting from the defaults.
#[derive(Default)]
pub(super) struct Merge {
    cfg: NotifyConfig,
}

impl Merge {
    pub(super) fn resolved(&self) -> NotifyConfig {
        self.cfg.clone()
    }
}

pub(crate) const KEYS: [&str; 11] = [
    "enabled",
    "on",
    "toast",
    "desktop",
    "history",
    "skip-when-seen",
    "done-min-work",
    "min-interval",
    "toast-time",
    "delivery",
    "sound",
];
const STATES: [&str; 5] = ["idle", "working", "blocked", "done", "error"];
const DELIVERY: [&str; 4] = ["auto", "terminal", "command", "off"];

impl Loader {
    /// `[notify]`.
    pub(super) fn notify(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        let DeValue::Table(t) = v else {
            let msg = format!("expected a table, found {}", v.type_str());
            return self.error(f.path, line, "notify", msg);
        };
        for (k, v) in entries(t) {
            let field = k.get_ref().as_ref();
            let at = f.line(k.span().start);
            let key = format!("notify.{field}");
            let wrong = |want: &str| format!("expected {want}, found {}", v.get_ref().type_str());
            let c = &mut self.notify.cfg;
            let result = match (field, v.get_ref()) {
                ("enabled", DeValue::Boolean(b)) => {
                    c.enabled = *b;
                    Ok(())
                }
                ("enabled", _) => Err(wrong("a boolean")),
                ("skip-when-seen", DeValue::Boolean(b)) => {
                    c.skip_when_seen = *b;
                    Ok(())
                }
                ("skip-when-seen", _) => Err(wrong("a boolean")),
                ("sound", DeValue::Boolean(b)) => {
                    c.sound = *b;
                    Ok(())
                }
                ("sound", _) => Err(wrong("a boolean")),
                ("on", DeValue::Array(a)) => states(a).map(|s| c.on = s),
                ("toast", DeValue::Array(a)) => states(a).map(|s| c.toast = s),
                ("desktop", DeValue::Array(a)) => states(a).map(|s| c.desktop = s),
                ("on" | "toast" | "desktop", _) => Err(wrong("an array of state names")),
                ("history", DeValue::Integer(n)) => usize::from_str_radix(n.as_str(), n.radix())
                    .map(|n| c.history = n)
                    .ok()
                    .ok_or_else(|| "expected a count of zero or more".to_string()),
                ("history", _) => Err(wrong("an integer")),
                ("done-min-work", DeValue::String(s)) => duration(s).map(|d| c.done_min_work = d),
                ("min-interval", DeValue::String(s)) => duration(s).map(|d| c.min_interval = d),
                ("toast-time", DeValue::String(s)) => duration(s).map(|d| c.toast_time = d),
                ("done-min-work" | "min-interval" | "toast-time", _) => {
                    Err(wrong("a duration string"))
                }
                ("delivery", DeValue::String(s)) if DELIVERY.contains(&s.as_ref()) => {
                    c.delivery = s.to_string();
                    Ok(())
                }
                ("delivery", DeValue::String(s)) => Err(format!(
                    "unknown delivery {s:?}, expected one of {}",
                    DELIVERY.join(", ")
                )),
                ("delivery", _) => Err(wrong("a string")),
                ("command", DeValue::Array(a)) => argv(a).map(|w| c.command = Some(w)),
                ("command", _) => Err(wrong("an array of strings")),
                (other, _) => {
                    self.notify_unknown_key(f, at, &key, other);
                    continue;
                }
            };
            match result {
                Ok(()) => self.provenance.set(&key, f.source(at)),
                Err(msg) => self.error(f.path, at, &key, msg),
            }
        }
    }

    fn notify_unknown_key(&mut self, f: &FileCtx, line: usize, key: &str, word: &str) {
        let known = KEYS.iter().copied().chain(["command"]);
        let hint = match closest(word, known) {
            Some(s) => format!("did you mean {s}?"),
            None => format!("did you mean one of {}, command?", KEYS.join(", ")),
        };
        self.warnings.push(format!(
            "{}:{line}: {key}: unknown key, {hint}",
            f.path.display()
        ));
    }
}

/// `on`, `toast`, `desktop`: known state names.
fn states(a: &[toml::Spanned<DeValue>]) -> Result<Vec<String>, String> {
    a.iter()
        .map(|item| match item.get_ref() {
            DeValue::String(s) if STATES.contains(&s.as_ref()) => Ok(s.to_string()),
            DeValue::String(s) => Err(format!(
                "unknown state {s:?}, expected one of {}",
                STATES.join(", ")
            )),
            other => Err(format!(
                "expected an array of strings, found {} in it",
                other.type_str()
            )),
        })
        .collect()
}

/// `command`: a non-empty argv, no shell string.
fn argv(a: &[toml::Spanned<DeValue>]) -> Result<Vec<String>, String> {
    let words: Vec<String> = a
        .iter()
        .map(|item| match item.get_ref() {
            DeValue::String(s) => Ok(s.to_string()),
            other => Err(format!(
                "expected an array of strings, found {} in it",
                other.type_str()
            )),
        })
        .collect::<Result<_, _>>()?;
    if words.first().is_none_or(|w| w.is_empty()) {
        return Err("a command needs a program as its first word".to_string());
    }
    Ok(words)
}

/// `500ms`, `30s`, `2m`: a whole number and a unit; zero is allowed.
fn duration(s: &str) -> Result<Duration, String> {
    let bad = || "expected a duration like 500ms, 30s or 2m".to_string();
    let digits = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let n: u64 = s[..digits].parse().map_err(|_| bad())?;
    match &s[digits..] {
        "ms" => Ok(Duration::from_millis(n)),
        "s" => Ok(Duration::from_secs(n)),
        "m" => n.checked_mul(60).map(Duration::from_secs).ok_or_else(bad),
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Layer, Resolved};
    use super::*;
    use crate::config::{ConfigError, Env};
    use std::path::Path;

    fn one(text: &str) -> Result<Resolved, Vec<ConfigError>> {
        let mut l = Loader::new(&Env::from_vars(None, None), true);
        l.load_text(Path::new("/c/0.toml"), text, Layer::Config);
        l.finish()
    }

    #[test]
    fn unknown_key_warns() {
        let r = one("[notify]\nfoo = 1").expect("a warning, not an error");
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("notify.foo"), "{}", r.warnings[0]);
        let r = one("[notify]\ntoast-tim = \"1s\"").expect("loads");
        assert!(r.warnings[0].contains("did you mean toast-time?"));
    }

    #[test]
    fn bad_state_name_errors() {
        let errs = one("[notify]\non = [\"bogus\"]").expect_err("bad state");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "notify.on");
        assert_eq!(errs[0].line, 2);
        assert!(one("[notify]\ntoast = [3]").is_err());
        assert!(one("[notify]\ndesktop = \"blocked\"").is_err());
    }

    #[test]
    fn delivery_values() {
        for ok in ["auto", "terminal", "command", "off"] {
            let r = one(&format!("[notify]\ndelivery = \"{ok}\"")).expect("loads");
            assert_eq!(r.notify.delivery, ok);
        }
        assert!(one("[notify]\ndelivery = \"smoke\"").is_err());
        assert!(one("[notify]\ndelivery = 1").is_err());
    }

    #[test]
    fn command_must_be_array() {
        let r = one("[notify]\ncommand = [\"n\", \"--\", \"{message}\"]").expect("loads");
        let want: Vec<String> = ["n", "--", "{message}"].map(String::from).into();
        assert_eq!(r.notify.command, Some(want));
        assert!(one("[notify]\ncommand = \"n {message}\"").is_err());
        assert!(one("[notify]\ncommand = []").is_err());
        assert!(one("[notify]\ncommand = [1]").is_err());
    }

    #[test]
    fn empty_toast_keeps_other_defaults() {
        let r = one("[notify]\ntoast = []").expect("loads");
        assert!(r.notify.toast.is_empty());
        let d = NotifyConfig::default();
        assert_eq!(r.notify, NotifyConfig { toast: vec![], ..d });
    }

    #[test]
    fn all_keys_parse_with_provenance() {
        let r = one(
            "[notify]\nenabled = false\non = [\"done\"]\ndesktop = [\"error\"]\n\
             history = 3\nskip-when-seen = false\ndone-min-work = \"2m\"\n\
             min-interval = \"0s\"\ntoast-time = \"500ms\"\nsound = true",
        )
        .expect("loads");
        let n = &r.notify;
        assert!(!n.enabled && !n.skip_when_seen && n.sound);
        assert_eq!((n.history, n.on.clone()), (3, vec!["done".to_string()]));
        assert_eq!(n.done_min_work, Duration::from_secs(120));
        assert_eq!(n.min_interval, Duration::ZERO);
        assert_eq!(n.toast_time, Duration::from_millis(500));
        assert!(r.provenance.get("notify.history").is_some());
        assert!(one("[notify]\nhistory = -1").is_err());
        assert!(one("[notify]\ntoast-time = \"soon\"").is_err());
    }
}
