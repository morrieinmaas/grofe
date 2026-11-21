# grofe: tmux control mode (`-C`, `-CC`)

Status: implemented as a client-side bridge (WS14 of `2026-11-15-codebase-health.md`, then
issue #5): every control notification and `refresh-client -B`. Everything below
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
for the attach itself (recorded). Each command of a `;` chain is its own block, and the chain
stops at its first error (recorded: `display -p a ; kill-window -t @99 ; display -p c` gives
an `a` block and an error block); the notifications of the chain follow its last block. Output is
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

All of tmux 3.7b's control notifications (control-notify.c, plus `%message`, `%config-error`
and `%subscription-changed`) are sent; section 10 lists where the bridge's timing or wording
differs.

| Notification | When (recorded unless noted) |
|---|---|
| `%output %<pane> <data>` | a pane printed; `data` has bytes below 0x20 and `\` as `\ooo` octal, all else raw (including bytes of 0x80 and up). Chunking follows the pane's reads, so a transcript compares after merging adjacent lines of one pane. |
| `%extended-output %<pane> <age> : <data>` | the same, once `pause-after` is set; `age` is the milliseconds the data waited. |
| `%window-add @<w>` | a window was linked into the attached session (`new-window`, `link-window`), or a window of the attached session was linked into another one. Sent after the creating command's `%end`. |
| `%unlinked-window-add @<w>` | a window outside the attached session was created or linked. |
| `%window-close @<w>` | a link of a window was removed while the window stays in the attached session (`unlink-window` of its link in another session). |
| `%unlinked-window-close @<w>` | a link was removed and the window is not in the attached session after it: `kill-window`, `kill-pane` of its last pane, `unlink-window` from the attached session, and each window of a killed session (after `%sessions-changed`). One per link: a window linked into two sessions and killed gives two. |
| `%window-renamed @<w> <name>` | a window of the attached session was renamed, by hand or automatically. |
| `%unlinked-window-renamed @<w> <name>` | the same for another session's window. |
| `%layout-change @<w> <layout> <visible-layout> <flags>` | a window's layout or size changed: after `split-window`, `kill-pane`, `select-layout`, `resize-window`, `refresh-client -C`. `flags` is the window's `#{window_flags}` (`*` for the current window). |
| `%window-pane-changed @<w> %<p>` | a window's active pane changed (not for `split-window -d`). |
| `%pane-mode-changed %<p>` | a pane entered or left a mode (`copy-mode`, `send-keys -X cancel`). |
| `%session-window-changed $<s> @<w>` | a session's current window changed. For `new-window` it comes before `%window-add`; for `link-window` after it; for `kill-window` before `%unlinked-window-close`. |
| `%session-changed $<s> <name>` | the client's session changed (attach, `switch-client`). |
| `%client-session-changed <client> $<s> <name>` | another client's session changed (`switch-client -c`). |
| `%client-detached <client>` | another client detached (`detach-client -s`). |
| `%session-renamed $<s> <name>` | any session was renamed (3.7b names the id). |
| `%sessions-changed` | a session was created or destroyed (also after `%unlinked-window-add` for a new session). |
| `%paste-buffer-changed <name>` | a paste buffer was set. Setting a buffer that exists frees it first: `%paste-buffer-deleted` then `%paste-buffer-changed`. |
| `%paste-buffer-deleted <name>` | a paste buffer was deleted. |
| `%message <text>` | `display-message` without `-p` from this client: printed inside the command's block, as its output. |
| `%config-error <file>:<line>: <error>` | `source-file` of a file with errors. The command's block ends `%end`; the errors follow it. |
| `%subscription-changed ...` | a subscription's value changed (section 7). |
| `%pause %<p>`, `%continue %<p>` | flow control (section 5). |
| `%exit` | the client is leaving. |

The order of several notifications from one command is tmux's, not alphabetical, and only the
sequences in section 8's cases are promised.

## 4. How grofe maps it

grofe's attach client is a binary frame stream carrying a terminal. A control client carries
lines, so it is a third connection kind, and grofe's server keeps its one JSON control protocol
(spec section 4.6) untouched.

**A client-side bridge.** `tmux -C attach` in the tmux front end runs `control_mode::run` in
the CLI process (`src/control_mode/`). It holds

- a command connection per command line: the JSON control hello of the front end (dialect `tmux`)
  with `pane` set to the attached session's active pane, so a command with no target resolves in the
  attached session;
- one event connection that sends `subscribe pane-output`, `subscribe` for the rest, and wakes the
  bridge on any event;
