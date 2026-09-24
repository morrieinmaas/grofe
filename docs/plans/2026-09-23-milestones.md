# grofe: milestones and implementation plan

**Date:** 2026-09-23
**Spec:** [`../specs/2026-09-23-grofe-design.md`](../specs/2026-09-23-grofe-design.md)
**Execution model:** subagent-driven. Tasks are sized for one implementer
subagent each, with explicit file ownership so tasks in the same wave run in
parallel worktrees without conflicts.

---

## Overview

| M | Content | Done means | Estimate |
|---|---|---|---|
| **M0** | Library verification, skeleton, shared types, single-pane spike, `TERM` | Neovim runs correctly inside the spike (scripted check + owner run); CI green | 4 days |
| **M1** | Headless server: socket, protocol, model, panes, M1 commands, minimal events | An integration test and a bats test each create a session, split it, run `echo hi` via `send-keys`, and read `hi` back with `capture-pane`, on Linux and macOS | 2 weeks |
| **M2** | Attach client, renderer, input, key tables, status line | Acceptance suite: attach, split, Neovim/fzf/less, detach, reattach at a different size. Owner hands-on session | 2 weeks |
| **M3** | Config, mouse, clipboard, scrollback view, prompt, key bindings | **Dogfood start.** Full acceptance suite green in CI; owner daily-drives for a week, tmux as fallback | 1.5 weeks + dogfood week |
| **M4** | Native persistence and pane identity | Kill the server, restore: every `uid` unchanged, every pane with `resume` runs it. Then across a real reboot (owner-verified) | 1 week |
| **M5** | Copy mode; `display-popup`, `display-menu` | Scope doc approved first; vi selection over wrapped lines, regex search, OSC 52 copy; popup and menu overlays | 2 weeks |
| **M6** | Events and plugin API, Lua | Spec §7 rewritten from dogfood notes; protocol v1 frozen; one out-of-process and one Lua plugin in-tree | planned after M5 |
| **M7** | agents plugin; canopy on grofe (spec §7.1) | canopy's four adapters report through grofe; a reboot returns every agent pane to its conversation | planned after M6 |
| **M8** | Compatibility extras (optional) | Decided after M7: tmux `-CC` subset, `grofe import-tmux` converter | unscheduled |

**Critical path to dogfood start:** T0.3 → T1.1b → T1.5a → T1.5b → T2.5a → T2.5b →
T3.5 → T3.6. Each step includes a review cycle, and M0/M2 have owner gates.
Estimate: about **7 weeks to dogfood start** (5.5 weeks of work + 30% buffer), not
counting the dogfood week itself.

M0 to M3 are planned to task level. M4 onward are planned at milestone level and
broken into tasks when the previous milestone closes, shaped by dogfooding.

---

## How subagents execute this

1. **One task, one implementer subagent, one worktree.** `wt switch --create
   <task-id>-<slug>`. The implementer receives: its task section, the spec sections
   it references, the contracts in this doc, and the repo's `CLAUDE.md`.
2. **File ownership is a hard boundary.** A task edits only files in its "Owns"
   column. Needing to change another file means stopping and reporting.
3. **No task edits `Cargo.toml`.** T0.2 adds every dependency through M3 up front.
   A missing dependency is reported; the orchestrator adds it on `main` and the
   worktree merges `origin/main`.
4. **Modules exist before tasks start.** T0.2 creates every module through M3 as an
   empty file with its `mod` line, so no task edits `lib.rs` or `server/mod.rs`
   declarations.
5. **CLI subcommands:** T1.1b ships `main.rs` with a subcommand table. A later task
   that adds a subcommand owns exactly one arm, named in its "Owns" column.
6. **Tests:** `tests/common/` is frozen after T1.1b; changes go through a contract
   edit. `tests/acceptance/main.rs` and `harness.rs` belong to T2.4; each later task
   owns `tests/acceptance/<task_id>.rs` plus its one `mod` line in `main.rs`.
