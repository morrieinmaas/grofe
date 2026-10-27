# grofe: design

**Date:** 2026-09-23
**Status:** draft, revised after three review passes (architecture, execution, requirements);
revised 2026-09-27 for the owner's tmux compatibility decision (D1, D12);
revised 2026-09-28 with the task-level M4 to M7 plan (§4.11, §6, §7, §9)
**Origin:** the `grofe-plan.md` design note (formerly `rtmux-plan.md`); canopy
(`morrieinmaas/canopy`) is the requirements document.

---

## 1. What this is

grofe is a tmux-shaped terminal multiplexer written in Rust. A server holds
sessions, windows and panes; clients attach and detach over a Unix socket.

**Familiar on the surface, correct underneath.** Someone with tmux muscle memory
should be productive in five minutes: same object model, same prefix-key habits,
same CLI verbs and target syntax. For the owner's own workflow it goes further:
with `tmux` on PATH pointing at grofe, their scripts, tools and key bindings
behave the same (D1, D12). Underneath, the things tmux got wrong for
twenty years are fixed: a typed config with a schema and provenance, no
subprocess on the status line's redraw path, pane identity that survives
respawn and reboot, and a plugin surface built on a real event stream.

**The real case, in priority order:**

1. **Native persistence and pane identity.** "A pane has a durable identity and a
   command that resumes it" is a struct field, not a save-command strategy patched
   into a vendored plugin. This is the strongest argument for the project.
2. **Config semantics.** TOML with a schema, layering, and provenance ("what did
   this resolve to, and which file said so").
3. **Plugin semantics.** Events and a scriptable control surface, designed after
   dogfooding, from what actually hurt.

**Not the case:** performance. tmux's redraw is already fast; what is slow in
practice is per-redraw subprocesses in status lines, which grofe bans by design.
If performance becomes the headline, the project has lost the plot.

**What grofe is not:**

- Not a general tmux reimplementation. It answers to the name `tmux` and
  covers what the owner's scripts and tools send (§4.5), but it does not read
  `tmux.conf` as its config. The owner's daily config is ported once to
  grofe-native TOML (plan M3.6), drafted by `grofe import-tmux` (M3.5, §5.5).
  The remaining tmux plugins run unchanged through canopy's plugin loader
  (plan M3.7, §5.1); TPM itself does not run.
- Not Zellij. No WASM plugin runtime, no modal UI by default, tmux semantics.
- Not cross-platform beyond Unix. macOS and Linux only.

---

## 2. Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | **tmux-compatible for the owner's workflow, not a general tmux reimplementation.** grofe answers to the name `tmux` (D12) and supports every CLI verb, flag, target form, format variable and modifier, and key-binding feature that the owner's scripts and tools send, as listed in §4.5 and pinned by a compat suite whose goldens are recorded from real tmux 3.7b (plan M3.6). That inventory closes the scope, not tmux's manual: anything outside it needs a dogfooding reason. The owner's daily config and canopy are ported once to grofe-native TOML, bindings and small scripts (plan M3.6), drafted by `grofe import-tmux` (M3.5, §5.5); the TPM plugins run unchanged through canopy's plugin loader (plan M3.7, owner ruling 2026-09-29). Compatibility is a **translation front end** (D12, §4.5): a thin, table-driven layer in `src/compat/` that maps tmux argv and tmux format strings onto grofe commands and features, not tmux semantics spread through grofe's own grammar. grofe's native CLI and config grammar may diverge from tmux wherever grofe does better. **Non-goal:** executing `tmux.conf` as the config, or running TPM itself; a plugin's own tmux files are read only as §5.1 says. No `-CC` wire format in v1. | The owner's goal is to put `tmux` on PATH pointing at grofe and notice no difference in daily work. Muscle memory lives in key bindings and scripts, not in the config language, so those must behave identically. tmux's config language and plugin semantics are exactly what grofe escapes, so they move once instead of running forever: typed TOML config, provenance and native persistence stay the core. A general reimplementation has no end; a closed, recorded list does. Keeping the list in one front-end table, rather than in the native parser, means a pure grofe user never meets tmux's quirks and grofe's own grammar stays small. `-CC` (iTerm2 integration) is a possible M8, not a v1 promise. |
| D2 | **One binary, `grofe`.** It is the server, the attach client, and the CLI, and it also answers to `tmux` (D12). Only session-creating commands auto-start a server (§4.1); every other command fails fast when none is running. | One thing to install and version. Agent hooks call `set` constantly, often from IDEs outside grofe, and must never spawn a stray server. |
| D3 | **The server renders; the client is thin.** The attach client puts the tty in raw mode, probes the outer terminal's capabilities once, forwards input bytes and resize events, and writes whatever bytes the server sends. All state and all composition live in the server. | Client crashes are survivable by construction. Same split as tmux. |
| D4 | **Headless control mode is the first interface.** Every operation is a command; the CLI, key bindings, control mode and (later) plugins all go through one command dispatcher. | Tests and canopy's bats suite can drive grofe before any UI exists. A scriptable surface is what makes the plugin API designable later. |
| D5 | **Single-owner state.** One tokio task (the server actor) owns the model. Pane I/O tasks, client connections and timers send it messages. The only shared state is each pane's terminal behind a mutex, with bounded lock holds (§4.8). | Removes a class of concurrency bugs; easy for parallel implementers to reason about. |
| D6 | **Window size follows the latest active client**, where "latest" means last input, debounced ~500 ms so two attached clients do not make the window flap. Larger clients are letterboxed (window drawn at its size, the rest filled with a dim pattern). Smaller clients are cropped, with a viewport that follows the active pane's cursor. The emulator reflows the primary screen on resize; alt-screen apps just get SIGWINCH. | Every multiplexer picks a side. tmux users already know this behavior. |
| D7 | **Advertise an honest `TERM`: `tmux-256color`.** Default `tmux-256color` plus `COLORTERM=truecolor`, falling back per host to `screen-256color` when the `tmux-256color` terminfo entry is missing, and to `xterm-256color` when both are (that last resort advertises more than `screen-256color` and exists only for such hosts). T0.4 checked whether every extra capability `tmux-256color`'s entry adds over `screen-256color`'s (italics, strikethrough, hidden text, the `Smulx` extended underline styles, OSC 52 clipboard, focus reporting, cursor colour, title relay) survived the M0 spike's render path; it did not, so M0 to M3 shipped `screen-256color`. T3.6 re-ran the check through grofe's real render path (pane, server, attach client, reconstructed outer terminal) on macOS and Linux after T2.2 and T3.3 landed, and every capability passed (`tests/acceptance/t3_6.rs`, docs/spike-notes.md "T3.6 re-run"), so the default is now `tmux-256color` under both names: under the `tmux` name `TERM` is what the owner sees in tmux today. The honesty rule stands: a capability that fails later is fixed, not advertised anyway, and the re-run passing is a gate for alias day (plan M3.6). | Whatever is advertised, apps will use. No custom terminfo entry: it breaks over ssh. The owner runs `tmux-256color` with italics and undercurl in Neovim; a smaller `TERM` would be a visible difference under the alias. |
| D8 | **No subprocess on the redraw path, ever.** The status line renders formats over options; rendering never forks or waits. Dynamic content arrives in options, set either by something external or by a **status source**: a command the server runs at most once per `status-interval` (or the source's own `interval`), off the redraw path, with one run in flight and a timeout after which it is killed; the last non-empty line of its output (CR or LF ends a line), as tmux 3.7b keeps it, is stored in an option. `#(cmd)` in a format is shorthand for an inline source keyed by the command: the format reads the cached value, empty until the first run returns, as in tmux. `run-shell` exists for key bindings and hooks. | Forks in the status line are the one real performance problem in tmux setups. canopy measured it: folding its status into one aggregator and removing 20 forks took a render from ~97 ms to ~67 ms (canopy 6ef854a). Banning commands outright would lose the owner's pet, network, battery and canopy-status segments, which the alias must keep. A cached source bounds the cost by the number of sources and their intervals, never by redraws, clients or windows, and a slow or hung command can only make its own segment stale. |
| D9 | **Build the plugin API last** (M6). Before that, the control protocol and events exist for tests and dogfooding, may change, and grow only when a test or feature needs them. | Needed events beat guessed events. |
| D10 | **Generic primitives in core, agent semantics in a plugin.** Core provides pane identity, a resume command, pane options, events, persistence, and config introspection. The five agent states, adapters and rollups live in a first-party `agents` plugin (M7). **[Dropped with M7, owner ruling 2026-10-06]:** no first-party agents plugin is built; agent semantics stay in tmux plugins run unchanged through canopy's loader and in canopy's own adapters and strategies. | Keeps core general. The plugin API's first users are now the tmux plugins and the in-tree event-log plugin (plan M6). |
| D11 | **Ghostty and WezTerm are the first-class outer terminals.** grofe is tested and tuned against both: kitty keyboard, synchronized output, OSC 52 and kitty graphics placeholders (§4.9). Other terminals work with legacy key encoding and whatever features the capability probe (§4.4) finds; missing features degrade, never break. WezTerm speaks kitty keys only with `enable_kitty_keyboard = true` in the user's WezTerm config, and grofe's docs must say so. | Two modern terminals cover the owner's machines and most of the target audience. Naming them keeps the manual checks and the probe fixtures concrete. |
| D12 | **grofe answers to `tmux`.** When `argv[0]`'s basename is `tmux` (a symlink or wrapper on PATH; a shell alias does not reach scripts), that CLI call behaves as tmux's CLI: `-V` prints `tmux <compat level>` (the tmux release the compat suite is recorded against, 3.7b today), and the aliases and prefix matching of §4.5's invocation row apply. **Identity is a server property:** fixed when the server starts, from the `argv[0]` of the CLI that started it, and overridable with the `compat-identity` server option (`auto`, the default, then `tmux` or `grofe`). A server with tmux identity gives panes `TERM_PROGRAM=tmux` and `TERM_PROGRAM_VERSION` set to the compat level, answers `#{version}` with the compat level (`#{grofe_version}` always gives grofe's own), and appends every verb, flag or format variable it does not know to `compat-miss.log` in its state directory, which the dogfood log reviews. Under both identities panes get `TMUX` in tmux's format (`<socket path>,<server pid>,<session id number>`) and `TMUX_PANE` (`%N`) alongside `GROFE` and `GROFE_PANE`. Error texts and exit statuses are tmux 3.7b's for tmux-dialect requests only; grofe-dialect requests keep grofe's own texts (T1.2's). Under tmux identity the built-in option defaults where grofe's differ (§5.3) become tmux 3.7b's (`base-index` and `pane-base-index` 0, `mouse` off, `escape-time` 10, `history-limit` 2000, `renumber-windows` off, and the rest of that list), and the default prefix table is tmux's stock one in place of grofe's defaults, so a script or a pair of hands sees what tmux would; a value the config sets still wins. Sockets stay in grofe's directory (§4.1), so a real tmux on the same machine never shares one. **The front end** (`src/compat/tmux/`, §4.5) handles exactly two sources of tmux dialect: a request whose hello carries `dialect: "tmux"` (set by a CLI invoked as `tmux`, §4.6), and text typed at the `:` command prompt of a server with tmux identity. Every other request is grofe dialect, including every `grofe` CLI call and control client; config files are always grofe-native; stored command strings keep the dialect they were stored with (§4.5). tmux's own `-C`/`-CC` control mode is not shipped in v1. A pure grofe user never meets the front end. | Scripts and tools (canopy-agent, vim-tmux-navigator, `fzf --tmux`, gh-dash, snacks.image) find the multiplexer through `tmux` on PATH and `$TMUX`/`$TMUX_PANE`, and use the socket in `$TMUX` with `-S`. Identity belongs to the server, not to each call, so a pane's environment never depends on which name a script happened to use; login autostart starts the server under the `tmux` name (§7.1). Setting `TMUX` under both identities means a tool behaves the same either way. The cost: a real tmux run inside a grofe pane reads `$TMUX` and would talk to grofe's socket, so the fallback is run as `env -u TMUX <absolute path to tmux>` (§8, README). Defaults follow the identity, never a single call's `argv[0]`, so a server keeps one set for its whole life; tmux's defaults and stock keys under the `tmux` name mean switching changes nothing a tmux user relies on, and the ported config sets the owner's values either way. canopy's `%if` gate on tmux's version is moot, since its config is ported (D1). Under grofe identity `TERM_PROGRAM` stays the outer terminal's (§4.2). |

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
3. **Compatibility fork.** D1: compatible for the owner's workflow (the
   `tmux` name, scripts, formats, bindings), own internals and config.
