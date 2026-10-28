//! The tmux front end (spec D1, §4.5; FE.1): the table that translates
//! tmux 3.7b's argv forms into native [`Command`]s, or into tmux's own
//! error text. This file holds the row schema ([`Row`]), the dispatcher
//! ([`dispatch`]), the area registration ([`table`]), tmux's command table
//! with its alias and prefix rule ([`resolve`]), the one tmux option-name
//! table ([`OPTION_NAMES`]) and the core rows: the invocation, pass-through
//! rows for every M1 to M3 verb whose tmux form is grofe's, `set`/`show` by
//! tmux name, `setw`/`showw`, `list-commands`, `info`, and the spec's out
//! of scope forms. FE.2, FE.3 and FE.4 each fill one area file
//! (`structure`, `formats`, `bindings`) with rows only; nothing after FE.1
//! edits this file.
//!
//! How a command is translated ([`dispatch_command`]): its verb resolves
//! through tmux's alias and prefix rule; the first row of [`table`] whose
//! form it matches decides (area rows come before the core rows, so an
//! area can take over a form); flags are read with tmux's own getopt rules
//! for that verb. A pass-through row hands the argv to the native parser
//! under the native verb name, with targets read the tmux way (a bare `=`
//! the mouse target, `+`/`-` offsets: [`cmd::target::with_tmux_targets`];
//! `=name` is exact in both grammars).
//!
//! Stored command strings keep their dialect (spec §4.5): a binding, hook,
//! alias, menu item or prompt template stored from a tmux-dialect request
//! is stored as tmux wrote it, tagged `Dialect::Tmux`, checked through this
//! front end when stored, and translated here each time it runs. So
//! `list-keys` shows what was bound, and a native template on a
//! tmux-identity server keeps its own dialect. Only `:` prompt text takes
//! the server's identity (`compat::prompt_dialect`).
//!
//! The split with `main.rs`: [`cli`] only reads tmux's argv (its global
//! flags, `-V`, bare `tmux`, the first verb's alias) into a [`TmuxCli`];
//! `main.rs` keeps the transport (socket, server start, hello, attach) and
//! sends `argv` with the dialect [`cli`] chose.

mod bindings;
mod copy;
pub(crate) mod formats;
mod structure;

use std::fmt;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::cmd::{self, Command, DisplayMessage, ParseError, Target};
use crate::protocol::{ClientCtx, Dialect};

/// A tmux-dialect request that fails, worded as tmux words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxError(pub String);

impl fmt::Display for TmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TmuxError {}

impl From<ParseError> for TmuxError {
    fn from(e: ParseError) -> Self {
        TmuxError(e.to_string())
    }
}

// ---- The schema --------------------------------------------------------

/// One tmux form the owner's setup sends (spec §4.5), and what grofe does
/// with it.
#[derive(Debug, Clone)]
pub struct Row {
    /// The verb, by its full tmux name, then the flags the form has, each
    /// as `-x`, or `-x[V]` for the flag with exactly the value `V` (`-t[=]`
    /// is a bare `=` target). A form that is not a verb's starts with
    /// `tmux ` (the CLI's global flags) or `-` (the target grammar).
    pub tmux_form: &'static str,
    pub kind: MappingKind,
    pub target: GrofeTarget,
    /// One mark per flag of `tmux_form`, in its order.
    pub flags: &'static [FlagMark],
    /// When the grofe feature the row maps onto lands (`M1`, `M3.6`, `M5`).
    pub milestone: &'static str,
    /// The golden that pins the row, `<task>/<scenario>`, or empty.
    pub golden_id: &'static str,
}

/// Spec §4.5's mapping kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingKind {
    /// `ls`, `setw`, `tmux a`: an alias or prefix of a verb.
    AliasOrPrefix,
    /// A flag renamed or merged: `-t =name`, tmux option names.
    FlagRename,
    /// A tmux behaviour onto a grofe feature.
    Behaviour,
    /// Verb and flags identical to grofe's.
    PassThrough,
}

/// Whether grofe may ignore a flag it cannot honour (noting it in
/// `compat-miss.log`) or must fail without it (spec §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagMark {
    Cosmetic,
    Semantic,
}

/// What a row maps onto.
#[derive(Debug, Clone, Copy)]
pub enum GrofeTarget {
    /// The native verb of this name, with the same flags: the argv is
    /// parsed natively ([`Invocation::native`]).
    Native(&'static str),
    /// A translation of the row's own.
    Translate(Translate),
    /// Not supported, for this reason (spec §4.5's out-of-scope table, or
    /// the milestone that brings it). The command fails loudly, as
    /// `<verb>: not supported (<reason>)`.
    Unsupported(&'static str),
    /// Refused with exactly this text, tmux 3.7b's own for the form
    /// (recorded: `send -M` is `no mouse target`). Always for good.
    Refuse(&'static str),
    /// Read by [`cli`] before any server is reached; what it does.
    Cli(&'static str),
    /// Part of the target grammar every row shares; what it does.
    Grammar(&'static str),
}

/// A row's translation: the native commands a tmux command means.
pub type Translate = fn(&Invocation<'_>) -> Result<Vec<Command>, TmuxError>;

impl Row {
    /// The verb the row's form is for (`tmux` or empty for a form that is
    /// not a verb's).
    pub fn verb(&self) -> &'static str {
        match self.tmux_form.split(' ').next() {
            Some(v) if !v.starts_with('-') => v,
            _ => "",
        }
    }

    /// Each flag of the form with its value restriction (`-x[V]`).
    pub fn form_flags(&self) -> Vec<(char, Option<&'static str>)> {
        self.tmux_form
            .split(' ')
            .skip(usize::from(!self.verb().is_empty()))
            .filter_map(|w| {
                let rest = w.strip_prefix('-')?;
                let flag = rest.chars().next()?;
                let value = rest[flag.len_utf8()..]
                    .strip_prefix('[')
                    .and_then(|v| v.strip_suffix(']'));
                Some((flag, value))
            })
            .collect()
    }

    /// The positionals the form pins, in order: its words after the verb
    /// that are not flags (`list-commands new-pane`).
    pub fn form_positionals(&self) -> Vec<&'static str> {
        self.tmux_form
            .split(' ')
            .skip(1)
            .filter(|w| !w.starts_with('-') && !self.verb().is_empty())
            .collect()
    }

    /// How much of a command the form pins: its flags and positionals.
    pub fn specificity(&self) -> usize {
        self.form_flags().len() + self.form_positionals().len()
    }

    /// How many exact values the form pins: flag values (`-x[M]`) and
    /// positionals, each more than a bare flag.
    pub fn pinned_values(&self) -> usize {
        self.form_flags()
            .iter()
            .filter(|(_, v)| v.is_some())
            .count()
            + self.form_positionals().len()
    }

    /// Whether the row refuses its form for good: a [`GrofeTarget::Refuse`]
    /// row, or an [`GrofeTarget::Unsupported`] one with no milestone (spec
    /// §4.5's out of scope forms). A refusal until a later milestone (M5's
    /// copy verbs) ranks as a mapping does, so that milestone can map the
    /// form.
    pub fn refuses(&self) -> bool {
        match self.target {
            GrofeTarget::Refuse(_) => true,
            GrofeTarget::Unsupported(_) => self.milestone.is_empty(),
            _ => false,
        }
    }

    /// Whether the command `inv` is this row's form: the same verb, with
    /// every flag the form names (and the value it names, if any), and
    /// each pinned positional in its place.
    fn matches(&self, inv: &Invocation<'_>) -> bool {
        if self.verb() != inv.verb {
            return false;
        }
        let got = inv.getopt();
        let flags = self.form_flags().iter().all(|(flag, value)| match value {
            None => got.has(*flag),
            Some(v) => got.flags.iter().any(|(f, x)| f == flag && *x == Some(*v)),
        });
        let positionals = inv.positionals();
        flags
            && self
                .form_positionals()
                .iter()
                .enumerate()
                .all(|(i, p)| positionals.get(i).is_some_and(|a| a == p))
    }
}

/// The row that decides for `inv`, of the rows it matches, by precedence:
///
/// 1. a refusal for good ([`Row::refuses`]) wins over any mapping, so an
///    out of scope form (`new-session -t`, a mouse argv form) is refused
///    however many flags a mapping row for the same verb names, and
///    wherever it is registered; an area row that refuses the same form
///    replaces the core refusal (by rule 4), which is how an area gives a
///    form tmux's own error text ([`GrofeTarget::Refuse`]);
/// 2. then the row pinning more exact values ([`Row::pinned_values`]): a
///    `-x[M]` outranks a bare `-x`;
/// 3. then the more specific row ([`Row::specificity`]);
/// 4. then the earlier row in `rows` ([`table`]'s order: the area files
///    before the core rows).
fn select<'r>(rows: &'r [Row], inv: &Invocation<'_>) -> Option<&'r Row> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.matches(inv))
        .max_by_key(|(i, row)| {
            (
                row.refuses(),
                row.pinned_values(),
                row.specificity(),
                std::cmp::Reverse(*i),
            )
        })
        .map(|(_, row)| row)
}

/// The row that decides for `argv` (a tmux command, `argv[0]` its verb),
/// before any rewrite: for the FE tasks' coverage tests.
pub fn row_for(argv: &[String]) -> Option<&'static Row> {
    let (name, args) = argv.split_first()?;
    let spec = resolve(name).ok()?;
    let ctx = ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Tmux,
    };
    let inv = Invocation {
        verb: spec.name,
        args,
        ctx: &ctx,
        spec,
    };
    select(table(), &inv)
}

/// Every row: the three area files' (FE.2, FE.3, FE.4), then this file's
/// own. [`select`] decides between the rows a command matches (a refusal
/// first, then pinned values, then specificity, then this order), so an
/// area row can take over a core mapping by naming its form.
pub fn table() -> &'static [Row] {
    static TABLE: OnceLock<Vec<Row>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut rows = structure::rows();
        rows.extend(formats::rows());
        rows.extend(bindings::rows());
        rows.extend(copy::rows());
        rows.extend(core_rows());
        rows
    })
}