7. **Test first.** Write the listed acceptance tests failing, then implement.
   `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
   before reporting done.
8. **Review, then merge.** A reviewer subagent checks the task against its section,
   the spec and the contracts; findings are fixed in the same worktree. The branch
   must be green after `git merge origin/main`. The orchestrator merges, in the
   wave's listed task order, and resolves conflicts.
9. **Contract changes:** an implementer reports; the orchestrator lands the contract
   edit (this doc plus any stub code) on `main` first; dependent worktrees pause,
   `git merge origin/main`, and resume.
10. **Owner gates.** Items tagged **[owner]** need the owner and are never marked
    done by an agent. Everything else is mechanically checkable.

---

## Crate layout

Single crate, one binary, a library target so integration tests use internals.

```
Cargo.toml
src/
  main.rs            # CLI entry, subcommand table
  lib.rs             # module declarations
  types.rs           # T0.5: ids, Rect, Dir, Edge, SplitSize, CaptureBound, CaptureOpts, MouseEncoding, InputModes
  event.rs           # T0.5: Event enum (serde)
  term.rs            # T0.4: inner TERM resolution (consumed by T1.4)
  paths.rs           # socket, lock, state and log paths
  protocol.rs        # hellos, control messages, attach frames, codecs
  cmd/
    mod.rs           # Command enum, ParseError
    parse.rs         # argv -> Command
    target.rs        # target grammar
    format.rs        # #{...} expansion
    keys.rs          # key-name table for send-keys and bindings
  server/
    mod.rs           # actor, lifecycle, accept, hook registration
    model.rs         # sessions, windows, panes, ids, env
    dispatch/
      mod.rs         # match Command -> handler
      session.rs
      window.rs
      pane.rs
      options.rs
    pane/
      mod.rs         # PaneHandle, spawn, pre_exec
      io.rs          # reader + writer tasks, EventListener
      procinfo.rs    # foreground command and cwd, per OS
    layout.rs
    events.rs        # subscriptions, bounded queues
    render.rs        # M2
    input.rs         # M2
    status.rs        # M2
    clients.rs       # M2: client registry, writer tasks, D6 policy
    mouse.rs         # M3
    passthrough.rs   # M3
    prompt.rs        # M3
  client.rs          # M2: attach client + capability probe
  config/
    mod.rs           # M3
    provenance.rs
tests/
  common/mod.rs      # TestServer harness
  cli.bats           # bats smoke suite
  acceptance/main.rs # M2+
```

---

## Interface contracts

Signatures are indicative; names and responsibilities are binding.

### Shared types (T0.5)

```rust
// types.rs
// ids derive Ord (ordered by number)
pub struct SessionId(pub u32);  // Display "$N"
pub struct WindowId(pub u32);   // Display "@N"
pub struct PaneId(pub u32);     // Display "%N"
pub struct Rect { pub x: u16, pub y: u16, pub cols: u16, pub rows: u16 }
pub enum Dir { Horizontal, Vertical }
pub enum Edge { Left, Right, Up, Down }
pub enum SplitSize { Half, Percent(u8), Cells(u16) }
pub enum CaptureBound { Line(i32), HistoryStart /* -S - */, VisibleEnd /* -E - */ }
pub struct CaptureOpts { pub start: Option<CaptureBound>, pub end: Option<CaptureBound>, pub escapes: bool }
                        // None = tmux default: first / last visible line
pub enum MouseEncoding { X10 /* default */, Utf8, Sgr }
pub struct InputModes { pub app_cursor: bool, pub app_keypad: bool, pub kitty_flags: u8,
                        pub bracketed_paste: bool, pub mouse: MouseMode, pub mouse_encoding: MouseEncoding,
                        pub focus: bool, pub alt_screen: bool, pub alternate_scroll: bool }

// event.rs
#[derive(Serialize)] #[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event { PaneExited { pane: PaneId, uid: String, status: Option<i32> },
                 PaneOutput { pane: PaneId, data: String /* base64 */ },
                 EventsDropped { count: u64 } }
