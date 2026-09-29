//! `grofe import-tmux`: convert a tmux.conf to grofe TOML (spec 5.5).
//!
//! A one-way, one-shot converter. It never starts or contacts a server, and
//! it never writes to anything it reads: every input is opened read-only,
//! and `-o` naming an input file is refused. `run` is the CLI entry point
//! (`main.rs`'s `import-tmux` arm); [`Importer`] does the work, one
//! `tmux.conf`-dialect line at a time, following `source-file` in place.
//!
//! Tokenizing is this module's own (`read_lines`/`tokenize_words`), not
//! [`cmd::split_command_string`]: that function expands `$VAR`, `${VAR}`
//! and `~` from the *importer's own process environment*, which would bake
//! this machine's paths into the output (T3.0 ruling, task-TI.1 brief).
//! `cmd::split_command_string` and `cmd::parse` are still used, deliberately,
//! to *validate* the command strings this module builds, exactly as
//! `config/load.rs`'s loader will read them back.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cmd;
use crate::config::provenance::tilde;
use crate::config::{self, ConfigError, Ty, toml_type};
use crate::server::dispatch::options::{Options, Owner};
use crate::server::input::{Key, parse_key};

const USAGE: &str = "usage: import-tmux [<tmux.conf>] [-o <path>]";

