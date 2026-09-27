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
| **M3** | Config, mouse, clipboard, scrollback view, prompt, key bindings, kitty graphics placeholders | **Dogfood start.** Full acceptance suite green in CI; owner daily-drives for a week in Ghostty and WezTerm, tmux as fallback | 1.5 weeks + dogfood week |
| **M3.5** | Minimal `grofe import-tmux` (spec §5.5) | A fixture tmux.conf converts to golden TOML that loads in grofe with no errors; every unconvertible line is reported | ~3 days, during the dogfood week |
| **M3.6** | tmux compatibility for the owner's workflow (spec D1, D12, §4.5): the `tmux` name and server identity, CLI/target/format/option breadth, cached `#()` status sources, hooks and server environment, popups and menus (moved from M5), multi-row status, the owner's daily config ported (canopy, `user.conf`, navigator), compat suite | **Alias day.** The compat suite is green in CI against goldens recorded from tmux 3.7b; **[owner]** `tmux` on PATH resolves to grofe with the ported config for a week of normal work, real tmux kept as `env -u TMUX <absolute path>` as the fallback | ~6 weeks |
| **M3.7** | Port the remaining TPM plugins once to grofe-native equivalents | Key parity passes for every table; each plugin row's acceptance passes; **[owner]** each daily binding used once | ~1.5 weeks alongside M4; closes with M5 (TP.5 yank and TP.9's copy-mode rows need copy mode) |
| **M4** | Native persistence and pane identity | Kill the server, restore: every `uid` unchanged, every pane with `resume` runs it. Then across a real reboot (owner-verified) | 1 week |
| **M5** | Copy mode, buffers, `copy-pipe*` (popups and menus moved to M3.6) | Scope doc approved first; vi selection over wrapped lines, regex search, OSC 52 copy, `copy-pipe-and-cancel`; then the yank port (TP.5). **No-difference parity** for the owner's workflow | 2 weeks |
| **M6** | Events and plugin API, Lua | Spec §7 rewritten from dogfood notes; protocol v1 frozen; one out-of-process and one Lua plugin in-tree | planned after M5 |
| **M7** | agents plugin; canopy on grofe (spec §7.1) | canopy's four adapters report through grofe; a reboot returns every agent pane to its conversation | planned after M6 |
| **M8** | Compatibility extras (optional) | Decided after M7: tmux `-CC` subset | unscheduled |

**Critical path to dogfood start:** T0.3 → T1.1b → T1.5a → T1.5b → T2.5a → T2.5b →
T3.5 → T3.6. Each step includes a review cycle, and M0/M2 have owner gates.
Estimate: about **7 weeks to dogfood start** (5.5 weeks of work + 30% buffer), not
counting the dogfood week itself. M3.5 adds about 3 days after dogfood start and
runs alongside the dogfood week; it does not move dogfood start.

**Recommended order after M3.5:** M3.6, then M4 and M3.7 side by side, then M5
(which also closes M3.7).

- **M3.6 first.** The owner can only do real work in grofe once their scripts
  and tools behave the same under the `tmux` name, and every later milestone
  (persistence above all) is proven by that daily use. Putting M3.6 right after
  M3.5 turns the dogfood week's "tmux as fallback" into dogfooding with the
  alias.
- **M4 next, not after M3.7.** Persistence is the core wedge, and under the
  alias its absence is a regression: the owner's tmux restores sessions today
  through resurrect and continuum. M3.7's tasks mostly own files in canopy and
  athome, disjoint from M4's, so both run as parallel waves; with one lane, M4
  goes first.
- **M5 last of the four,** carrying copy mode, buffers and the yank port. Its
  exit is the "no difference" point; until then shift-drag stays the copy
  fallback (spec §10).
- **Rejected alternative:** M4 before M3.6 brings persistence about six weeks
  earlier, but the owner would still be working in tmux, so nothing would
  exercise it.

**Critical path to alias day:** the dogfood-start path above, then (with TC.0
and TC.0b already done during M1 to M3) the M3.6 stub edit → TC.2 → TC.2b →
TC.11 → TC.10 → TP.0 → TP.1 → TC.13, about 23 working days with review cycles.
**Estimate, honestly, assuming at least three parallel implementer lanes**
(Wave B has five tasks that can start at once): alias day about **week 14**
from the start (7 to dogfood start, the dogfood week with M3.5, about 6 for
M3.6: 4.6 weeks of path work plus 30%); M4 alongside M3.7's parallel tasks ends
about week 15.5; M5, which also closes M3.7 through TP.5 and TP.9's copy-mode
rows, ends about week **18**, and with it no-difference parity. Against the
plan before the compat decision (M5 done at about week 11), that is about
**7 weeks more**: M3.6 about 6, M3.7 about 1.5 of which one overlaps M4, and
TP.5 inside M5. With one lane, M3.6 alone is about 56 task-days, roughly 14.5
weeks with the buffer, and alias day moves to about week 23.

M0 to M3.7 are planned to task level. M4 onward are planned at milestone level and
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
    graphics.rs      # M3: kitty graphics placeholders (T3.8)
    sources.rs       # M3.6: cached status sources, #() (TC.4)
    hooks.rs         # M3.6: set-hook, run on internal events (TC.5)
    popup.rs         # M3.6: display-popup (TC.6)
    menu.rs          # M3.6: display-menu (TC.7)
    tree.rs          # M3.6: choose-tree, choose-client, find-window (TC.9)
    dispatch/        # M3.6 stub handlers, one file per owning task:
                     #   hooks.rs (TC.5), structure.rs (TC.8), client.rs (TC.9),
                     #   menu.rs (TC.7), popup.rs (TC.6), bindings.rs (TC.10)
  client.rs          # M2: attach client + capability probe
  compat.rs          # M3.6: argv0 `tmux`, compat level, TMUX value (TC.1)
  config/
    mod.rs           # M3
    provenance.rs
  import_tmux.rs     # M3.5: tmux.conf to TOML converter (TI.1)
  cmd/tmux_names.rs  # M3.6: tmux option name table, shared with import_tmux (TC.2)
tests/
  common/mod.rs      # TestServer harness
  cli.bats           # bats smoke suite
  acceptance/main.rs # M2+
  compat/            # M3.6: recorder and harness (TC.0b), scenarios (TC.13)
  fixtures/          # probe replies (T2.1), tmux.conf inputs and golden TOML (TI.1),
                     # compat corpus and tmux 3.7b goldens (TC.0, TC.13)
```

T0.2's skeleton predates `graphics.rs` and `import_tmux.rs`. Adding their empty
files and `mod` lines is the orchestrator's stub edit on `main` before T3.8 and
TI.1 start (rule 4). The same holds for the M3.6 files above, stubbed before
M3.6 starts, together with any dependency M3.6 needs that T0.2 did not add
(`regex` for `#{m/r:}`), by the orchestrator on `main` (rule 3).

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
    pub fn current_argv(&self) -> Option<Vec<String>>; // foreground process's full argv, same sample
    pub fn current_path(&self) -> Option<PathBuf>;     // sampled (OSC 7 from M3)
    pub fn modes(&self) -> InputModes;
    pub fn take_dirty(&self) -> bool;
    pub fn snapshot_rows(&self, rows: Range<u16>) -> RowsSnapshot;  // M2 renderer input, copied under lock
}
pub enum PaneEvent { Exited(PaneId, Option<i32>) /* child exited AND master drained */,
                     Title(PaneId, String), Bell(PaneId), Clipboard(PaneId, String),
                     Cwd(PaneId, PathBuf) /* emitted from M3 (OSC 7 scanner) */,
                     Graphics(PaneId, Vec<u8>) /* M3: one framed APC _G command from the scanner */ }
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
let srv = TestServer::start();                 // unique -S socket in a tempdir, remain-on-exit on, killed on drop;
                                               // HOME, XDG_STATE_HOME, XDG_RUNTIME_DIR in tempdirs, restore-on-start off
