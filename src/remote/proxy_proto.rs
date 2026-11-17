//! The proxy protocol `grofe-proxy 1` (spec `docs/specs/2026-11-15-ssh-remote.md`
//! §1, wire format in `docs/protocol-proxy.md`): a hello line each way, then
//! frames `[u8 kind][u32 channel][u32 len][payload]` (big-endian, `len` at
//! most 1 MiB) carrying many channels over one byte stream, with per-channel
//! credit so one busy channel cannot starve the attach stream.
//!
//! The codec, hello and credit windows are pure. [`Mux`] runs them over a
//! blocking reader and writer with one reader thread; the client uses it
//! through [`ProxyConn`], the proxy (`grofe proxy`) uses it directly.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const PROXY_VERSION: u32 = 1;
/// Largest payload of one frame.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// The credit every channel starts with, in each direction.
pub const INITIAL_WINDOW: u32 = 256 * 1024;
/// Largest `data` payload a writer sends, so channels interleave on the wire.
pub const MAX_CHUNK: usize = 32 * 1024;
const HEADER_LEN: usize = 9;
const MAX_HELLO: u64 = 64 * 1024;

const K_OPEN: u8 = 1;
const K_DATA: u8 = 2;
const K_EOF: u8 = 3;
const K_CLOSE: u8 = 4;
const K_PING: u8 = 5;
const K_PONG: u8 = 6;
const K_WINDOW: u8 = 7;

// --- hello -------------------------------------------------------------------

/// The one-line JSON hello each side sends first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyHello {
    #[serde(rename = "grofe-proxy")]
    pub proxy: u32,
    pub grofe: String,
    pub os: String,
    pub arch: String,
    #[serde(default)]
    pub features: Vec<String>,
}

impl ProxyHello {
    /// This build's hello.
    pub fn current(features: &[&str]) -> ProxyHello {
        ProxyHello {
            proxy: PROXY_VERSION,
            grofe: env!("CARGO_PKG_VERSION").to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            features: features.iter().map(|f| f.to_string()).collect(),
        }
    }
}

/// The hello as sent: one JSON line with its newline.
pub fn encode_hello(hello: &ProxyHello) -> String {
    // A struct of strings and integers always serializes.
    let mut line = serde_json::to_string(hello).unwrap_or_default();
    line.push('\n');
    line
}

pub fn decode_hello(line: &str) -> Result<ProxyHello, String> {
    let hello: ProxyHello =
        serde_json::from_str(line.trim_end()).map_err(|e| format!("bad proxy hello: {e}"))?;
    if hello.proxy != PROXY_VERSION {
        return Err(format!(
            "proxy protocol version {}, this side speaks {PROXY_VERSION}",
            hello.proxy
        ));
    }
    Ok(hello)
}