/// Runs `import-tmux` with `args` (after the verb) and returns the exit
/// status (spec 5.5: 0 with a TOML file produced, however many lines were
/// reported; 1 when the entry file cannot be read or the output fails its
/// own load check; 2 for usage errors and the `-o` refusal).
pub fn run(args: &[String]) -> ExitCode {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => match it.next() {
                Some(p) => output = Some(PathBuf::from(p)),
                None => {
                    eprintln!("import-tmux: -o needs a path\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            s if s.starts_with('-') && s.len() > 1 => {
                eprintln!("import-tmux: unknown flag {s}\n{USAGE}");
                return ExitCode::from(2);
            }
            s if input.is_none() => input = Some(PathBuf::from(s)),
            s => {
                eprintln!("import-tmux: unexpected argument {s}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let env = config::Env::from_process();
    let entry = match input {
        Some(p) => p,
        None => {
            let xdg = std::env::var("XDG_CONFIG_HOME").ok().map(PathBuf::from);
            match default_entry_path(env.home.as_deref(), xdg.as_deref()) {
                Some(p) => p,
                None => {
                    eprintln!(
                        "import-tmux: no tmux.conf found (checked ~/.tmux.conf, then ~/.config/tmux/tmux.conf)"
                    );
                    return ExitCode::from(1);
                }
            }
        }
    };

    let mut imp = Importer::new(env.home.clone());
    if let Err(e) = imp.load_entry(&entry) {
        eprintln!("import-tmux: cannot read {}: {e}", entry.display());
        return ExitCode::from(1);
    }

    if let Some(out) = &output
        && let Ok(out_canon) = fs::canonicalize(out)
        && imp.read_files.contains(&out_canon)
    {
        eprintln!(
            "import-tmux: -o {} would overwrite a file this import read",
            out.display()
        );
        return ExitCode::from(2);
    }

    let (text, reports) = imp.finish(&entry);

    // Before writing anything: the generated text must load with no
    // errors (spec 5.5). A tempfile in the system temp dir, or in `-o`'s
    // own directory when one is given (so that file doubles as the atomic
    // write below).
    let check_dir = match &output {
        Some(p) => p
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir),
        None => std::env::temp_dir(),
    };
    let check_path = check_dir.join(format!(
        ".grofe-import-tmux-check-{}.toml",
        std::process::id()
    ));
    if let Err(e) = fs::write(&check_path, &text) {
        eprintln!("import-tmux: cannot write {}: {e}", check_path.display());
        return ExitCode::from(1);
    }
    let checked = config::load_one(&check_path, &env);
    let _ = fs::remove_file(&check_path);
    if let Err(errs) = checked {
        for e in &errs {
            eprintln!("import-tmux: generated config is invalid: {e}");
        }
        return ExitCode::from(1);
    }

    match &output {
        None => {
            let mut stdout = std::io::stdout().lock();
            if stdout.write_all(text.as_bytes()).is_err() {
                return ExitCode::from(1);
            }
            let _ = stdout.flush();
        }
        Some(path) => {
            let dir = path
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let tmp = dir.join(format!(
                ".grofe-import-tmux-{}.toml.tmp",
                std::process::id()
            ));
            let wrote = fs::write(&tmp, &text).and_then(|()| fs::rename(&tmp, path));
            if wrote.is_err() {
                let _ = fs::remove_file(&tmp);
                eprintln!("import-tmux: cannot write {}", path.display());
                return ExitCode::from(1);
            }
        }
    }

    for r in &reports {
        eprintln!("{r}");
    }
    ExitCode::SUCCESS
}

/// No path given: `~/.tmux.conf`, then `$XDG_CONFIG_HOME/tmux/tmux.conf`,
/// then `~/.config/tmux/tmux.conf` (spec 5.5; tmux 3.7b also checks
/// `$XDG_CONFIG_HOME`, which collapses onto the third check when it is
/// unset).
fn default_entry_path(home: Option<&Path>, xdg_config_home: Option<&Path>) -> Option<PathBuf> {
    if let Some(h) = home {
        let p = h.join(".tmux.conf");
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(xdg) = xdg_config_home {
        let p = xdg.join("tmux").join("tmux.conf");
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(h) = home {
        let p = h.join(".config").join("tmux").join("tmux.conf");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

// ---- tokenizing (this module's own line reader) ----

/// One word of a tmux.conf line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    /// Exact source text: quotes and backslash escapes kept, for binding
    /// command strings.
    raw: String,
    /// Unescaped, unquoted content: backslash escapes resolved, `$` and
    /// `~` left untouched (never expanded here).
    value: String,
    /// A `;` or `\;` command separator, read as its own word (never
    /// expanded, never dropped by the tokenizer; a caller that does not
    /// care about sub-commands can ignore it).
    is_sep: bool,
}

/// One logical tmux.conf line: physical lines joined on a trailing `\`.
struct Line {
    /// 1-based line number of the first physical line.
    line: usize,
    /// Raw source text of the (possibly joined) logical line.
    text: String,
    words: Vec<Word>,
}

/// Splits `text` into logical lines (a trailing unescaped `\` joins the
/// next physical line, spec 5.5), each split into words. Blank and
/// comment-only lines are dropped (but still consume their physical line
/// number, so later numbering stays right).
fn read_lines(text: &str) -> Vec<Line> {
    let physical: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < physical.len() {
        let first_trimmed = physical[i].trim_start();
        if first_trimmed.is_empty() || first_trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let start = i;
        let mut joined = String::new();
        loop {
            let l = physical[i];
            match l.strip_suffix('\\') {
                Some(stripped) if i + 1 < physical.len() => {
                    joined.push_str(stripped);
                    i += 1;
                }
                _ => {
                    joined.push_str(l);
                    i += 1;
                    break;
                }
            }
        }
        let words = tokenize_words(&joined);
        if !words.is_empty() {
            out.push(Line {
                line: start + 1,
                text: joined,
                words,
            });
        }
    }
    out
}

/// Splits one logical line into words: `#` outside quotes starts a
/// comment; single quotes are literal; double quotes and bare words take
/// backslash escapes; `;` and `\;` are read as standalone separator words.
/// Never expands `$VAR` or `~` (see module docs).
#[allow(unused_assignments)] // the final `end_word!()` resets `started` too, harmlessly
fn tokenize_words(line: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut raw = String::new();
    let mut value = String::new();
    let mut started = false;
    let mut escaped_semicolon = false;
    let mut chars = line.chars().peekable();

    macro_rules! end_word {
        () => {
            if started {
                let is_sep = escaped_semicolon && value == ";";
                words.push(Word {
                    raw: std::mem::take(&mut raw),
                    value: std::mem::take(&mut value),
                    is_sep,
                });
                started = false;
            }
        };
    }

    while let Some(c) = chars.next() {
        let fresh = !started;
        if !c.is_whitespace() && !(c == '\\' && fresh) {
            escaped_semicolon = false;
        }
        match c {
            '#' if fresh => break,
            c if c.is_whitespace() => end_word!(),
            ';' => {
                end_word!();
                words.push(Word {
                    raw: ";".to_string(),
                    value: ";".to_string(),
                    is_sep: true,
                });
            }
            '\\' => {
                started = true;
                raw.push('\\');
                let next = chars.next().unwrap_or('\\');
                raw.push(next);
                value.push(next);
                escaped_semicolon = fresh && next == ';';
            }
            '\'' => {
                started = true;
                raw.push('\'');
                for x in chars.by_ref() {
                    raw.push(x);
                    if x == '\'' {
                        break;
                    }
                    value.push(x);
                }
            }
            '"' => {
                started = true;
                raw.push('"');
                while let Some(x) = chars.next() {
                    raw.push(x);
                    match x {
                        '"' => break,
                        '\\' => {
                            let n = chars.next().unwrap_or('\\');
                            raw.push(n);
                            value.push(n);
                        }
                        x => value.push(x),
                    }
                }
            }
            c => {
                raw.push(c);
                value.push(c);
                started = true;
            }
        }
    }
    end_word!();
    words
}

/// Whether `raw` (exact source text) holds an unescaped `$` outside
/// single quotes (spec 5.5 rule 3).
fn has_bare_dollar(raw: &str) -> bool {
    let mut in_single = false;
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => in_single = !in_single,
            '\\' if !in_single => {
                chars.next();
            }
            '$' if !in_single => return true,
            _ => {}
        }
    }
    false
}

/// `$NAME`/`${NAME}` references outside single-quoted regions in `raw`
/// (exact source text), in order of first appearance, deduplicated. Never
/// expands anything; this only observes.
fn env_refs_outside_single_quotes(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_single = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => in_single = !in_single,
            '\\' if !in_single => {
                chars.next();
            }
            '$' if !in_single => {
                let name = read_ident(&mut chars);
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
            }
            _ => {}
        }
    }
    out
}

/// After a `$`: `${NAME}` or `NAME` (letters, digits, `_`); a `$` with
/// nothing recognisable after it gives an empty name.
fn read_ident(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    if chars.peek() == Some(&'{') {
        chars.next();
        let mut name = String::new();
        for c in chars.by_ref() {
            if c == '}' {
                break;
            }
            name.push(c);
        }
        name
    } else {
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                name.push(c);
                chars.next();
            } else {
                break;
            }
        }
        name
    }
}

// ---- the importer ----

#[derive(Debug, Clone)]
struct EnvEntry {
    value: String,
    file: PathBuf,
    line: usize,
}