// ---- Argument rewrites -------------------------------------------------

/// Which argument of a command a [`Rewrite`] applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    /// The value of this flag (`-F`).
    Flag(char),
    /// The positional at this index (`set`'s value is 1).
    Positional(usize),
    /// Every positional.
    Positionals,
}

/// A rewrite of one argument, applied before any row sees the command
/// (FE.3: tmux formats rewritten into grofe's in `display`, `list-* -F`
/// and `set` values, `#(cmd)` onto a status source). Rows then match and
/// translate the rewritten command, so a rewrite applies in front of
/// FE.2's rows and the core ones alike. Rewrites of the same argument
/// apply in [`rewrites`]' order, each on the previous one's value.
///
/// `rewrite` sees the command as sent ([`Invocation`]: its verb, its flags
/// through `getopt`, [`Invocation::option_name`] for `set`/`show`) and the
/// argument's value, and answers the new value plus any native commands
/// to run before the command itself ([`Rewritten::before`], such as
/// [`cmd::RegisterSource`] for a `#(cmd)`).
#[derive(Debug, Clone, Copy)]
pub struct Rewrite {
    /// The verb's full tmux name.
    pub verb: &'static str,
    pub arg: Arg,
    pub rewrite: fn(&Invocation<'_>, &str) -> Result<Rewritten, TmuxError>,
}

/// What a [`Rewrite`] makes of one argument.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rewritten {
    pub value: String,
    /// Native commands run before the rewritten command, in order.
    pub before: Vec<Command>,
}

impl Rewritten {
    /// Just a new value.
    pub fn value(value: impl Into<String>) -> Rewritten {
        Rewritten {
            value: value.into(),
            before: Vec::new(),
        }
    }
}

/// Every rewrite: the three area files', in the same order as [`table`].
pub fn rewrites() -> &'static [Rewrite] {
    static REWRITES: OnceLock<Vec<Rewrite>> = OnceLock::new();
    REWRITES.get_or_init(|| {
        let mut all = structure::rewrites();
        all.extend(formats::rewrites());
        all.extend(bindings::rewrites());
        all
    })
}

/// `inv`'s arguments with `rewrites` applied, rebuilt as separate flag and
/// value words, and the commands the rewrites run first; `None` when no
/// rewrite is for its verb.
/// A command's arguments after its rewrites, and the commands they run
/// first.
type Rewrote = (Vec<String>, Vec<Command>);

fn rewritten(inv: &Invocation<'_>, rewrites: &[Rewrite]) -> Result<Option<Rewrote>, TmuxError> {
    let mine: Vec<&Rewrite> = rewrites.iter().filter(|r| r.verb == inv.verb).collect();
    if mine.is_empty() {
        return Ok(None);
    }
    let mut before = Vec::new();
    let mut apply = |arg: Arg, value: &str| -> Result<String, TmuxError> {
        let mut value = value.to_string();
        for r in mine.iter().filter(|r| r.arg == arg) {
            let got = (r.rewrite)(inv, &value)?;
            before.extend(got.before);
            value = got.value;
        }
        Ok(value)
    };
    let got = inv.getopt();
    let mut out = Vec::new();
    for (flag, value) in &got.flags {
        out.push(format!("-{flag}"));
        if let Some(v) = value {
            out.push(apply(Arg::Flag(*flag), v)?);
        }
    }
    let rest = got.rest.min(inv.args.len());
    if rest > 0 && inv.args[rest - 1] == "--" {
        out.push("--".to_string());
    }
    for (i, word) in inv.args[rest..].iter().enumerate() {
        let word = apply(Arg::Positional(i), word)?;
        out.push(apply(Arg::Positionals, &word)?);
    }
    Ok(Some((out, before)))
}

// ---- tmux's command table ----------------------------------------------

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
    hidden: bool,
}

impl Spec {
    fn takes_value(&self, flag: char) -> bool {
        self.value_flags.contains(flag)
    }

    /// Whether tmux takes `flag` for this command: its usage lists it, or
    /// it is one of the hidden ones.
    fn lists(&self, flag: char) -> bool {
        self.takes_value(flag)
            || HIDDEN_BOOL_FLAGS.contains(&(self.name, flag))
            || self.usage.split("[-").skip(1).any(|part| {
                part.split([']', ' '])
                    .next()
                    .is_some_and(|l| l.contains(flag))
            })
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
const LIST_COMMANDS: &str = include_str!("../../../tests/fixtures/compat/golden/list-commands.txt");

/// Commands tmux 3.7b runs but `list-commands` leaves out, found only by
/// exact name or alias (recorded: `info` and `server-info` answer as
/// `show-messages`, and `se`, `ser` do not list them).
const HIDDEN: &[&str] = &["server-info (info) [-JT] [-t target-client]"];

/// Value flags tmux 3.7b accepts but its usage leaves out (recorded:
/// `select-pane -t x -P` fails `-P expects an argument`).
const HIDDEN_VALUE_FLAGS: &[(&str, char)] = &[("select-pane", 'P')];

/// Flags with no value tmux 3.7b accepts but its usage leaves out
/// (recorded: `load-buffer -w path` loads and exits 0).
const HIDDEN_BOOL_FLAGS: &[(&str, char)] = &[("load-buffer", 'w'), ("select-pane", 'g')];

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

// ---- One command ---------------------------------------------------------

/// One tmux command being translated.
pub struct Invocation<'a> {
    /// The verb's full tmux name.
    pub verb: &'static str,
    /// The arguments after the verb, as sent.
    pub args: &'a [String],
    pub ctx: &'a ClientCtx,
    spec: &'static Spec,
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

impl<'a> Invocation<'a> {
    pub fn getopt(&self) -> Getopt<'a> {
        let args = self.args;
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
                if !self.spec.takes_value(flag) {
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

    /// The option a `set`/`show` (or `setw`/`showw`) names: its first
    /// positional, as sent.
    pub fn option_name(&self) -> Option<&'a str> {
        matches!(
            self.verb,
            "set-option" | "show-options" | "set-window-option" | "show-window-options"
        )
        .then(|| self.positionals().first().map(String::as_str))
        .flatten()
    }

    /// The positionals, after the flags.
    pub fn positionals(&self) -> &'a [String] {
        &self.args[self.getopt().rest.min(self.args.len())..]
    }

    /// The native command `verb args` means, targets read the tmux way.
    /// An error names this command's tmux verb where the native parser
    /// named `verb` (`command set-window-option: ...`).
    pub fn native(&self, verb: &str, args: &[String]) -> Result<Vec<Command>, TmuxError> {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(verb.to_string());
        argv.extend_from_slice(args);
        SPELLED.with_borrow_mut(|spelled| {
            if let Some(spelled) = spelled {
                spelled.push(argv.clone());
            }
        });
        cmd::target::with_tmux_targets(|| cmd::parse(&argv))
            .map(|c| vec![c])
            .map_err(|e| {
                // T1.2's native text for a missing positional; tmux counts.
                let e = match e {
                    ParseError::MissingArgument(verb) => {
                        ParseError::TooFewArguments(verb, self.spec.min_args())
                    }
                    e => e,
                };
                let text = e.to_string();
                let native = format!("command {verb}:");
                match text.strip_prefix(&native) {
                    Some(rest) if verb != self.verb => {
                        TmuxError(format!("command {}:{rest}", self.verb))
                    }
                    _ => TmuxError(text),
                }
            })
    }

    /// Notes in `compat-miss.log` that a cosmetic flag was ignored.
    pub fn ignored(&self, flag: char) {
        crate::compat::note_miss(&format!("{} -{flag}: cosmetic, ignored", self.verb));
    }
}

/// tmux's position letters for menus and popups, as the corpus records them.
const POSITION_LETTERS: &[&str] = &["M", "W", "S", "C", "P", "R"];

/// One command's form as `docs/compat-coverage.md` prints it
/// (`scripts/corpus.py`'s `form_of`): the verb's full name, then its flags
/// in case-blind order, a bare `=` target as `-t[=]`, a menu or popup
/// position letter as `-x[M]`, and a flag tmux 3.7b lacks as `-P!`. `None`
/// when the verb does not resolve. For the FE tasks' coverage tests.
pub fn form_of(argv: &[String]) -> Option<String> {
    let (name, args) = argv.split_first()?;
    let spec = resolve(name).ok()?;
    let ctx = ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Tmux,
    };
    let inv = Invocation {
        verb: spec.name,
        args,
        ctx: &ctx,
        spec,
    };
    // A flag in the usage; hidden ones (`select-pane -P`) are not.
    let known = |flag: char| {
        spec.usage.split("[-").skip(1).any(|part| {
            part.split([']', ' '])
                .next()
                .is_some_and(|l| l.contains(flag))
        })
    };
    let mut toks: Vec<String> = Vec::new();
    for (flag, value) in inv.getopt().flags {
        let tok = if value == Some("=") && "ts".contains(flag) {
            format!("-{flag}[=]")
        } else if "xy".contains(flag)
            && matches!(spec.name, "display-menu" | "display-popup")
            && value.is_some_and(|v| POSITION_LETTERS.contains(&v))
        {
            format!("-{flag}[{}]", value.unwrap_or_default())
        } else if !known(flag) {
            format!("-{flag}!")
        } else {
            format!("-{flag}")
        };
        if !toks.contains(&tok) {
            toks.push(tok);
        }
    }
    toks.sort_by_key(|t| {
        let c = t.chars().nth(1).unwrap_or('-');
        (c.to_ascii_lowercase(), c.is_ascii_uppercase(), t.clone())
    });
    let mut out = spec.name.to_string();
    for t in toks {
        out.push(' ');
        out.push_str(&t);
    }
    Some(out)
}

