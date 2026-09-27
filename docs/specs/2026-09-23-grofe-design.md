# grofe: design

**Date:** 2026-09-23
**Status:** draft, revised after three review passes (architecture, execution, requirements)
**Origin:** the `grofe-plan.md` design note (formerly `rtmux-plan.md`); canopy
(`morrieinmaas/canopy`) is the requirements document.

---

## 1. What this is

grofe is a tmux-shaped terminal multiplexer written in Rust. A server holds
sessions, windows and panes; clients attach and detach over a Unix socket.

**Familiar on the surface, correct underneath.** Someone with tmux muscle memory
should be productive in five minutes: same object model, same prefix-key habits,
same CLI verbs and target syntax. Underneath, the things tmux got wrong for
twenty years are fixed: a typed config with a schema and provenance, no
subprocesses in the status line, pane identity that survives respawn and reboot,
and a plugin surface built on a real event stream.

**The real case, in priority order:**

1. **Native persistence and pane identity.** "A pane has a durable identity and a
   command that resumes it" is a struct field, not a save-command strategy patched
   into a vendored plugin. This is the strongest argument for the project.
2. **Config semantics.** TOML with a schema, layering, and provenance ("what did
   this resolve to, and which file said so"). Lua for the escape hatch.
3. **Plugin semantics.** Events and a scriptable control surface, designed after
   dogfooding, from what actually hurt.

**Not the case:** performance. tmux's redraw is already fast; what is slow in
practice is per-redraw subprocesses in status lines, which grofe bans by design.
If performance becomes the headline, the project has lost the plot.

**What grofe is not:**

- Not a tmux drop-in. It does not read `tmux.conf` at runtime or run tmux
  plugins. A one-shot converter, `grofe import-tmux` (M3.5, §5.5), turns the
  common subset of a `tmux.conf` into grofe TOML.
- Not Zellij. No WASM plugin runtime, no modal UI by default, tmux semantics.
- Not cross-platform beyond Unix. macOS and Linux only.

---

## 2. Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | **tmux-familiar, not tmux-compatible.** Match tmux's object model, default key bindings, CLI verb names, common flags, target syntax and a subset of format strings (§4.5). Do not read `tmux.conf` at runtime, run tmux plugins, or speak tmux's `-CC` wire format in v1. `grofe import-tmux` (M3.5, §5.5) converts the common subset of a `tmux.conf` once, to TOML the user then owns. | Muscle memory and scripts transfer; tmux's config language and plugin semantics are exactly what we are escaping. A one-shot converter lowers the switching cost without making tmux's language part of grofe. `-CC` (iTerm2 integration) is a possible M8, not a v1 promise. |
| D2 | **One binary, `grofe`.** It is the server, the attach client, and the CLI. Only session-creating commands auto-start a server (§4.1); every other command fails fast when none is running. | One thing to install and version. Agent hooks call `set` constantly, often from IDEs outside grofe, and must never spawn a stray server. |
| D3 | **The server renders; the client is thin.** The attach client puts the tty in raw mode, probes the outer terminal's capabilities once, forwards input bytes and resize events, and writes whatever bytes the server sends. All state and all composition live in the server. | Client crashes are survivable by construction. Same split as tmux. |
| D4 | **Headless control mode is the first interface.** Every operation is a command; the CLI, key bindings, control mode and (later) plugins all go through one command dispatcher. | Tests and canopy's bats suite can drive grofe before any UI exists. A scriptable surface is what makes the plugin API designable later. |
| D5 | **Single-owner state.** One tokio task (the server actor) owns the model. Pane I/O tasks, client connections and timers send it messages. The only shared state is each pane's terminal behind a mutex, with bounded lock holds (§4.8). | Removes a class of concurrency bugs; easy for parallel implementers to reason about. |
| D6 | **Window size follows the latest active client**, where "latest" means last input, debounced ~500 ms so two attached clients do not make the window flap. Larger clients are letterboxed (window drawn at its size, the rest filled with a dim pattern). Smaller clients are cropped, with a viewport that follows the active pane's cursor. The emulator reflows the primary screen on resize; alt-screen apps just get SIGWINCH. | Every multiplexer picks a side. tmux users already know this behavior. |
| D7 | **Advertise a small, honest `TERM`.** Default `screen-256color` plus `COLORTERM=truecolor`. T0.4 checked whether every extra capability `tmux-256color`'s terminfo entry adds over `screen-256color`'s (italics, strikethrough, hidden text, the `Smulx` extended underline styles, OSC 52 clipboard, focus reporting, cursor colour, title relay) survives the M0 spike's render path end to end. It does not: the spike's renderer collapses every underline style to plain underline, and several non-SGR extras are not forwarded to the outer terminal at all yet. So the default stays `screen-256color`, falling back to `xterm-256color` per host when even that entry is missing. That last resort advertises more than `screen-256color` and exists only for hosts lacking both `screen-256color` and `tmux-256color`. Revisit once the M2 renderer (T2.2) emits underline styles and M3 passthrough (T3.3) forwards title, OSC 52, cursor colour and focus; T3.6 re-runs the check. | Whatever is advertised, apps will use. No custom terminfo entry: it breaks over ssh. |
| D8 | **No subprocesses in the status line, ever.** The status line renders formats over options. Dynamic content arrives by something external setting an option. `run-shell` exists for key bindings only. | The one real performance problem in tmux setups, deleted by design. canopy measured it: folding its status into one aggregator and removing 20 forks took a render from ~97 ms to ~67 ms (canopy 6ef854a). |
| D9 | **Build the plugin API last** (M6). Before that, the control protocol and events exist for tests and dogfooding, may change, and grow only when a test or feature needs them. | Needed events beat guessed events. |
| D10 | **Generic primitives in core, agent semantics in a plugin.** Core provides pane identity, a resume command, pane options, events, persistence, and config introspection. The five agent states, adapters and rollups live in a first-party `agents` plugin (M7). | Keeps core general, and gives the plugin API a real, demanding first user. |
| D11 | **Ghostty and WezTerm are the first-class outer terminals.** grofe is tested and tuned against both: kitty keyboard, synchronized output, OSC 52 and kitty graphics placeholders (§4.9). Other terminals work with legacy key encoding and whatever features the capability probe (§4.4) finds; missing features degrade, never break. WezTerm speaks kitty keys only with `enable_kitty_keyboard = true` in the user's WezTerm config, and grofe's docs must say so. | Two modern terminals cover the owner's machines and most of the target audience. Naming them keeps the manual checks and the probe fixtures concrete. |

**Publishability rule (inherited from athome and canopy):** no identities,
hostnames, or machine specifics in shipped files.

---

## 3. Open questions from the design note, answered

1. **What rules Zellij out?** Owner preference, tmux semantics and muscle memory,
   and a lighter plugin story than WASM. Zellij is still worth reading; the files
   are listed in §8.
2. **Is Rust the constraint?** No, but it is the right tool. A config compiler
   targeting tmux's command stream (canopy's direction) fixes config semantics but
   cannot deliver native pane identity or persistence without continuum's
   save-file format. Rust gives a single static binary and the emulation libraries
   that make the project credible.