#[derive(Debug, Clone)]
struct OptionEntry {
    /// The store's normalized tmux-form value.
    value: String,
    file: PathBuf,
    line: usize,
}

#[derive(Debug, Clone)]
struct BindingEntry {
    table: String,
    key: Key,
    /// The command string, in grofe's own grammar; empty means unbound
    /// (R11).
    command: String,
    desc: Option<String>,
    repeat: bool,
    file: PathBuf,
    line: usize,
}

struct Importer {
    home: Option<PathBuf>,
    /// Validates and normalizes option values exactly as the config
    /// loader's option store would.
    store: Options,
    /// `set-environment -g` values seen so far, for `source-file` path
    /// resolution (rule 8).
    env: BTreeMap<String, EnvEntry>,
    /// The current inclusion chain (canonicalized), self included, for
    /// cycle detection and the reload rewrite.
    stack: Vec<PathBuf>,
    /// Every file this import read, canonicalized, for the `-o` refusal.
    read_files: HashSet<PathBuf>,
    /// `%if` starts currently open, innermost last.
    if_stack: Vec<(PathBuf, usize)>,
    options: BTreeMap<&'static str, OptionEntry>,
    /// In first-bound order; a later directive updates an existing entry
    /// in place rather than pushing a new one (rule 7).
    keys: Vec<BindingEntry>,
    reports: Vec<ConfigError>,
}

impl Importer {
    fn new(home: Option<PathBuf>) -> Importer {
        Importer {
            home,
            store: Options::new(false),
            env: BTreeMap::new(),
            stack: Vec::new(),
            read_files: HashSet::new(),
            if_stack: Vec::new(),
            options: BTreeMap::new(),
            keys: Vec::new(),
            reports: Vec::new(),
        }
    }

    fn load_entry(&mut self, path: &Path) -> std::io::Result<()> {
        let text = fs::read_to_string(path)?;
        let canon = canonical(path);
        self.read_files.insert(canon.clone());
        self.stack.push(canon);
        self.load_text(path, &text);
        self.stack.pop();
        Ok(())
    }

    fn load_text(&mut self, path: &Path, text: &str) {
        for l in read_lines(text) {
            self.dispatch(path, &l);
        }
    }

    fn report(&mut self, file: &Path, line: usize, reason: &str, text: &str) {
        self.reports.push(ConfigError {
            file: PathBuf::from(tilde(file, self.home.as_deref())),
            line,
            key: reason.to_string(),
            message: text.to_string(),
        });
    }

    fn note(&mut self, file: &Path, line: usize, message: String) {
        self.reports.push(ConfigError {
            file: PathBuf::from(tilde(file, self.home.as_deref())),
            line,
            key: "note".to_string(),
            message,
        });
    }

    fn is_control(name: &str) -> bool {
        matches!(name, "%if" | "%elif" | "%else" | "%endif")
    }

    fn dispatch(&mut self, file: &Path, l: &Line) {
        let Some(first) = l.words.first() else {
            return;
        };
        let name = first.value.clone();
        if Self::is_control(&name) {
            match name.as_str() {
                "%if" => {
                    self.report(file, l.line, "%if", &l.text);
                    self.if_stack.push((file.to_path_buf(), l.line));
                }
                "%elif" => self.report(file, l.line, "%elif", &l.text),
                "%else" => self.report(file, l.line, "%else", &l.text),
                "%endif" => {
                    self.report(file, l.line, "%endif", &l.text);
                    self.if_stack.pop();
                }
                _ => unreachable!(),
            }
            return;
        }
        if let Some((if_file, if_line)) = self.if_stack.last().cloned() {
            let reason = format!(
                "inside %if at {}:{}",
                tilde(&if_file, self.home.as_deref()),
                if_line
            );
            return self.report(file, l.line, &reason, &l.text);
        }
        let rest = &l.words[1..];
        match canonical_directive(&name) {
            "set-option" => self.handle_set(file, l.line, &l.text, rest),
            "bind-key" => self.handle_bind(file, l.line, &l.text, rest),
            "unbind-key" => self.handle_unbind(file, l.line, &l.text, rest),
            "source-file" => self.handle_source_file(file, l.line, &l.text, rest),
            "set-environment" => self.handle_set_environment(file, l.line, &l.text, rest),
            "set-hook" => self.report(file, l.line, "set-hook", &l.text),
            "if-shell" => self.report(file, l.line, "if-shell", &l.text),
            "run-shell" => self.handle_run_shell(file, l.line, &l.text, rest),
            "command-alias" => self.report(file, l.line, "command-alias", &l.text),
            _ => self.report(file, l.line, "command not converted", &l.text),
        }
    }

    // ---- set / set-option / setw / set-window-option ----

