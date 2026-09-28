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
| **M3.6** | tmux compatibility for the owner's workflow (spec D1, D12, §4.5): the `tmux` name and server identity, CLI/target/format/option breadth, cached `#()` status sources, hooks and server environment, popups and menus (moved from M5), multi-row status, all reached through a table-driven tmux front end (`src/compat/`), the owner's daily config ported (canopy, `user.conf`, navigator), compat suite | **Alias day.** The compat suite is green in CI against goldens recorded from tmux 3.7b; **[owner]** `tmux` on PATH resolves to grofe with the ported config for a week of normal work, real tmux kept as `env -u TMUX <absolute path>` as the fallback | ~6.5 weeks |
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
TC.11 → TC.10 → FE.4 → TP.0 → TP.1 → TC.13, about 25 working days with
review cycles. FE.1 lands first; FE.2, FE.3 and FE.4 then run in parallel, one
file each under `src/compat/tmux/`.
**Estimate, honestly, assuming at least three parallel implementer lanes**
(native Wave B has five tasks that can start at once, beside the FE tasks):
alias day about **week 14.5** from the start (7 to dogfood start, the dogfood
week with M3.5, about 6.5 for M3.6: 5 weeks of path work plus 30%); M4
alongside M3.7's parallel tasks ends about week 16; M5, which also closes
M3.7 through TP.5 and TP.9's copy-mode rows, ends about week **18.5**, and with
it no-difference parity. Against the plan before the compat decision (M5 done
at about week 11), that is about **7.5 weeks more**: M3.6 about 6.5, M3.7 about 1.5
of which one overlaps M4, and TP.5 inside M5. With one lane, M3.6 alone is
about 69 task-days (the one-lane sum in the M3.6 section), roughly 18 weeks
with the buffer, and alias day moves to about week 26 (splitting the front end
saves nothing in one lane).

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
  compat/
    mod.rs           # M3.6: argv0, identity, dialect, TMUX value, compat-miss log (TC.1)
    tmux/
      mod.rs         # M3.6: table schema, dispatcher, area registration, aliases and
                     #   prefix, global flags, -V, error texts, option names (FE.1)
      formats.rs     # M3.6: format rewriting, #() to sources, option, status and hook rows (FE.3)
      bindings.rs    # M3.6: if-shell, command-alias, popup and menu flags, list-keys (FE.4)
      structure.rs   # M3.6: structure and client verbs, layout codec, list-* output (FE.2)
  config/
    mod.rs           # M3
    provenance.rs
  import_tmux.rs     # M3.5: tmux.conf to TOML converter (TI.1)
tests/
  common/mod.rs      # TestServer harness
  cli.bats           # bats smoke suite
  acceptance/main.rs # M2+
  compat/            # M3.6: recorder and harness (TC.0b), scenarios (TC.13)
  compat_frontend*.rs # M3.6: front-end row goldens (FE.*)
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

### M2 seams (T2.0; filled by T2.2, T2.3, T2.5a, T2.6)

Landed by T2.0 as stubs; these names, fields and signatures are binding.
Pane rects, borders and `FrameInputs.cursor` are window coordinates; a
`Screen` is client coordinates.

```rust
// types.rs
pub struct ClientId(pub u32);
// server/pane/mod.rs
pub struct SnapCell { pub c: char, pub fg: Color, pub bg: Color, pub flags: CellFlags,
                      pub zerowidth: Vec<char>, pub underline_color: Option<Color> /* SGR 58 */ }
pub enum CursorShape { Block, Underline, Bar }
pub struct CursorState { pub row: u16, pub col: u16, pub visible: bool, pub shape: CursorShape, pub blink: bool }
pub struct RowsSnapshot { pub rows: Vec<Vec<SnapCell>>, pub cursor: CursorState }  // one lock
impl PaneHandle { pub fn cursor(&self) -> CursorState; }   // panes run with kitty_keyboard on
// protocol.rs
pub struct Caps { pub truecolor: bool, pub styled_underline: bool, pub sync: bool,
                  pub kitty_keys: bool, pub kitty_graphics: bool }   // serde default per field
pub const HELLO_REPLY_ID: u64 = 0;  // attach hello -> ONE JSON Reply line (ok, or err then close), then frames
// render.rs
pub struct Screen { pub cols: u16, pub rows: u16, pub cells: Vec<SnapCell> /* row-major */,
                    pub cursor: Option<CursorState> }
impl Screen { pub fn new(cols: u16, rows: u16) -> Screen; }
pub struct FrameInputs<'a> { pub client: (u16, u16) /* cols, rows */, pub viewport: (u16, u16) /* window cell at client top-left */,
                             pub area: Rect, pub panes: Vec<(PaneId, Rect, RowsSnapshot)>, pub borders: Vec<BorderSegment>,
                             pub active: PaneId, pub status: Option<StyledLine>, pub overlays: &'a [&'a dyn Overlay],
                             pub cursor: Option<CursorState> }
pub fn compose(inputs: &FrameInputs) -> Screen;
pub fn diff(prev: Option<&Screen>, next: &Screen, caps: &Caps) -> Vec<u8>;   // None = full redraw
pub trait Overlay { fn draw(&self, screen: &mut Screen); }
// status.rs
pub struct Style { pub fg: Color, pub bg: Color, pub attrs: CellFlags }   // Default: terminal colours
pub struct Span { pub text: String, pub style: Style }
pub struct StyledLine { pub spans: Vec<Span> }   // one row, laid out for its width
// input.rs
pub struct Key { pub code: KeyCode, pub mods: Mods }
pub enum KeyCode { Char(char), Enter, Tab, Backspace, Escape, Up, Down, Left, Right,
                   Home, End, PageUp, PageDown, Insert, Delete, F(u8) }   // T2.3 may add variants
pub struct Mods { pub ctrl: bool, pub alt: bool, pub shift: bool, pub super_: bool }
pub enum Routed { Command(Vec<String>), ToPane(Vec<u8>), Consumed }
pub trait InputHook { fn on_key(&mut self, key: &Key, client: ClientId) -> Option<Routed>; }
// model.rs (MRU, no behaviour until T2.5a/T2.5b)
Session { .., pub last_window: Option<WindowId> }
Model { .., pub client_last_session: BTreeMap<ClientId, SessionId> }
// server/mod.rs (T2.5a): register_overlay, register_input_hook; M3 tasks add one registration line each.
```