3. **Compatibility fork.** D1: familiar surface, own internals.
4. **Honest first-milestone date.** Dogfood start (M3) in about seven weeks, with
   tmux kept as the fallback. Persistence (M4) follows. See the plan's critical path.

---

## 4. Architecture

### 4.1 Process model and lifecycle

```
grofe (CLI) ──one-shot control──┐
grofe attach ──attach stream────┤
grofe control ──JSON lines──────┼──> Unix socket ──> grofe server
plugins (M6) ──JSON lines───────┘                     │
                                                      ├─ server actor (owns model)
                                                      ├─ per-pane reader + writer tasks
                                                      ├─ per-client reader + writer tasks
                                                      └─ render scheduler
```

**Socket path:** `$XDG_RUNTIME_DIR/grofe/<name>.sock`, falling back to
`$TMPDIR/grofe-$UID/<name>.sock` (macOS). Directory mode `0700`. `<name>` defaults
to `default`; `-L <name>` and `-S <path>` select another, as in tmux. Tests always
use a unique `-S`.

**Auto-start:** only session-creating commands (`new-session`, `start-server`,
and `attach` with create semantics) start a server. Every other command that
finds no server exits nonzero with `no server running on <socket>` and starts
nothing.

**Server start:** never fork a process that already runs tokio. A CLI that finds
no server takes `flock` on `<sock>.lock`, re-checks, and if the socket is absent or
refuses connection (`ECONNREFUSED`, meaning stale) it removes it and re-execs
itself as `grofe __server -S <sock>`, detached (`setsid`, stdio to a log under
`$XDG_STATE_HOME/grofe/`). The daemon closes every inherited fd above 2, not
only 0/1/2: a leaked pipe keeps the caller's reader waiting for an EOF that
never comes, which made canopy's test runs hang instead of fail (canopy
8070a22, 2907e89). The server builds its runtime only after that, binds,
and signals readiness over an inherited pipe; the CLI then releases the lock and
connects. Two concurrent CLIs therefore start exactly one server. State paths
(snapshot, log) derive from the socket, so a server on a test `-S` socket never
touches the user's real state.

**Server exit:** when the last session closes, unless configured to stay.

### 4.2 Object model

```
Server
  sessions: Session { id: $N, name, windows (ordered), active window, env, options }
  Window    { id: @N, index, name, layout: LayoutTree, active pane, zoomed, options }
  Pane      { id: %N, uid, spawn argv, cwd, resume: Option<argv>, dead: Option<exit status>,
              term (behind mutex), options }
  clients:  Client { id, size, caps, cwd, env, attached session, key table state, last frame }
```

- `%N`, `@N`, `$N` are **runtime ids**, as in tmux, reused across server lifetimes.
- `uid` is a **durable pane identity** (ULID). It survives `respawn-pane`, and
  from M4 survives server restart and reboot.
- `resume` is the argv that brings this pane's process back. Unset means "start a
  shell in `cwd`".
- **A pane is dead** only when its child has exited **and** the master has drained
  to EOF/EIO (background jobs can hold the slave open; output must not be lost).
  With `remain-on-exit = false` (default) a dead pane is removed; with `true` it
  stays, shows its exit status, and can be revived by `respawn-pane` with the same
  `uid`. Tests run with `remain-on-exit = true`.
- **Options** are scoped (global, session, window, pane). Built-in options are
  typed by the config schema. User options are prefixed `@` and are strings.
  A user option set with `set -p --persist` is included in the M4 snapshot.
  Setting an option to its current value is a no-op and emits no
  `option-changed` event, so hooks that re-set state on every tool call cost
  nothing downstream.
- **Environment:** a session holds an environment captured from the client that
  created it. On attach, variables named by `update-environment` (default
  `SSH_AUTH_SOCK SSH_CONNECTION DISPLAY WAYLAND_DISPLAY TERM_PROGRAM
  TERM_PROGRAM_VERSION`) are refreshed from the attaching client, as in tmux.
  `TERM_PROGRAM` passes through so apps can detect the outer terminal; `TERM`
  itself stays the honest inner value (D7). Panes get a UTF-8 locale forced if none is set,
  plus `TERM`, `COLORTERM`, `GROFE` (socket path) and `GROFE_PANE` (`%N`).

### 4.3 Layout