let out = srv.cmd(&["new-session", "-d", "-s", "t"])?;
srv.wait_for(|| srv.capture("t:1.1").contains("hi"), Duration::from_secs(5))?;
let mut ev = srv.subscribe(&["pane-exited"])?;
```

---

## M0: verify, skeleton, spike

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T0.1** Library verification | **Done 2026-09-23** during planning (spec §8) | spec §8 | none | met |
| **T0.2** Skeleton + CI | `cargo init`; every module in the crate layout as an empty file with its `mod` line; every dependency through M3 (`tokio`, `tokio-util`, `nix`, `libproc` (macOS), `alacritty_terminal`, `crossterm`, `serde`, `serde_json`, `toml`, `ulid`, `base64`, `anyhow`/`thiserror`, `futures-util` (`sink`), `unicode-width`; dev: `insta`, `expectrl`, `proptest`, `tempfile`); `CLAUDE.md`; GitHub Actions matrix (`ubuntu-latest`, `macos-latest`): fmt, clippy, test, bats, and install neovim, fzf, htop, tmux; split by cost (spec §9) into a `unit` job on every push and an `acceptance` job (integration tests, bats) on PRs, `main` and `workflow_dispatch` | `Cargo.toml`, `Cargo.lock`, all empty modules, `CLAUDE.md`, `.github/` | none | CI green on an empty test; every listed tool present on both runners |
| **T0.5** Shared types | `types.rs`, `event.rs` per contract, with `Display`/`FromStr` for ids and unit tests | `src/types.rs`, `src/event.rs` | T0.2 | ids round-trip `%3`/`@2`/`$1`; `Event` serializes to the §4.6 JSON |
| **T0.3** Single-pane spike | `examples/spike.rs`: spawn `$SHELL` per spec §4.8 (pre_exec, CLOEXEC), feed `alacritty_terminal` with a listener that answers PtyWrite, render the grid full-screen with a naive full redraw, forward input, handle resize. Throwaway code | `examples/spike.rs`, `tests/spike.rs`, `docs/spike-notes.md` | T0.2 | `tests/spike.rs` (expectrl): starts the spike, runs `nvim -u NONE`, inserts text, asserts it on the reconstructed screen, `:q` restores the shell prompt; `printf '\e[6n'` gets a reply. **[owner]** live run in Ghostty and in WezTerm (spec D11): true colour, cursor shape, resize reflow, less, fzf; gaps tagged "lib" or "ours" in the notes |
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
| **T1.4** Pane | `server/pane/*` per contract and spec §4.8: spawn with pre_exec and CLOEXEC (FD_CLOEXEC is not atomic with openpty: spawns are serialised under one process-wide lock from openpty to spawn so a concurrent fork cannot inherit a master); env: `TERM` = `term::resolve_term()` resolved once at server start and `COLORTERM` = `term::COLORTERM`, applied AFTER `PaneSpawn.env` so the outer `TERM` never leaks, plus `GROFE`/`GROFE_PANE`; `Config.scrolling_history` from `history-limit` (spec default 50000; lib default 10000); the reader sets the dirty flag and `Notify` itself (`Term` never emits `Wakeup`); reader task with bounded parse per lock and listener handling (PtyWrite to writer queue, other events to actor), writer task with bounded queue, dead detection (exit AND drained, where drained means the master hit EIO/EOF, or a bounded grace period, e.g. 200 ms, passed with no further output, for when a background grandchild holds the slave), resize, `capture`, cached foreground command, full foreground argv (`current_argv`, spec §4.8; never trust comm) and cwd on both OSes | `src/server/pane/*` | Tests (no server): `sh -c 'echo hi'` capture contains `hi` and `Exited` arrives after the output; `printf '\e[6n'; read` gets a CPR reply; resize changes `tput cols`; a 1 MiB input into `sleep 10` returns immediately from `enqueue_input` and a second pane stays responsive; no pane master fd is open in a sibling child (`ls /proc/self/fd` or `lsof`); `current_path` follows a `cd`; `current_argv` returns the full argv of a foreground process whose comm differs from its `argv[0]` (e.g. started via `exec -a`); a pane that sends `\e[?2026h` then stops still captures its text after ~150 ms (sync timeout driven by the reader) |
| **T1.1b** Lifecycle, actor, CLI, harness | Server start per spec §4.1 (flock, re-exec `grofe __server`, readiness pipe, stale socket); only session-creating commands auto-start, every other command fails fast with `no server running on <socket>`; the daemon puts /dev/null on fds 0/1/2 before any pane spawn (else a slave fd landing on 0-2 keeps CLOEXEC and closes at exec) and closes every other inherited fd above 2; state and log paths derive from `-S`; actor loop with stub dispatcher answering `kill-server`, one-shot CLI with exit status, `main.rs` subcommand table, `tests/common` harness that puts `HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR` in tempdirs and forces restore-on-start off (a no-op until M4, there so M4 can never restore the user's real sessions) | `src/server/mod.rs`, `src/main.rs`, `tests/common/`, `tests/lifecycle.rs` | Integration (the CLI decides auto-start by verb, so these run before T1.5a implements `new-session`): `new-session` auto-starts a server; a second call reuses it; ten concurrent `new-session` CLIs start exactly one server; a `set` with no server exits nonzero and starts nothing; a CLI started with an extra open pipe exits and the pipe's reader sees EOF while the server lives; `kill-server` stops it and removes the socket; a stale socket is replaced; the server log exists under the test's state dir |

**Wave B (after A; T1.5b and T1.5c parallel after T1.5a).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T1.5a** Model and session/window dispatch | `server/model.rs` (ids, sessions, windows, panes, session env from `ClientCtx`), `Resolve` impl, `dispatch/mod.rs` match skeleton, session and window commands, lifecycle cascade (dead pane removed unless `remain-on-exit`, empty window closes, last session closes the server) | `src/server/model.rs`, `src/server/dispatch/{mod,session,window}.rs`, `tests/sessions.rs` | Integration: create, list, kill sessions and windows; `Current` target resolves from the request's `pane`; killing the last pane closes the window and then the server |
| **T1.5b** Pane dispatch | `split-window`, `kill-pane`, `select-pane`, `resize-pane` (with cell counts), `respawn-pane`, `send-keys`, `capture-pane`, with execution-time format expansion in `-c`; bats runs the built binary with a temp `HOME` and no `GROFE*` env (spec §9) | `src/server/dispatch/pane.rs`, `tests/panes.rs`, `tests/cli.bats` | Integration: the M1 done-means scenario; `respawn-pane` on a dead pane keeps `pane_uid`; `split-window -c '#{pane_current_path}'` uses the target's cwd; `resize-pane -L 5` moves the border 5 cells. bats: the same scenario through the one-shot CLI, plus `display-message -p '#{pane_uid}'` |
| **T1.5c** Options and formats | `set-option`/`show-options` with scopes and `--persist` flag storage, `@` user options, same-value `set` is a no-op (spec §4.2), `FormatVars` for the model (including `pane_current_argv` and `window_zoomed_flag`), `-F` on list commands, `display-message -p` | `src/server/dispatch/options.rs`, `tests/options.rs` | Integration: scope precedence (pane over window over session over global); `list-panes -F '#{pane_id} #{pane_width}'` matches the layout; `#{pane_current_argv}` shows a foreground `sleep 30`'s argv; unknown built-in option errors |

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
| **T2.1** Attach client | `client.rs`: capability probe (spec §4.4), request richest key mode, raw mode, attach hello, stdin to `input` frames, SIGWINCH to `resize`, write `output` frames, restore the tty on every exit path including panic and SIGTERM (a panic in any thread restores the tty and exits); refuse only self-nesting attach (the target session contains the calling pane) without `-f`, and allow attach to another session from inside a pane (spec §4.6; the popup case lands with TC.6); `attach` arm in `main.rs` | `src/client.rs`, `attach` arm in `src/main.rs`, `tests/fixtures/probe/` | pty tests against a fake server: frames flow both ways; tty restored after SIGTERM; resize frame on SIGWINCH; probe times out cleanly on a terminal that answers nothing. Unit: recorded Ghostty and WezTerm replies (DA1, XTVERSION, DECRQM 2026, kitty keyboard query, kitty graphics query) parse to the expected caps, including WezTerm without `enable_kitty_keyboard` (kitty keys off, everything else on) |
| **T2.2** Renderer | `server/render.rs` per the M2 seams and spec §4.4: compose, diff, colour downgrade by caps, synchronized output, absolute cursor moves around wide cells, letterbox and crop viewport; replace `tests/term.rs`'s smulx negative test with a positive underline-style snapshot | `src/server/render.rs`, `tests/snapshots/`, `tests/render.rs` | Round-trip snapshots: single pane, splits, active border, colours in truecolor and 256 modes, emoji + VS16 + combining marks, cursor; unchanged frame diffs to nothing; one-cell change emits a short sequence |
| **T2.3** Input and key tables | `server/input.rs`: decode client bytes (CSI, SS3, UTF-8, kitty keys, SGR mouse, paste as opaque blob), key tables with prefix and repeat, encode for a pane's `InputModes`, `InputHook` trait | `src/server/input.rs` | Table tests for decode and encode; prefix then `%` routes to the bound command; paste containing the prefix key is never matched; escape-time resolves lone ESC |
| **T2.6** Status line | `server/status.rs` per spec §4.10: render `status.left/right`, `window-status-format` and `window-status-current-format` to a `StyledLine` with styles, expanding `#{}` inside `#[...]` and strftime sequences; add `#{==:a,b}`, `#{b:var}`, nesting and escaping to `cmd/format.rs` (contract edit: T2.6 co-owns `format.rs` for this) | `src/server/status.rs`, `src/cmd/format.rs` | Unit: formats with conditionals and user options render; `#[fg=#{@theme_blue}]` takes the option's colour; `%H:%M` expands against a fixed clock; `#{b:/a/b/c}` gives `c`; the current window uses the current format; overlong content truncates on the left side as tmux does |
| **T2.4** Acceptance harness | `tests/acceptance/{main,harness}.rs`: spawn `grofe attach` in a pty via expectrl, feed keys, reconstruct its output through `alacritty_terminal`, assert on screen text and cursor; tool paths (nvim, fzf, less, tmux) are discovered, never hardcoded (spec §9); consolidate the pty+reconstruct harness duplicated in `tests/spike.rs` and `tests/term.rs` into `tests/acceptance/harness.rs`; fix `spike_bin`'s debug-profile assumption | `tests/acceptance/main.rs`, `tests/acceptance/harness.rs`, `tests/spike.rs`, `tests/term.rs` | Harness self-test passes against the spike binary |

**Wave B (T2.5b after T2.5a).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T2.5a** Clients and frames | `server/clients.rs`: client registry, per-client writer task with one pending frame and full-redraw recovery, render scheduler (dirty flags, ~16 ms), D6 size policy with debounce, detach, overlay and input-hook registration API; attach handling in `server/mod.rs`; M2 commands `attach`, `detach-client`, `list-clients` | `src/server/clients.rs`, attach paths in `src/server/mod.rs`, `tests/acceptance/t2_5a.rs` | Acceptance: attach shows a prompt; detach and reattach from a smaller terminal crops with the viewport following the cursor, and from a larger one letterboxes; a client that stops reading does not delay a second client |
| **T2.5b** Bindings and routing | Default tmux bindings (spec §5.3), input routing into dispatch, remaining M2 commands (`next/previous/last-window`, `rename-*`, `switch-client -l -p -n`, `send-prefix`, `capture-pane -e`); `automatic-rename` to `#{b:pane_current_path}`, with OSC titles never freezing the name (spec §4.5) | `src/server/dispatch/window.rs` additions (coordinated, T1.5a merged), default key table in `src/server/input.rs` (T2.3 merged), `tests/acceptance/t2_5b.rs` | Acceptance: `prefix %` and `prefix "` split, arrows move, `prefix d` detaches; Neovim, fzf and less scenarios pass; a TUI that pushed kitty keyboard flags quits on a bare `q`; `cd /tmp` renames the window to `tmp`, and still does after the pane sets an OSC title. **[owner]** the bare-`q` check inside Claude Code; note that macOS option-as-alt misbehaves in some Ghostty builds (canopy 8070a22), so a failure there is triaged against the outer terminal first |

**Exit gate:** reviewer runs the acceptance suite on both OSes; **[owner]**
30-minute hands-on session split across Ghostty and WezTerm, listing what hurt
and in which terminal.

---

## M3: config and dogfood

**Wave A (parallel; each task registers through the T2.5a hooks with one line in `server/mod.rs`).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T3.1** Config core | `config/*`: schema structs (options from spec §5.2 including `status.style`, `pane-border` and key tables with `name` and `repeat`), layering (spec §5.1, `user.toml` last), `requires` header (spec §5.2), winning-source provenance, unknown-key and type errors with file and line, duplicate key and duplicate `name` rejection, `show-config --explain`, `show-config --file` without a server resolving the machine's full chain (`show-config` arm in `main.rs`) | `src/config/*`, `show-config` arm in `src/main.rs`, `tests/config.rs` | Unit: layering order, with `user.toml` beating a conf.d fragment; provenance names the right file; a type error fails with file, line and key; unknown key suggests the closest valid one; a file with `requires = "grofe >= 99.0"` and an unknown key is skipped with a warning while the same key without the header is an error; a key bound twice and a `name` used twice are both rejected; `--file` works with no server running, includes the user layers, and finishes under a test timeout |
| **T3.2** Mouse and scrollback view | Click to focus; wheel scrolls a read-only scrollback overlay in panes without mouse mode; wheel passes through to panes that requested mouse | `src/server/mouse.rs`, `tests/acceptance/t3_2.rs` | Acceptance: wheel in a shell scrolls history; wheel in Neovim scrolls Neovim; click focuses a pane |
| **T3.3** Passthrough | Spec §4.9 set: OSC 52, title, bell with activity flag, OSC 8 from cell hyperlinks, OSC 9/777; OSC 7/9/777 byte scanner (`vte` 0.15 parses none of them; survives split reads; feeds `PaneEvent::Cwd`), which also frames APC `_G` sequences (`vte` drops APC) into `PaneEvent::Graphics` for T3.8; bracketed paste and focus forwarding verified end to end | `src/server/passthrough.rs`, `tests/acceptance/t3_3.rs` | Acceptance: Neovim `"+y` produces an OSC 52 sequence in the outer pty stream; a multi-line paste into Neovim arrives as one bracketed paste; `printf '\a'` sets the window activity flag. Unit: an APC `_G` sequence split across two reads yields one `PaneEvent::Graphics` and none of its bytes reach the emulator |
| **T3.4** Prompt and run-shell | `prefix :` prompt in the status line with line editing and history; `command-prompt`; `run-shell` for bindings | `src/server/prompt.rs`, `tests/acceptance/t3_4.rs` | Acceptance: `prefix :` then `split-window -h` splits; a binding to `run-shell 'touch /tmp/x'` creates the file |
| **T3.7** Layout presets | `select-layout main-vertical`, `even-horizontal`, `even-vertical` (spec §4.3) as `LayoutTree` operations; `select-layout` dispatch arm | `src/server/layout.rs` additions (T1.3 merged), `select-layout` arm in `src/server/dispatch/window.rs`, `tests/layout_presets.rs` | Unit: `main-vertical` over four panes gives one full-height left pane and three stacked on the right; `even-*` gives equal sizes within one cell; the T1.3 tiling proptest still holds after a preset. Integration: `select-layout main-vertical` then `list-panes -F` matches |

**Wave B.**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T3.5** Wire config | Config drives options and key tables (`repeat` bindings, `name`s in `list-keys`); `reload` validates every layer, then applies all or nothing and reports all errors (spec §5.4); `bind-key`, `unbind-key`, `list-keys` (with source and description; runtime and plugin binds marked as outside the table), `source-file` (relative paths against the including file); `;` command sequences in binding strings and the CLI (contract edit: T3.5 co-owns `cmd/parse.rs` for this); README install and usage, including the supported terminals and the WezTerm `enable_kitty_keyboard = true` note (spec D11) | `src/server/dispatch/options.rs` additions, `src/server/input.rs` table loading, `src/cmd/parse.rs` (sequences), `README.md`, `tests/config_live.rs`, `.github/workflows/readme.yml` | Integration: editing a conf.d file then `reload` changes a binding; a `reload` with errors in two files reports both and changes nothing; `source-file sub.toml` from a conf.d file loads the file next to it whatever the server's cwd; `list-keys` shows each binding's source and marks a runtime `bind-key`; a `split-window ; select-pane -L` binding runs both; README steps pass in a clean Linux container job |
| **T3.8** Kitty graphics placeholders | Spec §4.9. First a T0-style check, recorded before any code: which of the owner's tools emit placeholder mode (Claude Code image output, `kitten icat --unicode-placeholder`, chafa and timg if relevant); **[owner]** reviews the result. Then `server/graphics.rs`: consume `PaneEvent::Graphics` from T3.3's scanner; per-pane image id remap; forward transmit, virtual-placement (`U=1`) and delete commands with `q=2` to clients whose caps report kitty graphics, queued ahead of the next frame; answer the pane's `a=q` queries from caps; keep the last data per image id under a memory cap and re-send it on attach and full redraw; the renderer rewrites placeholder cell colours to the remapped id and never downgrades them (contract edit: T3.8 co-owns `render.rs` for this); direct placement and sixel ignored | `src/server/graphics.rs`, one registration line in `src/server/mod.rs`, placeholder colour rewrite in `src/server/render.rs`, `tests/acceptance/t3_8.rs`, `docs/graphics-notes.md` | Acceptance (skipped when `kitten` is absent): `kitten icat --unicode-placeholder` in a pane produces the APC transmit in the outer pty stream and placeholder cells on the reconstructed screen; the placeholder cells survive a split and a scroll with the image id intact; a second client attaching afterwards receives the image data. Unit: two panes that both transmit id 1 are forwarded with distinct ids; the cache evicts oldest first at its cap |
| **T3.6** Dogfood readiness | Complete acceptance suite (spec §8 list, nested tmux and nested grofe included) green on both OSes; panic hook writes a report to the state dir; `docs/dogfood.md` log template with the copy-mode fallback (shift-drag) noted; re-run the T0.4 capability table against the real render path; flip D7 to tmux-256color if every extra capability now passes (a capability that still fails is logged as a gap to fix before alias day, spec D7) | `tests/acceptance/t3_6.rs`, panic hook in `src/main.rs`, `docs/dogfood.md` | CI green; the kitty-flags bare-`q` check from T2.5b passes with a config loaded; a forced panic writes a report; **[owner]** starts the dogfood week |

**Exit gate:** **[owner]** a week of daily use in both Ghostty and WezTerm,
logged in `docs/dogfood.md` with the terminal named per entry. The log feeds the
task breakdown for M4 and M5.

---

## M3.5: import-tmux

Starts once T3.6 is merged and the dogfood week has begun; finishes before M3.6
starts. One task.

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TI.1** `grofe import-tmux` | Spec §5.5: read a tmux.conf, following `source-file`; convert `set`/`set-option` of options grofe has, `bind-key`/`unbind-key` with `-n`, `-r`, `-T prefix`/`-T root`, `prefix`, `mouse`, `base-index`, and `status-left`/`status-right` formats without `#()`; write TOML to stdout or `-o <path>`; report every unconverted line with file, line and reason on stderr (`#()` in formats, `if-shell`, `run-shell`, hooks, plugins and TPM, unknown options); never write to the tmux.conf; `import-tmux` arm in `main.rs` | `src/import_tmux.rs`, `import-tmux` arm in `src/main.rs`, `tests/import_tmux.rs`, `tests/fixtures/import-tmux/` | T3.1, T3.5 | Golden-file tests over two fixtures, a canopy-style layered config and a typical dotfiles tmux.conf: the output matches the golden TOML; `show-config --file` on the output loads with no errors; the report lists every unconvertible line with its reason; the input files are byte-identical afterwards |

**Exit gate:** reviewer runs the golden tests on both OSes; **[owner]** runs it
once on their own tmux.conf and reads the report.

---

## M3.6: your-setup compatibility

Spec D1, D12, §4.5. Goal: with `tmux` on PATH resolving to grofe and the
owner's daily config ported (Wave C), the owner's scripts, tools and daily
bindings behave as they do under tmux 3.7b. Starts when M3.5 closes. Real tmux
stays installed as the fallback until M5 closes, run as
`env -u TMUX <absolute path to tmux>` because `$TMUX` inside a grofe pane
points at grofe (spec D12).

**Before Wave A starts:**

- **Stub edit** by the orchestrator on `main` (rules 3 and 4): the new files in
  the crate layout, plus `regex`; a `Command` variant with an empty flag struct
  for every verb in spec §4.5's compat table (TC.2 fills the fields); and
  routing in `server/dispatch/mod.rs` for every new verb to a stub handler
  returning `not implemented: <verb>`, placed in the file of the task that owns
  it: `dispatch/hooks.rs` (TC.5), `dispatch/structure.rs` (TC.8),
  `dispatch/client.rs` (TC.9), `dispatch/menu.rs` (TC.7), `dispatch/popup.rs`
  (TC.6), `dispatch/bindings.rs` (TC.10). After the stub edit no M3.6 task
  edits `dispatch/mod.rs`. New flags on verbs that already exist are handled
  in those verbs' existing handler files, by the task named in their row.
- T3.6's `TERM` re-run has passed, or its gaps are listed as blockers for the
  exit gate (spec D7).
- TC.0 and TC.0b are merged.

**Follow-ups to approved M1 contracts.** M3.6 extends these approved M1
contracts. Every extension is additive: no approved M1 branch is reopened and
nothing is reworked mid-M1. M1 closes on its own gate; each extension is an
M3.6 task that lands on merged `main` after M3. Each such task's acceptance
includes "every M1 test green, unchanged except the assertions this table
names". Checked against T1.2's approved head (`feb8fc6`): `x:` is already
accepted (`trailing_colon_means_session_current_window`), and no T1.2 test
asserts that `x:` or `=x` is rejected.

| Approved M1 contract | Extension | Kind | M3.6 task |
|---|---|---|---|
| Target grammar: `Target`, `parse_target` (T1.2) | New branches `=name` (exact session, never a prefix) and `=name:window`; bare `=` refused with an error naming it (spec §4.5) | **Contract edit:** `=x` parses today as the literal session name `=x`, untested; TC.2 changes that meaning and pins it with a new test. Every other form parses as before, and T1.2's `sess:%N` ruling stands | TC.2 |
| `ParseError` and the flag tables (T1.2) | New verbs, aliases, prefix matching and flags from spec §4.5's compat table | Additive | TC.2 |
| `ParseError` texts (T1.2) | Every text becomes tmux 3.7b's, under both identities (spec D12), compared against goldens | **Additive with test update:** where a T1.2 text differs from its golden, the text and its T1.2 assertion change in the same commit and TC.2's report lists each one (candidates: `parse.rs`'s `unknown command: foo` and `command kill-server: unknown flag -z` tests) | TC.2 |
| Option store and `set`/`show` (T1.5c) | tmux option names, `-a -F -o -q -u`, `-ga`/`-as` append, array options (`status-format[N]`, `command-alias[N]`), no-effect options | Additive | TC.2b |
| `FormatVars` for the model (T1.5c) | The inventory's variables, `version` per identity, `grofe_version` | Additive | TC.3 |
| Pane environment: `PaneSpawn.env` and the post-env block that sets `TERM`, `COLORTERM`, `GROFE`, `GROFE_PANE` (T1.4) | `TMUX`, `TMUX_PANE` in the same block under both identities; `TERM_PROGRAM=tmux` and `TERM_PROGRAM_VERSION` when the server has tmux identity; `GROFE` and `GROFE_PANE` unchanged | Additive | TC.1 |
| CLI: `src/main.rs` and its subcommand table (T1.1b) | An `argv[0]` check before the table; tmux's global flags (`-L -S -f -V -2 -u`); bare `tmux` as `new-session`. `\;` chains are T3.5's, and TC.1 only verifies them under the `tmux` name | Additive | TC.1 |
| Event emission sites (T1.6) | The internal events behind spec §4.5's hook names | Additive | TC.5 |

**Wave 0 (may start during M1 to M3; needs only real tmux).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TC.0** argv corpus | `scripts/tmux-argv-recorder.sh`: a `tmux` wrapper that appends its argv to a log and execs real tmux by an absolute path fixed at install time (never through `PATH`, so it cannot recurse); install notes: put it first on `PATH`, then restart the tmux server from a shell with that `PATH`, so `run-shell`, hooks and popups resolve `tmux` through the recorder too; `scripts/normalize-corpus.sh`: replaces paths, hostnames, session and window names and user option values with placeholders and dedupes; `docs/compat-coverage.md` maps each distinct verb and flag set to a spec §4.5 row or out-of-scope row, and notes `~/.claude/hooks/tmux-live-guard.sh` (its regex on `tmux ... kill-*` without `-L`/`-S` keeps matching under the alias, so it now guards grofe servers too). **[owner]** runs the recorder for at least three working days | `scripts/tmux-argv-recorder.sh`, `scripts/normalize-corpus.sh`, `tests/fixtures/compat/corpus.txt`, `docs/compat-coverage.md` | none | A check script exits nonzero if any corpus line maps to no §4.5 row; the corpus contains no string from a denylist built at check time (`$HOME`, hostname, git user email), keeping it publishable; the log shows at least one argv sent from a hook or `run-shell` (proof the restart took) |
| **TC.0b** Golden recorder | Spec §9's recording rules: `scripts/record-compat-goldens.sh` drives `tests/compat/record.rs` (an ignored test) against real tmux reached as `env -u TMUX <absolute path>`; it refuses to run unless that tmux's `-V` prints `tmux 3.7b`; `env -i` with fixed `SHELL=/bin/sh`, `TERM`, `LANG`; `TMUX_TMPDIR` and a private `-L` socket in a tempdir; `-f tests/fixtures/compat/tmux-defaults.conf`, which sets grofe's spec §5.3 values (`base-index 1`, `pane-base-index 1`, `renumber-windows on`, `history-limit 50000`, `escape-time 25`, `mouse on`) so ids, indices and sizes match; a pty-attached client from T2.4's harness for scenarios that need one (popups, status line); one normalization list (`normalize.txt`: socket paths, pids, timestamps, tty names) shared by recorder and suite; no strftime in any fixture; records `tmux list-commands` as the alias-table golden. Every later task that asserts a golden records it with this tool into `tests/fixtures/compat/golden/<task>/` | `scripts/record-compat-goldens.sh`, `tests/compat/{main,record,normalize}.rs`, `tests/fixtures/compat/{normalize.txt,tmux-defaults.conf}`, `tests/fixtures/compat/golden/list-commands.txt` | none | Recording twice in a row gives byte-identical goldens; with a different tmux version on the path the recorder exits nonzero naming it; a recorded pane's `env` holds only the fixed variables plus tmux's own; the sockets under the user's real tmux and grofe directories are identical before and after a recording |

**Wave A (parallel after the stub edit; merge order TC.2, TC.3, TC.1, TC.12).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TC.1** `tmux` name and identity | `compat.rs`: `argv[0]` basename detection, the compat level constant (`3.7b`), the `TMUX` value builder (`<socket path>,<server pid>,<session id number>`), identity fixed at server start from the starting CLI's `argv[0]` with the `compat-identity` server option (`auto`, `tmux`, `grofe`; one schema key in `src/config/`, additive), and the `compat-miss.log` writer that TC.2 and TC.3 call; `main.rs`: the `argv[0]` check, global `-L -S -f -V -2 -u`, bare `tmux` as `new-session`; pane env per spec D12 and §4.2; the self-nesting rule in `client.rs` and the attach path (spec §4.6); README's "put `tmux` on PATH" step and fallback note | `src/compat.rs`, `argv[0]` and global flags in `src/main.rs`, the env block in `src/server/pane/mod.rs`, the nesting check in `src/client.rs`, `compat-identity` in `src/config/`, README's install section, `tests/compat_identity.rs` | stub edit | Through a `tmux` symlink in a temp `PATH`: `tmux -V` prints `tmux 3.7b` and `grofe -V` grofe's version; bare `tmux` in a pty starts a server and attaches a new session; in a pane, `$TMUX` equals `#{socket_path},#{pid},` plus the session id number, and `$TMUX_PANE` equals `$GROFE_PANE`; a server started by `tmux new -d` gives panes `TERM_PROGRAM=tmux`, and a later `grofe` CLI call does not change that; a server started by `grofe new -d` gives the outer value; `compat-identity tmux` in config overrides a `grofe` start; `tmux -S "${TMUX%%,*}" display -p '#{pane_id}'` from a pane prints `$TMUX_PANE`; `tmux set -p -t "$TMUX_PANE" @a 1 \; set -p -t "$TMUX_PANE" @b 2` sets both (verifying T3.5's chains under the tmux name); attach from a pane to another session succeeds, to its own session fails with tmux's text; every M1 test green per the contract table |
| **TC.2** CLI breadth | Parse every verb and flag in spec §4.5's compat table into the stubbed `Command` variants; tmux 3.7b's alias table (from TC.0b's `list-commands` golden) and prefix rule; targets `=name`, `=name:window`, bare `=` refused; the tmux option name table moved from `import_tmux.rs` into `cmd/tmux_names.rs` (contract edit: TC.2 co-owns `import_tmux.rs` for that move); tmux 3.7b's error texts and exit statuses everywhere (contract table); under tmux identity, an unknown verb or flag is also appended to `compat-miss.log` | `src/cmd/{parse,target,mod,tmux_names}.rs`, the table move in `src/import_tmux.rs`, `tests/compat_parse.rs` | stub edit, TC.0b | Every argv in `corpus.txt` parses (the test iterates the file); every alias in the golden resolves to its command, and `tmux a` resolves to `attach-session`; an ambiguous prefix errors as tmux's golden; `=x` never matches session `xy`; `=x:2` is window 2 of exactly `x`; bare `-t =` errors naming the mouse target; each out-of-scope form errors naming the form; every `ParseError` text equals its golden; an unknown flag under tmux identity adds one line to `compat-miss.log`; T1.2's tests green per the contract table; TI.1's goldens unchanged |
| **TC.3** Format breadth | `cmd/format.rs`: short forms, the modifiers, bare-name conditions, `##`/`##{` escaping per spec §4.5's formats row; `#(cmd)` parses to a job lookup `FormatVars::job(cmd) -> Option<String>` (contract edit, answered by TC.4); the model's `FormatVars` gains the inventory's variables, `version` per identity and `grofe_version`; an unknown variable under tmux identity is appended to `compat-miss.log` | `src/cmd/format.rs`, the `FormatVars` impl in `src/server/dispatch/options.rs`, `tests/compat_formats.rs`, `tests/fixtures/compat/formats/` | stub edit, TC.0b | Every format string from the inventory, expanded against a fixed scripted model (two sessions, three windows, splits, user options), equals tmux 3.7b's expansion of the same format against the same model (goldens, normalized); `#{version}` is `3.7b` under tmux identity and `#{grofe_version}` grofe's under both; T1.2's and T2.6's format tests unchanged and green |
| **TC.12** `DCS tmux;` unwrap | Spec §4.9: unwrap `DCS tmux; ... ST` (with doubled ESC) in T3.3's scanner; a wrapped APC `_G` goes to T3.8's graphics path, a wrapped sequence in the §4.9 set is re-emitted, anything else dropped | the unwrap in `src/server/passthrough.rs` (T3.3 merged), `tests/acceptance/tc_12.rs` | stub edit | A kitty graphics transmit wrapped for tmux produces the same outer stream as the unwrapped one; a wrapped sequence split across two reads still unwraps; an unknown wrapped sequence reaches neither the emulator nor the client |

**Wave B (after Wave A; a task starts when its Depends are merged; merge order
TC.2b, TC.4, TC.5, TC.8, TC.11, TC.7, TC.9, TC.10, TC.6, so TC.11 lands before
TC.9 and TC.10).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TC.2b** Option store breadth | Spec §4.5's options row: `set`/`show` through TC.2's name table, `-a -F -o -q -u`, `-ga`/`-as` append, array options with `[N]`, no-effect options marked for `show-config --explain`, and the listed effects (`main-pane-width` read by `main-vertical`) | additions to `src/server/dispatch/options.rs` (after TC.3's, same file), `tests/compat_options.rs` | TC.2, TC.3 | `set -g status-left x` then `show -gv status-left` prints `x`; `set -ga update-environment FOO` appends; `set -g status-format[1] ""` then `show -gv status-format[1]` is empty and `status-format[0]` keeps its default; `set -s command-alias[0] 'wk=list-keys'` stores; `set -g terminal-features ...` succeeds and `show-config terminal-features --explain` says "no effect"; `set -g nosuch 1` errors with tmux's text; each case's output equals its golden |
| **TC.4** Status sources | Spec D8, §4.10: registry keyed by name or inline command; scheduled at `status-interval` or the source's `interval`; one run in flight; timeout kills the process group; spawn under the pane spawn lock (T1.4's deferred `pub(crate) spawn_lock()`); first output line stored in `@<name>` or `@src-<hash>`; a changed value triggers a repaint; `[status.sources.<name>]` in the schema (T3.1 merged, additive) | `src/server/sources.rs`, one registration line in `src/server/mod.rs`, the `status.sources` schema in `src/config/`, `tests/compat_sources.rs` | TC.3 | With `status-interval 1` and frames forced at 60 Hz on two attached clients, a source that appends to a counter file runs 10 or 11 times in 10 s; a source that sleeps 30 s never delays a frame (every frame interval under 50 ms) and is killed at its timeout; the segment is empty until the first result, then shows the first line; two formats using the same `#(cmd)` share one run; `show -v @batt` returns a named source's last value |
| **TC.5** Hooks and environment | `set-hook -g/-gu`, `show-hooks` for the twelve hook names in spec §4.5, each run through the dispatcher on its internal event with the event's target as context (T1.6's emission sites, additive); `run-shell -b` (the flag in T3.4's `run-shell` handler); `set-environment -g -u -h` and `show-environment -g -h`, session over global, reaching new panes, `run-shell`, hooks, popups and sources (spec §4.2) | `src/server/hooks.rs`, `src/server/dispatch/hooks.rs`, one registration line in `src/server/mod.rs`, `-b` in the `run-shell` handler, `tests/compat_hooks.rs` | TC.2, TC.0b | Each hook fires exactly once per triggering action, one table row per hook, with the event's `#{pane_id}` and `#{session_name}`; `set-hook -gu` removes it; a hook running `run-shell -b 'sleep 5'` does not delay the next command; a `set-environment -g` variable appears in a new pane and a `run-shell`; where a `-h` variable appears (pane, `run-shell`) matches tmux 3.7b's golden; `-u` removes; `show-environment -g` output equals its golden |
| **TC.8** Structure verbs | `join-pane`, `swap-pane`, `swap-window`, `break-pane`, `resize-window`, `last-pane`, `rotate-window -U -D`, `move-window -t`, `next-window -a`, `previous-window -a` (new verbs in `dispatch/structure.rs`); `resize-pane -x -y`, `new-window -n <name> [cmd]`, `respawn-pane -k <cmd>`, per-window `automatic-rename`, `select-layout -E`, `main-pane-width` in `main-vertical`, and tmux layout strings read by `select-layout` and written by `#{window_layout}` (spec §4.3) | `src/server/dispatch/structure.rs`, additions to `src/server/layout.rs` (T1.3, T3.7 merged) and `src/server/dispatch/{pane,window}.rs`, `tests/compat_structure.rs` | TC.2, TC.3, TC.0b | After the same scripted splits at 200x50, `#{window_layout}` equals tmux 3.7b's golden byte for byte; `select-layout '<golden string>'` reproduces the golden `list-panes -F '#{pane_left} #{pane_top} #{pane_width} #{pane_height}'`; join, swap, break, rotate, move, `select-layout -E` and `main-vertical` with `main-pane-width 50%` each match their golden `list-panes -a -F`; `last-pane` and `next-window -a` select what tmux's goldens select; T1.3's tiling proptest holds after each operation |
| **TC.11** Status breadth | `status 2` with `status-format[N]`, `status-position top`, `status-justify`, lengths, window-status formats and separator, `#[range]` and `#[align]`, `client_prefix` colour, a status-line message area for `display-message` without `-p` (used by TC.9), repaint within one frame on an option change (spec §4.10) | additions to `src/server/status.rs` (T2.6 merged), `tests/compat_status.rs` | TC.3, TC.2b, TC.0b | Two status rows drawn at the top with `status-format[1] ""`; for the canopy fixture theme, the rendered status (through the emulator, pty-attached client on both sides) matches tmux 3.7b's cell for cell with `@src-*` values set to fixed strings; the `client_prefix` colour shows only while the prefix is pending; `set @theme_blue` repaints on the next frame; overlong left and right parts truncate as the goldens |
| **TC.7** Menus, prompts, aliases | `display-menu -T -x -y` with item keys and separators, an item that runs `display-menu` opening a nested menu, and a caller-supplied position (TC.10 passes the pointer); `display-panes`; `confirm-before -p`; `command-prompt -I -p -k -T`; `list-keys -1N` (the flags in T3.5's `list-keys` handler); the `command-alias` option, expanded before parsing (contract edit: TC.7 co-owns `cmd/parse.rs` for alias expansion, TC.2 merged) | `src/server/menu.rs`, `src/server/dispatch/menu.rs`, one registration line in `src/server/mod.rs`, additions to `src/server/prompt.rs` (T3.4 merged), `-1N` in the `list-keys` handler, alias expansion in `src/cmd/parse.rs`, `tests/acceptance/tc_7.rs` | TC.2, TC.2b | A menu item's key runs its command; a nested menu opens from an item and Escape closes both; `display-panes` then a digit selects that pane; `confirm-before -p 'kill? (y/n)' kill-pane` kills only on `y`; `command-prompt -I '#W' 'rename-window %%'` renames with the old name prefilled; the stock `/` binding (`command-prompt -k` then `list-keys -1N`) shows the pressed key's binding; `set -s command-alias[0] 'wk=display-menu -T x a a "display -p a"'` makes `wk` a command |
| **TC.9** Client and session verbs | `new-session -A`, `has-session` exit statuses, `switch-client -c -t` and `-T`, `detach-client -s`, `display-message` without `-p` (in TC.11's message area, logged for `show-messages`), in the existing handlers; new verbs in `dispatch/client.rs`: `refresh-client -S`, `show-messages` and its alias `info`, `choose-client -Z`, `suspend-client`, `find-window -Z`, `choose-tree -Z -w` | additions to `src/server/dispatch/session.rs` and `src/server/clients.rs`, `src/server/dispatch/client.rs`, `src/server/tree.rs`, `tests/acceptance/tc_9.rs` | TC.2, TC.3, TC.11 | `new-session -A -s x` twice leaves one session and the second call attaches; `has-session -t =x` exits 0 for `x` and 1 when only `xy` exists; `switch-client -c <client> -t s` moves only that client; `detach-client -s s` detaches every client on `s`; `switch-client -T t` then a key runs table `t`'s binding and returns to root; `display-message hi` shows in the status line and then in `show-messages`; `choose-tree -Zw` zooms, lists windows under sessions, and Enter switches; `find-window -Z foo` lists only matching windows; `suspend-client` stops the attach client with SIGTSTP and `fg` resumes it; `info` exits 0 with a server and 1 without |
| **TC.10** Bindings and mouse | `if-shell` in both forms (new verb in `dispatch/bindings.rs`; shell form off the actor, `-b` not waiting); spec §4.5's built-in mouse model: bindable mouse keys acting on the pane or status range under the pointer, status range clicks setting `mouse_status_range`, border drag resize, a `display-menu` from a mouse key opening at the pointer; `bind -N`; custom tables | `src/server/dispatch/bindings.rs`, additions to `src/server/input.rs` and `src/server/mouse.rs` (T3.2 merged), `tests/acceptance/tc_10.rs` | TC.2, TC.3, TC.7, TC.11 | `bind -n C-h if -F '#{m/r:^n?vim$,#{pane_current_command}}' 'send C-h' 'select-pane -L'` sends C-h into nvim and moves focus from a shell; a shell-form condition that sleeps 2 s does not delay a second client; `MouseDown1Status` bound with `if -F '#{m:*net*,#{mouse_status_range}}'` fires only on the `net` range; dragging a border resizes both panes; `DoubleClick1Pane` fires; a `MouseDown3Pane` binding running `display-menu` opens the menu at the pointer cell |
| **TC.6** Popups | `display-popup` with every flag in spec §4.5's overlays row, as an overlay plus input hook (T2.5a); the popup's command runs in its own pty with the environment tmux gives a popup; `-E` closes on exit, otherwise a key closes it; nested attach from a popup (spec §4.6) | `src/server/popup.rs`, `src/server/dispatch/popup.rs`, one registration line in `src/server/mod.rs`, `tests/acceptance/tc_6.rs` | TC.1, TC.2, TC.5, TC.0b | `display-popup -E -w 80% -h 60% -d /tmp -b rounded -T t 'pwd; read'` draws a rounded border titled `t` at that size and prints `/tmp`; `-c <client>` opens on that client only; `fzf --tmux` from a pane opens in a popup and its selection reaches the caller (skipped when the installed fzf lacks `--tmux`); inside a popup `tmux attach -t other` shows `other`, and detaching returns to the popup's caller; `env` inside a popup matches tmux 3.7b's golden after normalization, including where a `set-environment -h` variable appears |

**Wave C: the owner's daily config (after Wave B; TP.0 first, then TP.1 and
TP.2 in parallel).** Moved from M3.7 so alias day runs the owner's ported
config, not grofe's defaults. TP.1 and TP.2 own files in canopy and athome, run
there with their own bats tests, and each has a grofe-side check.

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TP.0** import-tmux learns M3.6 | Spec §5.5's "with M3.6" list: `#()` to status sources, `if-shell`, `set-hook`, `set-environment`, `command-alias`, custom tables, mouse bindings, `display-popup`/`display-menu` bindings, `status 2` rows | `src/import_tmux.rs`, `tests/fixtures/import-tmux/` | all of Wave B | On the canopy-style fixture the report shrinks to plugin and TPM lines and spec §4.5 out-of-scope items; the output loads with `show-config --file`; goldens updated in the same change |
| **TP.1** canopy core and the owner's user config | canopy's conf.d `00-core`, `05-caps`, `10-keys` (from `keys.tsv`), `20-status`, `30-plugins`, and athome's `user.conf`, drafted with TP.0: canopy's generator emits `conf.d/NN-canopy-*.toml` (bindings with `name`, `desc`, `group`, `repeat`); two status rows at the top with pills and themes as options; canopy-status, pet, network and battery as named status sources; `user.conf` (including `B g G M N P T W K ?`) becomes athome's `user.toml`; `canopy validate` becomes `show-config --file` (spec §7.1) | canopy's grofe backend and generator; athome `dot_config/grofe/` | TP.0 | `grofe show-config --file` on the generated tree loads with no errors; the rendered status matches tmux's rendering of the same theme cell for cell (sources set to fixed strings); every binding in canopy's `keys.tsv` and the owner's `user.conf` has a grofe binding (a subset of TP.9's check, run here) |
| **TP.2** vim-tmux-navigator | root `C-h/j/k/l/C-\` generated into canopy's `conf.d/NN-canopy-nav.toml` as `if -F` on `#{pane_current_argv}` (matching `nvim`, `vim`, `fzf` and the owner's list), replacing the plugin's `if-shell` plus `ps` per keypress; Neovim's side unchanged (it calls `tmux -S <$TMUX socket> select-pane`) | canopy's nav fragment and its generator; `tests/acceptance/tp_2.rs` in grofe | TC.10 | C-h in a shell moves focus left; in nvim it reaches nvim, also when nvim runs under a wrapper whose comm differs; nvim at its left edge moves focus through its own `tmux` call; no move while zoomed, as the plugin's default; zero processes spawned per keypress (counted) |

**Wave D.**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TC.13** Compat acceptance suite | Spec §9's compat bullet, using TC.0b's recorder, normalizations and defaults fixture on both sides; the suite runs every scenario with `tmux` on the temp `PATH` pointing at grofe and the ported config from Wave C loaded. Scenarios: canopy `bin/*` (the canopy-agent hook path, canopy-status), `tsess`, `cockpit`, `fzf --tmux` (through the acceptance harness), vim-tmux-navigator's `tmux` calls from a headless Neovim, a smug-style start (`has-session`, `new -d`, `new-window`, `split-window`, `select-layout main-vertical`, `send-keys`), and a replay of every corpus argv. canopy and athome are found through `GROFE_COMPAT_CANOPY` and `GROFE_COMPAT_ATHOME` (skipped loudly when unset); CI checks both out at pinned SHAs in a `compat` job. Where tmux is not deterministic, the scenario asserts structure instead, and the file says so per scenario | `tests/compat/scenarios/`, `tests/fixtures/compat/golden/tc_13/`, the `compat` job in `.github/workflows/` | Waves A to C | Every scenario's normalized output is byte-identical to its golden; the corpus replay gives no `unknown` or `not implemented` error and leaves `compat-miss.log` empty; `command -v tmux` inside the suite resolves to the grofe symlink and `tmux -V` prints the compat level; editing one golden turns the suite red; the sockets under the user's real tmux and grofe directories are identical before and after a run |

**Critical path inside M3.6** (review cycles included): stub edit → TC.2 (4 d)
→ TC.2b (2 d) → TC.11 (3 d) → TC.10 (3 d) → TP.0 (3 d) → TP.1 (4 d) → TC.13
(4 d), about 23 working days; TC.2 → TC.5 → TC.6 (11 d) runs beside it and
also gates TP.0. TC.0 and TC.0b run before the stub edit, during M1 to M3.

**Exit gate (alias day):** reviewer runs the compat suite on both OSes from a
clean clone; CI `compat` job green; **[owner]** puts the `tmux` symlink first
on PATH with the Wave C config and works a week. Every difference noticed goes
in `docs/dogfood.md` tagged `compat`, and each becomes a TC.13 scenario before
it is fixed; `compat-miss.log` is read at the end of each day. The plugin
bindings (sessionx `s`, tmux-fzf `F`, fzf-url `u`, which-key `Space`,
opensessions `o`, claude-session-manager `a`/`A`) arrive with M3.7 and are
logged as known gaps until then.

---

## M3.7: port the remaining plugins

Spec D1, §7.1. The owner's remaining TPM plugins move once to grofe-native
equivalents; no `tmux.conf` or TPM plugin runs. Starts after M3.6's exit gate,
as a wave beside M4 (see the recommended order), and cannot close before M5,
because TP.5 (yank) and TP.9's `copy-mode-vi` rows need copy mode. Tasks that
own files in canopy or athome run in those repos with their own bats tests;
each also has a grofe-side check named in its row. TP.3, TP.4, TP.6, TP.7 and
TP.8 in parallel; TP.5 after M5's copy mode; TP.9 last.

| Task | Plugin or piece | grofe-native replacement | Owns | Depends | Accept |
|---|---|---|---|---|---|
| **TP.3** tmux-which-key | nested `display-menu` built from `@wk_menu_*`, `command-alias`, `setenv -h` | `display-menu --keys <table> [group]`, a grofe long flag (like `--persist`; no tmux script sends it), building nested menus from bindings' `desc` and `group`; canopy's menu data becomes `group` fields | `--keys` in `src/server/menu.rs`, canopy's `group` fields, `tests/acceptance/tp_3.rs` | M3.6 | `prefix Space` opens a menu listing every described prefix binding grouped as in `keys.tsv`; a group item opens its submenu; an item runs its binding |
| **TP.4** sessionx, tmux-fzf, tmux-fzf-url | fzf-tmux popups over `list-*`, `capture-pane -ep`, `swap-*`, buffers | Small scripts in canopy's grofe backend: `display-popup -E` running fzf over `list-sessions -F`, `list-windows -a -F` and `capture-pane -ep`, bound to `s`, `F` and `u`; sessionx's `unlink-window` flow dropped (spec §4.5) | canopy `grofe/bin/` scripts and their bats | M3.6 | bats: each script, with fzf driven by `--filter`, sends the expected `switch-client`, `kill-session` or open command to a scripted server; the compat suite runs each once through a real popup |
| **TP.5** tmux-yank | `copy-mode-vi` tables, `copy-pipe*`, `paste-buffer -p`, `set-buffer` | `y`, `Y` and `MouseDragEnd1Pane` bound to `copy-pipe-and-cancel` with `pbcopy` on macOS and `wl-copy` on Linux, plus OSC 52 | canopy's copy-mode bindings | M5 copy mode | A selection copied in copy mode reaches a stub clipboard command and the outer pty as OSC 52; `prefix ]` pastes it with bracketed paste in a pane that asked for it |
| **TP.6** opensessions | sidebar pane, twelve hooks with `run-shell -b` curl and `tmux -S #{socket_path}`, the `opensessions` key table, `@opensessions_width` | Its tmux-side config ported to TOML hooks and a custom table; its program runs unchanged against grofe | canopy/athome port files, a `tests/compat/scenarios/` opensessions scenario | M3.6 | Against a recording endpoint, a scripted session fires the same hook calls in the same order as tmux's golden (recorded with TC.0b); the sidebar opens through `join-pane` at `@opensessions_width` and keeps it across `resize-window` |
| **TP.7** claude-session-manager | nested attach in `display-popup`, `switch-client -c`, `detach-client -s` | `a` and `A` open `display-popup -E` running `tmux attach -t <agent session>` | canopy/athome port files, `tests/acceptance/tp_7.rs` | M3.6 | From a pane in `main`, `prefix a` shows the agent session in a popup; detaching closes the popup and leaves `main` attached; attaching to `main` from its own popup is refused |
| **TP.8** sensible, TPM, resurrect, continuum | sensible's options and `bind-if-unset`; TPM's `@plugin` lines and `start-server`; resurrect and continuum's save strategy and `continuum_save.sh` `#()` | sensible: grofe defaults (spec §5.3) or TP.1's settings; TPM: nothing, not ported; resurrect and continuum: M4, and `continuum_save.sh` leaves the status line | canopy port files, `scripts/check-sensible.sh` | M3.6 | The check script lists every option sensible sets and finds it at the same value in `grofe show-options -g`, or names the deliberate difference; no `@plugin`, `@resurrect-*` or `@continuum-*` line remains in the port |
| **TP.9** Parity | all tables | `scripts/key-parity.sh` compares real tmux's `list-keys` (old config, private socket, `env -u TMUX`) with `grofe list-keys` per table: `prefix` (114), `root` (22), `copy-mode-vi` (93), `opensessions` (13); stock mouse bindings are matched against the built-in mouse model and stock `m` against its out-of-scope reason (spec §4.5) | `scripts/key-parity.sh`, the reasons column in `docs/compat-coverage.md` | TP.3 to TP.8 | The script exits 0: every tmux binding has a grofe binding doing the same thing, or a listed reason; **[owner]** each daily binding (`s`, `K`, `?`, `u`, `g`, `G`, `P`, `T`, `W`, `a`, `A`) used once and ticked in `docs/dogfood.md` |

**Exit gate:** reviewer runs TP.9 and the compat suite; **[owner]** a week under
the alias with every plugin ported, `compat` entries in `docs/dogfood.md` all
closed or scheduled. The `copy-mode-vi` rows of TP.9 and TP.5 close with M5,
so M3.7 closes with M5.

---

## M4 to M8 (milestone level)

**M4 Persistence and pane identity** (spec §6). Snapshot writer with version
(no attached client needed), restore into an empty server, `pane-set-resume`
with client-side `argv[0]` resolution and binding to the setter's process,
respawn to a shell with the same `uid` when a resumed process exits,
`--persist` options, `restore-on-start` (default on), deterministic stagger,
`reboot-check` verdicts and exit codes, env on first attach. Tests adopt
canopy's fake-agent fixture and reboot scenarios (spec §9), each proven able to
fail by mutating the snapshot. Tests that pin the canopy lessons:

- a server started under `env -i PATH=/usr/bin:/bin` still runs a resume whose
  command lives elsewhere on the caller's `PATH`;
- a resume set from a foreground agent is cleared when the agent exits and the
  shell stays, so a restore starts a shell, not the closed conversation;
- a resumed process that exits leaves a shell in `cwd` with the same `uid`;
- stagger is identical across two runs for the same `uid`s, `0` spawns with no
  delay, and panes without a resume are not delayed;
- `reboot-check` exits 0, 1 and 2 in the three spec §6 cases, reading the
  snapshot on disk (a snapshot edited behind the server's back changes the
  verdict).

Done: kill the server, restore, every `uid` unchanged and every pane with
`resume` runs it; **[owner]** the same across a real reboot. Under the alias
this is also the replacement for resurrect and continuum (TP.8), so M4 runs
right after M3.6, beside M3.7.

**M5 Copy mode and buffers** (also closes M3.7). First a one-page scope doc (keymap, selection
model across wrapped lines, search, copy targets), **[owner]** approved, built
around the owner's `copy-mode-vi` table (93 bindings, spec §4.5 buffers row).
Then copy mode, `command-prompt -T search`, buffers (`set-buffer`,
`paste-buffer -p`, `choose-buffer`), `copy-pipe`, `copy-pipe-and-cancel`,
`copy-pipe-no-clear`, wheel-into-copy-mode and double/triple-click selection,
`#{copy_cursor_word}`; then TP.5 (yank) and TP.9's `copy-mode-vi` rows.
`display-popup` and `display-menu` moved to M3.6. Done adds: **no-difference
parity** for the owner's workflow, **[owner]** confirmed.

**M6 Events and plugins.** Rewrite spec §7 from the dogfood log and the events
actually needed. Freeze protocol v1. Plugin supervision, Lua runtime, format
functions.

**M7 agents plugin and canopy on grofe** (spec §7, §7.1). Port canopy's
adapters to the spec §7 adapter contract (manifest keys, id validation on entry
and use, hooks that always exit 0, `pane-set-resume` on every session id
change), the "restored/unknown" state after a restore, `adopt` (no respawn,
dry-run default, idempotent), canopy's grofe backend. Done: **[owner]** a
real reboot returns every Claude Code pane to its own conversation.

**M8 Compatibility extras.** Decide after M7 whether a tmux `-CC` subset earns
its keep. (`import-tmux` moved to M3.5.)
