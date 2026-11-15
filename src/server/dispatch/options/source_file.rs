use super::*;

/// `source-file [-Fnqv] path ...`: loads each file at runtime, read by its
/// name. A path is a glob, as tmux's `glob(3)`, and one that matches
/// nothing is `No such file or directory: path` unless `-q`; the others
/// still load. A relative path is the requester's cwd's, even in a nested
/// file (recorded).
///
/// - `*.toml` is grofe's config (spec §5.1), with its includes resolved
///   against it. Its values and bindings count as runtime (R10): `reload`
///   keeps them. All errors or nothing, as `reload`.
/// - Any other file is a tmux config when the request is tmux's (its
///   dialect, or a tmux-identity server): tmux's plugins source their own
///   `*.tmux` and `*.conf` files through `tmux` ([`source_tmux_file`]). A
///   native request on a grofe server reads it as TOML, as before.
///
/// Files sourced from files nest at most [`MAX_SOURCE_DEPTH`] deep, as in
/// tmux 3.7b; a deeper one is `too many nested files`. A TOML file cannot
/// run `source-file` (its includes are `config::load_one`'s, which refuse
/// a cycle), so only a tmux file nests this way.
pub fn source_file(state: &mut State, ctx: &ClientCtx, c: SourceFile) -> Result<String, String> {
    // `-t` can fail quietly: the file then runs for the current pane.
    let run = match c.target.as_ref().map(|t| state.model.resolve_pane(t, ctx)) {
        Some(Ok(pane)) => ClientCtx {
            pane: Some(pane),
            ..ctx.clone()
        },
        _ => ctx.clone(),
    };
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for given in &c.paths {
        // `-F`: the path is a format, expanded for the requester's pane.
        let given = if c.format {
            let pane = state.model.resolve_pane(&Target::Current, &run).ok();
            super::super::bindings::expand(state, &run, pane, given)
        } else {
            given.clone()
        };
        if given == "-" {
            files.push(PathBuf::from("-"));
            continue;
        }
        match glob_files(&source_pattern(&ctx.cwd, &given)) {
            Ok(found) => files.extend(found),
            Err(GlobError::NoMatch) if c.quiet => {}
            Err(e) => errors.push(format!("{}: {given}", e.text())),
        }
    }
    let mut out = String::new();
    // What tmux refused while parsing is printed when every file is done.
    let mut refused = String::new();
    // Every file is read (and traced) before any of them runs.
    let mut loaded = Vec::new();
    for path in files {
        match load_one(state, ctx, &run, &path, &c) {
            Ok(file) => loaded.push((path, file)),
            Err(e) if e.is_empty() => refused.push_str(&std::mem::take(&mut state.failed_out)),
            Err(e) => {
                out.push_str(&std::mem::take(&mut state.failed_out));
                errors.push(e);
            }
        }
    }
    for (_, file) in &mut loaded {
        if let Loaded::Tmux(parsed) = file {
            out.push_str(&std::mem::take(&mut parsed.shown));
        }
    }
    let mut loaded = loaded.into_iter();
    while let Some((_, file)) = loaded.next() {
        let result = match file {
            Loaded::Tmux(parsed) => run_parsed(state, parsed),
            Loaded::Toml(path) => apply_toml(state, &path),
        };
        match result {
            Ok(text) => out.push_str(&text),
            Err(e) if e.is_empty() => refused.push_str(&std::mem::take(&mut state.failed_out)),
            Err(e) => {
                out.push_str(&std::mem::take(&mut state.failed_out));
                errors.push(e);
            }
        }
        // A command that held the request holds the files after it too.
        if let Some(held) = &mut state.held {
            held.errors.append(&mut errors);
            for (path, file) in loaded.by_ref() {
                match file {
                    Loaded::Tmux(parsed) => {
                        let Parsed {
                            groups,
                            run,
                            depth,
                            verbose,
                            ..
                        } = parsed;
                        held.frames.extend(groups.into_iter().map(|cmds| {
                            super::super::client::HeldFrame {
                                ctx: run.clone(),
                                cmds,
                                keep_going: true,
                                depth,
                                verbose,
                            }
                        }));
                    }
                    Loaded::Toml(_) => {
                        let rest = SourceFile {
                            format: false,
                            paths: vec![glob_escape(&path)],
                            ..c.clone()
                        };
                        held.frames.push(super::super::client::HeldFrame {
                            ctx: ctx.clone(),
                            cmds: vec![Command::SourceFile(rest)],
                            keep_going: true,
                            depth: source_depth(),
                            verbose: source_verbose(),
                        });
                    }
                }
            }
            out.push_str(&refused);
            return Ok(out);
        }
    }
    out.push_str(&refused);
    if errors.is_empty() && refused.is_empty() {
        return Ok(out);
    }
    state.failed_out = out;
    Err(join_nonempty(&errors))
}