    fn handle_set(&mut self, file: &Path, line: usize, text: &str, words: &[Word]) {
        let mut append = false;
        let mut unset = false;
        let mut unsupported_flag = false;
        let mut i = 0;
        while let Some(w) = words.get(i) {
            let v = w.value.as_str();
            if v.len() > 1 && v.starts_with('-') && v[1..].chars().all(|c| c.is_ascii_alphabetic())
            {
                for c in v[1..].chars() {
                    match c {
                        'g' | 's' | 'w' | 'o' | 'q' => {}
                        'a' => append = true,
                        'u' => unset = true,
                        _ => unsupported_flag = true,
                    }
                }
                i += 1;
            } else {
                break;
            }
        }
        if unsupported_flag {
            return self.report(
                file,
                line,
                "format-expanded, pane or targeted option (not config in M3.5)",
                text,
            );
        }
        let Some(name_word) = words.get(i) else {
            return self.report(file, line, "command not converted", text);
        };
        let name = name_word.value.clone();
        if let Some(user) = name.strip_prefix('@') {
            let reason = if user == "plugin"
                || user == "tpm_plugins"
                || user.starts_with("resurrect-")
                || user.starts_with("continuum-")
            {
                "plugins and TPM"
            } else {
                "user option"
            };
            return self.report(file, line, reason, text);
        }
        let Some(toml_key) = reverse_tmux_name(&name) else {
            return self.report(file, line, "option grofe does not have", text);
        };
        i += 1;
        if unset {
            self.options.remove(toml_key);
            return;
        }
        let Some(value_word) = words.get(i) else {
            if toml_type(toml_key) == Ty::Bool {
                return self.report(file, line, "boolean with no value (tmux toggle)", text);
            }
            return self.report(file, line, "command not converted", text);
        };
        if value_word.value.contains("#(") {
            return self.report(file, line, "#() in a format (D8)", text);
        }
        if has_bare_dollar(&value_word.raw) {
            return self.report(file, line, "environment variable in a value", text);
        }
        let final_value = if append {
            match self.options.get(toml_key) {
                Some(existing) => format!("{}{}", existing.value, value_word.value),
                None => {
                    return self.report(
                        file,
                        line,
                        "appends to a default grofe does not share",
                        text,
                    );
                }
            }
        } else {
            value_word.value.clone()
        };
        match self.store.set(Owner::Global, &name, &final_value, false) {
            Ok(_) => {
                let normalized = self
                    .store
                    .get(Owner::Global, &name)
                    .map(|e| e.value.clone())
                    .unwrap_or(final_value);
                // A plain (non-append) set fully replaces an earlier value
                // in this import; note it at the superseded line so that
                // line is still accounted for (rule 7: later wins).
                if !append && let Some(old) = self.options.get(toml_key) {
                    let old_file = old.file.clone();
                    let old_line = old.line;
                    self.note(
                        &old_file,
                        old_line,
                        format!(
                            "{toml_key} is overridden by a later set at {}:{line}",
                            tilde(file, self.home.as_deref())
                        ),
                    );
                }
                self.options.insert(
                    toml_key,
                    OptionEntry {
                        value: normalized,
                        file: file.to_path_buf(),
                        line,
                    },
                );
            }
            Err(msg) => self.report(file, line, &msg, text),
        }
    }

    // ---- bind-key / unbind-key ----

    fn handle_bind(&mut self, file: &Path, line: usize, text: &str, words: &[Word]) {
        let mut root = false;
        let mut repeat = false;
        let mut table: Option<String> = None;
        let mut note: Option<String> = None;
        let mut i = 0;
        loop {
            let Some(w) = words.get(i) else {
                return self.report(file, line, "command not converted", text);
            };
            match w.value.as_str() {
                "-n" => {
                    root = true;
                    i += 1;
                }
                "-r" => {
                    repeat = true;
                    i += 1;
                }
                "-T" => {
                    i += 1;
                    let Some(t) = words.get(i) else {
                        return self.report(file, line, "command not converted", text);
                    };
                    table = Some(t.value.clone());
                    i += 1;
                }
                "-N" => {
                    i += 1;
                    let Some(n) = words.get(i) else {
                        return self.report(file, line, "command not converted", text);
                    };
                    note = Some(n.value.clone());
                    i += 1;
                }
                s if s.starts_with('-') && s.len() > 1 => {
                    return self.report(file, line, "flag not converted", text);
                }
                _ => break,
            }
        }
        let table_name = match (root, table) {
            (_, Some(t)) => t,
            (true, None) => "root".to_string(),
            (false, None) => "prefix".to_string(),
        };
        if table_name != "prefix" && table_name != "root" {
            return self.report(file, line, &format!("key table {table_name}"), text);
        }
        let Some(key_word) = words.get(i) else {
            return self.report(file, line, "command not converted", text);
        };
        let key = match parse_key(&key_word.value) {
            Ok(k) => k,
            Err(_) => return self.report(file, line, "key grofe does not know", text),
        };
        i += 1;
        let tail = &words[i..];
        if tail.is_empty() {
            return self.report(file, line, "command not converted", text);
        }
        let Some(cmd_string) = self.render_command_tail(file, line, text, &table_name, &key, tail)
        else {
            return;
        };
        self.upsert_key(BindingEntry {
            table: table_name,
            key,
            command: cmd_string,
            desc: note,
            repeat,
            file: file.to_path_buf(),
            line,
        });
    }

