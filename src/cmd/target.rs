//! Target grammar (spec §4.5): `$N`/`@N`/`%N` runtime ids, a bare word,
//! `session:window`, `session:window.pane`, with window by index or name
//! and pane by index. A bare word is read as tmux's cmd-find.c reads it,
//! by the kind of target the verb wants ([`SessionRef::Bare`]). Not supported in v1: fnmatch patterns, `{last}`/
//! `{next}` tokens, `!`, `+`/`-` offsets.
//!
//! The tmux front end (FE.1, spec §4.5) reads targets the tmux way through
//! [`with_tmux_targets`]: `=name` and `=name:window` are exact session
//! matches ([`SessionRef::Exact`]), `=N` in the window part is exactly
//! that window, `+`/`-` offsets in the window and pane parts are relative
//! to the active window or pane, and a bare `=` (tmux's mouse target, which
//! mouse keys replace before parsing, M36-R3) fails when it is resolved,
//! as in tmux, so a mouse binding that holds one still binds. The native
//! grammar never builds these: natively `=x` is the literal name `=x`.
//!
//! Parsing here is purely syntactic: it decides the *shape* of a target
//! string, not whether the session/window/pane it names exists. Existence
//! is [`Resolve`]'s job, against the live model and the request's
//! [`ClientCtx`].

use std::cell::Cell;

use crate::cmd::ParseError;
use crate::protocol::ClientCtx;
use crate::types::{PaneId, SessionId, WindowId};

/// The session part of a target. `Current` means the target string didn't
/// name a session explicitly (a bare `@N`/`%N` id, or an omitted target):
/// a [`Resolve`] impl uses the request's current session in that case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRef {
    Current,
    Id(SessionId),
    Name(String),
    /// Exactly this name, never a prefix: tmux's `=name`, built only by the
    /// tmux front end.
    Exact(String),
    /// tmux's bare `=`, the mouse target, outside a mouse key's commands
    /// (which get the pointer's target in its place first, M36-R3): it
    /// never resolves, and says so as tmux does ([`NO_MOUSE_TARGET`]).
    /// Built only by the tmux front end.
    Mouse,
    /// A bare word with no `:` or `.`, read as tmux's `cmd-find.c` reads it:
    /// what it names depends on the kind of target the verb wants. As a
    /// pane, a number is a pane index in the current window, then a window
    /// in the current session, then a session; as a window, a window in
    /// the current session, then a session; as a session, a session only.
    /// Natively too: the canopy port's `select-pane -t 1` means pane 1,
    /// as in tmux. Only the front end reads `window.pane` with no `:`.
    Bare(String),
}

/// The window part of a target; `Current` for the same reason as
/// [`SessionRef::Current`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRef {
    Current,
    Id(WindowId),
    Index(u32),
    Name(String),
    /// tmux's `+N`/`-N`: that many windows after or before the session's
    /// current one, wrapping. Built only by the tmux front end.
    Offset(i32),
}

/// The pane part of a `session:window.pane` target: always by id or
/// index, never by name (spec §4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanePart {
    Id(PaneId),
    Index(u32),
    /// tmux's `+N`/`-N`: that many panes after or before the window's
    /// active one, wrapping. Built only by the tmux front end.
    Offset(i32),
}

/// A fully parsed `session:window.pane` target, bundled into one type so
/// [`Target::Pane`] can carry it as a single field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRef {
    pub session: SessionRef,
    pub window: WindowRef,
    pub pane: PanePart,
}

/// A parsed target (spec §4.5). Syntax only: whether the named
/// session/window/pane actually exists is for [`Resolve`] to decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Session(SessionRef),
    Window(SessionRef, WindowRef),
    Pane(PaneRef),
    /// Empty or omitted: the client's current pane/window/session, from
    /// the request's [`ClientCtx`].
    Current,
}

/// Tokens spec §4.5 explicitly excludes from v1: `{last}`, `{next}`, `!`,
/// and `+`/`-` relative offsets. Used to give those a clear
/// [`ParseError::UnsupportedTarget`] instead of silently treating them as
/// a literal name.
fn is_unsupported_token(s: &str) -> bool {
    if s == "!" || s == "{last}" || s == "{next}" {
        return true;
    }
    let mut chars = s.chars();
    match chars.next() {
        Some('+') | Some('-') => !s[1..].is_empty() && s[1..].chars().all(|c| c.is_ascii_digit()),
        _ => false,
    }
}