/// Why a path gave no file: `glob(3)`'s own failures, with the `strerror`
/// text tmux prints for them.
pub(super) enum GlobError {
    NoMatch,
    Other(i32),
}

impl GlobError {
    pub(super) fn text(&self) -> &'static str {
        match self {
            GlobError::NoMatch => "No such file or directory",
            GlobError::Other(rc) if *rc == nix::libc::GLOB_NOSPACE => "Cannot allocate memory",
            GlobError::Other(_) => "Invalid argument",
        }
    }
}

/// The files `pattern` matches, in `glob(3)`'s order (sorted).
pub(super) fn glob_files(pattern: &Path) -> Result<Vec<PathBuf>, GlobError> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let pattern = std::ffi::CString::new(pattern.as_os_str().as_bytes())
        .map_err(|_| GlobError::Other(nix::libc::GLOB_ABORTED))?;
    // SAFETY: `found` is zeroed storage for `glob` to fill; the paths it
    // holds are read before `globfree` releases them.
    unsafe {
        let mut found: nix::libc::glob_t = std::mem::zeroed();
        let rc = nix::libc::glob(pattern.as_ptr(), 0, None, &mut found);
        if rc != 0 {
            nix::libc::globfree(&mut found);
            return Err(if rc == nix::libc::GLOB_NOMATCH {
                GlobError::NoMatch
            } else {
                GlobError::Other(rc)
            });
        }
        let files = (0..found.gl_pathc)
            .map(|i| {
                let path = std::ffi::CStr::from_ptr(*found.gl_pathv.add(i));
                PathBuf::from(std::ffi::OsString::from_vec(path.to_bytes().to_vec()))
            })
            .collect();
        nix::libc::globfree(&mut found);
        Ok(files)
    }
}

/// The pattern `source-file` globs for `given`: an absolute path as it
/// is, else under the requester's `cwd`, which is quoted so only `given`
/// is a pattern, as tmux's `cmd_source_file_quote_for_glob`.
pub(super) fn source_pattern(cwd: &Path, given: &str) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    if given.starts_with('/') {
        return PathBuf::from(given);
    }
    let mut pattern = glob_quote(cwd.as_os_str().as_bytes());
    pattern.push(b'/');
    pattern.extend_from_slice(given.as_bytes());
    PathBuf::from(std::ffi::OsString::from_vec(pattern))
}

