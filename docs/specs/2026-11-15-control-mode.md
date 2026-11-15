# grofe: tmux control mode (`-C`, `-CC`)

Status: first slice implemented (WS14 of `2026-11-15-codebase-health.md`). Everything below
marked "recorded" was observed on tmux 3.7b on a private socket (`-L grofe-cc -f /dev/null`);
the rest is from tmux's man page, CONTROL MODE.

## 1. What it is

`tmux -C attach` (and `tmux -C new-session`) turns the client's stdin and stdout into a text
protocol. iTerm2's tmux integration (`-CC`) and editor plugins drive tmux this way. It is the
largest drop-in gap left after the front end (spec D12).

The client sends tmux commands, one line each. The server answers each line with one block and
pushes notifications for everything else that happens. This document is the protocol as tmux 3.7b
speaks it, how grofe maps it onto its client model, and what the first slice covers.

## 2. The line protocol

### 2.1 Blocks

Each input line is one command line (the same grammar as a config file: quotes, `;` chains, `$VAR`).
Its answer is

```
%begin <secs> <num> <flags>
<output lines, possibly none>
%end <secs> <num> <flags>
```

or, with a failure, the error text as the output and `%error` in place of `%end`. `secs` is the
epoch time, `num` a command number that tmux keeps for the whole server (it counts every command
any client queues, so it is not predictable), `flags` is `1` for a command the client sent and `0`
for the attach itself (recorded). A `;` chain is one block; it stops at its first error. Output is
the command's own text, with `\n` between lines (recorded: `list-windows`, `display-message -p`).

A notification never appears inside a block. One that happens while a command runs is held and
written after that block's `%end` (recorded: `new-window -d` gives `%begin`, `%end`, then
`%window-add`). One exception, recorded: `refresh-client -A %0:pause` and `:continue` print
`%pause %0` and `%continue %0` as the command's own output, inside the block.

An empty line is ignored while attached. Under `wait-exit` it ends the wait (section 5).

Errors from the parser read `parse error: <text>` (recorded: `parse error: unknown command: bogus`,
`parse error: syntax error` for a bare `%0:pause`, since `%` starts a directive; the client must
quote it). Errors from a command are its own message (`no sessions`, `can't find window: @9`).

### 2.2 Start and end

On attach tmux writes an empty block first, flags `0`, then `%session-changed $<id> <name>`
(recorded). With `new-session` the block is followed by `%window-add @<id>`, `%sessions-changed`
and `%session-changed` (recorded). A refused attach is a block with the error and flags `0`
(`no sessions`), then `%exit`.