One algorithm: tmux's n-ary split tree. A window's layout is a tree of cells;
each internal cell splits its area horizontally or vertically among children;
leaves are panes. Splitting a pane in the direction its parent already splits
adds a sibling (three `split -h` give three columns). Supports zoom and resize by
cells or percent. Borders are one cell wide, shared between neighbours, active
pane border highlighted. Layout presets `select-layout main-vertical`,
`even-horizontal` and `even-vertical` [planned, M3]: dogfooding already asks for
them (canopy's cockpit binding uses `main-vertical`). Other presets wait for a
reason.

### 4.4 Rendering

- **Per-client capabilities.** Before its hello the client probes the outer
  terminal once (DA1, XTVERSION, DECRQM for synchronized output 2026, kitty
  keyboard query, kitty graphics query) and sends a caps struct: truecolor or
  256, styled underline, synchronized output, kitty keys, kitty graphics. The
  renderer downgrades colours and styles per client. The probe's answers are
  tuned against Ghostty and WezTerm (D11); anything else gets what it answers.
- **Compose and diff.** Per client the server keeps the last frame (a cell
  buffer). On change it snapshots the visible rows of each affected pane under
  that pane's lock, releases the lock, composes the next frame (pane rows clipped
  into layout rects, borders, status line, overlays), diffs against the last frame,
  and emits the minimal escape stream. Frames are wrapped in synchronized-output
  begin/end when the client supports it.
- **Wide characters.** Inner and outer width tables disagree (emoji, VS16,
  combining marks), so the renderer emits an absolute cursor move after any wide
  or zero-width cell and at each row start.
- **Scheduling.** Pane readers set a per-pane dirty flag and send one wakeup; the
  scheduler composes at most one frame per ~16 ms per client.
- **Slow clients.** Each client has a writer task holding at most one pending
  frame. If the client lags, pending diffs are dropped and the client is marked
  "needs full redraw"; the next frame is a full redraw against current state. A
  slow ssh client never stalls the actor or other clients.
- The cursor goes to the active pane's cursor, with its shape and visibility.

**Round-trip testing trick:** the renderer's output is fed into a fresh emulator
and the resulting screen is snapshot-tested.

### 4.5 Commands, targets, formats

Every operation is a command named and shaped like its tmux counterpart. One
parser turns argv into a typed `Command`; one dispatcher executes it. The CLI,
key bindings, control mode and plugins all produce argv.

| Command | Milestone |
|---|---|
| `new-session` (`-d -s -c -x -y`, command), `kill-session`, `has-session`, `list-sessions` | M1 |
| `split-window` (`-h -v -c -p -l -t`), `kill-pane`, `select-pane` (`-L -R -U -D -t`), `resize-pane` (`-L -R -U -D -Z`, with a cell count: `-L 5`), `respawn-pane` (`-k`), `list-panes` | M1 |
| `new-window`, `kill-window`, `select-window`, `list-windows` | M1 |
| `send-keys` (`-l -H`, key names), `capture-pane` (`-p -S -E`) | M1 |
| `set-option`/`set` (`-g -s -w -p -u`, `--persist`), `show-options`/`show`, `display-message -p` | M1 |
| `kill-server`, `subscribe` (control mode) | M1 |
| `attach-session`/`attach`, `detach-client`, `next-window`, `previous-window`, `last-window`, `rename-session`, `rename-window`, `switch-client` (`-l -p -n`), `send-prefix`, `list-clients`, `capture-pane -e` | M2 |
| `bind-key`, `unbind-key`, `list-keys`, `source-file`, `reload`, `show-config`, `command-prompt`, `run-shell` (bindings only, D8), `select-layout` (§4.3), `;` command sequences | M3 |
| `import-tmux` (grofe's own, not a tmux verb; runs without a server, §5.5) | M3.5 |
| `pane-set-resume`, `reboot-check`, `start-server --restore` | M4 |
| `display-popup -E`, `display-menu` (needed by canopy's palette and which-key) | M5 |

**Targets (supported forms):** `$N`, `@N`, `%N`; `session`, `session:window`,
`session:window.pane`, with window by index or name and pane by index; exact
session name, then unique prefix; empty or omitted means the client's current
(from the request context, §4.6). Not supported in v1: fnmatch patterns,
`{last}`/`{next}` tokens, `!`, `+`/`-` offsets.

**Formats:** M1 supports `#{name}` and `#{?cond,then,else}`; M2 adds
`#{==:a,b}`, the basename modifier `#{b:var}`, nesting and escaping when the
status line needs them. Formats in
flag values (`-c '#{pane_current_path}'`) are expanded at **execution** time
against the target's context, not at parse time. No `#()` substitution (D8).
Variables: `session_name`, `session_id`, `window_index`, `window_name`,
`window_id`, `window_active`, `window_zoomed_flag`, `pane_id`, `pane_uid`,
`pane_index`, `pane_active`, `pane_pid`, `pane_current_command`,
`pane_current_argv` (§4.8), `pane_current_path`, `pane_width`, `pane_height`,
`pane_dead`, `client_width`, `client_height`, `host_short`, and
`#{@user_option}`.

**Window names:** `automatic-rename` is on by default and names a window
`#{b:pane_current_path}` [planned, M2]. An OSC title from a pane is relayed
(§4.9) but never freezes the window name (tmux treats it as a manual
rename, and the name then stops following the cwd). Only `rename-window` stops
automatic renaming.

**`send-keys`:** as tmux: each argument that matches a key name (`Enter`, `C-c`,
`M-x`, `Up`, `F1`...) is sent as that key, otherwise as literal text; `-l` forces
literal, `-H` takes hex bytes.

### 4.6 Protocol

One socket, two modes, chosen by the first line the client sends. Both hellos
carry the client's context so `Current` targets, default cwd and environment
resolve correctly for a CLI call made from inside a pane:

```json
{"grofe": 1, "mode": "control", "cwd": "/home/u/src", "pane": "%3", "env": {"SSH_AUTH_SOCK": "..."}}
{"grofe": 1, "mode": "attach", "cwd": "...", "pane": null, "env": {...},
 "cols": 200, "rows": 50, "term": "xterm-ghostty", "caps": {"truecolor": true, "sync": true, "kitty_keys": true, "kitty_graphics": true}, "session": "main"}
```

`pane` is `$GROFE_PANE` when set. Attaching from inside a grofe pane is refused
unless `-f` is given.

**Control mode:** newline-delimited JSON both ways.

```json
→ {"id": 7, "argv": ["split-window", "-h", "-t", "%3"]}
← {"id": 7, "ok": true, "out": "%4\n"}
← {"id": 8, "ok": false, "err": "can't find pane: %99"}
→ {"id": 9, "argv": ["subscribe", "pane-exited"]}
← {"event": "pane-exited", "pane": "%4", "uid": "01J...", "status": 0}
```

The one-shot CLI is a control client that sends one command and exits with its
status. Subscribers have a bounded queue; on overflow events are dropped and an
`events-dropped` notice is sent.

**Attach mode:** after the JSON hello, binary frames `[u8 kind][u32 BE len][payload]`.

| Direction | Kind | Payload |
|---|---|---|
| client → server | `input` | raw bytes from the tty |
| client → server | `resize` | `u16 cols, u16 rows` |
| client → server | `detach` | none |
| server → client | `output` | bytes to write to the tty |
| server → client | `exit` | UTF-8 reason (`detached`, `server exited`, `session killed`) |

Protocol version mismatch is a clean error naming the side to upgrade. Until M6
the protocol is internal and may change between commits.

**Events:** M1 ships `pane-exited` and `pane-output` (base64, for tests). Others
are added when a test, the status line, or a plugin needs them. Candidates:
`session-created/closed/renamed`, `window-created/closed/renamed`,
`layout-changed`, `pane-created`, `pane-focus`, `option-changed`,
`client-attached/detached`, `config-reloaded`, `bell`.

### 4.7 Input

- **Outer mode.** The client asks the outer terminal for the richest key
  reporting it supports (kitty keyboard flags, else legacy with modifyOtherKeys),
  so the server receives keys worth routing.
- **Routing.** The server decodes client input into keys and mouse reports, runs
  keys through the client's current key table (`root`, `prefix`, `copy-mode`,
  custom), and either dispatches a bound command or encodes the key for the active
  pane from that pane's `TermMode` (application cursor, kitty flags the pane has
  pushed, mouse mode, focus events).
