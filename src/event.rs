//! Event enum (serde).

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::types::{PaneId, SessionId, WindowId};

/// An event pushed to subscribed control-mode clients (spec §7.2). Field
/// order is the wire order; ids are runtime ids, `client` is the name
/// `#{client_name}` shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    PaneExited {
        pane: PaneId,
        uid: String,
        status: Option<i32>,
    },
    /// A pane's process exited and `remain-on-exit` kept the pane.
    PaneDied {
        pane: PaneId,
        uid: String,
        window: WindowId,
        status: Option<i32>,
    },
    PaneOutput {
        pane: PaneId,
        /// Raw pane output, base64-encoded (standard alphabet).
        data: String,
    },
    /// A pane became the active pane of an attached client's current
    /// window, or that client's terminal regained focus.
    PaneFocus {
        pane: PaneId,
        window: WindowId,
        session: SessionId,
        client: String,
    },
    PaneResized {
        pane: PaneId,
        window: WindowId,
    },
    WindowFocused {
        window: WindowId,
        session: SessionId,
        pane: PaneId,
    },
    WindowCreated {
        window: WindowId,
        session: SessionId,
        pane: PaneId,
    },
    WindowResized {
        window: WindowId,
        session: SessionId,
    },
    SessionCreated {
        session: SessionId,
        name: String,
    },
    SessionClosed {
        session: SessionId,
        name: String,
    },
    ClientSessionChanged {
        client: String,
        session: SessionId,
    },
    ClientResized {
        client: String,
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    AlertBell {
        window: WindowId,
        session: SessionId,
        pane: PaneId,
    },
    AlertActivity {
        window: WindowId,
        session: SessionId,
        pane: PaneId,
    },
    AlertSilence {
        window: WindowId,
        session: SessionId,
        pane: PaneId,
    },
    /// An option's value changed. `scope` is `global` or the owner's id
    /// (`$N`, `@N`, `%N`); `value` is `null` once unset.
    OptionChanged {
        scope: String,
        name: String,
        value: Option<String>,
    },
    EventsDropped {
        count: u64,
    },
}

impl Event {
    /// The name `subscribe` matches against.
    pub fn name(&self) -> &'static str {
        match self {
            Event::PaneExited { .. } => "pane-exited",
            Event::PaneDied { .. } => "pane-died",
            Event::PaneOutput { .. } => "pane-output",
            Event::PaneFocus { .. } => "pane-focus",
            Event::PaneResized { .. } => "pane-resized",
            Event::WindowFocused { .. } => "window-focused",
            Event::WindowCreated { .. } => "window-created",
            Event::WindowResized { .. } => "window-resized",
            Event::SessionCreated { .. } => "session-created",
            Event::SessionClosed { .. } => "session-closed",
            Event::ClientSessionChanged { .. } => "client-session-changed",
            Event::ClientResized { .. } => "client-resized",
            Event::AlertBell { .. } => "alert-bell",
            Event::AlertActivity { .. } => "alert-activity",
            Event::AlertSilence { .. } => "alert-silence",
            Event::OptionChanged { .. } => "option-changed",
            Event::EventsDropped { .. } => "events-dropped",
        }
    }
}

impl Event {
    /// The pane, window and session the event names in its own fields
    /// (`option-changed` by its `scope`); what `subscribe -t` filters on.
    pub fn subject(&self) -> Subject {
        let mut s = Subject::default();
        match self {
            Event::PaneExited { pane, .. } | Event::PaneOutput { pane, .. } => s.pane = Some(*pane),
            Event::PaneDied { pane, window, .. } | Event::PaneResized { pane, window } => {
                s.pane = Some(*pane);
                s.window = Some(*window);
            }
            Event::PaneFocus {
                pane,
                window,
                session,
                ..
            }
            | Event::WindowFocused {
                window,
                session,
                pane,
            }
            | Event::WindowCreated {
                window,
                session,
                pane,
            }
            | Event::AlertBell {
                window,
                session,
                pane,
            }
            | Event::AlertActivity {
                window,
                session,
                pane,
            }
            | Event::AlertSilence {
                window,
                session,
                pane,
            } => {
                s.pane = Some(*pane);
                s.window = Some(*window);
                s.session = Some(*session);
            }
            Event::WindowResized { window, session } => {
                s.window = Some(*window);
                s.session = Some(*session);
            }
            Event::SessionCreated { session, .. }
            | Event::SessionClosed { session, .. }
            | Event::ClientSessionChanged { session, .. }
            | Event::ClientResized { session, .. } => s.session = Some(*session),
            Event::OptionChanged { scope, .. } => {
                s.pane = scope.parse().ok();
                s.window = scope.parse().ok();
                s.session = scope.parse().ok();
            }
            Event::EventsDropped { .. } => {}
        }
        s
    }
}

/// Where a pane was when it went, for a pane that is no longer in the model
/// (a killed pane's `pane-exited` still has to reach a `subscribe -t` on its
/// window or session).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub window: Option<WindowId>,
    pub session: Option<SessionId>,
}

