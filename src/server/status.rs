//! Renders `status.left/right`, `window-status-format` and
//! `window-status-current-format` to a [`StyledLine`] (spec §4.10).
//!
//! strftime sequences (`%H:%M`) expand first, over the raw template, and
//! `#{...}` expansion (including inside `#[...]` style directives)
//! follows, as tmux 3.7b does; option values and format
//! variables are resolved by the caller (`FormatVars`), so this module
//! stays independent of `Model`/`PaneHandle` and is unit-testable on its
//! own. tmux's own option names (`status-left`, `window-status-format`,
//! ...) map onto [`StatusInputs`]'s fields; wiring those option lookups
//! and per-window `FormatCtx`s onto the model is the render/attach path's
//! job (T2.5a).
//!
//! TC.11: a row is laid out by [`draw`], a port of tmux 3.7b's
//! `format_draw` (format-draw.c): `#[align=...]` sections, the window
//! list (`#[list=...]`, trimmed around the current window with `<` and
//! `>` markers), `#[range=...]` click ranges, `#[fill=...]` and
//! `#[push-default]`/`#[pop-default]`. [`render`] builds tmux's default
//! `status-format[0]` from the options and draws it; [`draw_row`] draws a
//! `status-format[N]` row. When a row is too wide, tmux trims the centre,
//! then the window list, then status-right, then status-left, so the
//! left-hand side is what stays.

use nix::libc;
use unicode_width::UnicodeWidthChar;

use crate::cmd::{FormatVars, expand};
use crate::server::pane::CellFlags;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// How a run of status text looks: the same colour and attribute vocabulary
/// as a pane cell ([`crate::server::pane::SnapCell`]), so the renderer draws
/// status runs and pane cells the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    /// Bold, dim, italic, underline, reverse, strikeout and the rest.
    pub attrs: CellFlags,
    /// As [`crate::server::pane::SnapCell::underline_color`]: `None`
    /// underlines in `fg`.
    pub underline_color: Option<Color>,
}

impl Default for Style {
    /// The terminal's default colours, no attributes.
    fn default() -> Self {
        Style {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            attrs: CellFlags::empty(),
            underline_color: None,
        }
    }
}

/// A run of text in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// One status row, left to right, already laid out for the width it was
/// rendered for: left part, window list, padding and right part are all
/// spans. The renderer draws it from column 0 and clips at the client width.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StyledLine {
    pub spans: Vec<Span>,
}

/// A clickable part of a status row (`#[range=...]`): columns
/// `start..end` of the row it was drawn on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRange {
    pub kind: RangeKind,
    pub start: u16,
    pub end: u16,
}

/// What a `#[range=...]` names, as tmux's `style_range_type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeKind {
    Left,
    Right,
    Control(u32),
    Pane(u32),
    Window(u32),
    Session(u32),
    /// `range=user|<name>`: at most 15 bytes of the name, as tmux keeps.
    User(String),
}

impl RangeKind {
    /// `#{mouse_status_range}` for a click on this range, as tmux 3.7b's
    /// `format_cb_mouse_status_range` words it.
    pub fn mouse_status_range(&self) -> String {
        match self {
            RangeKind::Left => "left".to_string(),
            RangeKind::Right => "right".to_string(),
            RangeKind::Pane(_) => "pane".to_string(),
            RangeKind::Window(_) => "window".to_string(),
            RangeKind::Session(_) => "session".to_string(),
            RangeKind::User(name) => name.clone(),
            RangeKind::Control(n) => n.to_string(),
        }
    }

    /// tmux's `format_is_type`: whether a style still names this range.
    fn same(&self, other: &RangeKind) -> bool {
        match (self, other) {
            (RangeKind::Left, RangeKind::Left)
            | (RangeKind::Right, RangeKind::Right)
            | (RangeKind::Control(_), RangeKind::Control(_)) => true,
            (RangeKind::Pane(a), RangeKind::Pane(b))
            | (RangeKind::Window(a), RangeKind::Window(b))
            | (RangeKind::Session(a), RangeKind::Session(b)) => a == b,
            (RangeKind::User(a), RangeKind::User(b)) => a == b,
            _ => false,
        }
    }
}

/// One drawn status row: exactly as many columns as it was drawn for, and
/// the click ranges on it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusRow {
    pub line: StyledLine,
    pub ranges: Vec<StatusRange>,
}

impl StatusRow {
    /// The range under column `x`, as tmux's `style_ranges_get_range`.
    pub fn range_at(&self, x: u16) -> Option<&StatusRange> {
        self.ranges.iter().find(|r| x >= r.start && x < r.end)
    }
}

/// One entry in the window list: the raw `window-status-format` or
/// `window-status-current-format` value already chosen for this window
/// (the current window uses the current format; that choice is the
/// caller's, since it is the caller that knows which window is active),
/// that window's format variables, and the window options tmux's default
/// `status-format[0]` reads for it.
pub struct WindowStatus<'a> {
    pub format: &'a str,
    pub vars: &'a dyn FormatVars,
    /// `window_index`, for the window's `range=window|N`.
    pub index: u32,
    pub current: bool,
    /// `window_last_flag`.
    pub last: bool,
    /// `window_bell_flag`.
    pub bell: bool,
    /// `window_activity_flag` or `window_silence_flag`: both take
    /// `window-status-activity-style`.
    pub alert: bool,
    pub options: WindowOptions<'a>,
}

/// The window options the window list reads, raw: `window-status-separator`
/// and the `window-status-*-style` styles (each may hold `#{...}`).
#[derive(Debug, Clone, Copy)]
pub struct WindowOptions<'a> {
    pub separator: &'a str,
    pub style: &'a str,
    pub current_style: &'a str,
    pub last_style: &'a str,
    pub bell_style: &'a str,
    pub activity_style: &'a str,
}

impl Default for WindowOptions<'_> {
    /// tmux 3.7b's defaults.
    fn default() -> Self {
        WindowOptions {
            separator: " ",
            style: "default",
            current_style: "default",
            last_style: "default",
            bell_style: "reverse",
            activity_style: "reverse",
        }
    }
}

/// Everything [`render`] needs to lay out one status line: raw option
/// values (tmux names: `status-left`, `status-right`,
/// `status-left-length`, `status-right-length`, `status-style`,
/// `status-left-style`, `status-right-style`, `status-justify`), the
/// format variables they expand against, the window list, the client
/// width to lay out for, and the broken-down time `%`-sequences expand
/// against (a fixed time in tests, so they never depend on the host clock
/// or `TZ`; the real local time in production).
pub struct StatusInputs<'a> {
    pub left: &'a str,
    pub right: &'a str,
    pub left_length: u16,
    pub right_length: u16,
    pub style: &'a str,
    pub left_style: &'a str,
    pub right_style: &'a str,
    /// `left`, `centre`, `right` or `absolute-centre`.
    pub justify: &'a str,
    pub vars: &'a dyn FormatVars,
    pub windows: &'a [WindowStatus<'a>],
    pub width: u16,
    pub now: libc::tm,
}

/// Renders tmux's default status row for `inputs.width` columns and
/// returns the line alone; [`render_row`] also returns its ranges.
pub fn render(inputs: &StatusInputs) -> StyledLine {
    render_row(inputs).line
}