- **Paste.** Bracketed-paste content is an opaque blob: never matched against
  bindings, forwarded with markers to panes in mode 2004 and without them otherwise.
- **Escape time.** `escape-time-ms = 25` default: low enough to feel instant, high
  enough to survive ssh. Irrelevant when the outer terminal speaks kitty keys.
- Prefix defaults to `C-b`.

### 4.8 Pane I/O and processes

- **Spawning.** `std::process::Command` with `pre_exec` doing only
  async-signal-safe calls (`setsid`, `ioctl(TIOCSCTTY)`, `dup2` of the slave to
  0/1/2). The master and slave get `FD_CLOEXEC` right after `openpty` so no child
  inherits another pane's master (leaked masters stop `kill-pane` from hanging
  up). Setting it is not atomic with `openpty`, so spawns are serialised under
  one process-wide lock from openpty to spawn so a concurrent fork cannot
  inherit a master. std resets
  SIGPIPE and the signal mask for the child. Exit is observed via the child handle,
  not a global SIGCHLD handler.
- **Reader task** (one per pane) reads the master via `AsyncFd`, and owns the
  emulator's `EventListener`. It parses at most ~16 KiB per lock hold, buffering
  listener events while locked. After unlocking it writes `PtyWrite` replies (DA,
  DSR/CPR, colour queries, size reports) straight to the pane's writer queue in
  order, never through the actor. Other listener events become `PaneEvent`s to the
  actor: `Title`, `Bell`, `Clipboard` (OSC 52 store), `ColorRequest` (answered from
  the configured palette). It also drives the parser's
  synchronized-update timeout so a pane that begins an update and dies cannot
  freeze. It sets the dirty flag and wakes the render scheduler.
- **Writer task** (one per pane) owns writes to the master through a bounded
  queue. The actor only enqueues, so a large paste into a pane that is not reading
  cannot stall the server.
- **Foreground command and cwd** are sampled on a timer and cached (they are
  syscalls: `tcgetpgrp`, then `/proc` on Linux or `libproc` on macOS). The
  sample includes the foreground process's full argv, exposed as
  `pane_current_argv`. Nothing downstream trusts the process name: a Claude
  Code pane reported its comm as `2.1.245` (canopy c15f63e), and hand-started
  agent panes are identified from argv (canopy 5aaac53). In M1
  cwd comes only from sampling: `vte` 0.15 does not parse OSC 7. From M3
  grofe's own OSC byte scanner (T3.3, §4.9) reads OSC 7, which is then the
  preferred cwd source when a shell emits it, and emits `PaneEvent::Cwd`.
  The same scanner also frames kitty graphics commands (APC `_G`, §4.9),
  which `vte` 0.15 drops.

### 4.9 Passthrough

The emulator consumes every byte, so nothing passes through implicitly. grofe
re-emits exactly this set to attached clients: OSC 52 clipboard (per config),
window title, bell (also sets the window activity flag, which agents rely on),
OSC 8 hyperlinks (re-emitted from cell hyperlink attributes), and OSC 9 / OSC 777
notifications. `vte` 0.15 does not parse OSC 9 or OSC 777, so those come from
the same grofe OSC byte scanner that reads OSC 7 (§4.8), run over pane output
before the parser sees it. Unsupported in v1: sixel, kitty graphics direct
placement, arbitrary DCS passthrough (a nested tmux works as a normal terminal
app, without passthrough).

**Inline images [planned, M3]: kitty graphics via Unicode placeholders.** grofe
supports the kitty graphics protocol's Unicode-placeholder mode, the mode kitty
documents for multiplexers. The application transmits image data and creates a
virtual placement (APC `_G` with `U=1`), then prints U+10EEEE placeholder
characters where the image should appear. The row and column of each cell are
encoded as combining diacritics, and the image id is encoded in the foreground
colour (and the placement id in the underline colour).