4. **Honest first-milestone date.** Dogfood start (M3) in about seven weeks, with
   tmux kept as the fallback. Alias day (M3.6, `tmux` on PATH is grofe, running
   the owner's ported daily config) about seven and a half weeks later; persistence (M4)
   follows. See the plan's critical path.

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

A server started implicitly by a session-creating command (not `start-server`)
also exits as soon as it has no sessions and no open connection, even if it
never had a session (the starting command failed). As in tmux, whose server
exits when no client is attached to a session, a long-lived observer does not
count as an open connection: a plugin's own `grofe control` (its environment
carries `GROFE_PLUGIN`) and any connection that has issued `subscribe`. A
one-shot CLI mid-request does count.

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
  A user option set with `set -p --persist` is included in the M4 snapshot,
  and stays marked until `set --no-persist` or an unset (§6).
  Setting an option to its current value is a no-op and emits no
  `option-changed` event, so hooks that re-set state on every tool call cost
  nothing downstream.
- **Environment:** a session holds the variables `update-environment` names
  (tmux 3.7b's default: `DISPLAY KRB5CCNAME MSYSTEM SSH_ASKPASS SSH_AUTH_SOCK
  SSH_AGENT_PID SSH_CONNECTION WAYLAND_DISPLAY WINDOWID XAUTHORITY
  XDG_CURRENT_DESKTOP XDG_SESSION_DESKTOP XDG_SESSION_TYPE`), taken from the
  client that created it and refreshed on attach and switch-client, as in tmux;
  one the client lacks is marked removed and hides the global value. Everything
  else a pane gets from the server comes from the server environment, so
  `TERM_PROGRAM` is the outer terminal's when the server started from it; `TERM`
  itself stays the honest inner value (D7). Panes get a UTF-8 locale forced if none is set,
  plus `TERM`, `COLORTERM`, `GROFE` (socket path) and `GROFE_PANE` (`%N`).
  From M3.6 they also get `TMUX` and `TMUX_PANE` (D12), and on a server with
  tmux identity (D12) `TERM_PROGRAM=tmux` replaces the outer terminal's value
  inside panes, as in tmux; the session environment keeps the outer value. A server
  environment (`set-environment -g`) sits under every session's and reaches
  new panes, `run-shell`, hooks, popups and status sources.

### 4.3 Layout

One algorithm: tmux's n-ary split tree. A window's layout is a tree of cells;
each internal cell splits its area horizontally or vertically among children;
leaves are panes. Splitting a pane in the direction its parent already splits
adds a sibling (three `split -h` give three columns). Supports zoom and resize by
cells or percent. Borders are one cell wide, shared between neighbours, active
pane border highlighted. Layout presets `select-layout main-vertical`,
`even-horizontal` and `even-vertical`: dogfooding already asks for
them (canopy's cockpit binding uses `main-vertical`). Other presets wait for a
reason. tmux layout strings (the checksummed form `#{window_layout}` prints)
are read by `select-layout` and written by `#{window_layout}` byte for byte as
tmux does: the owner's scripts pass them.

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

Every operation is a command, named and shaped like its tmux counterpart where
grofe has the same concept and no better form. One native parser turns argv
into a typed `Command`; one dispatcher executes it. The CLI, key bindings,
control mode and plugins all produce argv. The native parser grows only for
grofe's own reasons: no **new** tmux-only form enters it. M1's short names
(`new`, `neww`, `splitw`, `lsp`, `lsw`, `send`, `display`, `set`, `show` and
the rest of T1.2's `canonical_name` table) and its `-g` scope merges (`set
-gw`, `show -gw`) stay, as approved. Everything else tmux-only (tmux's full
alias table and prefix matching, tmux's option names and error texts)
reaches grofe through the tmux front end below. One tmux form is native
too, because it means the same in both: a leading `=` is an exact match
(targets below).

| Command | Milestone |
|---|---|
| `new-session` (`-d -s -c -x -y`, command), `kill-session`, `has-session`, `list-sessions` | M1 |
| `split-window` (`-h -v -c -p -l -t`), `kill-pane`, `select-pane` (`-L -R -U -D -t`), `resize-pane` (`-L -R -U -D -Z`, with a cell count: `-L 5`), `respawn-pane` (`-k`), `list-panes` | M1 |
| `new-window`, `kill-window`, `select-window`, `list-windows` | M1 |
| `send-keys` (`-l -H`, key names), `capture-pane` (`-p -S -E`) | M1 |
| `set-option`/`set` (`-g -s -w -p -u`, `--persist`), `show-options`/`show`, `display-message -p` | M1 |
| `kill-server`, `subscribe` (control mode) | M1 |
| `attach-session`/`attach`, `detach-client`, `next-window`, `previous-window`, `last-window`, `rename-session`, `rename-window`, `switch-client` (`-l -p -n`), `send-prefix`, `list-clients`, `capture-pane -e` | M2 |
| `bind-key`, `unbind-key`, `list-keys`, `source-file`, `reload`, `show-config`, `command-prompt`, `run-shell` (bindings and hooks, D8), `select-layout` (§4.3), `;` command sequences | M3 |
| `import-tmux` (grofe's own, not a tmux verb; runs without a server, §5.5) | M3.5 |
| The tmux compatibility breadth below: `display-popup`, `display-menu` (moved from M5: the owner's daily bindings need them), hooks, environment, structure verbs, formats, multi-row status | M3.6 |
| `pane-set-resume` (`-t -u`, `--expect <reason>`), `reboot-check` (`--json`), `start-server --restore` (§6) | M4 |
| Copy mode (`copy-mode -u -e`, `send-keys -X -N -F`), buffers (`set-buffer`, `show-buffer`, `list-buffers`, `delete-buffer`, `paste-buffer -p`, `choose-buffer`), `copy-pipe*` (§4.11) | M5 |
| `control` (a stdio bridge for plugins), `list-plugins`, `plugin-restart` (§7) | M6 |
| `agents adopt`, `agents next` (grofe's own, not tmux verbs; §7) | dropped with M7 (owner ruling 2026-10-06) |

**Targets (supported forms):** `$N`, `@N`, `%N`; `session`, `session:window`,
`session:window.pane`, with window by index or name and pane by index; exact
session name, then unique prefix; empty or omitted means the client's current
(from the request context, §4.6); `name:` is the session's current window. A
bare word with no `:` is read as tmux's `cmd-find.c` reads it, by the kind of
target the verb wants: as a pane, a number is a pane index in the current
window, then a window of the current session, then a session; as a window, a
window, then a session; as a session, a session only (TC.13: canopy's
`select-pane -t 1`).
A leading `=` means exact, natively and through the tmux front end alike,
as in tmux: `=name:window` and `=name:window.pane` name exactly session
`name`, never a prefix match; a bare `=name` is exact as a session or a
window and a literal word as a pane; `=N` or `=name` in the window part is
exactly that window. A bare `=` (tmux's mouse target in its stock bindings)
is refused with an error naming it, because mouse-bound commands act on the
pane under the pointer without a target (§4.5 bindings row). Not supported in
v1: fnmatch patterns, `{mouse}`/`{last}`/`{next}`/`{marked}` tokens, `!`,
`+`/`-` offsets.

**Formats:** M1 supports `#{name}` and `#{?cond,then,else}`; M2 adds
`#{==:a,b}`, the basename modifier `#{b:var}`, nesting and escaping when the
status line needs them. Formats in
flag values (`-c '#{pane_current_path}'`) are expanded at **execution** time
against the target's context, not at parse time. `#(cmd)` never forks during
expansion: it reads a cached status source (D8).
Expansion stops at tmux 3.7b's loop limit: text nested 100 `#{` deep
expands to nothing, as in tmux. An else branch that is one whole `#{?...}`
costs no depth, so the tmux front end's chain for `#{?c1,v1,c2,v2,...}` is
not cut, and the work stays linear in the format's length whatever it holds.
Variables: `session_name`, `session_id`, `window_index`, `window_name`,
`window_id`, `window_active`, `window_zoomed_flag`, `pane_id`, `pane_uid`,
`pane_index`, `pane_active`, `pane_pid`, `pane_current_command`,
`pane_current_argv` (§4.8), `pane_current_path`, `pane_width`, `pane_height`,
`pane_dead`, `client_width`, `client_height`, `host_short`, and
`#{@user_option}`. M4 adds `pane_resume`, `pane_resume_expected` and
`pane_restored` (§6); M5 adds `copy_cursor_x`, `copy_cursor_y`,
`scroll_position` and the `buffer_*` variables (§4.11).

**The tmux front end.** `src/compat/tmux/` (with
`src/compat/mod.rs` for identity, D12) holds one table: `mod.rs` defines its
schema and dispatcher, and per-area files (formats, bindings, structure)
register their rows into it. Each row is one tmux
verb or flag form the owner's setup actually sends (bounded by the recorded
corpus, plan TC.0), with its mapping and a golden test recorded from real tmux
3.7b (plan TC.0b). A newly discovered form is one table row plus one golden.
The table's columns are the tmux form, the mapping kind, the grofe command or
feature it maps to, and, per flag, **cosmetic** or **semantic**. Mapping kinds:

- **alias or prefix:** `ls`, `setw`, `tmux a` resolve to a verb (M1's
  short names such as `neww` pass through);
- **flag rename or merge:** `-t =name` to an exact session match, tmux option
  names to grofe's;
- **behaviour onto a grofe feature:** `#(cmd)` to a cached status source (D8),
  `set-hook <name>` to a grofe event subscription running the command,
  `command-alias` to a grofe alias, `if-shell -F` to grofe's conditional;
- **pass-through:** verb and flags identical to grofe's.

**Stored command strings keep their dialect.** A command string that is stored
and run later (a `bind` command, a `set-hook` command, a `command-alias` value,
`display-menu` items, a `confirm-before` command, a `command-prompt` template
with its `%%` and `-I` formats, `if-shell` branches) carries the dialect of
the request or config file that stored it. The front end translates it at
store time when the string is fully known; otherwise it is stored tagged with
its dialect and translated when it runs. Only `:` prompt input takes the
server's identity. So a `tmux bind ... neww` works, and a native-config
binding that opens a prompt with a native template still works on a server
with tmux identity.

**Unknown and unsupported forms.** A cosmetic flag grofe cannot honour (a popup
border style it lacks, a `-T` title where titles are unsupported) is ignored
with a `compat-miss.log` note. A semantic flag or verb grofe lacks fails
loudly, with tmux's error text for that case, and also writes a
`compat-miss.log` line. Nothing is silently dropped.

**Not flag-shaped, still exact.** The golden suite also checks, and the front
end owns where they are tmux-specific: output formats scripts parse (the
default output of `list-*`, `list-keys`, `show-options`, error texts, `-V`);
environment identity (`$TMUX`, `$TMUX_PANE`, `TERM_PROGRAM`, D12); and timing
semantics (when a hook fires relative to its command, what `display -p`
returns in the middle of a `\;` chain). Where the native behaviour differs,
the native feature is fixed; the front end does not fake it.

The table below groups the front end's rows by area. Flags already listed
above keep their milestone; the grofe feature each row maps to is native and
lands in the milestone shown.

| Area | tmux forms (rows) | Maps to | Milestone |
|---|---|---|---|
| Invocation | `argv[0]` `tmux` (D12); global `-L`, `-S`, `-V`; `-f <file>` loads that grofe TOML file as the only file layer, and `-f /dev/null` means none; `-2` and `-u` accepted with no effect; bare `tmux` means `new-session`, auto-starting a server (§4.1); tmux 3.7b's command aliases (`ls`, `setw`, `if`, `setenv`, `attach` and the rest of its table, taken from a `tmux list-commands` golden) and its prefix rule (an exact name or alias wins, otherwise a unique prefix, so `tmux a` attaches), in the tmux dialect only; natively, only M1's existing short names (`new`, `neww`, `show`, `display`, `send` and the rest of T1.2's table) | aliases and prefix in the tmux dialect; global flags pass through; `-V` output exact | M3.6 |
| Sessions and clients | `new-session -A` (attach if it exists) and `-ds <name> -c <dir> [cmd]`; `has-session -t =name`; exit statuses that make `attach \|\| new-session` work; `switch-client -c <client> -t <target>` and `-T <table>`; `detach-client -s <session>`; `refresh-client -S`; `info` (tmux's alias for `show-messages -JT`); `show-messages`; `start-server`; `list-clients -F`; `choose-client -Z`; `suspend-client` | pass-through; `=name` to an exact match; `info` alias | M3.6 (`switch-client -l -n -p`: M2) |
| Windows and panes | `new-window -n <name> [cmd]`; `select-window -t s:i`; `join-pane -d -h -v -s -t`; `swap-pane -U -D -s -t`; `swap-window -s -t`; `break-pane -d -n`; `resize-pane -x -y`; `resize-window -x -y -A -a`; `respawn-pane -k -t <pane> [cmd]`; `setw -t @N automatic-rename on`; `select-layout` with layout strings (§4.3) and `-E` (spread evenly); `last-pane`; `rotate-window -U -D`; `move-window -t`; `next-window -a`, `previous-window -a` (to a window with an alert); `find-window -Z` | pass-through to native verbs; layout strings through a codec onto grofe layouts | M3.6 |
| Listing and inspection | `list-sessions`, `list-windows` (`-a`, `-t`), `list-panes` (`-a`, `-t`), each with `-F` and `-f <filter>`; `display-message -p [-t] <fmt>`, and without `-p` as a status-line message for `display-time`; `show-options` with `-g`, `-v`, `-q` in any combination (`-gqv`) and `-t <session>`; `capture-pane -e -p -J -S -E` | pass-through; default output formats and `show` output rendered exactly | M3.6 (`-F`: M1; `capture-pane -e`: M2) |
| Options by tmux name | `set` and `show` accept tmux option names through one name table shared with `import-tmux` (`status-left` is `status.left`); `-a` append, `-F` expand, `-u` unset, `-q` quiet, `-o` only if unset; `set -p -t $TMUX_PANE @opt v`. Names in the table that grofe has no use for (`terminal-features`, `extended-keys-format`, `default-terminal`, which the probe and D7 decide) are accepted, stored and shown as "no effect" by `show-config --explain`; a name outside the table is an error, as in M1. Array options (`status-format[N]`, `command-alias[N]`) and append with `-ga`/`-as` extend the option store. The owner's options that could be in doubt: `main-pane-width 50%` has an effect (`main-vertical` uses it); `allow-rename off` is grofe's behaviour already (OSC titles never rename, above), so `on` has no effect; `aggressive-resize` has no effect, since D6 already sizes each window to its latest active client; `status-keys emacs` matches grofe's prompt keys, and `vi` has no effect | name renames; scope flag merges; arrays onto grofe list options | M3.6 |
| Command sequences | `\;` chains on the CLI and in bindings (canopy-agent's `set ... \; set ...` on every hook, the cockpit binding's five-command sequence) | pass-through | M3 (T3.5); goldens M3.6 |
| Bindings | `if-shell`, both the `-F` format form and the shell form (run off the actor; `-b` does not wait); `confirm-before -p`; `command-prompt -I -p` for renames; the `command-alias` option; `bind -N` notes as descriptions; custom key tables (`-T <name>`, entered with `switch-client -T`); `-r` repeat; bindable mouse keys (`MouseDown1Pane`, `MouseDown1Status`, `MouseDown3Pane`, `MouseDrag1Border`, `MouseDragEnd1Pane`, `WheelUpPane`, `WheelDownPane`, `DoubleClick1Pane`, `TripleClick1Pane`) acting on the pane or status range under the pointer, and a `display-menu` run from a mouse key opens at the pointer. **grofe's built-in mouse model** (T3.2, TC.10: focus, wheel, border drag, selection) replaces tmux's stock mouse bindings, so their argv forms (`-t =`, `send -M`, `resize-pane -M`, `display-menu -x M -y M`, `-x W -y S`) are not supported (M5 maps `copy-mode -M -e -H` onto native copy mode, TM.0 decision 3); TP.9 lists each stock mouse binding against its built-in replacement | `if-shell` to grofe's conditional; `command-alias` to grofe aliases; mouse argv forms fail as semantic | M3.6 |
| tmux default bindings | The stock prefix bindings the owner's server has live: `;` `last-pane`, `C-o`/`M-o` `rotate-window`, `q` `display-panes`, `.` `move-window` prompt, `f` `find-window` prompt, `C-z` `suspend-client`, `D` `choose-client -Z`, `~` `show-messages`, `E` `select-layout -E`, `M-n`/`M-p` `next-window -a`/`previous-window -a`, `/` `command-prompt -k` then `list-keys -1N`, `'` `command-prompt -T window-target`, plus `d w z [ ] x & , $ ! { } : = i n p` from earlier milestones and this table. `m` (`select-pane -m`, marking) is out of scope, below | none: grofe's native default key table | M3.6 (`[`, `]`, `=`: M5) |
| Overlays | `display-popup` with `-E -w -h -x -y -d -b <border> -T <title> -c <client> [cmd]`, what the installed fzf's `--tmux` sends (`list-commands new-pane` as its probe, `new-pane -P -F -t -c -x -y -X -Y` for a floating pane, `wait-for` and `wait-for -S`), and a nested `tmux attach-session -t <other>` inside it (§4.6); `display-menu -T -x -y` whose items may open further menus; `display-panes` (pick a pane by its number); `command-prompt -k` (read one key) and `-T <type>` (accepted, no completion); `list-keys -1N`; `choose-tree -Z -w` | pass-through; border style and title cosmetic | M3.6 |
| Hooks and environment | `set-hook -g`, `set-hook -gu`, `show-hooks`, for `after-select-pane`, `after-select-window`, `after-new-window`, `after-kill-pane`, `after-resize-pane`, `after-resize-window`, `client-resized`, `client-session-changed`, `session-created`, `session-closed`, `pane-exited`, `pane-died`, and since M3.7 tmux's three alert hooks with its window alert options; `run-shell -b`; `set-environment` with `-g`, `-u`, `-h` (and `-gu`); `show-environment -g -h` (§4.2) | hook names to grofe event subscriptions; environment flags pass through | M3.6 |
| Buffers and copy | `copy-mode -u -e -H -M`, `send-keys -X -F -N`; `set-buffer`, `show-buffer`, `list-buffers`, `delete-buffer`, `paste-buffer -p`, `choose-buffer`; `copy-pipe`, `copy-pipe-and-cancel`, `copy-pipe-no-clear` in the `copy-mode-vi` table, `command-prompt -T search`; `load-buffer -b -t -w` and `save-buffer -a -b`, paths the CLI's and `-` its stdin or stdout. `copy-mode -d -q -s -S` are refused (`docs/copy-mode.md`) | pass-through | M5, with copy mode (TM.7) |
| Formats | short forms `#S #I #W #h #P`; the inventory's variables (`client_name`, `client_tty`, `client_prefix`, `client_session`, `client_last_session`, `pane_tty`, `pane_title`, `pane_start_command`, `pane_in_mode`, `pane_mode`, `pane_left`, `pane_top`, `alternate_on`, `mouse_any_flag`, `window_layout`, `window_flags`, `window_panes`, `session_windows`, `session_grouped`, `pid`, `socket_path`, `mouse_status_range`, `history_size`, `version`, and grofe's own `grofe_version`); modifiers `#{!=:}`, `#{>:}`, `#{>=:}`, `#{&&:}`, `#{\|\|:}`, `#{m:}` (fnmatch), `#{m/r:}` (regex), `#{t:}`; conditions on bare names (`#{?#W,..}`, `#{?@opt,..}`); `##` and `##{` escaping; `#(cmd)` as a cached source (D8) | short forms and `##` rewritten; `#(cmd)` to a cached source; modifiers native | M3.6 (`copy_cursor_word`: M5) |
| Status line | `status 2` with `status-format[N]` rows; `status-position top`; `status-justify`; `status-interval`, which also drives sources; `status-left-length`, `status-right-length`; `window-status-format`, `window-status-current-format`, `window-status-separator`; styles `#[range=user\|<name>]..#[norange]` (a click sets `mouse_status_range`) and `#[align=left\|centre\|right]`; a repaint within one frame when an option the status reads changes | tmux option names onto native status options | M3.6 (one row, window formats, strftime: M2) |
| Terminal passthrough | `DCS tmux;` wrappers, which apps send once `$TMUX` is set, are unwrapped: a wrapped APC `_G` is handled as a placeholder-mode kitty graphics command (§4.9), any other wrapped sequence the bare path acts on is handled exactly as if sent bare, anything else is dropped (§4.9) | none: native pane-output handling, not argv | M3.6 |

**Out of scope for compatibility**, each with its reason:

| Item | Why not |
|---|---|
| `{mouse}`, `{last}`, `{marked}` target tokens, `!`, `+`/`-` offsets, fnmatch target patterns | Not in the owner's scripts. Mouse bindings act on the pane under the pointer without a token; `=name` covers exact matching |
| `link-window`, `unlink-window`, session groups | A window belongs to one session in grofe. sessionx's unlink flow is replaced in the port; `session_grouped` is always 0 |
| `%if`, `%hidden` and `NAME=value` lines in a sourced tmux file; parsing `list-keys` output in tmux's format | Config is ported once (D1); a plugin's own tmux file is read (§5.1), but none the owner runs uses them. Only sensible and TPM parse `list-keys`: TPM does not run on grofe, and sensible runs unchanged against the tmux dialect's `list-keys` (plan TP.4) |
| tmux's `-C` and `-CC` control mode | Not shipped in v1; nothing the owner runs uses it; possible M8. grofe's own control mode (§4.6) is always grofe dialect |
| Marked panes: `select-pane -m`/`-M`, the stock `m` binding | The mark only feeds `{marked}` targets and `-s` defaults, which the owner's scripts never use; `m` stays unbound and TP.9 lists it |
| Mouse argv forms: bare `=` target, `send -M`, `resize-pane -M`, `display-menu` position letters `M`, `W`, `S` | Only tmux's stock mouse bindings use them; grofe's built-in mouse model replaces those bindings (bindings row) |
| `pipe-pane`, `lock-server`, `clock-mode`, `customize-mode`, `server-access` | Nothing the owner runs sends them |
| Format modifiers `#{s/../../:}`, `#{=N:}`, `#{l:}`, `#{E:}`, `#{T:}` and the loops `#{S:}`, `#{W:}`, `#{P:}` | Not in the owner's formats; each is added when a ported script needs it (`#{q:}` was, for claude-session-manager, in M3.7) |
| Sockets under tmux's own directory, or scripts that build `/tmp/tmux-$UID/...` by hand | grofe keeps its socket directory (§4.1, D12); the owner's scripts use `$TMUX` or `#{socket_path}` |

**Window names:** `automatic-rename` is on by default and names a window
`#{b:pane_current_path}`. An OSC title from a pane is relayed
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
{"grofe": 1, "mode": "control", "dialect": "grofe", "cwd": "/home/u/src", "pane": "%3", "env": {"SSH_AUTH_SOCK": "..."}}
{"grofe": 1, "mode": "attach", "dialect": "tmux", "cwd": "...", "pane": null, "env": {...},
 "cols": 200, "rows": 50, "term": "xterm-ghostty", "caps": {"truecolor": true, "styled_underline": true, "sync": true, "kitty_keys": true, "kitty_graphics": true},
 "target": "main", "detach_others": false, "nest": false}
```

`pane` is `$GROFE_PANE` when set. `dialect` is `tmux` when the CLI was invoked
as `tmux` (D12), as in the attach example from `tmux attach`, and defaults to
`grofe`. **Nested attach:** attaching from inside a
grofe pane or popup is allowed when the target session is a different one;
the owner's claude-session-manager binding runs `tmux attach-session -t <agent
session>` inside a popup, and detaching returns to the popup's caller. Only
true self-nesting is refused unless `--nest` is given (grofe's own long
flag; `-f` stays tmux's client-flags option, for the tmux front end):
attaching to the session
that contains the calling pane (for a popup, the session of the client that
opened it), which would draw the session inside itself. That is grofe's
own rule. A tmux-dialect attach follows tmux 3.7b's instead (recorded,
M3.7 TP.10): a client with `$TMUX` set whose terminal is one of the
server's panes' is refused to any session, with tmux's message, and any
other client attaches, so a popup's `tmux attach-session -t` to its own
session shows the session nested in the popup.

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
The server answers the attach hello with exactly one JSON reply line (`{"id": 0, "ok": true, "out": ""}`, or `ok: false` with `err` for a refusal or a version mismatch, after which it closes), and only after an ok line do both sides switch to binary frames.

| Direction | Kind | Payload |
|---|---|---|
| client → server | `input` | raw bytes from the tty |
| client → server | `resize` | `u16 cols, u16 rows` |
| client → server | `detach` | none |
| server → client | `output` | bytes to write to the tty |
| server → client | `exit` | UTF-8 reason (`detached`, `server exited`, `session killed`) |
| server → client | `suspend` | none: give the terminal back and stop with SIGTSTP (`suspend-client`), then send a `resize` once continued |

Protocol version mismatch is a clean error naming the side to upgrade (§7.3).
M6 freezes the protocol as v1 under the rules in §7.3; until then it is
internal and may change between commits.

**Events:** a control client receives events after `subscribe`. §7.2 holds
the catalog (each event's payload, emitter and consumer), the `subscribe`
filters and the delivery rules; an event enters it only with a named
consumer. tmux-style hooks (`set-hook`, M3.6) run commands on the same
internal events, under the tmux hook names in §4.5. That is a command
surface, not the plugin API (D9), and its delivery rules are tmux's: per
origin queues, the `#{hook_*}` variables, and no `after-*` hook for a
hook's own commands.

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
  agent panes are identified from argv (canopy 5aaac53). The cwd,
  `#{pane_current_path}`, comes only from the foreground process, as tmux
  3.7b's does (`osdep_get_cwd`): it is read when a format asks for it, and
  the sample is the fallback. What a pane reports with OSC 7 is never its
  cwd. From M3 grofe's own OSC byte scanner (T3.3, §4.9; `vte` 0.15 does
  not parse OSC 7) reads it and emits `PaneEvent::Path`, which only sets
  `#{pane_path}` to the text as sent, as tmux shows it (TM.5, TM.7): never
  decoded, C0 bytes dropped, a backslash doubled, and text that is not
  UTF-8 or holds a DEL ignored. Unlike tmux, which keeps it, a report
  holding a C1 control character (U+0080 to U+009F) is ignored whole, so
  a pane cannot put a CSI into a format.
  The same scanner also frames kitty graphics commands (APC `_G`, §4.9),
  which `vte` 0.15 drops.

### 4.9 Passthrough

The emulator consumes every byte, so nothing passes through implicitly. grofe
re-emits exactly this set to attached clients: OSC 52 clipboard (per config),
window title, bell (also sets the window activity flag, which agents rely on),
OSC 8 hyperlinks (re-emitted from cell hyperlink attributes), and OSC 9 / OSC 777
notifications. `vte` 0.15 does not parse OSC 9 or OSC 777, so those come from
the same grofe OSC byte scanner that reads OSC 7 (§4.8), run over pane output
before the parser sees it. Unsupported: arbitrary DCS passthrough (a nested
tmux works as a normal terminal app, without passthrough). Kitty graphics
direct placement, animation and sixel are opt-in (the `graphics` option, G3,
below). The one exception is the `DCS tmux;` wrapper that
apps send once `$TMUX` is set (D12): it is unwrapped and its content handled
exactly as if sent bare (§4.5) [M3.6]. A wrapper is let out only when its
content is nothing but complete sequences the bare path acts on (OSC 0, 2,
7, 8, 9, 12, 52, 112 and 777, BEL, kitty graphics APC `_G`), each then
meeting its bare filters; anything else, CAN and SUB included, drops the
whole wrapper, as does content past tmux 3.7b's 1 MB input buffer. So a
wrapped sequence never reaches a client unless the same bytes sent bare
would. `allow-passthrough` is accepted with tmux's values and changes
nothing.

**Inline images: kitty graphics via Unicode placeholders.** grofe
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

**Direct placements, animation and sixel (G3, opt-in).** With the `graphics`
option on (grofe's own, off by default; TOML `graphics = true`) the placements
that draw at screen positions are tracked per pane and drawn per client.
`docs/graphics-placement.md` is the design; in short:

- **Store.** Each pane keeps its placements in the emulator, anchored to
  absolute rows (the viewport's first row has an id that scrolling advances),
  so a placement follows its text up and into the scrollback and is freed
  when it is further up than the history. The parser's handler tells the
  store about every scroll, erase and write; resize, the alternate screen
  (an empty layer of its own) and `RIS` have rules of their own.
- **Kitty.** `a=T`/`a=p` without `U=1` are placed at the cursor (size from
  `c`,`r`, or the image's pixels over the pane's cell size) and move the
  cursor as the protocol says, unless `C=1`; relative placements (`P`,`Q`,
  `H`,`V`) follow their parent; `z` is kept; every `d=` key of `a=d`, in both
  cases, is applied (an upper-case one also frees an image no placement uses
  any more). The transmit goes to clients as a plain `a=t`; frames and
  animation control (`a=f`, `a=a`, `a=c`) are forwarded with the remapped id
  and replayed after it to a later client. Each frame the renderer gives a
  client the placements clipped to their pane and to its screen, as `a=p`
  with a source rectangle; one a popup, menu, prompt, chooser or floating pane
  covers is deleted whole; a full redraw deletes and puts them all again.
- **Sixel.** `DCS q` is parsed as tmux 3.7b does (a port of its
  `image-sixel.c`), stored at the cursor, which goes to column 0 of the row
  below, scrolling the screen if the image does not fit; freed by the writes
  tmux frees it by; sent to a client with the `sixel` capability (DA1
  attribute 4, or a matching `terminal-features` entry) re-encoded for its
  cell size and cut to what is visible. A client without it gets tmux's text
  stand-in, as tmux does without cell pixels. The goldens under
  `tests/fixtures/compat/golden/g3/` come from a tmux built with sixel.
- **Off,** nothing above runs and clients get what T3.8 sent.

### 4.10 Status line

Formats over options only (D8); the tmux-compatible breadth is listed in
§4.5. Beyond `status.left` and `status.right`:

- `window-status-format` and `window-status-current-format` render the window
  list.
- `#{}` expands inside `#[...]` style directives, so themes are data:
  `#[fg=#{@theme_blue}]` (canopy 1aa91b4).
- strftime sequences (`%H:%M`) expand, so a clock needs no subprocess.
- Battery, network, pet and canopy-status come from status sources (D8). A source is
  declared in config (`[status.sources.<name>]` with `cmd`, `interval` and
  `timeout`, its value in `@<name>`) or inline as `#(cmd)` in a format (its
  value in `@src-<hash of cmd>`). A declared source shows in `show-config
  [--explain] [status.sources[.<name>]]` with the file that set it; an
  inline one shows only as the format that holds it, and is dropped once
  no format has asked for it for an hour, as tmux drops an unused job.
  Sources run with the server
  environment (§4.2), each spawn under the pane spawn lock (§4.8), and a
  changed value triggers a repaint.
- Multi-row status (`status 2` with `status-format[N]`; canopy uses two rows at
  the top) and `status-position top`.

### 4.11 Copy mode and buffers

The owner's `copy-mode-vi` table is the scope: 95 bindings, tmux 3.7b's 88
stock rows plus 7 that canopy's navigator and tmux-yank add (they also
replace 3 stock rows). A binding outside it needs a dogfooding reason. The
scope doc (`docs/copy-mode.md`, from TM.0) lists each binding with the test
that pins it against a golden recorded from tmux 3.7b.

- **Per pane.** Copy mode is a pane mode, entered with `copy-mode` (`-u` a page
  up, `-e` leave at the bottom), `prefix [`, `prefix PageUp` or the wheel. The
  view keeps its place while output continues. Keys go through the
  `copy-mode-vi` table, whose commands are `send-keys -X <command>` (`-N`
  count, `-F` a format argument), as in tmux, so the owner's bindings keep
  their argv. `mode-keys` stays `vi` (§5.3); the emacs table is not shipped.
- **Selection** runs over logical lines: soft-wrapped rows join into one line,
  hard newlines stay, trailing blanks are trimmed, wide cells are copied
  whole. Rectangle, line and word selection (`word-separators`) work as in
  tmux, and a selection survives a resize that reflows it.
- **Search** is by regular expression (`search-forward`, `search-backward`),
  with plain `-text` variants, `n` and `N`, `#` and `*` on
  `#{copy_cursor_word}`, `wrap-search` and match styles; the prompt is
  `command-prompt -T search`. Patterns are read as tmux's POSIX extended
  syntax (glibc's) and run with `regex-automata`, taking the leftmost-longest
  match as POSIX does; the scope doc lists where it still differs from tmux.
  A search never runs on the actor.
- **Buffers.** Named and automatic buffers under `buffer-limit`; `paste-buffer
  -p` adds bracketed-paste markers only for a pane that asked for them.
- **Copy targets.** `copy-pipe`, `copy-pipe-and-cancel` and
  `copy-pipe-no-clear` run their command (or `copy-command`) off the actor with
  the server environment and the selection on stdin, and each also stores a
  buffer. Under `set-clipboard on` or `external` (the default, as in tmux) a
  copy also goes to attached clients as OSC 52 (§4.9). A pane's own OSC 52
  becomes a buffer and reaches the clients only under `on`.
- **Mouse.** The wheel in a pane without mouse mode enters copy mode, replacing
  M3's read-only scrollback view; a drag selects; `MouseDragEnd1Pane` runs its
  binding; a double and a triple click select a word and a line, as the owner's
  root and copy-mode bindings say.

---

## 5. Config

### 5.1 Files and layering

Load order, later wins:

1. built-in defaults (compiled in, the same schema)
2. `~/.config/grofe/config.toml`
3. `~/.config/grofe/conf.d/*.toml`, lexical order
4. `~/.config/grofe/user.toml`, the user's final word
5. runtime `set-option` commands

Tools such as canopy write only `conf.d/NN-*.toml`; `config.toml` and
`user.toml` belong to the user and no tool writes them. `user.toml` loads after
every generated fragment, so the user's own settings always win over a tool's.
There is no scripting layer: the Lua `init.lua` layer was dropped with the
Lua runtime (owner ruling 2026-09-29). Plugins (§7.4) are not a layer
either. A plugin is declared in layers 1 to 4 like any setting, and changes
settings only by running commands, which land in layer 5.

`source-file <path>` loads another TOML file at the current layer. A relative
path resolves against the directory of the file that includes it, never the
server's cwd or environment. At runtime, `source-file` of a file not named
`*.toml` reads it as a tmux config through the tmux front end,
as tmux does, but only when the request is tmux's: one in the tmux dialect,
or any request to a server with tmux identity (D12). tmux plugins source
their own `*.tmux` and `*.conf` files that way (M3.7). A native request to a
grofe server reads such a file as TOML. Sourced tmux files nest at most 50
deep, as in tmux 3.7b; the next is `too many nested files`. `reload` (bound
to `prefix r`) re-reads layers 1 to 4 and applies the diff (§5.4).

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
`history-limit = 50000`, focus events on, true colour on, extended keys on,
`renumber-windows = true`, `automatic-rename = true` (§4.5),
`restore-on-start = true` (§6), `mode-keys = "vi"` (M5), new panes and windows
inherit the active pane's cwd, and the status line never forks on redraw (D8).

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

### 5.5 Importing a tmux.conf

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
- **With M3.6** (plan TP.0) the converter learns what M3.6
  adds, since the owner's port is drafted with it before alias day: `#()` becomes a status source,
  `if-shell`, `set-hook`, `set-environment`, `command-alias`, custom key
  tables, mouse bindings, `display-popup`/`display-menu` bindings and
  `status 2` rows convert. Plugins and TPM stay reported.

---

## 6. Persistence and pane identity (M4)

- **Snapshot.** The server writes `$XDG_STATE_HOME/grofe/<instance>/state.json`,
  where `<instance>` is the socket's name plus a hash of its path, as for the
  log (§4.1), so every socket has its own. The file has a `version` field. It
  is written on change: a structural change (sessions, windows, panes, names,
  layout), a cwd change (the sampling timer), a resume set or
  cleared, and a `--persist` option set or unset. Writes are debounced ~1 s and
  skipped when the content is unchanged, and they are atomic (a temp file in
  the same directory, fsync, rename; mode `0600`). Snapshots are server-driven
  and never depend on an attached client (continuum's did).
- **Contents.** Per session: name, cwd and active window. Per window: index,
  name, whether it renames automatically, size, layout geometry (cells keyed by
  pane `uid`), active pane and zoom. Per pane: `uid`, `cwd`, spawn `argv` and
  `resume`. Every option set with `--persist`, at any scope. Runtime ids are not
  saved, and neither is any environment: a session's environment can hold
  secrets, and a restored session takes the restoring server's.
- **Ending.** `kill-server`, SIGTERM and SIGHUP write the state as it was
  before shutdown, once, and then freeze the writer: a reboot or logout signals
  every process in the session, so panes die and sessions close after the
  server's signal, and none of that may reach the file. Killing the server and
  restoring therefore brings everything back. Only a normal exit of the last
  pane, with no termination signal pending, writes the empty state: closing
  everything is a choice the next start respects. A crash leaves the last
  debounced write.
- **Versions.** A snapshot whose `version` this grofe does not know is renamed
  to `state.json.v<N>` before the server's first write, so an older grofe
  never overwrites a newer one's state; an unreadable one is renamed
  `state.json.corrupt-<time>`. Both are logged, and the server starts empty.
- **What persists.** Identity-like options (`@agent_session`, `@agent_source`
  style) are set with `--persist`. State-like options (`@agent_state`) never
  are: after a restore an agent plugin should show a "restored" state until
  its adapter reports, because a stale `working` is a lie. The mark is sticky per
  option: a later `set` without `--persist` keeps it (canopy's report path sets
  values without the flag), and only `set --no-persist` or unsetting the
  option clears it.
- **Restore.** `grofe start-server --restore`, or `restore-on-start = true` in
  config (the default), recreates everything from the snapshot, and only into an
  empty server, so restoring twice cannot duplicate sessions. A server that
  restores on start does so before it reports ready, so the command that
  started it runs against the restored sessions (`new-session -A -s main`
  attaches the restored `main`). Each pane runs `resume` if set, otherwise a
  shell in `cwd` (the home directory when `cwd` is gone). When a process that a
  restore started from a resume exits, or cannot be started, the pane respawns
  as a shell in `cwd` with the same `uid`, so the layout does not collapse;
  other panes follow `remain-on-exit`. `uid`s are preserved; `#{pane_restored}`
  marks panes that came from the snapshot. A restored session was created by no
  client, so its `update-environment` variables start unset and are filled by
  the first client that attaches, as every attach refreshes them (§4.2): panes
  created after that get them, and panes already running keep what they
  started with.
- **Resume.** `pane-set-resume -t <pane> -- <argv...>`, and `-u` clears it. The
  agents plugin was to call it for an adapter whenever the adapter reports a
  new session id (§7); with M7 dropped (owner ruling 2026-10-06), canopy's
  adapters or a tmux plugin call it instead, which covers canopy's tier 1 and
  tier 2 agents without a save-time rewrite hook. A `before-save` hook is added only if dogfooding finds
  a case this cannot handle. Neovim is the second resume client
  (`pane-set-resume nvim -S <session>`), so the mechanism is not agent-shaped. A
  pane's resume is also set by the command it was spawned with and by
  `respawn-pane <cmd>`. `#{pane_resume}` shows it.
  - `argv[0]` is resolved to an absolute path, never against the server's
    `PATH`, so a server started at boot with a minimal `PATH` can still run it
    (canopy ef20852). A caller with pane context (a CLI inside a pane) is
    resolved against its own `PATH`, which every hello carries (§4.6). A caller
    without one, such as a supervised plugin, whose environment is the server's,
    is resolved against the target pane: its foreground process's own `PATH`
    first, then the pane's spawn environment.
  - A resume is bound to the process that set it: the pane's foreground process
    group leader at set time, recorded by pid and start time so a reused pid
    does not count. When that process exits, the resume is cleared; a stopped
    (suspended) process keeps it. Closing the agent and keeping the shell
    therefore does not resurrect the closed conversation on the next reboot. A
    spawn command or a `respawn-pane` command binds to the pane's root process.
  - `pane-set-resume --expect <reason>` records that a pane should have a resume
    but cannot name one yet ("no session id reported"), bound the same way and
    shown by `#{pane_resume_expected}`. Expectations are not saved.
- **Stagger.** Restoring many panes spreads their spawns with a deterministic
  per-pane delay, the FNV-1a hash of the `uid` modulo `restore-stagger-ms`
  (default 1000; 0 means exactly off). Only panes with a `resume` are staggered,
  and the delay is applied by the server, never embedded in the argv (canopy
  19fab4e, d0c7bea).
- **`grofe reboot-check`** reads the snapshot on disk and compares it with the
  live model; it never reports in-memory intent. A pane expects a resume when it
  has a resume or an expectation. For those panes the verdict is "will resume"
  (the snapshot holds the same resume for its `uid`), "will restart without its
  conversation" (saved without it, with an older one, or only expected, with the
  reason), or "not saved yet" (the snapshot lacks the `uid`, or there is no
  snapshot). When restore is off, every pane is "will not be restored". Panes
  that expect nothing are listed as "restores as a shell" and never change the
  status. Exit status: 0 every pane that expects a resume will resume (or no
  server runs), 1 some will not, 2 persistence is misconfigured: no
  `restore-on-start` and no autostart unit (a systemd user unit or launchd agent
  whose command runs `start-server --restore`), or a state directory that is not
  writable (canopy 1bc0255, ab37bf0).
- **Out of scope:** scrollback persistence and saving environments. Login
  autostart belongs to canopy on grofe (§7.1).

---

## 7. Plugins (M6), agents (dropped), canopy on grofe

**Plugins run out of process.** A plugin is any program, in any language,
that the server starts from a `[plugins.<name>]` table (§7.4) and
supervises. It speaks control mode (§4.6) over the socket `$GROFE` names,
or over stdio through `grofe control`, which bridges JSON lines in and out
so a plugin needs no socket code. It acts with ordinary commands and learns
what happened through `subscribe` (§7.2). There is no in-process scripting:
the Lua runtime was dropped (owner ruling 2026-09-29), and with it
`init.lua`, Lua key bindings and format functions. A key binding runs a
command or `run-shell`, and dynamic status content comes from options and
status sources (D8). The event catalog (§7.2) and the protocol (§7.3) are
frozen as v1 in M6 and change only by addition after that.

**An event enters the catalog only with a named consumer:** a tmux hook name
row (§4.5) that a plugin the owner runs binds, the in-tree event-log plugin
(plan T6.6), or a test. The catalog below was derived on 2026-10-27 (plan
T6.0) from TC.5's event list (`EVENTS` in `src/server/hooks.rs`), the
existing `subscribe` stream (`src/event.rs`, `src/server/events.rs`), the
hooks the owner's plugins bind, and `docs/dogfood.md`. Of those plugins,
opensessions binds twelve of TC.5's tmux hook names (every one but the three
alert hooks), and canopy's copy of tmux-claude-session-manager binds
`alert-bell`; the rest bind none. Neither the dogfood log nor the
`compat-miss.log` reviews named an event or hook grofe lacks; one that does
later enters by §7.3's additive rule. The owner approved this rewrite
(plan T6.0) on 2026-10-27. The agents part of this section was M7's.

**M7 is dropped (owner ruling 2026-10-06).** The three bullets marked
*[dropped]* below (the agents plugin, resumes from it, the adapter contract)
are kept as a record and are not built. Agent awareness (state rollups,
attention, resume, adopt) is left to existing tmux plugins run unchanged
through canopy's loader, such as tmux-claude-session-manager and
opensessions, and to canopy's own adapters and strategies. A grofe gap such a
plugin finds shows up in `compat-miss.log` and is fixed as a compat bug.

- *[dropped]* **agents plugin:** a supervised plugin that ships in the grofe binary
  (`grofe __agents`). The five normalized states (`idle`, `working`, `blocked`,
  `completed`, `exited`) live in the pane option `<prefix>state`. The prefix is
  `@agent_` by default, and canopy's fragment sets `@canopy_agent_`, so canopy's
  report path (`tmux set -p @canopy_agent_state ...`, answered by grofe, D12)
  stays the whole write path for adapters. The plugin validates states and
  writes rollups as window and session options, on transition only, holding the
  most urgent state in this order: `blocked`, `completed` while unseen,
  `working`, `restored`, `idle`, `completed`, `exited`. Entering `blocked` or
  `completed` while no attached client is looking at the pane sets
  `<prefix>unseen`, and `pane-focus` clears it; `grofe agents next` jumps to the
  oldest unseen pane. After a restore, panes with a persisted `<prefix>source`
  show `restored` until their adapter reports. The plugin forks nothing per
  report.
- *[dropped]* **Resumes come from the plugin.** It calls `pane-set-resume` for the adapter
  whenever a pane's reported session id changes, not only when its state does:
  `/clear` gives a new id without one (canopy c15f63e). For panes that never
  report, it reads the id off `pane_current_argv` through the adapter's
  templates, skipping interpreters, options and `VAR=value` (canopy 5aaac53);
  it never trusts the command name. A pane running an agent with no id gets an
  expectation (§6), which `reboot-check` reports. It sets `<prefix>source` and
  `<prefix>session` with `--persist`, never `<prefix>state`. The `exited` state
  stays visible: a restored agent pane respawns to a shell (§6) and keeps its
  options.
- *[dropped]* **Adapter contract [was planned for M7].** canopy's `docs/adapters/contract.md` is
  the authority; grofe reads the same manifest files, found in the `[agents]
  adapter-dirs` list (canopy's fragment names its config directory, then its
  store; the first wins). All six keys are required, each validated on every
  read:

  | Key | Meaning |
  |---|---|
  | `id` | equals the directory name; charset `[A-Za-z0-9._:-]`, validated on entry and again on use (canopy 94435f6) |
  | `command` | the agent's program name, non-empty, matched against the pane's argv, never its comm |
  | `resume_template` | argv that resumes a conversation; contains `{id}` |
  | `launch_template` | argv that starts a new conversation at a chosen id; must contain `{id}` when `can_pin_at_launch` is `yes`; may be empty (and is not checked) when `no` |
  | `can_pin_at_launch` | `yes` or `no`: whether the adapter can choose the session id at launch |
  | `detect_draft` | command reporting unsent input, which must contain `{pane}` unless empty: exit 0 has a draft, 1 none, anything else undetermined; empty means the adapter cannot answer |

  grofe turns a template into an argv by substituting a validated id and
  splitting on spaces; a template with shell quoting or metacharacters is
  rejected. Adapters are tiered 1/2/3 as in canopy, and a tier 3 agent gets no
  adapter. The four are canopy's `claude-code`, `opencode`, `pi` and `codex`.
  Hooks always exit 0 and stay cheap: a Claude Code `PreToolUse` hook that
  exits 2 blocks the tool.

### 7.1 canopy on grofe

canopy's scripts keep calling `tmux`, answered by grofe (D12), so they need no
port, with one exception: `canopy validate` starts a throwaway tmux server with
`-f /dev/null ... source-file <tmux.conf>`, which is out of scope (§4.5) and is
replaced by `show-config --file` in the port (plan TP.1). canopy's config moves
once to a grofe backend in plan M3.6, before alias day; the owner's remaining
TPM plugins run unchanged through canopy's loader from M3.7. The agents
plugin and adapter contract were M7's, which is dropped (owner ruling
2026-10-06), so the rows below marked *[dropped]* stay with canopy and its
plugins rather than grofe.

| canopy piece | On grofe |
|---|---|
| conf.d layers | canopy writes only `~/.config/grofe/conf.d/NN-canopy-*.toml`. The restore-point promise becomes trivial: canopy removes its own files |
| user.conf last | `~/.config/grofe/user.toml`, the final file layer (§5.1); `config.toml` is also the user's |
| caps | grofe has no `%if`; canopy generates capability-gated conf.d fragments itself and marks them with `requires` (§5.2) |
| themes | generated conf.d TOML using `status.style` and `pane-border` options; needs `#{}` inside `#[...]` (§4.10) |
| status aggregator | clock native via strftime (§4.10); canopy-status, pet, network and battery as named status sources (D8, M3.6), two rows at the top |
| keys.tsv | generated TOML bindings with `name`, `desc`, `group`, `repeat`, `send-prefix`, `;` sequences and `select-layout main-vertical` |
| validate (throwaway tmux server) | `grofe show-config --file`, no server (§5.4) |
| doctor, load-bearing settings | `show-config --explain` checks. The load-bearing checks become: `restore-on-start` or an autostart unit present, state dir writable, snapshot fresh. canopy's tmux ones (continuum hooks in `status-right`) no longer exist |
| resurrect, continuum, save strategy | run unchanged as plugins through canopy's loader from alias day (owner ruling 2026-09-30); M4 imports resurrect's newest save into grofe's state, then retires both |
| `reboot-check` | core (M4) gives each pane's verdict from the snapshot on disk; an expectation set with `pane-set-resume --expect` adds the reason ("no session id reported"); `canopy reboot-check` on grofe prints `grofe reboot-check` and exits with its status |
| `adopt` | *[dropped]* `grofe agents adopt` (M7), which `canopy adopt` calls on grofe. No respawn: adopt only calls `pane-set-resume` on the live pane, so the unsent-input guard is unnecessary for adopt itself. Keeps dry-run by default and idempotence, never invents an id, and treats a pane already carrying its resume as adopted |
| rollups, attention, render path | *[dropped]* agents plugin options, rendered by canopy's `40-agents` fragment with formats only, no `#()` (canopy §6.3) |
| inline images (`allow-passthrough on`) | an agent's kitty graphics in placeholder mode pass through natively (§4.9), with no passthrough option; direct placement, animation and sixel pass with `graphics on` (G3), and without it those programs fall back to whatever they draw without graphics |
| palette, which-key, key search | `display-popup -E` + fzf and `display-menu` (M3.6), backed by `list-keys` descriptions |
| login autostart | canopy ships the launchd/systemd unit running `tmux start-server --restore` through the `tmux` symlink, so the server starts with tmux identity (D12); the unit gives panes a real `PATH`, or relies on `argv[0]` resolution (§6); `reboot-check` finds the unit (§6) |

**Consciously dropped from canopy:** vendoring resurrect and continuum, and the
relative-path strategy hack (canopy 495df6b); the `@resurrect-processes` regex
(canopy d0c7bea); the stagger wrapper inside the command string; "status-right
must load before continuum"; continuum's other-server arithmetic; `%if`
duplicate-append guards on reload (canopy 6ef854a); throwaway-server validation
with `sleep 600` and `env -i`; the M-s/M-r key relocation; the five-minute save
interval (grofe snapshots on change). **Kept on purpose:** reading the session
id off a process argv for hand-started panes.

### 7.2 Event catalog

Every event is one JSON line on a subscribed connection: an object whose
`event` field is the event's name, followed by its payload fields. Ids are
runtime ids (`%N`, `@N`, `$N`, §4.2); `uid` is the pane's stable uid;
`client` is the client's name as `#{client_name}` shows it. A field the
table marks nullable is always present, as `null`, never omitted. An event
names what it is about by id and carries little else; a subscriber that
needs more asks with a command (`display -p -t <id> <format>`), which keeps
payloads small and stable under §7.3.

| Event | Payload (after `event`) | Fires when | Emitter (`src/`) | First consumer |
|---|---|---|---|---|
| `pane-exited` | `pane`, `uid`, `status` (exit code; null when a kill removed the pane while its process ran) | a pane's process exited, whether or not `remain-on-exit` keeps the pane; or a kill removed a live pane (`kill-pane`, `kill-window`, `kill-session`, `kill-server`, the structure verbs) | `handle_pane_event` in `server/mod.rs` for an exit; `Events::panes_killed` in `server/events.rs` for kills (existing, T1.6) | T1.6's `tests/events.rs`; opensessions' `pane-exited` and `after-kill-pane` hooks, which share one handler |
| `pane-died` | `pane`, `uid`, `window`, `status` | a pane's process exited and `remain-on-exit` kept the pane; after that pane's `pane-exited` | `hooks::pane_exited` in `server/hooks.rs`, the kept branch | opensessions' `pane-died` hook; `tests/compat/fe_3.rs` |
| `pane-output` | `pane`, `data` (base64, standard alphabet) | a pane's reader read output, only while a subscriber names this event | `handle_pane_event` in `server/mod.rs` (existing, T1.6) | T1.6's `tests/events.rs` |
| `pane-focus` | `pane`, `window`, `session`, `client` | a pane becomes the active pane of the current window of an attached client (`select-pane`, a click, a window or session switch, a split or kill that moves the active pane, an attach), or that client's terminal regains focus while the pane is active; never for a session with no attached client | new (T6.1), at one site: see below | T6.1's `tests/events_v1.rs`; the event-log plugin (T6.6); on the stream it stands in for opensessions' `after-select-pane` handler |
| `pane-resized` | `pane`, `window` | `resize-pane` succeeded | `hooks::run_command`, `Before::ResizePane` | opensessions' `after-resize-pane` hook (it repairs its sidebar's width) |
| `window-focused` | `window`, `session`, `pane` (the window's active pane) | `select-window` succeeded, even onto the current window | `hooks::run_command`, `Before::SelectWindow` | opensessions' `after-select-window` hook |
| `window-created` | `window`, `session`, `pane` (its first pane) | `new-window` made a window | `hooks::run_command`, `Before::NewWindow` | opensessions' `after-new-window` hook |
| `window-resized` | `window`, `session` | `resize-window` succeeded | `hooks::run_command`, `Before::ResizeWindow` | opensessions' `after-resize-window` hook |
| `session-created` | `session`, `name` | `new-session` made a session | `hooks::session_created`, called from `server/dispatch/session.rs` | opensessions' `session-created` hook |
| `session-closed` | `session`, `name` | a session was removed (killed, or its last pane went) | `hooks::run_pending`, draining `closed_sessions` from `server/model.rs` | opensessions' `session-closed` hook |
| `client-session-changed` | `client`, `session` | a client attached, or `switch-client` or `attach-session` ran for it, even to the session it already showed | `hooks::client_session_changed`, called from `server/clients.rs` | opensessions' `client-session-changed` hook |
| `client-resized` | `client`, `session`, `cols`, `rows` | a client attached, or its terminal reported a size, even an unchanged one | `hooks::client_resized`, called from `server/clients.rs` | opensessions' `client-resized` hook |
| `alert-bell` | `window`, `session`, `pane` (the window's active pane) | a bell in a monitored window, when `bell-action` applies | `hooks::window_alert`, called from `server/alerts.rs` | canopy's copy of tmux-claude-session-manager, whose `alert-bell` hook forwards the bell (`tests/compat/tp_10.rs`) |
| `alert-activity` | `window`, `session`, `pane` | output in a monitored window, when `activity-action` applies | `hooks::window_alert` | its tmux hook name row (§4.5); `tests/alerts.rs` |
| `alert-silence` | `window`, `session`, `pane` | no output in a monitored window for `monitor-silence` seconds | `hooks::window_alert` | its tmux hook name row (§4.5); `tests/alerts.rs` |
| `option-changed` | `scope` (`global`, `$N`, `@N` or `%N`), `name`, `value` (nullable: null once unset) | an option's value changed; setting the current value emits nothing (§4.2) | `server/dispatch/options.rs`, and `server/sources.rs` for status sources (existing) | T1.6's `tests/events.rs` |
| `events-dropped` | `count` | a subscriber's queue overflowed; sent before its next delivered event | `Subscriber::offer` in `server/events.rs` (existing) | T1.6's `tests/events.rs`; a notice, not subscribable: every subscriber can receive it |

**Rules for emitting.**

- **One internal event, two surfaces.** Each row TC.5 already raises is
  emitted where TC.5 fires its hook, but before the check for a bound hook,
  so a subscriber gets it whether or not any hook is set. It fires for
  commands a hook, a binding or a plugin runs too: that a hook's own
  commands fire no `after-*` hook is tmux's recursion guard for hooks, not
  a filter on events. Each event has one emission site (T6.1); the two
  existing multi-site rows, `pane-exited` and `option-changed`, keep theirs.
- **grofe's names, not tmux's.** The catalog uses the grofe event names of
  `EVENTS`; tmux's hook names map onto them for `set-hook` only (§4.5).
- **Where the stream differs from the hooks, on purpose.** T1.6's tests pin
  the stream's `pane-exited`: it covers every way a pane's process ends or
  a live pane goes, where tmux's `pane-exited` hook fires only for an exit
  that removed the pane. Each pane process gets at most one `pane-exited`:
  killing a pane whose process already exited (kept by `remain-on-exit`)
  emits no second one, and `respawn-pane` starts a process that can exit
  again. T6.1 pins this with a test, including a pane that `remain-on-exit`
  kept and that is then killed. Two of TC.5's event names stay hook-only for that reason.
  `pane-closed` (tmux's `after-kill-pane`) is `pane-exited` with a null
  status on the stream, which is how opensessions treats it anyway.
  `pane-focused` (tmux's `after-select-pane`) is covered on the stream by
  `pane-focus`, which also catches every other way focus moves.
- **`pane-focus` at one site.** After each command sequence and each client
  input, the server compares each attached client's current window, active
  pane and terminal focus with what it last reported for that client, and
  emits on a change. So one action gives one event per client whose view
  changed, however many verbs it took, and a split followed by a
  `select-pane` gives two (T6.6's acceptance). A terminal focus report
  counts only while `focus-events` is on, as it does for hooks (§4.7).

**`subscribe <event>... [-t target]`.**

- Names are literal or `*` patterns. An unknown literal name is an error
  (`unknown event: <name>`); a pattern may match nothing.
- A bare `subscribe` and every pattern match the v1.0 catalog above except
  `pane-output`. `pane-output`, for its volume, and any event added after
  v1.0 (§7.3) reach only a subscription that names them literally.
- `-t` resolves to a pane, window or session when `subscribe` runs. An event
  passes when its `pane`, `window` or `session` field lies inside that
  target: a pane target passes that pane's events, a window target its own
  and its panes', a session target everything in the session.
  `option-changed` passes by its `scope`, and an event with none of those
  fields (`events-dropped`) always passes. A target that goes away leaves a
  subscription that passes nothing more.
- A later `subscribe` on the same connection replaces the earlier one.

**Delivery.** A subscriber receives events in the order they happened, so
per-pane events arrive in the order of their actions (a pane's
`pane-exited` before its `pane-died`, a session's panes' `pane-exited`
before its `session-closed`). Each subscriber has a bounded queue of 1024
events; on overflow events are dropped and `events-dropped` with the count
comes before the next one delivered. A slow subscriber never delays the
server or another subscriber (D8). Nothing is replayed: events while a
plugin is down or before it subscribes are gone.

**Considered and left out for lack of a consumer:** `session-renamed`,
`window-closed`, `window-renamed`, `layout-changed`, `pane-created`,
`client-attached`, `client-detached`, `config-reloaded`, and `bell` (which
`alert-bell` covers), from the earlier candidate list; and every tmux hook
the owner's plugins do not bind, such as `after-split-window`,
`window-linked` and `pane-mode-changed`. `restore-done` and `resume-changed`
join the catalog with M4 (owner ruling 2026-10-27: M6 runs before M4), as
additions under §7.3.

### 7.3 Protocol v1 rules

`docs/protocol-v1.md` and `tests/fixtures/protocol/v1/` (plan T6.2) are the
frozen record: both hellos, every request, reply, event and attach frame,
one JSON line per message and event kind. These rules govern every change
after the freeze.

- **Versions in the hello.** Every hello carries `"grofe": 1`, and the
  server speaks exactly one version; there is no negotiation. A hello with
  no integer `grofe` field is an error (`hello has no valid "grofe"
  protocol version`). A hello with another version gets one error line, in
  either mode, and the server closes the connection:
  `{"id": 0, "ok": false, "err": "protocol mismatch: this build speaks v1, the peer speaks v2; upgrade the server"}`.
  The side named is the one with the lower version: `upgrade the client`
  when the peer is older, `upgrade the server` when it is newer. `dialect`
  absent means `grofe`.
- **Additive only.** Within v1 a change may add: optional fields to a hello
  or a request, which default when absent; fields to a reply or an event;
  events, which reach only subscriptions that name them (§7.2); verbs, and
  optional flags on existing verbs. It may not remove or rename a field, an
  event or a verb; change a field's type, nullability or meaning; make an
  optional field required; add an attach frame kind or change a frame's
  payload; or reword the version-mismatch error. A change that rewrites an
  existing fixture line is not additive. Anything else is `grofe: 2`.
- **Unknown fields are ignored.** The server ignores fields it does not know
  in a hello or a request. A client or plugin must ignore fields it does not
  know in a reply or an event, so a v1 plugin keeps working against any
  later v1 server.
- **Unknown verbs error.** A request whose verb the server does not know
  gets `{"id": N, "ok": false, "err": "unknown command: <verb>"}`. The
  connection stays open and later requests run. An unknown event name in
  `subscribe` is an error the same way (§7.2). An unknown `mode` in a hello
  is a malformed hello and closes the connection.
- **Shape.** A reply echoes its request's `id` and carries `ok` with `out`,
  or `ok: false` with `err`. Replies to one connection's requests come in
  request order; events carry no `id` and interleave between replies.
- **Attach frames** are the six kinds in §4.6, frozen. An unknown frame
  kind closes the connection.

### 7.4 `[plugins.<name>]`

```toml
[plugins.event-log]
cmd = ["sh", "event-log/event-log.sh"]
env = { LC_ALL = "C" }
enabled = true
restart = "always"
```

| Key | Type, default | Meaning |
|---|---|---|
| `cmd` | array of strings, required, non-empty | the argv, run directly and never through a shell. `cmd[0]` without a `/` is looked up on the plugin's `PATH`; a relative path containing `/` resolves against the directory of the file that set `cmd`, as `source-file` does (§5.1). Any later word that contains `/`, does not start with `/` or `-`, and names an existing path in that directory resolves the same way, so `["sh", "event-log/event-log.sh"]` finds its script wherever the server runs; a word that names nothing there is left as written |
| `env` | table of strings, empty | variables added over the server environment |
| `enabled` | boolean, `true` | `false` declares the plugin without starting it |
| `restart` | `always` (default), `on-failure` or `never` | `always` restarts after any exit; `on-failure` only after a non-zero exit or a signal, and an exit 0 leaves it `exited`; `never` leaves it `exited` after any exit |

- **Names and validation.** `<name>` matches `[A-Za-z0-9._-]+`, since it
  names the log file and `GROFE_PLUGIN`. Unknown keys and wrong types are
  errors naming the file, as everywhere in the schema (§5.2).
- **Layering.** A plugin's table merges key by key across layers 1 to 4
  (§5.1), so `user.toml` can set `enabled = false` on a plugin a canopy
  fragment declares; `cmd` must be present once merged. `show-config
  --explain` gives each key's provenance (§5.4).
- **Environment.** The server's global environment (`set-environment -g`,
  hidden variables left out as for any process, §4.2), then `env`, then
  `GROFE` (the socket path), `GROFE_PLUGIN=<name>` and `GROFE_STATE` (the
  state dir, where `plugins/<name>.log` lives), which `env` cannot
  override. On a server with tmux identity (D12) a plugin also gets `TMUX`
  in tmux's format, `<socket path>,<server pid>,-1` (the session field is
  `-1`, as tmux gives a process outside any session), so its `tmux` calls
  reach this server. A plugin is in no pane, so it gets no `GROFE_PANE` or
  `TMUX_PANE`. stdin is `/dev/null`; stdout and stderr go to
  `<state dir>/plugins/<name>.log`, rotated at 1 MiB.
- **Start and stop.** Enabled plugins start after the config loads (and,
  once M4 lands, after its restore), each spawned off the actor under the
  spawn lock, in its own process group, so a plugin that never reads its
  socket delays nothing. On server exit each group gets SIGTERM, then
  SIGKILL after 2 s, so `kill-server` leaves no plugin process. When the
  leader exits, the rest of its group is SIGKILLed, so a plugin that forks
  and leaves cannot pile up under a restart policy. A plugin gets default
  signal dispositions and only fds 0 to 2. If the server itself dies
  without running its exit path (SIGKILL, a crash), its plugins are not
  stopped: nothing ties them to it, and a new server starts its own.
- **Restarts.** Restarts back off from 1 s, doubling to 60 s, and the backoff
  resets after 60 s alive. Every exit the server did not ask for counts as a
  crash; five crashes within 60 s disable the plugin with a line in the
  server log, and nothing more is spawned for it.
- **`reload`** compares the merged tables: a new or newly enabled plugin
  starts, a removed or disabled one stops, one whose `cmd`, `env` or
  `restart` changed restarts, and the rest are left alone.
- **Restart count.** `restarts` counts restarts since the plugin was
  (re)started by the server, `plugin-restart` or a `reload` that changed it.
- **`list-plugins`** prints `name`, `pid`, `state`, `restarts` and `since`
  per plugin, tab separated, one line each, without a header (`-` for no
  pid). The states are `running`, `backoff` (waiting to restart),
  `exited` (its policy does not restart it), `disabled` (five crashes) and
  `off` (`enabled = false`).
- **`plugin-restart <name>`** stops the plugin as on server exit and starts
  it again at once, clearing its backoff and crash count; it also starts an
  `exited` or `disabled` plugin. An `off` plugin or an unknown name is an
  error.
- **Trust.** A plugin runs as the user with the server's environment, as
  `run-shell` does; a config file that declares one is trusted like a shell
  rc file.

---

## 8. Libraries and terminal support

Verified 2026-09-23 against crates.io, docs.rs and upstream source. The M0 spike
confirms them in practice before anything is built on top.

| Need | Pick | Why | Runner-up |
|---|---|---|---|
| pty | `nix` 0.31 (`openpty`, `TIOCSWINSZ`, `tcgetpgrp`) + `std::process::Command` with `pre_exec` + `tokio::io::unix::AsyncFd` | Full control, async reads with no thread per pane; Zellij's approach (`os_input_output_unix.rs`) | `portable-pty` 0.9.0: complete API but blocking readers, no tokio integration, last release 2025-02 |
| foreground cwd | Linux `/proc/<pgrp>/cwd`; macOS `libproc` 0.14; never OSC 7 (§4.8) | No crate covers both; small per-OS function | none |
| emulation | `alacritty_terminal` 0.26.0 (Apache-2.0, active) | Alt screen, bracketed paste and mouse modes, reflowing resize, damage tracking, OSC 52 and PtyWrite via `EventListener`, serde; published for external use (Zed depends on it) | `vt100` 0.16.2: built-in state diffing, but no reflow and dormant since 2025-07 |
| watch list | `libghostty-vt` 0.2.1 | Best-in-class emulation, but API declared unstable and the build needs Zig | revisit at M6 |
| client tty | `crossterm` 0.29 | Raw mode, SIGWINCH, size; Zellij's client uses it | `rustix` termios |
| diff rendering | own compositor (§4.4) | Pane content is arbitrary styled cells; Zellij does the same (`output/mod.rs`) | `termwiz` 0.23 `Surface` |
| config | `serde` 1.0, `toml` 1.1; `schemars` 1.2 later | Validation through serde | none |
| async, sockets, framing | `tokio` 1.53 Unix sockets, `tokio-util` 0.7 codecs | Standard stack | `interprocess` 2.4 (Zellij) |
| tests | `insta` 1.48 snapshots; `expectrl` 0.9 for pty-driven acceptance; `bats` for the CLI smoke suite | Snapshot the screen an emulator reconstructs from our output | `rexpect` 0.7 |

Not usable: `wezterm-term` is not published on crates.io.

**Licensing:** grofe is Apache-2.0, which matches `alacritty_terminal`
(Apache-2.0 only).

**Inner `TERM`:** D7. WezTerm advertises `xterm-256color`; Zellij sets nothing
and panes inherit the outer `TERM`. grofe panes get `tmux-256color` plus
`COLORTERM=truecolor`, resolved by `src/term.rs::resolve_term()` against the
host's compiled terminfo database (falling back to `screen-256color` where
`tmux-256color` is missing, then to `xterm-256color`). T0.4 found that the M0
spike's render path lost some of the capabilities `tmux-256color` adds over
`screen-256color` (underline styles collapsed to plain underline; title
relay, OSC 52 clipboard, cursor colour and focus events were not forwarded),
so grofe shipped `screen-256color` until T3.6 re-ran every extra capability
through the real render path on macOS and Linux and all of them passed. See
docs/spike-notes.md's "TERM decision" and "T3.6 re-run" sections for the
evidence; the passing re-run is what alias day requires (D7).

An inner `CSI > q` (XTVERSION) gets no reply from the library; grofe answers
it only if an app needs it. This is distinct from the client's probe of the
outer terminal in §4.4.

**Zellij files worth reading:** `zellij-server/src/panes/grid.rs`,
`terminal_pane.rs`, `os_input_output_unix.rs`, `pty.rs`, `screen.rs`, `route.rs`,
`output/mod.rs`; `zellij-utils/src/ipc.rs`; `zellij-client/src/stdin_ansi_parser.rs`.

**Acceptance suite (terminal correctness):** Neovim (alt screen, true colour,
cursor shape, mouse, bracketed paste, focus events, background colour query),
fzf, less, htop, `git log` with a pager, a nested tmux (run as
`env -u TMUX <absolute path to tmux>`, since `$TMUX` points at grofe, D12) and
a nested grofe, a
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
  fail by mutating the saved snapshot and watching it go red. The reboot itself
  is tested in a container: restarting it kills every process and empties the
  runtime directory while the state directory survives, and the server then
  starts as a login unit would start it (plan T4.6; with canopy and a real
  systemd user unit, T7.8).
- **End to end (containers):** `scripts/e2e.sh` runs the acceptance suite, the
  compat suite, the reboot scenario and canopy on grofe in podman containers
  (Debian, Arch, and a systemd image for login units), and the release gate
  runs them all (plan "E2E and release"). Nothing runs against the owner's live
  tmux, grofe or canopy: host-run suites check those directories are
  byte-identical before and after.
- **tmux compatibility (M3.6):** a hermetic suite with a temp `HOME` and a
  `PATH` whose `tmux` is a symlink to the built grofe runs the owner's real
  scripts and tools (canopy `bin/*`, `tsess`, `cockpit`, `fzf --tmux`,
  vim-tmux-navigator's calls from a headless Neovim, a smug-style start) and
  replays a recorded corpus of the argv they send through the front end,
  whose table rows each carry their own golden (§4.5). Where tmux is
  deterministic, outputs must be byte-identical, after one shared list of
  normalizations (socket paths, pids, timestamps, tty names), to goldens
  recorded from real tmux 3.7b (the recorder checks `tmux -V`). Recording is
  hermetic: `env -i` with fixed `SHELL`, `TERM` and `LANG`, `TMUX_TMPDIR` and a
  private `-L` socket in a tempdir, and a defaults fixture (`-f`) that sets
  grofe's §5.3 values so indices and sizes match; scenarios that need a client
  (popups, the status line) run with a pty-attached client; fixtures use no
  strftime sequences. Real tmux is reached only as `env -u TMUX <absolute
  path>`, for recording and for the nested-tmux acceptance item. The suite is proven able to fail by mutating a
  golden, and never touches the user's own sockets.
- **Determinism:** inputs that depend on pids (stagger, jitter, generated
  names) are seeded deterministically in tests (canopy 11a3adf).
- **CI**, on `ubuntu-latest` and `macos-latest`, split by cost into separate jobs
  so one red job cannot hide another (canopy 94435f6, acc19eb): `unit` (fmt,
  clippy, lib and doc tests) on every push; `acceptance` (integration tests,
  bats, and later containers) on pull requests, `main` and manual dispatch.
  GitHub Actions is disabled for now: `scripts/ci-local.sh` runs the same
  workflow with `act` on podman for Linux and natively on macOS, and from M4 an
  `e2e` leg.

---

## 10. Risks

| Risk | Mitigation |
|---|---|
| Emulator library gaps | M0 spike runs Neovim through `alacritty_terminal` before anything is built on it. |
| Copy mode grows into a subsystem | Own milestone (M5), scoped in writing first. Until then the dogfood fallback is the outer terminal's native selection (shift-drag bypasses mouse mode). |
| Detach/reattach at a different size | D6; integration test at M2. |
| A server crash loses live work | Dogfood alongside tmux in month one; M4 snapshots make a crash recoverable; panics write a report; no `unwrap` on socket or pty paths. |
| Scope creep toward "replace tmux for everyone" | §1 non-goals, D1, and the milestone column in §4.5. New commands need a dogfooding reason. |
| Compatibility breadth creeps toward all of tmux, or into grofe's own grammar | The owner's inventory and the recorded argv corpus close the list (§4.5, D1), which lives in the front end's table, never in the native parser; the out-of-scope table names what stays out and why. A new item needs a corpus line or a dogfood entry, and costs one row plus one golden. |
| Parity drifts as tmux releases change behaviour | Goldens are pinned to one compat level (tmux 3.7b, D12); moving to a newer tmux is a deliberate re-record, reviewed as a diff. |
| Hooks and status sources reintroduce the forks D8 removed | Sources are interval-bound with one run in flight and a timeout; hooks and `if-shell` shell forms run off the actor. The M3.6 tests count spawns under forced 60 Hz redraws. |
| Protocol churn breaking tests | Protocol internal until M6; tests go through one harness module. |
| Terminal replies stuck behind the actor, making apps hang | §4.8: replies go from the reader straight to the writer queue. |
| Inline images help only programs that use kitty's placeholder mode | Before building it, a T0-style check records which of the owner's tools emit placeholders (Claude Code image output, `kitten icat --unicode-placeholder`, chafa and timg if relevant). Programs that use direct placement or sixel show nothing unless `graphics` is on (G3). |

---

## 11. Non-goals (v1)

- Windows support.
- Reading or executing `tmux.conf` as the config, and running TPM itself (D1:
  the owner's config is ported once, plan M3.6, drafted by `grofe import-tmux`,
  §5.5; the plugins run unchanged through canopy's loader, plan M3.7); tmux's `-CC` wire format (M8 maybe); tmux compatibility beyond the
  owner's inventory (§4.5).
- WASM plugins.
- Network session sharing.
- Arbitrary DCS passthrough. Kitty graphics in Unicode-placeholder mode are
  supported (§4.9); direct placement, animation and sixel are opt-in (the
  `graphics` option, G3).
- Scrollback persistence across restarts.
- Layout presets beyond `main-vertical` and `even-*` (§4.3), until dogfooding asks.
