//! The shared normalization list (spec §9): `tests/fixtures/compat/normalize.txt`
//! parsed into rules and applied to recorded output. The golden recorder and
//! the compat suite both go through [`Normalizer`], so the two sides can never
//! drift apart. The rule grammar is documented at the top of `normalize.txt`.

use std::collections::HashMap;
use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

/// `tests/fixtures/compat/`, where the fixtures and goldens live.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compat")
}

/// What one run knows about itself: the values the rules replace.
#[derive(Debug, Default, Clone)]
pub struct Context {
    /// `socket`, `tmpdir`, `host`, `user`: the values for `sub` and `word`.
    pub vars: HashMap<String, String>,
    /// Every pid the run could have produced (the `pid` rule). A window,
    /// because the recorder cannot know every pid tmux forks.
    pub pids: Option<RangeInclusive<u64>>,
    /// Pids known exactly (the `pid` rule, besides the window). A window can
    /// mask a wrong-pid bug (any in-window number passes as `<PID>`), so a
    /// suite that knows its pids should set `pids = None` and list them here.
    pub exact_pids: Vec<u64>,
    /// Unix seconds the run spanned, with slack (the `epoch` rule). The same
    /// masking caveat applies: any in-window time passes as `<EPOCH>`.
    pub epochs: Option<RangeInclusive<u64>>,
}

#[derive(Debug, Clone, PartialEq)]
enum Rule {
    Sub { var: String, to: String },
    Word { var: String, to: String },
    Tty { prefix: String, to: String },
    Ctime { to: String },
    Epoch { to: String },
    Pid { to: String },
}

#[derive(Debug, Clone)]
pub struct Normalizer {
    rules: Vec<Rule>,
}

impl Normalizer {
    /// Load the shared list from `normalize.txt`.
    pub fn load() -> Normalizer {
        let path = fixtures_dir().join("normalize.txt");
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        Normalizer::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Normalizer, String> {
        let mut rules = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [kind, arg, to] = fields[..] else {
                return Err(format!("line {}: want 3 fields, got {line:?}", n + 1));
            };
            let (arg, to) = (arg.to_string(), to.to_string());
            rules.push(match kind {
                "sub" => Rule::Sub { var: arg, to },
                "word" => Rule::Word { var: arg, to },
                "tty" => Rule::Tty { prefix: arg, to },
                "ctime" => Rule::Ctime { to },
                "epoch" => Rule::Epoch { to },
                "pid" => Rule::Pid { to },
                other => return Err(format!("line {}: unknown rule {other:?}", n + 1)),
            });
        }
        Ok(Normalizer { rules })
    }

    pub fn apply(&self, ctx: &Context, text: &str) -> String {
        let mut out = text.to_string();
        for rule in &self.rules {
            out = match rule {
                Rule::Sub { var, to } => match var_value(ctx, var) {
                    Some(v) => out.replace(v, to),
                    None => out,
                },
                Rule::Word { var, to } => match word_values(ctx, var) {
                    Some((v, short)) => {
                        let out = replace_words(&out, v, to);
                        match short {
                            Some(short) => replace_words(&out, short, to),
                            None => out,
                        }
                    }
                    None => out,
                },
                Rule::Tty { prefix, to } => replace_tty(&out, prefix, to),
                Rule::Ctime { to } => replace_ctime(&out, to),
                Rule::Epoch { to } => match &ctx.epochs {
                    Some(range) => replace_numbers(&out, to, |s| {
                        s.len() == 10 && s.parse().is_ok_and(|n: u64| range.contains(&n))
                    }),
                    None => out,
                },
                Rule::Pid { to } => {
                    if ctx.pids.is_none() && ctx.exact_pids.is_empty() {
                        out
                    } else {
                        replace_numbers(&out, to, |s| {
                            s.parse().is_ok_and(|n: u64| {
                                ctx.pids.as_ref().is_some_and(|r| r.contains(&n))
                                    || ctx.exact_pids.contains(&n)
                            })
                        })
                    }
                }
            };
        }
        out
    }
}

fn var_value<'a>(ctx: &'a Context, var: &str) -> Option<&'a str> {
    ctx.vars
        .get(var)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