/// Renders tmux 3.7b's default `status-format[0]`: `status-left` (trimmed
/// to `status-left-length`, 0 meaning no limit) in `range=left`, the
/// window list aligned by `status-justify`, each window in its
/// `range=window|N`, joined by `window-status-separator`, then
/// `status-right` (trimmed to `status-right-length`) in `range=right`.
/// The native format language has no window-list loop, so the row is
/// spelled here and then drawn by [`draw`], as tmux draws its own.
pub fn render_row(inputs: &StatusInputs) -> StatusRow {
    let vars = inputs.vars;
    let now = &inputs.now;
    let mut row = String::new();
    row.push_str(&format!(
        "#[align=left range=left {}]#[push-default]{}#[pop-default]#[norange default]",
        expand(inputs.left_style, vars),
        trim_to(&expand_time(inputs.left, vars, now), inputs.left_length),
    ));
    row.push_str(&format!(
        "#[list=on align={}]#[list=left-marker]<#[list=right-marker]>#[list=on]",
        inputs.justify
    ));
    for (i, w) in inputs.windows.iter().enumerate() {
        row.push_str(&window_entry(w, now));
        if i + 1 < inputs.windows.len() {
            row.push_str(&expand(w.options.separator, w.vars));
        }
    }
    row.push_str(&format!(
        "#[nolist align=right range=right {}]#[push-default]{}#[pop-default]#[norange default]",
        expand(inputs.right_style, vars),
        trim_to(&expand_time(inputs.right, vars, now), inputs.right_length),
    ));
    draw(&row, base_style(inputs.style, vars), inputs.width)
}

/// One window of tmux's default `status-format[0]` list, as its `#{W:}`
/// loop spells it: a bell takes `window-status-bell-style`, unless that is
/// `default`, and then an activity or silence alert takes
/// `window-status-activity-style`, unless that is `default` too.
fn window_entry(w: &WindowStatus, now: &libc::tm) -> String {
    let o = &w.options;
    let e = |s: &str| expand(s, w.vars);
    let is_default = |s: &str| s == "default";
    let mut style = if w.current {
        let current = e(o.current_style);
        let chosen = if is_default(&current) {
            e(o.style)
        } else {
            current
        };
        format!("list=focus {chosen}")
    } else {
        e(o.style)
    };
    let last = e(o.last_style);
    if w.last && !is_default(&last) {
        style.push(' ');
        style.push_str(&last);
    }
    let bell = e(o.bell_style);
    let activity = e(o.activity_style);
    if w.bell && !is_default(&bell) {
        style.push(' ');
        style.push_str(&bell);
    } else if w.alert && !is_default(&activity) {
        style.push(' ');
        style.push_str(&activity);
    }
    let close = if w.current {
        "#[norange list=on default]"
    } else {
        "#[norange default]"
    };
    format!(
        "#[range=window|{} {style}]#[push-default]{}#[pop-default]{close}",
        w.index,
        expand_time(w.format, w.vars, now)
    )
}

/// A `status-format[N]` row: its template with strftime and then `#{...}`
/// applied, drawn over `status-style`.
pub fn draw_row(
    template: &str,
    style: &str,
    vars: &dyn FormatVars,
    width: u16,
    now: &libc::tm,
) -> StatusRow {
    draw(
        &expand_time(template, vars, now),
        base_style(style, vars),
        width,
    )
}

/// A blank row in `status-style`: a status line with no `status-format`
/// entry for it, as tmux draws one.
pub fn blank_row(style: &str, vars: &dyn FormatVars, width: u16) -> StatusRow {
    draw("", base_style(style, vars), width)
}

/// A status-line message over a row, in `message-style` (tmux 3.7b's
/// default `message-format`, `#[<message-style>]#{message}`). Styles in
/// `text` are drawn; pass it through [`escape_hashes`] to show it as it
/// is, as tmux does for a command's error.
pub fn message_row(text: &str, style: &str, vars: &dyn FormatVars, width: u16) -> StatusRow {
    draw(
        &format!("#[{}]{}", expand(style, vars), text),
        Style::default(),
        width,
    )
}

/// `#` doubled, so [`draw`] shows it as text (tmux's
/// `status_prompt_escape`).
pub fn escape_hashes(text: &str) -> String {
    text.replace('#', "##")
}

/// A style option's value as the base cell of a row: `#{...}` expanded
/// first (tmux's `style_apply`), then parsed onto the terminal's default
/// cell. An invalid style gives the default cell, as tmux does.
pub fn base_style(value: &str, vars: &dyn FormatVars) -> Style {
    let mut sy = DrawStyle::new(Style::default());
    if parse_directive(&mut sy, &Style::default(), &expand(value, vars)) {
        sy.cell
    } else {
        Style::default()
    }
}

/// Whether `value` is a style tmux's `style_parse` takes (a style option
/// is checked when it is set, unless it holds a format).
pub fn style_valid(value: &str) -> bool {
    let mut sy = DrawStyle::new(Style::default());
    parse_directive(&mut sy, &Style::default(), value)
}

/// strftime over the raw template, then `#{...}` (tmux's
/// `format_expand_time`).
pub fn expand_time(fmt: &str, vars: &dyn FormatVars, now: &libc::tm) -> String {
    expand(&apply_strftime(fmt, now), vars)
}

/// tmux's `=N` trim for a `*-length` option: 0 leaves the text whole.
fn trim_to(expanded: &str, limit: u16) -> String {
    if limit == 0 {
        return expanded.to_string();
    }
    trim_left(expanded, usize::from(limit))
}

// ---- tmux's format_draw, from format-draw.c (tmux 3.7b) ----

/// One drawn column. A wide character is its cell plus a `width: 0` pad
/// after it, as tmux's grid keeps it.
#[derive(Debug, Clone)]
struct Cell {
    text: String,
    width: u8,
    style: Style,
}