thread_local! {
    /// Set while the tmux front end parses: targets read the tmux way.
    static TMUX_TARGETS: Cell<bool> = const { Cell::new(false) };
}

/// tmux 3.7b's error for a bare `=` target with no mouse event (recorded:
/// `has-session -t =`).
pub const NO_MOUSE_TARGET: &str = "no mouse target";

/// Runs `f` (a native parse) with targets read as the tmux front end reads
/// them (see the module docs). Only `compat::tmux` calls this, so the
/// native grammar is unchanged everywhere else.
pub fn with_tmux_targets<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            TMUX_TARGETS.with(|t| t.set(self.0));
        }
    }
    let _restore = Restore(TMUX_TARGETS.with(|t| t.replace(true)));
    f()
}

fn tmux_targets() -> bool {
    TMUX_TARGETS.with(Cell::get)
}

/// tmux's `+`, `-`, `+N`, `-N` in a window or pane part, read only for the
/// tmux front end.
fn tmux_offset(s: &str) -> Option<i32> {
    if !tmux_targets() {
        return None;
    }
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.is_empty() {
        return Some(sign);
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i32>().ok().map(|n| sign * n)
}

/// Parses one target string (spec §4.5). See the module docs for the
/// supported grammar.
pub fn parse_target(s: &str) -> Result<Target, ParseError> {
    if tmux_targets() {
        if s == "=" {
            return Ok(Target::Session(SessionRef::Mouse));
        }
        if let Some(rest) = s.strip_prefix('=') {
            return parse_position(rest).map(exact_session);
        }
    }
    parse_position(s)
}

/// `target` with its named session made an exact match (tmux's `=name`).
fn exact_session(target: Target) -> Target {
    let exact = |s: SessionRef| match s {
        SessionRef::Name(name) | SessionRef::Bare(name) => SessionRef::Exact(name),
        other => other,
    };
    match target {
        Target::Session(s) => Target::Session(exact(s)),
        Target::Window(s, w) => Target::Window(exact(s), w),
        Target::Pane(p) => Target::Pane(PaneRef {
            session: exact(p.session),
            ..p
        }),
        Target::Current => Target::Current,
    }
}

fn parse_position(s: &str) -> Result<Target, ParseError> {
    if s.is_empty() {
        return Ok(Target::Current);
    }
    // A bare "$N" is a raw session id; "$N:window[.pane]" is a compound
    // target whose session part happens to be an id, handled by
    // `parse_session_ref` below.
    if s.starts_with('$') && !s.contains(':') {
        return s
            .parse::<SessionId>()
            .map(|id| Target::Session(SessionRef::Id(id)))
            .map_err(|_| ParseError::BadTarget(s.to_string()));
    }
    if s.starts_with('@') {
        return s
            .parse::<WindowId>()
            .map(|id| Target::Window(SessionRef::Current, WindowRef::Id(id)))
            .map_err(|_| ParseError::BadTarget(s.to_string()));
    }
    if s.starts_with('%') {
        return s
            .parse::<PaneId>()
            .map(|id| {
                Target::Pane(PaneRef {
                    session: SessionRef::Current,
                    window: WindowRef::Current,
                    pane: PanePart::Id(id),
                })
            })
            .map_err(|_| ParseError::BadTarget(s.to_string()));
    }

    // A bare ".pane" (no session/window part at all, no ':') means the
    // current window's pane N (spec: "empty or omitted means current").
    if let Some(pane_part) = s.strip_prefix('.')
        && !s.contains(':')
    {
        if pane_part.is_empty()
            || pane_part.contains('.')
            || (is_unsupported_token(pane_part) && tmux_offset(pane_part).is_none())
        {
            return Err(ParseError::UnsupportedTarget(s.to_string()));
        }
        return Ok(Target::Pane(PaneRef {
            session: SessionRef::Current,
            window: WindowRef::Current,
            pane: parse_pane_part(pane_part)?,
        }));
    }

    if is_unsupported_token(s) {
        return Err(ParseError::UnsupportedTarget(s.to_string()));
    }

    let mut colon_positions = s.match_indices(':');
    let Some((colon_idx, _)) = colon_positions.next() else {
        if tmux_targets() {
            return tmux_bare(s);
        }
        // No ':': a bare word, read by the kind of target the verb wants,
        // as tmux reads it ([`SessionRef::Bare`]).
        return Ok(Target::Session(SessionRef::Bare(s.to_string())));
    };
    if colon_positions.next().is_some() {
        return Err(ParseError::UnsupportedTarget(s.to_string()));
    }

    let session_part = &s[..colon_idx];
    let rest = &s[colon_idx + 1..];
    // Empty session part ("`:2`", "`sess:`", "`:`") means current (spec:
    // "empty or omitted means current").
    let session = if session_part.is_empty() {
        SessionRef::Current
    } else {
        parse_session_ref(session_part)?
    };

    let mut dot_positions = rest.match_indices('.');
    let Some((dot_idx, _)) = dot_positions.next() else {
        if let Some(n) = tmux_offset(rest) {
            return Ok(Target::Window(session, WindowRef::Offset(n)));
        }
        if is_unsupported_token(rest) {
            return Err(ParseError::UnsupportedTarget(s.to_string()));
        }
        if rest.is_empty() {
            // "sess:" / ":": the session's current window.
            return Ok(Target::Window(session, WindowRef::Current));
        }
        return Ok(Target::Window(session, parse_window_ref(rest)?));
    };
    if dot_positions.next().is_some() {
        return Err(ParseError::UnsupportedTarget(s.to_string()));
    }

    let window_part = &rest[..dot_idx];
    let pane_part = &rest[dot_idx + 1..];
    let window_offset = tmux_offset(window_part);
    if pane_part.is_empty() || (is_unsupported_token(window_part) && window_offset.is_none()) {
        return Err(ParseError::UnsupportedTarget(s.to_string()));
    }
    // Empty window part ("`sess:.1`", "`:.1`"): the current window.
    let window = if window_part.is_empty() {
        WindowRef::Current
    } else if let Some(n) = window_offset {
        WindowRef::Offset(n)
    } else {
        parse_window_ref(window_part)?
    };
    Ok(Target::Pane(PaneRef {
        session,
        window,
        pane: parse_pane_part(pane_part)?,
    }))
}

/// tmux's target with no `:`: `window.pane` in the current session, or a
/// bare word left for the resolver ([`SessionRef::Bare`]).
fn tmux_bare(s: &str) -> Result<Target, ParseError> {
    let Some((window, pane)) = s.split_once('.') else {
        return Ok(Target::Session(SessionRef::Bare(s.to_string())));
    };
    if window.is_empty() || pane.is_empty() || pane.contains('.') {
        return Err(ParseError::UnsupportedTarget(s.to_string()));
    }
    let window = match tmux_offset(window) {
        Some(n) => WindowRef::Offset(n),
        None => parse_window_ref(window)?,
    };
    Ok(Target::Pane(PaneRef {
        session: SessionRef::Current,
        window,
        pane: parse_pane_part(pane)?,
    }))
}

fn parse_session_ref(s: &str) -> Result<SessionRef, ParseError> {
    if s.starts_with('$') {
        return s
            .parse::<SessionId>()
            .map(SessionRef::Id)
            .map_err(|_| ParseError::BadTarget(s.to_string()));
    }
    Ok(SessionRef::Name(s.to_string()))
}

fn parse_window_ref(s: &str) -> Result<WindowRef, ParseError> {
    // tmux's `=N` / `=name`: exactly that window. grofe's window lookups
    // are exact already, so the `=` only has to go.
    let s = match s.strip_prefix('=') {
        Some(rest) if tmux_targets() && !rest.is_empty() => rest,
        _ => s,
    };
    if s.starts_with('@') {
        return s
            .parse::<WindowId>()
            .map(WindowRef::Id)
            .map_err(|_| ParseError::BadTarget(s.to_string()));
    }
    if let Ok(idx) = s.parse::<u32>() {
        return Ok(WindowRef::Index(idx));
    }
    Ok(WindowRef::Name(s.to_string()))
}

fn parse_pane_part(s: &str) -> Result<PanePart, ParseError> {
    if let Some(n) = tmux_offset(s) {
        return Ok(PanePart::Offset(n));
    }
    if s.starts_with('%') {
        return s
            .parse::<PaneId>()
            .map(PanePart::Id)
            .map_err(|_| ParseError::BadTarget(s.to_string()));
    }
    s.parse::<u32>()
        .map(PanePart::Index)
        .map_err(|_| ParseError::UnsupportedTarget(s.to_string()))
}

/// Resolves a [`Target`] against live server state. Implemented by the
/// server's model (T1.5a); T1.2 only defines the contract each dispatch
/// handler calls into.
pub trait Resolve {
    fn resolve_pane(&self, target: &Target, ctx: &ClientCtx) -> Result<PaneId, String>;
    fn resolve_window(&self, target: &Target, ctx: &ClientCtx) -> Result<WindowId, String>;
    fn resolve_session(&self, target: &Target, ctx: &ClientCtx) -> Result<SessionId, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_current() {
        assert_eq!(parse_target("").unwrap(), Target::Current);
    }

    #[test]
    fn raw_session_id() {
        assert_eq!(
            parse_target("$3").unwrap(),
            Target::Session(SessionRef::Id(SessionId(3)))
        );
    }

    #[test]
    fn raw_window_id_has_no_session_context() {
        assert_eq!(
            parse_target("@7").unwrap(),
            Target::Window(SessionRef::Current, WindowRef::Id(WindowId(7)))
        );
    }

    #[test]
    fn raw_pane_id_has_no_session_or_window_context() {
        assert_eq!(
            parse_target("%9").unwrap(),
            Target::Pane(PaneRef {
                session: SessionRef::Current,
                window: WindowRef::Current,
                pane: PanePart::Id(PaneId(9)),
            })
        );
    }

    #[test]
    fn bad_raw_id_is_an_error() {
        assert!(matches!(parse_target("$x"), Err(ParseError::BadTarget(_))));
        assert!(matches!(parse_target("@"), Err(ParseError::BadTarget(_))));
        assert!(matches!(parse_target("%x3"), Err(ParseError::BadTarget(_))));
    }

    #[test]
    fn bare_name_is_a_bare_word() {
        assert_eq!(
            parse_target("work").unwrap(),
            Target::Session(SessionRef::Bare("work".to_string()))
        );
    }

    #[test]
    fn session_and_window_by_index() {
        assert_eq!(
            parse_target("work:2").unwrap(),
            Target::Window(SessionRef::Name("work".to_string()), WindowRef::Index(2))
        );
    }

    #[test]
    fn session_and_window_by_name() {
        assert_eq!(
            parse_target("work:editor").unwrap(),
            Target::Window(
                SessionRef::Name("work".to_string()),
                WindowRef::Name("editor".to_string())
            )
        );
    }

    /// `session:@N` is a valid window id in the window slot (verified
    /// against tmux 3.7b: `select-pane -t sess:@N` and `select-window -t
    /// sess:@N` both resolve to that window).
    #[test]
    fn session_and_window_by_id() {
        assert_eq!(
            parse_target("work:@2").unwrap(),
            Target::Window(
                SessionRef::Name("work".to_string()),
                WindowRef::Id(WindowId(2))
            )
        );
    }

    /// `session:%N` is NOT a pane shortcut in the window slot: verified
    /// against real tmux 3.7b, `select-pane -t sess:%N`, `capture-pane -t
    /// sess:%N`, `send-keys -t sess:%N` and `select-window -t sess:%N` all
    /// fail identically with `can't find window: %N` for both pane-taking
    /// and window-taking commands. tmux treats `%N` there as a literal
    /// (failed) window lookup, not as a pane id, so grofe's grammar is
    /// already right: parsing succeeds with a window named `%N`, and
    /// resolving that name against the live model (T1.5a) is where it
    /// fails, same as tmux (which also parses this syntactically before
    /// failing to find the window at lookup time).
    #[test]
    fn session_and_window_percent_id_is_a_literal_window_name() {
        assert_eq!(
            parse_target("work:%3").unwrap(),
            Target::Window(
                SessionRef::Name("work".to_string()),
                WindowRef::Name("%3".to_string())
            )
        );
    }

    #[test]
    fn session_window_and_pane_by_index() {
        assert_eq!(
            parse_target("work:1.2").unwrap(),
            Target::Pane(PaneRef {
                session: SessionRef::Name("work".to_string()),
                window: WindowRef::Index(1),
                pane: PanePart::Index(2),
            })
        );
    }

    #[test]
    fn session_by_id_in_a_compound_target() {
        assert_eq!(
            parse_target("$3:1.2").unwrap(),
            Target::Pane(PaneRef {
                session: SessionRef::Id(SessionId(3)),
                window: WindowRef::Index(1),
                pane: PanePart::Index(2),
            })
        );
    }

    #[test]
    fn pane_by_name_is_unsupported() {
        assert!(matches!(
            parse_target("work:1.editor"),
            Err(ParseError::UnsupportedTarget(_))
        ));
    }

    /// Empty or omitted target parts mean current (spec §4.5).
    #[test]
    fn empty_session_before_colon_means_current() {
        assert_eq!(
            parse_target(":2").unwrap(),
            Target::Window(SessionRef::Current, WindowRef::Index(2))
        );
    }

    #[test]
    fn bare_dot_pane_means_current_window() {
        assert_eq!(
            parse_target(".1").unwrap(),
            Target::Pane(PaneRef {
                session: SessionRef::Current,
                window: WindowRef::Current,
                pane: PanePart::Index(1),
            })
        );
    }

    #[test]
    fn trailing_colon_means_session_current_window() {
        assert_eq!(
            parse_target("sess:").unwrap(),
            Target::Window(SessionRef::Name("sess".to_string()), WindowRef::Current)
        );
    }

    #[test]
    fn lone_colon_means_current_session_and_window() {
        assert_eq!(
            parse_target(":").unwrap(),
            Target::Window(SessionRef::Current, WindowRef::Current)
        );
    }

    #[test]
    fn colon_dot_pane_means_current_session_and_window() {
        assert_eq!(
            parse_target(":.1").unwrap(),
            Target::Pane(PaneRef {
                session: SessionRef::Current,
                window: WindowRef::Current,
                pane: PanePart::Index(1),
            })
        );
    }

    #[test]
    fn session_dot_pane_means_session_current_window() {
        assert_eq!(
            parse_target("sess:.1").unwrap(),
            Target::Pane(PaneRef {
                session: SessionRef::Name("sess".to_string()),
                window: WindowRef::Current,
                pane: PanePart::Index(1),
            })
        );
    }

    #[test]
    fn multiple_colons_is_unsupported() {
        assert!(matches!(
            parse_target("a:b:c"),
            Err(ParseError::UnsupportedTarget(_))
        ));
    }

    #[test]
    fn multiple_dots_is_unsupported() {
        assert!(matches!(
            parse_target("work:1.2.3"),
            Err(ParseError::UnsupportedTarget(_))
        ));
    }

    #[test]
    fn bang_token_is_unsupported() {
        assert!(matches!(
            parse_target("!"),
            Err(ParseError::UnsupportedTarget(_))
        ));
    }

    #[test]
    fn last_and_next_tokens_are_unsupported() {
        assert!(matches!(
            parse_target("{last}"),
            Err(ParseError::UnsupportedTarget(_))
        ));
        assert!(matches!(
            parse_target("{next}"),
            Err(ParseError::UnsupportedTarget(_))
        ));
    }

    #[test]
    fn relative_offset_tokens_are_unsupported() {
        assert!(matches!(
            parse_target("+1"),
            Err(ParseError::UnsupportedTarget(_))
        ));
        assert!(matches!(
            parse_target("-1"),
            Err(ParseError::UnsupportedTarget(_))
        ));
        assert!(matches!(
            parse_target("work:+1"),
            Err(ParseError::UnsupportedTarget(_))
        ));
    }

    /// Natively `=x` is the literal name `=x`, and offsets stay refused.
    #[test]
    fn tmux_forms_are_native_literals_outside_the_front_end() {
        assert_eq!(
            parse_target("=x").unwrap(),
            Target::Session(SessionRef::Bare("=x".to_string()))
        );
        assert_eq!(
            parse_target("=").unwrap(),
            Target::Session(SessionRef::Bare("=".to_string()))
        );
        assert!(parse_target(":.+").is_err());
    }

    #[test]
    fn the_front_end_reads_exact_sessions_windows_and_offsets() {
        let exact = |s: &str| SessionRef::Exact(s.to_string());
        let t = |s: &str| with_tmux_targets(|| parse_target(s));
        assert_eq!(t("=x").unwrap(), Target::Session(exact("x")));
        assert_eq!(
            t("=x:2").unwrap(),
            Target::Window(exact("x"), WindowRef::Index(2))
        );
        assert_eq!(
            t("=x:").unwrap(),
            Target::Window(exact("x"), WindowRef::Current)
        );
        assert_eq!(
            t(":=3").unwrap(),
            Target::Window(SessionRef::Current, WindowRef::Index(3))
        );
        assert_eq!(
            t("=x:.1").unwrap(),
            Target::Pane(PaneRef {
                session: exact("x"),
                window: WindowRef::Current,
                pane: PanePart::Index(1),
            })
        );
        assert_eq!(
            t(":.+").unwrap(),
            Target::Pane(PaneRef {
                session: SessionRef::Current,
                window: WindowRef::Current,
                pane: PanePart::Offset(1),
            })
        );
        assert_eq!(
            t(":-1").unwrap(),
            Target::Window(SessionRef::Current, WindowRef::Offset(-1))
        );
        assert_eq!(
            t("w:+2.-").unwrap(),
            Target::Pane(PaneRef {
                session: SessionRef::Name("w".to_string()),
                window: WindowRef::Offset(2),
                pane: PanePart::Offset(-1),
            })
        );
        assert_eq!(t("=").unwrap(), Target::Session(SessionRef::Mouse));
        // A word with no `:` is left for the resolver, and `w.p` is a pane
        // of the current session's window w (cmd-find.c).
        assert_eq!(
            t("1").unwrap(),
            Target::Session(SessionRef::Bare("1".to_string()))
        );
        assert_eq!(
            t("2.1").unwrap(),
            Target::Pane(PaneRef {
                session: SessionRef::Current,
                window: WindowRef::Index(2),
                pane: PanePart::Index(1),
            })
        );
        assert_eq!(
            parse_target("1").unwrap(),
            Target::Session(SessionRef::Bare("1".to_string()))
        );
        // Ids and a bare offset keep their native reading.
        assert_eq!(t("%3").unwrap(), parse_target("%3").unwrap());
        assert!(t("+1").is_err());
        // The reading ends with the call.
        assert!(parse_target(":.+").is_err());
    }

    /// A minimal in-memory fake so [`Resolve`]'s contract is exercised
    /// without depending on `server/model.rs` (a different task's file).
    struct FakeModel;

    impl Resolve for FakeModel {
        fn resolve_pane(&self, target: &Target, ctx: &ClientCtx) -> Result<PaneId, String> {
            match target {
                Target::Current => ctx.pane.ok_or_else(|| "no current pane".to_string()),
                Target::Pane(PaneRef {
                    pane: PanePart::Id(id),
                    ..
                }) => Ok(*id),
                other => Err(format!("cannot resolve {other:?} as a pane")),
            }
        }

        fn resolve_window(&self, _target: &Target, _ctx: &ClientCtx) -> Result<WindowId, String> {
            Err("not implemented in fake".to_string())
        }

        fn resolve_session(&self, _target: &Target, _ctx: &ClientCtx) -> Result<SessionId, String> {
            Err("not implemented in fake".to_string())
        }
    }

    fn ctx_with_pane(pane: Option<PaneId>) -> ClientCtx {
        ClientCtx {
            cwd: std::path::PathBuf::from("/"),
            pane,
            env: Vec::new(),
            client: None,
            dialect: crate::protocol::Dialect::Grofe,
        }
    }

    #[test]
    fn current_target_resolves_from_client_ctx() {
        let model = FakeModel;
        let ctx = ctx_with_pane(Some(PaneId(4)));
        assert_eq!(
            model.resolve_pane(&Target::Current, &ctx).unwrap(),
            PaneId(4)
        );
    }

    #[test]
    fn current_target_without_a_pane_errors() {
        let model = FakeModel;
        let ctx = ctx_with_pane(None);
        assert!(model.resolve_pane(&Target::Current, &ctx).is_err());
    }

    #[test]
    fn raw_pane_id_resolves_directly() {
        let model = FakeModel;
        let ctx = ctx_with_pane(None);
        let target = parse_target("%9").unwrap();
        assert_eq!(model.resolve_pane(&target, &ctx).unwrap(), PaneId(9));
    }
}
