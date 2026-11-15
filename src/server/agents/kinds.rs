//! Agent kind names, built-in kinds, argv matching and resume argv.

/// One agent kind: how to recognise it and how to resume it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    pub name: String,
    pub matches: Vec<String>,
    pub command: Option<String>,
    pub resume: Option<String>,
    /// `ask`, `always` or `never`, over `[agents.new] worktree`.
    pub worktree: Option<String>,
}

const LAUNCHERS: &[&str] = &[
    "node", "bun", "deno", "python", "python3", "uv", "uvx", "npx", "bunx", "pnpm",
];
const SUBCOMMANDS: &[&str] = &["run", "x", "dlx", "exec"];
const SCRIPT_EXTS: &[&str] = &[".js", ".mjs", ".cjs", ".ts", ".py"];

fn kind(name: &str, resume: Option<&str>) -> Kind {
    Kind {
        name: name.into(),
        matches: vec![name.into()],
        command: None,
        resume: resume.map(Into::into),
        worktree: None,
    }
}

/// The built-in kinds (spec §8).
pub fn builtin() -> Vec<Kind> {
    vec![
        kind("claude", Some("claude --resume {id}")),
        kind("codex", Some("codex resume {id}")),
        kind("opencode", Some("opencode --session {id}")),
        kind("pi", None),
        kind("gemini", None),
        kind("aider", None),
    ]
}

fn basename(arg: &str) -> &str {
    let base = arg.rsplit('/').next().unwrap_or(arg);
    for ext in SCRIPT_EXTS {
        if let Some(stem) = base.strip_suffix(ext)
            && !stem.is_empty()
        {
            return stem;
        }
    }
    base
}

/// The program name an argv runs: `argv[0]`'s basename, or for a launcher
/// the first non-option, non-subcommand argument's basename.
fn program(argv: &[String]) -> Option<&str> {
    let first = basename(argv.first()?);
    if !LAUNCHERS.contains(&first) {
        return Some(first);
    }
    argv[1..]
        .iter()
        .find(|a| !a.starts_with('-') && !SUBCOMMANDS.contains(&a.as_str()))
        .map(|a| basename(a))
}

/// The name of the first kind whose `matches` contains the program of `argv`.
pub fn match_argv<'a>(kinds: &'a [Kind], argv: &[String]) -> Option<&'a str> {
    let prog = program(argv)?;
    kinds
        .iter()
        .find(|k| k.matches.iter().any(|m| m == prog))
        .map(|k| k.name.as_str())
}

/// The resume argv: `template` split on spaces with `{id}` replaced. `None`
/// for an empty template or an id outside `[A-Za-z0-9._:-]{1,128}`.
pub fn resume_argv(template: &str, id: &str) -> Option<Vec<String>> {
    let ok = (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    if !ok {
        return None;
    }
    let argv: Vec<String> = template
        .split(' ')
        .filter(|t| !t.is_empty())
        .map(|t| t.replace("{id}", id))
        .collect();
    (!argv.is_empty()).then_some(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn m(a: &[&str]) -> Option<String> {
        match_argv(&builtin(), &argv(a)).map(String::from)
    }

    #[test]
    fn launcher_skips_options() {
        assert_eq!(m(&["npx", "-y", "opencode"]).as_deref(), Some("opencode"));
    }

    #[test]
    fn pnpm_dlx() {
        assert_eq!(m(&["pnpm", "dlx", "gemini"]).as_deref(), Some("gemini"));
    }

    #[test]
    fn basename_only() {
        assert_eq!(m(&["/opt/claude-tools/bin/x"]), None);
    }

    #[test]
    fn resume_rejects_bad_id() {
        assert_eq!(resume_argv("claude --resume {id}", "x;rm"), None);
        assert_eq!(resume_argv("claude --resume {id}", "a b"), None);
        assert_eq!(resume_argv("claude --resume {id}", ""), None);
        assert_eq!(resume_argv("claude --resume {id}", &"a".repeat(129)), None);
    }

    #[test]
    fn examples() {
        assert_eq!(
            m(&["/usr/local/bin/claude", "--resume"]).as_deref(),
            Some("claude")
        );
        assert_eq!(m(&["uv", "run", "aider"]).as_deref(), Some("aider"));
        assert_eq!(m(&["node", "/x/codex.js"]).as_deref(), Some("codex"));
        assert_eq!(m(&["vim"]), None);
        assert_eq!(m(&[]), None);
        assert_eq!(m(&["node"]), None);
    }

    #[test]
    fn resume_argv_replaces_id() {
        assert_eq!(
            resume_argv("claude --resume {id}", "abc-1"),
            Some(argv(&["claude", "--resume", "abc-1"]))
        );
        assert_eq!(resume_argv("", "abc"), None);
    }

    #[test]
    fn builtin_resume_templates() {
        let b = builtin();
        let r = |n: &str| {
            b.iter()
                .find(|k| k.name == n)
                .and_then(|k| k.resume.clone())
        };
        assert_eq!(r("claude").as_deref(), Some("claude --resume {id}"));
        assert_eq!(r("codex").as_deref(), Some("codex resume {id}"));
        assert_eq!(r("opencode").as_deref(), Some("opencode --session {id}"));
        assert_eq!(r("pi"), None);
        assert_eq!(r("aider"), None);
    }
}
