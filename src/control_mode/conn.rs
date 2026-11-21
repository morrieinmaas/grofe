//! The bridge's connections to grofe's server: one JSON control request per
//! connection, and the event connection.

use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::protocol::{ClientCtx, Dialect, Hello, Reply, Request, encode_hello};
use crate::types::PaneId;

use super::Options;

/// The ctx of a request: the hello of the front end, with `pane` the
/// attached session's active pane so a command with no target resolves there.
pub(super) fn ctx(opts: &Options, pane: Option<&str>) -> ClientCtx {
    ClientCtx {
        cwd: opts.cwd.clone(),
        pane: pane.and_then(|p| p.parse::<PaneId>().ok()),
        env: opts.env.clone(),
        client: None,
        dialect: Dialect::Tmux,
        request: Default::default(),
    }
}

fn hello_line(ctx: &ClientCtx) -> Result<String, String> {
    encode_hello(&Hello::Control(ctx.clone())).map_err(|e| e.to_string())
}

/// Runs one request on its own connection: `Ok(out)`, or the error text.
pub(super) fn exchange(socket: &Path, ctx: &ClientCtx, argv: &[String]) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("server gone: {e}"))?;
    let req = serde_json::to_string(&Request {
        id: 1,
        argv: argv.to_vec(),
        ..Request::default()
    })
    .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{}\n{req}\n", hello_line(ctx)?).as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;
    let mut reader = std::io::BufReader::new(&stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Err("server closed the connection".to_string()),
            Ok(_) => {}
        }
        match serde_json::from_str::<Reply>(&line) {
            Ok(Reply::Ok { out, .. }) => return Ok(out),
            Ok(Reply::Err { err, out, .. }) => {
                return Err(format!("{out}{err}").trim_end().to_string());
            }
            Ok(Reply::Event(_)) => {}
            Err(e) => return Err(format!("bad reply from server: {e}")),
        }
    }
}

/// Opens the event connection: every event, and `pane-output`.
pub(super) fn subscribe(opts: &Options) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(&opts.socket).map_err(|e| format!("server gone: {e}"))?;
    let mut text = format!("{}\n", hello_line(&ctx(opts, None))?);
    for (id, argv) in [
        (1, vec!["subscribe"]),
        (2, vec!["subscribe", "pane-output"]),
    ] {
        let req = Request {
            id,
            argv: argv.into_iter().map(String::from).collect(),
            ..Request::default()
        };
        text.push_str(&serde_json::to_string(&req).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    stream
        .write_all(text.as_bytes())
        .map_err(|e| format!("subscribing: {e}"))?;
    Ok(stream)
}
