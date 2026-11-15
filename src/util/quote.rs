//! Quoting for each dialect grofe writes: a POSIX shell word, text inside a
//! shell's double quotes, a tmux command argument and a TOML string.

/// One word for a POSIX shell: in single quotes, a `'` as `'\''`, so
/// backslashes, `$` and `~` stay as they are.
pub fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A value for inside double quotes in `sh`: `$`, `` ` ``, `"` and `\` get
/// a backslash, as tmux's `cmd_show_environment_escape`.
pub fn sh_double(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '$' | '`' | '"' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// One argument as tmux 3.7b's `args_escape` prints it (`list-keys`,
/// `show-options`): `''` when empty; double quotes around a word holding
/// any of ` #';${}%`, else single quotes around one holding a space or `"`;
/// a lone such character (or `~`) as a backslash and itself; inside, `\\`,
/// `\"` (in double quotes), `\$` before a name, C escapes (`\n`, `\t`, ...)
/// and octal for other control characters; a leading `~` escaped.
pub fn tmux_arg(s: &str) -> String {
    const DQUOTED: &str = " #';${}%";
    const SQUOTED: &str = " \"";
    if s.is_empty() {
        return "''".to_string();
    }
    let quote = if s.contains(|c| DQUOTED.contains(c)) {
        Some('"')
    } else if s.contains(|c| SQUOTED.contains(c)) {
        Some('\'')
    } else {
        None
    };
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c != ' '
        && (quote.is_some() || c == '~')
    {
        return format!("\\{c}");
    }
    let bytes = s.as_bytes();
    let mut body = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        match c {
            '\\' => body.push_str("\\\\"),
            '\n' => body.push_str("\\n"),
            '\t' => body.push_str("\\t"),
            '\r' => body.push_str("\\r"),
            '\x07' => body.push_str("\\a"),
            '\x08' => body.push_str("\\b"),
            '\x0b' => body.push_str("\\v"),
            '\x0c' => body.push_str("\\f"),
            '\0' => {
                body.push_str("\\0");
                // `\0` then a digit would read as one octal escape.
                if bytes.get(i + 1).is_some_and(|n| (b'0'..=b'7').contains(n)) {
                    body.push_str("00");
                }
            }
            c if c.is_ascii_control() => body.push_str(&format!("\\{:03o}", c as u32)),
            '"' if quote == Some('"') => body.push_str("\\\""),
            // `$` only where a shell would start a variable after it.
            '$' if quote == Some('"')
                && bytes.get(i + 1).is_some_and(|&n| {
                    n == b'_' || n == b'{' || n.is_ascii_alphabetic() || n >= 0x80
                }) =>
            {
                body.push_str("\\$")
            }
            c => body.push(c),
        }
    }
    let tilde = if body.starts_with('~') { "\\" } else { "" };
    match quote {
        Some('\'') => format!("'{body}'"),
        Some(q) => format!("{q}{tilde}{body}{q}"),
        None => format!("{tilde}{body}"),
    }
}

/// A TOML basic string.
pub fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// A TOML key: bare when it is only letters, digits, `-` and `_`, else a
/// quoted string.
pub fn toml_key(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare { s.to_string() } else { toml_str(s) }
}