/// A command as `compat-miss.log` records it: the verb's full name and
/// the flag letters given, never a value or a positional, so nothing a
/// command carries is logged. An unknown verb is `<unknown>`, and each
/// leading flag word keeps only its first letter, since its value flags
/// are unknown.
pub fn miss_form(argv: &[String]) -> String {
    let Some((name, args)) = argv.split_first() else {
        return String::new();
    };
    let mut out = String::new();
    match resolve(name) {
        Ok(spec) => {
            out.push_str(spec.name);
            let ctx = ClientCtx {
                cwd: PathBuf::from("/"),
                pane: None,
                env: Vec::new(),
                client: None,
                dialect: Dialect::Tmux,
            };
            let inv = Invocation {
                verb: spec.name,
                args,
                ctx: &ctx,
                spec,
            };
            for (flag, _) in inv.getopt().flags {
                out.push_str(&format!(" -{flag}"));
            }
        }
        // An unknown verb could be anything the user typed.
        Err(_) => {
            out.push_str("<unknown>");
            for arg in args {
                let Some(flag) = arg.strip_prefix('-').and_then(|f| f.chars().next()) else {
                    break;
                };
                if !flag.is_ascii_alphanumeric() {
                    break;
                }
                out.push_str(&format!(" -{flag}"));
            }
        }
    }
    out
}

/// Translates one tmux-dialect request (`argv[0]` is the verb) into the
/// native commands it means, or tmux's error. The request is split into
/// commands at `;` arguments as tmux's CLI splits them (T3.5's
/// `cmd::parse::split_argv`), and the whole of it is translated before
/// anything runs, as tmux parses a whole chain first.
pub fn dispatch(argv: &[String], ctx: &ClientCtx) -> Result<Vec<Command>, TmuxError> {
    // A request of nothing but `;` is no command, which tmux runs as such
    // (recorded: `tmux \;` exits 0).
    let mut out = Vec::new();
    for words in cmd::parse::split_argv(argv) {
        out.extend(dispatch_command(&words, ctx)?);
    }
    Ok(out)
}

/// Why [`dispatch_checked`] could not translate a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// tmux 3.7b refuses the command itself, while parsing it: an unknown
    /// command, a flag its usage lacks, a missing flag value, or the wrong
    /// number of arguments.
    Syntax(TmuxError),
    /// Valid tmux that grofe does not do (an `Unsupported` row, a flag or
    /// hook it lacks).
    Unsupported(TmuxError),
}

/// [`dispatch_command`], telling tmux's own parse errors from commands
/// grofe merely cannot run. A sourced file stops at the first kind and goes
/// on past the second.
pub fn dispatch_checked(argv: &[String], ctx: &ClientCtx) -> Result<Vec<Command>, Refusal> {
    let (name, args) = argv
        .split_first()
        .ok_or_else(|| Refusal::Syntax(ParseError::EmptyCommand.into()))?;
    let spec = resolve(name).map_err(Refusal::Syntax)?;
    let inv = Invocation {
        verb: spec.name,
        args,
        ctx,
        spec,
    };
    check_flags(&inv).map_err(Refusal::Syntax)?;
    dispatch_command(argv, ctx).map_err(|e| {
        let counts = e.0.starts_with("command ")
            && (e.0.contains(": too many arguments") || e.0.contains(": too few arguments"));
        if counts {
            Refusal::Syntax(e)
        } else {
            Refusal::Unsupported(e)
        }
    })
}

/// Translates one command whose argv is already split (a stored command
/// string, a binding, prompt text): a `;` argument in it is literal.
pub fn dispatch_command(argv: &[String], ctx: &ClientCtx) -> Result<Vec<Command>, TmuxError> {
    translate_with(argv, ctx, table(), rewrites())
}

thread_local! {
    /// While [`spell`] runs: each argv [`Invocation::native`] hands the
    /// native parser, in order.
    static SPELLED: std::cell::RefCell<Option<Vec<Vec<String>>>> =
        const { std::cell::RefCell::new(None) };
}

/// [`dispatch_command`] for `import-tmux` (TP.0), which runs without a
/// server and writes grofe's own command strings: the native commands one
/// tmux command means, and each native argv this table's rows parsed to
/// make them, in order. Those argv are the live front end's own
/// translation, so the converter spells a command exactly as the front end
/// reads it and keeps no table of its own. Nothing is noted in
/// `compat-miss.log`.
pub fn spell(argv: &[String]) -> Result<(Vec<Command>, Vec<Vec<String>>), TmuxError> {
    let ctx = ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Tmux,
    };
    let outer = SPELLED.replace(Some(Vec::new()));
    let got = crate::compat::quietly(|| dispatch_command(argv, &ctx));
    let spelled = SPELLED.replace(outer).unwrap_or_default();
    got.map(|commands| (commands, spelled))
}

/// [`dispatch_command`] over the given rows and rewrites: the verb
/// resolves, its flags are checked against tmux's usage, the rewrites
/// apply, then the most specific matching row decides.
fn translate_with(
    argv: &[String],
    ctx: &ClientCtx,
    rows: &[Row],
    rewrites: &[Rewrite],
) -> Result<Vec<Command>, TmuxError> {
    let (name, args) = argv.split_first().ok_or(ParseError::EmptyCommand)?;
    let spec = resolve(name)?;
    let inv = Invocation {
        verb: spec.name,
        args,
        ctx,
        spec,
    };
    check_flags(&inv)?;
    let (owned, mut before) = match rewritten(&inv, rewrites)? {
        Some((args, before)) => (Some(args), before),
        None => (None, Vec::new()),
    };
    let args = owned.as_deref().unwrap_or(args);
    let inv = Invocation { args, ..inv };
    let main = match select(rows, &inv).map(|r| r.target) {
        Some(GrofeTarget::Native(verb)) => inv.native(verb, args),
        Some(GrofeTarget::Translate(translate)) => translate(&inv),
        Some(GrofeTarget::Unsupported(reason)) => Err(TmuxError(format!(
            "{}: not supported ({reason})",
            spec.name
        ))),
        Some(GrofeTarget::Refuse(text)) => Err(TmuxError(text.to_string())),
        Some(GrofeTarget::Cli(_) | GrofeTarget::Grammar(_)) | None => {
            Err(TmuxError(format!("not implemented: {}", spec.name)))
        }
    }?;
    before.extend(main);
    Ok(before)
}

/// tmux's own flag errors, before any row: a flag its usage does not list
/// (hidden flags it still takes, [`HIDDEN_VALUE_FLAGS`], count as listed)
/// is `unknown flag`, a value flag with no value `expects an argument`.
fn check_flags(inv: &Invocation<'_>) -> Result<(), TmuxError> {
    let spec = inv.spec;
    for (flag, value) in inv.getopt().flags {
        if flag == '-' {
            return Err(ParseError::UnknownLongFlag(spec.name.to_string()).into());
        }
        if !spec.lists(flag) {
            return Err(ParseError::UnknownFlag(spec.name.to_string(), flag).into());
        }
        if spec.takes_value(flag) && value.is_none() {
            return Err(ParseError::MissingValue(spec.name.to_string(), flag).into());
        }
    }
    Ok(())
}

// ---- Options by tmux name ------------------------------------------------

/// The one tmux option-name table (spec §4.5), shared with `import-tmux`:
/// every option name the tmux dialect sets or shows, by tmux name, with
/// the name grofe's option store keeps it under. It holds every option in
/// the corpus (`docs/compat-coverage.md`, "Options by tmux name") and every
/// built-in option; a name outside it passes through and fails as it does
/// natively (`invalid option: <name>`). Names grofe stores without acting
/// on them are the option store's `NO_EFFECT` list.
pub const OPTION_NAMES: &[(&str, &str)] = &[
    ("activity-action", "activity-action"),
    ("aggressive-resize", "aggressive-resize"),
    ("allow-passthrough", "allow-passthrough"),
    ("allow-rename", "allow-rename"),
    ("automatic-rename", "automatic-rename"),
    ("automatic-rename-format", "automatic-rename-format"),
    ("base-index", "base-index"),
    ("bell-action", "bell-action"),
    ("buffer-limit", "buffer-limit"),
    ("clock-mode-colour", "clock-mode-colour"),
    ("clock-mode-style", "clock-mode-style"),
    ("command-alias", "command-alias"),
    ("compat-identity", "compat-identity"),
    ("copy-command", "copy-command"),
    (
        "copy-mode-current-match-style",
        "copy-mode-current-match-style",
    ),
    ("copy-mode-match-style", "copy-mode-match-style"),
    ("default-command", "default-command"),
    ("default-shell", "default-shell"),
    ("default-size", "default-size"),
    ("default-terminal", "default-terminal"),
    ("detach-on-destroy", "detach-on-destroy"),
    ("display-time", "display-time"),
    ("escape-time", "escape-time"),
    ("exit-empty", "exit-empty"),
    ("extended-keys", "extended-keys"),
    ("extended-keys-format", "extended-keys-format"),
    ("focus-events", "focus-events"),
    ("focus-follows-mouse", "focus-follows-mouse"),
    ("graphics", "graphics"),
    ("history-limit", "history-limit"),
    ("main-pane-height", "main-pane-height"),
    ("main-pane-width", "main-pane-width"),
    ("message-style", "message-style"),
    ("mode-keys", "mode-keys"),
    ("mode-style", "mode-style"),
    ("monitor-activity", "monitor-activity"),
    ("monitor-bell", "monitor-bell"),
    ("monitor-silence", "monitor-silence"),
    ("mouse", "mouse"),
    ("pane-active-border-style", "pane-active-border-style"),
    ("pane-base-index", "pane-base-index"),
    ("pane-border-format", "pane-border-format"),
    ("pane-border-indicators", "pane-border-indicators"),
    ("pane-border-lines", "pane-border-lines"),
    ("pane-border-status", "pane-border-status"),
    ("pane-border-style", "pane-border-style"),
    ("prefix", "prefix"),
    ("prefix2", "prefix2"),
    ("remain-on-exit", "remain-on-exit"),
    ("renumber-windows", "renumber-windows"),
    ("repeat-time", "repeat-time"),
    ("set-clipboard", "set-clipboard"),
    ("set-titles", "set-titles"),
    ("set-titles-string", "set-titles-string"),
    ("silence-action", "silence-action"),
    ("status", "status"),
    ("status-bg", "status-bg"),
    ("status-fg", "status-fg"),
    ("status-format", "status-format"),
    ("status-interval", "status-interval"),
    ("status-justify", "status-justify"),
    ("status-keys", "status-keys"),
    ("status-left", "status-left"),
    ("status-left-length", "status-left-length"),
    ("status-left-style", "status-left-style"),
    ("status-position", "status-position"),
    ("status-right", "status-right"),
    ("status-right-length", "status-right-length"),
    ("status-right-style", "status-right-style"),
    ("status-style", "status-style"),
    ("synchronize-panes", "synchronize-panes"),
    ("terminal-features", "terminal-features"),
    ("terminal-overrides", "terminal-overrides"),
    ("update-environment", "update-environment"),
    ("visual-activity", "visual-activity"),
    ("visual-bell", "visual-bell"),
    ("visual-silence", "visual-silence"),
    ("window-size", "window-size"),
    (
        "window-status-activity-style",
        "window-status-activity-style",
    ),
    ("window-status-bell-style", "window-status-bell-style"),
    (
        "window-status-current-format",
        "window-status-current-format",
    ),
    ("window-status-current-style", "window-status-current-style"),
    ("window-status-format", "window-status-format"),
    ("window-status-last-style", "window-status-last-style"),
    ("window-status-separator", "window-status-separator"),
    ("window-status-style", "window-status-style"),
    ("window-style", "window-style"),
    ("window-active-style", "window-active-style"),
    ("word-separators", "word-separators"),
    ("wrap-search", "wrap-search"),
];

