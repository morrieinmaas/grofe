# grofe protocol v1

Frozen 2026-10-27 (spec §4.6, §7.2, §7.3). This file and the fixtures in
`tests/fixtures/protocol/v1/` are the record: one JSON line per message and
event kind, each checked byte for byte by `tests/protocol_v1.rs`. A change
that rewrites a fixture line is not additive and is not v1.

Contents: 1 Transport · 2 Hellos · 3 Requests · 4 Replies · 5 Errors ·
6 Events · 7 Attach frames · 8 Rules · 9 `grofe control`.

## 1. Transport

One Unix socket (`$GROFE` names it in a pane). The first line a client sends
is its hello and chooses the mode: `control` (newline-delimited JSON both
ways) or `attach` (a JSON reply, then binary frames). A line is at most
16 MiB; a hello at most 4 MiB. Key order inside an object carries no meaning;
the fixtures show the order this build writes.

## 2. Hellos

Fixtures: `hello-control.json`, `hello-control-tmux.json`,
`hello-attach.json`.

Common fields (every hello):

| Field | Type | Meaning |
|---|---|---|
| `grofe` | integer, required | the protocol version, `1` |
| `mode` | `"control"` or `"attach"`, required | the mode for the connection |
| `dialect` | `"grofe"` or `"tmux"`, optional, default `grofe` | the command language of the argv (`tmux` when the CLI was invoked as `tmux`, D12) |
| `cwd` | string, required | the client's working directory |
| `pane` | `"%N"` or `null`, optional, default `null` | `$GROFE_PANE` when set; the `Current` target |
| `env` | object of strings, required | the client's environment, for panes it starts |

Attach adds `cols`, `rows` (integers), `term` (string), `caps` (object of
booleans `truecolor`, `styled_underline`, `sync`, `kitty_keys`,
`kitty_graphics`, `sixel`, each defaulting to `false`, and `cell_px`, `[w, h]`
or `null`), `target` (string or `null`, `attach -t` raw), `detach_others` and
`nest` (booleans, default `false`), and optionally `tty` (string) and
`created` (boolean, default `false`: the session is the one this client's
`new-session` just made, which fires no `client-attached` hook), and
optionally `extras`, sent only when one of its fields is set: an object of
`cwd` (string, `attach-session -c`), `read_only` (`-r`), `flags` (string,
`-f`), `no_env` (`-E`) and `kill_parent` (`-x`), the booleans defaulting to
`false`. A server older than `extras` ignores it, so those flags do nothing
there.

An unknown field in a hello is ignored. An unknown `mode`, or a hello that
does not parse, gets `malformed hello: ...` and a close.

## 3. Requests

Control mode only. Fixtures: `request.json`, `request-subscribe.json`,
`request-stdin.json`.

```json
{"id":7,"argv":["split-window","-h","-t","%3"]}
```

| Field | Type | Meaning |
|---|---|---|
| `id` | unsigned integer, required | echoed in the reply |
| `argv` | array of strings, required | a command as the CLI's `argv[1..]`, in the hello's dialect |
| `stdin` | base64 string, optional | the CLI's stdin for `load-buffer -` |
| `umask` | integer, optional | the CLI's umask for `save-buffer` |

An unknown field is ignored. `subscribe <event>... [-t target]` is an
ordinary request that also makes this connection receive events (section 6).
The server runs one connection's requests one at a time, in order.

## 4. Replies

Fixtures: `reply-ok.json`, `reply-err.json`, `reply-err-status.json`,
`reply-attach-ok.json`.

```json
{"id":7,"ok":true,"out":"%4\n"}
{"id":8,"ok":false,"err":"can't find pane: %99"}
{"id":11,"ok":false,"err":"exited 3","out":"partial\n","status":3}
```

A reply echoes its request's `id` and has `ok`: true with `out` (the
command's output, possibly empty), or false with `err`, plus `out` (what the
command printed before it failed) and `status` (the exit status the one-shot
CLI uses, 1 when absent) only when they are not empty. Replies to one
connection's requests arrive in request order; events (section 6) carry no
`id` and arrive between them. A client ignores reply fields it does not know.

An attach hello is answered by exactly one reply line with `id` `0`: `ok`
true (`reply-attach-ok.json`) and then binary frames in both directions, or
`ok` false and a close.

## 5. Errors

An error is a reply with `ok: false`. The ones the protocol itself defines:

| Case | `err` | Fixture | Connection |
|---|---|---|---|
| hello of a newer version | `protocol mismatch: this build speaks v1, the peer speaks v2; upgrade the server` | `error-version-newer.json` | closed |
| hello of an older version | `protocol mismatch: this build speaks v1, the peer speaks v0; upgrade the client` | `error-version-older.json` | closed |
| hello with no integer `grofe` | `hello has no valid "grofe" protocol version` | `error-no-version.json` | closed |
| unknown hello `mode`, unparsable hello | `malformed hello: ...` | | closed |
| unknown verb | `unknown command: <verb>` | `error-unknown-verb.json` | open |
| unknown event in `subscribe` | `unknown event: <name>` | `error-unknown-event.json` | open |
| request line that does not parse | `malformed request: ...` (`id` 0) | | open |
| `grofe control` only, from the bridge itself and never sent to the server: stdin line over 16 MiB | `request line over 16777216 bytes` (`id` 0) | | open |
| `grofe control` only, likewise: stdin line that is not UTF-8 | `request line is not valid UTF-8` (`id` 0) | | open |

**Version mismatch.** Every hello carries `"grofe": 1`; the server speaks one
version and there is no negotiation. A hello of another version, in either
mode, gets one error line (the `id` is `0`) and the connection is closed. The
side named is the one with the lower version: `upgrade the client` when the
peer is older, `upgrade the server` when it is newer. The wording is part of
v1.