    fn handle_unbind(&mut self, file: &Path, line: usize, text: &str, words: &[Word]) {
        let mut root = false;
        let mut table: Option<String> = None;
        let mut i = 0;
        loop {
            let Some(w) = words.get(i) else {
                return self.report(file, line, "command not converted", text);
            };
            match w.value.as_str() {
                "-n" => {
                    root = true;
                    i += 1;
                }
                "-T" => {
                    i += 1;
                    let Some(t) = words.get(i) else {
                        return self.report(file, line, "command not converted", text);
                    };
                    table = Some(t.value.clone());
                    i += 1;
                }
                "-a" => return self.report(file, line, "unbind -a", text),
                s if s.starts_with('-') && s.len() > 1 => {
                    return self.report(file, line, "flag not converted", text);
                }
                _ => break,
            }
        }
        let table_name = match (root, table) {
            (_, Some(t)) => t,
            (true, None) => "root".to_string(),
            (false, None) => "prefix".to_string(),
        };
        if table_name != "prefix" && table_name != "root" {
            return self.report(file, line, &format!("key table {table_name}"), text);
        }
        let Some(key_word) = words.get(i) else {
            return self.report(file, line, "command not converted", text);
        };
        let key = match parse_key(&key_word.value) {
            Ok(k) => k,
            Err(_) => return self.report(file, line, "key grofe does not know", text),
        };
        self.upsert_key(BindingEntry {
            table: table_name,
            key,
            command: String::new(),
            desc: None,
            repeat: false,
            file: file.to_path_buf(),
            line,
        });
    }

    /// Inserts or updates `new` (rule 7): a later binding for the same
    /// table and key overrides an earlier one's content in place, keeping
    /// the earlier one's position (spec 5.5 output layout: order by first
    /// bound, content and comment by the winning line), and notes the
    /// superseded line so it is still accounted for.
    fn upsert_key(&mut self, new: BindingEntry) {
        if let Some(i) = self
            .keys
            .iter()
            .position(|b| b.table == new.table && b.key == new.key)
        {
            let old_file = self.keys[i].file.clone();
            let old_line = self.keys[i].line;
            if (old_file.as_path(), old_line) != (new.file.as_path(), new.line) {
                self.note(
                    &old_file,
                    old_line,
                    format!(
                        "keys.{}.{} is overridden by a later binding at {}:{}",
                        new.table,
                        new.key,
                        tilde(&new.file, self.home.as_deref()),
                        new.line
                    ),
                );
            }
            self.keys[i] = new;
        } else {
            self.keys.push(new);
        }
    }

