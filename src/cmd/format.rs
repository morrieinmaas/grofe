//! `#{...}` format expansion (spec §4.5). M1 supports a bare variable
//! reference, `#{name}`, and a ternary, `#{?cond,then,else}`. M2 adds
//! nesting, `##` escaping, `#{==:a,b}` and the `#{b:var}` modifier.
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

/// Expands every `#{...}` in `fmt` against `vars`, and `##` to a literal
/// `#`.
///
/// An unknown variable expands to an empty string, and a malformed
/// `#{...}` (no closing brace, or a `?`/`==:` form missing one of its
/// arguments) is passed through unchanged, rather than failing the whole
/// format: tmux's own formats are equally forgiving, and a status line
/// built from several small `#{...}` pieces shouldn't go blank because
/// one of them is wrong. `#{...}` may nest: the closing `}` is found by
/// tracking nested `#{`/`}` pairs, not by the first `}` seen.
pub fn expand(fmt: &str, vars: &dyn FormatVars) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i..].starts_with("##") {
            out.push('#');
            i += 2;
            continue;
        }
        if fmt[i..].starts_with("#{")
            && let Some(end) = find_unnested(&fmt[i + 2..], b'}')
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

/// Finds the first `target` byte in `s` that isn't inside a nested
/// `#{...}`, so callers can split a body on its own top-level `}` or `,`
/// without being confused by commas or braces belonging to a nested
/// format. `s` is assumed to be otherwise balanced (it is either the
/// tail after an opening `#{`, or an already-extracted body).
fn find_unnested(s: &str, target: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        if depth == 0 && bytes[i] == target {
            return Some(i);
        }
        if s[i..].starts_with("#{") {
            depth += 1;
            i += 2;
            continue;
        }
        if bytes[i] == b'}' && depth > 0 {
            depth -= 1;
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().expect("i is a char boundary");
        i += ch.len_utf8();
    }
    None
}

/// Expands the body of one `#{...}`: `?cond,then,else`, `==:a,b`, `b:var`,
/// or a bare variable name.
fn expand_body(body: &str, vars: &dyn FormatVars) -> String {
    if let Some(rest) = body.strip_prefix('?') {
        return expand_ternary(rest, body, vars);
    }
    if let Some(rest) = body.strip_prefix("==:") {
        return expand_eq(rest, body, vars);
    }
    if let Some(rest) = body.strip_prefix("b:") {
        return basename(&resolve(rest, vars));
    }
    resolve(body, vars)
}

/// Resolves one modifier argument or ternary condition: a bare token
/// (no nested `#{...}`) is looked up directly as a variable name, exactly
/// as if it had been written `#{token}`; a token that itself contains
/// `#{...}` is expanded first and its result used as-is. This is why
/// `#{b:pane_current_path}` looks up `pane_current_path`, while
/// `#{b:/a/b/c}` looks up the (nonexistent) variable named `/a/b/c` and
/// gets nothing, matching tmux 3.7b.
fn resolve(s: &str, vars: &dyn FormatVars) -> String {
    if s.contains("#{") {
        expand(s, vars)
    } else {
        vars.get(s).unwrap_or_default()
    }
}

