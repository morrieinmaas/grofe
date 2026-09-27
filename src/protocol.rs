//! hellos, control messages, attach frames, codecs.
//!
//! One socket, two modes, chosen by the first line the client sends (spec
//! §4.6). Control mode is newline-delimited JSON both ways ([`Request`] /
//! [`Reply`]); attach mode follows its JSON [`Hello`] with binary [`Frame`]s.

use std::fmt;
use std::path::PathBuf;

use serde::de::Error as DeError;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio_util::bytes::{BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder, LinesCodec};

use crate::event::Event;
use crate::types::PaneId;

/// The size cap for a control-mode JSON line or a single attach-frame
/// payload, so a bad peer can't make the server allocate unboundedly by
/// claiming (or streaming) an enormous message.
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

/// Builds the [`LinesCodec`] used for control-mode JSON lines, capped at
/// [`MAX_MESSAGE_LEN`].
pub fn lines_codec() -> LinesCodec {
    LinesCodec::new_with_max_length(MAX_MESSAGE_LEN)
}

/// The context a client carries in its hello (spec §4.6), so `Current`
/// targets, default cwd and environment resolve correctly for a CLI call
/// made from inside a pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCtx {
    pub cwd: PathBuf,
    pub pane: Option<PaneId>,
    /// A JSON object on the wire; kept as ordered pairs internally since
    /// environments may (rarely) repeat a key and order can matter to a
    /// spawned pane.
    #[serde(with = "env_map")]
    pub env: Vec<(String, String)>,
}

/// (De)serializes [`ClientCtx::env`] as a JSON object, per spec §4.6.
mod env_map {
    use std::collections::BTreeMap;

    use serde::ser::SerializeMap;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(env: &[(String, String)], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(env.len()))?;
        for (key, value) in env {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<(String, String)>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let map = BTreeMap::<String, String>::deserialize(deserializer)?;
        Ok(map.into_iter().collect())
    }
}

/// Per-client rendering/input capabilities, probed once against the outer
/// terminal before the hello (spec §4.4; M2 fills in the probing). Every
/// field defaults to `false`, and the struct-level `default` means a caps
/// object missing some (or all) fields, from an older or newer client,
/// still parses instead of failing the whole hello.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Caps {
    pub truecolor: bool,
    pub sync: bool,
    pub kitty_keys: bool,
    pub kitty_graphics: bool,
}

/// The JSON hello a client sends first (spec §4.6), choosing the mode for
/// the rest of the connection. [`encode_hello`] / [`decode_hello`] add the
/// `"grofe"` protocol version around this shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Hello {
    Control(ClientCtx),
    Attach {
        #[serde(flatten)]
        ctx: ClientCtx,
        cols: u16,
        rows: u16,
        term: String,
        caps: Caps,
        /// The session to attach to; `None` when the client leaves it to
        /// the server to pick (or create) one.
        #[serde(default)]
        session: Option<String>,
    },
}

/// The protocol version this build speaks: the `"grofe"` field of a hello
/// line (spec §4.6). Until M6 the protocol is internal and may change
/// between commits.
pub const PROTOCOL_VERSION: u32 = 1;

/// Which side of a version mismatch needs to upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Client,
    Server,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Client => "client",
            Side::Server => "server",
        })
    }
}

/// Errors decoding a hello line.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("hello has no valid \"grofe\" protocol version")]
    MissingVersion,
    #[error(
        "protocol mismatch: this build speaks v{ours}, the peer speaks v{theirs}; upgrade the {upgrade}"
    )]
    VersionMismatch {
        ours: u32,
        theirs: u32,
        upgrade: Side,
    },
    #[error("malformed hello: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// Encodes `hello` as the JSON hello line, embedding [`PROTOCOL_VERSION`]
/// as `"grofe"`.
pub fn encode_hello(hello: &Hello) -> Result<String, ProtocolError> {
    let mut value = serde_json::to_value(hello)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("grofe".to_string(), PROTOCOL_VERSION.into());
    }
    Ok(value.to_string())
}