The two bridge texts are `ok: false` reply lines the bridge prints on stdout
in place of a request it could not send (section 9). When the server closes
the connection with a request unanswered, the bridge prints nothing more and
exits 1 with `server closed the connection without replying` on stderr.

Other errors are the command's own (tmux 3.7b's texts for tmux-dialect
requests, grofe's for grofe-dialect ones).

## 6. Events

A connection that sent `subscribe` receives events as single JSON lines whose
`event` field names the event, followed by its payload. Ids are runtime ids
(`%N` panes, `@N` windows, `$N` sessions); `uid` is a pane's stable id;
`client` is `#{client_name}`. A nullable field is always present, as `null`.
Fixture: `event-<name>.json` for each event, plus `event-pane-exited-killed`
and `event-option-changed-unset` for the null shapes.

| Event | Payload after `event` | Fires when |
|---|---|---|
| `pane-exited` | `pane`, `uid`, `status` (nullable: null when a kill removed a live pane) | a pane's process exited, or a kill removed a live pane |
| `pane-died` | `pane`, `uid`, `window`, `status` (nullable) | the process exited and `remain-on-exit` kept the pane |
| `pane-output` | `pane`, `data` (base64, standard alphabet) | a pane produced output; only for a subscription naming it |
| `pane-focus` | `pane`, `window`, `session`, `client` | a pane became an attached client's active pane, or that client's terminal regained focus |
| `pane-resized` | `pane`, `window` | `resize-pane` succeeded |
| `window-focused` | `window`, `session`, `pane` | `select-window` succeeded |
| `window-created` | `window`, `session`, `pane` | `new-window` made a window |
| `window-resized` | `window`, `session` | `resize-window` succeeded |
| `session-created` | `session`, `name` | a session was made |
| `session-closed` | `session`, `name` | a session was removed |
| `client-session-changed` | `client`, `session` | a client attached or switched session |
| `client-resized` | `client`, `session`, `cols`, `rows` | a client attached or reported a size |
| `alert-bell` | `window`, `session`, `pane` | a bell in a monitored window |
| `alert-activity` | `window`, `session`, `pane` | output in a monitored window |
| `alert-silence` | `window`, `session`, `pane` | silence in a monitored window |
| `option-changed` | `scope` (`global`, `$N`, `@N` or `%N`), `name`, `value` (nullable: null once unset) | an option's value changed |
| `events-dropped` | `count` | a notice, not subscribable: the subscriber's queue overflowed; sent before its next delivered event |

The full table (emitters, consumers, exact firing rules) is spec §7.2.

**`subscribe <event>... [-t target]`.** Names are literal or `*` patterns; an
unknown literal is an error. A bare `subscribe` and every pattern match the
v1.0 events above except `pane-output`; that event, and any event added after
v1.0, reach only a subscription naming them literally. `-t` limits events to
a pane, window or session. A later `subscribe` on the same connection
replaces the earlier one.

**Delivery.** In the order things happened. Each subscriber has a queue of
1024 events; on overflow events are dropped and `events-dropped` with the
count comes before the next one delivered. A slow subscriber never delays the
server or another subscriber. Nothing is replayed.

## 7. Attach frames

After an ok attach reply both sides send `[u8 kind][u32 big-endian
length][payload]`, the length at most 16 MiB. Fixtures `frame-<kind>.hex`
hold the whole frame as one line of hex.

| Kind | Name | Direction | Payload |
|---|---|---|---|
| 0 | `input` | client to server | raw tty bytes |
| 1 | `resize` | client to server | `u16 cols`, `u16 rows`, big-endian |
| 2 | `detach` | client to server | none |
| 3 | `output` | server to client | bytes to write to the tty |
| 4 | `exit` | server to client | UTF-8 reason (`detached`, `server exited`, `session killed`, ...) |
| 5 | `suspend` | server to client | none; give the terminal back and stop with SIGTSTP, then send a `resize` once continued |

An unknown kind closes the connection. These six kinds are the whole set for
v1.

## 8. Rules for changes inside v1

Additive only. A change may add:

- optional fields to a hello or a request, which default when absent;
- fields to a reply or an event;
- events, which reach only subscriptions that name them literally;
- verbs, and optional flags on existing verbs.

It may not remove or rename a field, an event or a verb; change a field's
type, nullability or meaning; make an optional field required; add an attach
frame kind or change a frame's payload; or reword the version-mismatch error.
Anything else is `grofe: 2`.

The server ignores fields it does not know in a hello or a request. A client
or plugin must ignore fields it does not know in a reply or an event, so a v1
plugin keeps working against any later v1 server. An unknown verb gets
`unknown command: <verb>` and the connection stays open.

## 9. `grofe control`

A stdio bridge, so a plugin in any language needs no socket code.

```sh
printf '{"id":1,"argv":["list-sessions"]}\n' | grofe control
```

The bridge sends the control hello itself (with its own `cwd`, `env`, and
`pane` from `$GROFE_PANE`), then copies each non-empty stdin line to the
server as a request and each line the server sends to stdout as it came,
flushed per line. It takes no arguments and does not start a server. At the
end of stdin it leaves once every reply is out, unless a `subscribe` was
accepted: then it streams events until the server closes the connection or
stdout does. A stdin line the server could not take (over 16 MiB, or not
UTF-8) is not sent: the bridge prints an `ok: false` reply with `id` 0 for it
and goes on. `grofe control` is grofe's own verb; invoked as `tmux` it is an
unknown command. It exits 1 if the server closes the connection with a request
unanswered (a refused hello, for one), with the server's line already on
stdout. The bridge does not buffer events: a reader that stalls stalls the
socket, and the server drops events and sends `events-dropped` (section 6).