```

### Commands (T1.2)

```rust
pub enum Command { NewSession(NewSession), SplitWindow(SplitWindow), SendKeys(SendKeys), /* M1 set */ }
pub fn parse(argv: &[String]) -> Result<Command, ParseError>;
pub enum Target { Session(SessionRef), Window(SessionRef, WindowRef), Pane(PaneRef), Current }
pub fn parse_target(s: &str) -> Result<Target, ParseError>;
pub trait Resolve { fn resolve_pane(&self, t: &Target, ctx: &ClientCtx) -> Result<PaneId, String>; /* window, session */ }
pub trait FormatVars { fn get(&self, name: &str) -> Option<String>; }
pub fn expand(fmt: &str, vars: &dyn FormatVars) -> String;
pub fn key_bytes(name: &str, modes: &InputModes) -> Option<Vec<u8>>;   // cmd/keys.rs
```

Flag values that may contain formats stay as strings in the `Command`; dispatch
expands them against the target at execution time.

### Layout (T1.3)

```rust
impl LayoutTree {
    pub fn new(pane: PaneId) -> Self;
    pub fn split(&mut self, target: PaneId, dir: Dir, new: PaneId, size: SplitSize) -> Result<(), LayoutError>;
    pub fn remove(&mut self, pane: PaneId);
    pub fn resize(&mut self, pane: PaneId, edge: Edge, delta: i32);
    pub fn neighbour(&self, pane: PaneId, edge: Edge) -> Option<PaneId>;
    pub fn compute(&self, area: Rect) -> Vec<(PaneId, Rect)>;       // excludes borders
    pub fn borders(&self, area: Rect) -> Vec<BorderSegment>;         // defined in layout.rs
}
```

### Pane (T1.4)

```rust
pub struct PaneSpawn { pub id: PaneId, pub argv: Option<Vec<String>>, pub cwd: PathBuf,
                       pub env: Vec<(String, String)>, pub cols: u16, pub rows: u16 }
pub struct PaneHandle { /* master in AsyncFd, child, Arc<Mutex<Term<Listener>>>, writer queue, dirty flag */ }
impl PaneHandle {
    pub fn spawn(spec: PaneSpawn, to_actor: mpsc::Sender<PaneEvent>, wake: Arc<Notify>) -> io::Result<Self>;
    pub fn enqueue_input(&self, bytes: Vec<u8>) -> Result<(), QueueFull>;   // never blocks
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
    pub fn capture(&self, opts: &CaptureOpts) -> String;
    pub fn pid(&self) -> Option<u32>;
    pub fn current_command(&self) -> Option<String>;   // cached, timer-sampled
    pub fn current_path(&self) -> Option<PathBuf>;     // sampled (OSC 7 from M3)
    pub fn modes(&self) -> InputModes;
    pub fn take_dirty(&self) -> bool;
    pub fn snapshot_rows(&self, rows: Range<u16>) -> RowsSnapshot;  // M2 renderer input, copied under lock
}
pub enum PaneEvent { Exited(PaneId, Option<i32>) /* child exited AND master drained */,
                     Title(PaneId, String), Bell(PaneId), Clipboard(PaneId, String),
                     Cwd(PaneId, PathBuf) /* emitted from M3 (OSC 7 scanner) */ }
```

### Protocol and actor (T1.1a, T1.1b)

```rust
pub struct ClientCtx { pub cwd: PathBuf, pub pane: Option<PaneId>, pub env: Vec<(String, String)> }
pub enum Hello { Control(ClientCtx), Attach { ctx: ClientCtx, cols: u16, rows: u16, term: String, caps: Caps, session: Option<String> } }
pub struct Request { pub id: u64, pub argv: Vec<String> }
pub enum Reply { Ok { id: u64, out: String }, Err { id: u64, err: String }, Event(Event) }
pub enum Frame { Input(Vec<u8>), Resize(u16, u16), Detach, Output(Vec<u8>), Exit(String) }
pub enum ServerMsg { Control { ctx: ClientCtx, req: Request, reply: oneshot::Sender<Reply> },
                     Pane(PaneEvent), /* M2: client frames */ }
```

### M2 seams (T2.2, T2.3, T2.5a)

```rust
// render.rs
pub struct FrameInputs<'a> { pub area: Rect, pub panes: Vec<(Rect, RowsSnapshot)>, pub borders: Vec<BorderSegment>,
                             pub active: PaneId, pub status: Option<StyledLine>, pub overlays: &'a [&'a dyn Overlay],
                             pub cursor: Option<CursorState> }