- **Transmit.** `vte` 0.15 drops APC, so the pre-advance byte scanner that reads
  OSC 7/9/777 (§4.8) also frames APC `_G` sequences, surviving split reads. The
  transmit, virtual-placement and delete commands are forwarded to every
  attached client whose probe reports kitty graphics (§4.4), queued ahead of the
  next frame. Image ids are remapped per pane so two panes that both pick id 1
  cannot collide; forwarded commands carry `q=2` so the outer terminal's replies
  never land in grofe's input stream, and grofe answers the pane's own queries
  (`a=q`) from the client's caps.
- **Placeholders are text.** The emulator stores placeholder characters and
  their diacritics as ordinary cell content, so scrolling, splits, zoom and
  resize need no image tracking: the outer terminal draws whatever placeholder
  cells grofe renders. The renderer's only special case is the per-pane id
  remap: it rewrites a placeholder cell's colour to the remapped id and never
  downgrades that colour.
- **Late attach.** The server keeps the last transmitted data per image id,
  under a memory cap (oldest evicted first), and re-sends it to a client that
  attaches later or needs a full redraw.
- **Out of scope in v1:** direct placement (any placement without `U=1`) and
  sixel. Both draw at screen positions and need
  per-pane image tracking and clipping.

### 4.10 Status line [planned, M2]

Formats over options only (D8). Beyond `status.left` and `status.right`:

- `window-status-format` and `window-status-current-format` render the window
  list.
- `#{}` expands inside `#[...]` style directives, so themes are data:
  `#[fg=#{@theme_blue}]` (canopy 1aa91b4).
- strftime sequences (`%H:%M`) expand, so a clock needs no subprocess.
- Battery and network have no provider until M6 plugins set options; canopy's
  dogfood setup loses them until then.
- Multi-row status (`status.rows`; canopy uses two rows at the top) is a later
  nice-to-have, not scheduled.

---

## 5. Config

### 5.1 Files and layering

Load order, later wins:

1. built-in defaults (compiled in, the same schema)
2. `~/.config/grofe/config.toml`
3. `~/.config/grofe/conf.d/*.toml`, lexical order
4. `~/.config/grofe/user.toml`, the user's final word
5. `~/.config/grofe/init.lua` (M6)
6. runtime `set-option` commands

Tools such as canopy write only `conf.d/NN-*.toml`; `config.toml` and
`user.toml` belong to the user and no tool writes them. `user.toml` loads after
every generated fragment, so the user's own settings always win over a tool's.

`source-file <path>` loads another TOML file at the current layer. A relative
path resolves against the directory of the file that includes it, never the
server's cwd or environment. `reload` (bound to `prefix r`) re-reads layers 1
to 4 and applies the diff (§5.4).

### 5.2 Schema

The config is a Rust struct with serde derive. Unknown keys are an error naming
the file and the closest valid key; type errors fail loudly, never a silent
default. A generated JSON Schema for editor completion (via `schemars`) is
deferred until after dogfooding.

**Version skew.** A file may declare `requires = "grofe >= X.Y"` at the top. A
file whose requirement the running grofe does not meet is skipped with a
warning, not a fatal error, so a fragment generated for a newer grofe cannot
kill an older one. Strict unknown keys without this escape hatch are how
canopy's `extended-keys-format` line broke every tmux 3.4 install (canopy
b303aae). Unknown keys in a file without a `requires` header stay errors.

```toml
prefix = "C-b"
base-index = 1
pane-base-index = 1
escape-time-ms = 25
history-limit = 50000
mouse = true
remain-on-exit = false

[status]
position = "bottom"
style = "fg=default,bg=default"
left = "#{session_name} "
right = "#{?#{@agent_rollup},#{@agent_rollup} ,}#{host_short}"

[pane-border]
style = "fg=brightblack"
active-style = "fg=green"

[keys.prefix]
"|" = "split-window -h -c '#{pane_current_path}'"
"-" = "split-window -v -c '#{pane_current_path}'"
"r" = "reload"

[keys.root]
"M-h" = { cmd = "select-pane -L", desc = "focus left", name = "focus-left" }
"M-H" = { cmd = "resize-pane -L 5", name = "grow-left", repeat = true }
```

Key bindings are data: key names map to command strings (parsed by the CLI
parser, `;` separating a command sequence) or to `{ cmd, desc, group, name,
repeat }` tables, so help surfaces get descriptions by construction. `name` is a
stable identifier that tools and help screens refer to instead of the key;
`repeat = true` lets the binding repeat without the prefix within the repeat
time. Validation rejects a key bound twice in one table and a `name` used twice.

### 5.3 Saner defaults than tmux

`base-index = 1`, `pane-base-index = 1`, `escape-time-ms = 25`, `mouse = true`,
`history-limit = 50000`, focus events on, true colour on, extended keys on, OSC
52 clipboard on, `renumber-windows = true`, `automatic-rename = true` (§4.5),
`restore-on-start = true` (§6), `mode-keys = "vi"` (M5), new panes and windows
inherit the active pane's cwd, and the status line cannot run subprocesses.

Two deliberate differences from canopy: canopy sets `escape-time 0`, grofe keeps
25 ms so a lone ESC survives ssh; canopy sets `history-limit 100000`, grofe
keeps 50000 as a default the user can raise.

### 5.4 Provenance and introspection

Every resolved setting records the layer and file that set it (plus the line
where the TOML parser gives it cheaply). This replaces canopy's `sleep 600`
throwaway-server workaround with a command:

```
$ grofe show-config status.right --explain
status.right = "#{host_short}"
  set by ~/.config/grofe/conf.d/20-status.toml
```