/// A key binding as a config value: the bare command string when it has
/// nothing else, else an inline table with `cmd`, each of `extras` that is
/// set, and `repeat`.
pub fn toml_binding(command: &str, extras: &[(&str, Option<&str>)], repeat: bool) -> String {
    if repeat || extras.iter().any(|(_, v)| v.is_some()) {
        let mut fields = vec![format!("cmd = {}", toml_str(command))];
        for (k, v) in extras {
            if let Some(v) = v {
                fields.push(format!("{k} = {}", toml_str(v)));
            }
        }
        if repeat {
            fields.push("repeat = true".to_string());
        }
        format!("{{ {} }}", fields.join(", "))
    } else {
        toml_str(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_quotes_a_word_for_a_posix_shell() {
        for (word, want) in [
            ("", "''"),
            ("a", "'a'"),
            ("a b", "'a b'"),
            ("it's", r"'it'\''s'"),
            ("/it's/grofe", r"'/it'\''s/grofe'"),
            ("a\\b$c~", "'a\\b$c~'"),
            ("#{x}", "'#{x}'"),
        ] {
            assert_eq!(sh(word), want, "{word:?}");
        }
    }

    #[test]
    fn sh_double_escapes_what_double_quotes_interpret() {
        for (value, want) in [
            ("", ""),
            ("plain", "plain"),
            ("a$b", "a\\$b"),
            ("`x`", "\\`x\\`"),
            ("say \"hi\"", "say \\\"hi\\\""),
            ("a\\b", "a\\\\b"),
            ("it's", "it's"),
        ] {
            assert_eq!(sh_double(value), want, "{value:?}");
        }
    }

    /// Each case recorded from tmux 3.7b (`show-options`, `list-keys`), the
    /// union of what the three former copies each pinned.
    #[test]
    fn tmux_arg_is_args_escape() {
        for (arg, want) in [
            ("", "''"),
            ("a", "a"),
            ("kill-pane", "kill-pane"),
            ("a b", "\"a b\""),
            ("#S", "\"#S\""),
            ("%%", "\"%%\""),
            ("a#b", "\"a#b\""),
            ("a{b", "\"a{b\""),
            ("#{x}", "\"#{x}\""),
            ("a}b", "\"a}b\""),
            ("ab", "ab"),
            ("a\"b", "'a\"b'"),
            ("a'b", "\"a'b\""),
            ("a$b", "\"a\\$b\""),
            ("a;b", "\"a;b\""),
            ("a\\b", "a\\\\b"),
            ("~a", "\\~a"),
            ("a~", "a~"),
            ("#", "\\#"),
            ("~", "\\~"),
            ("$", "\\$"),
            ("\"", "\\\""),
            (";", r"\;"),
            (" ", "\" \""),
            ("~/x", "\\~/x"),
            ("~a b", "\"\\~a b\""),
            ("a \"b", "\"a \\\"b\""),
            ("a `b", "\"a `b\""),
            ("a\tb", "a\\tb"),
            ("a\nb", "a\\nb"),
            ("a\u{1}b", "a\\001b"),
            ("a\u{7}b", "a\\ab"),
            ("a \\b", "\"a \\\\b\""),
            ("a,b", "a,b"),
            ("[a]", "[a]"),
            ("a%b", "\"a%b\""),
            ("say \"hi\" #x", "\"say \\\"hi\\\" #x\""),
            ("$HOME x", "\"\\$HOME x\""),
            ("$1 x", "\"$1 x\""),
            ("$\u{e9} x", "\"\\$\u{e9} x\""),
            (":=0", ":=0"),
            ("a\0b", "a\\0b"),
            ("a\x001", "a\\0001"),
        ] {
            assert_eq!(tmux_arg(arg), want, "{arg:?}");
        }
    }

    #[test]
    fn toml_strings_and_keys() {
        assert_eq!(toml_str("a\"b\\c"), "'a\"b\\c'");
        assert_eq!(toml_str(""), "\"\"");
        for (key, want) in [
            ("prefix", "prefix"),
            ("a-b_c1", "a-b_c1"),
            ("", "\"\""),
            ("|", "\"|\""),
            ("(", "\"(\""),
            ("a.b", "\"a.b\""),
        ] {
            assert_eq!(toml_key(key), want, "{key:?}");
        }
    }

    #[test]
    fn toml_binding_is_a_string_until_it_has_more_to_say() {
        assert_eq!(toml_binding("kill-pane", &[], false), "\"kill-pane\"");
        assert_eq!(
            toml_binding("kill-pane", &[("desc", None), ("name", None)], false),
            "\"kill-pane\""
        );
        assert_eq!(
            toml_binding(
                "kill-pane",
                &[
                    ("desc", Some("Close")),
                    ("group", None),
                    ("name", Some("k"))
                ],
                true
            ),
            "{ cmd = \"kill-pane\", desc = \"Close\", name = \"k\", repeat = true }"
        );
        assert_eq!(
            toml_binding("x", &[], true),
            "{ cmd = \"x\", repeat = true }"
        );
    }
}
