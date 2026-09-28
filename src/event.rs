//! Event enum (serde).

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::types::PaneId;

/// An event pushed to subscribed control-mode clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    PaneExited {
        pane: PaneId,
        uid: String,
        status: Option<i32>,
    },
    PaneOutput {
        pane: PaneId,
        /// Raw pane output, base64-encoded (standard alphabet).
        data: String,
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
            Event::PaneOutput { .. } => "pane-output",
            Event::OptionChanged { .. } => "option-changed",
            Event::EventsDropped { .. } => "events-dropped",
        }
    }
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
}
