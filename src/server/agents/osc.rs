//! Parser for the body of an `OSC 7501` agent status report.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::{ReportState, valid_kind};

#[derive(Debug, PartialEq, Eq)]
pub enum Osc7501 {
    Query,
    Report {
        state: ReportState,
        blocked: Option<String>,
        msg: Option<String>,
        app: Option<String>,
    },
}

/// Parses the body after `7501;`: `?` or `key=value` pairs joined by `:`.
/// Returns `None` without a known `state` or for a non-root `id`.
pub fn parse(body: &[u8]) -> Option<Osc7501> {
    let body = std::str::from_utf8(body).ok()?;
    if body == "?" {
        return Some(Osc7501::Query);
    }
    let mut state = None;
    let mut blocked = None;
    let mut msg = None;
    let mut app = None;
    for pair in body.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "state" => state = ReportState::parse(value),
            "kind" => {
                blocked = ["permission", "question", "auth"]
                    .contains(&value)
                    .then(|| value.to_string());
            }
            "msg" => {
                msg = STANDARD
                    .decode(value)
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok());
            }
            "app" => app = valid_kind(value).then(|| value.to_string()),
            "id" if !matches!(value, "" | "/" | "root") => return None,
            _ => {}
        }
    }
    Some(Osc7501::Report {
        state: state?,
        blocked,
        msg,
        app,
    })
}

#[cfg(test)]
mod tests {
    use super::super::AgentState;
    use super::*;

    type Fields = (ReportState, Option<String>, Option<String>, Option<String>);

    fn report(body: &str) -> Option<Fields> {
        match parse(body.as_bytes())? {
            Osc7501::Report {
                state,
                blocked,
                msg,
                app,
            } => Some((state, blocked, msg, app)),
            Osc7501::Query => None,
        }
    }

    #[test]
    fn bad_base64_drops_msg() {
        let (state, _, msg, _) = report("state=done:msg=!!!").unwrap();
        assert_eq!(state, ReportState::Set(AgentState::Done));
        assert_eq!(msg, None);
    }

    #[test]
    fn non_root_id_ignored() {
        assert_eq!(parse(b"id=build/test:state=done"), None);
        assert!(parse(b"id=root:state=done").is_some());
    }

    #[test]
    fn kind_other_than_three_dropped() {
        let (_, blocked, _, _) = report("state=blocked:kind=bogus").unwrap();
        assert_eq!(blocked, None);
        let (_, blocked, _, _) = report("state=blocked:kind=auth").unwrap();
        assert_eq!(blocked.as_deref(), Some("auth"));
    }

    #[test]
    fn progress_ignored() {
        let (state, ..) = report("state=working:progress=50:title=x").unwrap();
        assert_eq!(state, ReportState::Set(AgentState::Working));
    }

    #[test]
    fn examples() {
        assert_eq!(
            report("state=blocked:kind=permission:msg=aGk="),
            Some((
                ReportState::Set(AgentState::Blocked),
                Some("permission".into()),
                Some("hi".into()),
                None
            ))
        );
        assert_eq!(parse(b"?"), Some(Osc7501::Query));
        assert_eq!(parse(b"state=bogus"), None);
        assert_eq!(parse(b"msg=aGk="), None);
        assert_eq!(report("state=clear").unwrap().0, ReportState::Clear);
        assert_eq!(
            report("state=done:app=claude").unwrap().3.as_deref(),
            Some("claude")
        );
    }
}
