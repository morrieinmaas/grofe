//! Target grammar (spec §4.5): `$N`/`@N`/`%N` runtime ids, `session`,
//! `session:window`, `session:window.pane`, with window by index or name
//! and pane by index. Not supported in v1: fnmatch patterns, `{last}`/
//! `{next}` tokens, `!`, `+`/`-` offsets.
//!
//! Parsing here is purely syntactic: it decides the *shape* of a target
//! string, not whether the session/window/pane it names exists. Existence
//! is [`Resolve`]'s job, against the live model and the request's
//! [`ClientCtx`].

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
}

/// The window part of a target; `Current` for the same reason as
/// [`SessionRef::Current`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRef {
    Current,
    Id(WindowId),
    Index(u32),
    Name(String),
}

/// The pane part of a `session:window.pane` target: always by id or
/// index, never by name (spec §4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanePart {
    Id(PaneId),
    Index(u32),
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

/// Parses one target string (spec §4.5). See the module docs for the
/// supported grammar.
pub fn parse_target(s: &str) -> Result<Target, ParseError> {
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
        if pane_part.is_empty() || pane_part.contains('.') || is_unsupported_token(pane_part) {
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
        // No ':': a bare name is a session (spec: "session" alone).
        return Ok(Target::Session(SessionRef::Name(s.to_string())));
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
    if pane_part.is_empty() || is_unsupported_token(window_part) {
        return Err(ParseError::UnsupportedTarget(s.to_string()));
    }
    // Empty window part ("`sess:.1`", "`:.1`"): the current window.
    let window = if window_part.is_empty() {
        WindowRef::Current
    } else {
        parse_window_ref(window_part)?
    };
    Ok(Target::Pane(PaneRef {
        session,
        window,
        pane: parse_pane_part(pane_part)?,
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
    fn bare_name_is_a_session() {
        assert_eq!(
            parse_target("work").unwrap(),
            Target::Session(SessionRef::Name("work".to_string()))
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