- a snapshot of `list-sessions`, `list-windows -a` (with `#{window_visible_layout}`),
  `list-panes -a` (with `#{pane_mode}`), `list-clients` and `list-buffers` (one format each).
  After a command finishes and on every event, the bridge takes a new snapshot and writes the
  notifications that differ from the last (section 3's order). It also takes one every 250 ms,
  for changes no event announces (an automatic rename, a buffer set by a key).

`%output` comes straight from `pane-output` events, base64-decoded and escaped. The bridge is its
own flow controller: `pause-after`, `-A`, and `no-output` drop or hold data before it is written.

Client-bound commands are answered in the bridge: `detach-client` with no `-a`, `-s` or `-t`,
`switch-client -t` (the attached session) and `refresh-client` (`-C` as `resize-window` on the
attached session's windows, `-f`, `-A`, `-B`). `display-message` without `-p` runs with it and
prints `%message`; `source-file` errors of the `<file>:<line>:` form become `%config-error`
lines after a successful block. `display-message -p` formats naming `client_*` go to the
server, which has no such client.

**Ceilings of the bridge**, each marked `// ponytail:` in the code:

- the control client is not a server client: `list-clients`, `#{client_name}` and `client-attached`
  hooks do not see it, and window sizing does not take its size into account except through
  `resize-window`;
- state changes are found by diffing, so two changes between snapshots merge, and notices of
  one command come in section 3's fixed order rather than the order the command made them;
- `refresh-client -r` is refused.

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

The bridge writes `%extended-output` with the age of an event when it was received, and pauses
a pane itself, as tmux does, when output reaches it `<s>` seconds old (a consumer that does
not read): it writes `%pause %<p>` and drops that pane's output until `:continue`.

## 6. Sizing

`refresh-client -C <w>x<h>` sets the client size, which sets the size of the session's windows
(recorded: `%layout-change @0 a87d,100x30,...`). `refresh-client -C @<w>:<w>x<h>` sets one window.
The bridge turns both into `resize-window -x -y`.

## 7. Subscriptions

`refresh-client -B <name>:<what>:<format>` (recorded, and read from tmux's control.c) adds a
subscription, replacing one of the same name; `-B <name>` alone removes it, and a value with
one `:` is ignored. `<what>` is `%<n>` (one pane), `%*` (every pane of the session), `@<n>` (one
window), `@*` (every window), and anything else the session (so `%x` watches the session). `-B`
may be given more than once.

Once a second, starting a second after the first subscription, each subscription's format is
expanded for every row it covers, and a value that differs from the last one written for that
row is written:

```
%subscription-changed <name> $<s> - - - : <value>              (session)
%subscription-changed <name> $<s> @<w> <index> %<p> : <value>  (pane)
%subscription-changed <name> $<s> @<w> <index> - : <value>     (window)
```

In tmux's order: session subscriptions, then single panes and windows, by name; then `%*` and
`@*`, row by row across their subscriptions. Removing a subscription writes nothing. A pane or
window outside the attached session is not reported.

The bridge expands the formats on the server: `display-message -p -t $<s>:` for the session,
`list-panes -s -t $<s>` and `list-windows -t $<s>` for the rest, and checks on its 250 ms
tick once a second is up.

## 8. Parity test

`tests/control_mode/` runs each case of a table against grofe through a `tmux` link and compares
the transcript with the case's golden, `tests/fixtures/compat/golden/control_mode/<case>.golden`,
after:

- `%begin`/`%end`/`%error` times and numbers replaced by `T N`, and the flags kept;
- `$N`, `@N`, `%N` ids kept (both servers number from 0 in a fresh server);
- adjacent `%output` lines of one pane merged, and `%extended-output` ages dropped;
- the fixtures directory written `{fixtures}`, and the pty client's name `CLIENT`;
- window names that tmux sets from the process (automatic rename) fixed by running panes under `-n`.

The cases: `commands` (`list-windows -F`, `new-window -d -n`, `split-window -d -h`,
`select-window`, `rename-window`, `send-keys` with output, an unknown command, `new-session -d`,
`switch-client`, `kill-window`, `kill-session`, EOF), `flow` (panes, `-C`, `-f`, `-A`),
`new-session`, `no-target`, `links` (`link-window` and `unlink-window` both ways,
`rename-session` and `kill-session` of another session), `notices` (modes, buffers,
`display-message`, `source-file` with an error), `subscriptions` (every kind, a change, removal,
a bad `what`) and `clients` (a pty client switched and detached by the control client).

`scripts/record-compat-goldens.sh control_mode` records the goldens: it runs the cases from the
host (`record_control_goldens`, ignored by default) with `tmux` a wrapper that runs each
command in a container of the compat image, so the tmux is that image's 3.7b, on a private
`-L` socket with `-f /dev/null`.

## 9. Not done

`refresh-client -r`, `-l` and `-S`, `switch-client -n/-p/-l`, `-C` with `new-session -A`, and
any command but `attach-session` and `new-session` after `-C`. iTerm2 has not been checked end
to end.

## 10. Divergences

Where the bridge, seeing the server only through snapshots and its own commands, cannot match
tmux:

- **Notice order inside one command.** The diff writes a command's notices in section 3's fixed
  order. tmux interleaves them as the command runs: `kill-window` of a window linked into two
  sessions gives `%unlinked-window-close`, `%session-window-changed`, `%unlinked-window-close`;
  the bridge writes the `%session-window-changed` first.
- **Changes that leave no trace.** A buffer set again to the same content, a mode left and
  entered again, or a client that detaches and reattaches between two snapshots sends nothing.
  A window linked twice into one session counts once.
- **`source-file` blocks.** tmux gives a control client one more block after `source-file`'s
  own for each command in the file (with that command's output); grofe runs the file on the
  server and answers with one block.
- **`%config-error` at start.** tmux reports errors of the config it loads at server start to
  the first control client; grofe's server reads its config before any client, and the bridge
  never sees those errors.
- **Subscriptions see no client.** A format naming `client_*` expands without the control
  client (it is not a server client), and the check runs on the bridge's 250 ms tick, so a value
  may show up to a quarter second later than tmux's one-second timer.
- **`%client-*` notices** come from diffing `list-clients`, so a client that is not a server
  client (another control bridge) is never named.