/// The ids an [`Event`] names.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Subject {
    pub pane: Option<PaneId>,
    pub window: Option<WindowId>,
    pub session: Option<SessionId>,
}

impl Event {
    /// Builds a `PaneOutput` event, base64-encoding `bytes` with the standard alphabet.
    pub fn pane_output(pane: PaneId, bytes: &[u8]) -> Event {
        Event::PaneOutput {
            pane,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_exited_serializes_exactly() {
        let event = Event::PaneExited {
            pane: PaneId(4),
            uid: "01J".into(),
            status: Some(0),
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":"pane-exited","pane":"%4","uid":"01J","status":0}"#
        );
    }

    #[test]
    fn events_dropped_serializes_exactly() {
        let event = Event::EventsDropped { count: 3 };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":"events-dropped","count":3}"#
        );
    }

    #[test]
    fn option_changed_serializes_exactly() {
        let event = Event::OptionChanged {
            scope: "$0".into(),
            name: "@x".into(),
            value: None,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":"option-changed","scope":"$0","name":"@x","value":null}"#
        );
        assert_eq!(event.name(), "option-changed");
    }

    #[test]
    fn pane_output_base64_encodes_standard_alphabet() {
        let event = Event::pane_output(PaneId(1), b"hi");
        match &event {
            Event::PaneOutput { pane, data } => {
                assert_eq!(*pane, PaneId(1));
                assert_eq!(data, "aGk=");
            }
            other => panic!("expected PaneOutput, got {other:?}"),
        }
    }

    #[test]
    fn events_round_trip_through_json() {
        for event in [
            Event::PaneExited {
                pane: PaneId(4),
                uid: "01J".into(),
                status: Some(0),
            },
            Event::PaneExited {
                pane: PaneId(5),
                uid: "01K".into(),
                status: None,
            },
            Event::pane_output(PaneId(1), b"hi"),
            Event::EventsDropped { count: 3 },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            let round_tripped: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(round_tripped, event);
        }
    }

    #[test]
    fn catalog_events_serialize_exactly() {
        let (w, s, p) = (WindowId(1), SessionId(2), PaneId(3));
        let cases = [
            (
                Event::PaneDied {
                    pane: p,
                    uid: "01J".into(),
                    window: w,
                    status: None,
                },
                r#"{"event":"pane-died","pane":"%3","uid":"01J","window":"@1","status":null}"#,
            ),
            (
                Event::PaneFocus {
                    pane: p,
                    window: w,
                    session: s,
                    client: "client-1".into(),
                },
                r#"{"event":"pane-focus","pane":"%3","window":"@1","session":"$2","client":"client-1"}"#,
            ),
            (
                Event::PaneResized { pane: p, window: w },
                r#"{"event":"pane-resized","pane":"%3","window":"@1"}"#,
            ),
            (
                Event::WindowFocused {
                    window: w,
                    session: s,
                    pane: p,
                },
                r#"{"event":"window-focused","window":"@1","session":"$2","pane":"%3"}"#,
            ),
            (
                Event::WindowCreated {
                    window: w,
                    session: s,
                    pane: p,
                },
                r#"{"event":"window-created","window":"@1","session":"$2","pane":"%3"}"#,
            ),
            (
                Event::WindowResized {
                    window: w,
                    session: s,
                },
                r#"{"event":"window-resized","window":"@1","session":"$2"}"#,
            ),
            (
                Event::SessionCreated {
                    session: s,
                    name: "a".into(),
                },
                r#"{"event":"session-created","session":"$2","name":"a"}"#,
            ),
            (
                Event::SessionClosed {
                    session: s,
                    name: "a".into(),
                },
                r#"{"event":"session-closed","session":"$2","name":"a"}"#,
            ),
            (
                Event::ClientSessionChanged {
                    client: "c".into(),
                    session: s,
                },
                r#"{"event":"client-session-changed","client":"c","session":"$2"}"#,
            ),
            (
                Event::ClientResized {
                    client: "c".into(),
                    session: s,
                    cols: 80,
                    rows: 24,
                },
                r#"{"event":"client-resized","client":"c","session":"$2","cols":80,"rows":24}"#,
            ),
            (
                Event::AlertBell {
                    window: w,
                    session: s,
                    pane: p,
                },
                r#"{"event":"alert-bell","window":"@1","session":"$2","pane":"%3"}"#,
            ),
            (
                Event::AlertActivity {
                    window: w,
                    session: s,
                    pane: p,
                },
                r#"{"event":"alert-activity","window":"@1","session":"$2","pane":"%3"}"#,
            ),
            (
                Event::AlertSilence {
                    window: w,
                    session: s,
                    pane: p,
                },
                r#"{"event":"alert-silence","window":"@1","session":"$2","pane":"%3"}"#,
            ),
        ];
        for (event, json) in cases {
            assert_eq!(serde_json::to_string(&event).unwrap(), json);
            assert_eq!(serde_json::from_str::<Event>(json).unwrap(), event);
        }
    }
}