impl Cell {
    fn blank(style: Style) -> Cell {
        Cell {
            text: " ".to_string(),
            width: 1,
            style,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Default,
    Left,
    Centre,
    Right,
    AbsoluteCentre,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListMode {
    Off,
    On,
    Focus,
    LeftMarker,
    RightMarker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefaultType {
    Base,
    Push,
    Pop,
    Set,
}

/// tmux's `struct style`: a cell style plus the drawing state a
/// `#[...]` directive can set.
#[derive(Debug, Clone)]
struct DrawStyle {
    cell: Style,
    fill: Option<Color>,
    align: Align,
    list: ListMode,
    range: Option<RangeKind>,
    default_type: DefaultType,
    ignore: bool,
}

impl DrawStyle {
    fn new(cell: Style) -> DrawStyle {
        DrawStyle {
            cell,
            fill: None,
            align: Align::Default,
            list: ListMode::Off,
            range: None,
            default_type: DefaultType::Base,
            ignore: false,
        }
    }
}

/// tmux's `colour_fromstring`: `None` for a word it refuses.
enum TmuxColour {
    Default,
    Colour(Color),
}

fn tmux_colour(s: &str) -> Option<TmuxColour> {
    use NamedColor::*;
    let lower = s.to_ascii_lowercase();
    if s.starts_with('#') {
        return (s.len() == 7)
            .then(|| parse_indexed_or_hex(s))
            .flatten()
            .map(TmuxColour::Colour);
    }
    for prefix in ["colour", "color"] {
        if let Some(n) = lower.strip_prefix(prefix) {
            return n
                .parse::<u8>()
                .ok()
                .map(|n| TmuxColour::Colour(Color::Indexed(n)));
        }
    }
    let named = match lower.as_str() {
        "default" => return Some(TmuxColour::Default),
        // ponytail: `terminal` is tmux's "the terminal's own colour"; the
        // default is the nearest grofe has.
        "terminal" => return Some(TmuxColour::Default),
        "black" | "0" => Black,
        "red" | "1" => Red,
        "green" | "2" => Green,
        "yellow" | "3" => Yellow,
        "blue" | "4" => Blue,
        "magenta" | "5" => Magenta,
        "cyan" | "6" => Cyan,
        "white" | "7" => White,
        "brightblack" | "90" => BrightBlack,
        "brightred" | "91" => BrightRed,
        "brightgreen" | "92" => BrightGreen,
        "brightyellow" | "93" => BrightYellow,
        "brightblue" | "94" => BrightBlue,
        "brightmagenta" | "95" => BrightMagenta,
        "brightcyan" | "96" => BrightCyan,
        "brightwhite" | "97" => BrightWhite,
        // ponytail: tmux also takes X11 colour names (`colour_byname`).
        _ => return None,
    };
    Some(TmuxColour::Colour(Color::Named(named)))
}

/// tmux's `attributes_fromstring`: the attribute bits a word (or
/// `|`-joined words) names; `None` for an unknown one. `acs`, `blink` and
/// `overline` are accepted and draw nothing: a cell has no bit for them.
fn tmux_attributes(s: &str) -> Option<CellFlags> {
    const DELIMS: &[char] = &[' ', ',', '|'];
    if s.is_empty() || s.starts_with(DELIMS) || s.ends_with(DELIMS) {
        return None;
    }
    if s.eq_ignore_ascii_case("default") || s.eq_ignore_ascii_case("none") {
        return Some(CellFlags::empty());
    }
    let mut attrs = CellFlags::empty();
    for word in s.split(DELIMS) {
        attrs |= match word.to_ascii_lowercase().as_str() {
            "acs" | "blink" | "overline" => CellFlags::empty(),
            "bright" | "bold" => CellFlags::BOLD,
            "dim" => CellFlags::DIM,
            "underscore" => CellFlags::UNDERLINE,
            "reverse" => CellFlags::INVERSE,
            "hidden" => CellFlags::HIDDEN,
            "italics" => CellFlags::ITALIC,
            "strikethrough" => CellFlags::STRIKEOUT,
            "double-underscore" => CellFlags::DOUBLE_UNDERLINE,
            "curly-underscore" => CellFlags::UNDERCURL,
            "dotted-underscore" => CellFlags::DOTTED_UNDERLINE,
            "dashed-underscore" => CellFlags::DASHED_UNDERLINE,
            _ => return None,
        };
    }
    Some(attrs)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// tmux's `style_parse`: applies a directive's words onto `sy`, `default`
/// meaning `base`. Any word it refuses leaves `sy` as it was and returns
/// false, so a bad directive changes nothing.
fn parse_directive(sy: &mut DrawStyle, base: &Style, input: &str) -> bool {
    let saved = sy.clone();
    for word in input.split([' ', ',', '\n']).filter(|w| !w.is_empty()) {
        if word.len() > 255 || !apply_word(sy, base, word) {
            *sy = saved;
            return false;
        }
    }
    true
}

fn apply_word(sy: &mut DrawStyle, base: &Style, word: &str) -> bool {
    let is = |w: &str| word.eq_ignore_ascii_case(w);
    if is("default") {
        sy.cell = *base;
    } else if is("ignore") {
        sy.ignore = true;
    } else if is("noignore") {
        sy.ignore = false;
    } else if is("push-default") {
        sy.default_type = DefaultType::Push;
    } else if is("pop-default") {
        sy.default_type = DefaultType::Pop;
    } else if is("set-default") {
        sy.default_type = DefaultType::Set;
    } else if is("nolist") {
        sy.list = ListMode::Off;
    } else if let Some(v) = strip_prefix_ci(word, "list=") {
        sy.list = match v.to_ascii_lowercase().as_str() {
            "on" => ListMode::On,
            "focus" => ListMode::Focus,
            "left-marker" => ListMode::LeftMarker,
            "right-marker" => ListMode::RightMarker,
            _ => return false,
        };
    } else if is("norange") {
        sy.range = None;
    } else if word.len() > 6
        && let Some(v) = strip_prefix_ci(word, "range=")
    {
        return parse_range(sy, v);
    } else if is("noalign") {
        sy.align = Align::Default;
    } else if word.len() > 6
        && let Some(v) = strip_prefix_ci(word, "align=")
    {
        sy.align = match v.to_ascii_lowercase().as_str() {
            "left" => Align::Left,
            "centre" => Align::Centre,
            "right" => Align::Right,
            "absolute-centre" => Align::AbsoluteCentre,
            _ => return false,
        };
    } else if word.len() > 5
        && let Some(v) = strip_prefix_ci(word, "fill=")
    {
        match tmux_colour(v) {
            None => return false,
            Some(TmuxColour::Default) => sy.fill = None,
            Some(TmuxColour::Colour(c)) => sy.fill = Some(c),
        }
    } else if word.len() > 3 && word.get(1..3).is_some_and(|g| g.eq_ignore_ascii_case("g=")) {
        let Some(value) = tmux_colour(&word[3..]) else {
            return false;
        };
        match word.as_bytes()[0].to_ascii_lowercase() {
            b'f' => {
                sy.cell.fg = match value {
                    TmuxColour::Default => base.fg,
                    TmuxColour::Colour(c) => c,
                }
            }
            b'b' => {
                sy.cell.bg = match value {
                    TmuxColour::Default => base.bg,
                    TmuxColour::Colour(c) => c,
                }
            }
            _ => return false,
        }
    } else if word.len() > 3
        && let Some(v) = strip_prefix_ci(word, "us=")
    {
        sy.cell.underline_color = match tmux_colour(v) {
            None => return false,
            Some(TmuxColour::Default) => base.underline_color,
            Some(TmuxColour::Colour(c)) => Some(c),
        };
    } else if is("none") {
        sy.cell.attrs = CellFlags::empty();
    } else if word.len() > 2
        && let Some(rest) = strip_prefix_ci(word, "no")
    {
        // `noattr` is tmux's "no attributes from this style"; grofe
        // has no such bit, so it draws as nothing.
        if rest != "attr" {
            let Some(attrs) = tmux_attributes(rest) else {
                return false;
            };
            sy.cell.attrs.remove(attrs);
        }
    } else if word.len() > 6
        && let Some(v) = strip_prefix_ci(word, "width=")
    {
        // Read only by the message area's geometry in tmux, not by a row.
        return v.strip_suffix('%').unwrap_or(v).parse::<u32>().is_ok();
    } else if word.len() > 4
        && let Some(v) = strip_prefix_ci(word, "pad=")
    {
        return v.parse::<u32>().is_ok();
    } else {
        let Some(attrs) = tmux_attributes(word) else {
            return false;
        };
        if attrs.intersects(CellFlags::ALL_UNDERLINES) {
            sy.cell.attrs.remove(CellFlags::ALL_UNDERLINES);
        }
        sy.cell.attrs.insert(attrs);
    }
    true
}

/// `range=<type>[|<argument>]`, as tmux's `style_parse`. An unknown type
/// is accepted and changes nothing, as in tmux.
fn parse_range(sy: &mut DrawStyle, v: &str) -> bool {
    let (kind, arg) = match v.split_once('|') {
        Some((_, "")) => return false,
        Some((k, a)) => (k, Some(a)),
        None => (v, None),
    };
    let number = |a: &str| a.strip_prefix('+').unwrap_or(a).parse::<u32>().ok();
    let range = match (kind.to_ascii_lowercase().as_str(), arg) {
        ("left", None) => RangeKind::Left,
        ("right", None) => RangeKind::Right,
        ("left" | "right", Some(_)) => return false,
        ("control", Some(a)) => match number(a) {
            Some(n) if n <= 9 => RangeKind::Control(n),
            _ => return false,
        },
        ("pane", Some(a)) => match a.strip_prefix('%').and_then(number) {
            Some(n) => RangeKind::Pane(n),
            None => return false,
        },
        ("window", Some(a)) => match number(a) {
            Some(n) => RangeKind::Window(n),
            None => return false,
        },
        ("session", Some(a)) => match a.strip_prefix('$').and_then(number) {
            Some(n) => RangeKind::Session(n),
            None => return false,
        },
        ("user", Some(a)) => {
            let mut end = a.len().min(15);
            while !a.is_char_boundary(end) {
                end -= 1;
            }
            RangeKind::User(a[..end].to_string())
        }
        ("control" | "pane" | "window" | "session" | "user", None) => return false,
        _ => return true,
    };
    sy.range = Some(range);
    true
}

/// tmux's `format_skip(s, "]")`: the index of the `]` that ends a style,
/// skipping `#{...}` and the escapes `#,` `##` `#{` `#}` `#:`.
fn skip_to_bracket(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
            depth += 1;
        }
        if b[i] == b'#' && b.get(i + 1).is_some_and(|c| b",#{}:".contains(c)) {
            i += 2;
            continue;
        }
        if b[i] == b'}' {
            depth -= 1;
        }
        if b[i] == b']' && depth == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Where a `#[range=...]` began: which screen, and its columns there.
#[derive(Debug, Clone)]
struct PendingRange {
    screen: usize,
    kind: RangeKind,
    start: usize,
    end: usize,
    /// Moved onto the row: a range whose screen is never drawn is dropped.
    placed: bool,
}

const LEFT: usize = 0;
const CENTRE: usize = 1;
const RIGHT: usize = 2;
const ABSOLUTE_CENTRE: usize = 3;
const LIST: usize = 4;
const LIST_LEFT: usize = 5;
const LIST_RIGHT: usize = 6;
const AFTER: usize = 7;

/// The screens a row is built in and the row it is drawn onto.
struct Canvas {
    row: Vec<Cell>,
    screens: [Vec<Cell>; 8],
    ranges: Vec<PendingRange>,
    /// What a screen's unwritten columns hold (tmux clears each with the
    /// base colour when it starts).
    blank: Style,
}

impl Canvas {
    fn width(&self, screen: usize) -> usize {
        self.screens[screen].len()
    }

    /// tmux's `format_draw_put`: `width` columns of `screen` from `start`,
    /// at row column `offset`, moving that screen's ranges along.
    fn put(&mut self, screen: usize, offset: usize, start: usize, width: usize) {
        self.copy(screen, offset, start, width);
        for r in self.ranges.iter_mut().filter(|r| r.screen == screen) {
            if r.end <= start || r.start >= start + width {
                r.start = r.end;
                continue;
            }
            r.start = r.start.max(start) - start + offset;
            r.end = r.end.min(start + width) - start + offset;
            r.placed = true;
        }
        self.ranges
            .retain(|r| r.screen != screen || (r.placed && r.start < r.end));
    }

    /// The copy alone, as for the list markers, whose ranges tmux leaves.
    fn copy(&mut self, screen: usize, offset: usize, start: usize, width: usize) {
        for k in 0..width {
            let Some(slot) = self.row.get_mut(offset + k) else {
                break;
            };
            *slot = self.screens[screen]
                .get(start + k)
                .cloned()
                .unwrap_or_else(|| Cell::blank(self.blank));
        }
    }

    /// tmux's `format_draw_put_list`: the list at `offset`, trimmed to
    /// `width` around the focus (`focus` is its start and end column),
    /// with the `<` and `>` markers where it was cut.
    fn put_list(&mut self, mut offset: usize, mut width: usize, focus: (usize, usize)) {
        let list = self.width(LIST);
        if width >= list {
            self.put(LIST, offset, 0, width);
            return;
        }
        let centre = focus.0 + focus.1.saturating_sub(focus.0) / 2;
        let mut start = centre.saturating_sub(width / 2);
        if centre < width / 2 {
            start = 0;
        }
        if start + width > list {
            start = list - width;
        }
        let (left, right) = (self.width(LIST_LEFT), self.width(LIST_RIGHT));
        if start != 0 && width > left {
            self.copy(LIST_LEFT, offset, 0, left);
            offset += left;
            start += left;
            width -= left;
        }
        if start + width < list && width > right {
            self.copy(LIST_RIGHT, offset + width - right, 0, right);
            width -= right;
        }
        self.put(LIST, offset, start, width);
    }

    /// tmux's `format_draw_none`: left, right, centre and absolute
    /// centre with no list, trimming the centre, then the right, then
    /// the left.
    fn draw_none(&mut self, available: usize) {
        let (mut wl, mut wc, mut wr, mut wa) = (
            self.width(LEFT),
            self.width(CENTRE),
            self.width(RIGHT),
            self.width(ABSOLUTE_CENTRE),
        );
        while wl + wc + wr > available {
            if wc > 0 {
                wc -= 1;
            } else if wr > 0 {
                wr -= 1;
            } else {
                wl -= 1;
            }
        }
        self.put(LEFT, 0, 0, wl);
        self.put(RIGHT, available - wr, self.width(RIGHT) - wr, wr);
        let centre_at = (wl + ((available - wr) - wl) / 2).saturating_sub(wc / 2);
        self.put(
            CENTRE,
            centre_at,
            (self.width(CENTRE) / 2).saturating_sub(wc / 2),
            wc,
        );
        wa = wa.min(available);
        self.put(ABSOLUTE_CENTRE, (available - wa) / 2, 0, wa);
    }

    /// tmux's `format_draw_left`: the list follows the left part.
    fn draw_left(&mut self, available: usize, focus: Option<(usize, usize)>) {
        let [mut wleft, mut wc, mut wr, wa0, mut wlist, mut wafter] = self.widths();
        while wleft + wc + wr + wlist + wafter > available {
            if wc > 0 {
                wc -= 1;
            } else if wlist > 0 {
                wlist -= 1;
            } else if wr > 0 {
                wr -= 1;
            } else if wafter > 0 {
                wafter -= 1;
            } else {
                wleft -= 1;
            }
        }
        if wlist == 0 {
            // tmux copies what follows the list after the left part
            // without moving its cursor, so it is never drawn.
            self.draw_none(available);
            return;
        }
        self.put(LEFT, 0, 0, wleft);
        self.put(RIGHT, available - wr, self.width(RIGHT) - wr, wr);
        self.put(AFTER, wleft + wlist, 0, wafter);
        let used = wleft + wlist + wafter;
        let centre_at = (used + ((available - wr) - used) / 2).saturating_sub(wc / 2);
        self.put(
            CENTRE,
            centre_at,
            (self.width(CENTRE) / 2).saturating_sub(wc / 2),
            wc,
        );
        self.put_list(wleft, wlist, focus.unwrap_or((0, 0)));
        let wa = wa0.min(available);
        self.put(ABSOLUTE_CENTRE, (available - wa) / 2, 0, wa);
    }

    /// tmux's `format_draw_centre`: the list in the middle of the space
    /// between the left and right parts.
    fn draw_centre(&mut self, available: usize, focus: Option<(usize, usize)>) {
        let [mut wleft, mut wc, mut wr, wa0, mut wlist, mut wafter] = self.widths();
        while wleft + wc + wr + wlist + wafter > available {
            if wlist > 0 {
                wlist -= 1;
            } else if wafter > 0 {
                wafter -= 1;
            } else if wc > 0 {
                wc -= 1;
            } else if wr > 0 {
                wr -= 1;
            } else {
                wleft -= 1;
            }
        }
        if wlist == 0 {
            self.draw_none(available);
            return;
        }
        self.put(LEFT, 0, 0, wleft);
        self.put(RIGHT, available - wr, self.width(RIGHT) - wr, wr);
        let middle = wleft + ((available - wr) - wleft) / 2;
        self.put(CENTRE, middle.saturating_sub(wlist / 2 + wc), 0, wc);
        self.put(AFTER, middle - wlist / 2 + wlist, 0, wafter);
        let list = self.width(LIST);
        self.put_list(
            middle - wlist / 2,
            wlist,
            focus.unwrap_or((list / 2, list / 2)),
        );
        let wa = wa0.min(available);
        self.put(ABSOLUTE_CENTRE, (available - wa) / 2, 0, wa);
    }

    /// tmux's `format_draw_right`: the list just before the right part.
    fn draw_right(&mut self, available: usize, focus: Option<(usize, usize)>) {
        let [mut wleft, mut wc, mut wr, wa0, mut wlist, mut wafter] = self.widths();
        while wleft + wc + wr + wlist + wafter > available {
            if wc > 0 {
                wc -= 1;
            } else if wlist > 0 {
                wlist -= 1;
            } else if wr > 0 {
                wr -= 1;
            } else if wafter > 0 {
                wafter -= 1;
            } else {
                wleft -= 1;
            }
        }
        if wlist == 0 {
            self.draw_none(available);
            return;
        }
        self.put(LEFT, 0, 0, wleft);
        self.put(
            AFTER,
            available - wafter,
            self.width(AFTER) - wafter,
            wafter,
        );
        self.put(RIGHT, available - wr - wlist - wafter, 0, wr);
        let right_at = available - wr - wlist - wafter;
        let centre_at = (wleft + (right_at - wleft) / 2).saturating_sub(wc / 2);
        self.put(
            CENTRE,
            centre_at,
            (self.width(CENTRE) / 2).saturating_sub(wc / 2),
            wc,
        );
        self.put_list(available - wlist - wafter, wlist, focus.unwrap_or((0, 0)));
        let wa = wa0.min(available);
        self.put(ABSOLUTE_CENTRE, (available - wa) / 2, 0, wa);
    }

    /// tmux's `format_draw_absolute_centre`: the list (with the absolute
    /// centre part before it) in the middle of the whole row.
    fn draw_absolute_centre(&mut self, available: usize, focus: Option<(usize, usize)>) {
        let [mut wleft, mut wc, mut wr, mut wa, mut wlist, mut wafter] = self.widths();
        while wleft + wc + wr > available {
            if wc > 0 {
                wc -= 1;
            } else if wr > 0 {
                wr -= 1;
            } else {
                wleft -= 1;
            }
        }
        while wlist + wafter + wa > available {
            if wlist > 0 {
                wlist -= 1;
            } else if wafter > 0 {
                wafter -= 1;
            } else {
                wa -= 1;
            }
        }
        self.put(LEFT, 0, 0, wleft);
        self.put(RIGHT, available - wr, self.width(RIGHT) - wr, wr);
        let middle = wleft + ((available - wr) - wleft) / 2;
        self.put(CENTRE, middle.saturating_sub(wc), 0, wc);
        let list = self.width(LIST);
        let focus = focus.unwrap_or((list / 2, list / 2));
        let mut at = (available - wlist - wa) / 2;
        self.put(ABSOLUTE_CENTRE, at, 0, wa);
        at += wa;
        self.put_list(at, wlist, focus);
        at += wlist;
        self.put(AFTER, at, 0, wafter);
    }

    /// The widths the layouts start from: `[left, centre, right,
    /// absolute centre, list, after]`.
    fn widths(&self) -> [usize; 6] {
        [LEFT, CENTRE, RIGHT, ABSOLUTE_CENTRE, LIST, AFTER].map(|s| self.width(s))
    }
}

fn push_char(screen: &mut Vec<Cell>, ch: char, style: Style) {
    let width = char_width(ch);
    if width == 0 {
        if let Some(last) = screen.iter_mut().rev().find(|c| c.width > 0) {
            last.text.push(ch);
        }
        return;
    }
    screen.push(Cell {
        text: ch.to_string(),
        width: width as u8,
        style,
    });
    if width == 2 {
        screen.push(Cell {
            text: String::new(),
            width: 0,
            style: Style::default(),
        });
    }
}

/// Draws an expanded format onto a row `width` columns wide, first filled
/// with `base`: tmux 3.7b's `format_draw`. See the module doc for what the
/// directives do; a `#[` with no closing `]` leaves the row blank, as in
/// tmux.
pub fn draw(expanded: &str, base: Style, width: u16) -> StatusRow {
    draw_on(
        expanded,
        base,
        vec![Cell::blank(base); usize::from(width)],
        false,
    )
}

/// [`draw`] onto cells that already hold something (a pane border's line
/// glyphs): columns the format writes nothing to keep what they had, as
/// tmux's pane status line draws over its border.
pub fn draw_over(expanded: &str, base: Style, under: &[(char, Style)]) -> StatusRow {
    let row = under
        .iter()
        .map(|(c, style)| Cell {
            text: c.to_string(),
            width: 1,
            style: *style,
        })
        .collect();
    draw_on(expanded, base, row, true)
}

/// `keep_cut`: a wide character the width cuts in two (or a right half left
/// without its character) is not drawn, and what was under it shows, as
/// tmux's pane status line leaves it.
fn draw_on(expanded: &str, base: Style, row: Vec<Cell>, keep_cut: bool) -> StatusRow {
    let available = row.len();
    let under = keep_cut.then(|| row.clone());
    let mut cv = Canvas {
        row,
        screens: Default::default(),
        ranges: Vec::new(),
        blank: Style {
            bg: base.bg,
            ..Style::default()
        },
    };
    let mut base_default = base;
    let mut current_default = base;
    let mut sy = DrawStyle::new(current_default);
    let mut current = LEFT;
    // By alignment: default, left, centre, right, absolute centre.
    let mut map = [LEFT, LEFT, CENTRE, RIGHT, ABSOLUTE_CENTRE];
    let (mut focus_start, mut focus_end): (Option<usize>, Option<usize>) = (None, None);
    let mut list_state: i8 = -1;
    let mut list_align = Align::Default;
    let mut fill: Option<Color> = None;
    let mut fr: Option<PendingRange> = None;

    let b = expanded.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // Runs of `#` not starting a style.
        if b[i] == b'#' && i + 1 < b.len() && b[i + 1] != b'[' {
            let mut n = 1;
            while b.get(i + n) == Some(&b'#') {
                n += 1;
            }
            let even = n % 2 == 0;
            if b.get(i + n) != Some(&b'[') {
                i += n;
                for _ in 0..n.div_ceil(2) {
                    push_char(&mut cv.screens[current], '#', sy.cell);
                }
                continue;
            }
            i += if even { n + 1 } else { n - 1 };
            if sy.ignore {
                continue;
            }
            for _ in 0..n / 2 {
                push_char(&mut cv.screens[current], '#', sy.cell);
            }
            if even {
                push_char(&mut cv.screens[current], '[', sy.cell);
            }
            continue;
        }

        if b[i] != b'#' || b.get(i + 1) != Some(&b'[') || sy.ignore {
            let Some(ch) = expanded[i..].chars().next() else {
                break;
            };
            i += ch.len_utf8();
            if ch.is_control() {
                continue;
            }
            push_char(&mut cv.screens[current], ch, sy.cell);
            continue;
        }

        let Some(end) = skip_to_bracket(&expanded[i + 2..]) else {
            return StatusRow {
                line: to_line(&cv.row),
                ranges: Vec::new(),
            };
        };
        let directive = &expanded[i + 2..i + 2 + end];
        i += 2 + end + 1;
        let saved = sy.clone();
        if !parse_directive(&mut sy, &current_default, directive) {
            continue;
        }
        if let Some(f) = sy.fill {
            fill = Some(f);
        }
        match sy.default_type {
            DefaultType::Push => current_default = saved.cell,
            DefaultType::Pop => current_default = base_default,
            DefaultType::Set => {
                base_default = saved.cell;
                current_default = saved.cell;
            }
            DefaultType::Base => {}
        }
        sy.default_type = DefaultType::Base;

        match sy.list {
            ListMode::On => {
                if list_state != 0 {
                    fr = None;
                    list_state = 0;
                    list_align = sy.align;
                }
                if focus_start.is_some() && focus_end.is_none() {
                    focus_end = Some(cv.width(LIST));
                }
                current = LIST;
            }
            ListMode::Focus => {
                if list_state == 0 && focus_start.is_none() {
                    focus_start = Some(cv.width(LIST));
                }
            }
            ListMode::Off => {
                if list_state == 0 {
                    fr = None;
                    if focus_start.is_some() && focus_end.is_none() {
                        focus_end = Some(cv.width(LIST));
                    }
                    map[align_index(list_align)] = AFTER;
                    if list_align == Align::Left {
                        map[align_index(Align::Default)] = AFTER;
                    }
                    list_state = 1;
                }
                current = map[align_index(sy.align)];
            }
            ListMode::LeftMarker | ListMode::RightMarker => {
                let marker = if sy.list == ListMode::LeftMarker {
                    LIST_LEFT
                } else {
                    LIST_RIGHT
                };
                if list_state == 0 && cv.width(marker) == 0 {
                    fr = None;
                    if focus_start.is_some() && focus_end.is_none() {
                        focus_start = None;
                        focus_end = None;
                    }
                    current = marker;
                }
            }
        }

        if let Some(r) = &fr
            && !sy.range.as_ref().is_some_and(|k| r.kind.same(k))
        {
            // As tmux: the range ends one column past where the directive
            // is, and on the current screen, whichever the range began on.
            let cx = cv.width(current);
            if cx != r.start {
                let mut done = r.clone();
                done.end = cx + 1;
                cv.ranges.push(done);
            }
            fr = None;
        }
        if fr.is_none()
            && let Some(kind) = &sy.range
        {
            fr = Some(PendingRange {
                screen: current,
                kind: kind.clone(),
                start: cv.width(current),
                end: 0,
                placed: false,
            });
        }
    }

    if let Some(bg) = fill {
        let style = Style {
            bg,
            ..Style::default()
        };
        cv.row = vec![Cell::blank(style); available];
    }
    let focus = focus_start.zip(focus_end);
    match list_align {
        Align::Default => cv.draw_none(available),
        Align::Left => cv.draw_left(available, focus),
        Align::Centre => cv.draw_centre(available, focus),
        Align::Right => cv.draw_right(available, focus),
        Align::AbsoluteCentre => cv.draw_absolute_centre(available, focus),
    }
    if let Some(under) = under {
        #[allow(clippy::needless_range_loop)]
        for x in 0..available {
            let w = cv.row[x].width;
            let whole = if w == 2 {
                cv.row.get(x + 1).is_some_and(|n| n.width == 0)
            } else {
                w != 0 || (x > 0 && cv.row[x - 1].width == 2)
            };
            if !whole {
                cv.row[x] = under[x].clone();
            }
        }
    }
    let ranges = cv
        .ranges
        .iter()
        .filter(|r| r.placed)
        .map(|r| StatusRange {
            kind: r.kind.clone(),
            start: r.start.min(available) as u16,
            end: r.end.min(available) as u16,
        })
        .collect();
    StatusRow {
        line: to_line(&cv.row),
        ranges,
    }
}

fn align_index(a: Align) -> usize {
    match a {
        Align::Default => 0,
        Align::Left => 1,
        Align::Centre => 2,
        Align::Right => 3,
        Align::AbsoluteCentre => 4,
    }
}

/// The row as spans. A wide character cut from its right half draws as a
/// space in its own style, a right half cut from its character as a space
/// in the terminal's default colours (tmux's padding cell), as tmux 3.7b
/// shows both.
fn to_line(row: &[Cell]) -> StyledLine {
    let mut spans: Vec<Span> = Vec::new();
    let mut push = |text: &str, style: Style| match spans.last_mut() {
        Some(last) if last.style == style => last.text.push_str(text),
        _ => spans.push(Span {
            text: text.to_string(),
            style,
        }),
    };
    for (x, cell) in row.iter().enumerate() {
        match cell.width {
            2 if row.get(x + 1).is_some_and(|n| n.width == 0) => push(&cell.text, cell.style),
            2 => push(" ", cell.style),
            0 if x > 0 && row[x - 1].width == 2 => {}
            0 => push(" ", Style::default()),
            _ => push(&cell.text, cell.style),
        }
    }
    StyledLine { spans }
}

/// tmux's `format_trim_left`: the first `limit` columns of an expanded
/// format, keeping its `#[...]` directives (up to where the limit is
/// reached) and counting `##` as one column.
pub fn trim_left(expanded: &str, limit: usize) -> String {
    let b = expanded.as_bytes();
    let mut out = String::new();
    let mut width = 0;
    let mut i = 0;
    while i < b.len() {
        if width >= limit {
            break;
        }
        if b[i] == b'#' {
            let mut n = 0;
            while b.get(i + n) == Some(&b'#') {
                n += 1;
            }
            let (mut lead, end) = if b.get(i + n) != Some(&b'[') {
                (n.div_ceil(2), i + n)
            } else if n % 2 == 0 {
                (n / 2, i + n)
            } else {
                (n / 2, i + n - 1)
            };
            lead = lead.min(limit - width);
            if lead != 0 {
                if n == 1 {
                    out.push('#');
                } else {
                    out.push_str(&"#".repeat(2 * lead));
                }
                width += lead;
            }
            i = end;
            if b.get(i) == Some(&b'#') {
                let Some(close) = skip_to_bracket(&expanded[i + 2..]) else {
                    break;
                };
                let stop = i + 2 + close + 1;
                out.push_str(&expanded[i..stop]);
                i = stop;
            }
            continue;
        }
        let Some(ch) = expanded[i..].chars().next() else {
            break;
        };
        i += ch.len_utf8();
        if ch.is_control() {
            continue;
        }
        let w = char_width(ch);
        if width + w <= limit {
            out.push(ch);
        }
        width += w;
    }
    out
}

/// Parses a tmux style value (`status-style`'s own syntax, and a
/// `#[...]` directive's content) onto `style`.
pub(crate) fn apply_style_directive(style: &mut Style, directive: &str) {
    for item in directive.split(',') {
        let item = item.trim();
        if let Some(v) = item.strip_prefix("fg=") {
            style.fg = named_default_or(v, NamedColor::Foreground, style.fg);
        } else if let Some(v) = item.strip_prefix("bg=") {
            style.bg = named_default_or(v, NamedColor::Background, style.bg);
        } else if let Some(v) = item.strip_prefix("us=") {
            style.underline_color = if v.eq_ignore_ascii_case("default") {
                None
            } else {
                parse_color(v)
            };
        } else {
            apply_attr_keyword(&mut style.attrs, item);
        }
    }
}

/// `parse_style` is `apply_style_directive` starting from
/// [`Style::default`], for a bare option value like `status-style`'s.
pub fn parse_style(value: &str) -> Style {
    let mut style = Style::default();
    apply_style_directive(&mut style, value);
    style
}

fn named_default_or(v: &str, default: NamedColor, current: Color) -> Color {
    if v.eq_ignore_ascii_case("default") {
        Color::Named(default)
    } else {
        parse_color(v).unwrap_or(current)
    }
}

/// A tmux colour word: the 16 ANSI names, `colour0`..`colour255` /
/// `color0`..`color255`, or `#rrggbb`. `default` is handled by the caller
/// (it means different things for `fg=`/`bg=`/`us=`). An unrecognised
/// word leaves the current colour unchanged, like an unknown format
/// variable expands to nothing rather than erroring.
fn parse_color(s: &str) -> Option<Color> {
    let named = match s.to_ascii_lowercase().as_str() {
        "black" => NamedColor::Black,
        "red" => NamedColor::Red,
        "green" => NamedColor::Green,
        "yellow" => NamedColor::Yellow,
        "blue" => NamedColor::Blue,
        "magenta" => NamedColor::Magenta,
        "cyan" => NamedColor::Cyan,
        "white" => NamedColor::White,
        "brightblack" => NamedColor::BrightBlack,
        "brightred" => NamedColor::BrightRed,
        "brightgreen" => NamedColor::BrightGreen,
        "brightyellow" => NamedColor::BrightYellow,
        "brightblue" => NamedColor::BrightBlue,
        "brightmagenta" => NamedColor::BrightMagenta,
        "brightcyan" => NamedColor::BrightCyan,
        "brightwhite" => NamedColor::BrightWhite,
        _ => return parse_indexed_or_hex(s),
    };
    Some(Color::Named(named))
}

fn parse_indexed_or_hex(s: &str) -> Option<Color> {
    if let Some(n) = s.strip_prefix("colour").or_else(|| s.strip_prefix("color")) {
        return n.parse::<u8>().ok().map(Color::Indexed);
    }
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Color::Spec(Rgb {
        r: byte(0)?,
        g: byte(2)?,
        b: byte(4)?,
    }))
}

/// tmux style attribute keywords: `no`-prefixed clears, the four
/// underline styles share one slot (setting one clears the others), and
/// `none` resets every attribute. An unrecognised word is ignored, the
/// same forgiving handling as the rest of format expansion.
// ponytail: `blink`/`noblink` are accepted (fall into the `_` arm, a
// no-op) rather than rejected: alacritty's cell `Flags` has no blink bit
// to set.
fn apply_attr_keyword(attrs: &mut CellFlags, word: &str) {
    match word.to_ascii_lowercase().as_str() {
        "bold" => attrs.insert(CellFlags::BOLD),
        "nobold" => attrs.remove(CellFlags::BOLD),
        "dim" => attrs.insert(CellFlags::DIM),
        "nodim" => attrs.remove(CellFlags::DIM),
        "italics" => attrs.insert(CellFlags::ITALIC),
        "noitalics" => attrs.remove(CellFlags::ITALIC),
        "reverse" => attrs.insert(CellFlags::INVERSE),
        "noreverse" => attrs.remove(CellFlags::INVERSE),
        "hidden" => attrs.insert(CellFlags::HIDDEN),
        "nohidden" => attrs.remove(CellFlags::HIDDEN),
        "strikethrough" => attrs.insert(CellFlags::STRIKEOUT),
        "nostrikethrough" => attrs.remove(CellFlags::STRIKEOUT),
        "underscore" => set_underline(attrs, CellFlags::UNDERLINE),
        "double-underscore" => set_underline(attrs, CellFlags::DOUBLE_UNDERLINE),
        "curly-underscore" => set_underline(attrs, CellFlags::UNDERCURL),
        "dotted-underscore" => set_underline(attrs, CellFlags::DOTTED_UNDERLINE),
        "dashed-underscore" => set_underline(attrs, CellFlags::DASHED_UNDERLINE),
        "nounderscore" => attrs.remove(CellFlags::ALL_UNDERLINES),
        "none" => *attrs = CellFlags::empty(),
        _ => {}
    }
}

fn set_underline(attrs: &mut CellFlags, style: CellFlags) {
    attrs.remove(CellFlags::ALL_UNDERLINES);
    attrs.insert(style);
}

/// Applies strftime `%` sequences (spec §4.10) over `text`, so a clock
/// needs no subprocess. A no-`%` fast path skips the libc round trip.
fn apply_strftime(text: &str, tm: &libc::tm) -> String {
    if !text.contains('%') {
        return text.to_string();
    }
    let Ok(cformat) = std::ffi::CString::new(text) else {
        return text.to_string();
    };
    // 4096 bytes is far beyond any status-line-sized template (the
    // longest built-in default, `status-right`, is a few dozen bytes);
    // `strftime` returns 0 both for "result didn't fit" and for a
    // legitimately empty result, so a generous fixed buffer avoids ever
    // having to tell those two apart.
    let mut buf = vec![0u8; 4096];
    // SAFETY: `buf` outlives the call and its length is passed as `max`;
    // `cformat` is a valid NUL-terminated C string; `tm` is a valid,
    // fully-initialized `libc::tm` for the duration of the call.
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            cformat.as_ptr(),
            tm,
        )
    };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
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

    /// A broken-down time, built by hand rather than sampled from the
    /// clock or converted via `TZ`, so strftime tests are deterministic
    /// on any host.
    fn fixed_tm(year: i32, month: i32, day: i32, hour: i32, min: i32, sec: i32) -> libc::tm {
        // SAFETY: `libc::tm` is a plain-old-data struct; a zeroed value
        // is valid, and every field strftime reads for `%H%M%S%d%m%y%b`
        // is set explicitly below.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = year - 1900;
        tm.tm_mon = month - 1;
        tm.tm_mday = day;
        tm.tm_hour = hour;
        tm.tm_min = min;
        tm.tm_sec = sec;
        tm
    }

    fn plain_text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn no_windows() -> &'static [WindowStatus<'static>] {
        &[]
    }

