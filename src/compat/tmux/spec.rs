//! tmux's command table (`list-commands`) and how tmux's getopt reads a
//! command's flags.

use std::sync::OnceLock;

use super::{ParseError, TmuxError};

/// One tmux 3.7b command, as `list-commands` prints it.
#[derive(Debug)]
pub struct Spec {
    pub name: &'static str,
    pub alias: Option<&'static str>,
    /// Everything after the name and alias (`[-dErx] [-c ...] ...`).
    pub usage: &'static str,
    /// The `list-commands` line, as tmux prints it.
    pub line: &'static str,
    /// Flags that take a value, from the usage (`[-c working-directory]`)
    /// and, for flags tmux keeps out of it, [`HIDDEN_VALUE_FLAGS`].
    value_flags: String,
    /// Not in `list-commands`, found only by its exact name or alias.
    pub(super) hidden: bool,
}

impl Spec {
    pub(super) fn takes_value(&self, flag: char) -> bool {
        self.value_flags.contains(flag)
    }

    /// Whether tmux takes `flag` for this command: its usage lists it, or
    /// it is one of the hidden ones.
    fn lists(&self, flag: char) -> bool {
        !UNPARSED_FLAGS.contains(&(self.name, flag))
            && (self.takes_value(flag)
                || HIDDEN_BOOL_FLAGS.contains(&(self.name, flag))
                || self.usage.split("[-").skip(1).any(|part| {
                    part.split([']', ' '])
                        .next()
                        .is_some_and(|l| l.contains(flag))
                }))
    }

    /// tmux's own flag errors for `args` (after the verb): a flag the usage
    /// does not list (hidden flags it still takes, [`HIDDEN_VALUE_FLAGS`],
    /// count as listed) is `unknown flag`, a value flag with no value
    /// `expects an argument`.
    pub fn check_flags(&self, args: &[String]) -> Result<(), TmuxError> {
        for (flag, value) in self.getopt(args).flags {
            if flag == '-' {
                return Err(ParseError::UnknownLongFlag(self.name.to_string()).into());
            }
            if !self.lists(flag) {
                return Err(ParseError::UnknownFlag(self.name.to_string(), flag).into());
            }
            if self.takes_value(flag) && value.is_none() {
                return Err(ParseError::MissingValue(self.name.to_string(), flag).into());
            }
        }
        Ok(())
    }

    /// `args` (after the verb) as tmux's getopt reads them for this
    /// command.
    pub fn getopt<'a>(&self, args: &'a [String]) -> Getopt<'a> {
        let mut flags = Vec::new();
        let mut i = 0;
        while let Some(arg) = args.get(i) {
            if arg == "--" {
                i += 1;
                break;
            }
            let Some(letters) = arg.strip_prefix('-').filter(|l| !l.is_empty()) else {
                break;
            };
            i += 1;
            for (at, flag) in letters.char_indices() {
                if !self.takes_value(flag) {
                    flags.push((flag, None));
                    continue;
                }
                let attached = &letters[at + flag.len_utf8()..];
                let value = if attached.is_empty() {
                    let next = args.get(i).map(String::as_str);
                    i += usize::from(next.is_some());
                    next
                } else {
                    Some(attached)
                };
                flags.push((flag, value));
                break;
            }
        }
        Getopt { flags, rest: i }
    }

    /// How many positionals the usage requires: its words outside any
    /// `[...]` (`option [value]` is 1, `shell-command command [command]`
    /// 2).
    pub fn min_args(&self) -> usize {
        let mut depth = 0usize;
        let mut words = 0;
        for word in self.usage.split(' ') {
            if depth == 0 && !word.starts_with('[') && word != "..." && !word.is_empty() {
                words += 1;
            }
            depth += word.matches('[').count();
            depth = depth.saturating_sub(word.matches(']').count());
        }
        words
    }
}

/// TC.0b's raw `tmux list-commands` (tmux 3.7b), the alias table.
const LIST_COMMANDS: &str = include_str!("data/list-commands.txt");

/// Commands tmux 3.7b runs but `list-commands` leaves out, found only by
/// exact name or alias (recorded: `info` and `server-info` answer as
/// `show-messages`, and `se`, `ser` do not list them).
const HIDDEN: &[&str] = &["server-info (info) [-JT] [-t target-client]"];

/// Value flags tmux 3.7b accepts but its usage leaves out (recorded:
/// `select-pane -t x -P` fails `-P expects an argument`).
const HIDDEN_VALUE_FLAGS: &[(&str, char)] = &[
    ("select-pane", 'P'),
    ("join-pane", 'p'),
    ("move-pane", 'p'),
    ("refresh-client", 'F'),
    ("run-shell", 's'),
];