/// Decodes a hello line, checking the protocol version before the shape so
/// a mismatch is reported on its own terms rather than as a parse failure.
pub fn decode_hello(line: &str) -> Result<Hello, ProtocolError> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    let theirs = value
        .get("grofe")
        .and_then(serde_json::Value::as_u64)
        .ok_or(ProtocolError::MissingVersion)?;
    let theirs = theirs as u32;
    if theirs != PROTOCOL_VERSION {
        let upgrade = if theirs < PROTOCOL_VERSION {
            Side::Client
        } else {
            Side::Server
        };
        return Err(ProtocolError::VersionMismatch {
            ours: PROTOCOL_VERSION,
            theirs,
            upgrade,
        });
    }
    Ok(serde_json::from_value(value)?)
}

/// A control-mode request (spec §4.6): one JSON line, `argv` shaped like
/// the CLI's own `argv[1..]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub argv: Vec<String>,
}

/// A control-mode reply: a command's result, or a pushed subscription
/// event. The wire shapes (spec §4.6) don't fit any of serde's built-in
/// enum representations, `ok` is a bool discriminant, and `Event` carries
/// no `id`/`ok` at all, hence the manual impls below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok { id: u64, out: String },
    Err { id: u64, err: String },
    Event(Event),
}

impl Serialize for Reply {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Reply::Ok { id, out } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("out", out)?;
                map.end()
            }
            Reply::Err { id, err } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("ok", &false)?;
                map.serialize_entry("err", err)?;
                map.end()
            }
            Reply::Event(event) => event.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Reply {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("event").is_some() {
            return Event::deserialize(value)
                .map(Reply::Event)
                .map_err(DeError::custom);
        }
        #[derive(Deserialize)]
        struct OkPayload {
            id: u64,
            out: String,
        }
        #[derive(Deserialize)]
        struct ErrPayload {
            id: u64,
            err: String,
        }
        match value.get("ok").and_then(serde_json::Value::as_bool) {
            Some(true) => serde_json::from_value::<OkPayload>(value)
                .map(|p| Reply::Ok {
                    id: p.id,
                    out: p.out,
                })
                .map_err(DeError::custom),
            Some(false) => serde_json::from_value::<ErrPayload>(value)
                .map(|p| Reply::Err {
                    id: p.id,
                    err: p.err,
                })
                .map_err(DeError::custom),
            None => Err(DeError::custom("reply has neither \"ok\" nor \"event\"")),
        }
    }
}

/// An attach-mode binary frame (spec §4.6): `[u8 kind][u32 BE len][payload]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// client -> server: raw bytes from the tty.
    Input(Vec<u8>),
    /// client -> server.
    Resize(u16, u16),
    /// client -> server.
    Detach,
    /// server -> client: bytes to write to the tty.
    Output(Vec<u8>),
    /// server -> client: `detached`, `server exited`, `session killed`, ...
    Exit(String),
}

const KIND_INPUT: u8 = 0;
const KIND_RESIZE: u8 = 1;
const KIND_DETACH: u8 = 2;
const KIND_OUTPUT: u8 = 3;
const KIND_EXIT: u8 = 4;

/// One kind byte plus a big-endian `u32` payload length.
const FRAME_HEADER_LEN: usize = 5;

/// Errors framing an attach-mode byte stream.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("attach frame of {len} bytes exceeds the {max} byte limit")]
    TooLarge { len: usize, max: usize },
    #[error("unknown attach frame kind {0}")]
    UnknownKind(u8),
    #[error("attach frame payload is not valid UTF-8")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("attach resize frame payload must be 4 bytes, got {0}")]
    BadResizePayload(usize),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Codec for attach-mode binary frames, for use with