M2 commands (parse and stub dispatch arms land in T2.0; T2.5a/T2.5b replace
the "not implemented yet" arms): `AttachSession { target, detach_others /* -d */,
nest /* --nest */ }` (alias `attach`), `DetachClient { session: Option<Target> /* -s */,
client: Option<String> /* -t, raw */, all_others /* -a */ }`, `ListClients { format }`,
`NextWindow`/`PreviousWindow`/`LastWindow(WindowStep { target })`,
`RenameSession`/`RenameWindow(Rename { target, name })`, `SwitchClient { target:
Option<Target>, last /* -l */, next /* -n */, previous /* -p */, key_table /* -T */ }`,
`SendPrefix { target }`; `capture-pane -e` sets `CaptureOpts.escapes`.

Options use tmux's names (M3's TOML `status.left` maps onto `status-left`):
`status` (choice off/on/2-5), `status-left`, `status-right`, `status-left-length`,
`status-right-length`, `status-style`, `status-interval`, `window-status-format`,
`window-status-current-format`, `prefix`, `escape-time` (global), `repeat-time`,
`automatic-rename`, `automatic-rename-format`, `allow-rename`.

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
| **T2.1** Attach client | `client.rs`: capability probe (spec §4.4), request richest key mode, raw mode, attach hello, stdin to `input` frames, SIGWINCH to `resize`, write `output` frames, restore the tty on every exit path including panic and SIGTERM (a panic in any thread restores the tty and exits); refuse only self-nesting attach (the target session contains the calling pane) without `--nest` (`-f` is reserved for tmux's client flags, M3.6), and allow attach to another session from inside a pane (spec §4.6; the popup case lands with TC.6); `attach` arm in `main.rs` | `src/client.rs`, `attach` arm in `src/main.rs`, `tests/fixtures/probe/` | pty tests against a fake server: frames flow both ways; tty restored after SIGTERM; resize frame on SIGWINCH; probe times out cleanly on a terminal that answers nothing. Unit: recorded Ghostty and WezTerm replies (DA1, XTVERSION, DECRQM 2026, kitty keyboard query, kitty graphics query) parse to the expected caps, including WezTerm without `enable_kitty_keyboard` (kitty keys off, everything else on) |
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

**Two kinds of task.** Compatibility is a translation front end (spec D1,
§4.5), so M3.6 splits its work in two:

- **Native tasks (TC.*)** build the grofe features the owner's setup needs, in
  grofe's own grammar: new verbs, formats, option lists, status sources, event
  hooks, popups, menus, the conditional, the mouse model. The native parser
  grows only for these features, with each verb shaped like tmux's where grofe
  has the same concept and no better form. Native acceptance runs through
  grofe's own CLI.
- **Front-end tasks (FE.*)** own `src/compat/tmux/`: the mapping table, its
  rows' cosmetic or semantic marks, tmux's output formats, error texts and
  timing checks, and the goldens from real tmux 3.7b. FE.1 lands the schema
  and the dispatcher that the area files register rows into (the M2 hook
  registration pattern); FE.2, FE.3 and FE.4 then run in parallel, each owning
  one area file and its golden tests. Each area's rows land after the native
  features they map to.

**Before Wave A starts:**

- **Stub edit** by the orchestrator on `main` (rules 3 and 4): the new files in
  the crate layout, plus `regex`; a `Command` variant with an empty flag struct
  for every new native verb (TC.2 fills the fields); routing in
  `server/dispatch/mod.rs` for each new verb to a stub handler returning
  `not implemented: <verb>`, placed in the file of the task that owns it:
  `dispatch/hooks.rs` (TC.5), `dispatch/structure.rs` (TC.8),
  `dispatch/client.rs` (TC.9), `dispatch/menu.rs` (TC.7), `dispatch/popup.rs`
  (TC.6), `dispatch/bindings.rs` (TC.10); and `src/compat/tmux/` with
  `mod.rs` holding a dispatcher stub that rejects every tmux form, and empty
  `formats.rs`, `bindings.rs` and `structure.rs` (FE.1 fills them with the
  row type's empty row-set functions); `compat::identity()` in
  `src/compat/mod.rs` returning grofe until TC.1 lands. After the
  stub edit no M3.6 task edits `dispatch/mod.rs`. New flags on verbs that
  already exist are handled in those verbs' existing handler files, by the
  task named in their row.
- T3.6's `TERM` re-run has passed, or its gaps are listed as blockers for the
  exit gate (spec D7).
- TC.0 and TC.0b are merged.

**Follow-ups to approved M1 contracts.** The front end is additive by
construction: it translates tmux forms into the `Command`s the native parser
already produces, so no tmux-only surface enters an approved M1 contract. The
remaining extensions are ordinary additions. No approved M1 branch is
reopened and nothing is reworked mid-M1; M1 closes on its own gate, and each
extension is an M3.6 task that lands on merged `main` after M3. Each such
task's acceptance includes "every M1 test unchanged and green". Checked
against T1.2's approved head (`feb8fc6`): no T1.2 test asserts that `x:` or
`=x` is rejected, and the native parser keeps its approved reading of both
(`x:` is the session's current window; `=x` is the literal name `=x`, since
`=name` is a front-end form). T1.2's error texts stay as approved; the front
end renders tmux 3.7b's texts for tmux-dialect requests.

| Approved M1 contract | Extension (additive) | M3.6 task |
|---|---|---|
| `Target` type (T1.2) and its `Resolve` impl (T1.5a) | A `SessionRef::Exact(String)` variant, built only by the front end (no native grammar change), and its resolution arm | FE.1 |
| `ParseError` and the flag tables (T1.2) | Native forms of the new verbs grofe itself needs (spec §4.5); no tmux-only form | TC.2 |
| Option store and `set`/`show` (T1.5c) | List-valued options with indexed set and append, which grofe needs for status rows and aliases | TC.2b |
| `FormatVars` for the model (T1.5c) | The inventory's variables, `version` per identity, `grofe_version` | TC.3 |
| Hellos and `ClientCtx` (T1.1a) | A `dialect` field (`grofe` default, `tmux`), serde-defaulted so old hellos still parse (spec §4.6) | TC.1 |
| Pane environment: `PaneSpawn.env` and the post-env block that sets `TERM`, `COLORTERM`, `GROFE`, `GROFE_PANE` (T1.4) | `TMUX`, `TMUX_PANE` in the same block under both identities; `TERM_PROGRAM=tmux` and `TERM_PROGRAM_VERSION` when the server has tmux identity; `GROFE` and `GROFE_PANE` unchanged | TC.1 |
| CLI: `src/main.rs` and its subcommand table (T1.1b) | An `argv[0]` check before the table that hands a `tmux` invocation's argv to the front end with `dialect: tmux`; `\;` chains stay T3.5's, and FE.1 only verifies them in the tmux dialect | TC.1 |
| Event emission sites (T1.6) | The internal events that grofe's event hooks need | TC.5 |

**Wave 0 (may start during M1 to M3; needs only real tmux).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TC.0** argv corpus | `scripts/tmux-argv-recorder.sh`: a `tmux` wrapper that appends its argv to a log and execs real tmux by an absolute path fixed at install time (never through `PATH`, so it cannot recurse); install notes: put it first on `PATH`, then restart the tmux server from a shell with that `PATH`, so `run-shell`, hooks and popups resolve `tmux` through the recorder too; `scripts/normalize-corpus.sh`: replaces paths, hostnames, session and window names and user option values with placeholders and dedupes; `docs/compat-coverage.md` maps each distinct verb and flag set to a spec §4.5 row or out-of-scope row, and notes `~/.claude/hooks/tmux-live-guard.sh` (its regex on `tmux ... kill-*` without `-L`/`-S` keeps matching under the alias, so it now guards grofe servers too). **[owner]** runs the recorder for at least three working days | `scripts/tmux-argv-recorder.sh`, `scripts/normalize-corpus.sh`, `tests/fixtures/compat/corpus.txt`, `docs/compat-coverage.md` | none | A check script exits nonzero if any corpus line maps to no §4.5 row; the corpus contains no string from a denylist built at check time (`$HOME`, hostname, git user email), keeping it publishable; the log shows at least one argv sent from a hook or `run-shell` (proof the restart took) |
| **TC.0b** Golden recorder | Spec §9's recording rules: `scripts/record-compat-goldens.sh` drives `tests/compat/record.rs` (an ignored test) against real tmux reached as `env -u TMUX <absolute path>`; it refuses to run unless that tmux's `-V` prints `tmux 3.7b`; `env -i` with fixed `SHELL=/bin/sh`, `TERM`, `LANG`; `TMUX_TMPDIR` and a private `-L` socket in a tempdir; `-f tests/fixtures/compat/tmux-defaults.conf`, which sets grofe's spec §5.3 values (`base-index 1`, `pane-base-index 1`, `renumber-windows on`, `history-limit 50000`, `escape-time 25`, `mouse on`) so ids, indices and sizes match; a pty-attached client from T2.4's harness for scenarios that need one (popups, status line); one normalization list (`normalize.txt`: socket paths, pids, timestamps, tty names) shared by recorder and suite; no strftime in any fixture; records `tmux list-commands` as the alias-table golden. Every FE task records its goldens with this tool into `tests/fixtures/compat/golden/<task>/` | `scripts/record-compat-goldens.sh`, `tests/compat/{main,record,normalize}.rs`, `tests/fixtures/compat/{normalize.txt,tmux-defaults.conf}`, `tests/fixtures/compat/golden/list-commands.txt` | none | Recording twice in a row gives byte-identical goldens; with a different tmux version on the path the recorder exits nonzero naming it; a recorded pane's `env` holds only the fixed variables plus tmux's own; the sockets under the user's real tmux and grofe directories are identical before and after a recording |

**Wave A (parallel after the stub edit; merge order TC.2, TC.3, TC.1, TC.12).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TC.1** Identity and entry | `src/compat/mod.rs`: `argv[0]` basename detection, the compat level constant (`3.7b`), the `TMUX` value builder (`<socket path>,<server pid>,<session id number>`), identity fixed at server start from the starting CLI's `argv[0]` with the `compat-identity` server option (`auto`, `tmux`, `grofe`; one schema key in `src/config/`, additive), the dialect decision (spec D12: only from the hello's `dialect` field and from `:` prompt text on a server with tmux identity; control clients are grofe dialect), and the `compat-miss.log` writer; `main.rs`: the `argv[0]` hand-off; the hello's `dialect` field; pane env per spec D12 and §4.2; the self-nesting rule in `client.rs` and the attach path (spec §4.6); README's "put `tmux` on PATH" step and fallback note | `src/compat/mod.rs`, the `argv[0]` hand-off in `src/main.rs`, `dialect` in `src/protocol.rs`, the env block in `src/server/pane/mod.rs`, the nesting check in `src/client.rs`, `compat-identity` in `src/config/`, README's install section, `tests/compat_identity.rs` | stub edit | In a pane, `$TMUX` equals `#{socket_path},#{pid},` plus the session id number, and `$TMUX_PANE` equals `$GROFE_PANE`; a server started through a `tmux` symlink gives panes `TERM_PROGRAM=tmux`, and a later `grofe` CLI call does not change that; a server started as `grofe` gives the outer value; `compat-identity tmux` in config overrides a `grofe` start; a `tmux`-invoked CLI sends `dialect: tmux`, a `grofe` one `grofe`, and a hello without the field parses as `grofe`; attach from a pane to another session succeeds, to its own session is refused; the log writer appends one line per call; every M1 test unchanged and green |
| **TC.2** Native verbs | Native parsing, into the stubbed `Command` variants, of the new verbs grofe needs (spec §4.5's rows, native forms): `join-pane`, `swap-pane`, `swap-window`, `break-pane`, `resize-window`, `last-pane`, `rotate-window`, `move-window`, `refresh-client`, `show-messages`, `choose-client`, `suspend-client`, `find-window`, `choose-tree`, `display-popup`, `display-menu`, `display-panes`, `confirm-before`, the conditional, event hooks, `set-environment`/`show-environment`, and new flags on existing verbs; no new tmux-only form: M1's short names (T1.2's `canonical_name` table) and `-g` scope merges stay as approved, and tmux's full alias table, prefix matching and `=name` exact targets belong to FE.1 | `src/cmd/{parse,mod}.rs`, `tests/native_parse.rs` | stub edit | Each new verb parses from its native argv with every flag its row lists; tmux-only forms M1 lacks are unknown commands natively (`ls`, `setw`, the unique prefix `a`), and `-t =` and `-t =x` resolve as literal session names (`can't find session: =x`); M1's short names (`new`, `neww`, `splitw`, `lsp`, `send`, `display`) and `set -gw`/`show -gw` keep working; T1.2's tests unchanged and green |
| **TC.3** Formats | `cmd/format.rs`: the modifiers and bare-name conditions from spec §4.5's formats row that grofe's format language adopts; `#(cmd)`, `#S`-style short forms and `##` stay out of the native language (FE.3 rewrites them); a job reference `#{@src-<hash>}` works like any user option; the model's `FormatVars` gains the inventory's variables, `version` per identity and `grofe_version` | `src/cmd/format.rs`, the `FormatVars` impl in `src/server/dispatch/options.rs`, `tests/formats_native.rs` | stub edit | Table tests for each modifier and variable against a fixed scripted model; `#{version}` returns what `compat::identity()` reports (stubbed as grofe until TC.1 lands; the tmux-identity check is FE.3's) and `#{grofe_version}` grofe's version; `#(date)` in a native format is literal text; T1.2's and T2.6's format tests unchanged and green |
| **TC.12** `DCS tmux;` unwrap | Spec §4.9: unwrap `DCS tmux; ... ST` (with doubled ESC) in T3.3's scanner; a wrapped APC `_G` goes to T3.8's graphics path, a wrapped sequence in the §4.9 set is re-emitted, anything else dropped | the unwrap in `src/server/passthrough.rs` (T3.3 merged), `tests/acceptance/tc_12.rs` | stub edit | A kitty graphics transmit wrapped for tmux produces the same outer stream as the unwrapped one; a wrapped sequence split across two reads still unwraps; an unknown wrapped sequence reaches neither the emulator nor the client |

**Wave B, native (after Wave A; a task starts when its Depends are merged;
merge order TC.2b, TC.4, TC.5, TC.8, TC.11, TC.7, TC.9, TC.10, TC.6, so TC.11
lands before TC.9 and TC.10).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TC.2b** Option lists | List-valued options with indexed set, append and unset (native forms), used by status rows and aliases; `main-pane-width` read by `main-vertical`; the no-effect marker `show-config --explain` prints | additions to `src/server/dispatch/options.rs` (after TC.3's, same file), `tests/options_lists.rs` | TC.2, TC.3 | Setting index 1 of `status.rows` leaves index 0 at its default; append adds to a list; unset removes one index; a no-effect option shows "no effect" in `--explain`; an unknown option still errors as in M1 |
| **TC.4** Status sources | Spec D8, §4.10: registry keyed by name or inline command; scheduled at the status interval or the source's `interval`; one run in flight; timeout kills the process group; spawn under the pane spawn lock (T1.4's deferred `pub(crate) spawn_lock()`); first output line stored in `@<name>` or `@src-<hash>`; inline sources registered by the front end through one function; a changed value triggers a repaint; `[status.sources.<name>]` in the schema (T3.1 merged, additive) | `src/server/sources.rs`, one registration line in `src/server/mod.rs`, the `status.sources` schema in `src/config/`, `tests/sources.rs` | TC.3 | With a 1 s interval and frames forced at 60 Hz on two attached clients, a source that appends to a counter file runs 10 or 11 times in 10 s; a source that sleeps 30 s never delays a frame (every frame interval under 50 ms) and is killed at its timeout; the segment is empty until the first result, then shows the first line; two registrations of the same command share one run; `show -v @batt` returns a named source's last value |
| **TC.5** Event hooks and environment | grofe's event hooks: a command bound to a grofe event runs through the dispatcher with the event's target as context (T1.6's emission sites, additive), in config (`[hooks]`) and at runtime (native form); `run-shell -b` (the flag in T3.4's `run-shell` handler); `set-environment`/`show-environment` with global, session, unset and hidden, reaching new panes, `run-shell`, hooks, popups and sources (spec §4.2) | `src/server/hooks.rs`, `src/server/dispatch/hooks.rs`, one registration line in `src/server/mod.rs`, `-b` in the `run-shell` handler, `tests/hooks.rs` | TC.2 | Each grofe event a tmux hook name needs (FE.3's rows) runs its command exactly once per triggering action, with the event's `#{pane_id}` and `#{session_name}`; removing a hook stops it; a hook running `run-shell -b 'sleep 5'` does not delay the next command; a global environment variable appears in a new pane and a `run-shell`; unset removes it; a hidden variable reaches `run-shell` and not a new pane |
| **TC.8** Structure verbs | The native verbs `join-pane`, `swap-pane`, `swap-window`, `break-pane`, `resize-window`, `last-pane`, `rotate-window`, `move-window` (in `dispatch/structure.rs`); new flags on existing verbs: `resize-pane -x -y`, `new-window -n <name> [cmd]`, `respawn-pane -k <cmd>`, per-window `automatic-rename`, `select-layout -E`, next and previous window with an alert; a `LayoutTree` geometry export and import (cells with sizes and offsets) that FE.2's tmux layout-string codec builds on | `src/server/dispatch/structure.rs`, additions to `src/server/layout.rs` (T1.3, T3.7 merged) and `src/server/dispatch/{pane,window}.rs`, `tests/structure.rs` | TC.2, TC.3 | Each verb's effect checked through `list-panes -F` geometry; export then import of a layout reproduces it exactly; `main-vertical` with `main-pane-width 50%` gives a half-width main pane; T1.3's tiling proptest holds after each operation |
| **TC.11** Status breadth | Multiple status rows, top position, justify, lengths, window-status formats and separator, `#[range]` and `#[align]`, the `client_prefix` colour, a status-line message area for `display-message` without `-p` (used by TC.9), repaint within one frame on an option change (spec §4.10) | additions to `src/server/status.rs` (T2.6 merged), `tests/status_rows.rs` | TC.3, TC.2b | Two rows drawn at the top when configured; `#[range=user\|net]` records the range under each cell; `#[align=centre]` centres; the `client_prefix` colour shows only while the prefix is pending; setting `@theme_blue` repaints on the next frame; overlong parts truncate on the documented side |
| **TC.7** Menus, prompts, aliases | `display-menu` with title, position, item keys, separators and nested menus, at a caller-supplied position (TC.10 passes the pointer); `display-panes`; `confirm-before`; prompt default text, prompt string, one-key read and prompt type; `list-keys` for one key with its note (the flags in T3.5's `list-keys` handler); grofe aliases (config `[aliases]` and a runtime form), expanded before parsing (contract edit: TC.7 co-owns `cmd/parse.rs` for alias expansion, TC.2 merged) | `src/server/menu.rs`, `src/server/dispatch/menu.rs`, one registration line in `src/server/mod.rs`, additions to `src/server/prompt.rs` (T3.4 merged), the `list-keys` flags, alias expansion in `src/cmd/parse.rs`, `tests/acceptance/tc_7.rs` | TC.2, TC.2b | A menu item's key runs its command; a nested menu opens from an item and Escape closes both; `display-panes` then a digit selects that pane; `confirm-before` runs its command only on `y`; a rename prompt shows the old name prefilled; a one-key prompt reports the pressed key's binding; an alias defined in config becomes a command |
| **TC.9** Client and session verbs | In existing handlers: `new-session -A`, `has-session` exit statuses, `switch-client -c -t` and `-T`, `detach-client -s`, `display-message` without `-p` (in TC.11's message area, logged for `show-messages`); new verbs in `dispatch/client.rs`: `refresh-client -S`, `show-messages`, `choose-client -Z`, `suspend-client`, `find-window -Z`, `choose-tree -Z -w` | additions to `src/server/dispatch/session.rs` and `src/server/clients.rs`, `src/server/dispatch/client.rs`, `src/server/tree.rs`, `tests/acceptance/tc_9.rs` | TC.2, TC.3, TC.11 | `new-session -A -s x` twice leaves one session and the second call attaches; `switch-client -c <client> -t s` moves only that client; `detach-client -s s` detaches every client on `s`; `switch-client -T t` then a key runs table `t`'s binding and returns to root; `display-message hi` shows in the status line and then in `show-messages`; `choose-tree -Zw` zooms, lists windows under sessions, and Enter switches; `find-window -Z foo` lists only matching windows; `suspend-client` stops the attach client with SIGTSTP and `fg` resumes it |
| **TC.10** Conditional, bindings and mouse | grofe's conditional (format form and shell form; shell form off the actor, background form not waiting) in `dispatch/bindings.rs`; spec §4.5's built-in mouse model: bindable mouse keys acting on the pane or status range under the pointer, status range clicks setting `mouse_status_range`, border drag resize, a menu from a mouse key opening at the pointer; binding notes; custom tables | `src/server/dispatch/bindings.rs`, additions to `src/server/input.rs` and `src/server/mouse.rs` (T3.2 merged), `tests/acceptance/tc_10.rs` | TC.2, TC.3, TC.7, TC.11 | A root C-h binding whose format condition matches `nvim` sends C-h into nvim and moves focus from a shell; a shell-form condition that sleeps 2 s does not delay a second client; a `MouseDown1Status` binding conditioned on `#{m:*net*,#{mouse_status_range}}` fires only on the `net` range; dragging a border resizes both panes; `DoubleClick1Pane` fires; a `MouseDown3Pane` binding that opens a menu opens it at the pointer cell |
| **TC.6** Popups | `display-popup` with every flag in spec §4.5's overlays row, as an overlay plus input hook (T2.5a); the popup's command runs in its own pty with the environment tmux gives a popup; closes on exit when asked, otherwise on a key; nested attach from a popup (spec §4.6) | `src/server/popup.rs`, `src/server/dispatch/popup.rs`, one registration line in `src/server/mod.rs`, `tests/acceptance/tc_6.rs` | TC.1, TC.2, TC.5 | A popup at 80% by 60% with a rounded border and title runs in its `-d` directory; `-c <client>` opens on that client only; inside a popup, attaching another session shows it, and detaching returns to the popup's caller; a hidden environment variable reaches the popup as TC.5 defines |

**Front-end tasks (FE.1 first; then FE.2, FE.3 and FE.4 in parallel, each
starting when its Depends are merged; merge order FE.1, FE.2, FE.3, FE.4).**
FE.1 defines the row type, writes an empty row-set function in each of
`formats.rs`, `bindings.rs` and `structure.rs`, and registers the three in
`src/compat/tmux/mod.rs`, so no later FE task edits `mod.rs` and each area
task starts from a stub it then owns. Every row an FE task adds carries
its mapping kind, its cosmetic or semantic marks, and a golden recorded with
TC.0b into `tests/fixtures/compat/golden/<task>/`. Every FE task's acceptance
also includes: each of its rows' goldens matches; a cosmetic flag it marks is
ignored with one `compat-miss.log` line; a semantic flag or verb it lacks fails
with tmux's error text and one `compat-miss.log` line.

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **FE.1** Front-end core | The table's schema (tmux form, mapping kind, grofe target, per-flag cosmetic or semantic, milestone, golden id) and dispatcher (tmux argv to one or more native `Command`s, or a tmux error); tmux 3.7b's alias table (from TC.0b's `list-commands` golden) and prefix rule; bare `tmux` as `new-session`; global `-L -S -f -V -2 -u` and `-V`'s output; targets `=name`, `=name:window` (to `SessionRef::Exact`, with its resolution arm) and bare `=` refused; tmux's error texts; the one tmux option-name table, holding every option name in the corpus (moved from `import_tmux.rs`; contract edit: FE.1 co-owns `import_tmux.rs` for the move) with `set`/`show` flag merges; pass-through rows for every M1 to M3 verb whose tmux form is identical; stored command strings keep their dialect (spec §4.5): translated at store time when fully known, otherwise stored tagged and translated when run, with only `:` prompt input taking the server identity; `display -p` in the middle of a `\;` chain | `src/compat/tmux/mod.rs` (schema, dispatcher, area registration, option-name table), the empty row-set stubs in `src/compat/tmux/{formats,bindings,structure}.rs`, `SessionRef::Exact` in `src/cmd/target.rs` and its arm in `src/server/model.rs`, the table move in `src/import_tmux.rs`, `tests/compat_frontend.rs`, `tests/fixtures/compat/golden/fe_1/` | TC.1, TC.2, TC.0b | Through a `tmux` symlink: `tmux -V` prints `tmux 3.7b` and `grofe -V` grofe's version; every alias in the golden resolves, `tmux a` attaches, and an ambiguous prefix errors as the golden; `=x` never matches `xy`, `=x:2` is window 2 of exactly `x`, `-t =` errors naming the mouse target; `set -gw`, `show -gqv` and `set -p -t "$TMUX_PANE" @a 1 \; set -p -t "$TMUX_PANE" @b 2` behave as their goldens; `display -p` mid-chain returns what the golden returns; every argv in `corpus.txt` whose verb is from M1 to M3 translates; TI.1's goldens unchanged |
| **FE.2** Structure, client and listing | Rows for: the TC.8 and TC.9 verbs' tmux forms; tmux layout strings (checksum form) read by `select-layout` and written by `#{window_layout}`, through TC.8's geometry export and import; `new-session -A`, `switch-client -c/-T`, `detach-client -s`, `info` as `show-messages -JT`; `list-*` default output formats and `-f` filters | `src/compat/tmux/structure.rs`, `tests/compat_frontend_fe2.rs`, `tests/fixtures/compat/golden/fe_2/` | FE.1, TC.8, TC.9, TC.0b | After the same scripted splits at 200x50, `#{window_layout}` equals tmux 3.7b's golden byte for byte; `select-layout '<golden string>'` reproduces the golden geometry; join, swap, break, rotate, move and `select-layout -E` each match their golden `list-panes -a -F`; `list-sessions`, `list-windows -a` and `list-panes -a` with no `-F` equal their goldens; `has-session -t =x` exit statuses equal tmux's; `info` exits 0 with a server and 1 without |
| **FE.3** Formats, options, status, hooks | Rows for: tmux format strings rewritten into grofe formats (short forms `#S #I #W #h #P`, `##` and `##{`, `#(cmd)` registered as an inline source through TC.4 and replaced by `#{@src-<hash>}`); array and append semantics (`status-format[N]`, `command-alias[N]`, `-ga`/`-as`) onto TC.2b's lists, using FE.1's option-name table (no second table); `show-options` output format; the twelve tmux hook names onto grofe events (TC.5), with `set-hook -g/-gu` and `show-hooks`, and hook timing relative to the command; `set-environment`/`show-environment` flags | `src/compat/tmux/formats.rs`, `tests/compat_frontend_fe3.rs`, `tests/fixtures/compat/golden/fe_3/` | FE.1, TC.2b, TC.3, TC.4, TC.5, TC.11, TC.0b | Every inventory format string, expanded after rewriting against a fixed scripted model, equals tmux 3.7b's expansion (goldens, normalized); for the canopy fixture theme, the rendered status (pty-attached client on both sides) matches tmux's cell for cell with source values fixed; `show -g` output equals its golden; each hook name fires exactly once per triggering action, in the same order relative to its command's reply as the golden; `show-environment -g` equals its golden; `#{version}` is `3.7b` on a server with tmux identity; a hook set with `tmux set-hook` whose command uses tmux-only forms (`neww`, `setw`, `-t =x`) runs correctly when it fires |
| **FE.4** Bindings and overlays | Rows for: `if-shell` and `if -F` onto grofe's conditional (TC.10); `command-alias` onto grofe aliases (TC.7); `display-popup` flags (border style and title cosmetic where unsupported), `display-menu`, `display-panes`, `confirm-before`, `command-prompt -I -p -k -T`, `list-keys -1N`; `list-keys` output format; mouse argv forms (`send -M`, `resize-pane -M`, `copy-mode -M`, `display-menu -x M`) marked semantic | `src/compat/tmux/bindings.rs`, `tests/compat_frontend_fe4.rs`, `tests/fixtures/compat/golden/fe_4/` | FE.1, TC.6, TC.7, TC.10, TC.0b | `bind -n C-h if -F '#{m/r:^n?vim$,#{pane_current_command}}' 'send C-h' 'select-pane -L'` translates and behaves as in tmux; `fzf --tmux` from a pane opens in a popup and its selection reaches the caller (skipped when the installed fzf lacks `--tmux`); `env` inside a popup matches the golden; `display-popup -b double` on a grofe without that style is ignored with a log line; `list-keys -T prefix` output equals its golden; `send -M` fails with tmux's text and a log line; a `tmux bind` whose command uses `neww`, `setw` and `ls` works when pressed; a native-config binding that opens a `command-prompt` with a native template works on a server with tmux identity |

**Wave C: the owner's daily config (after Wave B; TP.0 first, then TP.1 and
TP.2 in parallel; TP.2 owns only a template file that TP.1's generator picks
up, and merges after TP.1).** Moved from M3.7 so alias day runs the owner's ported
config, not grofe's defaults. TP.1 and TP.2 own files in canopy and athome, run
there with their own bats tests, and each has a grofe-side check.

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TP.0** import-tmux learns M3.6 | Spec §5.5's "with M3.6" list: `#()` to status sources, `if-shell`, `set-hook`, `set-environment`, `command-alias`, custom tables, mouse bindings, `display-popup`/`display-menu` bindings, `status 2` rows; command strings in bindings and hooks translated through the front end's table, so the converter and the live front end share one mapping | `src/import_tmux.rs`, `tests/fixtures/import-tmux/` | all of Wave B, FE.2, FE.3, FE.4 | On the canopy-style fixture the report shrinks to plugin and TPM lines and spec §4.5 out-of-scope items; the output loads with `show-config --file`; goldens updated in the same change |
| **TP.1** canopy core and the owner's user config | canopy's conf.d `00-core`, `05-caps`, `10-keys` (from `keys.tsv`), `20-status`, `30-plugins`, and athome's `user.conf`, drafted with TP.0: canopy's generator emits `conf.d/NN-canopy-*.toml` (bindings with `name`, `desc`, `group`, `repeat`); two status rows at the top with pills and themes as options; canopy-status, pet, network and battery as named status sources; `user.conf` (including `B g G M N P T W K ?`) becomes athome's `user.toml`; `canopy validate` becomes `show-config --file` (spec §7.1) | canopy's grofe backend and its generator, which renders one `conf.d` fragment per template file it finds in its templates directory, plus every template except nav; athome `dot_config/grofe/` | TP.0, TC.0b | `grofe show-config --file` on the generated tree loads with no errors; the rendered status matches tmux's rendering of the same theme cell for cell (sources set to fixed strings); every binding in canopy's `keys.tsv` and the owner's `user.conf` has a grofe binding (a subset of TP.9's check, run here) |
| **TP.2** vim-tmux-navigator | root `C-h/j/k/l/C-\` generated into canopy's `conf.d/NN-canopy-nav.toml`, rendered by TP.1's generator from a nav template file, as grofe's native conditional (TC.10) on `#{pane_current_argv}`, since the output is native TOML (matching `nvim`, `vim`, `fzf` and the owner's list), replacing the plugin's `if-shell` plus `ps` per keypress; Neovim's side unchanged (it calls `tmux -S <$TMUX socket> select-pane`) | only the nav template file in the generator's templates directory (no edit to TP.1's files); `tests/acceptance/tp_2.rs` in grofe | TC.10, TC.1, FE.1 (the acceptance uses Neovim's own `tmux` call); merges after TP.1 | C-h in a shell moves focus left; in nvim it reaches nvim, also when nvim runs under a wrapper whose comm differs; nvim at its left edge moves focus through its own `tmux` call; no move while zoomed, as the plugin's default; zero processes spawned per keypress (counted) |

**Wave D.**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TC.13** Compat acceptance suite | Spec §9's compat bullet, using TC.0b's recorder, normalizations and defaults fixture on both sides; the suite runs every scenario with `tmux` on the temp `PATH` pointing at grofe and the ported config from Wave C loaded. Scenarios: canopy `bin/*` (the canopy-agent hook path, canopy-status), `tsess`, `cockpit`, `fzf --tmux` (through the acceptance harness), vim-tmux-navigator's `tmux` calls from a headless Neovim, a smug-style start (`has-session`, `new -d`, `new-window`, `split-window`, `select-layout main-vertical`, `send-keys`), and a replay of every corpus argv through the front end. canopy and athome are found through `GROFE_COMPAT_CANOPY` and `GROFE_COMPAT_ATHOME` (skipped loudly when unset); CI checks both out at pinned SHAs in a `compat` job. Where tmux is not deterministic, the scenario asserts structure instead, and the file says so per scenario | `tests/compat/scenarios/`, `tests/fixtures/compat/golden/tc_13/`, the `compat` job in `.github/workflows/` | Waves A to C, the FE tasks, TC.0b | Every scenario's normalized output is byte-identical to its golden; the corpus replay gives no `unknown` or `not implemented` error and leaves `compat-miss.log` empty; `command -v tmux` inside the suite resolves to the grofe symlink and `tmux -V` prints the compat level; editing one golden turns the suite red; the sockets under the user's real tmux and grofe directories are identical before and after a run |

**Critical path inside M3.6** (review cycles included): stub edit → TC.2 (3 d)
→ TC.2b (2 d) → TC.11 (3 d) → TC.10 (3 d) → FE.4 (3 d) → TP.0 (3 d) → TP.1
(4 d) → TC.13 (4 d), about 25 working days. FE.1 (4 d) runs from day 3 to 7,
after TC.2. FE.3 is sized at 5 d (format rewriting, hook timing, a cell-exact
status line and `show` output) and runs day 8 to 13, one day of slack before
TP.0; FE.2 (TC.9, day 11 to 14) runs inside FE.4's window. The TC.2 → TC.5
(3 d) → TC.6 (4 d) chain reaches FE.4 on day 10, one day of slack. So two
branches sit one day off the path: a slip of more than a day in either moves
it. The one-file lane this replaces took 28 days.

**One-lane sum** (task-days, review included): TC.0b 3, TC.1 3, TC.2 3, TC.3
3, TC.12 2, TC.2b 2, TC.4 3, TC.5 3, TC.8 3, TC.11 3, TC.7 3, TC.9 3, TC.10 3,
TC.6 4, FE.1 4, FE.2 3, FE.3 5, FE.4 3, TP.0 3, TP.1 4, TP.2 2, TC.13 4:
69 task-days. TC.0 and TC.0b run before the stub edit, during M1 to M3.

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
| **TP.6** opensessions | sidebar pane, twelve hooks with `run-shell -b` curl and `tmux -S #{socket_path}`, the `opensessions` key table, `@opensessions_width` | Its tmux-side config ported to TOML hooks and a custom table; its program runs unchanged against grofe | canopy/athome port files, a `tests/compat/scenarios/` opensessions scenario | M3.6, TC.0b | Against a recording endpoint, a scripted session fires the same hook calls in the same order as tmux's golden (recorded with TC.0b); the sidebar opens through `join-pane` at `@opensessions_width` and keeps it across `resize-window` |
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