/// `bytes` with `glob(3)`'s special characters quoted, so the pattern
/// matches only itself.
pub(super) fn glob_quote(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if matches!(b, b'*' | b'?' | b'[' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

/// `path` as a pattern that matches only itself.
pub(super) fn glob_escape(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    String::from_utf8_lossy(&glob_quote(path.as_os_str().as_bytes())).into_owned()
}

/// A file `source-file` found, read: a tmux file is parsed (and traced)
/// before any file runs, as tmux reads every file when its read is done,
/// a TOML one is applied when its turn comes.
pub(super) enum Loaded {
    Tmux(Parsed),
    Toml(PathBuf),
}

/// A tmux file read and parsed.
pub(super) struct Parsed {
    /// What `-v` traced.
    pub(super) shown: String,
    /// The commands of each line, to run in turn (none with `-n`).
    pub(super) groups: Vec<Vec<Command>>,
    /// The context they run in.
    pub(super) run: ClientCtx,
    /// The source-file depth and `-v` they run under.
    pub(super) depth: u32,
    pub(super) verbose: bool,
}

/// Reads one file `source-file` found.
pub(super) fn load_one(
    state: &mut State,
    ctx: &ClientCtx,
    run: &ClientCtx,
    path: &Path,
    c: &SourceFile,
) -> Result<Loaded, String> {
    let tmux =
        ctx.dialect == Dialect::Tmux || state.model.identity == crate::compat::Identity::Tmux;
    if path == Path::new("-") || tmux && path.extension().is_none_or(|e| e != "toml") {
        let depth = source_depth();
        if depth >= MAX_SOURCE_DEPTH {
            return Err("too many nested files".to_string());
        }
        let verbose = c.verbose || source_verbose();
        let parsed = with_source_depth(depth + 1, verbose, || {
            parse_tmux_file(state, ctx, run, path, c.parse_only, verbose)
        })?;
        return Ok(Loaded::Tmux(Parsed {
            depth: depth + 1,
            verbose,
            ..parsed
        }));
    }
    Ok(Loaded::Toml(path.to_path_buf()))
}

/// Applies a TOML file's settings and bindings.
pub(super) fn apply_toml(state: &mut State, path: &Path) -> Result<String, String> {
    let resolved = config::load_one(path, &config_env()).map_err(|e| join_errors(&e))?;
    check_live_names(&state.registry.key_tables, &resolved)?;
    log_warnings(&resolved);
    let model = &mut state.model;
    for (name, value, _) in resolved.configured() {
        set_global(model, &mut state.events, name, value);
        model.options.mark_runtime(name);
    }
    for (name, items, _) in resolved.configured_lists() {
        set_global_list(model, &mut state.events, name, list_items(items));
        model.options.mark_runtime(name);
    }
    let tables = &mut state.registry.key_tables;
    for k in &resolved.keys {
        if k.cmds.is_empty() {
            tables.unbind_runtime(&k.table, &k.key);
            continue;
        }
        let mut b = Binding::new(k.cmds.clone(), k.repeat, BindingSource::Runtime);
        b.desc = k.desc.clone();
        b.name = k.name.clone();
        tables.insert(&k.table, k.key, b);
    }
    Ok(String::new())
}

/// How deep `source-file` of a tmux file nests, as tmux 3.7b's (recorded:
/// a file that sources itself loads 50 times, then the 51st is `too many
/// nested files`, exit 1, the server up).
pub(super) const MAX_SOURCE_DEPTH: u32 = 50;

thread_local! {
    /// The tmux files being sourced now, one inside another (the actor's
    /// stack). A held file's later lines run outside it, so a
    /// [`super::super::client::HeldFrame`] carries the depth it was held at and
    /// runs under it again ([`with_source_depth`]).
    static SOURCE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// How many tmux files are being sourced, one inside another, right now.
pub(crate) fn source_depth() -> u32 {
    SOURCE_DEPTH.get()
}

/// Runs `f` at source-file depth `depth`, then puts the old depth back.
pub(crate) fn with_source_depth<T>(depth: u32, verbose: bool, f: impl FnOnce() -> T) -> T {
    let old = SOURCE_DEPTH.replace(depth);
    let was = SOURCE_VERBOSE.replace(verbose);
    let out = f();
    SOURCE_DEPTH.set(old);
    SOURCE_VERBOSE.set(was);
    out
}

/// Whether `source-file -v` is on for the file being sourced now.
pub(crate) fn source_verbose() -> bool {
    SOURCE_VERBOSE.get()
}

/// Reads a config file for [`source_tmux_file`]: `%if` conditions are
/// formats expanded against the requester's pane, and `NAME=value` sets the
/// global environment (not for `source-file -n`).
pub(super) struct FileHost<'a> {
    pub(super) state: &'a mut State,
    pub(super) ctx: &'a ClientCtx,
    pub(super) parse_only: bool,
}

impl crate::cmd::ConfigHost for FileHost<'_> {
    fn condition(&mut self, format: &str) -> bool {
        let pane = self
            .state
            .model
            .resolve_pane(&Target::Current, self.ctx)
            .ok();
        let text = super::super::bindings::expand(self.state, self.ctx, pane, format);
        !text.is_empty() && text != "0"
    }

    fn assign(&mut self, assignment: &str, hidden: bool) {
        let Some((name, value)) = assignment.split_once('=') else {
            return;
        };
        if !self.parse_only {
            crate::server::hooks::set_global(self.state, name, Some(value.to_string()), hidden);
        }
    }
}

thread_local! {
    /// `source-file -v` is on for the files it sources in turn.
    static SOURCE_VERBOSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A command as `source-file -v` prints it (tmux's `cmd_print`, recorded):
/// the full name, a cluster of flags in letter order, flags with a value
/// after them, and every argument as `args_escape` writes it.
pub(super) fn print_command(argv: &[String]) -> String {
    tmux_command(argv)
}

/// A config file's text. An open that fails says why, as tmux (`strerror`
/// first, then the path); a read that fails is always `Input/output error`
/// (recorded: a directory), as tmux's file reader reports it.
pub(super) fn read_config(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| {
        let text = e.to_string();
        let text = text.split(" (os error").next().unwrap_or_default();
        format!("{text}: {}", path.display())
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| format!("Input/output error: {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A tmux config file, read as tmux 3.7b's `source-file` reads one
/// (recorded): split into lines ([`cmd::split_config_file_with`], which
/// also settles `%if` and `NAME=value`) and every command parsed through
/// the tmux front end before any runs. What tmux refuses while parsing (a
/// syntax error, an unknown command, a flag its usage lacks) runs nothing
/// and is `<path>:<line>: <error>`. A command that is valid tmux but that
/// grofe cannot translate fails only itself, with the same text: it stops
/// the rest of its line, the lines around it run, and it is noted in
/// `compat-miss.log`. Then each line's commands run as one group: an error
/// stops the rest of its line, the next line still runs, and the request
/// fails at the end with every error, after what the commands printed. A
/// command that holds the request (`run-shell` without `-b`, `wait-for`)
/// holds the lines after it too, as tmux's queue waits; those lines still
/// run on after an error, and the errors from before the hold and after it
/// fail the request at the end. With `parse_only` (`-n`) nothing runs.
pub(super) fn parse_tmux_file(
    state: &mut State,
    ctx: &ClientCtx,
    run: &ClientCtx,
    path: &Path,
    parse_only: bool,
    verbose: bool,
) -> Result<Parsed, String> {
    let text = if path == Path::new("-") {
        // The CLI client's stdin; an attached client has none (tmux's EBADF).
        match ctx.request.cli_io.as_deref() {
            Some(crate::protocol::CliIo {
                stdin: Some(data), ..
            }) => String::from_utf8_lossy(data).into_owned(),
            _ => return Err("Bad file descriptor: -".to_string()),
        }
    } else {
        read_config(path)?
    };
    let at = |line: usize, e: &dyn std::fmt::Display| format!("{}:{line}: {e}", path.display());
    // tmux collects what it refuses while parsing and prints it on stdout
    // when the file is done (`cfg_print_causes`), and the request fails.
    let refused = |state: &mut State, message: String| {
        state.failed_out = format!("{message}\n");
        Err(String::new())
    };
    // `%if` reads the client's current pane; the commands run for `-t`'s.
    let ctx = ClientCtx {
        dialect: Dialect::Tmux,
        ..ctx.clone()
    };
    let run = ClientCtx {
        dialect: Dialect::Tmux,
        ..run.clone()
    };
    let split = {
        let mut host = FileHost {
            state: &mut *state,
            ctx: &ctx,
            parse_only,
        };
        crate::cmd::split_config_file_with(&text, &mut host)
    };
    let mut groups = Vec::new();
    let split = match split {
        Ok(split) => split,
        Err((line, e)) => return refused(state, at(line, &e)),
    };
    let mut shown = String::new();
    for (line, cmds) in &split {
        if verbose {
            let cmds: Vec<String> = cmds.iter().map(|argv| print_command(argv)).collect();
            shown.push_str(&format!(
                "{}:{line}: {}\n",
                path.display(),
                cmds.join(" ; ")
            ));
        }
    }
    for (line, cmds) in split {
        let mut group = Vec::new();
        for argv in cmds {
            match super::super::parse_split_checked(state, &run, &argv) {
                Ok(parsed) => group.extend(parsed),
                Err((true, e)) => return refused(state, at(line, &e)),
                Err((false, e)) => {
                    // Fails when its turn comes; the line stops there.
                    group.push(Command::Refused(at(line, &e)));
                    break;
                }
            }
        }
        groups.push(group);
    }
    Ok(Parsed {
        shown,
        groups: if parse_only { Vec::new() } else { groups },
        run,
        depth: 0,
        verbose,
    })
}

/// Runs a parsed tmux file's lines (see [`parse_tmux_file`]).
pub(super) fn run_parsed(state: &mut State, parsed: Parsed) -> Result<String, String> {
    let Parsed {
        groups,
        run,
        depth,
        verbose,
        ..
    } = parsed;
    with_source_depth(depth, verbose, || {
        let mut out = String::new();
        let mut errors = Vec::new();
        let mut groups = groups.into_iter();
        while let Some(group) = groups.next() {
            match super::super::run_commands(state, &run, group) {
                Ok(text) => out.push_str(&text),
                Err(e) => {
                    out.push_str(&std::mem::take(&mut state.failed_out));
                    errors.push(e);
                }
            }
            if state.held.is_some() {
                let held = state.held.get_or_insert_default();
                held.errors.append(&mut errors);
                held.frames
                    .extend(groups.by_ref().map(|cmds| super::super::client::HeldFrame {
                        ctx: run.clone(),
                        cmds,
                        keep_going: true,
                        depth,
                        verbose,
                    }));
                return Ok(out);
            }
        }
        if errors.is_empty() {
            return Ok(out);
        }
        state.failed_out = out;
        Err(join_nonempty(&errors))
    })
}

/// `errors` one to a line, leaving out those whose text already went to
/// stdout (a nested file's parse errors).
pub(crate) fn join_nonempty(errors: &[String]) -> String {
    let shown: Vec<&str> = errors
        .iter()
        .map(String::as_str)
        .filter(|e| !e.is_empty())
        .collect();
    shown.join("\n")
}

/// A `name` is unique across the live tables too (R6): `load_one` checks
/// only inside the file it loads.
pub(super) fn check_live_names(tables: &KeyTables, resolved: &Resolved) -> Result<(), String> {
    let mut errors = Vec::new();
    for k in &resolved.keys {
        let Some(name) = &k.name else { continue };
        let taken = tables.table_names().find_map(|table| {
            tables
                .keys(table)
                .into_iter()
                .find(|key| {
                    !(table == k.table && *key == k.key)
                        && tables
                            .exact(table, key)
                            .is_some_and(|b| b.name.as_deref() == Some(name))
                })
                .map(|key| format!("{table} {key}"))
        });
        if let Some(taken) = taken {
            let file = k.source.file.clone().unwrap_or_default();
            errors.push(config::ConfigError {
                file,
                line: k.source.line,
                key: format!("keys.{}.{}", k.table, k.key),
                message: format!("name {name} already used by {taken}"),
            });
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(join_errors(&errors))
    }
}