The client leaves with `%exit` alone on a line when its stdin reaches EOF, `detach-client` runs,
or its session is destroyed. With `-CC` tmux writes ST (`ESC \`) after the `%exit` line. A reason
may follow `%exit` (`%exit server exited unexpectedly`); detach and EOF give none (recorded).

### 2.3 `-CC`

`-CC` is `-C` with the terminal made raw (no echo, no line editing) and the whole stream wrapped as
a DCS: the client writes `ESC P 1000 p` before anything else and `ESC \` after `%exit` (recorded).
It needs a tty on stdin: `tcgetattr failed: Inappropriate ioctl for device` otherwise (recorded). `-C` works on
a pipe.

## 3. Notifications

The first slice sends these (the rest of the list is in section 7).

| Notification | When (recorded unless noted) |
|---|---|
| `%output %<pane> <data>` | a pane printed; `data` has bytes below 0x20 and `\` as `\ooo` octal, all else raw (including bytes of 0x80 and up). Chunking follows the pane's reads, so a transcript compares after merging adjacent lines of one pane. |
| `%extended-output %<pane> <age> : <data>` | the same, once `pause-after` is set; `age` is the milliseconds the data waited. |
| `%window-add @<w>` | a window was linked into the attached session. Sent after the creating command's `%end`. |
| `%unlinked-window-add @<w>` | a window of another session was created. |
| `%window-close @<w>` | not seen from the commands tried: tmux sends it only while the window is still linked in the client's session at notify time. |
| `%unlinked-window-close @<w>` | a window was killed, whichever session it was in (`kill-window`, `kill-pane` of its last pane). |
| `%window-renamed @<w> <name>` | a window of the attached session was renamed, by hand or automatically. |
| `%unlinked-window-renamed @<w> <name>` | the same for another session's window. |
| `%layout-change @<w> <layout> <visible-layout> <flags>` | a window's layout or size changed: after `split-window`, `kill-pane`, `select-layout`, `resize-window`, `refresh-client -C`. `flags` is the window's `#{window_flags}` (`*` for the current window). |
| `%window-pane-changed @<w> %<p>` | a window's active pane changed (not for `split-window -d`). |
| `%session-window-changed $<s> @<w>` | a session's current window changed. For `new-window` it comes before `%window-add`; for `kill-window` before `%unlinked-window-close`. |
| `%session-changed $<s> <name>` | the client's session changed (attach, `switch-client`). |
| `%session-renamed $<s> <name>` | the attached session was renamed (3.7b names the id). |
| `%sessions-changed` | a session was created or destroyed (also after `%unlinked-window-add` for a new session). |
| `%pause %<p>`, `%continue %<p>` | flow control (section 5). |
| `%exit` | the client is leaving. |

The order of several notifications from one command is tmux's, not alphabetical, and only the
sequences in section 8's parity script are promised.

## 4. How grofe maps it

grofe's attach client is a binary frame stream carrying a terminal. A control client carries
lines, so it is a third connection kind, and grofe's server keeps its one JSON control protocol
(spec section 4.6) untouched.

**First slice: a client-side bridge.** `tmux -C attach` in the tmux front end runs
`control_mode::run` in the CLI process. It holds

- a command connection per command line: the JSON control hello of the front end (dialect `tmux`)
  with `pane` set to the attached session's active pane, so a command with no target resolves in the
  attached session;
- one event connection that sends `subscribe pane-output`, `subscribe` for the rest, and wakes the
  bridge on any event;
- a snapshot of `list-sessions` and `list-windows -a` (one format each). After a command finishes
  and on every event, the bridge takes a new snapshot and writes the notifications that differ from
  the last (section 3's order). It also takes one every 250 ms, for changes no event announces
  (an automatic rename).

`%output` comes straight from `pane-output` events, base64-decoded and escaped. The bridge is its
own flow controller: `pause-after`, `-A`, and `no-output` drop or hold data before it is written.

Client-bound commands are answered in the bridge: `detach-client` and `switch-client -t` (the
attached session), `refresh-client` (`-C` as `resize-window` on the attached session's windows,
`-f`, `-A`), and `display-message -p` formats naming `client_*` are left to the server.

**Ceilings of this slice**, each marked `// ponytail:` in the code:

- the control client is not a server client: `list-clients`, `#{client_name}` and `client-attached`
  hooks do not see it, and window sizing does not take its size into account except through
  `resize-window`;
- state changes are found by diffing, so two changes between snapshots merge, and
  `%window-close` (the still-linked case) is never sent;
- `%subscription-changed` (`refresh-client -B`), `%pane-mode-changed`, `%paste-buffer-*`,
  `%message`, `%config-error`, `%client-*` and `refresh-client -r` are not sent (section 7);
- `window_visible_layout` is not a format grofe has, so the visible layout is the layout (the
  two differ only for a zoomed window).

A later slice moves this into the server as a client kind (`Hello::Control` plus a `-C` flag),
where the snapshot diff becomes direct emission at the places the model changes.

## 5. Flow control

`refresh-client -f pause-after=<s>` (or `attach -f`) switches `%output` to `%extended-output`
(recorded). tmux pauses a pane for a client once its data is `<s>` seconds old and tells it with
`%pause %<p>`; `refresh-client -A %<p>:continue` resumes with `%continue %<p>`. Data produced
while paused is discarded for that client (recorded). `refresh-client -A %<p>:off` stops sending
a pane's output (what was produced is sent on `:on`, recorded), `:pause` and `:continue` do it by hand.
`-f no-output` drops all output while set, and `-f '!no-output'` resumes it (recorded: the output of
the `no-output` span is lost). `-f wait-exit` makes the client wait for an empty input line before
`%exit`.

The bridge's pause is by age: a pane whose pending data has waited `<s>` seconds because stdout
would not take it (a write that blocks, tracked by a writer thread's queue age) is paused.

## 6. Sizing

`refresh-client -C <w>x<h>` sets the client size, which sets the size of the session's windows
(recorded: `%layout-change @0 a87d,100x30,...`). `refresh-client -C @<w>:<w>x<h>` sets one window.
The bridge turns both into `resize-window -x -y`.

## 7. Out of the first slice

`%subscription-changed`, `%pane-mode-changed`, `%paste-buffer-changed` and `-deleted`, `%message`,
`%config-error`, `%client-detached`, `%client-session-changed`, `refresh-client -r`, `-l`, `-S`
and `-B`, `-CC` pty details (the DCS is written, the raw mode is set when stdin is a tty),
`attach -f` flags beyond those above, `-C` with `new-session -A`. iTerm2 end to end is checked by hand
and recorded in `docs/compat-coverage.md`.

## 8. Parity test

`tests/control_mode.rs` runs one script against real tmux 3.7b (`GROFE_REAL_TMUX`, default
`/opt/nanobrew/prefix/bin/tmux`, always with `-L` and `-f /dev/null`; the test skips when it is
missing) and against grofe through a `tmux` link, and compares the transcripts after:

- `%begin`/`%end`/`%error` times and numbers replaced by `T N`, and the flags kept;
- `$N`, `@N`, `%N` ids kept (both servers number from 0 in a fresh server) but the layout checksum
  replaced;
- adjacent `%output` lines of one pane merged;
- window names that tmux sets from the process (automatic rename) fixed by running panes under `-n`.

The script: `list-windows -F`, `new-window -d -n`, `split-window -d -h`, `select-window`,
`rename-window`, `send-keys` with output, an unknown command, `new-session -d`, `switch-client`,
`kill-window`, `kill-session`, EOF.