`grofe show-config --file <path>` resolves a config **without a running server**,
for tests and for tools like canopy. It resolves the chain this machine actually
loads (`config.toml`, `conf.d`, `user.toml`), not only the named file, and it
must never hang: canopy's equivalent probe deadlocked for 9m20s in CI (canopy
2907e89). The full override chain in `--explain` output is deferred until after
dogfooding.

**Errors.** Every config error carries its file and line. `reload` validates
every layer first, then applies all of it or none of it, and reports all errors
at once rather than stopping at the first. Exit statuses are honest: in tmux a
globbed `source-file` and a `run-shell` reset the status, so canopy's checks
passed on broken config (canopy 1aa91b4, 94435f6).

**Key provenance** covers every binding, including runtime `bind-key` and
plugin binds, and `list-keys` marks bindings that did not come from the config
table, so a doctor can flag them. tmux-resurrect silently took over canopy's
`prefix C-s` this way (canopy 94435f6).

### 5.5 Importing a tmux.conf [planned, M3.5]

`grofe import-tmux [<tmux.conf>] [-o <path>]` reads a `tmux.conf` (default
`~/.tmux.conf`, then `~/.config/tmux/tmux.conf`), follows `source-file`, and
writes grofe TOML to stdout or to `<path>`. It never edits the tmux.conf.

- **Converted:** `set`/`set-option` of options grofe has (tmux names mapped to
  the schema, e.g. `status-left` to `status.left`), `bind-key`/`unbind-key`
  including `-n`, `-r` and `-T` with the `prefix` and `root` tables, `prefix`,
  `mouse`, `base-index`, and `status-left`/`status-right` formats that do not
  use `#()`.
- **Reported, not converted:** every other line, each with its file, line and
  reason: `#()` in a format (D8), `if-shell`, `run-shell` and hooks, plugins
  and TPM, options grofe does not have. The report goes to stderr, so stdout
  stays valid TOML.
- The output loads in grofe with no errors, and is meant as a starting
  `config.toml` or `user.toml` that the user then edits. There is no
  round trip and no runtime reading of tmux.conf (D1).

---

## 6. Persistence and pane identity (M4)