/// The option store's name for tmux option `tmux`.
pub fn option_name(tmux: &str) -> Option<&'static str> {
    OPTION_NAMES
        .iter()
        .find(|(t, _)| *t == tmux)
        .map(|(_, n)| *n)
}

/// `name` in the option store's spelling: a user option (`@x`) as it is, a
/// table name renamed, `name[N]` with its base renamed; anything else as
/// it is, for the store to refuse.
fn store_name(name: &str) -> String {
    if name.starts_with('@') {
        return name.to_string();
    }
    let (base, index) = match name.find('[') {
        Some(at) => name.split_at(at),
        None => (name, ""),
    };
    match option_name(base) {
        Some(store) => format!("{store}{index}"),
        None => name.to_string(),
    }
}

/// The arguments of a tmux `set`/`show` for the native option verbs: `-s`
/// (tmux's server scope) becomes `-g`, since grofe's server scope is its
/// global one and natively `-s` is grofe's session scope; `-w` goes first
/// for the window forms; the option name (the first positional) is in the
/// store's spelling. Scope merges (`-gw`, `-gqv`) and `-a -F -u -q -o`
/// are native.
pub(crate) fn option_args(inv: &Invocation<'_>, window: bool) -> Vec<String> {
    let got = inv.getopt();
    let mut args = Vec::new();
    if window {
        args.push("-w".to_string());
    }
    for (flag, value) in &got.flags {
        let flag = if *flag == 's' { 'g' } else { *flag };
        args.push(format!("-{flag}"));
        args.extend(value.map(str::to_string));
    }
    let rest = got.rest.min(inv.args.len());
    if rest > 0 && inv.args[rest - 1] == "--" {
        args.push("--".to_string());
    }
    for (i, word) in inv.args[rest..].iter().enumerate() {
        args.push(if i == 0 {
            store_name(word)
        } else {
            word.clone()
        });
    }
    args
}

/// A named user option's own tmux scope: tmux keeps server (`-s`), global
/// session (`-g`) and global window (`-gw`) user options apart (recorded:
/// `set -s @s 1` then `show -g @s` is `invalid option: @s`). grofe's
/// global scope is the global session one; the other two are their own.
fn user_scope(inv: &Invocation<'_>, window: bool) -> Option<cmd::OptionScope> {
    let got = inv.getopt();
    let name = inv.args.get(got.rest)?;
    if !name.starts_with('@') {
        return None;
    }
    if got.has('s') {
        Some(cmd::OptionScope::Server)
    } else if got.has('g') && (window || got.has('w')) {
        Some(cmd::OptionScope::GlobalWindow)
    } else {
        None
    }
}

/// The native option command for a tmux `set`/`show`, a user option kept
/// in its own tmux scope.
pub(crate) fn option_command(
    inv: &Invocation<'_>,
    verb: &str,
    window: bool,
) -> Result<Vec<Command>, TmuxError> {
    // tmux's `-s` names the server table: a session, window or pane
    // option set there is stored where nothing reads it, and shown by name
    // (recorded: after `set -s base-index 3`, `show -sv base-index` is `3`
    // and `show -gv base-index` still `0`; `show -s mouse` prints nothing),
    // and a bare `show -s` lists the server options alone.
    let got = inv.getopt();
    let base = inv.option_name().map(|n| n.split('[').next().unwrap_or(n));
    let scope = base.and_then(crate::server::dispatch::options::builtin_scope);
    let server_table =
        got.has('s') && (base.is_none() || scope.is_some_and(|s| s != cmd::OptionScope::Global));
    let mut out = inv.native(verb, &option_args(inv, window))?;
    let scope = match server_table {
        true => Some(cmd::OptionScope::Server),
        false => user_scope(inv, window),
    };
    if let Some(scope) = scope {
        for c in &mut out {
            match c {
                Command::SetOption(c) => c.scope = Some(scope),
                Command::ShowOptions(c) => c.scope = Some(scope),
                _ => {}
            }
        }
    }
    Ok(out)
}

fn set_option(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    option_command(inv, "set-option", false)
}

fn show_options(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    option_command(inv, "show-options", false)
}

/// `set-window-option` (`setw`): `set-option -w` (M36-R10).
fn set_window_option(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    option_command(inv, "set-option", true)
}

/// `show-window-options` (`showw`): `show-options -w` (M36-R10).
fn show_window_options(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    option_command(inv, "show-options", true)
}

// ---- Front-end-only verbs ------------------------------------------------

/// `list-commands [-F format] [command]`, answered from tmux's own table
/// (M36-R1: fzf probes `list-commands new-pane`). There is no native verb:
/// the text goes out through `display-message -p`, escaped, so only the
/// `command_list_*` variables are tmux's and the rest of a `-F` format
/// expands as `display -p` expands it.
// ponytail: `display -p` needs a pane to expand against, so on a server
// with no session this fails where tmux answers.
fn list_commands(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    let got = inv.getopt();
    if got.has('F') && got.value('F').is_none() {
        return Err(TmuxError(
            "command list-commands: -F expects an argument".to_string(),
        ));
    }
    let names = inv.positionals();
    if names.len() > 1 {
        return Err(ParseError::TooManyArguments("list-commands".to_string(), 1).into());
    }
    let specs: Vec<&Spec> = match names.first() {
        // Hidden commands are not listed (recorded: `list-commands info`
        // is `unknown command: info`).
        Some(name) => match resolve(name)? {
            spec if spec.hidden => return Err(TmuxError(format!("unknown command: {name}"))),
            spec => vec![spec],
        },
        None => specs().iter().filter(|s| !s.hidden).collect(),
    };
    let lines: Vec<String> = specs
        .iter()
        .map(|s| match got.value('F') {
            None => escape(s.line),
            Some(format) => format
                .replace("#{command_list_name}", &escape(s.name))
                .replace("#{command_list_alias}", &escape(s.alias.unwrap_or("")))
                .replace("#{command_list_usage}", &escape(s.usage)),
        })
        .collect();
    Ok(vec![Command::DisplayMessage(DisplayMessage {
        print: true,
        target: Target::Current,
        text: Some(lines.join("\n")),
        client: None,
        delay: None,
    })])
}

/// `text` as a format that expands to itself.
fn escape(text: &str) -> String {
    text.replace('#', "##")
}

/// `info` (`server-info`): `show-messages -JT`, as tmux 3.7b runs it
/// (recorded: `info -Z` fails as `command show-messages`).
fn server_info(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    let mut args = vec!["-J".to_string(), "-T".to_string()];
    args.extend_from_slice(inv.args);
    let out = inv.native("show-messages", &args);
    out.map_err(|e| TmuxError(e.0.replace("command server-info:", "command show-messages:")))
}

/// `args` rebuilt from `got` without `flag`, the positionals after them.
fn without(inv: &Invocation<'_>, got: &Getopt<'_>, flag: char) -> Vec<String> {
    let mut args = Vec::new();
    for (f, value) in &got.flags {
        if *f == flag {
            continue;
        }
        args.push(format!("-{f}"));
        args.extend(value.map(str::to_string));
    }
    let rest = got.rest.min(inv.args.len());
    if rest > 0 && inv.args[rest - 1] == "--" {
        args.push("--".to_string());
    }
    args.extend_from_slice(&inv.args[rest..]);
    args
}

/// `display-message -F <format>`: the format is the message (recorded:
/// giving both is tmux's `only one of -F or argument must be given`).
fn display_format(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    let got = inv.getopt();
    let Some(format) = got.value('F') else {
        return Err(ParseError::MissingValue("display-message".to_string(), 'F').into());
    };
    if !inv.positionals().is_empty() {
        return Err(TmuxError(
            "only one of -F or argument must be given".to_string(),
        ));
    }
    let mut args = without(inv, &got, 'F');
    args.push(format.to_string());
    inv.native("display-message", &args)
}

/// `start-server` inside a request the server already answers: nothing to
/// do, as in tmux (`start-server \; show-option ...`, TPM). The CLI's own
/// `start-server` is `main.rs`'s.
fn start_server(_inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    Ok(Vec::new())
}

