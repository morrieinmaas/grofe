//! Renders `status.left/right`, `window-status-format` and
//! `window-status-current-format` to a [`StyledLine`] (spec §4.10).
//!
//! `#{...}` expansion (including inside `#[...]` style directives) and
//! strftime sequences (`%H:%M`) both apply; option values and format
//! variables are resolved by the caller (`FormatVars`), so this module
//! stays independent of `Model`/`PaneHandle` and is unit-testable on its
//! own. tmux's own option names (`status-left`, `window-status-format`,
//! ...) map onto [`StatusInputs`]'s fields; wiring those option lookups
//! and per-window `FormatCtx`s onto the model is the render/attach path's
//! job (T2.5a).

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

/// One entry in the window list: the raw `window-status-format` or
/// `window-status-current-format` value already chosen for this window
/// (the current window uses the current format; that choice is the
/// caller's, since it is the caller that knows which window is active),
/// and that window's format variables.
pub struct WindowStatus<'a> {
    pub format: &'a str,
    pub vars: &'a dyn FormatVars,
}

/// Everything [`render`] needs to lay out one status line: raw option
/// values (tmux names: `status-left`, `status-right`,
/// `status-left-length`, `status-right-length`, `status-style`), the
/// format variables `status-left`/`status-right` expand against, the
/// window list, the client width to lay out for, and the broken-down time
/// `%`-sequences expand against (a fixed time in tests, so they never
/// depend on the host clock or `TZ`; the real local time in production).
pub struct StatusInputs<'a> {
    pub left: &'a str,
    pub right: &'a str,
    pub left_length: u16,
    pub right_length: u16,
    pub style: &'a str,
    pub vars: &'a dyn FormatVars,
    pub windows: &'a [WindowStatus<'a>],
    pub width: u16,
    pub now: libc::tm,
}

/// Renders one status line for `inputs.width` columns: `status-left`
/// (clipped to `left_length`), then the window list separated by single
/// spaces, then padding, then `status-right` (clipped to `right_length`)
/// at the far end. If the composed line is still wider than `width`, it
/// is cropped from the left, as tmux does, so the right-hand side (the
/// window list's current window, then `status-right`) stays visible.
pub fn render(inputs: &StatusInputs) -> StyledLine {
    let base = parse_style(inputs.style);

    let left = clip_right(
        render_piece(inputs.left, inputs.vars, &inputs.now, base),
        inputs.left_length as usize,
    );
    let right = clip_right(
        render_piece(inputs.right, inputs.vars, &inputs.now, base),
        inputs.right_length as usize,
    );

    let mut windows: Vec<Span> = Vec::new();
    for (i, w) in inputs.windows.iter().enumerate() {
        if i > 0 {
            // ponytail: `window-status-separator` is M3.6 (spec §4.5); a
            // single space is tmux's own default for it.
            windows.push(Span {
                text: " ".to_string(),
                style: base,
            });
        }
        windows.extend(render_piece(w.format, w.vars, &inputs.now, base));
    }

    let content_width = spans_width(&left) + spans_width(&windows) + spans_width(&right);
    let width = inputs.width as usize;

    let mut spans = left;
    spans.extend(windows);
    if content_width < width {
        spans.push(Span {
            text: " ".repeat(width - content_width),
            style: base,
        });
    }
    spans.extend(right);

    StyledLine {
        spans: clip_left(spans, width),
    }
}

/// Expands `#{...}` (including nested ones inside `#[...]`), then
/// strftime `%` sequences, then splits the result on `#[...]` style
/// directives.
fn render_piece(fmt: &str, vars: &dyn FormatVars, now: &libc::tm, base: Style) -> Vec<Span> {
    let text = expand(fmt, vars);
    let text = apply_strftime(&text, now);
    styled_spans(&text, base)
}

/// Splits `text` (already `#{...}`-expanded) on `#[...]` style
/// directives, applying each as a delta onto the running style starting
/// from `base`. An unterminated `#[...]` is left as literal text, like an
/// unterminated `#{...}` in `cmd::format::expand`.
fn styled_spans(text: &str, base: Style) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut style = base;
    let mut buf = String::new();
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with("#[")
            && let Some(end) = text[i + 2..].find(']')
        {
            if !buf.is_empty() {
                spans.push(Span {
                    text: std::mem::take(&mut buf),
                    style,
                });
            }
            apply_style_directive(&mut style, &text[i + 2..i + 2 + end]);
            i += 2 + end + 1;
            continue;
        }
        let ch = text[i..]
            .chars()
            .next()
            .expect("i is a char boundary within text");
        buf.push(ch);
        i += ch.len_utf8();
    }
    if !buf.is_empty() {
        spans.push(Span { text: buf, style });
    }
    spans
}