/// tmux's basename modifier: the last `/`-separated component, with any
/// trailing slashes stripped first. Basename of an empty string is empty,
/// not `.` as POSIX `basename(1)` would give.
fn basename(s: &str) -> String {
    s.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

fn expand_ternary(rest: &str, original_body: &str, vars: &dyn FormatVars) -> String {
    let malformed = || format!("#{{{original_body}}}");
    let Some(c1) = find_unnested(rest, b',') else {
        return malformed();
    };
    let cond = &rest[..c1];
    let after = &rest[c1 + 1..];
    let Some(c2) = find_unnested(after, b',') else {
        return malformed();
    };
    let then_branch = &after[..c2];
    let else_branch = &after[c2 + 1..];
    if is_truthy(&resolve(cond, vars)) {
        expand(then_branch, vars)
    } else {
        expand(else_branch, vars)
    }
}

/// `#{==:a,b}`: both sides are literal text that may itself contain
/// nested `#{...}` (mixed-text, like a `then`/`else` branch), not a bare
/// variable lookup -- so `#{==:#{window_name},bash}` compares the
/// window's name against the literal word `bash`.
fn expand_eq(rest: &str, original_body: &str, vars: &dyn FormatVars) -> String {
    let Some(c) = find_unnested(rest, b',') else {
        return format!("#{{{original_body}}}");
    };
    let a = expand(&rest[..c], vars);
    let b = expand(&rest[c + 1..], vars);
    if a == b { "1" } else { "0" }.to_string()
}

/// tmux's truthiness for a format value: present, non-empty and not `"0"`.
fn is_truthy(value: &str) -> bool {
    !value.is_empty() && value != "0"
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
    fn multibyte_text_around_a_variable() {
        assert_eq!(
            expand("café #{x} → done", &vars(&[("x", "1")])),
            "café 1 → done"
        );
    }

    // --- M2: nesting, escaping, ==:, b: ---

    #[test]
    fn nested_ternary_condition_expands() {
        // A nested `#{...}` as the ternary's condition (spec §4.5, M2
        // nesting). The outer scanner must find the true closing `}`
        // past the nested one, and the condition itself is expanded
        // (rather than looked up literally as `#{@agent_rollup}`).
        let input = "#{?#{@agent_rollup},present,absent}";
        assert_eq!(expand(input, &vars(&[("@agent_rollup", "1")])), "present");
        assert_eq!(expand(input, &vars(&[])), "absent");
    }

    #[test]
    fn nested_variable_in_then_branch() {
        assert_eq!(
            expand("#{?x,[#{y}],no}", &vars(&[("x", "1"), ("y", "z")])),
            "[z]"
        );
    }

    #[test]
    fn escaped_hash_hash_is_literal_hash() {
        assert_eq!(expand("##", &vars(&[])), "#");
    }

    #[test]
    fn escaping_prevents_format_expansion() {
        assert_eq!(
            expand("##{not a format}", &vars(&[("not a format", "x")])),
            "#{not a format}"
        );
    }

    #[test]
    fn equality_true_and_false() {
        assert_eq!(expand("#{==:a,a}", &vars(&[])), "1");
        assert_eq!(expand("#{==:a,b}", &vars(&[])), "0");
    }

    #[test]
    fn equality_compares_expanded_nested_values_against_a_literal() {
        // `a`/`b` here are mixed text, not bare variable lookups: a plain
        // word is literal, and a nested `#{...}` is expanded first.
        assert_eq!(
            expand(
                "#{==:#{window_name},bash}",
                &vars(&[("window_name", "bash")])
            ),
            "1"
        );
        assert_eq!(
            expand(
                "#{==:#{window_name},bash}",
                &vars(&[("window_name", "zsh")])
            ),
            "0"
        );
    }

    #[test]
    fn equality_missing_comma_passes_through() {
        assert_eq!(expand("#{==:onlyone}", &vars(&[])), "#{==:onlyone}");
    }

    #[test]
    fn basename_modifier_looks_up_the_named_variable() {
        assert_eq!(
            expand(
                "#{b:pane_current_path}",
                &vars(&[("pane_current_path", "/a/b/c")])
            ),
            "c"
        );
    }

    #[test]
    fn basename_modifier_of_a_literal_path_is_empty() {
        // Matches tmux 3.7b: `/a/b/c` here is a variable *name* to look
        // up, not literal text, and no such variable exists.
        assert_eq!(expand("#{b:/a/b/c}", &vars(&[])), "");
    }

    #[test]
    fn basename_modifier_on_a_nested_format() {
        assert_eq!(
            expand(
                "#{b:#{pane_current_path}}",
                &vars(&[("pane_current_path", "/a/b/c")])
            ),
            "c"
        );
    }
}