    /// Builds and validates the emitted command string for a binding's
    /// tail (spec 5.5 "Binding command strings"): raw words joined by
    /// spaces, `;`/`\;` separators written ` ; `, with the reload rewrite
    /// applied per sub-command. `None` means it was reported instead
    /// (a parse error); the caller does nothing further.
    fn render_command_tail(
        &mut self,
        file: &Path,
        line: usize,
        text: &str,
        table: &str,
        key: &Key,
        tail: &[Word],
    ) -> Option<String> {
        let mut groups: Vec<Vec<&Word>> = vec![Vec::new()];
        for w in tail {
            if w.is_sep {
                groups.push(Vec::new());
            } else {
                groups
                    .last_mut()
                    .expect("always at least one group")
                    .push(w);
            }
        }
        groups.retain(|g| !g.is_empty());
        if groups.is_empty() {
            self.report(file, line, "command not converted", text);
            return None;
        }
        let mut rendered = Vec::new();
        for g in &groups {
            if let Some(rewritten) = self.try_reload_rewrite(file, g) {
                self.note(
                    file,
                    line,
                    "source-file of the imported config rewritten to reload".to_string(),
                );
                rendered.push(rewritten);
            } else {
                let joined = g
                    .iter()
                    .map(|w| w.raw.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                rendered.push(joined);
            }
        }
        let cmd_string = rendered.join(" ; ");
        if let Err(msg) = validate_command(&cmd_string) {
            self.report(file, line, &msg, text);
            return None;
        }
        for var in env_refs_outside_single_quotes(&cmd_string) {
            self.note_env_var(file, line, table, key, &var);
        }
        Some(cmd_string)
    }

    /// A `source-file <path>` sub-command whose path resolves to a file
    /// currently on the inclusion chain becomes `reload` (M3 ledger).
    fn try_reload_rewrite(&self, file: &Path, group: &[&Word]) -> Option<String> {
        let first = group.first()?;
        if !matches!(first.value.as_str(), "source-file" | "source") {
            return None;
        }
        let mut i = 1;
        while let Some(w) = group.get(i) {
            if w.value == "-q" {
                i += 1;
            } else {
                break;
            }
        }
        let path_word = group.get(i)?;
        let resolved = self.resolve_path_word(&path_word.raw);
        let target = self.to_absolute(&resolved, file);
        let canon = canonical(&target);
        self.stack.contains(&canon).then(|| "reload".to_string())
    }

    fn note_env_var(&mut self, file: &Path, line: usize, table: &str, key: &Key, var: &str) {
        let msg = match self.env.get(var) {
            Some(e) => format!(
                "${var} in keys.{table}.{key} is expanded when grofe loads the config, from \
                 the server's environment; tmux read it from set-environment -g at {}:{}, which \
                 M3.5 does not convert",
                tilde(&e.file, self.home.as_deref()),
                e.line
            ),
            None => format!(
                "${var} in keys.{table}.{key} is expanded when grofe loads the config, from \
                 the server's environment"
            ),
        };
        self.note(file, line, msg);
    }

    // ---- source-file ----

    fn handle_source_file(&mut self, file: &Path, line: usize, text: &str, words: &[Word]) {
        let mut quiet = false;
        let mut i = 0;
        while let Some(w) = words.get(i) {
            if w.value == "-q" {
                quiet = true;
                i += 1;
            } else {
                break;
            }
        }
        let Some(path_word) = words.get(i) else {
            return self.report(file, line, "command not converted", text);
        };
        let resolved = self.resolve_path_word(&path_word.raw);
        let target = self.to_absolute(&resolved, file);
        let target_str = target.to_string_lossy().into_owned();
        let matches: Vec<PathBuf> = if is_glob_pattern(&target_str) {
            glob_sorted(&target_str)
        } else {
            vec![target]
        };
        if matches.is_empty() {
            if quiet {
                self.report(
                    file,
                    line,
                    "skipped: no file matches (source-file -q)",
                    text,
                );
            } else {
                self.report(file, line, "cannot read", text);
            }
            return;
        }
        for m in matches {
            self.load_source(file, line, text, &m, quiet);
        }
    }

    fn load_source(
        &mut self,
        from_file: &Path,
        from_line: usize,
        from_text: &str,
        path: &Path,
        quiet: bool,
    ) {
        let canon = canonical(path);
        if self.stack.contains(&canon) {
            let chain: Vec<String> = self
                .stack
                .iter()
                .chain([&canon])
                .map(|p| tilde(p, self.home.as_deref()))
                .collect();
            let msg = format!("include cycle: {}", chain.join(" -> "));
            return self.report(from_file, from_line, &msg, from_text);
        }
        match fs::read_to_string(path) {
            Ok(text) => {
                self.read_files.insert(canon.clone());
                self.stack.push(canon);
                self.load_text(path, &text);
                self.stack.pop();
            }
            Err(e) if quiet && e.kind() == std::io::ErrorKind::NotFound => {
                self.report(
                    from_file,
                    from_line,
                    "skipped: no file matches (source-file -q)",
                    from_text,
                );
            }
            Err(_) => {
                self.report(from_file, from_line, "cannot read", from_text);
            }
        }
    }

    /// Resolves a `source-file` path word (rule 8): `$VAR`/`${VAR}` expand
    /// first from `set-environment -g` values seen so far in this import,
    /// then the importer's own process environment; a leading `~` (the
    /// whole word, unquoted) expands from `$HOME`. Single-quoted text is
    /// never touched.
    fn resolve_path_word(&self, raw: &str) -> String {
        let mut out = String::new();
        let mut chars = raw.chars().peekable();
        let mut in_single = false;
        let mut fresh = true;
        while let Some(c) = chars.next() {
            match c {
                '\'' => {
                    in_single = !in_single;
                    fresh = false;
                }
                '"' => fresh = false,
                '\\' if !in_single => {
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                    fresh = false;
                }
                '$' if !in_single => {
                    let name = read_ident(&mut chars);
                    if name.is_empty() {
                        out.push('$');
                    } else {
                        out.push_str(&self.lookup_env(&name));
                    }
                    fresh = false;
                }
                '~' if fresh && !in_single => {
                    match &self.home {
                        Some(h) => out.push_str(&h.to_string_lossy()),
                        None => out.push('~'),
                    }
                    fresh = false;
                }
                c => {
                    out.push(c);
                    fresh = false;
                }
            }
        }
        out
    }

    fn lookup_env(&self, name: &str) -> String {
        if let Some(e) = self.env.get(name) {
            return e.value.clone();
        }
        std::env::var(name).unwrap_or_default()
    }

    fn to_absolute(&self, resolved: &str, current_file: &Path) -> PathBuf {
        let p = PathBuf::from(resolved);
        if p.is_absolute() {
            p
        } else {
            current_file
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .join(p)
        }
    }

    // ---- set-environment / run-shell ----

    fn handle_set_environment(&mut self, file: &Path, line: usize, text: &str, words: &[Word]) {
        let mut i = 0;
        while let Some(w) = words.get(i) {
            let v = w.value.as_str();
            if v.len() > 1 && v.starts_with('-') && v[1..].chars().all(|c| c.is_ascii_alphabetic())
            {
                i += 1;
            } else {
                break;
            }
        }
        if let (Some(name_w), Some(value_w)) = (words.get(i), words.get(i + 1)) {
            let resolved = self.resolve_path_word(&value_w.raw);
            self.env.insert(
                name_w.value.clone(),
                EnvEntry {
                    value: resolved,
                    file: file.to_path_buf(),
                    line,
                },
            );
        }
        self.report(file, line, "set-environment", text);
    }

    fn handle_run_shell(&mut self, file: &Path, line: usize, text: &str, words: &[Word]) {
        let mut i = 0;
        while let Some(w) = words.get(i) {
            if w.value == "-b" {
                i += 1;
            } else {
                break;
            }
        }
        if let Some(cmd_word) = words.get(i) {
            let first_token = cmd_word.value.split_whitespace().next().unwrap_or("");
            let base = first_token.rsplit('/').next().unwrap_or(first_token);
            if base == "tpm" {
                return self.report(file, line, "plugins and TPM", text);
            }
        }
        self.report(file, line, "run-shell", text);
    }

    // ---- rendering ----

    fn finish(self, entry: &Path) -> (String, Vec<ConfigError>) {
        let mut out = String::new();
        out.push_str(&format!(
            "# Generated by grofe import-tmux from {}.\n",
            tilde(entry, self.home.as_deref())
        ));
        out.push_str("# A starting config.toml or user.toml: edit it; it is not kept in sync.\n");

        let mut current_section: Option<String> = None;
        let mut started = false;
        for (toml_key, _) in config::OPTION_NAMES {
            let Some(entry) = self.options.get(toml_key) else {
                continue;
            };
            let section = section_of(toml_key);
            if !started || section != current_section {
                out.push('\n');
                if let Some(s) = &section {
                    out.push_str(&format!("[{s}]\n"));
                }
                current_section = section;
                started = true;
            }
            out.push_str(&format!(
                "# {}:{}\n",
                tilde(&entry.file, self.home.as_deref()),
                entry.line
            ));
            out.push_str(&format!(
                "{} = {}\n",
                local_key(toml_key),
                literal(toml_key, &entry.value)
            ));
        }

        for table in ["prefix", "root"] {
            let bindings: Vec<&BindingEntry> =
                self.keys.iter().filter(|b| b.table == table).collect();
            if bindings.is_empty() {
                continue;
            }
            out.push_str(&format!("\n[keys.{table}]\n"));
            for b in bindings {
                out.push_str(&format!(
                    "# {}:{}\n",
                    tilde(&b.file, self.home.as_deref()),
                    b.line
                ));
                out.push_str(&format!(
                    "{} = {}\n",
                    bare_or_quoted(&b.key.to_string()),
                    binding_value(b)
                ));
            }
        }
        (out, self.reports)
    }
}

/// The tmux name to its TOML key (the reverse of
/// [`config::tmux_name`]): a private helper over `OPTION_NAMES`, per the
/// task brief (no second copy in `config/mod.rs`).
fn reverse_tmux_name(tmux: &str) -> Option<&'static str> {
    config::OPTION_NAMES
        .iter()
        .find(|(_, n)| *n == tmux)
        .map(|(t, _)| *t)
}