/// Parses a tmux style value (`status-style`'s own syntax, and a
/// `#[...]` directive's content) onto `style`.
fn apply_style_directive(style: &mut Style, directive: &str) {
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

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|s| s.text.chars().map(char_width).sum::<usize>())
        .sum()
}

/// Clips `spans` to at most `max_cols` display columns, dropping
/// whatever follows (used for `status-left-length`/`status-right-length`:
/// each field is capped independently). A cell that would only partly
/// fit is dropped rather than split.
fn clip_right(spans: Vec<Span>, max_cols: usize) -> Vec<Span> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        if used >= max_cols {
            break;
        }
        let mut text = String::new();
        for ch in span.text.chars() {
            let w = char_width(ch);
            if used + w > max_cols {
                break;
            }
            text.push(ch);
            used += w;
        }
        if !text.is_empty() {
            out.push(Span {
                text,
                style: span.style,
            });
        }
    }
    out
}

/// Crops `spans` to at most `max_cols` display columns by dropping
/// columns from the start, as tmux does when the whole status line is
/// wider than the client, so the right-hand side (status-right, and the
/// current window) stays visible.
fn clip_left(spans: Vec<Span>, max_cols: usize) -> Vec<Span> {
    let total = spans_width(&spans);
    if total <= max_cols {
        return spans;
    }
    let mut to_drop = total - max_cols;
    let mut out = Vec::new();
    for span in spans {
        if to_drop == 0 {
            out.push(span);
            continue;
        }
        let mut text = String::new();
        for ch in span.text.chars() {
            if to_drop > 0 {
                to_drop -= char_width(ch);
                continue;
            }
            text.push(ch);
        }
        if !text.is_empty() {
            out.push(Span {
                text,
                style: span.style,
            });
        }
    }
    out
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

    #[test]
    fn conditional_and_user_option_render() {
        let v = vars(&[("session_name", "work"), ("@x", "1")]);
        let inputs = StatusInputs {
            left: "[#{session_name}] #{?@x,on,off}",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
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
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let hi = &line.spans.iter().find(|s| s.text == "hi").unwrap();
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
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 9, 28, 14, 5, 0),
        };
        let line = render(&inputs);
        assert!(plain_text(&line.spans).trim_end().ends_with("14:05"));
    }

    #[test]
    fn strftime_runs_after_hash_expansion() {
        // A format variable's own value can itself feed strftime, since
        // expansion happens before the strftime pass over the result.
        let v = vars(&[("clock_fmt", "%H:%M")]);
        let inputs = StatusInputs {
            left: "",
            right: "#{clock_fmt}",
            left_length: 40,
            right_length: 40,
            style: "",
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 9, 28, 9, 30, 0),
        };
        let line = render(&inputs);
        assert!(plain_text(&line.spans).trim_end().ends_with("09:30"));
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
            WindowStatus {
                format: "#{window_index}:#{window_name} ",
                vars: &v,
            },
            WindowStatus {
                format: "#{window_index}:#{window_name}* ",
                vars: &v2,
            },
        ];
        let empty = vars(&[]);
        let inputs = StatusInputs {
            left: "",
            right: "",
            left_length: 40,
            right_length: 40,
            style: "",
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
            vars: &v,
            windows: no_windows(),
            width: 80,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        assert!(plain_text(&line.spans).starts_with("01234 "));
        assert!(!plain_text(&line.spans).contains('5'));
    }

    #[test]
    fn overlong_status_line_truncates_on_the_left() {
        let v = vars(&[]);
        let inputs = StatusInputs {
            left: "left-side-content-that-is-quite-long",
            right: "clock",
            left_length: 100,
            right_length: 100,
            style: "",
            vars: &v,
            windows: no_windows(),
            width: 10,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        let text = plain_text(&line.spans);
        assert_eq!(text.chars().count(), 10);
        assert!(text.ends_with("clock"));
        assert!(!text.contains("left-side"));
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
            vars: &v,
            windows: no_windows(),
            width: 40,
            now: fixed_tm(2026, 1, 1, 0, 0, 0),
        };
        let line = render(&inputs);
        assert_eq!(plain_text(&line.spans).trim(), "");
    }
}
