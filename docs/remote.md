# Remote sessions

A grofe session on another machine works like a local one. You open it with one command, it
survives dropped connections and sleep without losing anything, it forwards what you need
(clipboard, URLs, files, ports, the ssh agent), it hides latency, and it sits next to your local
sessions in one window with one agent list.

The server on the remote owns every pane's screen; the client on your machine is a thin pipe.
So a reconnect is one full redraw and nothing a pane printed is lost.

## Connecting

```sh
grofe -r devbox                  # attach to session "main" on devbox (made if missing)
grofe -r devbox ls               # any command runs there and prints here
grofe -r devbox agent list
grofe attach ssh://me@devbox:2222/work
```

`-r` takes anything `ssh` takes, including aliases from `~/.ssh/config`; ssh does the
authentication, `ProxyJump` and `ControlMaster`. grofe runs
`ssh -T <host> -- grofe proxy` and speaks its own protocol over that one stream (see
`docs/protocol-proxy.md` in the repository). It adds `ServerAliveInterval=15` and
`ServerAliveCountMax=2` only when your ssh config sets no value of its own.

Without a command, `-r host` means `new-session -A -s main` there. Every other command goes
through a control channel and prints exactly as the local command would, with the same exit
status.

A remote session needs no nested prefix: your terminal still runs exactly one grofe client.

## Installing grofe on the remote

When the remote has no grofe, or one too old to speak the proxy protocol, grofe asks:

```text
grofe is not installed on devbox (linux x86_64). Install v0.3.1 to ~/.local/share/grofe/bin/0.3.1/? [y/N]
```

`--install` answers yes and `--no-install` no; without a terminal the answer is no. The install
downloads the release archive for the remote's system on your machine, checks it against the
release's `SHA256SUMS`, and copies the binary over the same ssh connection into
`~/.local/share/grofe/bin/<version>/`. The copy is checked for its length and hash on the remote
before it is moved into place. It never uses sudo and never touches the remote's config.

- `--install-from <path>` installs a local binary instead, for hosts with no way out to the
  internet. That file is not checksummed: it is yours.
- The option `remote-release-url` (default
  `https://github.com/termage/grofe/releases/download`) points at a mirror. grofe fetches
  `<base>/v<version>/grofe-v<version>-<target>.tar.gz` and `<base>/v<version>/SHA256SUMS`. The
  base may be an https URL, a `file://` URL or a directory. Plain http is refused except on a
  loopback address. A non-default URL is named on stderr when it is used.

Once installed, that versioned binary is used even when an older grofe is on the remote's
`PATH`.

## Dropped connections

The client notices a drop when the stream ends, ssh exits, or two pings in a row (one every
5 seconds) go unanswered. Then:

- the last screen stays up, and the bottom row shows
  `grofe: reconnecting to devbox (attempt 3, next in 4s) · prefix d detach`;
- your typing is dropped, except the detach key (the prefix, then `d`), which ends the client
  cleanly;
- it retries after 0.5 s, 1, 2, 4 and so on, up to 30 s, with some jitter; a link that stays up
  for a minute starts the schedule over; waking from sleep retries at once;
- on reconnect it attaches to the same session again and the server redraws everything,
  including whatever the panes printed meanwhile.

Reconnects never ask for a password (ssh runs with `BatchMode=yes`), so use keys or an agent.
`remote-reconnect off` makes a drop end the client, as for a local server.

## Forwarding

- **Clipboard.** Copies from programs (OSC 52) and from copy mode reach your terminal through
  the screen stream, as they do locally; `set-clipboard` decides which.
- **Notifications** reach your terminal the same way.
- **Opening URLs.** Panes get `BROWSER="grofe open"` while `forward-browser` is on (the
  default), so a program that opens a link on the remote opens it on your machine. You can run
  `grofe open <url>` yourself too. Only `http`, `https` and `file` URLs are opened, and `file`
  only when the path exists on your machine. With no remote client attached, `grofe open` uses
  the remote's own opener.
- **Files.** Drop files into your terminal (a bracketed paste of absolute local paths that all
  exist) and they are uploaded to the pane's directory on the remote, or to
  `~/.cache/grofe/uploads/`, and the remote paths are pasted instead, quoted for the shell.
  `remote-upload-paste` is `ask` (default: `upload 2 files (3.1 MB) to devbox:~/src? [y/N]`),
  `on` or `off`. `remote-upload-max` caps one paste (100 MiB by default). A name that is taken
  gets ` (2)`; nothing is ever overwritten. Directories are pasted as text.
