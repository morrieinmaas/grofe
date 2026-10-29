//! The layout file: parsed, validated whole, and resolved into a [`Plan`]
//! (paths absolute, `~` expanded, defaults filled) before anything is built.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use toml::Spanned;

use crate::server::layout::{MainSize, Preset, parse_tmux_layout};

/// One mistake in a layout file; `Display` is `<file>:<line>: <message>`
/// (the line left out when it is 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutError {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}: {}", self.file.display(), self.message)
        } else {
            write!(f, "{}:{}: {}", self.file.display(), self.line, self.message)
        }
    }
}

impl std::error::Error for LayoutError {}

/// How a pane's command is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Run {
    /// A shell, nothing typed.
    Shell,
    /// Typed into the shell, which stays.
    Typed(String),
    /// The pane's own command.
    Exec(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitFor {
    /// A regex matched against the pane's visible screen.
    Regex(String),
    /// A path that must exist.
    Path(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    pub what: WaitFor,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanePlan {
    pub cwd: PathBuf,
    pub run: Run,
    pub env: Vec<(String, String)>,
    /// The `split-window -l` value.
    pub size: Option<String>,
    pub focus: bool,
    pub wait: Option<Wait>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatPlan {
    pub cwd: PathBuf,
    pub run: Run,
    pub env: Vec<(String, String)>,
    pub x: Option<String>,
    pub y: Option<String>,
    pub width: Option<String>,
    pub height: Option<String>,
    pub border: Option<String>,
    pub title: Option<String>,
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowPlan {
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub layout: Option<String>,
    pub focus: bool,
    pub env: Vec<(String, String)>,
    /// `split-window -h` instead of `-v`.
    pub side_by_side: bool,
    /// Never empty: a window without panes gets one shell.
    pub panes: Vec<PanePlan>,
    pub floats: Vec<FloatPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlan {
    pub name: String,
    pub root: PathBuf,
    pub env: Vec<(String, String)>,
    pub before: Option<String>,
    pub on_start: Option<String>,
    pub after: Option<String>,
    pub on_stop: Option<String>,
    pub attach: bool,
    pub windows: Vec<WindowPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub sessions: Vec<SessionPlan>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    session: Vec<RawSession>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSession {
    name: Option<Spanned<String>>,
    root: Option<Spanned<String>>,
    env: Option<Spanned<BTreeMap<String, String>>>,
    before: Option<String>,
    on_start: Option<String>,
    after: Option<String>,
    on_stop: Option<String>,
    attach: Option<bool>,
    #[serde(default)]
    window: Vec<RawWindow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    name: Option<Spanned<String>>,
    cwd: Option<String>,
    layout: Option<Spanned<String>>,
    #[serde(default)]
    focus: bool,
    env: Option<Spanned<BTreeMap<String, String>>>,
    split: Option<Spanned<String>>,
    #[serde(default)]
    pane: Vec<RawPane>,
    #[serde(default)]
    float: Vec<RawFloat>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPane {
    cwd: Option<String>,
    cmd: Option<Spanned<String>>,
    exec: Option<Spanned<bool>>,
    env: Option<Spanned<BTreeMap<String, String>>>,
    size: Option<Spanned<toml::Value>>,
    #[serde(default)]
    focus: bool,
    wait: Option<Spanned<RawWait>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWait {
    regex: Option<String>,
    path: Option<String>,
    timeout: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFloat {
    cwd: Option<String>,
    cmd: Option<Spanned<String>>,
    exec: Option<Spanned<bool>>,
    env: Option<Spanned<BTreeMap<String, String>>>,
    x: Option<Spanned<toml::Value>>,
    y: Option<Spanned<toml::Value>>,
    width: Option<Spanned<toml::Value>>,
    height: Option<Spanned<toml::Value>>,
    border: Option<Spanned<String>>,
    title: Option<String>,
    #[serde(default)]
    focus: bool,
}

/// Values `pane-border-lines` takes.
const BORDERS: [&str; 6] = ["single", "double", "heavy", "simple", "number", "spaces"];

/// The default wait timeout, in seconds.
pub const DEFAULT_WAIT_SECS: u64 = 30;

/// The longest wait timeout: a day, far below what would overflow a deadline.
pub const MAX_WAIT_SECS: u64 = 86_400;

struct Ctx<'a> {
    text: &'a str,
    file: &'a Path,
    home: Option<&'a Path>,
    cwd: &'a Path,
}

impl Ctx<'_> {
    fn err(&self, span: Range<usize>, message: impl Into<String>) -> LayoutError {
        LayoutError {
            file: self.file.to_path_buf(),
            line: line_of(self.text, span.start),
            message: message.into(),
        }
    }

    fn expand(&self, path: &str) -> PathBuf {
        match (path.strip_prefix("~/"), path, self.home) {
            (Some(rest), _, Some(home)) => home.join(rest),
            (None, "~", Some(home)) => home.to_path_buf(),
            _ => PathBuf::from(path),
        }
    }

    /// `path` against `base`: absolute and `~` paths stand alone.
    fn join(&self, base: &Path, path: Option<&str>) -> PathBuf {
        match path {
            Some(p) => base.join(self.expand(p)),
            None => base.to_path_buf(),
        }
    }
}

/// The 1-based line holding byte `offset`.
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

fn toml_error(text: &str, file: &Path, e: &toml::de::Error) -> LayoutError {
    LayoutError {
        file: file.to_path_buf(),
        line: e.span().map_or(0, |s| line_of(text, s.start)),
        message: e.message().trim().to_string(),
    }
}

/// Parses and validates `text`, read from `file`. `home` expands `~`;
/// relative paths are made absolute against `cwd`.
pub fn parse(
    text: &str,
    file: &Path,
    home: Option<&Path>,
    cwd: &Path,
) -> Result<Plan, LayoutError> {
    let ctx = Ctx {
        text,
        file,
        home,
        cwd,
    };
    let table: toml::Table = text.parse().map_err(|e| toml_error(text, file, &e))?;
    let (raw, multi): (Vec<RawSession>, bool) = if table.contains_key("session") {
        let f: RawFile = toml::from_str(text).map_err(|e| toml_error(text, file, &e))?;
        (f.session, true)
    } else {
        let s: RawSession = toml::from_str(text).map_err(|e| toml_error(text, file, &e))?;
        (vec![s], false)
    };
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut sessions = Vec::new();
    for s in raw {
        sessions.push(session(&ctx, s, multi, &stem)?);
    }
    for (i, s) in sessions.iter().enumerate() {
        if sessions[..i].iter().any(|o| o.name == s.name) {
            return Err(LayoutError {
                file: file.to_path_buf(),
                line: 0,
                message: format!("session name {:?} is used twice", s.name),
            });
        }
    }
    if sessions.is_empty() {
        return Err(LayoutError {
            file: file.to_path_buf(),
            line: 0,
            message: "no session defined".to_string(),
        });
    }
    Ok(Plan { sessions })
}

fn session(ctx: &Ctx, s: RawSession, multi: bool, stem: &str) -> Result<SessionPlan, LayoutError> {
    let name = match &s.name {
        Some(n) => {
            check_session_name(ctx, n.span(), n.get_ref())?;
            n.get_ref().clone()
        }
        None if multi => {
            return Err(LayoutError {
                file: ctx.file.to_path_buf(),
                line: 0,
                message: "every [[session]] needs a name".to_string(),
            });
        }
        None => stem.to_string(),
    };
    let root = match &s.root {
        Some(r) => ctx.join(ctx.cwd, Some(r.get_ref())),
        None => ctx.cwd.to_path_buf(),
    };
    let env = env(ctx, s.env.as_ref())?;
    let mut windows = Vec::new();
    for w in s.window {
        windows.push(window(ctx, w, &root)?);
    }
    if windows.is_empty() {
        windows.push(window(ctx, empty_window(), &root)?);
    }
    if windows.iter().filter(|w| w.focus).count() > 1 {
        return Err(LayoutError {
            file: ctx.file.to_path_buf(),
            line: 0,
            message: format!("session {name:?}: more than one window has focus = true"),
        });
    }
    Ok(SessionPlan {
        name,
        root,
        env,
        before: s.before,
        on_start: s.on_start,
        after: s.after,
        on_stop: s.on_stop,
        attach: s.attach.unwrap_or(true),
        windows,
    })
}

fn empty_window() -> RawWindow {
    RawWindow {
        name: None,
        cwd: None,
        layout: None,
        focus: false,
        env: None,
        split: None,
        pane: Vec::new(),
        float: Vec::new(),
    }
}

/// The name rules shared with `--session`: tmux cannot hold `:` or `.`.
pub fn session_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("a session name cannot be empty")
    } else if name.contains([':', '.', '\n']) {
        Some("a session name cannot contain ':', '.' or a newline")
    } else {
        None
    }
}

fn check_session_name(ctx: &Ctx, span: Range<usize>, name: &str) -> Result<(), LayoutError> {
    match session_name_problem(name) {
        Some(why) => Err(ctx.err(span, why)),
        None => Ok(()),
    }
}

fn env(
    ctx: &Ctx,
    env: Option<&Spanned<BTreeMap<String, String>>>,
) -> Result<Vec<(String, String)>, LayoutError> {
    let Some(env) = env else {
        return Ok(Vec::new());
    };
    for key in env.get_ref().keys() {
        let mut chars = key.chars();
        let ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !ok {
            return Err(ctx.err(env.span(), format!("{key:?} is not a valid variable name")));
        }
    }
    Ok(env
        .get_ref()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect())
}

fn window(ctx: &Ctx, w: RawWindow, root: &Path) -> Result<WindowPlan, LayoutError> {
    let cwd = ctx.join(root, w.cwd.as_deref());
    if let Some(l) = &w.layout {
        check_layout(ctx, l)?;
    }
    let side_by_side = match w.split.as_ref().map(|s| (s.span(), s.get_ref().as_str())) {
        None | Some((_, "v")) => false,
        Some((_, "h")) => true,
        Some((span, other)) => {
            return Err(ctx.err(span, format!("split must be \"v\" or \"h\", not {other:?}")));
        }
    };
    let has_layout = w.layout.is_some();
    let mut panes = Vec::new();
    for (i, p) in w.pane.into_iter().enumerate() {
        panes.push(pane(ctx, p, &cwd, i == 0, has_layout)?);
    }
    if panes.is_empty() {
        panes.push(pane(ctx, empty_pane(), &cwd, true, has_layout)?);
    }
    let mut floats = Vec::new();
    for f in w.float {
        floats.push(float(ctx, f, &cwd)?);
    }
    let focused =
        panes.iter().filter(|p| p.focus).count() + floats.iter().filter(|f| f.focus).count();
    if focused > 1 {
        return Err(LayoutError {
            file: ctx.file.to_path_buf(),
            line: 0,
            message: format!(
                "window {:?}: more than one pane or float has focus = true",
                w.name.as_ref().map(|n| n.get_ref().as_str()).unwrap_or("")
            ),
        });
    }
    if let (Some(l), true) = (&w.layout, !panes.is_empty())
        && let Some(g) = parse_tmux_layout(l.get_ref())
    {
        let cells = count_cells(&g);
        if panes.len() > cells {
            return Err(ctx.err(
                l.span(),
                format!(
                    "the layout has {cells} cells but the window has {} panes",
                    panes.len()
                ),
            ));
        }
    }
    Ok(WindowPlan {
        name: w.name.map(|n| n.into_inner()),
        cwd,
        layout: w.layout.map(|l| l.into_inner()),
        focus: w.focus,
        env: env(ctx, w.env.as_ref())?,
        side_by_side,
        panes,
        floats,
    })
}

fn count_cells(g: &crate::server::layout::LayoutGeometry) -> usize {
    use crate::server::layout::GeometryCell;
    match &g.cell {
        GeometryCell::Pane(_) => 1,
        GeometryCell::Split { children, .. } => children.iter().map(count_cells).sum(),
    }
}

fn check_layout(ctx: &Ctx, l: &Spanned<String>) -> Result<(), LayoutError> {
    let s = l.get_ref();
    let preset = Preset::from_name(s, MainSize::WIDTH, MainSize::HEIGHT, 0).is_some();
    if preset || parse_tmux_layout(s).is_some() {
        return Ok(());
    }
    Err(ctx.err(
        l.span(),
        format!("{s:?} is neither a layout preset nor a valid tmux layout string"),
    ))
}

fn empty_pane() -> RawPane {
    RawPane {
        cwd: None,
        cmd: None,
        exec: None,
        env: None,
        size: None,
        focus: false,
        wait: None,
    }
}

fn run(
    ctx: &Ctx,
    cmd: Option<&Spanned<String>>,
    exec: Option<&Spanned<bool>>,
) -> Result<Run, LayoutError> {
    match (cmd, exec.map(|e| (e.span(), *e.get_ref()))) {
        (None, Some((span, true))) => Err(ctx.err(span, "exec = true needs a cmd")),
        (None, _) => Ok(Run::Shell),
        (Some(c), _) if c.get_ref().is_empty() => Err(ctx.err(c.span(), "cmd cannot be empty")),
        (Some(c), Some((_, true))) => Ok(Run::Exec(c.get_ref().clone())),
        (Some(c), _) => Ok(Run::Typed(c.get_ref().clone())),
    }
}

fn pane(
    ctx: &Ctx,
    p: RawPane,
    window_cwd: &Path,
    first: bool,
    has_layout: bool,
) -> Result<PanePlan, LayoutError> {
    let size = match &p.size {
        None => None,
        Some(s) if first => {
            return Err(ctx.err(
                s.span(),
                "size on the window's first pane: no split makes that pane",
            ));
        }
        Some(s) if has_layout => {
            return Err(ctx.err(s.span(), "size in a window that has a layout"));
        }
        Some(s) => Some(size(ctx, s)?),
    };
    let wait = match &p.wait {
        None => None,
        Some(w) => Some(wait(ctx, w, &ctx.join(window_cwd, p.cwd.as_deref()))?),
    };
    Ok(PanePlan {
        cwd: ctx.join(window_cwd, p.cwd.as_deref()),
        run: run(ctx, p.cmd.as_ref(), p.exec.as_ref())?,
        env: env(ctx, p.env.as_ref())?,
        size,
        focus: p.focus,
        wait,
    })
}

/// A `split-window -l` value: `N%` (1 to 99) or cells.
fn size(ctx: &Ctx, s: &Spanned<toml::Value>) -> Result<String, LayoutError> {
    let bad = || {
        ctx.err(
            s.span(),
            "size is a percentage like \"30%\" (1 to 99) or a number of cells",
        )
    };
    match s.get_ref() {
        toml::Value::Integer(n) if *n >= 1 && *n <= i64::from(u16::MAX) => Ok(n.to_string()),
        toml::Value::String(t) => match t.strip_suffix('%') {
            Some(p) => match p.parse::<u8>() {
                Ok(n) if (1..=99).contains(&n) => Ok(t.clone()),
                _ => Err(bad()),
            },
            None => match t.parse::<u16>() {
                Ok(n) if n >= 1 => Ok(n.to_string()),
                _ => Err(bad()),
            },
        },
        _ => Err(bad()),
    }
}

fn wait(ctx: &Ctx, w: &Spanned<RawWait>, pane_cwd: &Path) -> Result<Wait, LayoutError> {
    let secs = w.get_ref().timeout.unwrap_or(DEFAULT_WAIT_SECS);
    if secs > MAX_WAIT_SECS {
        return Err(ctx.err(
            w.span(),
            format!("wait.timeout: at most {MAX_WAIT_SECS} seconds"),
        ));
    }
    let timeout = Duration::from_secs(secs);
    let what = match (&w.get_ref().regex, &w.get_ref().path) {
        (Some(re), None) => {
            regex::Regex::new(re).map_err(|e| {
                ctx.err(
                    w.span(),
                    format!("wait.regex: {}", e.to_string().replace('\n', " ")),
                )
            })?;
            WaitFor::Regex(re.clone())
        }
        (None, Some(p)) => WaitFor::Path(ctx.join(pane_cwd, Some(p))),
        _ => {
            return Err(ctx.err(w.span(), "wait needs exactly one of regex and path"));
        }
    };
    Ok(Wait { what, timeout })
}

/// A float geometry value: signed cells or `N%`; a percentage over 100 can
/// never fit.
fn extent(
    ctx: &Ctx,
    key: &str,
    v: Option<&Spanned<toml::Value>>,
    positive: bool,
) -> Result<Option<String>, LayoutError> {
    let Some(v) = v else {
        return Ok(None);
    };
    let bad = |why: &str| ctx.err(v.span(), format!("{key}: {why}"));
    match v.get_ref() {
        toml::Value::Integer(n) => {
            if positive && *n < 1 {
                return Err(bad("must be at least 1 cell"));
            }
            if i32::try_from(*n).is_err() {
                return Err(bad("out of range"));
            }
            Ok(Some(n.to_string()))
        }
        toml::Value::String(t) => match t.strip_suffix('%').map(str::parse::<u32>) {
            Some(Ok(p)) if p <= 100 && !(positive && p == 0) => Ok(Some(t.clone())),
            Some(Ok(_)) => Err(bad(
                "a percentage must be at most 100 (and positive for a size)",
            )),
            _ => Err(bad(
                "expected a number of cells or a percentage like \"50%\"",
            )),
        },
        _ => Err(bad(
            "expected a number of cells or a percentage like \"50%\"",
        )),
    }
}

fn float(ctx: &Ctx, f: RawFloat, window_cwd: &Path) -> Result<FloatPlan, LayoutError> {
    if let Some(b) = &f.border
        && !BORDERS.contains(&b.get_ref().as_str())
    {
        return Err(ctx.err(
            b.span(),
            format!("border must be one of {}", BORDERS.join(", ")),
        ));
    }
    Ok(FloatPlan {
        cwd: ctx.join(window_cwd, f.cwd.as_deref()),
        run: run(ctx, f.cmd.as_ref(), f.exec.as_ref())?,
        env: env(ctx, f.env.as_ref())?,
        x: extent(ctx, "x", f.x.as_ref(), false)?,
        y: extent(ctx, "y", f.y.as_ref(), false)?,
        width: extent(ctx, "width", f.width.as_ref(), true)?,
        height: extent(ctx, "height", f.height.as_ref(), true)?,
        border: f.border.map(|b| b.into_inner()),
        title: f.title,
        focus: f.focus,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(text: &str) -> Result<Plan, LayoutError> {
        parse(
            text,
            Path::new("/l/dev.toml"),
            Some(Path::new("/home/u")),
            Path::new("/work"),
        )
    }

    #[test]
    fn a_single_session_gets_the_file_name_and_defaults() {
        let plan = parse_str("[[window]]\n").unwrap();
        let s = &plan.sessions[0];
        assert_eq!(s.name, "dev");
        assert_eq!(s.root, Path::new("/work"));
        assert!(s.attach);
        assert_eq!(s.windows[0].panes.len(), 1, "a window gets one shell");
        assert_eq!(s.windows[0].panes[0].run, Run::Shell);
    }

    #[test]
    fn paths_resolve_against_root_window_and_pane() {
        let plan = parse_str(
            "root = \"~/code\"\n[[window]]\ncwd = \"sub\"\n[[window.pane]]\ncwd = \"x\"\n[[window.pane]]\ncwd = \"/abs\"\n[[window.pane]]\ncwd = \"~/h\"\n",
        )
        .unwrap();
        let w = &plan.sessions[0].windows[0];
        assert_eq!(w.cwd, Path::new("/home/u/code/sub"));
        assert_eq!(w.panes[0].cwd, Path::new("/home/u/code/sub/x"));
        assert_eq!(w.panes[1].cwd, Path::new("/abs"));
        assert_eq!(w.panes[2].cwd, Path::new("/home/u/h"));
    }

    #[test]
    fn cmd_and_exec() {
        let plan = parse_str(
            "[[window]]\n[[window.pane]]\ncmd = \"a\"\n[[window.pane]]\ncmd = \"b\"\nexec = true\n",
        )
        .unwrap();
        let p = &plan.sessions[0].windows[0].panes;
        assert_eq!(p[0].run, Run::Typed("a".into()));
        assert_eq!(p[1].run, Run::Exec("b".into()));
    }

    #[test]
    fn wait_defaults_to_thirty_seconds() {
        let plan = parse_str("[[window]]\n[[window.pane]]\nwait = { path = \"f\" }\n").unwrap();
        let w = plan.sessions[0].windows[0].panes[0].wait.as_ref().unwrap();
        assert_eq!(w.timeout, Duration::from_secs(30));
        assert_eq!(w.what, WaitFor::Path(PathBuf::from("/work/f")));
    }

    #[test]
    fn a_wait_timeout_that_would_overflow_the_deadline_is_refused() {
        let t = format!(
            "[[window]]\n[[window.pane]]\nwait = {{ path = \"f\", timeout = {} }}\n",
            i64::MAX
        );
        let e = parse_str(&t).unwrap_err();
        assert!(e.message.contains("at most 86400"), "{e}");
        assert_eq!(e.line, 3);
        let t = "[[window]]\n[[window.pane]]\nwait = { path = \"f\", timeout = 86400 }\n";
        assert!(parse_str(t).is_ok());
    }

    #[test]
    fn errors_name_the_line() {
        let e = parse_str("name = \"x\"\n\n[[window]]\nnmae = \"y\"\n").unwrap_err();
        assert_eq!(e.line, 4);
        assert!(e.message.contains("unknown field"), "{e}");
        assert_eq!(e.to_string(), format!("/l/dev.toml:4: {}", e.message));
    }

    #[test]
    fn top_level_keys_next_to_session_tables_are_unknown() {
        let e = parse_str("name = \"x\"\n[[session]]\nname = \"a\"\n").unwrap_err();
        assert!(e.message.contains("unknown field"), "{e}");
    }

    #[test]
    fn sizes_and_float_geometry() {
        for ok in ["\"30%\"", "20", "\"20\""] {
            let t = format!("[[window]]\n[[window.pane]]\n[[window.pane]]\nsize = {ok}\n");
            assert!(parse_str(&t).is_ok(), "{ok}");
        }
        for bad in ["\"0%\"", "\"100%\"", "0", "\"x\"", "true", "-3"] {
            let t = format!("[[window]]\n[[window.pane]]\n[[window.pane]]\nsize = {bad}\n");
            assert!(parse_str(&t).is_err(), "{bad}");
        }
        let plan = parse_str(
            "[[window]]\n[[window.float]]\nx = -2\ny = \"50%\"\nwidth = \"100%\"\nheight = 5\n",
        )
        .unwrap();
        let f = &plan.sessions[0].windows[0].floats[0];
        assert_eq!(
            (
                f.x.as_deref(),
                f.y.as_deref(),
                f.width.as_deref(),
                f.height.as_deref()
            ),
            (Some("-2"), Some("50%"), Some("100%"), Some("5"))
        );
        assert!(parse_str("[[window]]\n[[window.float]]\nwidth = 0\n").is_err());
        assert!(parse_str("[[window]]\n[[window.float]]\nx = \"101%\"\n").is_err());
    }

    #[test]
    fn a_layout_string_needs_enough_cells() {
        let one = "b260,80x24,0,0,3";
        let t = format!("[[window]]\nlayout = \"{one}\"\n[[window.pane]]\n[[window.pane]]\n");
        let e = parse_str(&t).unwrap_err();
        assert!(e.message.contains("1 cells"), "{e}");
        assert!(parse_str(&format!("[[window]]\nlayout = \"{one}\"\n")).is_ok());
        assert!(parse_str("[[window]]\nlayout = \"b261,80x24,0,0,3\"\n").is_err());
    }

    #[test]
    fn env_values_stay_literal() {
        let plan = parse_str("env = { A = \"$(x) `y` $HOME\" }\n").unwrap();
        assert_eq!(
            plan.sessions[0].env,
            [("A".to_string(), "$(x) `y` $HOME".to_string())]
        );
    }
}
