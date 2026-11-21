//! The generated options reference for the book.

use super::*;

const REFERENCE_INTRO: &str = "# Options

<!-- Generated from the option table in src/server/dispatch/options/table.rs and lists.rs.
     Do not edit by hand: run `just docs` to regenerate. -->

Every built-in option, by its tmux name. Set one in `config.toml` by its
TOML key (a dotted key such as `status.right` is the `right` key in the
`[status]` table), or at runtime by its tmux name:

```sh
grofe set-option -g status-interval 5
grofe show-options -g status-interval
```

The scope is the one `set-option` and `show-options` use when no scope flag
is given. Under tmux identity (grofe run as `tmux`, or
`compat-identity = \"tmux\"`) some defaults are tmux's instead; those are
listed with each option. Options marked as having no effect are accepted so
that tmux configs and plugins load, but grofe never acts on them.
";

/// The book's options reference (`docs/book/src/reference/options.md`),
/// generated from [`BUILTINS`] and [`LISTS`] so it cannot drift:
/// `tests/docs_reference.rs` compares it with the committed file.
pub fn reference_markdown() -> String {
    fn code(s: &str) -> String {
        match s {
            "" => "empty".to_string(),
            s if s.contains('`') => format!("`` {s} ``"),
            s => format!("`{s}`"),
        }
    }
    fn block(label: &str, lines: &[String]) -> String {
        match lines {
            [] => format!("{label}: empty\n\n"),
            [one] if one.chars().count() <= 60 => format!("{label}: {}\n\n", code(one)),
            _ => format!("{label}:\n\n```text\n{}\n```\n\n", lines.join("\n")),
        }
    }
    fn values(kind: Kind) -> String {
        match kind {
            Kind::Bool => "`on` or `off` (`true` or `false` in TOML)".to_string(),
            Kind::Number { max } => format!("a whole number, at most {max}"),
            Kind::String => "text".to_string(),
            Kind::Choice(words) => words.iter().map(|w| code(w)).collect::<Vec<_>>().join(", "),
            Kind::Key => "a key name, or `None`".to_string(),
            Kind::Colour => {
                "a colour: `default`, `terminal`, a name such as `red`, `colourN` or `#rrggbb`"
                    .to_string()
            }
            Kind::Size => "`<columns>x<rows>`".to_string(),
        }
    }
    let notes = |name: &str| {
        let mut out = String::new();
        if is_grofe_only(name) {
            out.push_str("grofe only: tmux does not have this option.\n\n");
        }
        if has_no_effect(name) {
            out.push_str("Accepted for compatibility; it has no effect in grofe.\n\n");
        }
        out
    };
    let mut entries: Vec<(&str, String)> = Vec::new();
    for b in BUILTINS {
        let key = crate::config::toml_key(b.name).unwrap_or("(none)");
        let ty = crate::config::toml_type_name(key);
        let mut text = format!(
            "## {}\n\n{}\n\n- **TOML key**: `{key}` ({ty})\n- **Scope**: {}\n- **Values**: {}\n\n",
            b.name,
            b.doc,
            scope_word(b.scope),
            values(b.kind),
        );
        text.push_str(&block("Default", &[b.default.to_string()]));
        let tmux = match b.name {
            "mode-keys" | "status-keys" => {
                Some("`emacs`, or `vi` when `$VISUAL` or `$EDITOR` names a vi".to_string())
            }
            _ => b.tmux_default.map(str::to_string),
        };
        match tmux {
            Some(v) if v.starts_with('`') => {
                text.push_str(&format!("Default under tmux identity: {v}\n\n"))
            }
            Some(v) => text.push_str(&block("Default under tmux identity", &[v])),
            None => {}
        }
        text.push_str(&notes(b.name));
        entries.push((b.name, text));
    }
    for l in LISTS {
        let key = crate::config::LIST_NAMES
            .iter()
            .find(|(_, n)| *n == l.name)
            .map_or("(none)", |(t, _)| *t);
        let indexed = |items: &[&str]| -> Vec<String> {
            items
                .iter()
                .enumerate()
                .map(|(i, v)| format!("[{i}] {v}"))
                .collect()
        };
        let mut text = format!(
            "## {}\n\n{}\n\n- **TOML key**: `{key}` (array of strings)\n- **Scope**: {}\n- **Values**: a list of text, element N at index N\n\n",
            l.name,
            l.doc,
            scope_word(l.scope),
        );
        text.push_str(&block("Default", &indexed(l.default)));
        if l.name == "command-alias" {
            text.push_str(&block(
                "Default under tmux identity",
                &indexed(TMUX_COMMAND_ALIASES),
            ));
        }
        text.push_str(&notes(l.name));
        entries.push((l.name, text));
    }
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = format!("{REFERENCE_INTRO}\n");
    for (_, text) in entries {
        out.push_str(&text);
    }
    format!("{}\n", out.trim_end())
}

fn scope_word(scope: OptionScope) -> &'static str {
    match scope {
        OptionScope::Global | OptionScope::Server => "global",
        OptionScope::Session => "session",
        OptionScope::Window | OptionScope::GlobalWindow => "window",
        OptionScope::Pane => "pane",
    }
}

/// A built-in option as the settings menu lists it.
pub(crate) struct OptionDoc {
    pub name: &'static str,
    pub toml: &'static str,
    pub doc: &'static str,
    pub default: &'static str,
    pub kind: Kind,
}

/// Every built-in option, in table order.
pub(crate) fn option_docs() -> impl Iterator<Item = OptionDoc> {
    BUILTINS.iter().map(|b| OptionDoc {
        name: b.name,
        toml: crate::config::toml_key(b.name).unwrap_or(b.name),
        doc: b.doc,
        default: b.default,
        kind: b.kind,
    })
}
