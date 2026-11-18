# Proxy protocol `grofe-proxy 1`

`grofe proxy` runs on the remote machine and speaks this protocol on its stdin and stdout, so
one byte stream (usually `ssh host grofe proxy`) carries many channels: the server's attach and
control connections, TCP forwards and file uploads. It sits beside, not inside, protocol v1
(`protocol-v1.md`): the bytes of an attach or control channel are a v1 connection (the proxy may add
`SSH_AUTH_SOCK` to an attach hello, see below).

Fixtures for every message below are in `tests/fixtures/proxy/`, checked byte for byte by
`tests/protocol_proxy.rs`. Changing a fixture is a breaking change.

## Hello

Each side writes one JSON line first, without waiting for the other:

```json
{"grofe-proxy":1,"grofe":"0.2.0","os":"linux","arch":"x86_64","features":["attach","control","tcp","upload"]}
```

`grofe-proxy` is the protocol version. A side that reads another version closes the connection.
Unknown fields are ignored. A hello is at most 64 KiB.

## Frames

```
[u8 kind][u32 channel][u32 len][payload]      big-endian, len <= 1 MiB (1048576)
```

| kind | name   | channel | payload |
|------|--------|---------|---------|
| 1    | open   | new id  | JSON, see below |
| 2    | data   | id      | bytes |
| 3    | eof    | id      | empty; the sender will send no more data (half close) |
| 4    | close  | id      | UTF-8 reason, empty for a normal close; channel 0 closes the connection |
| 5    | ping   | 0       | u64 nonce |
| 6    | pong   | 0       | the same u64 |
| 7    | window | id      | u32 credit |

An unknown kind, a payload over 1 MiB or a ping, pong or window frame of the wrong size ends
the connection: the receiver sends `close` on channel 0 with an error line, then stops.

Channel ids are chosen by the opener: the client (the side that started `grofe proxy`) opens
odd ids, the proxy even ones. An `open` with an id of the wrong parity, zero, or already in use
is answered with `close` and the reason `bad channel id`. Version 1 has no proxy-initiated opens.
A `close` ends a channel in both directions; frames for a channel no longer known are ignored.

## Channels

`open` carries JSON with a `type`:

| payload | what the proxy does |
|---------|---------------------|
| `{"type":"attach"}` | connects to the server socket (starting the server as any CLI call does) and bridges bytes both ways. The client's first data is the v1 attach hello line. When the hello's `env` has no `SSH_AUTH_SOCK` and the proxy's own is a live agent socket (ssh agent forwarding), the proxy adds it to the hello before passing it on. |
| `{"type":"control"}` | the same, for a v1 control connection (control hello, then requests). |
| `{"type":"tcp","port":N}` | connects to `127.0.0.1:N`, or to `[::1]:N` when that is refused (a server listening on IPv6 localhost only), and bridges bytes. Nothing but the loopback addresses is ever connected to. |
| `{"type":"upload","dir":"...","name":"..."}` | creates `dir` if needed (a leading `~/` is the proxy user's home), writes the data to `dir/name` until `eof`, never overwriting: a taken name gets ` (2)`, ` (3)`, ... appended. It then sends the final absolute path as `data`, `eof`, and `close`. `name` must not contain `/` and must not be `.` or `..`. |

A failure (no server, connection refused, bad name, disk error) is a `close` with the reason.
For bridged channels, the far end closing its side sends `eof` then `close`; `eof` from the
client shuts down the write half of the bridged connection.

## Flow control

Every channel starts with 262144 bytes (256 KiB) of credit in each direction. A sender may
send `data` only while it has credit, and each `data` byte uses one; writers send at most 32768
bytes per frame so channels interleave. A receiver that has consumed 131072 bytes (half the
window) since it last granted credit sends `window` with that count. A receiver never stops
reading frames for one slow channel (data for it waits in a buffer bounded by the credit it
gave), so a stalled tcp channel cannot hold up the attach stream. A peer that sends `data` past
its credit has broken the protocol: the receiver drops what the channel holds and closes it
with the reason `flow control: data past the window`.

## Liveness

Either side may send `ping`; the other answers `pong` with the same nonce. The client pings
every 5 s and treats two missed pongs, EOF, or the ssh process exiting as a drop. The proxy
ends the connection (every channel closed) after 20 s with no frame from the client, so a
client must keep pinging while it is connected.