/// Flags a usage lists that the command's args template does not take
/// (`server-access [-t target-pane]` is `unknown flag -t`).
const UNPARSED_FLAGS: &[(&str, char)] = &[("server-access", 't')];

/// Flags with no value tmux 3.7b accepts but its usage leaves out
/// (recorded: `load-buffer -w path` loads and exits 0).
const HIDDEN_BOOL_FLAGS: &[(&str, char)] = &[
    ("load-buffer", 'w'),
    ("select-pane", 'g'),
    ("choose-buffer", 'y'),
    ("choose-client", 'y'),
    ("choose-tree", 'y'),
    ("customize-mode", 'y'),
    ("list-buffers", 'r'),
    ("list-clients", 'r'),
    ("new-pane", 'E'),
    ("new-pane", 'L'),
    ("split-window", 'E'),
    ("switch-client", 'F'),
];

fn parse_spec(line: &'static str, hidden: bool) -> Spec {
    let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
    let (alias, usage) = match rest.strip_prefix('(').and_then(|r| r.split_once(')')) {
        Some((alias, usage)) => (Some(alias), usage.trim_start()),
        None => (None, rest),
    };
    // `[-c working-directory]` takes a value; `[-dErx]` and `[-L|-S|-U]`
    // do not.
    let mut value_flags: String = usage
        .split("[-")
        .skip(1)
        .filter_map(|part| {
            let mut chars = part.chars();
            let flag = chars.next()?;
            (chars.next() == Some(' ')).then_some(flag)
        })
        .collect();
    value_flags.extend(
        HIDDEN_VALUE_FLAGS
            .iter()
            .filter(|(verb, _)| *verb == name)
            .map(|(_, f)| *f),
    );
    Spec {
        name,
        alias,
        usage,
        line,
        value_flags,
        hidden,
    }
}

/// tmux's commands in its own table order, then the hidden ones.
pub fn specs() -> &'static [Spec] {
    static SPECS: OnceLock<Vec<Spec>> = OnceLock::new();
    SPECS.get_or_init(|| {
        let mut specs: Vec<Spec> = LIST_COMMANDS
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| parse_spec(l, false))
            .collect();
        specs.extend(HIDDEN.iter().map(|l| parse_spec(l, true)));
        specs
    })
}

/// The command `name` means, by tmux 3.7b's rule (`cmd_find`): walking the
/// table in order, an alias equal to `name` wins at once; otherwise a name
/// `name` is a prefix of, and it is ambiguous when more than one is,
/// unless the last one found is exactly `name`. The errors are tmux's.
pub fn resolve(name: &str) -> Result<&'static Spec, TmuxError> {
    let specs = specs();
    if let Some(spec) = specs
        .iter()
        .find(|s| s.hidden && (s.name == name || s.alias == Some(name)))
    {
        return Ok(spec);
    }
    let visible = || specs.iter().filter(|s| !s.hidden);
    let mut found = None;
    let mut ambiguous = false;
    for spec in visible() {
        if spec.alias == Some(name) {
            found = Some(spec);
            ambiguous = false;
            break;
        }
        if !spec.name.starts_with(name) {
            continue;
        }
        ambiguous |= found.is_some();
        found = Some(spec);
        if spec.name.len() == name.len() {
            break;
        }
    }
    match found {
        Some(_) if ambiguous => {
            let names: Vec<&str> = visible()
                .filter(|s| s.name.starts_with(name))
                .map(|s| s.name)
                .collect();
            Err(TmuxError(format!(
                "ambiguous command: {name}, could be: {}",
                names.join(", ")
            )))
        }
        Some(spec) => Ok(spec),
        None => Err(TmuxError(format!("unknown command: {name}"))),
    }
}

/// A command's flags as tmux's getopt reads them: they end at `--` or at
/// the first word that is not a flag; a value flag takes the rest of its
/// word or the next word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Getopt<'a> {
    pub flags: Vec<(char, Option<&'a str>)>,
    /// Index into the arguments of the first positional.
    pub rest: usize,
}

impl Getopt<'_> {
    pub fn has(&self, flag: char) -> bool {
        self.flags.iter().any(|(f, _)| *f == flag)
    }

    /// The flag's value, the last one given.
    pub fn value(&self, flag: char) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(f, _)| *f == flag)
            .and_then(|(_, v)| *v)
    }
}