/// Sends `mine`, reads the peer's. Both sides write first, so neither waits
/// on the other (a hello fits any pipe buffer).
pub fn handshake<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mine: &ProxyHello,
) -> io::Result<ProxyHello> {
    writer.write_all(encode_hello(mine).as_bytes())?;
    writer.flush()?;
    let mut line = String::new();
    let n = reader.by_ref().take(MAX_HELLO).read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed before the proxy hello",
        ));
    }
    decode_hello(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// --- frames --------------------------------------------------------------------

/// What an `open` frame asks for (its JSON payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OpenSpec {
    /// Bytes of an attach connection to the server socket (hello included).
    Attach,
    /// Bytes of a control connection to the server socket (hello included).
    Control,
    /// A TCP connection to `127.0.0.1:port` on the proxy's host.
    Tcp { port: u16 },
    /// A file written under `dir`, named `name` (never overwriting).
    Upload { dir: String, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Open(u32, Vec<u8>),
    Data(u32, Vec<u8>),
    Eof(u32),
    /// With a reason, empty for a normal close. Channel 0 closes the connection.
    Close(u32, String),
    Ping(u64),
    Pong(u64),
    /// Credit: the peer may send this many more `data` bytes on the channel.
    Window(u32, u32),
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    #[error("frame payload of {0} bytes exceeds the 1 MiB limit")]
    TooLong(u32),
    #[error("malformed {0} frame")]
    Malformed(&'static str),
}

impl Frame {
    fn parts(&self) -> (u8, u32, Vec<u8>) {
        match self {
            Frame::Open(c, p) => (K_OPEN, *c, p.clone()),
            Frame::Data(c, p) => (K_DATA, *c, p.clone()),
            Frame::Eof(c) => (K_EOF, *c, Vec::new()),
            Frame::Close(c, r) => (K_CLOSE, *c, r.as_bytes().to_vec()),
            Frame::Ping(n) => (K_PING, 0, n.to_be_bytes().to_vec()),
            Frame::Pong(n) => (K_PONG, 0, n.to_be_bytes().to_vec()),
            Frame::Window(c, credit) => (K_WINDOW, *c, credit.to_be_bytes().to_vec()),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let (kind, channel, payload) = self.parts();
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.push(kind);
        out.extend_from_slice(&channel.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }

    fn decode(kind: u8, channel: u32, payload: Vec<u8>) -> Result<Frame, FrameError> {
        let word = |name: &'static str| -> Result<[u8; 8], FrameError> {
            payload
                .as_slice()
                .try_into()
                .map_err(|_| FrameError::Malformed(name))
        };
        Ok(match kind {
            K_OPEN => Frame::Open(channel, payload),
            K_DATA => Frame::Data(channel, payload),
            K_EOF => Frame::Eof(channel),
            K_CLOSE => Frame::Close(channel, String::from_utf8_lossy(&payload).into_owned()),
            K_PING => Frame::Ping(u64::from_be_bytes(word("ping")?)),
            K_PONG => Frame::Pong(u64::from_be_bytes(word("pong")?)),
            K_WINDOW => {
                let b: [u8; 4] = payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| FrameError::Malformed("window"))?;
                Frame::Window(channel, u32::from_be_bytes(b))
            }
            other => return Err(FrameError::UnknownKind(other)),
        })
    }
}

/// Writes one frame (one `write_all`, so a locked writer never interleaves).
pub fn write_frame<W: Write + ?Sized>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let bytes = frame.encode();
    if bytes.len() - HEADER_LEN > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too long",
        ));
    }
    w.write_all(&bytes)?;
    w.flush()
}

