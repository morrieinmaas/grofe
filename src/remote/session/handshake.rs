//! Attaching over a link: the attach hello and its reply, what the client
//! learns about the attach (session id, prefix), and the threads that turn
//! the attach channel and the event channel into main-loop events.

use std::io::{Read, Write};
use std::sync::mpsc::SyncSender;

use tokio_util::bytes::BytesMut;
use tokio_util::codec::Decoder as _;

use super::{Conn, DEFAULT_PREFIX, Failure, Kind, Params, post, set_client_tty, tty_for};
use crate::client::{self, Out};
use crate::protocol::{Frame, FrameCodec, HELLO_REPLY_ID, Hello, Reply, encode_hello};
use crate::remote::connect::Link;
use crate::remote::proxy_proto::Channel;
use crate::server::input::{Key, KeyCode, parse_prefix};

/// Attaches over `link`: the attach hello and its reply, the session id
/// and prefix, the reader thread and (when hooks want events) the event
/// thread.
pub(super) fn handshake(
    link: Link,
    p: &Params,
    generation: u64,
    out: &SyncSender<Out>,
) -> Result<Conn, Failure> {
    let mut chan = link
        .open_attach()
        .map_err(|e| Failure::Retry(e.to_string()))?;
    let tty = tty_for(generation);
    set_client_tty(&tty);
    match send_hello(&link, &mut chan, p, &tty) {
        // The remembered session is gone (or refused): attach to the
        // server's choice on a fresh channel rather than end the client.
        Err(Failure::Fatal(_)) if p.fallback && p.opts.target.is_some() => {
            let mut anywhere = p.clone();
            anywhere.opts.target = None;
            chan = link
                .open_attach()
                .map_err(|e| Failure::Retry(e.to_string()))?;
            send_hello(&link, &mut chan, &anywhere, &tty)?;
        }
        other => other?,
    }
    let session = attached_session(&link, &p.hello, &tty);
    let prefix = server_prefix(&link, &p.hello);
    let events = start_readers(&link, &chan, p, generation, out);
    Ok(Conn {
        link,
        tty,
        attach: chan,
        session,
        prefix,
        events,
    })
}

/// Sends the attach hello on `chan` and reads its one reply.
fn send_hello(link: &Link, chan: &mut Channel, p: &Params, tty: &str) -> Result<(), Failure> {
    let (cols, rows) = client::tty_size();
    let hello = encode_hello(&Hello::Attach {
        ctx: p.ctx.clone(),
        cols,
        rows,
        term: p.term.clone(),
        caps: p.caps,
        target: p.opts.target.clone(),
        detach_others: p.opts.detach_others,
        nest: p.opts.nest,
        created: p.opts.created,
        extras: p.opts.extras.clone(),
        tty: Some(tty.to_string()),
    })
    .map_err(|e| Failure::Fatal(e.to_string()))?;
    chan.write_all(format!("{hello}\n").as_bytes())
        .map_err(|e| Failure::Retry(e.to_string()))?;
    match read_line_reply(chan) {
        Ok(Reply::Ok {
            id: HELLO_REPLY_ID, ..
        }) => Ok(()),
        Ok(Reply::Err { err, .. }) => Err(Failure::Fatal(err)),
        Ok(other) => Err(Failure::Fatal(format!(
            "unexpected reply to attach: {other:?}"
        ))),
        Err(e) if link.conn().is_closed() => Err(Failure::Retry(e)),
        Err(e) => Err(Failure::Fatal(e)),
    }
}

/// The id of the session the client with `tty` attached to.
pub(super) fn attached_session(link: &Link, hello: &str, tty: &str) -> Option<String> {
    link.request(
        hello,
        &[
            "list-clients".into(),
            "-F".into(),
            "#{client_tty} #{session_id}".into(),
        ],
    )
    .ok()
    .and_then(|out| {
        out.lines()
            .filter_map(|l| l.split_once(' '))
            .find(|(t, _)| *t == tty)
            .map(|(_, id)| id.trim().to_string())
    })
}

/// The server's prefix key, for the offline detach key.
// Ruling: the global prefix, read once per connection; a session or
// client prefix set elsewhere is not followed. Cost: a session with
// its own prefix detaches offline with the global one.
fn server_prefix(link: &Link, hello: &str) -> Key {
    link.request(
        hello,
        &["show-options".into(), "-gv".into(), "prefix".into()],
    )
    .ok()
    .and_then(|v| parse_prefix(v.trim()).ok().flatten())
    .or_else(|| parse_prefix(DEFAULT_PREFIX).ok().flatten())
    .unwrap_or(Key::plain(KeyCode::Char('b')))
}