- **Snapshot.** The server writes `$XDG_STATE_HOME/grofe/<socket>/state.json`
  (with a `version` field) on structural change and on cwd change (the sampling
  timer; OSC 7 from M3), debounced ~1 s, and on clean shutdown. Writes are atomic (temp
  file plus rename). Per pane: `uid`, `cwd`, spawn `argv`, `resume`, and user
  options set with `--persist`. Snapshots are server-driven and never depend on
  an attached client (continuum's did).
- **What persists.** Identity-like options (`@agent_session`, `@agent_source`
  style) are set with `--persist`. State-like options (`@agent_state`) never
  are: after a restore the agents plugin shows a "restored/unknown" state until
  its adapter reports, because a stale `working` is a lie.
- **Restore.** `grofe start-server --restore`, or `restore-on-start = true` in
  config (the default), recreates everything from the snapshot, and only into an
  empty server, so restoring twice cannot duplicate sessions. Each pane runs
  `resume` if set, otherwise a shell in `cwd`. When a resumed process exits, the
  pane respawns as a shell in `cwd` with the same `uid`, so the layout does not
  collapse. `uid`s are preserved. Panes restored before any client attaches get
  their `update-environment` variables on first attach.
- **Resume.** `pane-set-resume -t <pane> -- <argv...>`. An adapter calls it as soon
  as it learns its session id, which covers canopy's tier 1 and tier 2 agents
  without a save-time rewrite hook. A `before-save` hook is added only if
  dogfooding finds a case this cannot handle. Neovim is the second resume client
  (`pane-set-resume nvim -S <session>`), so the mechanism is not agent-shaped.
  - The CLI resolves `argv[0]` to an absolute path using the **caller's** `PATH`
    before sending it, so a server started at boot with a minimal `PATH` can
    still run it (canopy ef20852).
  - A resume is bound to the process that set it: the pane's foreground process
    group at set time. When that process exits or leaves the foreground, the
    resume is cleared. Closing the agent and keeping the shell therefore does
    not resurrect the closed conversation on the next reboot.
- **Stagger.** Restoring many panes spreads their spawns with a deterministic
  per-pane delay (a hash of the `uid`), bounded by `restore-stagger-ms`
  (default 1000; 0 means exactly off). Only panes with a `resume` are staggered,
  and the delay is applied by the server, never embedded in the argv (canopy
  19fab4e, d0c7bea).
- **`grofe reboot-check`** reads the snapshot on disk and compares it with the
  live model; it never reports in-memory intent. Per pane it gives one verdict:
  "will resume", "will restart without its conversation", "not saved yet", or
  "will not be restored", with the reason. Exit status: 0 every pane will
  resume, 1 some will not, 2 persistence is misconfigured (no
  `restore-on-start` and no autostart unit) (canopy 1bc0255, ab37bf0).
- **Out of scope:** scrollback persistence. Login autostart belongs to canopy on
  grofe (§7.1).

---

## 7. Plugins (M6), agents (M7), canopy on grofe

Sketch only; §7 is rewritten after M5 from dogfooding notes.

- **Out-of-process plugins** speak control mode over the socket, in any language,
  and subscribe to events. The server launches configured plugins and restarts
  them with backoff.
- **In-process Lua** (`mlua`) runs `init.lua` and synchronous callbacks (key
  bindings, format functions) where a socket round trip would be wrong.
- **agents plugin:** the five normalized states (`idle`, `working`, `blocked`,
  `completed`, `exited`) as the pane option `@agent_state`; `grofe set -p
  @agent_state working` is the whole write path for adapters; rollups written as
  window and session options on transition; unseen cleared on `pane-focus`.
  canopy's adapters port by replacing `tmux set` with `grofe set` and calling
  `pane-set-resume`. The `exited` state is visible because agent panes run with
  `remain-on-exit`.
- **Adapter contract [planned, M7].** Each adapter is a directory with a
  manifest:

  | Key | Meaning |
  |---|---|
  | `id` | equals the directory name; charset `[A-Za-z0-9._:-]`, validated on entry and again on use (canopy 94435f6) |
  | `command` | the agent's executable |
  | `resume_template` | argv that resumes a conversation; contains `{id}` |
  | `launch_template` | argv that starts a new conversation |
  | `can_pin_at_launch` | whether the adapter can choose the session id at launch |
  | `detect_draft` | command reporting unsent input: exit 0 has a draft, 1 none, anything else undetermined; empty means the adapter cannot answer |

  Adapters are tiered 1/2/3 as in canopy. Hooks always exit 0 and stay cheap:
  a Claude Code `PreToolUse` hook that exits 2 blocks the tool. An adapter
  calls `pane-set-resume` whenever the session id changes, not only on a state
  change: `/clear` gives a new id without one (canopy c15f63e).

### 7.1 canopy on grofe

canopy keeps its tmux path until grofe reaches parity, then gains a grofe backend:

| canopy piece | On grofe |
|---|---|
| conf.d layers | canopy writes only `~/.config/grofe/conf.d/NN-canopy-*.toml`. The restore-point promise becomes trivial: canopy removes its own files |
| user.conf last | `~/.config/grofe/user.toml`, the final file layer (§5.1); `config.toml` is also the user's |
| caps | grofe has no `%if`; canopy generates capability-gated conf.d fragments itself and marks them with `requires` (§5.2) |
| themes | generated conf.d TOML using `status.style` and `pane-border` options; needs `#{}` inside `#[...]` (§4.10) |
| status aggregator | clock native via strftime (§4.10); battery and network via an M6 plugin that sets options |
| keys.tsv | generated TOML bindings with `name`, `repeat`, `send-prefix`, `;` sequences and `select-layout main-vertical` |
| doctor, load-bearing settings | `show-config --explain` checks. The load-bearing checks become: `restore-on-start` or an autostart unit present, state dir writable, snapshot fresh. canopy's tmux ones (continuum hooks in `status-right`) no longer exist |
| resurrect, continuum, save strategy | replaced by grofe M4 |
| `reboot-check` | core (M4) says whether a resume is set; the agents plugin adds reasons ("no session id reported") |
| `adopt` | agents plugin (M7). No respawn: adopt only calls `pane-set-resume` on the live pane, so the unsent-input guard is unnecessary for adopt itself. Keeps dry-run by default and idempotence |
| inline images (`allow-passthrough on`) | an agent's kitty graphics in placeholder mode pass through natively (§4.9), with no passthrough option; direct placement and sixel do not pass, and those programs fall back to whatever they draw without graphics |
| palette, which-key, key search | `display-popup -E` + fzf and `display-menu` (M5), backed by `list-keys` descriptions |
| login autostart | canopy ships the launchd/systemd unit running `grofe start-server --restore`; the unit gives panes a real `PATH`, or relies on `argv[0]` resolution (§6) |

**Consciously dropped from canopy:** vendoring resurrect and continuum, and the
relative-path strategy hack (canopy 495df6b); the `@resurrect-processes` regex
(canopy d0c7bea); the stagger wrapper inside the command string; "status-right
must load before continuum"; continuum's other-server arithmetic; `%if`
duplicate-append guards on reload (canopy 6ef854a); throwaway-server validation
with `sleep 600` and `env -i`; the M-s/M-r key relocation; the five-minute save
interval (grofe snapshots on change). **Kept on purpose:** reading the session
id off a process argv for hand-started panes.

---

## 8. Libraries and terminal support

Verified 2026-09-23 against crates.io, docs.rs and upstream source. The M0 spike
confirms them in practice before anything is built on top.

| Need | Pick | Why | Runner-up |
|---|---|---|---|
| pty | `nix` 0.31 (`openpty`, `TIOCSWINSZ`, `tcgetpgrp`) + `std::process::Command` with `pre_exec` + `tokio::io::unix::AsyncFd` | Full control, async reads with no thread per pane; Zellij's approach (`os_input_output_unix.rs`) | `portable-pty` 0.9.0: complete API but blocking readers, no tokio integration, last release 2025-02 |
| foreground cwd | Linux `/proc/<pgrp>/cwd`; macOS `libproc` 0.14; OSC 7 preferred | No crate covers both; small per-OS function | none |
| emulation | `alacritty_terminal` 0.26.0 (Apache-2.0, active) | Alt screen, bracketed paste and mouse modes, reflowing resize, damage tracking, OSC 52 and PtyWrite via `EventListener`, serde; published for external use (Zed depends on it) | `vt100` 0.16.2: built-in state diffing, but no reflow and dormant since 2025-07 |
| watch list | `libghostty-vt` 0.2.1 | Best-in-class emulation, but API declared unstable and the build needs Zig | revisit at M6 |
| client tty | `crossterm` 0.29 | Raw mode, SIGWINCH, size; Zellij's client uses it | `rustix` termios |
| diff rendering | own compositor (§4.4) | Pane content is arbitrary styled cells; Zellij does the same (`output/mod.rs`) | `termwiz` 0.23 `Surface` |
| config | `serde` 1.0, `toml` 1.1; `schemars` 1.2 later | Validation through serde | none |
| scripting (M6) | `mlua` 0.12 (Lua 5.4, `vendored`, `async`, `serde`) | Active and mature | none |
| async, sockets, framing | `tokio` 1.53 Unix sockets, `tokio-util` 0.7 codecs | Standard stack | `interprocess` 2.4 (Zellij) |
| tests | `insta` 1.48 snapshots; `expectrl` 0.9 for pty-driven acceptance; `bats` for the CLI smoke suite | Snapshot the screen an emulator reconstructs from our output | `rexpect` 0.7 |

Not usable: `wezterm-term` is not published on crates.io.

**Licensing:** grofe is Apache-2.0, which matches `alacritty_terminal`
(Apache-2.0 only).

**Inner `TERM`:** D7. WezTerm advertises `xterm-256color`; Zellij sets nothing
and panes inherit the outer `TERM`. grofe panes get `screen-256color` plus
`COLORTERM=truecolor`, resolved by `src/term.rs::resolve_term()` against the
host's compiled terminfo database (falling back to `xterm-256color` if even
`screen-256color` is missing). T0.4 compared `tmux-256color`'s terminfo
entry against `screen-256color`'s and confirmed `alacritty_terminal` 0.26
parses every extra SGR capability tmux advertises (italics, strikeout,
hidden text, the `Smulx` extended underline styles), but the M0 spike's
render path does not reproduce all of them: underline styles collapse to
plain underline, and non-SGR extras (title relay, OSC 52 clipboard, cursor
colour, focus events) are not forwarded to the outer terminal yet: the M2 renderer (T2.2) emits underline styles and M3 passthrough (T3.3) forwards title, OSC 52, cursor colour and focus. Since not
every extra capability works end to end, grofe stays on the smaller,
honest `screen-256color` for now. See docs/spike-notes.md's "TERM decision"
section for the full evidence.