- **ssh agent.** Each session's `SSH_AUTH_SOCK` points at a stable link that every attach
  repoints, so panes that started before a reconnect keep a working agent. For `grofe -r`, the
  agent is the one ssh forwards to the remote (`ssh -A`, or `ForwardAgent yes` in your ssh
  config); without agent forwarding nothing changes. This also works for plain `ssh host` then
  `grofe attach`. `stable-ssh-agent` turns it off.
- **Ports.** When a pane starts listening on a TCP port, the client offers
  `forward devbox:5173 to localhost:5173? [y/N/always]`. The local end binds `127.0.0.1` only,
  on the same port or the next free one. `remote-forward-ports` is `ask` (default), `auto` or
  `off`. A forward closes when the port does or the client exits. `grofe -r devbox ports` lists
  the forwards your clients attached to devbox hold, without connecting. `#{pane_ports}` and `list-ports` show what
  panes listen on.

## Latency

- **Predictive echo.** On a slow link the client draws printable keys at once, underlined,
  before the remote echoes them; the server's echo confirms them and the underline goes. A
  mismatch, a switch to the alternate screen or a second without confirmation repaints from the
  real screen. Enter, control keys and escape sequences are never predicted, and nothing is
  predicted at a password prompt or while the pane has echo off. `predictive-echo` is `auto`
  (default: on only for remote links with a round trip of 50 ms or more), `on` or `off`.
- **Round trip.** `#{client_rtt}` shows the link's smoothed round trip (`42ms`), for the status
  line. The remote client reports it with `set-client-info -t <client> rtt <ms>`; the same verb
  takes `machines <text>` for `#{client_machines}`.
- **Roaming.** `--transport quic` (or the option `remote-transport quic`) moves the session
  onto QUIC over UDP after setting it up over ssh, so it survives a changed address: Wi-Fi to a
  phone's tether, sleep and wake. ssh still does the login and the install. If UDP is blocked
  the client says so and stays on ssh. The remote proxy listens on a port from
  `remote-udp-ports` (default `60001-61000`) and exits after `remote-quic-idle` seconds
  (default 7 days) with no client.

## Many machines in one window

```sh
grofe machine add devbox devbox --session work
grofe machine add gpu me@gpu.lan --transport quic
grofe machine ls
grofe machine rm gpu
```

Saved machines live in `machines.toml` in grofe's config directory (name, target, session,
transport, enabled; never secrets). A saved name works anywhere a host does: `grofe -r devbox`. `machine ls` prints one line per
machine: name, target, session, transport and `on` or `off`, with `-` for an unset field.
`local` is reserved for this machine.

- `grofe --machines`, or bare `grofe` once machines are saved, connects to your local server and
  every enabled machine. Only the machine on screen streams its screen; the others keep a light
  control connection for agent and notification events, so a slow machine never stalls the
  one you are using.
- The multi-machine client does not forward URLs, files or ports yet; use `grofe -r <name>` for
  those.
- `M-m` (`machine-key`) opens the machine picker: one row per machine with its state
  (connected, reconnecting, offline), its sessions and its agents. Enter switches; `M-1` to
  `M-9` jump straight to a row. The session you leave keeps running.
- `#{client_machines}` lists the machines for the status line, marking one with a blocked agent
  or an unread notification.
- `grofe agent list --all-machines` prints every machine's agents with a `machine` column;
  `grofe --machine devbox agent list` asks one. `--machine` takes only saved names (and
  `local`); `-r` takes saved names and ssh targets alike. An offline machine shows its last known agents,
  marked offline.

## Security notes

- Nothing listens on the remote except the opt-in QUIC port. It accepts one client at a time,
  which must hold the certificate hash and the 32-byte random token that were handed over ssh.
  The client pins that certificate (no certificate authority is involved), TLS 1.3 only, with no
  0-RTT. A newer QUIC proxy for the same server stops the older one.
- Port forwards bind `127.0.0.1` on your machine only.
- `open-url` opens only http, https and existing local files; uploads never overwrite.
- The install checks `SHA256SUMS`, which comes from the same place as the archive: it proves the
  download is intact, and a mirror is trusted as much as its host. Release downloads use https.
- Text a remote sends back in errors and prompts is shown with control characters escaped, so a
  remote cannot rewrite your terminal through them.
