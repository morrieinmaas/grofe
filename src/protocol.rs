//! hellos, control messages, attach frames, codecs.
//!
//! One socket, two modes, chosen by the first line the client sends (spec
//! §4.6). Control mode is newline-delimited JSON both ways ([`Request`] /
//! [`Reply`]); attach mode follows its JSON [`Hello`] with binary [`Frame`]s.
//!
//! **Attach handshake (spec §4.6).** The server answers an attach hello with
//! exactly ONE JSON [`Reply`] line, id [`HELLO_REPLY_ID`]: `Reply::Ok` (its
//! `out` empty) when it accepts, or `Reply::Err` for a refusal (no such
//! session, self-nesting without `--nest`) or a version mismatch, after which it
//! closes the connection. After an ok line both sides switch to binary
//! [`Frame`]s; bytes already buffered past the line belong to the frame
//! stream and must not be dropped.
//!
//! **Attach check.** Before it touches the terminal, `grofe attach` sends
//! [`ATTACH_CHECK`] with its terminal and its `attach-session` arguments as
//! a control request: the server resolves the target and refuses self-nesting as the
//! attach hello would, without changing anything. Only on an `Ok` does the
//! client probe the terminal and send the hello, so a bad target never
//! costs a probe (tmux resolves the target before it opens the terminal).

use std::fmt;
use std::path::PathBuf;

use serde::de::Error as DeError;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio_util::bytes::{BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder, LinesCodec};

use crate::event::Event;
use crate::types::{ClientId, PaneId};

/// The size cap for a control-mode JSON line or a single attach-frame
/// payload, so a bad peer can't make the server allocate unboundedly by
/// claiming (or streaming) an enormous message.
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

/// The longest hello line. A hello carries the client's whole environment,
/// so this sits above macOS's ARG_MAX (1 MiB) and Linux's default
/// environment limit, while still bounding what a connection that has not
/// said who it is can make the server buffer.
pub const HELLO_MAX_LEN: usize = 4 * 1024 * 1024;

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
    /// The attached client this request comes from: set by the server for
    /// an attach connection and for commands its key bindings run; `None`
    /// for one-shot CLI and control requests. Never on the wire.
    #[serde(skip)]
    pub client: Option<ClientId>,
    /// Which command language the request's argv is in (spec D12, §4.6).
    /// Absent on the wire means grofe, so old hellos still parse; TC.1
    /// decides it, and `compat` routes a tmux one to the front end.
    #[serde(default, skip_serializing_if = "Dialect::is_grofe")]
    pub dialect: Dialect,
}

/// A request's command language (spec D12): grofe's own, or tmux's as the
/// tmux front end (`compat::tmux`) translates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    #[default]
    Grofe,
    Tmux,
}

impl Dialect {
    fn is_grofe(&self) -> bool {
        *self == Dialect::Grofe
    }
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
    /// Styled (curly, dotted, dashed) and coloured underlines: SGR `4:x`
    /// and SGR 58.
    pub styled_underline: bool,
    pub sync: bool,
    pub kitty_keys: bool,
    pub kitty_graphics: bool,
    /// One cell's `(width, height)` in pixels, from `CSI 16 t`; `None`
    /// when the terminal did not say. Panes get their pixel size from it
    /// (image tools need it).
    #[serde(default)]
    pub cell_px: Option<(u16, u16)>,
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
        /// `attach -t`, raw: resolved by the server, so `s:1` keeps its
        /// window part. `None` leaves the pick to the server.
        #[serde(default)]
        target: Option<String>,
        /// `attach -d`: detach the session's other clients.
        #[serde(default)]
        detach_others: bool,
        /// `attach --nest`: allow self-nesting (spec §4.6).
        #[serde(default)]
        nest: bool,
        /// The client's terminal (`ttyname` of its stdin): tmux's
        /// `client_tty`, and the client's name when it attaches in the tmux
        /// dialect (FE.2). An older client sends none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tty: Option<String>,
    },
}

/// The protocol version this build speaks: the `"grofe"` field of a hello
/// line (spec §4.6). Until M6 the protocol is internal and may change
/// between commits.
pub const PROTOCOL_VERSION: u32 = 1;

/// The `id` of the one [`Reply`] line that answers an attach hello (a hello
/// carries no request id of its own). See the module docs for the handshake.
pub const HELLO_REPLY_ID: u64 = 0;

/// The control verb of the attach check (module docs): `argv[1]` is the
/// client's terminal (`ttyname` of its stdin, empty without one), the
/// rest `attach-session`'s arguments.
pub const ATTACH_CHECK: &str = "__attach-check";

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
    #[error("environment too large: the hello is {len} bytes, over the 4 MiB limit")]
    TooLarge { len: usize },
}