pub fn compose(inputs: &FrameInputs) -> Frame;
pub fn diff(prev: &Frame, next: &Frame, caps: &Caps) -> Vec<u8>;
pub trait Overlay { fn draw(&self, frame: &mut Frame); }
// input.rs
pub enum Routed { Command(Vec<String>), ToPane(Vec<u8>), Consumed }
pub trait InputHook { fn on_key(&mut self, key: &Key, client: ClientId) -> Option<Routed>; }
// server/mod.rs (T2.5a): register_overlay, register_input_hook; M3 tasks add one registration line each.
```

### Test harness (T1.1b)

```rust
let srv = TestServer::start();                 // unique -S socket in a tempdir, remain-on-exit on, killed on drop
let out = srv.cmd(&["new-session", "-d", "-s", "t"])?;
srv.wait_for(|| srv.capture("t:1.1").contains("hi"), Duration::from_secs(5))?;
let mut ev = srv.subscribe(&["pane-exited"])?;
```

---

## M0: verify, skeleton, spike

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T0.1** Library verification | **Done 2026-09-23** during planning (spec §8) | spec §8 | none | met |
| **T0.2** Skeleton + CI | `cargo init`; every module in the crate layout as an empty file with its `mod` line; every dependency through M3 (`tokio`, `tokio-util`, `nix`, `libproc` (macOS), `alacritty_terminal`, `crossterm`, `serde`, `serde_json`, `toml`, `ulid`, `base64`, `anyhow`/`thiserror`, `futures-util` (`sink`), `unicode-width`; dev: `insta`, `expectrl`, `proptest`, `tempfile`); `CLAUDE.md`; GitHub Actions matrix (`ubuntu-latest`, `macos-latest`): fmt, clippy, test, bats, and install neovim, fzf, htop, tmux | `Cargo.toml`, `Cargo.lock`, all empty modules, `CLAUDE.md`, `.github/` | none | CI green on an empty test; every listed tool present on both runners |
| **T0.5** Shared types | `types.rs`, `event.rs` per contract, with `Display`/`FromStr` for ids and unit tests | `src/types.rs`, `src/event.rs` | T0.2 | ids round-trip `%3`/`@2`/`$1`; `Event` serializes to the §4.6 JSON |
| **T0.3** Single-pane spike | `examples/spike.rs`: spawn `$SHELL` per spec §4.8 (pre_exec, CLOEXEC), feed `alacritty_terminal` with a listener that answers PtyWrite, render the grid full-screen with a naive full redraw, forward input, handle resize. Throwaway code | `examples/spike.rs`, `tests/spike.rs`, `docs/spike-notes.md` | T0.2 | `tests/spike.rs` (expectrl): starts the spike, runs `nvim -u NONE`, inserts text, asserts it on the reconstructed screen, `:q` restores the shell prompt; `printf '\e[6n'` gets a reply. **[owner]** live run in Ghostty: true colour, cursor shape, resize reflow, less, fzf; gaps tagged "lib" or "ours" in the notes |
| **T0.4** Decide `TERM` | Per spec D7: list `infocmp tmux-256color` capabilities, test the extras in the spike, pick the default, record per-OS fallback | spec D7, `docs/spike-notes.md` | T0.3 | Decision recorded; a test asserts the resolved `TERM` per OS |

**Exit gate:** reviewer confirms spike tests on both OSes; **[owner]** accepts the
library picks. If `alacritty_terminal` fails the spike, M0 extends; nothing else
starts on a lib that failed. "CI green" is **[owner]**-gated on GitHub Actions
billing; until it runs, a Linux container run (`rust:1-bookworm` with neovim) at
the final SHA stands in for the Linux leg.

---

## M1: headless server

**M1 command set (exactly):** `new-session`, `kill-session`, `has-session`,
`list-sessions`, `new-window`, `kill-window`, `select-window`, `list-windows`,
`split-window`, `kill-pane`, `select-pane`, `resize-pane`, `respawn-pane`,
`list-panes`, `send-keys`, `capture-pane -p -S -E`, `set-option`, `show-options`,
`display-message -p`, `kill-server`, `subscribe`. Flags per spec §4.5.

**Wave A (parallel; merge order T1.1a, T1.2, T1.3, T1.4, T1.1b).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T1.1a** Paths and protocol | `paths.rs` (socket, lock, state, log paths per spec §4.1); `protocol.rs`: hellos with `ClientCtx`, JSON-lines codec, attach frame codec, version check | `src/paths.rs`, `src/protocol.rs` | Unit: frame codec round-trips arbitrary bytes and split reads; hello round-trips; version mismatch error names the side to upgrade; paths honour `XDG_RUNTIME_DIR` and fall back on macOS |
| **T1.2** Parser, targets, formats, key names | `cmd/*` for the M1 set with tmux-compatible flags; target forms per spec §4.5; formats `#{var}` and `#{?c,a,b}`; key-name table with `-l`/`-H` semantics | `src/cmd/*` | Table tests: every M1 command parses from tmux-style argv; bad flags give tmux-like errors; every supported target form; unsupported forms give a clear error; format cases; `send-keys` arg classification (key name vs literal) |
| **T1.3** Layout tree | `server/layout.rs` per contract: n-ary behaviour, percent and cell sizes, collapse on remove, deterministic remainder distribution, neighbours, borders | `src/server/layout.rs` | Unit tests for each operation; one proptest: after random split/remove sequences, rects never overlap and rects plus borders exactly tile the area |
| **T1.4** Pane | `server/pane/*` per contract and spec §4.8: spawn with pre_exec and CLOEXEC (FD_CLOEXEC is not atomic with openpty: spawns are serialised under one process-wide lock from openpty to spawn so a concurrent fork cannot inherit a master); env: `TERM` = `term::resolve_term()` resolved once at server start and `COLORTERM` = `term::COLORTERM`, applied AFTER `PaneSpawn.env` so the outer `TERM` never leaks, plus `GROFE`/`GROFE_PANE`; `Config.scrolling_history` from `history-limit` (spec default 50000; lib default 10000); the reader sets the dirty flag and `Notify` itself (`Term` never emits `Wakeup`); reader task with bounded parse per lock and listener handling (PtyWrite to writer queue, other events to actor), writer task with bounded queue, dead detection (exit AND drained, where drained means the master hit EIO/EOF, or a bounded grace period, e.g. 200 ms, passed with no further output, for when a background grandchild holds the slave), resize, `capture`, cached foreground command and cwd on both OSes | `src/server/pane/*` | Tests (no server): `sh -c 'echo hi'` capture contains `hi` and `Exited` arrives after the output; `printf '\e[6n'; read` gets a CPR reply; resize changes `tput cols`; a 1 MiB input into `sleep 10` returns immediately from `enqueue_input` and a second pane stays responsive; no pane master fd is open in a sibling child (`ls /proc/self/fd` or `lsof`); `current_path` follows a `cd`; a pane that sends `\e[?2026h` then stops still captures its text after ~150 ms (sync timeout driven by the reader) |
| **T1.1b** Lifecycle, actor, CLI, harness | Server start per spec §4.1 (flock, re-exec `grofe __server`, readiness pipe, stale socket); the daemon puts /dev/null on fds 0/1/2 before any pane spawn (else a slave fd landing on 0-2 keeps CLOEXEC and closes at exec); actor loop with stub dispatcher answering `kill-server`, one-shot CLI with exit status, `main.rs` subcommand table, `tests/common` harness | `src/server/mod.rs`, `src/main.rs`, `tests/common/`, `tests/lifecycle.rs` | Integration: CLI auto-starts a server; a second call reuses it; ten concurrent CLIs start exactly one server; `kill-server` stops it and removes the socket; a stale socket is replaced; the server log exists |

**Wave B (after A; T1.5b and T1.5c parallel after T1.5a).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T1.5a** Model and session/window dispatch | `server/model.rs` (ids, sessions, windows, panes, session env from `ClientCtx`), `Resolve` impl, `dispatch/mod.rs` match skeleton, session and window commands, lifecycle cascade (dead pane removed unless `remain-on-exit`, empty window closes, last session closes the server) | `src/server/model.rs`, `src/server/dispatch/{mod,session,window}.rs`, `tests/sessions.rs` | Integration: create, list, kill sessions and windows; `Current` target resolves from the request's `pane`; killing the last pane closes the window and then the server |
| **T1.5b** Pane dispatch | `split-window`, `kill-pane`, `select-pane`, `resize-pane`, `respawn-pane`, `send-keys`, `capture-pane`, with execution-time format expansion in `-c` | `src/server/dispatch/pane.rs`, `tests/panes.rs`, `tests/cli.bats` | Integration: the M1 done-means scenario; `respawn-pane` on a dead pane keeps `pane_uid`; `split-window -c '#{pane_current_path}'` uses the target's cwd. bats: the same scenario through the one-shot CLI, plus `display-message -p '#{pane_uid}'` |
| **T1.5c** Options and formats | `set-option`/`show-options` with scopes and `--persist` flag storage, `@` user options, `FormatVars` for the model, `-F` on list commands, `display-message -p` | `src/server/dispatch/options.rs`, `tests/options.rs` | Integration: scope precedence (pane over window over session over global); `list-panes -F '#{pane_id} #{pane_width}'` matches the layout; unknown built-in option errors |

**Wave C.**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T1.6** Events | `server/events.rs`: `subscribe`, bounded per-subscriber queue with `events-dropped`, `pane-exited` and `pane-output` emitted from the actor's `PaneEvent` handler | `src/server/events.rs`, one emission site in `server/mod.rs`, `tests/events.rs` | Integration: `pane-exited` fires once with the exit status; unsubscribed events are not sent; a subscriber that never reads does not delay `new-session` |

**Exit gate:** reviewer runs the M1 scenario and the bats suite from a clean clone
on macOS; CI green on both OSes.

---

## M2: attach and render

**Wave A (parallel; merge order T2.3, T2.2, T2.6, T2.1, T2.4).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T2.1** Attach client | `client.rs`: capability probe (spec §4.4), request richest key mode, raw mode, attach hello, stdin to `input` frames, SIGWINCH to `resize`, write `output` frames, restore the tty on every exit path including panic and SIGTERM (a panic in any thread restores the tty and exits); refuse nested attach without `-f`; `attach` arm in `main.rs` | `src/client.rs`, `attach` arm in `src/main.rs` | pty tests against a fake server: frames flow both ways; tty restored after SIGTERM; resize frame on SIGWINCH; probe times out cleanly on a terminal that answers nothing |
| **T2.2** Renderer | `server/render.rs` per the M2 seams and spec §4.4: compose, diff, colour downgrade by caps, synchronized output, absolute cursor moves around wide cells, letterbox and crop viewport; replace `tests/term.rs`'s smulx negative test with a positive underline-style snapshot | `src/server/render.rs`, `tests/snapshots/`, `tests/render.rs` | Round-trip snapshots: single pane, splits, active border, colours in truecolor and 256 modes, emoji + VS16 + combining marks, cursor; unchanged frame diffs to nothing; one-cell change emits a short sequence |
| **T2.3** Input and key tables | `server/input.rs`: decode client bytes (CSI, SS3, UTF-8, kitty keys, SGR mouse, paste as opaque blob), key tables with prefix and repeat, encode for a pane's `InputModes`, `InputHook` trait | `src/server/input.rs` | Table tests for decode and encode; prefix then `%` routes to the bound command; paste containing the prefix key is never matched; escape-time resolves lone ESC |
| **T2.6** Status line | `server/status.rs`: render `status.left/right` formats to a `StyledLine` with styles; add `#{==:a,b}`, nesting and escaping to `cmd/format.rs` (contract edit: T2.6 co-owns `format.rs` for this) | `src/server/status.rs`, `src/cmd/format.rs` | Unit: formats with conditionals and user options render; overlong content truncates on the left side as tmux does |
| **T2.4** Acceptance harness | `tests/acceptance/{main,harness}.rs`: spawn `grofe attach` in a pty via expectrl, feed keys, reconstruct its output through `alacritty_terminal`, assert on screen text and cursor; consolidate the pty+reconstruct harness duplicated in `tests/spike.rs` and `tests/term.rs` into `tests/acceptance/harness.rs`; fix `spike_bin`'s debug-profile assumption | `tests/acceptance/main.rs`, `tests/acceptance/harness.rs`, `tests/spike.rs`, `tests/term.rs` | Harness self-test passes against the spike binary |

**Wave B (T2.5b after T2.5a).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T2.5a** Clients and frames | `server/clients.rs`: client registry, per-client writer task with one pending frame and full-redraw recovery, render scheduler (dirty flags, ~16 ms), D6 size policy with debounce, detach, overlay and input-hook registration API; attach handling in `server/mod.rs`; M2 commands `attach`, `detach-client`, `list-clients` | `src/server/clients.rs`, attach paths in `src/server/mod.rs`, `tests/acceptance/t2_5a.rs` | Acceptance: attach shows a prompt; detach and reattach from a smaller terminal crops with the viewport following the cursor, and from a larger one letterboxes; a client that stops reading does not delay a second client |
| **T2.5b** Bindings and routing | Default tmux bindings (spec §5.3), input routing into dispatch, remaining M2 commands (`next/previous/last-window`, `rename-*`, `switch-client`, `capture-pane -e`) | `src/server/dispatch/window.rs` additions (coordinated, T1.5a merged), default key table in `src/server/input.rs` (T2.3 merged), `tests/acceptance/t2_5b.rs` | Acceptance: `prefix %` and `prefix "` split, arrows move, `prefix d` detaches; Neovim, fzf and less scenarios pass |

**Exit gate:** reviewer runs the acceptance suite on both OSes; **[owner]**
30-minute hands-on session in Ghostty, listing what hurt.

---

## M3: config and dogfood

**Wave A (parallel; each task registers through the T2.5a hooks with one line in `server/mod.rs`).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T3.1** Config core | `config/*`: schema structs (options from spec §5.2 including `status.style` and `pane-border`), layering (spec §5.1), winning-source provenance, unknown-key and type errors, `show-config --explain`, `show-config --file` without a server (`show-config` arm in `main.rs`) | `src/config/*`, `show-config` arm in `src/main.rs`, `tests/config.rs` | Unit: layering order; provenance names the right file; a type error fails with file and key; unknown key suggests the closest valid one; `--file` works with no server running |
| **T3.2** Mouse and scrollback view | Click to focus; wheel scrolls a read-only scrollback overlay in panes without mouse mode; wheel passes through to panes that requested mouse | `src/server/mouse.rs`, `tests/acceptance/t3_2.rs` | Acceptance: wheel in a shell scrolls history; wheel in Neovim scrolls Neovim; click focuses a pane |
| **T3.3** Passthrough | Spec §4.9 set: OSC 52, title, bell with activity flag, OSC 8 from cell hyperlinks, OSC 9/777; OSC 7/9/777 byte scanner (`vte` 0.15 parses none of them; survives split reads; feeds `PaneEvent::Cwd`); bracketed paste and focus forwarding verified end to end | `src/server/passthrough.rs`, `tests/acceptance/t3_3.rs` | Acceptance: Neovim `"+y` produces an OSC 52 sequence in the outer pty stream; a multi-line paste into Neovim arrives as one bracketed paste; `printf '\a'` sets the window activity flag |
| **T3.4** Prompt and run-shell | `prefix :` prompt in the status line with line editing and history; `command-prompt`; `run-shell` for bindings | `src/server/prompt.rs`, `tests/acceptance/t3_4.rs` | Acceptance: `prefix :` then `split-window -h` splits; a binding to `run-shell 'touch /tmp/x'` creates the file |

**Wave B.**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T3.5** Wire config | Config drives options and key tables; `reload` applies the diff; `bind-key`, `unbind-key`, `list-keys` (with source and description), `source-file`; README install and usage | `src/server/dispatch/options.rs` additions, `src/server/input.rs` table loading, `README.md`, `tests/config_live.rs`, `.github/workflows/readme.yml` | Integration: editing a conf.d file then `reload` changes a binding; `list-keys` shows each binding's source; README steps pass in a clean Linux container job |
| **T3.6** Dogfood readiness | Complete acceptance suite (spec §8 list, nested tmux and nested grofe included) green on both OSes; panic hook writes a report to the state dir; `docs/dogfood.md` log template with the copy-mode fallback (shift-drag) noted; re-run the T0.4 capability table against the real render path; flip D7 to tmux-256color if every extra capability now passes | `tests/acceptance/t3_6.rs`, panic hook in `src/main.rs`, `docs/dogfood.md` | CI green; a forced panic writes a report; **[owner]** starts the dogfood week |

**Exit gate:** **[owner]** a week of daily use, logged in `docs/dogfood.md`. The
log feeds the task breakdown for M4 and M5.

---

## M4 to M8 (milestone level)

**M4 Persistence and pane identity** (spec §6). Snapshot writer with version,
restore into an empty server, `pane-set-resume`, `--persist` options,
`restore-on-start`, stagger, `reboot-check`, env on first attach. Done: kill the
server, restore, every `uid` unchanged and every pane with `resume` runs it;
**[owner]** the same across a real reboot.

**M5 Copy mode and overlays.** First a one-page scope doc (keymap, selection
model across wrapped lines, search, copy targets), **[owner]** approved. Then copy
mode, `display-popup -E`, and `display-menu`.

**M6 Events and plugins.** Rewrite spec §7 from the dogfood log and the events
actually needed. Freeze protocol v1. Plugin supervision, Lua runtime, format
functions.

**M7 agents plugin and canopy on grofe** (spec §7, §7.1). Port canopy's
adapters, `adopt` with its guards, canopy's grofe backend. Done: **[owner]** a
real reboot returns every Claude Code pane to its own conversation.

**M8 Compatibility extras.** Decide after M7 whether a tmux `-CC` subset or an
`import-tmux` converter earns its keep.