/// `resize-pane -M` outside a mouse event: nothing, as
/// tmux 3.7b does (a stock mouse binding's drag is grofe's built-in mouse
/// model's), noted in `compat-miss.log` as ignored so it is never silent.
fn mouse_no_op(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    inv.ignored('M');
    Ok(Vec::new())
}

/// `select-pane -P <style>` (opensessions' pane flash), as tmux 3.7b runs
/// the undocumented `-P` (recorded): it selects the pane and sets the
/// pane's `window-style` and `window-active-style` to the style, unchecked;
/// the renderer draws them (B1c). `-g` (recorded) prints the pane's
/// `window-style` and selects nothing.
fn select_pane_style(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    let got = inv.getopt();
    let print = got.has('g');
    let mut out = Vec::new();
    if !print {
        out = inv.native("select-pane", &without(inv, &got, 'P'))?;
    }
    let target = |mut args: Vec<String>| {
        if let Some(target) = got.value('t') {
            args.extend(["-t".to_string(), target.to_string()]);
        }
        args
    };
    if let Some(style) = got.value('P') {
        for name in ["window-style", "window-active-style"] {
            let mut args = target(vec!["-p".to_string()]);
            args.extend([name.to_string(), style.to_string()]);
            out.extend(inv.native("set-option", &args)?);
        }
    }
    if print {
        let mut args = target(vec!["-pv".to_string()]);
        args.push("window-style".to_string());
        out.extend(inv.native("show-options", &args)?);
    }
    Ok(out)
}

// ---- The core rows -------------------------------------------------------

const UNUSED: &str = "nothing the owner runs sends it (spec 4.5)";

const fn row(
    tmux_form: &'static str,
    kind: MappingKind,
    target: GrofeTarget,
    flags: &'static [FlagMark],
    milestone: &'static str,
    golden_id: &'static str,
) -> Row {
    Row {
        tmux_form,
        kind,
        target,
        flags,
        milestone,
        golden_id,
    }
}

const fn pass(verb: &'static str, milestone: &'static str) -> Row {
    row(
        verb,
        MappingKind::PassThrough,
        GrofeTarget::Native(verb),
        &[],
        milestone,
        "",
    )
}

const fn unsupported(
    tmux_form: &'static str,
    reason: &'static str,
    flags: &'static [FlagMark],
) -> Row {
    row(
        tmux_form,
        MappingKind::Behaviour,
        GrofeTarget::Unsupported(reason),
        flags,
        "",
        "",
    )
}

const SEMANTIC: &[FlagMark] = &[FlagMark::Semantic];
const COSMETIC: &[FlagMark] = &[FlagMark::Cosmetic];

fn core_rows() -> Vec<Row> {
    use FlagMark::Cosmetic;
    use GrofeTarget::{Cli, Grammar, Translate};
    use MappingKind::{AliasOrPrefix, Behaviour, FlagRename, PassThrough};
    vec![
        // The invocation (spec §4.5), read by `cli` and `main.rs`.
        row(
            "tmux -V",
            Behaviour,
            Cli("prints `tmux 3.7b`, grofe -V grofe's own"),
            SEMANTIC,
            "M3.6",
            "fe_1/invocation",
        ),
        row(
            "tmux -L",
            PassThrough,
            Cli("a socket name in grofe's own directory"),
            SEMANTIC,
            "M1",
            "fe_1/invocation",
        ),
        row(
            "tmux -S",
            PassThrough,
            Cli("a socket path, never in tmux's directory"),
            SEMANTIC,
            "M1",
            "",
        ),
        row(
            "tmux -2",
            FlagRename,
            Cli("accepted, no effect"),
            COSMETIC,
            "M3.6",
            "fe_1/invocation",
        ),
        row(
            "tmux -u",
            FlagRename,
            Cli("accepted, no effect"),
            COSMETIC,
            "M3.6",
            "fe_1/invocation",
        ),
        row(
            "tmux",
            AliasOrPrefix,
            Cli("bare `tmux` is `new-session`"),
            &[],
            "M3.6",
            "",
        ),
        row(
            "tmux -f",
            FlagRename,
            Cli("the only config of the server it starts; /dev/null or a missing file is none"),
            SEMANTIC,
            "M3.6",
            "fe_1/invocation",
        ),
        row(
            "tmux -c",
            Behaviour,
            GrofeTarget::Unsupported("runs a shell command in place of a client; not wired"),
            SEMANTIC,
            "M3.6",
            "",
        ),
        // The target grammar (`cmd::with_tmux_targets`).
        row(
            "-t[=name]",
            FlagRename,
            Grammar("an exact session, never a prefix (SessionRef::Exact)"),
            SEMANTIC,
            "M3.6",
            "fe_1/targets",
        ),
        row(
            "-t[=]",
            FlagRename,
            Grammar(
                "fails `no mouse target` when resolved; a mouse key's commands get the pointer's target first (M36-R3)",
            ),
            SEMANTIC,
            "M3.6",
            "fe_1/targets",
        ),
        row(
            "-t[:+N]",
            FlagRename,
            Grammar("window and pane offsets, wrapping (stock `o`, the window menu)"),
            SEMANTIC,
            "M3.6",
            "fe_1/targets",
        ),
        // Out of scope (spec §4.5), before the verbs they would match.
        // tmux 3.7b's own text for a `send -M` with no mouse event
        // (recorded).
        row(
            "send-keys -M",
            Behaviour,
            GrofeTarget::Refuse(cmd::target::NO_MOUSE_TARGET),
            SEMANTIC,
            "",
            "fe_1/errors",
        ),
        // tmux 3.7b accepts this with no mouse event and does nothing,
        // exit 0 (recorded); grofe's built-in mouse model does the drag.
        // (`copy-mode -M` is TM.7's row, native since TM.6.)
        row(
            "resize-pane -M",
            Behaviour,
            Translate(mouse_no_op),
            SEMANTIC,
            "M3.6",
            "fe_4/mouse-forms",
        ),
        unsupported("pipe-pane", UNUSED, &[]),
        unsupported("lock-server", UNUSED, &[]),
        unsupported("customize-mode", UNUSED, &[]),
        unsupported("server-access", UNUSED, &[]),
        // Front-end rows.
        row(
            "display-message -F",
            FlagRename,
            Translate(display_format),
            SEMANTIC,
            "M3.6",
            "",
        ),
        row(
            "select-pane -P",
            FlagRename,
            Translate(select_pane_style),
            &[Cosmetic],
            "M3.6",
            "fe_1/errors",
        ),
        row(
            "select-pane -g",
            FlagRename,
            Translate(select_pane_style),
            SEMANTIC,
            "B1c",
            "b1c/window-style",
        ),
        row(
            "set-option",
            FlagRename,
            Translate(set_option),
            &[],
            "M3.6",
            "fe_1/options",
        ),
        row(
            "show-options",
            FlagRename,
            Translate(show_options),
            &[],
            "M3.6",
            "fe_1/options",
        ),
        row(
            "set-window-option",
            AliasOrPrefix,
            Translate(set_window_option),
            &[],
            "M3.6",
            "fe_1/options",
        ),
        row(
            "show-window-options",
            AliasOrPrefix,
            Translate(show_window_options),
            &[],
            "M3.6",
            "fe_1/options",
        ),
        row(
            "list-commands",
            Behaviour,
            Translate(list_commands),
            &[],
            "M3.6",
            "fe_1/invocation",
        ),
        row(
            "server-info",
            AliasOrPrefix,
            Translate(server_info),
            &[],
            "M3.6",
            "",
        ),
        // Pass-through: every M1 to M3 verb whose tmux form is grofe's.
        pass("new-session", "M1"),
        pass("kill-session", "M1"),
        pass("has-session", "M1"),
        pass("list-sessions", "M1"),
        pass("split-window", "M1"),
        pass("kill-pane", "M1"),
        pass("select-pane", "M1"),
        row(
            "clock-mode",
            PassThrough,
            GrofeTarget::Native("clock-mode"),
            &[],
            "B1d",
            "b1d/clock",
        ),
        pass("resize-pane", "M1"),
        pass("respawn-pane", "M1"),
        pass("list-panes", "M1"),
        pass("new-window", "M1"),
        pass("kill-window", "M1"),
        pass("select-window", "M1"),
        pass("list-windows", "M1"),
        pass("send-keys", "M1"),
        pass("capture-pane", "M1"),
        pass("display-message", "M1"),
        pass("kill-server", "M1"),
        pass("attach-session", "M2"),
        pass("detach-client", "M2"),
        pass("next-window", "M2"),
        pass("previous-window", "M2"),
        pass("last-window", "M2"),
        pass("rename-session", "M2"),
        pass("rename-window", "M2"),
        pass("switch-client", "M2"),
        pass("send-prefix", "M2"),
        pass("list-clients", "M2"),
        pass("bind-key", "M3"),
        pass("unbind-key", "M3"),
        pass("list-keys", "M3"),
        pass("source-file", "M3"),
        pass("command-prompt", "M3"),
        pass("run-shell", "M3"),
        pass("select-layout", "M3"),
        row(
            "start-server",
            PassThrough,
            Translate(start_server),
            &[],
            "M1",
            "",
        ),
    ]
}

// ---- The CLI ---------------------------------------------------------------

/// What the CLI run as `tmux` sends: the socket its global flags name, and
/// the request, in the dialect it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxCli {
    /// `-S <path>`.
    pub socket: Option<PathBuf>,
    /// `-L <name>`.
    pub name: Option<String>,
    /// The request, `argv[0]` its verb, handled by `main.rs` as a grofe
    /// argv is (`attach`, `new` without `-d`, a one-shot command).
    pub argv: Vec<String>,
    pub dialect: Dialect,
    /// `-V`: print `tmux <compat level>` and exit, before anything else.
    pub version: bool,
    /// `-f <file>`, each one given: the config a server this call starts
    /// loads, alone (`compat::start_config`).
    pub files: Vec<PathBuf>,
    /// `-c <shell-command>`: run it with the default shell instead of a
    /// command (tmux-fzf's `menu.sh`). The last `-c` wins, as in tmux.
    pub shell_command: Option<String>,
}