/// The attach channel's reader thread and, when hooks name events, the
/// event channel's; that channel, so the connection can close it.
fn start_readers(
    link: &Link,
    chan: &Channel,
    p: &Params,
    generation: u64,
    out: &SyncSender<Out>,
) -> Option<Channel> {
    let reader = chan.clone();
    let tx = out.clone();
    std::thread::spawn(move || read_frames(reader, generation, tx));
    if p.events.is_empty() {
        return None;
    }
    let ctl = link.open_control().ok()?;
    let tx = out.clone();
    let (hello, events) = (p.hello.clone(), p.events.clone());
    let kept = ctl.clone();
    std::thread::spawn(move || read_events(ctl, &hello, &events, generation, tx));
    Some(kept)
}

/// The attach reply line, read a byte at a time so no frame bytes after
/// it are consumed.
fn read_line_reply(chan: &mut impl Read) -> Result<Reply, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match chan.read(&mut byte) {
            Ok(0) => return Err("server closed the connection during attach".to_string()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() >= crate::protocol::MAX_MESSAGE_LEN => {
                return Err("attach reply too long".to_string());
            }
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("waiting for the server: {e}")),
        }
    }
    serde_json::from_slice(&line).map_err(|e| format!("bad reply from server: {e}"))
}

/// Attach frames to the main loop until the channel ends.
fn read_frames(mut chan: impl Read, generation: u64, out: SyncSender<Out>) {
    let mut buf = BytesMut::with_capacity(64 * 1024);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        loop {
            match FrameCodec.decode(&mut buf) {
                Ok(Some(Frame::Output(bytes))) => {
                    if !post(&out, generation, Kind::Output(bytes.to_vec())) {
                        return;
                    }
                }
                Ok(Some(Frame::Exit(reason))) => {
                    post(&out, generation, Kind::Exit(reason));
                    return;
                }
                Ok(Some(Frame::Suspend)) => {
                    post(&out, generation, Kind::Suspend);
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    post(&out, generation, Kind::Lost(format!("bad frame: {e}")));
                    return;
                }
            }
        }
        match chan.read(&mut chunk) {
            Ok(0) => {
                post(&out, generation, Kind::Lost(String::new()));
                return;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                post(&out, generation, Kind::Lost(e.to_string()));
                return;
            }
        }
    }
}

/// Subscribes on `chan` and posts every event line.
fn read_events(
    chan: Channel,
    hello: &str,
    events: &[String],
    generation: u64,
    out: SyncSender<Out>,
) {
    // Only this client's session's events (`subscribe -c`, following a
    // switch): another session's URL or port is not this client's.
    let mut args = vec!["-c".to_string(), super::client_tty().to_string()];
    args.extend(events.iter().cloned());
    pump_events(chan, hello, &args, |value| {
        post(&out, generation, Kind::Server(value))
    });
}

/// Subscribes to `events` on `chan` and hands every event line to
/// `deliver` until it returns false or the channel ends. One reader serves
/// the reply and every line after it, so an event that came in the same
/// read as the reply (the subscribe-time snapshots) is kept.
fn pump_events<S: Read + Write>(
    chan: S,
    hello: &str,
    events: &[String],
    mut deliver: impl FnMut(serde_json::Value) -> bool,
) {
    use std::io::BufRead;
    let mut argv = vec!["subscribe".to_string()];
    argv.extend(events.iter().cloned());
    let Ok(req) = serde_json::to_string(&crate::protocol::Request {
        id: 1,
        argv,
        ..Default::default()
    }) else {
        return;
    };
    let mut reader = std::io::BufReader::new(chan);
    if reader
        .get_mut()
        .write_all(format!("{hello}\n{req}\n").as_bytes())
        .is_err()
    {
        return;
    }
    let mut subscribed = false;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("event").is_some() {
            if !deliver(value) {
                return;
            }
        } else if !subscribed {
            // The subscribe's reply: refused means no events.
            if value.get("ok") != Some(&serde_json::Value::Bool(true)) {
                return;
            }
            subscribed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The subscribe reply and the first event in one read: the event is
    /// still delivered.
    #[test]
    fn an_event_sent_with_the_subscribe_reply_is_kept() {
        use std::io::BufRead;
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(b.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap(); // hello
            r.read_line(&mut line).unwrap(); // subscribe
            (&b).write_all(
                b"{\"id\":1,\"ok\":true,\"out\":\"\"}\n{\"event\":\"pane-echo\",\"pane\":\"%0\",\"echo\":false}\n",
            )
            .unwrap();
            b.shutdown(std::net::Shutdown::Both).unwrap();
        });
        let mut got = Vec::new();
        pump_events(a, "{}", &["pane-echo".to_string()], |v| {
            got.push(v);
            true
        });
        server.join().unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0]["event"], "pane-echo");
    }
}