/// Reads one frame; `None` at a clean end of stream between frames.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Option<Frame>, FrameError> {
    let mut head = [0u8; HEADER_LEN];
    let mut got = 0;
    while got < HEADER_LEN {
        match r.read(&mut head[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let kind = head[0];
    let channel = u32::from_be_bytes([head[1], head[2], head[3], head[4]]);
    let len = u32::from_be_bytes([head[5], head[6], head[7], head[8]]);
    if len as usize > MAX_PAYLOAD {
        return Err(FrameError::TooLong(len));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Frame::decode(kind, channel, payload).map(Some)
}

// --- flow control --------------------------------------------------------------

/// The credit a sender holds on one channel.
#[derive(Debug)]
pub struct SendWindow(u32);

impl SendWindow {
    pub fn new() -> SendWindow {
        SendWindow(INITIAL_WINDOW)
    }

    /// Takes up to `want` bytes of credit; 0 when none is left.
    pub fn take(&mut self, want: usize) -> usize {
        let n = want.min(self.0 as usize);
        self.0 -= n as u32;
        n
    }

    pub fn add(&mut self, credit: u32) {
        self.0 = self.0.saturating_add(credit);
    }
}

impl Default for SendWindow {
    fn default() -> Self {
        SendWindow::new()
    }
}

/// Why a channel whose peer sent past its credit is closed.
pub const OVERRUN: &str = "flow control: data past the window";

/// What a receiver has consumed since it last granted credit.
#[derive(Debug, Default)]
pub struct RecvWindow(u32);

impl RecvWindow {
    /// Records `n` consumed bytes; once half the initial window is used up,
    /// returns the credit to grant back in a `window` frame.
    pub fn consumed(&mut self, n: usize) -> Option<u32> {
        self.0 = self.0.saturating_add(n as u32);
        if self.0 >= INITIAL_WINDOW / 2 {
            Some(std::mem::take(&mut self.0))
        } else {
            None
        }
    }
}

mod mux;

pub use mux::{Channel, Mux, Role};

// --- the client side ---------------------------------------------------------------

/// The client's end of a proxy connection (to `grofe proxy`'s stdin and
/// stdout, over ssh or a pipe).
pub struct ProxyConn {
    mux: Mux,
    peer: ProxyHello,
}

impl ProxyConn {
    /// Exchanges hellos over `reader` and `writer`, then starts the
    /// connection. Fails on a peer that speaks another proxy version.
    pub fn new<R, W>(reader: R, writer: W) -> io::Result<ProxyConn>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let mut reader = BufReader::new(reader);
        let mut writer = writer;
        let peer = handshake(&mut reader, &mut writer, &ProxyHello::current(&[]))?;
        // The peer opening a channel is not part of version 1.
        let mux = Mux::start(reader, writer, Role::Client, |c, _| {
            c.close("unexpected open")
        });
        Ok(ProxyConn { mux, peer })
    }

    /// The proxy's hello (version, os, arch, features).
    pub fn peer(&self) -> &ProxyHello {
        &self.peer
    }

    pub fn open(&self, spec: &OpenSpec) -> io::Result<Channel> {
        self.mux.open(spec)
    }

    /// Round trip time of a ping.
    pub fn ping(&self, timeout: Duration) -> io::Result<Duration> {
        self.mux.ping(timeout)
    }

    pub fn is_closed(&self) -> bool {
        self.mux.is_closed()
    }

    /// Blocks until the connection ends; why it ended.
    pub fn wait_closed(&self) -> String {
        self.mux.wait_closed()
    }

    /// Uploads `data` as `name` under `dir` on the proxy's host; the path it
    /// ended up at (a clash gets a ` (2)` suffix, never an overwrite).
    pub fn upload(&self, dir: &str, name: &str, mut data: impl Read) -> io::Result<String> {
        let mut chan = self.open(&OpenSpec::Upload {
            dir: dir.into(),
            name: name.into(),
        })?;
        if let Err(e) = io::copy(&mut data, &mut chan) {
            // A reason, so the proxy never takes the part sent for the file.
            chan.close(&format!("upload aborted: {e}"));
            return Err(e);
        }
        chan.shutdown_write()?;
        let mut path = String::new();
        chan.read_to_string(&mut path)?;
        if path.is_empty() {
            return Err(io::Error::other("upload failed"));
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn data_past_the_window_closes_the_channel() {
        let (a, b) = UnixStream::pair().unwrap();
        let mux = Mux::start(a.try_clone().unwrap(), a, Role::Client, |c, _| c.close(""));
        let mut chan = mux.open(&OpenSpec::Attach).unwrap();
        let mut peer = b.try_clone().unwrap();
        // The window, exactly, is fine; one byte more is an overrun.
        let chunk = vec![7u8; MAX_CHUNK];
        for _ in 0..INITIAL_WINDOW as usize / MAX_CHUNK {
            write_frame(&mut peer, &Frame::Data(chan.id(), chunk.clone())).unwrap();
        }
        write_frame(&mut peer, &Frame::Data(chan.id(), vec![1])).unwrap();
        let mut reader = BufReader::new(b);
        let close = loop {
            match read_frame(&mut reader).unwrap() {
                Some(Frame::Close(id, why)) => break (id, why),
                Some(_) => {}
                None => panic!("no close"),
            }
        };
        assert_eq!(close, (chan.id(), OVERRUN.to_string()));
        // What came within the window was dropped with the channel.
        let err = chan.read(&mut [0u8; 16]).unwrap_err();
        assert_eq!(err.to_string(), OVERRUN);
    }
}