An inner `CSI > q` (XTVERSION) gets no reply from the library; grofe answers
it only if an app needs it. This is distinct from the client's probe of the
outer terminal in §4.4.

**Zellij files worth reading:** `zellij-server/src/panes/grid.rs`,
`terminal_pane.rs`, `os_input_output_unix.rs`, `pty.rs`, `screen.rs`, `route.rs`,
`output/mod.rs`; `zellij-utils/src/ipc.rs`; `zellij-client/src/stdin_ansi_parser.rs`.

**Acceptance suite (terminal correctness):** Neovim (alt screen, true colour,
cursor shape, mouse, bracketed paste, focus events, background colour query),
fzf, less, htop, `git log` with a pager, a nested tmux and a nested grofe, a
wide-character and emoji screen, and `kitten icat --unicode-placeholder` where
kitten is installed. Every item must pass before a milestone that
claims daily-driver status is done.

---

## 9. Testing

- **Unit:** command parser, targets, format expansion, layout, config layering
  and provenance, protocol framing, key encode and decode, and the capability
  probe against recorded replies from Ghostty and WezTerm (D11).
- **Integration (the backbone):** each test starts a server on a unique `-S`
  socket with `remain-on-exit = true`, with `HOME`, `XDG_STATE_HOME` and
  `XDG_RUNTIME_DIR` in tempdirs and restore-on-start forced off, so a test can
  never restore the user's real sessions. It drives the server through control
  mode and asserts on output, `capture-pane` and events.
- **CLI smoke (bats):** drives the built `grofe` binary with no Rust harness,
  a temp `HOME` and no `GROFE*` variables in the environment, proving
  canopy-style shell scripts can use grofe. canopy had 93 green tests while its
  installed config loaded nothing, because the tests never ran what was installed.
- **Render snapshots:** the round-trip trick from §4.4.
- **Acceptance:** an attached client in a pty, driven by `expectrl`, runs the §8
  suite and asserts on the reconstructed screen. CI installs neovim, fzf, htop and
  tmux on Linux and macOS. Tests discover tool paths, never hardcode them
  (canopy acc19eb), and run against the system's tools as well as the dev
  toolchain: a mise-provided tmux hid canopy's tmux 3.4 floor bug (canopy
  b303aae).
- **Persistence (M4):** canopy's fake-agent fixture (appends a launch/resume
  transcript and loops instead of exec'ing `sleep`) and its reboot scenarios
  (canopy ef20852, `test/smoke/scenarios.sh`). Each scenario is proven able to
  fail by mutating the saved snapshot and watching it go red.
- **Determinism:** inputs that depend on pids (stagger, jitter, generated
  names) are seeded deterministically in tests (canopy 11a3adf).
- **CI**, on `ubuntu-latest` and `macos-latest`, split by cost into separate jobs
  so one red job cannot hide another (canopy 94435f6, acc19eb): `unit` (fmt,
  clippy, lib and doc tests) on every push; `acceptance` (integration tests,
  bats, and later containers) on pull requests, `main` and manual dispatch.

---

## 10. Risks

| Risk | Mitigation |
|---|---|
| Emulator library gaps | M0 spike runs Neovim through `alacritty_terminal` before anything is built on it. |
| Copy mode grows into a subsystem | Own milestone (M5), scoped in writing first. Until then the dogfood fallback is the outer terminal's native selection (shift-drag bypasses mouse mode). |
| Detach/reattach at a different size | D6; integration test at M2. |
| A server crash loses live work | Dogfood alongside tmux in month one; M4 snapshots make a crash recoverable; panics write a report; no `unwrap` on socket or pty paths. |
| Scope creep toward "replace tmux for everyone" | §1 non-goals, D1, and the milestone column in §4.5. New commands need a dogfooding reason. |
| Protocol churn breaking tests | Protocol internal until M6; tests go through one harness module. |
| Terminal replies stuck behind the actor, making apps hang | §4.8: replies go from the reader straight to the writer queue. |
| Inline images help only programs that use kitty's placeholder mode | Before building it, a T0-style check records which of the owner's tools emit placeholders (Claude Code image output, `kitten icat --unicode-placeholder`, chafa and timg if relevant). Programs that use direct placement or sixel still show nothing, as in v1's non-goals. |

---

## 11. Non-goals (v1)

- Windows support.
- Reading `tmux.conf` at runtime and running tmux plugins (`grofe import-tmux`
  converts once, M3.5, §5.5); tmux's `-CC` wire format (M8 maybe).
- WASM plugins.
- Network session sharing.
- Sixel, kitty graphics direct placement, arbitrary DCS passthrough. Kitty
  graphics in Unicode-placeholder mode are supported (§4.9).
- Scrollback persistence across restarts.
- Layout presets beyond `main-vertical` and `even-*` (§4.3), until dogfooding asks.