/// tmux 3.7b's usage text, printed after a bad global option.
const USAGE: &str = "usage: tmux [-2CDhlNuVv] [-c shell-command] [-f file] [-L socket-name]
            [-S socket-path] [-T features] [command [flags]]";

/// tmux's other global flags, those taking a value first. Valid for tmux,
/// not implemented in grofe.
const OTHER_WITH_VALUE: &[char] = &['T'];
const OTHER_FLAGS: &[char] = &['C', 'D', 'h', 'l', 'N', 'q', 'U', 'v'];

/// Reads the argv of the CLI run as `tmux` (`args` follow `argv[0]`), or
/// the error to print and exit 1 with. Global flags are read as tmux's
/// `getopt` reads them (`-L x`, `-Lx`, `-f x`, `-2u`, `-V`, `--`). Sockets stay in
/// grofe's directory (spec D12), so `-L name` never reaches tmux's own
/// `/tmp/tmux-$UID`. `-2` and `-u` are accepted with no effect. The first
/// command's verb goes out by its full tmux name, so `tmux a` reaches
/// `main.rs`'s attach as `attach-session`; a verb that does not resolve is
/// left for the server to refuse, as tmux's server does.
pub fn cli(args: &[String]) -> Result<TmuxCli, String> {
    let mut out = TmuxCli {
        socket: None,
        name: None,
        argv: Vec::new(),
        dialect: Dialect::Tmux,
        version: false,
        files: Vec::new(),
        shell_command: None,
    };
    let mut i = 0;
    'args: while let Some(arg) = args.get(i) {
        i += 1;
        if arg == "--" {
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) else {
            i -= 1;
            break;
        };
        for (at, flag) in flags.char_indices() {
            match flag {
                'L' | 'S' | 'f' | 'c' => {
                    let attached = &flags[at + 1..];
                    let value = if attached.is_empty() {
                        i += 1;
                        args.get(i - 1).cloned().ok_or_else(|| {
                            format!("tmux: option requires an argument -- {flag}\n{USAGE}")
                        })?
                    } else {
                        attached.to_string()
                    };
                    // A socket name is a file name in grofe's own socket
                    // directory: a path separator could leave it.
                    if flag == 'L' && value.contains('/') {
                        return Err(format!(
                            "grofe: refusing -L {value}: a socket name has no path separator"
                        ));
                    }
                    match flag {
                        'L' => out.name = Some(value),
                        'S' => out.socket = Some(PathBuf::from(value)),
                        'c' => out.shell_command = Some(value),
                        _ => out.files.push(PathBuf::from(value)),
                    }
                    continue 'args;
                }
                // tmux prints its version as soon as getopt meets `-V`.
                'V' => {
                    out.version = true;
                    return Ok(out);
                }
                '2' | 'u' => {}
                c if OTHER_WITH_VALUE.contains(&c) || OTHER_FLAGS.contains(&c) => {
                    return Err(format!("tmux front end is not implemented: -{c}"));
                }
                c => return Err(format!("tmux: unknown option -- {c}\n{USAGE}")),
            }
        }
    }
    out.argv = args[i..].to_vec();
    // tmux 3.7b: `-c` takes no command after it (recorded: usage, exit 1).
    if out.shell_command.is_some() {
        if !out.argv.is_empty() {
            return Err(USAGE.to_string());
        }
        return Ok(out);
    }
    // tmux 3.7b's client packs the command into one message (recorded:
    // up to 16364 bytes, a NUL after each argument, goes; to 16380 is
    // `failed to send command`; past that `command too long`).
    let size: usize = out.argv.iter().map(|a| a.len() + 1).sum();
    if size > 16380 {
        return Err("command too long".to_string());
    }
    if size > 16364 {
        return Err("failed to send command".to_string());
    }
    // Bare `tmux` is `new-session`, as in tmux 3.7b.
    if out.argv.is_empty() {
        out.argv.push("new-session".to_string());
    }
    if let Ok(spec) = resolve(&out.argv[0]) {
        out.argv[0] = spec.name.to_string();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{ListSessions, SessionRef, SetOption, ShowMessages, ShowOptions, WindowRef};

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn ctx() -> ClientCtx {
        ClientCtx {
            dialect: Dialect::Tmux,
            ..crate::server::test_ctx()
        }
    }

    fn one_or_none(words: &[&str]) -> Result<Vec<Command>, TmuxError> {
        dispatch(&args(words), &ctx())
    }

    fn one(words: &[&str]) -> Result<Command, TmuxError> {
        let mut got = dispatch(&args(words), &ctx())?;
        assert_eq!(got.len(), 1, "{got:?}");
        Ok(got.remove(0))
    }

    #[test]
    fn the_command_table_is_tmux_s_91_plus_the_hidden_one() {
        let specs = specs();
        assert_eq!(specs.iter().filter(|s| !s.hidden).count(), 91);
        let attach = resolve("attach-session").unwrap();
        assert_eq!(attach.alias, Some("attach"));
        assert!(attach.takes_value('t') && attach.takes_value('c'));
        assert!(!attach.takes_value('d'));
        assert!(resolve("wait-for").unwrap().usage.starts_with("[-L|-S|-U]"));
        assert!(!resolve("wait-for").unwrap().takes_value('L'));
        assert!(resolve("select-pane").unwrap().takes_value('P'));
    }

    /// Every alias in the golden resolves to its command, as does every
    /// full name.
    #[test]
    fn every_alias_and_name_resolves() {
        for spec in specs() {
            assert_eq!(resolve(spec.name).unwrap().name, spec.name);
            if let Some(alias) = spec.alias {
                assert_eq!(resolve(alias).unwrap().name, spec.name, "{alias}");
            }
        }
        assert_eq!(resolve("info").unwrap().name, "server-info");
    }

    /// Recorded on tmux 3.7b (`-L grofe-fe1 -f /dev/null`).
    #[test]
    fn prefixes_resolve_as_tmux_s() {
        assert_eq!(resolve("a").unwrap().name, "attach-session");
        assert_eq!(resolve("new-s").unwrap().name, "new-session");
        assert_eq!(resolve("last").unwrap().name, "last-window");
        assert_eq!(resolve("lastp").unwrap().name, "last-pane");
        assert_eq!(
            resolve("kill-s").unwrap_err().0,
            "ambiguous command: kill-s, could be: kill-server, kill-session"
        );
        assert_eq!(
            resolve("list").unwrap_err().0,
            "ambiguous command: list, could be: list-buffers, list-clients, list-commands, \
             list-keys, list-panes, list-sessions, list-windows"
        );
        assert!(resolve("se").unwrap_err().0.starts_with(
            "ambiguous command: se, could be: select-layout, select-pane, select-window, \
             send-keys, send-prefix, server-access, set-buffer,"
        ));
        assert_eq!(resolve("nope").unwrap_err().0, "unknown command: nope");
    }

    #[test]
    fn getopt_reads_as_tmux_s() {
        let spec = resolve("new-session").unwrap();
        let a = args(&["-ds", "x", "-c/tmp", "--", "-t", "cmd"]);
        let inv = Invocation {
            verb: spec.name,
            args: &a,
            ctx: &ctx(),
            spec,
        };
        let got = inv.getopt();
        assert_eq!(
            got.flags,
            vec![('d', None), ('s', Some("x")), ('c', Some("/tmp"))]
        );
        assert_eq!(inv.positionals(), &a[4..]);
    }

    #[test]
    fn rows_are_well_formed_and_every_form_verb_is_tmux_s() {
        for row in table() {
            let verb = row.verb();
            if !row.tmux_form.starts_with("tmux") && !verb.is_empty() {
                assert_eq!(resolve(verb).map(|s| s.name), Ok(verb), "{}", row.tmux_form);
            }
            assert_eq!(
                row.form_flags().len(),
                row.flags.len(),
                "{}: one mark per flag",
                row.tmux_form
            );
        }
        let r = table().iter().find(|r| r.tmux_form == "-t[=]").unwrap();
        assert_eq!(r.form_flags(), vec![('t', Some("="))]);
    }

    #[test]
    fn m1_to_m3_verbs_pass_through_by_alias_and_prefix() {
        assert_eq!(
            one(&["ls"]).unwrap(),
            Command::ListSessions(ListSessions::default())
        );
        assert_eq!(
            one(&["list-s"]).unwrap(),
            Command::ListSessions(ListSessions::default())
        );
        let got = dispatch(&args(&["ls", ";", "kill-server"]), &ctx()).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn exact_targets_and_offsets_reach_the_native_command() {
        let Command::HasSession(c) = one(&["has", "-t", "=x"]).unwrap() else {
            panic!()
        };
        assert_eq!(c.target, Target::Session(SessionRef::Bare("=x".into())));
        let Command::SelectWindow(c) = one(&["selectw", "-t", "=x:2"]).unwrap() else {
            panic!()
        };
        assert_eq!(
            c.target,
            Target::Window(SessionRef::Exact("x".into()), WindowRef::Index(2))
        );
        // A bare `=` binds, and fails when it resolves (tmux's order).
        let Command::HasSession(c) = one(&["has-session", "-t", "="]).unwrap() else {
            panic!()
        };
        assert_eq!(c.target, Target::Session(SessionRef::Mouse));
        assert!(one(&["select-pane", "-t", ":.+"]).is_ok());
    }

    #[test]
    fn set_and_show_take_tmux_names_and_window_forms() {
        // `setw -g` is native `set -w -g` (`set -gw`).
        assert_eq!(
            one(&["setw", "-g", "mode-keys", "vi"]).unwrap(),
            cmd::parse(&args(&["set-option", "-gw", "mode-keys", "vi"])).unwrap()
        );
        let Command::ShowOptions(ShowOptions {
            value_only, quiet, ..
        }) = one(&["show", "-gqv", "base-index"]).unwrap()
        else {
            panic!()
        };
        assert!(value_only && quiet);
        assert_eq!(
            one(&["showw", "-v", "remain-on-exit"]).unwrap(),
            cmd::parse(&args(&["show-options", "-wv", "remain-on-exit"])).unwrap()
        );
        assert!(matches!(
            one(&["set", "-a", "status-format[1]", "x"]).unwrap(),
            Command::SetOption(SetOption { append: true, .. })
        ));
        assert_eq!(
            one(&["set-window-option"]).unwrap_err().0,
            "command set-window-option: too few arguments (need at least 1)"
        );
        // tmux's server scope is grofe's global one.
        assert_eq!(
            one(&["set", "-as", "terminal-features", ",x:RGB"]).unwrap(),
            cmd::parse(&args(&[
                "set-option",
                "-a",
                "-g",
                "terminal-features",
                ",x:RGB"
            ]))
            .unwrap()
        );
        assert_eq!(
            one(&["show", "-sv", "escape-time"]).unwrap(),
            cmd::parse(&args(&["show-options", "-gv", "escape-time"])).unwrap()
        );
        assert_eq!(resolve("set-option").unwrap().min_args(), 1);
        assert_eq!(resolve("if-shell").unwrap().min_args(), 2);
        assert_eq!(resolve("command-prompt").unwrap().min_args(), 0);
        assert_eq!(store_name("status-format[2]"), "status-format[2]");
        assert_eq!(store_name("@x"), "@x");
        assert_eq!(store_name("nope"), "nope");
    }

    /// The option table covers every option grofe has, and only those.
    #[test]
    fn the_option_table_is_the_option_store_s() {
        use crate::server::dispatch::options::{builtin_names, list_names};
        let store: Vec<&str> = builtin_names().chain(list_names()).collect();
        for name in &store {
            assert!(
                option_name(name).is_some(),
                "{name} has no OPTION_NAMES row"
            );
        }
        for (tmux, internal) in OPTION_NAMES {
            assert!(
                store.contains(internal),
                "{tmux} maps to unknown {internal}"
            );
        }
    }

    #[test]
    fn list_commands_answers_from_the_golden() {
        let Command::DisplayMessage(c) = one(&["lscm", "has"]).unwrap() else {
            panic!()
        };
        assert!(c.print);
        assert_eq!(
            c.text.as_deref(),
            Some("has-session (has) [-t target-session]")
        );
        let Command::DisplayMessage(c) = one(&[
            "list-commands",
            "-F",
            "#{command_list_name}|#{command_list_alias}|#{session_name}",
            "attach",
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            c.text.as_deref(),
            Some("attach-session|attach|#{session_name}")
        );
        let Command::DisplayMessage(c) = one(&["list-commands"]).unwrap() else {
            panic!()
        };
        assert_eq!(c.text.unwrap().lines().count(), 91);
        assert_eq!(
            one(&["lscm", "nope"]).unwrap_err().0,
            "unknown command: nope"
        );
        assert_eq!(
            one(&["lscm", "a", "b"]).unwrap_err().0,
            "command list-commands: too many arguments (need at most 1)"
        );
    }

    #[test]
    fn info_is_show_messages_jt() {
        assert_eq!(
            one(&["info"]).unwrap(),
            Command::ShowMessages(ShowMessages {
                client: None,
                jobs: true,
                terminals: true,
            })
        );
    }

    #[test]
    fn select_pane_style_selects_and_sets_the_pane_styles() {
        let got = dispatch(
            &args(&["select-pane", "-P", "bg=colour238", "-t", "%1"]),
            &ctx(),
        )
        .unwrap();
        let [
            Command::SelectPane(select),
            Command::SetOption(style),
            Command::SetOption(active),
        ] = &got[..]
        else {
            panic!("{got:?}")
        };
        let pane = cmd::target::parse_target("%1").unwrap();
        assert_eq!(select.target, pane);
        for (set, name) in [(style, "window-style"), (active, "window-active-style")] {
            assert_eq!(
                (
                    set.scope,
                    &set.target,
                    set.name.as_str(),
                    set.value.as_deref()
                ),
                (
                    Some(cmd::OptionScope::Pane),
                    &pane,
                    name,
                    Some("bg=colour238")
                )
            );
        }
        assert_eq!(
            one(&["select-pane", "-t", "%1", "-P"]).unwrap_err().0,
            "command select-pane: -P expects an argument"
        );
    }

    /// The most specific row decides, wherever it is registered: a
    /// flagless `new-session` row ahead of the core rows (as an area row
    /// would be) does not swallow the refusal of `new-session -t`.
    #[test]
    fn the_most_specific_row_decides() {
        let mut rows = vec![pass("new-session", "M1")];
        rows.push(unsupported("new-session -t", "test", SEMANTIC));
        let got = translate_with(&args(&["new-session", "-t", "x"]), &ctx(), &rows, &[]);
        assert!(got.unwrap_err().0.contains("not supported"));
        assert!(translate_with(&args(&["new-session", "-d"]), &ctx(), &rows, &[]).is_ok());
        // Between rows as specific, the earlier one.
        let rows = vec![
            unsupported("new-session", "first", &[]),
            pass("new-session", "M1"),
        ];
        let err = translate_with(&args(&["new-session"]), &ctx(), &rows, &[]).unwrap_err();
        assert_eq!(err.0, "new-session: not supported (first)");
        // A pinned positional counts as much as a flag.
        let rows = vec![
            pass("list-commands", "M3.6"),
            unsupported("list-commands new-pane", "pinned", &[]),
        ];
        let err = translate_with(&args(&["lscm", "new-pane"]), &ctx(), &rows, &[]);
        assert!(err.unwrap_err().0.contains("pinned"));
    }

    /// A registered rewrite applies before any row matches, so a row
    /// that pins the rewritten value sees it.
    #[test]
    fn rewrites_apply_before_the_rows() {
        fn short_forms(_: &Invocation<'_>, v: &str) -> Result<Rewritten, TmuxError> {
            Ok(Rewritten::value(v.replace("#S", "#{session_name}")))
        }
        let registered = [
            Rewrite {
                verb: "display-message",
                arg: Arg::Positional(0),
                rewrite: short_forms,
            },
            Rewrite {
                verb: "list-sessions",
                arg: Arg::Flag('F'),
                rewrite: short_forms,
            },
        ];
        let mut rows = vec![unsupported(
            "list-sessions -F[#{session_name}]",
            "pinned after the rewrite",
            SEMANTIC,
        )];
        rows.extend(core_rows());
        let got =
            translate_with(&args(&["display", "-p", "#S!"]), &ctx(), &rows, &registered).unwrap();
        let [Command::DisplayMessage(d)] = &got[..] else {
            panic!("{got:?}")
        };
        assert_eq!(d.text.as_deref(), Some("#{session_name}!"));
        let err = translate_with(&args(&["ls", "-F#S"]), &ctx(), &rows, &registered).unwrap_err();
        assert!(err.0.contains("pinned after the rewrite"), "{err}");
        // With no rewrite for its verb, a command is left as sent.
        let got = translate_with(&args(&["lsw", "-F", "#S"]), &ctx(), &rows, &registered).unwrap();
        let [Command::ListWindows(w)] = &got[..] else {
            panic!("{got:?}")
        };
        assert_eq!(w.format.as_deref(), Some("#S"));
    }

    /// FE.3's `#(cmd)`: a rewrite of `set`'s value, seeing the option it
    /// is for, registers the source first and leaves the option reading
    /// it.
    #[test]
    fn a_rewrite_registers_a_source_before_its_set() {
        fn jobs(inv: &Invocation<'_>, v: &str) -> Result<Rewritten, TmuxError> {
            assert_eq!(inv.option_name(), Some("status-right"));
            let Some(job) = v.strip_prefix("#(").and_then(|j| j.strip_suffix(')')) else {
                return Ok(Rewritten::value(v));
            };
            let key = crate::server::sources::inline_key(job);
            Ok(Rewritten {
                value: format!("#{{@{key}}}"),
                before: vec![Command::RegisterSource(cmd::RegisterSource {
                    name: None,
                    command: job.to_string(),
                })],
            })
        }
        let registered = [Rewrite {
            verb: "set-option",
            arg: Arg::Positional(1),
            rewrite: jobs,
        }];
        let got = translate_with(
            &args(&["set", "-g", "status-right", "#(date)"]),
            &ctx(),
            &core_rows(),
            &registered,
        )
        .unwrap();
        let [Command::RegisterSource(source), Command::SetOption(set)] = &got[..] else {
            panic!("{got:?}")
        };
        assert_eq!(
            (source.name.as_deref(), source.command.as_str()),
            (None, "date")
        );
        let key = crate::server::sources::inline_key("date");
        assert_eq!(set.value.as_deref(), Some(format!("#{{@{key}}}").as_str()));
        assert_eq!(set.name, "status-right");
        assert_eq!(
            cmd::parse(&args(&["__register-source", "-n", "batt", "acpi"])).unwrap(),
            Command::RegisterSource(cmd::RegisterSource {
                name: Some("batt".into()),
                command: "acpi".into(),
            })
        );
    }

    /// A refusal for good wins over any mapping for its verb, however many
    /// flags the mapping names and wherever it is registered; a pinned
    /// value outranks a bare flag.
    #[test]
    fn refusals_and_pinned_values_come_first() {
        let two_flags = row(
            "new-session -d -s",
            MappingKind::PassThrough,
            GrofeTarget::Native("new-session"),
            &[FlagMark::Semantic, FlagMark::Semantic],
            "M3.6",
            "",
        );
        let mut rows = vec![two_flags];
        rows.push(unsupported("new-session -t", "a refusal", SEMANTIC));
        let err = translate_with(
            &args(&["new", "-d", "-s", "x", "-t", "y"]),
            &ctx(),
            &rows,
            &[],
        );
        assert!(err.unwrap_err().0.contains("not supported"));
        assert!(translate_with(&args(&["new", "-d", "-s", "x"]), &ctx(), &rows, &[]).is_ok());

        // A mouse refusal, in either registration order.
        let map = row(
            "resize-pane -M -Z",
            MappingKind::PassThrough,
            GrofeTarget::Native("resize-pane"),
            &[FlagMark::Semantic, FlagMark::Semantic],
            "M3.6",
            "",
        );
        let refuse = unsupported("resize-pane -M", "a mouse form", SEMANTIC);
        for rows in [vec![map.clone(), refuse.clone()], vec![refuse, map]] {
            let err = translate_with(&args(&["resize-pane", "-M", "-Z"]), &ctx(), &rows, &[]);
            assert!(err.unwrap_err().0.contains("mouse"), "{rows:?}");
        }

        // `-x[M]` over a bare `-x` with more flags.
        let bare = unsupported("display-menu -x -y", "bare", &[]);
        let bare = Row {
            milestone: "M3.6",
            ..bare
        };
        let pinned = Row {
            milestone: "M3.6",
            ..unsupported("display-menu -x[M]", "pinned", &[])
        };
        let err = translate_with(
            &args(&["menu", "-x", "M", "-y", "1", "a", "b", "c"]),
            &ctx(),
            &[bare, pinned],
            &[],
        );
        assert!(err.unwrap_err().0.contains("pinned"));
        // M5's copy refusal ranks as a mapping: M5 can take it over.
        let m5 = row(
            "copy-mode",
            MappingKind::PassThrough,
            GrofeTarget::Unsupported("mapped by M5"),
            &[],
            "M5",
            "",
        );
        let mut rows = vec![m5];
        rows.extend(core_rows());
        let err = translate_with(&args(&["copy-mode"]), &ctx(), &rows, &[]).unwrap_err();
        assert!(err.0.contains("mapped by M5"), "{err}");
    }

    /// A refusal carries tmux's own text; an area row refusing the same
    /// form replaces the core refusal, and a mapping never does.
    #[test]
    fn an_area_refusal_replaces_a_core_one() {
        assert_eq!(one(&["send", "-M"]).unwrap_err().0, "no mouse target");
        let area = row(
            "resize-pane -M",
            MappingKind::Behaviour,
            GrofeTarget::Refuse("tmux's own text"),
            SEMANTIC,
            "",
            "",
        );
        let mut rows = vec![area];
        rows.extend(core_rows());
        let err = translate_with(&args(&["resize-pane", "-M"]), &ctx(), &rows, &[]);
        assert_eq!(err.unwrap_err().0, "tmux's own text");
        let map = row(
            "pipe-pane",
            MappingKind::PassThrough,
            GrofeTarget::Native("pipe-pane"),
            SEMANTIC,
            "M3.6",
            "",
        );
        let mut rows = vec![map];
        rows.extend(core_rows());
        let err = translate_with(&args(&["pipe-pane"]), &ctx(), &rows, &[]);
        assert!(err.unwrap_err().0.contains("not supported"));
        // tmux 3.7b's own answer to these outside a mouse event: nothing.
        assert_eq!(one_or_none(&["resize-pane", "-M"]), Ok(Vec::new()));
        // TM.7 maps `copy-mode -M` onto the native flag, which does
        // nothing outside a mouse key, as in tmux.
        assert!(matches!(
            one(&["copy-mode", "-M"]),
            Ok(Command::CopyMode(cmd::CopyMode { mouse: true, .. }))
        ));
        assert!(row_for(&args(&["send", "-M"])).is_some_and(Row::refuses));
    }

    /// Recorded on tmux 3.7b: flags are checked against tmux's usage.
    #[test]
    fn flags_tmux_lacks_are_refused_as_tmux_refuses_them() {
        assert_eq!(
            one(&["showw", "-p"]).unwrap_err().0,
            "command show-window-options: unknown flag -p"
        );
        assert_eq!(
            one(&["start-server", "-x"]).unwrap_err().0,
            "command start-server: unknown flag -x"
        );
        assert_eq!(
            one(&["kill-session", "-t"]).unwrap_err().0,
            "command kill-session: -t expects an argument"
        );
        assert_eq!(
            one(&["lscm", "info"]).unwrap_err().0,
            "unknown command: info"
        );
        assert_eq!(dispatch(&args(&[";"]), &ctx()), Ok(Vec::new()));
    }

    /// tmux keeps server, global session and global window user options
    /// apart; builtins stay in grofe's one global scope.
    #[test]
    fn user_options_keep_their_tmux_scope() {
        let scope = |words: &[&str]| match one(words).unwrap() {
            Command::SetOption(c) => c.scope,
            Command::ShowOptions(c) => c.scope,
            other => panic!("{other:?}"),
        };
        use cmd::OptionScope::{Global, GlobalWindow, Server};
        assert_eq!(scope(&["set", "-s", "@s", "1"]), Some(Server));
        assert_eq!(scope(&["show", "-sv", "@s"]), Some(Server));
        assert_eq!(scope(&["set", "-gw", "@w", "1"]), Some(GlobalWindow));
        assert_eq!(scope(&["setw", "-g", "@w", "1"]), Some(GlobalWindow));
        assert_eq!(scope(&["showw", "-gv", "@w"]), Some(GlobalWindow));
        assert_eq!(scope(&["set", "-g", "@g", "1"]), Some(Global));
        assert_eq!(scope(&["set", "-s", "escape-time", "0"]), Some(Global));
        assert_eq!(scope(&["setw", "-g", "mode-keys", "vi"]), Some(Global));
    }

    #[test]
    fn a_socket_name_has_no_path_separator() {
        let err = cli(&args(&["-L", "a/b", "ls"])).unwrap_err();
        assert!(err.contains("refusing -L a/b"), "{err}");
        assert!(cli(&args(&["-L../x", "ls"])).is_err());
    }

    #[test]
    fn cli_reads_sockets_and_leaves_the_request_for_the_transport() {
        let got = cli(&args(&["-L", "work", "new-session", "-d", "-L", "x"])).unwrap();
        assert_eq!(got.name.as_deref(), Some("work"));
        assert_eq!(got.socket, None);
        assert_eq!(got.argv, args(&["new-session", "-d", "-L", "x"]));
        assert_eq!(got.dialect, Dialect::Tmux);
        assert!(!got.version);

        let got = cli(&args(&["-S/tmp/s.sock", "-Lname", "ls"])).unwrap();
        assert_eq!(got.socket, Some(PathBuf::from("/tmp/s.sock")));
        assert_eq!(got.name.as_deref(), Some("name"));
        assert_eq!(got.argv, args(&["list-sessions"]));

        let got = cli(&args(&["--", "-x"])).unwrap();
        assert_eq!(got.argv, args(&["-x"]));
        let got = cli(&args(&["-L", "x"])).unwrap();
        assert_eq!(got.argv, args(&["new-session"]));
    }

    #[test]
    fn cli_names_the_first_verb_in_full() {
        let got = cli(&args(&["a", "-t", "x"])).unwrap();
        assert_eq!(got.argv, args(&["attach-session", "-t", "x"]));
        let got = cli(&args(&["new", "-d"])).unwrap();
        assert_eq!(got.argv, args(&["new-session", "-d"]));
        // An unknown or ambiguous verb is the server's to refuse.
        assert_eq!(cli(&args(&["s"])).unwrap().argv, args(&["s"]));
    }

    #[test]
    fn cli_accepts_2_and_u_with_no_effect() {
        let got = cli(&args(&["-2", "-u", "-L", "x", "ls"])).unwrap();
        assert_eq!(got.name.as_deref(), Some("x"));
        assert_eq!(got.argv, args(&["list-sessions"]));
        let got = cli(&args(&["-2uLx", "ls"])).unwrap();
        assert_eq!(got.name.as_deref(), Some("x"));
        assert!(cli(&args(&["-2V"])).unwrap().version);
    }

    #[test]
    fn cli_version_wins_as_soon_as_it_is_read() {
        for argv in [
            &["-V"][..],
            &["-V", "-Z"],
            &["-L", "x", "-V"],
            &["-SV", "-V"],
        ] {
            let got = cli(&args(argv)).unwrap();
            assert!(got.version, "{argv:?}");
        }
        // `-V` as `-L`'s value is a socket name.
        assert!(!cli(&args(&["-L", "-V"])).unwrap().version);
    }

    /// The texts are tmux 3.7b's, recorded on a private socket.
    #[test]
    fn cli_errors_are_tmux_s() {
        let err = cli(&args(&["-Z"])).unwrap_err();
        assert!(err.starts_with("tmux: unknown option -- Z\nusage: tmux [-2CDhlNuVv]"));
        let err = cli(&args(&["-L"])).unwrap_err();
        assert!(err.starts_with("tmux: option requires an argument -- L\nusage: tmux"));
        let err = cli(&args(&["-T", "x"])).unwrap_err();
        assert_eq!(err, "tmux front end is not implemented: -T");
        let got = cli(&args(&["-c", "echo a", "-cecho b"])).unwrap();
        assert_eq!(got.shell_command.as_deref(), Some("echo b"));
        assert!(got.argv.is_empty());
        let err = cli(&args(&["-c", "true", "ls"])).unwrap_err();
        assert!(err.starts_with("usage: tmux [-2CDhlNuVv]"), "{err}");
        let got = cli(&args(&["-L", "x", "-f", "/dev/null", "ls"])).unwrap();
        assert_eq!(got.files, [PathBuf::from("/dev/null")]);
        let got = cli(&args(&["-f", "a.toml", "-fb.toml", "ls"])).unwrap();
        assert_eq!(
            got.files,
            [PathBuf::from("a.toml"), PathBuf::from("b.toml")]
        );
        assert_eq!(got.argv, args(&["list-sessions"]));
        assert_eq!(
            cli(&args(&["-f/a.toml"])).unwrap().files,
            [PathBuf::from("/a.toml")]
        );
    }
}