/// [`tokio_util::codec::Framed`].
#[derive(Debug, Default)]
pub struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = FrameError;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        if buf.len() < FRAME_HEADER_LEN {
            return Ok(None);
        }
        let kind = buf[0];
        let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
        if len > MAX_MESSAGE_LEN {
            return Err(FrameError::TooLarge {
                len,
                max: MAX_MESSAGE_LEN,
            });
        }
        let total = FRAME_HEADER_LEN + len;
        if buf.len() < total {
            // Bounded: `len` is already capped above, so this can't grow
            // past MAX_MESSAGE_LEN plus the header.
            buf.reserve(total - buf.len());
            return Ok(None);
        }
        let payload = buf.split_to(total).split_off(FRAME_HEADER_LEN).to_vec();
        Ok(Some(match kind {
            KIND_INPUT => Frame::Input(payload),
            KIND_RESIZE => {
                if payload.len() != 4 {
                    return Err(FrameError::BadResizePayload(payload.len()));
                }
                let cols = u16::from_be_bytes([payload[0], payload[1]]);
                let rows = u16::from_be_bytes([payload[2], payload[3]]);
                Frame::Resize(cols, rows)
            }
            KIND_DETACH => Frame::Detach,
            KIND_OUTPUT => Frame::Output(payload),
            KIND_EXIT => Frame::Exit(String::from_utf8(payload)?),
            other => return Err(FrameError::UnknownKind(other)),
        }))
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = FrameError;

    fn encode(&mut self, frame: Frame, buf: &mut BytesMut) -> Result<(), FrameError> {
        let (kind, payload) = match frame {
            Frame::Input(bytes) => (KIND_INPUT, bytes),
            Frame::Resize(cols, rows) => {
                let mut payload = Vec::with_capacity(4);
                payload.extend_from_slice(&cols.to_be_bytes());
                payload.extend_from_slice(&rows.to_be_bytes());
                (KIND_RESIZE, payload)
            }
            Frame::Detach => (KIND_DETACH, Vec::new()),
            Frame::Output(bytes) => (KIND_OUTPUT, bytes),
            Frame::Exit(reason) => (KIND_EXIT, reason.into_bytes()),
        };
        if payload.len() > MAX_MESSAGE_LEN {
            return Err(FrameError::TooLarge {
                len: payload.len(),
                max: MAX_MESSAGE_LEN,
            });
        }
        buf.reserve(FRAME_HEADER_LEN + payload.len());
        buf.put_u8(kind);
        buf.put_u32(payload.len() as u32);
        buf.put_slice(&payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn ctx(cwd: &str, pane: Option<u32>, env: &[(&str, &str)]) -> ClientCtx {
        ClientCtx {
            cwd: PathBuf::from(cwd),
            pane: pane.map(PaneId),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn control_hello_round_trips() {
        let hello = Hello::Control(ctx("/home/u/src", Some(3), &[("SSH_AUTH_SOCK", "/tmp/a")]));
        let line = encode_hello(&hello).unwrap();
        assert_eq!(decode_hello(&line).unwrap(), hello);
    }

    #[test]
    fn attach_hello_round_trips() {
        let hello = Hello::Attach {
            ctx: ctx("/home/u/src", None, &[("A", "1"), ("B", "2")]),
            cols: 200,
            rows: 50,
            term: "xterm-ghostty".to_string(),
            caps: Caps {
                truecolor: true,
                sync: true,
                kitty_keys: true,
                kitty_graphics: true,
            },
            session: Some("main".to_string()),
        };
        let line = encode_hello(&hello).unwrap();
        assert_eq!(decode_hello(&line).unwrap(), hello);
    }

    #[test]
    fn hello_wire_shape_matches_spec_example() {
        let hello = Hello::Control(ctx("/home/u/src", Some(3), &[("SSH_AUTH_SOCK", "...")]));
        let line = encode_hello(&hello).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["grofe"], 1);
        assert_eq!(value["mode"], "control");
        assert_eq!(value["cwd"], "/home/u/src");
        assert_eq!(value["pane"], "%3");
        assert_eq!(value["env"]["SSH_AUTH_SOCK"], "...");
    }

    #[test]
    fn attach_hello_flattens_ctx_alongside_attach_fields() {
        let hello = Hello::Attach {
            ctx: ctx("/", None, &[]),
            cols: 200,
            rows: 50,
            term: "xterm-ghostty".to_string(),
            caps: Caps::default(),
            session: None,
        };
        let line = encode_hello(&hello).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        // ctx's fields sit at the top level, not nested under "ctx".
        assert!(value.get("ctx").is_none());
        assert_eq!(value["cwd"], "/");
        assert_eq!(value["pane"], serde_json::Value::Null);
        assert_eq!(value["session"], serde_json::Value::Null);
    }

    #[test]
    fn attach_hello_without_session_key_defaults_to_none() {
        let line = r#"{"grofe":1,"mode":"attach","cwd":"/","pane":null,"env":{},
                        "cols":80,"rows":24,"term":"xterm","caps":{}}"#;
        let hello = decode_hello(line).unwrap();
        match hello {
            Hello::Attach { session, caps, .. } => {
                assert_eq!(session, None);
                assert_eq!(caps, Caps::default());
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[test]
    fn missing_version_is_an_error() {
        let line = r#"{"mode":"control","cwd":"/","pane":null,"env":{}}"#;
        assert!(matches!(
            decode_hello(line),
            Err(ProtocolError::MissingVersion)
        ));
    }

    #[test]
    fn older_client_is_told_to_upgrade_the_client() {
        let line = r#"{"grofe":0,"mode":"control","cwd":"/","pane":null,"env":{}}"#;
        let err = decode_hello(line).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::VersionMismatch {
                upgrade: Side::Client,
                ..
            }
        ));
        assert!(err.to_string().contains("upgrade the client"));
    }

    #[test]
    fn newer_client_is_told_to_upgrade_the_server() {
        let line = r#"{"grofe":2,"mode":"control","cwd":"/","pane":null,"env":{}}"#;
        let err = decode_hello(line).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::VersionMismatch {
                upgrade: Side::Server,
                ..
            }
        ));
        assert!(err.to_string().contains("upgrade the server"));
    }

    #[test]
    fn malformed_hello_is_an_error() {
        assert!(decode_hello("not json").is_err());
        assert!(decode_hello(r#"{"grofe":1}"#).is_err());
    }

    #[test]
    fn caps_missing_fields_default_to_false() {
        let caps: Caps = serde_json::from_str("{}").unwrap();
        assert_eq!(caps, Caps::default());

        let caps: Caps = serde_json::from_str(r#"{"truecolor":true}"#).unwrap();
        assert!(caps.truecolor);
        assert!(!caps.sync);
    }

    #[test]
    fn caps_ignores_unknown_fields_from_a_newer_client() {
        let caps: Caps = serde_json::from_str(r#"{"truecolor":true,"future_flag":true}"#).unwrap();
        assert!(caps.truecolor);
    }

    #[test]
    fn env_round_trips_as_json_object() {
        let value = ctx("/", None, &[("A", "1"), ("B", "2")]);
        let json = serde_json::to_string(&value).unwrap();
        assert!(json.contains(r#""env":{"A":"1","B":"2"}"#));
        let back: ClientCtx = serde_json::from_str(&json).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn reply_ok_matches_spec_wire_shape() {
        let reply = Reply::Ok {
            id: 7,
            out: "%4\n".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            r#"{"id":7,"ok":true,"out":"%4\n"}"#
        );
        let line = r#"{"id":7,"ok":true,"out":"%4\n"}"#;
        assert_eq!(serde_json::from_str::<Reply>(line).unwrap(), reply);
    }

    #[test]
    fn reply_err_matches_spec_wire_shape() {
        let reply = Reply::Err {
            id: 8,
            err: "can't find pane: %99".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            r#"{"id":8,"ok":false,"err":"can't find pane: %99"}"#
        );
        let line = r#"{"id":8,"ok":false,"err":"can't find pane: %99"}"#;
        assert_eq!(serde_json::from_str::<Reply>(line).unwrap(), reply);
    }

    #[test]
    fn reply_event_carries_no_id_or_ok() {
        let reply = Reply::Event(Event::PaneExited {
            pane: PaneId(4),
            uid: "01J".to_string(),
            status: Some(0),
        });
        let json = serde_json::to_string(&reply).unwrap();
        assert_eq!(
            json,
            r#"{"event":"pane-exited","pane":"%4","uid":"01J","status":0}"#
        );
        assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), reply);
    }

    #[test]
    fn reply_missing_ok_and_event_is_an_error() {
        assert!(serde_json::from_str::<Reply>(r#"{"id":1}"#).is_err());
    }

    fn frame_round_trip(frame: Frame) {
        let mut buf = BytesMut::new();
        let mut codec = FrameCodec;
        codec.encode(frame.clone(), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, frame);
        assert!(buf.is_empty());
    }

    #[test]
    fn input_frame_round_trips() {
        frame_round_trip(Frame::Input(b"hello".to_vec()));
        frame_round_trip(Frame::Input(Vec::new()));
    }

    #[test]
    fn resize_frame_round_trips() {
        frame_round_trip(Frame::Resize(200, 50));
    }

    #[test]
    fn detach_frame_round_trips() {
        frame_round_trip(Frame::Detach);
    }

    #[test]
    fn output_frame_round_trips() {
        frame_round_trip(Frame::Output(vec![0, 1, 2, 255]));
    }

    #[test]
    fn exit_frame_round_trips() {
        frame_round_trip(Frame::Exit("detached".to_string()));
    }

    #[test]
    fn frame_encodes_to_exact_wire_layout() {
        let mut buf = BytesMut::new();
        FrameCodec
            .encode(Frame::Input(vec![1, 2, 3]), &mut buf)
            .unwrap();
        assert_eq!(&buf[..], &[KIND_INPUT, 0, 0, 0, 3, 1, 2, 3]);
    }

    #[test]
    fn decode_waits_for_a_full_header_before_deciding() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[KIND_INPUT, 0, 0]);
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
        assert_eq!(buf.len(), 3, "a partial header must not be consumed");
    }

    #[test]
    fn decode_handles_a_frame_split_across_many_reads() {
        let mut codec = FrameCodec;
        let mut whole = BytesMut::new();
        codec
            .encode(Frame::Output(b"split me".to_vec()), &mut whole)
            .unwrap();

        let mut buf = BytesMut::new();
        let mut got = None;
        for byte in whole {
            buf.extend_from_slice(&[byte]);
            if let Some(frame) = codec.decode(&mut buf).unwrap() {
                got = Some(frame);
                break;
            }
        }
        assert_eq!(got, Some(Frame::Output(b"split me".to_vec())));
        assert!(buf.is_empty());
    }

    #[test]
    fn decode_leaves_the_next_frame_in_the_buffer() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        codec.encode(Frame::Detach, &mut buf).unwrap();
        codec
            .encode(Frame::Input(b"next".to_vec()), &mut buf)
            .unwrap();

        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::Detach));
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Input(b"next".to_vec()))
        );
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn oversized_declared_length_errors_without_buffering_the_payload() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(KIND_INPUT);
        buf.put_u32((MAX_MESSAGE_LEN + 1) as u32);
        // No payload bytes at all: a malicious peer that only sends the
        // header must still be rejected immediately, not left pending.
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { .. }));
    }

    #[test]
    fn unknown_kind_is_an_error() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(99);
        buf.put_u32(0);
        assert!(matches!(
            codec.decode(&mut buf).unwrap_err(),
            FrameError::UnknownKind(99)
        ));
    }

    #[test]
    fn bad_resize_payload_length_is_an_error() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(KIND_RESIZE);
        buf.put_u32(2);
        buf.put_slice(&[0, 0]);
        assert!(matches!(
            codec.decode(&mut buf).unwrap_err(),
            FrameError::BadResizePayload(2)
        ));
    }

    #[test]
    fn non_utf8_exit_payload_is_an_error() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(KIND_EXIT);
        buf.put_u32(1);
        buf.put_slice(&[0xff]);
        assert!(matches!(
            codec.decode(&mut buf).unwrap_err(),
            FrameError::InvalidUtf8(_)
        ));
    }

    proptest! {
        #[test]
        fn input_frame_round_trips_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
            let mut codec = FrameCodec;
            let mut buf = BytesMut::new();
            codec.encode(Frame::Input(bytes.clone()), &mut buf).unwrap();
            let decoded = codec.decode(&mut buf).unwrap().unwrap();
            prop_assert_eq!(decoded, Frame::Input(bytes));
            prop_assert!(buf.is_empty());
        }

        #[test]
        fn exit_frame_round_trips_arbitrary_strings(reason in ".*") {
            let mut codec = FrameCodec;
            let mut buf = BytesMut::new();
            codec.encode(Frame::Exit(reason.clone()), &mut buf).unwrap();
            let decoded = codec.decode(&mut buf).unwrap().unwrap();
            prop_assert_eq!(decoded, Frame::Exit(reason));
            prop_assert!(buf.is_empty());
        }

        #[test]
        fn input_frame_round_trips_split_across_two_reads(bytes in prop::collection::vec(any::<u8>(), 0..4096), split in 0usize..FRAME_HEADER_LEN + 4096) {
            let mut codec = FrameCodec;
            let mut whole = BytesMut::new();
            codec.encode(Frame::Input(bytes.clone()), &mut whole).unwrap();
            let split = split.min(whole.len());

            let mut buf = BytesMut::new();
            buf.extend_from_slice(&whole[..split]);
            let first = codec.decode(&mut buf).unwrap();
            buf.extend_from_slice(&whole[split..]);
            let frame = match first {
                Some(frame) => frame,
                None => codec.decode(&mut buf).unwrap().unwrap(),
            };
            prop_assert_eq!(frame, Frame::Input(bytes));
        }
    }
}