/// Encodes `hello` as the JSON hello line, embedding [`PROTOCOL_VERSION`]
/// as `"grofe"`.
pub fn encode_hello(hello: &Hello) -> Result<String, ProtocolError> {
    let mut value = serde_json::to_value(hello)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("grofe".to_string(), PROTOCOL_VERSION.into());
    }
    let line = value.to_string();
    if line.len() > HELLO_MAX_LEN {
        return Err(ProtocolError::TooLarge { len: line.len() });
    }
    Ok(line)
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub argv: Vec<String>,
    /// The CLI's stdin, base64, for a `load-buffer -` in `argv` (TM.7):
    /// tmux's client reads its stdin for the server. Off the wire when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// The CLI's umask, for a file `save-buffer` creates (TM.7): tmux's
    /// client creates it, under its own umask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub umask: Option<u32>,
}

/// A control-mode reply: a command's result, or a pushed subscription
/// event. The wire shapes (spec §4.6) don't fit any of serde's built-in
/// enum representations, `ok` is a bool discriminant, and `Event` carries
/// no `id`/`ok` at all, hence the manual impls below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok {
        id: u64,
        out: String,
    },
    /// A failure. `out` and `status` (TC.5, D20) are left off the wire
    /// when empty, so older replies still parse: `out` is what the command
    /// printed before it failed (the CLI prints it on stdout), `status` the
    /// exit status the CLI exits with (1 when absent).
    Err {
        id: u64,
        err: String,
        out: String,
        status: Option<i32>,
    },
    Event(Event),
}

/// A failed command's answer, as a deferred handler sends it: the error,
/// and for `run-shell` the output and the command's own exit status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Failure {
    pub err: String,
    pub out: String,
    pub status: Option<i32>,
}

impl From<String> for Failure {
    fn from(err: String) -> Failure {
        Failure {
            err,
            ..Failure::default()
        }
    }
}

impl Reply {
    /// The error reply for request `id`, with no output and status 1.
    pub fn err(id: u64, err: impl Into<String>) -> Reply {
        Reply::failed(id, Failure::from(err.into()))
    }

    /// The reply for request `id` that failed with `f`.
    pub fn failed(id: u64, f: Failure) -> Reply {
        Reply::Err {
            id,
            err: f.err,
            out: f.out,
            status: f.status,
        }
    }
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
            Reply::Err {
                id,
                err,
                out,
                status,
            } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("ok", &false)?;
                map.serialize_entry("err", err)?;
                if !out.is_empty() {
                    map.serialize_entry("out", out)?;
                }
                if let Some(status) = status {
                    map.serialize_entry("status", status)?;
                }
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
            #[serde(default)]
            out: String,
            #[serde(default)]
            status: Option<i32>,
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
                    out: p.out,
                    status: p.status,
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
    /// server -> client (`suspend-client`, TC.9): give the terminal back and
    /// stop with SIGTSTP; once continued, take it again and send a
    /// `Resize`, which the server answers with a full redraw.
    Suspend,
}