    fn window<'a>(
        format: &'a str,
        vars: &'a dyn FormatVars,
        index: u32,
        current: bool,
    ) -> WindowStatus<'a> {
        WindowStatus {
            format,
            vars,
            index,
            current,
            last: false,
            bell: false,
            alert: false,
            options: WindowOptions::default(),
        }
    }

    #[test]
    fn conditional_and_user_option_render() {
        let v = vars(&[("session_name", "work"), ("@x", "1")]);
        let inputs = StatusInputs {
            left: "[#{session_name}] #{?@x,on,off}",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 80,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        assert!(plain_text(&line.spans).starts_with("[work] on"));
    }

    #[test]
    fn style_directive_takes_the_option_colour() {
        // spec §4.10: `#[fg=#{@theme_blue}]` (canopy 1aa91b4).
        let v = vars(&[("@theme_blue", "blue")]);
        let inputs = StatusInputs {
            left: "#[fg=#{@theme_blue}]hi",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let hi = line
            .spans
            .iter()
            .find(|s| s.text.trim() == "hi")
            .expect("hi span");
        assert_eq!(hi.style.fg, Color::Named(NamedColor::Blue));
    }

    #[test]
    fn style_directive_with_hex_colour() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "#[fg=#336699]hi",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let hi = line
            .spans
            .iter()
            .find(|s| s.text.trim() == "hi")
            .expect("hi span");
        assert_eq!(
            hi.style.fg,
            Color::Spec(Rgb {
                r: 0x33,
                g: 0x66,
                b: 0x99
            })
        );
    }

    #[test]
    fn attribute_keyword_sets_bold() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "#[bold]hi",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let hi = &line.spans.iter().find(|s| s.text == "hi").unwrap();
        assert!(hi.style.attrs.contains(CellFlags::BOLD));
    }

    #[test]
    fn status_style_sets_the_base_colours() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "hi",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "bg=green,fg=black",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let hi = &line
            .spans
            .iter()
            .find(|s| s.text.starts_with("hi"))
            .unwrap();
        assert_eq!(hi.style.fg, Color::Named(NamedColor::Black));
        assert_eq!(hi.style.bg, Color::Named(NamedColor::Green));
    }

    #[test]
    fn strftime_expands_against_the_fixed_clock() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "",
            right: "%H:%M",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 9, 28, 14, 5, 0),
        };
        let line = render(&inputs);
        assert!(plain_text(&line.spans).trim_end().ends_with("14:05"));
    }

    #[test]
    fn strftime_runs_on_the_raw_template_not_a_variables_value() {
        // Verified against real tmux 3.7b: with a session literally named
        // `abc%dxyz`, `[#{session_name}] %H:%M` renders
        // `[abc%dxyz] 11:08` -- the `%d` inside the variable's *value*
        // is never touched, because strftime already ran on the raw
        // template before `#{session_name}` was substituted in.
        let v = vars(&[("session_name", "abc%dxyz")]);
        let inputs = StatusInputs {
            left: "[#{session_name}] %H:%M",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 9, 28, 11, 8, 0),
        };
        let line = render(&inputs);
        assert_eq!(plain_text(&line.spans).trim_end(), "[abc%dxyz] 11:08");
    }

    #[test]
    fn a_variables_percent_sequences_are_not_re_expanded_by_strftime() {
        // Verified against real tmux 3.7b: `#{@clock_fmt}` holding
        // `%H:%M` renders literally, since strftime's one pass already
        // happened over the raw template (which has no `%` in it) before
        // `#{@clock_fmt}` was substituted in.
        let v = vars(&[("@clock_fmt", "%H:%M")]);
        let inputs = StatusInputs {
            left: "",
            right: "#{@clock_fmt}",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 9, 28, 9, 30, 0),
        };
        let line = render(&inputs);
        assert_eq!(plain_text(&line.spans).trim(), "%H:%M");
    }

    #[test]
    fn basename_modifier_via_status_render() {
        let v = vars(&[("pane_current_path", "/a/b/c")]);
        let inputs = StatusInputs {
            left: "#{b:pane_current_path}",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        assert_eq!(plain_text(&line.spans).trim_end(), "c");
    }

    #[test]
    fn current_window_uses_the_current_format() {
        let v = vars(&[("window_index", "1"), ("window_name", "shell")]);
        let v2 = vars(&[("window_index", "2"), ("window_name", "build")]);
        let windows = [
            window("#{window_index}:#{window_name} ", &v, 1, false),
            window("#{window_index}:#{window_name}* ", &v2, 2, true),
        ];
        let empty = vars(&[]);
        let inputs = StatusInputs {
            left: "",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &empty,
            windows: &windows,
            width: 40,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let text = plain_text(&line.spans);
        assert!(text.contains("1:shell"));
        assert!(text.contains("2:build*"));
        assert!(!text.contains("1:shell*"));
    }

    #[test]
    fn status_left_length_caps_the_left_field() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "0123456789ABCDEF",
            right: "",
            left_length: 5,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 80,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        assert!(plain_text(&line.spans).starts_with("01234 "));
        assert!(!plain_text(&line.spans).contains('5'));
    }

    /// Recorded on tmux 3.7b: a row too wide for the client trims
    /// status-right before status-left, so the left-hand side stays (M2
    /// cropped from the left instead).
    #[test]
    fn overlong_status_line_keeps_the_left_as_tmux() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "left-side-content-that-is-quite-long",
            right: "clock",
            left_length: 100,
            right_length: 100,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 10,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let text = plain_text(&line.spans);
        assert_eq!(text, "left-side-");
    }

    /// Recorded on tmux 3.7b: "你好" in 3 columns keeps "你", and the
    /// "好" that no longer fits draws as a space.
    #[test]
    fn a_wide_char_cut_by_the_edge_draws_as_a_space() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "你好",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 3,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let text = plain_text(&line.spans);
        assert_eq!(text, "你 ");
        assert_eq!(unicode_width::UnicodeWidthStr::width(text.as_str()), 3);
    }

    #[test]
    fn an_emoji_that_fits_stays_and_the_rest_is_trimmed() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "😀b",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 2,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let text = plain_text(&line.spans);
        assert_eq!(text, "😀");
        assert_eq!(unicode_width::UnicodeWidthStr::width(text.as_str()), 2);
    }

    #[test]
    fn basename_of_a_literal_path_is_empty_matching_tmux() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "#{b:/a/b/c}",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
            left_style: "default",
            right_style: "default",
            justify: "left",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        assert_eq!(plain_text(&line.spans).trim(), "");
    }

    /// Recorded on tmux 3.7b (`b3wpc/alerts-*`): a bell takes
    /// `window-status-bell-style`, an activity or silence alert
    /// `window-status-activity-style`, and a bell whose style is `default`
    /// falls to the activity one.
    #[test]
    fn an_alert_takes_its_style_and_a_bell_wins() {
        let now = fixed_tm(2026, 1, 1, 0, 0, 0);
        let v = vars(&[]);
        let entry = |bell: bool, alert: bool, bell_style: &'static str| {
            let w = WindowStatus {
                bell,
                alert,
                options: WindowOptions {
                    bell_style,
                    activity_style: "bg=green",
                    ..WindowOptions::default()
                },
                ..window("x", &v, 2, false)
            };
            window_entry(&w, &now)
        };
        assert!(
            entry(false, false, "reverse").contains("default]")
                && !entry(false, false, "reverse").contains("reverse")
        );
        assert!(entry(true, false, "reverse").contains(" reverse]"));
        assert!(entry(false, true, "reverse").contains(" bg=green]"));
        assert!(entry(true, true, "reverse").contains(" reverse]"));
        assert!(!entry(true, true, "reverse").contains("bg=green"));
        assert!(entry(true, true, "default").contains(" bg=green]"));
    }
}