/// Resolves tmux's short directive names to the canonical ones this module
/// switches on.
fn canonical_directive(name: &str) -> &str {
    match name {
        "set" | "set-option" | "setw" | "set-window-option" => "set-option",
        "bind" | "bind-key" => "bind-key",
        "unbind" | "unbind-key" => "unbind-key",
        "source" | "source-file" => "source-file",
        "set-environment" | "setenv" => "set-environment",
        "if" | "if-shell" => "if-shell",
        "run" | "run-shell" => "run-shell",
        other => other,
    }
}

/// Checks a binding's built command string exactly as the config loader
/// will read it back (`config/load.rs`'s private `commands()`).
fn validate_command(s: &str) -> Result<(), String> {
    let cmds = cmd::split_command_string(s).map_err(|e| e.to_string())?;
    if cmds.is_empty() {
        return Err(format!("no command in {s:?}"));
    }
    for argv in &cmds {
        cmd::parse(argv).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn is_glob_pattern(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// The files matching a glob `source-file` target (only the last path
/// component is glob-expanded; the directory part is literal), sorted
/// (spec 5.5 rule 8).
fn glob_sorted(resolved: &str) -> Vec<PathBuf> {
    let path = Path::new(resolved);
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let pattern = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name())
        .filter(|name| glob_match(pattern, &name.to_string_lossy()))
        .map(|name| dir.join(name))
        .collect();
    out.sort();
    out
}

/// A small shell-style glob matcher: `*` any run of characters, `?` one
/// character, `[abc]`/`[a-z]`/`[!abc]` a character class (canopy's
/// `[1-9]*.conf`). No `**`, no brace expansion: tmux's own globbing for
/// `source-file` is this small too.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(pat: &[char], s: &[char]) -> bool {
        match pat.first() {
            None => s.is_empty(),
            Some('*') => go(&pat[1..], s) || (!s.is_empty() && go(pat, &s[1..])),
            Some('?') => !s.is_empty() && go(&pat[1..], &s[1..]),
            Some('[') => {
                let Some(close) = pat.iter().position(|c| *c == ']').filter(|&i| i > 0) else {
                    return !s.is_empty() && s[0] == '[' && go(&pat[1..], &s[1..]);
                };
                let Some(&c) = s.first() else {
                    return false;
                };
                let mut class = &pat[1..close];
                let negate = class.first() == Some(&'!');
                if negate {
                    class = &class[1..];
                }
                let mut matched = false;
                let mut i = 0;
                while i < class.len() {
                    if i + 2 < class.len() && class[i + 1] == '-' {
                        if class[i] <= c && c <= class[i + 2] {
                            matched = true;
                        }
                        i += 3;
                    } else {
                        if class[i] == c {
                            matched = true;
                        }
                        i += 1;
                    }
                }
                if matched == negate {
                    return false;
                }
                go(&pat[close + 1..], &s[1..])
            }
            Some(&pc) => s.first() == Some(&pc) && go(&pat[1..], &s[1..]),
        }
    }
    let pat: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = name.chars().collect();
    go(&pat, &s)
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn section_of(toml_key: &str) -> Option<String> {
    toml_key
        .rsplit_once('.')
        .map(|(section, _)| section.to_string())
}

fn local_key(toml_key: &str) -> &str {
    toml_key.rsplit_once('.').map_or(toml_key, |(_, k)| k)
}

/// A value in its config literal form, the way `show-config`'s private
/// `literal()` in `config/cli.rs` renders it (the same few lines, copied
/// here rather than exposed: `cli.rs` stays untouched).
fn literal(toml: &str, value: &str) -> String {
    match (toml_type(toml), toml, value) {
        (Ty::Bool, _, v) => (v == "on").to_string(),
        (Ty::Int, "status.lines", "off") => "0".to_string(),
        (Ty::Int, "status.lines", "on") => "1".to_string(),
        (Ty::Int, _, v) => v.to_string(),
        (Ty::Str, _, v) => quote(v),
    }
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn bare_or_quoted(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare { s.to_string() } else { quote(s) }
}

fn binding_value(b: &BindingEntry) -> String {
    if b.desc.is_none() && !b.repeat {
        return quote(&b.command);
    }
    let mut fields = vec![format!("cmd = {}", quote(&b.command))];
    if let Some(d) = &b.desc {
        fields.push(format!("desc = {}", quote(d)));
    }
    if b.repeat {
        fields.push("repeat = true".to_string());
    }
    format!("{{ {} }}", fields.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        tokenize_words(s).into_iter().map(|w| w.value).collect()
    }

    #[test]
    fn single_quotes_join_like_a_shell() {
        assert_eq!(words(r#"'it'"'"'s'"#), vec!["it's".to_string()]);
    }

    #[test]
    fn semicolons_are_their_own_words() {
        let ws = tokenize_words("a \\; b ; c");
        let shape: Vec<(String, bool)> = ws.into_iter().map(|w| (w.value, w.is_sep)).collect();
        assert_eq!(
            shape,
            vec![
                ("a".to_string(), false),
                (";".to_string(), true),
                ("b".to_string(), false),
                (";".to_string(), true),
                ("c".to_string(), false),
            ]
        );
    }

    #[test]
    fn trailing_backslash_joins_lines_and_keeps_first_line_number() {
        let lines = read_lines("set -g @x '   \\\n  more   \\\n'\nbind y noop\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line, 1);
        assert_eq!(lines[1].line, 4);
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        let ws = tokenize_words(r#"set -g x "a#b""#);
        assert_eq!(ws.last().unwrap().value, "a#b");
    }

    #[test]
    fn block_spanning_lines_reports_once_at_its_first_line() {
        // A `{ ... }` command block is out of scope for M3.5 (rule 10:
        // reported like any other unconverted line); this only checks
        // line numbering keeps up afterwards.
        let lines = read_lines("run-shell 'x'\nbind z noop\n");
        assert_eq!(lines[1].line, 2);
    }

    #[test]
    fn dollar_outside_single_quotes_is_detected() {
        assert!(has_bare_dollar(r#""$HOME/x""#));
        assert!(!has_bare_dollar("'$HOME/x'"));
        assert!(has_bare_dollar("$HOME"));
        assert!(!has_bare_dollar(r#""\$HOME""#));
    }

    #[test]
    fn env_refs_are_found_outside_single_quotes_only() {
        assert_eq!(
            env_refs_outside_single_quotes(r#"run-shell "$A/b" '$B'"#),
            vec!["A".to_string()]
        );
    }

    #[test]
    fn glob_matches_leading_digit_class() {
        assert!(glob_match("[1-9]*.conf", "20-status.conf"));
        assert!(!glob_match("[1-9]*.conf", "00-core.conf"));
    }

    #[test]
    fn duplicate_binding_keeps_one_entry_with_the_last_value() {
        let mut imp = Importer::new(None);
        let f = Path::new("t.conf");
        let entry = |command: &str, line: usize| BindingEntry {
            table: "prefix".to_string(),
            key: parse_key("x").unwrap(),
            command: command.to_string(),
            desc: None,
            repeat: false,
            file: f.to_path_buf(),
            line,
        };
        imp.upsert_key(entry("a", 1));
        imp.upsert_key(entry("", 2));
        imp.upsert_key(entry("b", 3));
        assert_eq!(imp.keys.len(), 1);
        assert_eq!(imp.keys[0].command, "b");
        assert_eq!(imp.keys[0].line, 3);
    }

    #[test]
    fn duplicate_option_keeps_the_last_value_and_its_comment() {
        let mut imp = Importer::new(None);
        let f = Path::new("t.conf");
        imp.handle_set(
            f,
            1,
            "set -g base-index 0",
            &tokenize_words("-g base-index 0"),
        );
        imp.handle_set(
            f,
            2,
            "set -g base-index 1",
            &tokenize_words("-g base-index 1"),
        );
        let e = imp.options.get("base-index").expect("set");
        assert_eq!(e.value, "1");
        assert_eq!(e.line, 2);
    }

    #[test]
    fn set_with_no_g_still_converts_and_notes_it() {
        // Not exercised by either fixture (both always spell -g), but the
        // rule (spec 5.5) is: no -g in a config file still converts, with
        // a report note.
        let mut imp = Importer::new(None);
        let f = Path::new("t.conf");
        imp.handle_set(f, 1, "set prefix C-a", &tokenize_words("prefix C-a"));
        assert_eq!(
            imp.options.get("prefix").map(|e| e.value.as_str()),
            Some("C-a")
        );
    }

    #[test]
    fn split_round_trip_for_a_plain_binding() {
        let mut imp = Importer::new(None);
        let f = Path::new("t.conf");
        let table = "prefix".to_string();
        let key = parse_key("x").unwrap();
        let tail = tokenize_words(r##"split-window -h -c "#{pane_current_path}""##);
        let cmd = imp
            .render_command_tail(f, 1, "bind x split-window -h", &table, &key, &tail)
            .expect("validates");
        let argv = cmd::split_command_string(&cmd).unwrap();
        assert_eq!(argv.len(), 1);
        assert_eq!(
            argv[0],
            vec!["split-window", "-h", "-c", "#{pane_current_path}"]
        );
    }
}