/// The value(s) a `word` rule replaces. `host` also covers its short form
/// (tmux's `#h`, up to the first dot), so a dotted host name such as a
/// container's `localhost.localdomain` normalizes in both spellings; the
/// full name goes first. A `user` of `root` is skipped: the recorder never
/// runs as root, so `root` in output is tmux's key table, never the user.
fn word_values<'a>(ctx: &'a Context, var: &str) -> Option<(&'a str, Option<&'a str>)> {
    let v = var_value(ctx, var)?;
    match var {
        "host" => {
            let short = v.split('.').next().filter(|s| !s.is_empty() && *s != v);
            Some((v, short))
        }
        "user" if v == "root" => None,
        _ => Some((v, None)),
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Replace `word` where neither neighbour is a word character.
fn replace_words(text: &str, word: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(word) {
        let before = rest[..i].chars().next_back().or(out.chars().next_back());
        let after = rest[i + word.len()..].chars().next();
        out.push_str(&rest[..i]);
        if before.is_some_and(is_word_char) || after.is_some_and(is_word_char) {
            out.push_str(word);
        } else {
            out.push_str(to);
        }
        rest = &rest[i + word.len()..];
    }
    out.push_str(rest);
    out
}

/// Replace `prefix` followed by one or more digits.
fn replace_tty(text: &str, prefix: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(prefix) {
        let tail = &rest[i + prefix.len()..];
        let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        out.push_str(&rest[..i]);
        if digits > 0 {
            out.push_str(to);
            rest = &tail[digits..];
        } else {
            out.push_str(prefix);
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Replace every maximal run of ASCII digits for which `hit` holds.
fn replace_numbers(text: &str, to: &str, hit: impl Fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if !run.is_empty() {
            out.push_str(if hit(run) { to } else { run });
            run.clear();
        }
    };
    for c in text.chars() {
        if c.is_ascii_digit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Replace asctime-style dates as `ctime(3)` prints them (and tmux's
/// `#{t:...}` without a format): `Www Mmm dd hh:mm:ss yyyy`, the day
/// space-padded. Always 24 ASCII bytes.
fn replace_ctime(text: &str, to: &str) -> String {
    const LEN: usize = 24;
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    let mut i = 0;
    while i + LEN <= bytes.len() {
        if is_ctime(&bytes[i..i + LEN]) {
            out.push_str(&text[start..i]);
            out.push_str(to);
            i += LEN;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[start..]);
    out
}

fn is_ctime(b: &[u8]) -> bool {
    const DAYS: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
    const MONTHS: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    let digit = |i: usize| b[i].is_ascii_digit();
    DAYS.contains(&&b[0..3])
        && b[3] == b' '
        && MONTHS.contains(&&b[4..7])
        && b[7] == b' '
        && (b[8] == b' ' || digit(8))
        && digit(9)
        && b[10] == b' '
        && digit(11)
        && digit(12)
        && b[13] == b':'
        && digit(14)
        && digit(15)
        && b[16] == b':'
        && digit(17)
        && digit(18)
        && b[19] == b' '
        && (20..24).all(digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        let mut vars = HashMap::new();
        vars.insert("socket".into(), "/tmp/.tmpAB12/tmux-0/golden".into());
        vars.insert("tmpdir".into(), "/tmp/.tmpAB12".into());
        vars.insert("host".into(), "box".into());
        vars.insert("user".into(), "golden".into());
        Context {
            vars,
            pids: Some(20000..=20500),
            exact_pids: Vec::new(),
            epochs: Some(1_790_000_000..=1_790_000_100),
        }
    }

    #[test]
    fn the_shared_list_parses() {
        let n = Normalizer::load();
        assert!(n.rules.len() >= 6, "{:?}", n.rules);
    }

    #[test]
    fn unknown_rule_and_wrong_arity_are_errors() {
        assert!(Normalizer::parse("frob x <X>").is_err());
        assert!(Normalizer::parse("sub socket").is_err());
    }

    #[test]
    fn applies_every_rule_kind() {
        let n = Normalizer::load();
        let raw = "TMUX=/tmp/.tmpAB12/tmux-0/golden,20017,0\n\
                   HOME=/tmp/.tmpAB12/home\n\
                   box boxer golden goldens\n\
                   /dev/pts/3 /dev/ttys012 /dev/ptsx\n\
                   main: 1 windows (created Mon Sep  8 10:00:00 2026)\n\
                   1790000042 1790009999 20499 50000 200\n";
        let want = "TMUX=<SOCKET>,<PID>,0\n\
                    HOME=<TMPDIR>/home\n\
                    <HOST> boxer <USER> goldens\n\
                    <TTY> <TTY> /dev/ptsx\n\
                    main: 1 windows (created <TIME>)\n\
                    <EPOCH> 1790009999 <PID> 50000 200\n";
        assert_eq!(n.apply(&ctx(), raw), want);
    }

    #[test]
    fn unset_vars_and_windows_skip_their_rules() {
        let n = Normalizer::load();
        let raw = "golden 20017 1790000042";
        assert_eq!(n.apply(&Context::default(), raw), raw);
    }

    /// The recorder runs as the user `golden`, never root: as root the user
    /// rule would rewrite tmux's `root` key table.
    #[test]
    fn the_root_key_table_survives() {
        let n = Normalizer::load();
        let raw = "bind-key -T root MouseDown1Pane select-pane -t =\nroot\n";
        assert_eq!(n.apply(&ctx(), raw), raw);
    }

    /// A dotted host name (a CI container's) normalizes both in full and as
    /// `#h`'s short form, and a run as root keeps the `root` key table.
    #[test]
    fn a_dotted_host_and_its_short_form_normalize_and_root_is_kept() {
        let n = Normalizer::load();
        let mut ctx = ctx();
        ctx.vars
            .insert("host".into(), "localhost.localdomain".into());
        ctx.vars.insert("user".into(), "root".into());
        let raw = "main|localhost|localhost.localdomain|localhostx\nkey-table root\n";
        let want = "main|<HOST>|<HOST>|localhostx\nkey-table root\n";
        assert_eq!(n.apply(&ctx, raw), want);
    }

    #[test]
    fn exact_pids_replace_without_a_window() {
        let n = Normalizer::load();
        let ctx = Context {
            exact_pids: vec![4242],
            ..Context::default()
        };
        assert_eq!(n.apply(&ctx, "4242 4243"), "<PID> 4243");
    }

    #[test]
    fn normalizing_is_idempotent() {
        let n = Normalizer::load();
        let once = n.apply(
            &ctx(),
            "x /tmp/.tmpAB12/tmux-0/golden 20001 Tue Oct 13 01:02:03 2026",
        );
        assert_eq!(n.apply(&ctx(), &once), once);
    }
}