const KIND_INPUT: u8 = 0;
const KIND_RESIZE: u8 = 1;
const KIND_DETACH: u8 = 2;
const KIND_OUTPUT: u8 = 3;
const KIND_EXIT: u8 = 4;
const KIND_SUSPEND: u8 = 5;

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
            KIND_SUSPEND => Frame::Suspend,
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
            Frame::Suspend => (KIND_SUSPEND, Vec::new()),
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
            client: None,
            dialect: Dialect::Grofe,
        }
    }

    #[test]
    fn an_over_cap_hello_names_the_environment() {
        let env = [("BIG", "x".repeat(HELLO_MAX_LEN))];
        let hello = Hello::Control(ClientCtx {
            cwd: PathBuf::from("/"),
            pane: None,
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            client: None,
            dialect: Dialect::Grofe,
        });
        let err = encode_hello(&hello).unwrap_err().to_string();
        assert!(
            err.starts_with("environment too large: the hello is "),
            "{err}"
        );
        assert!(err.ends_with("bytes, over the 4 MiB limit"), "{err}");
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
                styled_underline: true,
                sync: true,
                kitty_keys: true,
                kitty_graphics: true,
                cell_px: Some((10, 20)),
            },
            target: Some("main:2".to_string()),
            detach_others: true,
            nest: true,
            tty: None,
        };
        let line = encode_hello(&hello).unwrap();
        assert_eq!(decode_hello(&line).unwrap(), hello);
    }

    /// The stub edit's `dialect` field: a grofe hello's wire form is
    /// unchanged, a hello without it parses as grofe, and a tmux one
    /// round-trips, in both hello modes.
    #[test]
    fn dialect_is_grofe_unless_the_hello_says_tmux() {
        let grofe = Hello::Control(ctx("/", None, &[]));
        let line = encode_hello(&grofe).unwrap();
        assert!(!line.contains("dialect"), "{line}");
        let old = r#"{"grofe":1,"mode":"control","cwd":"/","pane":null,"env":{}}"#;
        assert_eq!(decode_hello(old).unwrap(), grofe);

        let tmux = ClientCtx {
            dialect: Dialect::Tmux,
            ..ctx("/", None, &[])
        };
        let line = encode_hello(&Hello::Control(tmux.clone())).unwrap();
        assert!(line.contains(r#""dialect":"tmux""#), "{line}");
        assert_eq!(decode_hello(&line).unwrap(), Hello::Control(tmux.clone()));
        let attach = Hello::Attach {
            ctx: tmux,
            cols: 80,
            rows: 24,
            term: "xterm".to_string(),
            caps: Caps::default(),
            target: None,
            detach_others: false,
            nest: false,
            tty: None,
        };
        let line = encode_hello(&attach).unwrap();
        assert_eq!(decode_hello(&line).unwrap(), attach);
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
            target: None,
            detach_others: false,
            nest: false,
            tty: None,
        };
        let line = encode_hello(&hello).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        // ctx's fields sit at the top level, not nested under "ctx".
        assert!(value.get("ctx").is_none());
        assert_eq!(value["cwd"], "/");
        assert_eq!(value["pane"], serde_json::Value::Null);
        assert_eq!(value["target"], serde_json::Value::Null);
        assert_eq!(value["detach_others"], false);
        assert_eq!(value["nest"], false);
        // The server-side client id never goes on the wire.
        assert!(value.get("client").is_none());
    }

    #[test]
    fn attach_hello_without_optional_keys_takes_defaults() {
        let line = r#"{"grofe":1,"mode":"attach","cwd":"/","pane":null,"env":{},
                        "cols":80,"rows":24,"term":"xterm","caps":{}}"#;
        let hello = decode_hello(line).unwrap();
        match hello {
            Hello::Attach {
                ctx,
                target,
                detach_others,
                nest,
                caps,
                ..
            } => {
                assert_eq!(target, None);
                assert!(!detach_others);
                assert!(!nest);
                assert_eq!(ctx.client, None);
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
        assert!(!caps.styled_underline);
        let caps: Caps = serde_json::from_str(r#"{"styled_underline":true}"#).unwrap();
        assert!(caps.styled_underline);
    }

    #[test]
    fn attach_hello_flags_round_trip_and_client_is_not_sent() {
        let mut c = ctx("/", None, &[]);
        c.client = Some(ClientId(4));
        let hello = Hello::Attach {
            ctx: c,
            cols: 80,
            rows: 24,
            term: "xterm".to_string(),
            caps: Caps::default(),
            target: Some("s:1".to_string()),
            detach_others: true,
            nest: true,
            tty: None,
        };
        let decoded = decode_hello(&encode_hello(&hello).unwrap()).unwrap();
        match decoded {
            Hello::Attach {
                ctx,
                target,
                detach_others,
                nest,
                ..
            } => {
                assert_eq!(target.as_deref(), Some("s:1"));
                assert!(detach_others && nest);
                assert_eq!(ctx.client, None);
            }
            other => panic!("expected Attach, got {other:?}"),
        }
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
        let reply = Reply::err(8, "can't find pane: %99");
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            r#"{"id":8,"ok":false,"err":"can't find pane: %99"}"#
        );
        let line = r#"{"id":8,"ok":false,"err":"can't find pane: %99"}"#;
        assert_eq!(serde_json::from_str::<Reply>(line).unwrap(), reply);
    }

    /// D20: a failure may carry output and an exit status; both stay off
    /// the wire when empty, and an older reader ignores them.
    #[test]
    fn reply_err_carries_output_and_status() {
        let reply = Reply::failed(
            9,
            Failure {
                err: String::new(),
                out: "a\n'x' returned 3\n".to_string(),
                status: Some(3),
            },
        );
        let json = serde_json::to_string(&reply).unwrap();
        assert_eq!(
            json,
            r#"{"id":9,"ok":false,"err":"","out":"a\n'x' returned 3\n","status":3}"#
        );
        assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), reply);
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
        frame_round_trip(Frame::Suspend);
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
