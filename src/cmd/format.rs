//! `#{...}` format expansion (spec §4.5). M1 supports a bare variable
//! reference, `#{name}`, and a ternary, `#{?cond,then,else}`; nesting,
//! escaping, `#{==:a,b}` and the `#{b:var}` modifier are M2.
//!
//! Format-bearing flag values (`-c`, `-F`, `display-message` text) stay
//! raw strings on the parsed `Command`; dispatch calls [`expand`] against
//! the target's context at execution time, not at parse time.

/// Supplies named values to [`expand`]. Implemented by whatever holds the
/// pane/window/session/client state a format string reaches for
/// (`server/model.rs`, from T1.5c onward).
pub trait FormatVars {
    fn get(&self, name: &str) -> Option<String>;
}

/// Expands every `#{...}` in `fmt` against `vars`.
///
/// An unknown variable expands to an empty string, and a malformed
/// `#{...}` (no closing brace, or a `?` ternary missing one of its two
/// commas) is passed through unchanged, rather than failing the whole
/// format: tmux's own formats are equally forgiving, and a status line
/// built from several small `#{...}` pieces shouldn't go blank because
/// one of them is wrong.
pub fn expand(fmt: &str, vars: &dyn FormatVars) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i..].starts_with("#{")
            && let Some(end) = fmt[i + 2..].find('}')
        {
            let body = &fmt[i + 2..i + 2 + end];
            out.push_str(&expand_body(body, vars));
            i += 2 + end + 1;
            continue;
        }
        let ch = fmt[i..]
            .chars()
            .next()
            .expect("i is a char boundary within fmt");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Expands the body of one `#{...}`: either `?cond,then,else` or a bare
/// variable name.
fn expand_body(body: &str, vars: &dyn FormatVars) -> String {
    let Some(cond_rest) = body.strip_prefix('?') else {
        return vars.get(body).unwrap_or_default();
    };
    let Some(first_comma) = cond_rest.find(',') else {
        return format!("#{{{body}}}");
    };
    let cond = &cond_rest[..first_comma];
    let after_cond = &cond_rest[first_comma + 1..];
    let Some(second_comma) = after_cond.find(',') else {
        return format!("#{{{body}}}");
    };
    let then_branch = &after_cond[..second_comma];
    let else_branch = &after_cond[second_comma + 1..];
    if is_truthy(vars.get(cond)) {
        then_branch.to_string()
    } else {
        else_branch.to_string()
    }
}

/// tmux's truthiness for a format value: present, non-empty and not `"0"`.
fn is_truthy(value: Option<String>) -> bool {
    match value {
        None => false,
        Some(v) => !v.is_empty() && v != "0",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct MapVars(BTreeMap<&'static str, &'static str>);

    impl FormatVars for MapVars {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|v| v.to_string())
        }
    }

    fn vars(pairs: &[(&'static str, &'static str)]) -> MapVars {
        MapVars(pairs.iter().copied().collect())
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(expand("hello world", &vars(&[])), "hello world");
    }

    #[test]
    fn bare_variable_expands() {
        assert_eq!(
            expand("#{session_name}", &vars(&[("session_name", "work")])),
            "work"
        );
    }

    #[test]
    fn unknown_variable_expands_to_empty() {
        assert_eq!(expand("[#{nope}]", &vars(&[])), "[]");
    }

    #[test]
    fn user_option_variable() {
        assert_eq!(
            expand("#{@agent_rollup}", &vars(&[("@agent_rollup", "3")])),
            "3"
        );
    }

    #[test]
    fn variable_embedded_in_text() {
        assert_eq!(
            expand("[#{a}-#{b}]", &vars(&[("a", "1"), ("b", "2")])),
            "[1-2]"
        );
    }

    #[test]
    fn ternary_true_branch() {
        assert_eq!(
            expand("#{?window_active,yes,no}", &vars(&[("window_active", "1")])),
            "yes"
        );
    }

    #[test]
    fn ternary_false_branch_when_zero() {
        assert_eq!(
            expand("#{?window_active,yes,no}", &vars(&[("window_active", "0")])),
            "no"
        );
    }

    #[test]
    fn ternary_false_branch_when_missing() {
        assert_eq!(expand("#{?window_active,yes,no}", &vars(&[])), "no");
    }

    #[test]
    fn ternary_false_branch_when_empty() {
        assert_eq!(
            expand("#{?window_active,yes,no}", &vars(&[("window_active", "")])),
            "no"
        );
    }

    #[test]
    fn ternary_branches_can_be_empty() {
        assert_eq!(expand("#{?x,,no}", &vars(&[("x", "1")])), "");
        assert_eq!(expand("#{?x,yes,}", &vars(&[])), "");
    }

    #[test]
    fn unterminated_brace_passes_through() {
        assert_eq!(expand("a #{b", &vars(&[])), "a #{b");
    }

    #[test]
    fn malformed_ternary_missing_comma_passes_through() {
        assert_eq!(expand("#{?onlyone}", &vars(&[])), "#{?onlyone}");
    }

    #[test]
    fn malformed_ternary_missing_second_comma_passes_through() {
        assert_eq!(expand("#{?cond,then}", &vars(&[])), "#{?cond,then}");
    }

    #[test]
    fn literal_hash_without_brace_passes_through() {
        assert_eq!(expand("50% #done", &vars(&[])), "50% #done");
    }

    #[test]
    fn attempted_nesting_passes_through_unchanged() {
        // Nesting is M2 (spec §4.5). A `#{...}` inside a `?cond,then,else`
        // body confuses the M1 scanner into finding the inner `}` as the
        // close, producing an unparseable ternary body that gets
        // reconstructed and passed through -- so the net effect is the
        // whole thing is left alone rather than mis-expanded.
        let input = "#{?#{@agent_rollup},present,absent}";
        assert_eq!(expand(input, &vars(&[])), input);
    }

    #[test]
    fn multibyte_text_around_a_variable() {
        assert_eq!(
            expand("café #{x} → done", &vars(&[("x", "1")])),
            "café 1 → done"
        );
    }
}
