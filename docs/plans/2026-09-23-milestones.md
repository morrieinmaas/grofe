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
| **M3.7** | Run the owner's remaining TPM plugins unchanged through a small loader (spec D1, §7.1); one compat check per plugin. | TP.9a and the compat suite green; **[owner]** each daily binding used once; closes with M5 (TP.11 yank, TP.9b). | ~4 days, before M5 (not beside M4). |
| **M4** | Native persistence and pane identity (spec §6): snapshot, restore, resume, `reboot-check` | Kill the server, restore: every `uid` unchanged, every pane with `resume` runs it; the container reboot scenario (T4.6) green; **[owner]** the same across a real reboot on each OS | ~3 weeks, after the alias-day container test |
| **M5** | Copy mode, buffers, `copy-pipe*`, OSC 52 (popups and menus moved to M3.6) | Scope doc approved first (written at the start of M5); the 93 `copy-mode-vi` bindings match tmux 3.7b's goldens (TM.7); then the yank compat check (TP.11) and TP.9b's parity rows. **No-difference parity**, **[owner]** confirmed | ~5.5 weeks |
| **M6** | Events and plugin API, Lua | Spec §7's plugin part rewritten from the dogfood log (T6.0, during M5); event catalog and protocol v1 frozen with fixtures; supervised plugins; sandboxed Lua with format functions; one out-of-process and one Lua plugin in-tree | ~2.5 weeks |
| **M7** | agents plugin; canopy on grofe (spec §7, §7.1) | canopy's four adapters report through grofe; rollups and attention render with formats only; after a container reboot each fake agent is back in its own conversation (T7.8); **[owner]** a real reboot returns every Claude Code pane to its conversation | ~3.5 weeks (T7.1 and T7.5 during M6) |
| **Alias-day container test** | TA.1, the owner's setup (athome + canopy + grofe as `tmux`) end-to-end in a container, scoped to M3.6+M3.7+M5. | report PASS; **[owner]** reviews it. | ~3 days |
| **E2E** | Release gate: every suite from a clean clone, in containers; rollback drill | `scripts/release-gate.sh` green; **[owner]** alias day for good, then a week with no open `compat` or `copy` entry: **fully done** | ~1 week + the owner's week |
| **M8** | Compatibility extras (optional) | Decided after M7: tmux `-CC` subset | unscheduled |

**Critical path to dogfood start:** T0.3 → T1.1b → T1.5a → T1.5b → T2.5a → T2.5b →
T3.5 → T3.6. Each step includes a review cycle, and M0/M2 have owner gates.
Estimate: about **7 weeks to dogfood start** (5.5 weeks of work + 30% buffer), not
counting the dogfood week itself. M3.5 adds about 3 days after dogfood start and
runs alongside the dogfood week; it does not move dogfood start.

**Recommended order after M3.5:** M3.6, then M3.7, then M5 (which closes
M3.7), then the alias-day container test, then M4 and the rest.

- **M3.6 first.** Unchanged reasoning: the owner can only do real work in
  grofe once their scripts and tools behave the same under the `tmux`
  name.
- **M3.7 next, now that it is cheap.** Running the owner's plugins
  unchanged through a loader is most of a week, not two, so there is no
  reason to run it beside anything: it simply comes right after M3.6.
- **M5 third**, closing M3.7 (TP.11's yank check, TP.9b's copy-mode
  parity). This is alias-day's full scope: M3.6 + M3.7 + M5.
- **The container test fourth, before M4 starts.** The owner
  wants to see the whole stack (athome, canopy, grofe as `tmux`) work in
  a disposable container before any real machine's persistence or agent
  plugin exists to lose anything. M4 (persistence), M6 (events and
  plugins, without Lua) and M7 (agents) all wait on this gate.
- **Rejected alternative (superseded):** the original plan ran M4 beside
  M3.7, reasoning that persistence was the core wedge and M3.7's tasks
  were file-disjoint from M4's. The 2026-09-29 ruling overrides this: the
  owner would rather see the container proof first than get persistence
  six weeks earlier.
- **Then M6, M7 and the release gate, in that order,** as before. M7's
  agents plugin is M6's first real user, so M6 cannot follow it.

**Critical path to alias day:** the dogfood-start path above, then (with TC.0
and TC.0b already done during M1 to M3) the M3.6 stub edit → TC.2 → TC.2b →
TC.11 → TC.10 → FE.4 → TP.0 → TP.1 → TC.13, about 25 working days with
review cycles. FE.1 lands first; FE.2, FE.3 and FE.4 then run in parallel, one
file each under `src/compat/tmux/`.
**Estimate, honestly, assuming at least three parallel implementer lanes**
(native Wave B has five tasks that can start at once, beside the FE tasks):
alias day about **week 14.5** from the start (7 to dogfood start, the dogfood
week with M3.5, about 6.5 for M3.6: 5 weeks of path work plus 30%). The
estimate from alias day on follows below. With one lane, M3.6 alone is
about 69 task-days (the one-lane sum in the M3.6 section), roughly 18 weeks
with the buffer, and alias day moves to about week 26 (splitting the front end
saves nothing in one lane).

**Critical path from alias day to fully done:** TA.1 (about 3 days, gated on
the owner's review) → M4 (stub edit → T4.3 → T4.4 → T4.6, 11 days) → M6
without Lua (stub edit → T6.1 → T6.2/T6.3 → T6.6, about 8 days; T6.0 during
M4) → M7 (T7.2 → T7.4 → T7.7 → T7.8, 14 days; T7.1 and T7.5 during M6) →
TE.2b → TE.3 (per the m35 amendment), with M5's 21 days and M3.7's task-days
now landing *before* alias day instead of after it. The one-lane sums for M4,
M6 and M7 are otherwise unchanged by this amendment (M6's already-updated sum
is in §4 above); M3.7's one-lane sum (new, since the section never had one
before) is: TP.3 3, TP.4 1, TP.5 1, TP.6 2, TP.7 2, TP.8 1, TP.9 2, TP.10 1,
TP.9a 2 (before M5) + TP.11 1, TP.9b 1 (in M5) = **17 task-days**, well under
the original's "~1.5 weeks" estimate for the ported version, consistent with
"run unchanged" being cheaper than "port."
**Estimate, honestly, with the same three lanes:** M4 ends about week 17.5
(M3.7's parallel tasks end inside it, about week 16), M5 about week 23, M6
about week 25.5, M7 about week 29, the release gate about week 30, and
**fully done**, after the owner's alias day for good and a week of work, about
**week 31**. The milestone-level plan had M4 at 1 week and M5 at 2; the task
breakdown found the rest: M4's restore engine, resume binding and container
reboot harness, and M5's 93 parity scenarios and the mouse model's move into
copy mode. With one lane, M4 to the release gate is about 103 task-days (M4
20, M5 29 with TP.11 and TP.9b, M6 20, M7 26, E2E 8), roughly 27 weeks with the
buffer, so fully done moves from the one-lane alias day at
about week 26, plus M3.7's 1.5 weeks, to about week 55.

M0 to M7 and the release gate are planned to task level; M8 stays at milestone
level and is decided after M7. Rows from M4 on name files M1 built or that
their milestone's stub edit creates; when dogfooding reshapes one, it changes
through rule 9.

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
    snapshot.rs      # M4: snapshot schema and writer (T4.1)
    restore.rs       # M4: restore, stagger, respawn to a shell (T4.3, T4.4)
    resume.rs        # M4: pane-set-resume binding and clearing (T4.2)
    copy/            # M5: mod.rs (TM.1), select.rs (TM.2), search.rs (TM.3), pipe.rs (TM.5)
    buffers.rs       # M5: paste buffers (TM.4)
    plugins.rs       # M6: plugin supervision (T6.3)
    dispatch/        # M3.6 stub handlers, one file per owning task:
                     #   hooks.rs (TC.5), structure.rs (TC.8), client.rs (TC.9),
                     #   menu.rs (TC.7), popup.rs (TC.6), bindings.rs (TC.10);
                     # M4: persist.rs (T4.2, T4.5); M5: copy.rs (TM.1), buffers.rs (TM.4);
                     # M6: plugins.rs (T6.3)
  client.rs          # M2: attach client + capability probe
  compat/
    mod.rs           # M3.6: argv0, identity, dialect, TMUX value, compat-miss log (TC.1)
    tmux/
      mod.rs         # M3.6: table schema, dispatcher, area registration, aliases and
                     #   prefix, global flags, -V, error texts, option names (FE.1)
      formats.rs     # M3.6: format rewriting, #() to sources, option, status and hook rows (FE.3)
      bindings.rs    # M3.6: if-shell, command-alias, popup and menu flags, list-keys (FE.4)
      structure.rs   # M3.6: structure and client verbs, layout codec, list-* output (FE.2)
      copy.rs        # M5: copy-mode and buffer rows (TM.7)
  agents/            # M7: mod.rs, manifest.rs (T7.1), state.rs (T7.2), identity.rs (T7.3), adopt.rs (T7.4)
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
                     # compat corpus and tmux 3.7b goldens (TC.0, TC.13), canopy's
                     # fake-agent (M4), protocol v1 messages (T6.2), adapter manifests (T7.1)
  e2e/               # M4+: Containerfiles and scenarios run by scripts/e2e.sh (T4.6, T7.8, TE.*)
plugins/event-log/   # M6: in-tree sh plugin (T6.6)
examples/lua/        # M6: in-tree Lua plugin (T6.6)
scripts/             # ci-local.sh; e2e.sh (T4.6); check-copy-scope.sh (TM.0); release-gate.sh (TE.3);
                     # compat recorders (TC.0, TC.0b)
docs/                # copy-mode.md (TM.0), protocol-v1.md (T6.2)
```

T0.2's skeleton predates `graphics.rs` and `import_tmux.rs`. Adding their empty
files and `mod` lines is the orchestrator's stub edit on `main` before T3.8 and
TI.1 start (rule 4). The same holds for the M3.6 files above, stubbed before
M3.6 starts, together with any dependency M3.6 needs that T0.2 did not add
(`regex` for `#{m/r:}`), by the orchestrator on `main` (rule 3). From M4 on,
each milestone's section opens with its own stub edit, which creates that
milestone's files and adds its dependencies.

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

### M2 seams (T2.0; filled by T2.2, T2.3, T2.5a, T2.6), as amended by T3.0

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
pub struct ClientCtx { pub cwd: PathBuf, pub pane: Option<PaneId>, pub env: Vec<(String, String)>,
                       #[serde(skip)] pub client: Option<ClientId> }  // set by the server for attached clients
                                                                     // and their bindings' commands; None for CLI
pub enum Hello { Control(ClientCtx),
                 Attach { #[serde(flatten)] ctx: ClientCtx, cols: u16, rows: u16, term: String, caps: Caps,
                          #[serde(default)] target: Option<String> /* attach -t, raw; resolved server-side */,
                          #[serde(default)] detach_others: bool /* -d */, #[serde(default)] nest: bool /* --nest */ } }
// render.rs
pub struct Screen { pub cols: u16, pub rows: u16, pub cells: Vec<SnapCell>, pub cursor: Option<CursorState> }
impl Screen { pub fn new(cols: u16, rows: u16) -> Screen; }
pub struct FrameInputs<'a> { pub client: (u16, u16), pub viewport: (u16, u16), pub area: Rect,
                             pub panes: Vec<(PaneId, Rect, RowsSnapshot)>, pub borders: Vec<BorderSegment>,
                             pub border_style: Style, pub active_border_style: Style, pub active: PaneId,
                             pub status: Option<StyledLine>, pub cursor: Option<CursorState>,
                             pub graphics: Option<&'a graphics::Remap> /* T3.8 */ }
pub fn compose(inputs: &FrameInputs) -> Screen;
pub fn diff(prev: Option<&Screen>, next: &Screen, caps: &Caps) -> Vec<u8>;
pub fn fix_wide_pairs(screen: &mut Screen);        // run after overlays
pub struct OverlayCtx { pub client: ClientId, pub viewport: (u16, u16), pub status_row: Option<u16>,
                        pub panes: Vec<(PaneId, Rect)> /* client coordinates */ }
pub trait Overlay { fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen); }
// status.rs
pub struct Style { pub fg: Color, pub bg: Color, pub attrs: CellFlags,
                  pub underline_color: Option<Color> }   // Default: terminal colours, no attrs
pub struct Span { pub text: String, pub style: Style }
pub struct StyledLine { pub spans: Vec<Span> }   // one row, laid out for its width
// input.rs
pub struct Key { pub code: KeyCode, pub mods: Mods }
pub enum KeyCode { Char(char), Enter, Tab, Backspace, Escape, Up, Down, Left, Right,
                   Home, End, PageUp, PageDown, Insert, Delete, F(u8) }   // T2.3 may add variants
pub struct Mods { pub ctrl: bool, pub alt: bool, pub shift: bool, pub super_: bool }
pub enum Routed { Command(Vec<Vec<String>>), ToPane(Vec<u8>), Consumed }
pub struct Binding { pub cmds: Vec<Vec<String>>, pub repeat: bool /* T3.5 adds source, desc, name */ }
pub trait InputHook { fn on_input(&self, state: &mut State, client: ClientId, input: &Input, now: Instant) -> Option<Routed>; }
// cmd/parse.rs (T3.0)
pub fn split_command_string(s: &str) -> Result<Vec<Vec<String>>, ParseError>;
// model.rs (MRU, no behaviour until T2.5a/T2.5b)
Session { .., pub last_windows: Vec<WindowId> }   // tmux's lastw stack, most recent last
Model { .., pub client_last_session: BTreeMap<ClientId, SessionId> }
// server/mod.rs
State { .., pub clients: BTreeMap<ClientId, clients::ClientEntry> }   // registry; empty until T2.5a
// server/clients.rs
pub struct ClientEntry {}   // stub; T2.5a adds its fields
// server/mod.rs (T2.5a, T3.0): register_overlay, register_input_hook. T3.0 writes every M3
// registration line in register_builtins, in priority order (prompt, then mouse): hooks run in
// that order and the first Some wins; overlays draw in reverse order, highest priority on top.
// Module state lives in typed State fields (prompt::Prompts, mouse::MouseState,
// passthrough::Passthrough, graphics::Graphics); hooks and overlays are stateless.
// Outer-terminal mouse modes are written by the server inside ordinary Output frames
// (clients::queue_raw); the client's RESET_TAIL resets them. No new Frame kind.
```

M2 commands (parse and stub dispatch arms land in T2.0; T2.5a and T2.5b each
replace their own "not implemented yet" arm lines in `dispatch/mod.rs`): `AttachSession { target, detach_others /* -d */,
nest /* --nest */ }` (alias `attach`), `DetachClient { session: Option<Target> /* -s */,
client: Option<String> /* -t, raw */, all_others /* -a */ }`, `ListClients { format }`,
`NextWindow`/`PreviousWindow`/`LastWindow(WindowStep { target })`,
`RenameSession`/`RenameWindow(Rename { target, name })`, `SwitchClient { target:
Option<Target>, last /* -l */, next /* -n */, previous /* -p */, key_table /* -T */ }`,
`SendPrefix { target }`; `capture-pane -e` sets `CaptureOpts.escapes`.

Options use tmux's names (M3's TOML `status.left` maps onto `status-left`):
`status` (choice off/on/2-5; `remain-on-exit` is a choice off/on too, as tmux), `status-left`, `status-right`, `status-left-length`,
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
| **T2.1** Attach client | `client.rs`: capability probe (spec §4.4), request richest key mode, raw mode, attach hello, stdin to `input` frames, SIGWINCH to `resize`, write `output` frames, restore the tty on every exit path including panic and SIGTERM (a panic in any thread restores the tty and exits); refuse only self-nesting attach (the target session contains the calling pane) without `--nest` (`-f` is reserved for tmux's client flags, M3.6), and allow attach to another session from inside a pane (spec §4.6; the popup case lands with TC.6); the hello carries `attach -t` raw as `target` plus `detach_others` (`-d`) and `nest` (`--nest`); `attach` arm in `main.rs` | `src/client.rs`, `attach` arm in `src/main.rs`, `tests/fixtures/probe/` | pty tests against a fake server: frames flow both ways; tty restored after SIGTERM; resize frame on SIGWINCH; probe times out cleanly on a terminal that answers nothing. Unit: recorded Ghostty and WezTerm replies (DA1, XTVERSION, DECRQM 2026, kitty keyboard query, kitty graphics query) parse to the expected caps, including WezTerm without `enable_kitty_keyboard` (kitty keys off, everything else on) |
| **T2.2** Renderer | `server/render.rs` per the M2 seams and spec §4.4: compose, diff, colour downgrade by caps, synchronized output, absolute cursor moves around wide cells, letterbox and crop viewport; replace `tests/term.rs`'s smulx negative test with a positive underline-style snapshot | `src/server/render.rs`, `tests/snapshots/`, `tests/render.rs` | Round-trip snapshots: single pane, splits, active border, colours in truecolor and 256 modes, emoji + VS16 + combining marks, cursor; unchanged frame diffs to nothing; one-cell change emits a short sequence |
| **T2.3** Input and key tables | `server/input.rs`: decode client bytes (CSI, SS3, UTF-8, kitty keys, SGR mouse, paste as opaque blob), key tables with prefix and repeat, encode for a pane's `InputModes`, `InputHook` trait | `src/server/input.rs` | Table tests for decode and encode; prefix then `%` routes to the bound command; paste containing the prefix key is never matched; escape-time resolves lone ESC |
| **T2.6** Status line | `server/status.rs` per spec §4.10: render `status.left/right`, `window-status-format` and `window-status-current-format` to a `StyledLine` with styles, expanding `#{}` inside `#[...]` and strftime sequences; add `#{==:a,b}`, `#{b:var}`, nesting and escaping to `cmd/format.rs` (contract edit: T2.6 co-owns `format.rs` for this) | `src/server/status.rs`, `src/cmd/format.rs` | Unit: formats with conditionals and user options render; `#[fg=#{@theme_blue}]` takes the option's colour; `%H:%M` expands against a fixed clock; `#{b:/a/b/c}` gives `c`; the current window uses the current format; overlong content truncates on the left side as tmux does |
| **T2.4** Acceptance harness | `tests/acceptance/{main,harness}.rs`: spawn `grofe attach` in a pty via expectrl, feed keys, reconstruct its output through `alacritty_terminal`, assert on screen text and cursor; tool paths (nvim, fzf, less, tmux) are discovered, never hardcoded (spec §9); consolidate the pty+reconstruct harness duplicated in `tests/spike.rs` and `tests/term.rs` into `tests/acceptance/harness.rs`; fix `spike_bin`'s debug-profile assumption | `tests/acceptance/main.rs`, `tests/acceptance/harness.rs`, `tests/spike.rs`, `tests/term.rs` | Harness self-test passes against the spike binary |

**Wave B (T2.5b after T2.5a).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T2.5a** Clients and frames | `server/clients.rs`: client registry, per-client writer task with one pending frame and full-redraw recovery, render scheduler (dirty flags, ~16 ms), D6 size policy with debounce, detach, overlay and input-hook registration API; attach handling in `server/mod.rs`; M2 commands `attach`, `detach-client`, `list-clients` | `src/server/clients.rs` (fills the `ClientEntry` stub), attach paths in `src/server/mod.rs` (including `State.clients` use and setting `ClientCtx.client`), its own arm lines (`AttachSession`, `DetachClient`, `ListClients`) in `src/server/dispatch/mod.rs`, `tests/acceptance/t2_5a.rs` | Acceptance: attach shows a prompt; detach and reattach from a smaller terminal crops with the viewport following the cursor, and from a larger one letterboxes; a client that stops reading does not delay a second client |
| **T2.5b** Bindings and routing | Default tmux bindings (spec §5.3), input routing into dispatch, remaining M2 commands (`next/previous/last-window`, `rename-*`, `switch-client -l -p -n`, `send-prefix`, `capture-pane -e`); `automatic-rename` to `#{b:pane_current_path}`, with OSC titles never freezing the name (spec §4.5) | `src/server/dispatch/window.rs` additions (coordinated, T1.5a merged); additions in `src/server/dispatch/session.rs` (`rename-session`, `switch-client`) and `src/server/dispatch/pane.rs` (`send-prefix`); `PaneHandle::capture` in `src/server/pane/mod.rs` (`-e` via T2.2's SGR writer); MRU upkeep in `Model::remove_window`/`remove_session` in `src/server/model.rs` (killing the active window falls back to the session's `last_window`, as tmux, else by position; stale `last_window` and `client_last_session` entries are cleared); its own arm lines in `src/server/dispatch/mod.rs`; default key table in `src/server/input.rs` (T2.3 merged); `tests/acceptance/t2_5b.rs` | Acceptance: `prefix %` and `prefix "` split, arrows move, `prefix d` detaches; Neovim, fzf and less scenarios pass; a TUI that pushed kitty keyboard flags quits on a bare `q`; `cd /tmp` renames the window to `tmp`, and still does after the pane sets an OSC title. **[owner]** the bare-`q` check inside Claude Code; note that macOS option-as-alt misbehaves in some Ghostty builds (canopy 8070a22), so a failure there is triaged against the outer terminal first |

**Exit gate:** reviewer runs the acceptance suite on both OSes; **[owner]**
30-minute hands-on session split across Ghostty and WezTerm, listing what hurt
and in which terminal.

---

## M3: config and dogfood

**Wave 0: T3.0 seam wave** (one task, before Wave A): the stubs, seams and
contract edits listed in the M3 addendum, including every
`register_builtins` line. After it no Wave A or Wave B task edits
`server/mod.rs`.

**Wave A (parallel after T3.0; merge order T3.7, T3.1, T3.3, T3.2, T3.4; T3.8's placeholder survey may run here, doc only).**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T3.1** Config core | `config/*`: schema structs (options from spec §5.2 including `status.style`, `pane-border` and key tables with `name` and `repeat`), layering (spec §5.1, `user.toml` last), `requires` header (spec §5.2), winning-source provenance, unknown-key and type errors with file and line, duplicate key and duplicate `name` rejection, `show-config --explain`, `show-config --file` without a server resolving the machine's full chain (repeatable; the layer slot follows the extension: .toml replaces config.toml, .lua is reserved for layer 5, T6.6) (`show-config` arm in `main.rs`) | `src/config/*`, `show-config` arm in `src/main.rs`, `tests/config.rs` | Unit: layering order, with `user.toml` beating a conf.d fragment; provenance names the right file; a type error fails with file, line and key; unknown key suggests the closest valid one; a file with `requires = "grofe >= 99.0"` and an unknown key is skipped with a warning while the same key without the header is an error; a key bound twice and a `name` used twice are both rejected; `--file` works with no server running, includes the user layers, and finishes under a test timeout |
| **T3.2** Mouse and scrollback view | Click to focus; wheel scrolls a read-only scrollback overlay in panes without mouse mode; wheel passes through to panes that requested mouse | `src/server/mouse.rs`, `tests/acceptance/t3_2.rs` | Acceptance: wheel in a shell scrolls history; wheel in Neovim scrolls Neovim; click focuses a pane |
| **T3.3** Passthrough | Spec §4.9 set: OSC 52, title, bell with activity flag, OSC 8 from cell hyperlinks, OSC 9/777; OSC 7/9/777 byte scanner (`vte` 0.15 parses none of them; survives split reads; feeds `PaneEvent::Cwd`), which also frames APC `_G` sequences (`vte` drops APC) into `PaneEvent::Graphics` for T3.8; bracketed paste and focus forwarding verified end to end | `src/server/passthrough.rs`, `tests/acceptance/t3_3.rs` | Acceptance: Neovim `"+y` produces an OSC 52 sequence in the outer pty stream; a multi-line paste into Neovim arrives as one bracketed paste; `printf '\a'` sets the window activity flag. Unit: an APC `_G` sequence split across two reads yields one `PaneEvent::Graphics` and none of its bytes reach the emulator |
| **T3.4** Prompt and run-shell | `prefix :` prompt in the status line with line editing and history; `command-prompt`; `run-shell` for bindings | `src/server/prompt.rs`, `tests/acceptance/t3_4.rs` | Acceptance: `prefix :` then `split-window -h` splits; a binding to `run-shell 'touch /tmp/x'` creates the file |
| **T3.7** Layout presets | `select-layout main-vertical`, `even-horizontal`, `even-vertical` (spec §4.3) as `LayoutTree` operations; `select-layout` dispatch arm | `src/server/layout.rs` additions (T1.3 merged), `select-layout` arm in `src/server/dispatch/window.rs`, `tests/layout_presets.rs` | Unit: `main-vertical` over four panes gives one full-height left pane and three stacked on the right; `even-*` gives equal sizes within one cell; the T1.3 tiling proptest still holds after a preset. Integration: `select-layout main-vertical` then `list-panes -F` matches |

**Wave B.**

| Task | Scope | Owns | Accept |
|---|---|---|---|
| **T3.5** Wire config | Config drives options and key tables (`repeat` bindings, `name`s in `list-keys`); `reload` validates every layer, then applies all or nothing and reports all errors (spec §5.4); `bind-key`, `unbind-key`, `list-keys` (with source and description; runtime and plugin binds marked as outside the table), `source-file` (relative paths against the including file); `;` command sequences in binding strings and the CLI (contract edit: T3.5 co-owns `cmd/parse.rs` for this); README install and usage, including the supported terminals and the WezTerm `enable_kitty_keyboard = true` note (spec D11) | `src/server/dispatch/options.rs` additions, `src/server/input.rs` table loading, `src/cmd/parse.rs` (sequences), `README.md`, `tests/config_live.rs`, `.github/workflows/readme.yml` | Integration: editing a conf.d file then `reload` changes a binding; a `reload` with errors in two files reports both and changes nothing; `source-file sub.toml` from a conf.d file loads the file next to it whatever the server's cwd; `list-keys` shows each binding's source and marks a runtime `bind-key`; a `split-window ; select-pane -L` binding runs both; README steps pass in a clean Linux container job |
| **T3.8** Kitty graphics placeholders | Spec §4.9. First a T0-style check, recorded before any code: which of the owner's tools emit placeholder mode (Claude Code image output, `kitten icat --unicode-placeholder`, chafa and timg if relevant); **[owner]** reviews the result. Then `server/graphics.rs`: consume `PaneEvent::Graphics` from T3.3's scanner; per-pane image id remap; forward transmit, virtual-placement (`U=1`) and delete commands with `q=2` to clients whose caps report kitty graphics, queued ahead of the next frame; answer the pane's `a=q` queries from caps; keep the last data per image id under a memory cap and re-send it on attach and full redraw; the renderer rewrites placeholder cell colours to the remapped id and never downgrades them (contract edit: T3.8 co-owns `render.rs` for this); direct placement and sixel ignored | `src/server/graphics.rs`, `tests/fixtures/graphics/`, placeholder colour rewrite in `src/server/render.rs`, `tests/acceptance/t3_8.rs`, `docs/graphics-notes.md` | Acceptance (skipped when `kitten` is absent): `kitten icat --unicode-placeholder` in a pane produces the APC transmit in the outer pty stream and placeholder cells on the reconstructed screen; the placeholder cells survive a split and a scroll with the image id intact; a second client attaching afterwards receives the image data. Unit: two panes that both transmit id 1 are forwarded with distinct ids; the cache evicts oldest first at its cap |
| **T3.6** Dogfood readiness | Complete acceptance suite (spec §8 list, nested tmux and nested grofe included) green on both OSes; panic hook writes a report to the state dir; `docs/dogfood.md` log template with the copy-mode fallback (shift-drag) noted; re-run the T0.4 capability table against the real render path; flip D7 to tmux-256color if every extra capability now passes (a capability that still fails is logged as a gap to fix before alias day, spec D7) | `tests/acceptance/t3_6.rs`, panic hook in `src/main.rs`, `docs/dogfood.md` | CI green; the kitty-flags bare-`q` check from T2.5b passes with a config loaded; a forced panic writes a report; **[owner]** starts the dogfood week |

> **Options added by T3.0.** `dispatch/options.rs`'s `BUILTINS` belongs to
> T1.5c and, in M3, to T3.5's additions; T3.0 adds the rows M3 reads (`mouse`,
> `set-clipboard`, `focus-events`, `pane-border-style`,
> `pane-active-border-style`, `status-position`) so Wave A can use them. They
> use tmux's names and scopes, and spec §5.3's defaults where grofe differs:
> `mouse on` (tmux `off`), `set-clipboard on` (tmux `external`),
> `focus-events on` (tmux `off`). `status-position` accepts only `bottom` until
> TC.11. This is the one exception to "M2 rows use tmux's defaults".

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
stays installed as the fallback until the release gate's alias-day week ends
(E2E and release), run as
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
| **TP.1** canopy core and the owner's user config | canopy's conf.d `00-core`, `05-caps`, `10-keys` (from `keys.tsv`), `20-status`, `30-plugins`, and athome's `user.conf`, drafted with TP.0: canopy's generator emits `conf.d/NN-canopy-*.toml` (bindings with `name`, `desc`, `group`, `repeat`); two status rows at the top with pills and themes as options; canopy-status, pet, network and battery as named status sources; `user.conf` (including `B g G M N P T W K ?`) becomes athome's `user.toml`; `canopy validate` becomes `show-config --file` (spec §7.1) | canopy's grofe backend and its generator, which renders one `conf.d` fragment per template file it finds in its templates directory, plus every template except nav; athome `dot_config/grofe/` | TP.0, TC.0b | `grofe show-config --file` on the generated tree loads with no errors; the rendered status matches tmux's rendering of the same theme cell for cell (sources set to fixed strings); every binding in canopy's `keys.tsv` and the owner's `user.conf` has a grofe binding (a subset of TP.9a's check, run here) |
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

## M3.7: run the remaining plugins unchanged

Spec D1, §7.1, owner ruling 2026-09-29. The owner's seven still-TPM-managed
plugins (`tmux-sensible`, `tmux-yank`, `tmux-fzf`, `tmux-sessionx`,
`tmux-which-key`, `opensessions`, `tmux-fzf-url`, `tmux-claude-session-manager`;
eight names, `vim-tmux-navigator` excluded per §1 of `plan-amend-2026-09-29.md`)
run **unchanged**: no `tmux.conf`, no plugin script is rewritten or ported.
TP.3 replaces real TPM with a small loader modeled on canopy's own
resurrect/continuum pattern (vendored tree, absolute-path load, no clone,
dormant with one log line when bash or the tree is missing); every other
task is a **compat check**, one per plugin, that the plugin's own unmodified
script, run through the tmux compatibility front end, behaves exactly as it
does under real tmux 3.7b. A native Lua or Rust rewrite of any one plugin is
a later, one-at-a-time decision, made only when that plugin proves slow or
fragile under the loader, and is out of scope here. Starts after M3.6's exit
gate. Runs **before** M5, not beside M4: M4 no longer starts until the
alias-day container test (new section below) has run, so M3.7 and M5 are
sequential, both ahead of M4. TP.3 first; TP.4 through TP.10 run in parallel
after it; TP.9a closes the wave once every compat check is in. TP.11 and
TP.9b need M5's copy-mode commands and run there, closing M3.7 for good, the
same shape the original plan gave TP.5 and TP.9b, just renumbered. Tasks
that own files in canopy or athome run there with their own bats tests; each
also has a grofe-side check named in its row. Resurrect and continuum are
**not** in this table (§2 of `plan-amend-2026-09-29.md`): they keep loading
exactly as today, unaffected by TP.3, pending the owner's answer to Q-M37-1
(§7 of `plan-amend-2026-09-29.md`).

| Task | Plugin or piece | What it verifies | Owns | Depends | Accept |
|---|---|---|---|---|---|
| **TP.3** TPM-style plugin loader | replaces TPM, `@tpm_plugins`, `~/.config/tmux/plugins/` | Extends canopy's existing vendoring (`plugins/vendor.sh`, `VERSIONS`, `CHECKSUMS`) from resurrect/continuum to the eight plugins named above: one `vendor.sh <name>` run per plugin, its tree and pin committed; a generic loader (a `lib/plugins.sh` function, called once per name in a fixed list from `30-plugins.conf`, replacing its two hand-written blocks with a loop over all ten names including resurrect/continuum) that checks bash and the vendored entry point exist, then runs it with the same `if-shell`-guarded `run-shell "<absolute path>"` pattern already in use; no network at runtime; runs once at server start and again on `source-file`/`reload`, matching TPM's own timing | canopy: the 8 new `plugins/<name>/` trees, `plugins/VERSIONS`, `plugins/CHECKSUMS`, the generalized loader in `lib/plugins.sh` and its call site (replacing `30-plugins.conf`'s two blocks), `test/plugin_loader.bats`; grofe: none (the loader is plain shell calling verbs already native since M1–M3.6) | M3.6's exit gate (bind-key, if-shell, run-shell, set-hook, set-environment, display-menu all merged) | `vendor.sh <name>` for each of the 8 reproduces a tree whose digest matches a freshly recorded `CHECKSUMS` line; `canopy doctor` reports all ten vendored trees present and unmodified; starting grofe under the `tmux` name with the ported config loads with no error and no `compat-miss.log` line from the load itself; deleting one plugin's vendored tree makes only that plugin go dormant (one log line), every other plugin still loads; the same loader run against real tmux 3.7b on a private `-L` socket loads the same set (proving the loader itself is not tmux-specific) |
| **TP.4** compat check: tmux-sensible | `tmux-plugins/tmux-sensible` | Its `.tmux` script's `set-option -g`/`bind-if-unset` calls (escape-time, focus-events, aggressive-resize, display-time, history-limit, and the rest) land at the values it sets, or are left alone where the owner's ported config already set them first (load order matters: canopy's own config loads before the plugin layer) | `tests/acceptance/tp_4.rs` | TP.3 | Every option sensible's script sets matches a golden recorded from real tmux 3.7b loading sensible alone; no `compat-miss.log` line from sensible's own calls; `show-options -g` includes each one at the golden's value |
| **TP.5** compat check: tmux-which-key | `alexwforsythe/tmux-which-key` | `prefix Space` opens the same nested `display-menu` (TC.7, native since M3.6) built from `@wk_menu_*`, matching the owner's `keys.tsv` groups | `tests/acceptance/tp_5.rs` | TP.3, TC.7 | `prefix Space`'s menu matches TC.0b's golden for the owner's group layout; a submenu opens and Escape closes both; an item's key runs its bound command |
| **TP.6** compat check: tmux-sessionx | `omerxx/tmux-sessionx` | `prefix s` opens its fzf-tmux popup over `list-sessions`, zoxide mode lists directories, choosing a session switches to it | `tests/acceptance/tp_6.rs` | TP.3, TC.6, FE.1, FE.4 | `prefix s` opens the popup; selecting a session switches the client; the plugin's own no-fzf error path matches tmux's text |
| **TP.7** compat check: tmux-fzf | `sainnhe/tmux-fzf` | `prefix F` opens the plugin's own action menu (switch window, kill pane, and the rest of its default action set) over `display-popup` | `tests/acceptance/tp_7.rs` | TP.3, TC.6, TC.8, FE.2 | Choosing an action in the menu behaves as tmux's golden for that action (a switched window, a killed pane) |
| **TP.8** compat check: tmux-fzf-url | `wfxr/tmux-fzf-url` | `prefix u` captures the pane (`capture-pane -ep`, native since M2), extracts URLs, and opens the fzf-url picker; `@fzf-url-history-limit 2000` (already set in the ported config) is honoured | `tests/acceptance/tp_8.rs` | TP.3, TC.6 | `prefix u` over a pane with three URLs lists all three, matching the golden; choosing one runs its open command with that URL as argv |
| **TP.9** compat check: opensessions | `Ataraxy-Labs/opensessions` | Its twelve hooks (`run-shell -b` plus `tmux -S #{socket_path}`) and its sidebar (`join-pane` at `@opensessions_width`) fire in tmux's order against a recording endpoint | `tests/compat/scenarios/opensessions.rs`, reusing TC.0b's recording-endpoint pattern | TP.3, TC.5, TC.8 | A scripted session fires the same hook calls in the same order as a golden recorded with TC.0b; the sidebar opens at `@opensessions_width` and keeps it across `resize-window` |
| **TP.10** compat check: claude-session-manager | `craftzdog/tmux-claude-session-manager` | `prefix a`/`prefix A` (`@claude_launch_key`/`@claude_list_key`) open a nested `display-popup -E` running `tmux attach -t <agent session>` | `tests/acceptance/tp_10.rs` | TP.3, TC.6, TC.1 | From a pane in `main`, `prefix a` shows the named agent session in a popup; detaching closes the popup and leaves `main` attached; attaching to `main` from its own popup is refused, matching TC.1's nesting rule |
| **TP.9a** Parity, all tables but copy mode | every stock, canopy and plugin binding not in `copy-mode-vi` | `scripts/key-parity.sh` compares real tmux's `list-keys` (old config, private socket, `env -u TMUX`) with `grofe list-keys` per table: `prefix` (114) without `[`, `]`, `=` and `PageUp`; `root` (22) without the wheel, drag, double-click and triple-click rows; `opensessions` (13); stock mouse bindings matched against the built-in mouse model, stock `m` against its out-of-scope reason | `scripts/key-parity.sh`, the reasons column in `docs/compat-coverage.md` | TP.3 through TP.10 | The script exits 0 on every row not deferred to TP.9b; **[owner]** each daily binding (`s`, `K`, `?`, `u`, `g`, `G`, `P`, `T`, `W`, `a`, `A`) used once and ticked in `docs/dogfood.md` |
| **TP.11** compat check: tmux-yank *(runs in M5)* | `tmux-plugins/tmux-yank` | `y`, `Y` and `MouseDragEnd1Pane` in `copy-mode-vi`, bound by the plugin's own script to `copy-pipe-and-cancel` with the OS clipboard command it detects, plus OSC 52, the same criterion TP.5 carried in the original plan, now met by running the plugin unchanged once M5's copy-pipe targets exist, instead of hand-writing the bindings | `tests/acceptance/tp_11.rs` | TP.3, TM.5, TM.6 | A selection copied in copy mode reaches the plugin's own clipboard command and the outer pty as OSC 52; `prefix ]` pastes it with bracketed paste in a pane that asked for it |
| **TP.9b** Parity, copy mode *(runs in M5)* | `copy-mode-vi` (93), `prefix` `[`, `]`, `=`, `PageUp`, `root` wheel/drag/click | the TP.9a rows the copy-mode table left out | the TP.9b rows of `scripts/key-parity.sh` | TP.9a, TP.11, TM.1, TM.4, TM.6, TM.7 | The script exits 0 for every table, with no row left out |

**Exit gate:** reviewer runs TP.9a (and, once M5 closes, TP.9b) and the
compat suite; **[owner]** a week under the alias with every plugin loaded
unchanged, `compat` entries in `docs/dogfood.md` all closed or scheduled.
TP.11 and TP.9b close with M5, so M3.7 closes with M5, unchanged from the
original plan's shape.

---

## M5: copy mode and buffers

Spec §4.11 and §4.5's buffers row. Goal: copy mode, buffers and copy targets
behave as the owner's `copy-mode-vi` table does under tmux 3.7b, so the
shift-drag fallback (spec §10) retires. **Overlays:** `display-popup`,
`display-menu`, `display-panes` and `choose-tree` moved to M3.6 (TC.6, TC.7,
TC.9) and are done there; M5's only overlays are copy mode's own view and
`choose-buffer`, which reuses TC.9's chooser. M5 also closes M3.7: TP.11 (yank)
and TP.9b (the parity rows that need copy mode) run at its end. The wheel's read-only scrollback
view from T3.2 is replaced by copy mode, as the owner's root `WheelUpPane`
binding asks.

**Before Wave A starts:**

- TM.0's scope doc is **[owner]** approved (TM.0 itself runs at the very
  start of M5, right after M3.7's non-M5 tasks (TP.3 through TP.9a) close;
  M4 hasn't started yet under the new order, so 'during M4' no longer
  applies).
- **Stub edit** by the orchestrator on `main`: `src/server/copy/{mod,select,search,pipe}.rs`
  (one file per task, so TM.2, TM.3 and TM.5 never share one),
  `src/server/buffers.rs`, `src/server/dispatch/{copy,buffers}.rs` and `src/compat/tmux/copy.rs` (an
  empty row-set function, registered in `src/compat/tmux/mod.rs`, the one line
  there no task owns); `Command` variants `CopyMode`, `SetBuffer`,
  `ShowBuffer`, `ListBuffers`, `DeleteBuffer`, `PasteBuffer`, `ChooseBuffer`
  with empty flag structs, routed to stubs; the `-X`, `-N` and `-F` fields on
  `SendKeys` (TM.1 fills them); every new built-in option registered in the
  schema and option table with its tmux default (`mode-keys`,
  `word-separators`, `wrap-search`, `copy-command`, `buffer-limit`,
  `mode-style`, `copy-mode-match-style`, `copy-mode-current-match-style`);
  `set-clipboard` already exists from T3.0 with grofe's default `on` (spec
  §5.3); the stub does not re-register it; the `FormatVars` entries `copy_cursor_x`, `copy_cursor_y`,
  `copy_cursor_word`, `scroll_position`, `pane_in_mode`, `pane_mode` and
  `buffer_*`, reading accessors the owning tasks fill; and every M5 default
  binding in `src/server/input.rs`'s default tables, as data: the 93
  `copy-mode-vi` rows from TM.0's golden, `prefix` `[`, `PageUp`, `]` and
  `=`, and the `root` wheel, drag, double and triple click rows. After the
  stub edit no M5 task edits `input.rs`.

**Wave 0 (at the start of M5, after M3.7's non-M5 tasks close).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TM.0** Scope doc | `docs/copy-mode.md`, one page plus the keymap table. The keymap: tmux 3.7b's `list-keys -T copy-mode-vi`, recorded with TC.0b under `tmux-defaults.conf` plus `tests/fixtures/compat/copy-mode.conf` (`mode-keys vi` and the copy-mode bindings canopy and tmux-yank add, `y`, `Y` and `MouseDragEnd1Pane` to `copy-pipe-and-cancel`, reproduced from their sources, never from the owner's live server), into `tests/fixtures/compat/golden/tm_0/list-keys-copy-mode-vi.txt`; one row per binding: key, tmux command, the grofe copy-mode command, the task that builds it, the parity scenario id. Then, per spec §4.11: the selection model over wrapped lines, the search dialect and its differences from tmux's POSIX regex, the copy targets, the mouse, and what stays out (the emacs table, `copy-mode -M`) | `docs/copy-mode.md`, `tests/fixtures/compat/copy-mode.conf`, `tests/fixtures/compat/golden/tm_0/`, `scripts/check-copy-scope.sh` | TC.0b | The check script exits 0: every line of the recorded golden has exactly one row in the doc and every row one line, every row names an M5 task and a scenario id; the golden's count equals the inventory's 93, or the doc names each difference and its cause; **[owner]** approves the doc |

**Wave A (after the stub edit; parallel; merge order TM.4, TM.1).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TM.1** Copy-mode core | Spec §4.11: per-pane copy mode entered with `copy-mode` (`-u`, `-e`, `-t`), stock `prefix [` and `prefix PageUp`; a view over history and screen that keeps its offset while output continues; the `[offset/size]` indicator in `mode-style`; entering and leaving the stub's `copy-mode-vi` table with the mode; `send-keys -X <command>` with `-N <count>` and `-F`, erroring outside copy mode as tmux does; every motion, scroll and cancel command whose TM.0 row names TM.1; the accessors behind `pane_in_mode`, `pane_mode`, `copy_cursor_x`, `copy_cursor_y`, `scroll_position` and `copy_cursor_word`; drawn through T2.5a's overlay API | `src/server/copy/mod.rs`, `src/server/dispatch/copy.rs`, the `-X` route in the `send-keys` handler in `src/server/dispatch/pane.rs`, one registration line in `src/server/mod.rs`, `tests/fixtures/copy/`, `tests/acceptance/tm_1.rs`, `tests/fixtures/compat/golden/tm_1/` | stub edit, TM.0 | For each TM.1 row, over a fixed fixture printed into an 80x24 pane with 500 lines of history: the keys, then `display -p '#{copy_cursor_x},#{copy_cursor_y},#{scroll_position}'`, equal the golden recorded from tmux 3.7b with TC.0b; output arriving in copy mode does not move the view; `q` returns to the live pane; `send -X cursor-up` outside copy mode fails with tmux's text; `#{pane_in_mode}` is 1 only in the mode |
| **TM.4** Buffers | Named and automatic buffers (`buffer0`, newest first) with `buffer-limit` (50) evicting the oldest automatic one; `set-buffer [-a] [-b] [-n] <data>`, `show-buffer [-b]`, `list-buffers [-F]` with tmux's default format, `delete-buffer [-b]`, `paste-buffer [-p] [-b] [-d] [-r] [-s] [-t]`, with bracketed-paste markers only under `-p` and only for a pane that set mode 2004; `choose-buffer [-Z]` through TC.9's chooser in `tree.rs`, used by its public API only (a change there goes through rule 9) (Enter pastes, `d` deletes); the stub's `]` and `=` bindings start working; the accessors behind `buffer_name`, `buffer_size`, `buffer_sample`, `buffer_created` | `src/server/buffers.rs`, `src/server/dispatch/buffers.rs`, `tests/buffers.rs`, `tests/fixtures/compat/golden/tm_4/` | stub edit, TC.9 | `list-buffers` after three `set-buffer`s equals tmux 3.7b's golden; `paste-buffer -p` into `cat -v` with bracketed paste on shows `^[[200~` around the text, and without it shows none; `-d` deletes after pasting; the 51st automatic buffer evicts the oldest; `set-buffer -a` appends; `choose-buffer` then Enter pastes the chosen buffer |

**Wave B (TM.2 and TM.3 in parallel after TM.1; TM.5 after TM.2; TM.6 after
TM.5; merge order TM.2, TM.3, TM.5, TM.6).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TM.2** Selection | `begin-selection`, `select-line`, `rectangle-toggle`, `select-word` (with `word-separators`), `other-end`, `clear-selection`, `copy-selection` and its `-and-cancel` and `-no-clear` forms, `copy-end-of-line`, `append-selection` and its `-and-cancel` form; extraction per spec §4.11: soft-wrapped rows (alacritty's `WRAPLINE`) join into one logical line, hard newlines stay, trailing blanks are trimmed, wide cells whole, combining marks kept, rectangles per row; selections survive a reflowing resize | `src/server/copy/select.rs`, `tests/acceptance/tm_2.rs`, `tests/fixtures/compat/golden/tm_2/` | TM.1, TM.4 | `show-buffer` equals tmux 3.7b's golden byte for byte for: a selection over a line wrapped three times at 80 columns (one line); the same after resizing to 50 columns; a rectangle over rows mixing CJK, emoji and ASCII; `select-word` on `foo/bar-baz.rs` with the default separators; `append-selection` onto an existing buffer |
| **TM.3** Search and jumps | `search-forward`, `search-backward` (regex, spec §4.11) and their `-text` forms, `search-again`, `search-reverse`, `#`/`*` through `-FX` with `#{copy_cursor_word}`, `/` and `?` through `command-prompt -T search` (TC.7's prompt type), `wrap-search`, `copy-mode-match-style` and `copy-mode-current-match-style`, matches across soft wraps as tmux finds them; jumps `f F t T ; ,` through `command-prompt -1`; a search scans a copy of the grid off the actor | `src/server/copy/search.rs`, `tests/acceptance/tm_3.rs`, `tests/fixtures/compat/golden/tm_3/` | TM.1 | Cursor positions equal tmux 3.7b's goldens for a regex `fo+[0-9]`, a `-text` search for `a.b`, `n` past the last match with `wrap-search` on and off, a match split by a soft wrap, and each jump key; every regex difference TM.0 lists has a test showing grofe's documented behaviour; a search over a full 50 000-line history never delays a second client's frames by more than 50 ms |
| **TM.5** Copy targets | `copy-pipe`, `copy-pipe-and-cancel`, `copy-pipe-no-clear` with a command or the `copy-command` option; the command runs off the actor under the spawn lock with the server environment and the selection on stdin, and each also stores a buffer; `set-clipboard` `on`, `external` and `off`: a copy sends OSC 52 to attached clients under `on` and `external`, and a pane's own OSC 52 creates a buffer only under `on` (T3.3 already forwards it) | `src/server/copy/pipe.rs`, the OSC 52 send in `src/server/passthrough.rs` (T3.3 merged), `tests/acceptance/tm_5.rs` | TM.2 | `y` bound to `copy-pipe-and-cancel 'cat > $TMPDIR/clip'` writes the selection to the file and leaves copy mode; the outer pty stream carries OSC 52 with the selection's base64 under `on` and none under `off`; a `copy-pipe` whose command sleeps 10 s delays neither the next key nor a frame; an app's OSC 52 appears in `list-buffers` under `on` and not under `external`; with no argument the `copy-command` runs |
| **TM.6** Mouse in copy mode | The wheel in a pane without mouse mode enters copy mode with `-e` and scrolls (`send -X -N 5 scroll-up` as tmux's stock binding), leaving at the bottom; a drag enters copy mode and selects; `MouseDragEnd1Pane` runs its binding; double and triple click select a word and a line as tmux's stock root bindings do; a click in copy mode moves the cursor; panes in mouse mode still get the wheel; T3.2's scrollback overlay is removed | additions to `src/server/mouse.rs` (T3.2, TC.10 merged; the mouse bindings themselves are the stub's), `tests/acceptance/tm_6.rs` | TM.2, TM.5 | Through SGR mouse reports in the acceptance harness: the wheel in a shell shows the indicator and history text; the wheel in Neovim scrolls Neovim; a drag from (2,3) to (20,5) and release puts that text in the newest buffer and runs the `MouseDragEnd1Pane` binding (a stub copy command gets it); a double click's buffer equals tmux 3.7b's golden for the same fixture; T3.2's and TC.10's tests change only where the overlay became copy mode, and the diff lists each |

**Wave C.**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TM.7** Front-end rows and parity | Rows in `src/compat/tmux/copy.rs`, with FE.1's rules (mapping kind, cosmetic or semantic marks, goldens): `copy-mode -u -e`, `send -X`, `send -FX`, `send -X -N`, `copy-pipe*` strings inside `bind -T copy-mode-vi`, `command-prompt -T search`, the buffer verbs and their output formats, and the options `mode-keys`, `set-clipboard`, `copy-command`, `word-separators`, `wrap-search`, `buffer-limit`; the parity suite `tests/compat/scenarios/copy_mode/`: one scenario per TM.0 row (the same fixture, keys and a final `show-buffer` or cursor readout) recorded on tmux 3.7b with TC.0b and run on grofe through the `tmux` symlink | `src/compat/tmux/copy.rs`, `tests/compat/scenarios/copy_mode/`, `tests/fixtures/compat/golden/tm_7/` | TM.1 to TM.6, FE.1, TC.0b | All 93 scenarios are byte-identical to their goldens after normalization; `tmux list-keys -T copy-mode-vi` equals TM.0's golden; editing one golden turns the suite red; `compat-miss.log` is empty after the suite; the sockets check of "Rules for M4 onward" passes |

**Then (M3.7 tasks, run here):** TP.11 (yank) on top of TM.5 and TM.6, and
TP.9b on top of TM.1, TM.4, TM.6 and TM.7; tmux-fzf's buffer actions (TP.7)
once TM.4 lands. Their rows in M3.7 hold as written.

**Critical path inside M5** (review cycles included): TM.0 (2 d) and its
approval (about 1 d) right after M3.7's non-M5 tasks close; then stub edit →
TM.1 (4 d) → TM.2 (3 d) → TM.5 (2 d) → TM.6 (3 d) → TM.7 (5 d) → TP.11 (2 d) →
TP.9b (2 d), about 21 working days. TM.4 (3 d) runs beside TM.1 and TM.3
(3 d) beside TM.2 and TM.5, each in its own file. **One-lane sum:** TM.0 2, TM.1 4, TM.2 3, TM.3 3, TM.4 3, TM.5 2,
TM.6 3, TM.7 5, TP.11 2, TP.9b 2: 29 task-days.

**Exit gate (no-difference parity):** reviewer runs `scripts/ci-local.sh` and
TM.7's suite; TP.9b exits 0 for every table; **[owner]** a week under the
alias using copy mode instead of shift-drag, every `copy` entry in
`docs/dogfood.md` closed (substitute: TM.7's 93 scenarios and TP.9b green).
M3.7 closes here.

---

## Alias-day container test

Owner ruling 2026-09-29: after M3.6, M3.7 and M5 close (alias-day scope),
the owner's actual setup (athome, canopy and grofe as `tmux`) runs
end-to-end in a disposable container before anything moves further. M4, M6
(without Lua) and M7 start only once this test is green; the owner does not
switch a real machine before seeing its results. This is deliberately
**narrower** than the release gate's `TE.2b` (`.superpowers/sdd/m35/e2e-athome-amendment.md`),
which runs the same shape of test again, later, with M4's persistence and
M7's agents plugin added on top: no container restart here (M4 doesn't
exist yet) and no agents-plugin rollup step (M7 doesn't exist yet). TA.1
builds its own minimal container-running harness, since T4.6's shared
`scripts/e2e.sh` is an M4 deliverable that hasn't landed; **when M4's T4.6
lands, it generalizes TA.1's `Containerfile.alias-day` and `alias-day.sh`
into the shared `scripts/e2e.sh` rather than building parallel
infrastructure**, and `TE.2b` (§4 of the m35 amendment) builds on the
result instead of starting from nothing.

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TA.1** Alias-day container test | Stages 1–3 of `TE.2b`'s design (`.superpowers/sdd/m35/e2e-athome-amendment.md` §4), narrowed: **(1) inputs**, read-only, on the host: grofe at `HEAD`, athome at a named `ATHOME_ALIAS_REF` (a branch commit that pins grofe and canopy, puts the `tmux` symlink first on `PATH`, and installs grofe's ported config, the same shape as the later switch ref, but not required to be the final one), canopy at the tag containing M3.6/M3.7's port (`CANOPY_TAG`); the same secrets gate (no `.secrets/`, no `.env`, `gitleaks` clean, nothing from `~/.secrets`/`~/.config/chezmoi`/`~/.ssh`/rbw read or copied). **(2) apply**, in a systemd container, as a test user: `chezmoi init --apply --force` from the archive, the same script allowlist and skip-reason table `TE.2b` defines (persistence-only script 24 still runs; nothing here depends on it doing anything more than starting a server), the same stage-2 checks (`tmux` resolves to grofe, `tmux -V` prints the compat level, `grofe show-config` loads clean). **(3) workflow**, through a pty (`tests/e2e_alias_day.rs`, ignored by default, reusing T2.4's harness the same way `TE.2b` does): `TE.2b`'s steps W1 through W12 unchanged (splits, cockpit layout, session cycling, reload, nested tmux, tsess/keys-popup/theme-popup, the ported plugins at `s`/`Space`/`u`/`o`/`a`/`A`, vim-tmux-navigator, detach/attach), **dropping W13** (agents-plugin rollups, needs M7) and **W14** unchanged (`compat-miss.log` empty); two new steps replacing the excluded "copy mode and yank" note: W15 enters copy mode, selects text spanning a soft wrap, and reads it back with `show-buffer`, matching a TM.7 golden; W16 yanks a selection with `y` (TP.11's tmux-yank, unchanged) and checks the outer pty's OSC 52 stream and the stub clipboard command's output. Writes the same report shape `TE.2b` defines to `target/e2e/<run-id>/alias-day/report.md` | `tests/e2e/Containerfile.alias-day`, `tests/e2e/scenarios/alias-day.sh`, `tests/e2e/alias-day/` (seed config, skip list, report template, copied from `TE.2b`'s design, not re-derived), `tests/e2e_alias_day.rs`, `scripts/e2e-alias-day.sh` (a standalone runner; folded into `scripts/e2e.sh` by T4.6) | M3.7's exit gate, M5's exit gate (TP.11, TP.9b closed), athome's alias-day ref (owned outside grofe, prepared the same way §2 of the m35 amendment describes the switch ref, just not required to be final) | Every `TE.2b` pass criterion that doesn't name stage 4 or W13 holds (secrets gate passed, stage 2 checks held, W1–W12 and W14–W16 passed, each excluded step listed with its reason, no credential entered the container, the host-state check passed); **[owner]** reads the report before M4's stub edit is briefed |

**Exit gate:** the report shows PASS; **[owner]** reviews it and says go;
this is the ruling's "seen the container results" gate, and it is the thing
that unblocks M4's stub edit, not a fixed calendar date.

---

## Rules for M4 onward

These hold for every task from M4 to the release gate, on top of "How subagents
execute this".

- **Never touch the owner's live tmux or canopy.** Integration tests run
  through `TestServer` (temp `HOME` and XDG dirs, a unique `-S`); real tmux is
  reached only through TC.0b's recorder (a private `-L` socket in a tempdir,
  `env -u TMUX <absolute path>`); every end-to-end scenario runs in a container
  through `scripts/e2e.sh` (T4.6), which copies repos in with `git archive
  <sha>` (grofe at `HEAD`, canopy and athome at their pinned SHAs), never a
  working tree, and never mounts the host's home, sockets or runtime dir. Any suite that runs on the
  host checks, before and after, that the owner's tmux socket directory
  (`${TMUX_TMPDIR:-/tmp}/tmux-$UID`), grofe's socket and state directories,
  `~/.config/canopy` and canopy's store directory are byte-identical, and fails
  otherwise.
- **CI means `scripts/ci-local.sh`.** GitHub Actions is disabled; the script
  runs the workflow with `act` on podman (`linux`) and natively (`macos`). T4.6
  adds an `e2e` leg. "Green" in an exit gate means every leg the gate names.
- **Owner steps have substitutes.** Each **[owner]** step (a real reboot, a
  week of use) is paired with a container scenario an agent can run. The
  substitute is the task's gate; the owner step is the milestone's.

---

## M4: persistence and pane identity

Spec §6. Goal: the server's state survives a kill, a crash and a reboot, with
every pane's `uid` intact and every pane that has a `resume` running it.
Whether this replaces resurrect and continuum, or coexists with them running
as plugins, is open (§2 of `plan-amend-2026-09-29.md`; owner to decide no
later than this stub edit). M4 starts once the alias-day container test (new
section, above) is green. It builds on TC.8's layout geometry export
and import and on TC.5's internal events.

**Before Wave A starts:**

- **Stub edit** by the orchestrator on `main` (rules 3, 4 and 6): the files
  `src/server/{snapshot,restore,resume}.rs` and `src/server/dispatch/persist.rs`
  with their `mod` lines; `Command` variants `PaneSetResume` and `RebootCheck`
  with empty flag structs, routed in `dispatch/mod.rs` to stubs in
  `dispatch/persist.rs`; the snapshot schema below, as types with serde derives
  and no logic; `paths::snapshot_path(env, socket)` (the instance directory
  beside the log, spec §6); the config keys `restore-on-start` (default `true`)
  and `restore-stagger-ms` (default `1000`); canopy's fake-agent fixture,
  vendored as `tests/fixtures/fake-agent` with a header naming its canopy SHA;
  and the harness extension below (a contract edit to the frozen
  `tests/common/`). Also, so no two Wave A tasks share a file:
  - the internal events `resume-changed` and `restore-done` in TC.5's
    internal event list, raised by nobody yet;
  - model fields `Pane.resume_binding`, `Pane.expect` and `Pane.restored`,
    and the `FormatVars` entries `pane_resume`, `pane_resume_expected` and
    `pane_restored` in `src/server/dispatch/options.rs` reading them, so T4.2
    and T4.3 only set fields;
  - `paths::fnv1a64` made `pub(crate)`;
  - hooks at the call sites, each a no-op until its task fills it:
    `resume::bind_root(pane)` after every spawn and respawn in
    `src/server/dispatch/{session,window,pane}.rs` (T4.2), and
    `restore::on_pane_exited(state, pane) -> bool` in the actor's exit
    handling in `src/server/mod.rs`, which keeps the pane when it returns
    `true` (T4.4);
  - the `--no-persist` long flag on `set-option`, parsed only (T4.1 gives it
    meaning).
- M3.6's exit gate has passed (TC.8's geometry and TC.5's events are merged).

**Contract: the snapshot (stub edit; names binding).**

```rust
// server/snapshot.rs
pub const SNAPSHOT_VERSION: u32 = 1;
pub struct Snapshot   { pub version: u32, pub written_at: String /* RFC 3339 */, pub grofe_version: String,
                        pub options: Vec<OptionSnap> /* global, --persist only */, pub sessions: Vec<SessionSnap> }
pub struct SessionSnap { pub name: String, pub cwd: PathBuf, /* no environment: spec §6 */
                         pub active_window: u32 /* index */, pub options: Vec<OptionSnap>, pub windows: Vec<WindowSnap> }
pub struct WindowSnap  { pub index: u32, pub name: String, pub automatic_rename: bool, pub cols: u16, pub rows: u16,
                         pub layout: LayoutGeometry /* TC.8, cells keyed by pane uid */, pub active_pane: String /* uid */,
                         pub zoomed: bool, pub options: Vec<OptionSnap>, pub panes: Vec<PaneSnap> }
pub struct PaneSnap    { pub uid: String, pub cwd: PathBuf, pub argv: Option<Vec<String>>,
                         pub resume: Option<Vec<String>>, pub options: Vec<OptionSnap> /* --persist only */ }
pub struct OptionSnap  { pub name: String, pub value: String }
pub fn read(path: &Path) -> Result<Snapshot, SnapshotError>;                      // T4.1
pub fn write_atomic(path: &Path, snap: &Snapshot) -> io::Result<()>;               // T4.1
// server/restore.rs
pub fn restore(state: &mut State, snap: Snapshot) -> Result<RestoreReport, RestoreError>;  // T4.3, T4.4
```

**Contract: the harness extension (stub edit).**

```rust
let srv = TestServer::builder().restore_on_start(true).path_env("/usr/bin:/bin").start();
srv.snapshot_path();                          // this server's state.json, under its temp XDG_STATE_HOME
srv.edit_snapshot(|v: &mut serde_json::Value| { /* mutation proofs */ });
srv.kill_hard();                              // SIGKILL the server and its process group, as a crash would
srv.signal_all(Signal::SIGTERM);              // the server and every pane process, as a reboot or logout does
let srv = srv.restart();                      // same socket and state dir; restore as configured
```

**Wave A (parallel after the stub edit; merge order T4.1, T4.2, T4.3).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T4.1** Snapshot writer | Spec §6: build a `Snapshot` from the model with no client involved; a writer registered on the internal event stream (TC.5's events, cwd changes, T4.2's `resume-changed`, `option-changed` for `--persist` options): at most once a second, if anything arrived, build the snapshot and write it only when its bytes differ from the last write; `kill-server`, SIGTERM and SIGHUP write the pre-shutdown state once, synchronously, and then **freeze** the writer: no further write, whatever the panes do while they die (a reboot or logout signals every process in the cgroup, so panes exit and sessions close after the server's signal); the empty state is written only after a normal last-pane exit with no termination signal pending; `--persist` is sticky per option (`Options::set` in `src/server/dispatch/options.rs` keeps the mark on a `set` without the flag, and only `set --no-persist` or unset clears it), and a change of the mark alone counts as a change for the writer; `write_atomic`: temp file in the same directory, `fsync`, `rename`, `fsync` of the directory, file mode `0600`; `read`: an unknown `version` renames the file to `state.json.v<N>` before this server's first write, an unparsable file to `state.json.corrupt-<unix time>`, each with one log line | `src/server/snapshot.rs` (logic, beside the stub types), one registration line in `src/server/mod.rs`, the persist mark in `Options::set` in `src/server/dispatch/options.rs`, `tests/snapshot.rs` | stub edit | With no client attached: after `split-window` the new pane's `uid`, `cwd` and `argv` are in `state.json` within 2 s; after `cd /tmp` in a pane its `cwd` follows within 2 s of the next sample; with nothing changing, the file's mtime and inode stay the same for 5 s; `set -p --persist @agent_session x` is saved and `set -p @agent_state working` never is; a later `set -p @agent_session y` without the flag saves `y` (the mark is sticky), and `set -p --no-persist @agent_session y` drops it from the file; `kill-server` leaves the pre-kill sessions in the file; `srv.signal_all(SIGTERM)` (panes dying after the server's signal) leaves the pre-shutdown sessions in the file, and so does SIGHUP; the last pane exiting normally leaves `"sessions": []`; 200 seeded structural changes with the server SIGKILLed at a seeded random point leave a file that parses and equals one of the states written before; a `"version": 2` file becomes `state.json.v2` and a fresh version 1 file is written; a garbage file becomes `state.json.corrupt-*`; the file is at `srv.snapshot_path()` with mode `0600`; every M1 test unchanged and green |
| **T4.2** Resume | Spec §6: `pane-set-resume [-t pane] [-u] [--expect <reason>] [-- argv...]` in native form (contract edit: T4.2 co-owns `cmd/parse.rs` for this verb, as TC.2 did); `argv[0]` without a `/` is resolved per spec §6: against the caller's `PATH` (from `ClientCtx.env`) when the caller has pane context; otherwise (a plugin, a control client) against the target pane's foreground process's own `PATH` (procinfo reads its environment: `/proc/<pid>/environ`, `KERN_PROCARGS2` on macOS), then the pane's spawn-environment `PATH`; never the server's; an unresolvable name errors and leaves the old resume; the binding: the pane's foreground process group leader at set time (via `tcgetpgrp` on the master), recorded in the stub's `Pane.resume_binding` as pid plus start time (`/proc/<pid>/stat` on Linux, `libproc` on macOS) so a reused pid does not match; the resume is cleared when that process no longer exists, checked on the procinfo sampling timer and on pane exit, and a stopped process keeps it; a resume from a spawn command (`new-session`, `new-window`, `split-window` with a command) or from `respawn-pane <cmd>` binds to the pane's root process, through the stub's `resume::bind_root` hook; `--expect <reason>` records an expectation with no argv, bound the same way and never saved; the model fields behind `#{pane_resume}` and `#{pane_resume_expected}` (their `FormatVars` entries are the stub's); raising `resume-changed` | `src/server/resume.rs` (including `bind_root`), `pane-set-resume` in `src/server/dispatch/persist.rs`, the verb in `src/cmd/parse.rs`, start-time and environment sampling in `src/server/pane/procinfo.rs`, `tests/resume.rs` | stub edit | A server started under `env -i PATH=/usr/bin:/bin`, and a CLI run inside a pane whose `PATH` adds a temp dir holding `fake-agent`: `pane-set-resume -- fake-agent --resume a` stores the absolute path in `#{pane_resume}`; from outside any pane, the same call resolves through the target pane's `PATH` (its foreground process's, then its spawn environment's), and fails when neither has it; and `respawn-pane -k` then runs it (the transcript gains `resume a`); a `fake-agent --session-id a` started in a pane's shell and bound from outside with `pane-set-resume -t %N`, then stopped with C-c, leaves `#{pane_resume}` empty within one sample plus 1 s while the shell stays; `kill -STOP` on the agent keeps it; a resume set while the shell is in the foreground survives running and exiting `ls`; `--expect "no id"` leaves `#{pane_resume}` empty, shows in `#{pane_resume_expected}` and clears with its process; `-u` clears; `pane-set-resume` with no server exits nonzero and starts nothing; every M1 test unchanged and green |
| **T4.3** Restore core | Spec §6: `restore()` into an empty server only (otherwise `server not empty: restore needs an empty server`, exit 1, nothing changes); `restore-on-start` read from config, with `GROFE_RESTORE_ON_START=0` still forcing it off (the harness); a restoring server restores before it signals readiness, so the starting CLI's own command runs against the restored sessions; `start-server --restore` forces a restore whatever the config says and reports when there is no snapshot; sessions by name, windows by index with name, `automatic-rename` state, size, layout geometry through TC.8's import keyed by `uid`, active window and pane, zoom; `--persist` options at every scope; `uid`s preserved, runtime ids fresh; every pane gets a shell in its `cwd` here (resumes are T4.4), the home directory with one log line when `cwd` is gone; a restored session takes the restoring server's environment (the snapshot holds none), its `update-environment` variables start unset and the first attach fills them; `Pane.restored` set for panes from the snapshot (the stub's `#{pane_restored}` reads it); raising `restore-done` with counts | `src/server/restore.rs` (without resumes), the `--restore` flag in the `start-server` arm of `src/main.rs`, one call at server start in `src/server/mod.rs`, the first-attach environment fill in `src/server/clients.rs`, the `restore-*` keys' reading in `src/config/`, `tests/restore.rs` | stub edit | Build two sessions, three windows and six panes with splits, a zoomed window, renamed windows, persisted options and `cd`s; `kill-server`; `start-server --restore`: `list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_uid} #{pane_current_path} #{pane_width}x#{pane_height}'` equals the pre-kill output line for line, and so does `list-windows -a -F '#{window_name} #{window_zoomed_flag} #{window_active}'`; persisted `@agent_session` is back and `@agent_state` is empty; `--restore` into a server with a session errors and `list-sessions` is unchanged; with restore-on-start on, `new-session -A -s main` after a kill attaches the restored `main` and `list-sessions` shows one `main`; under the default harness (forced off) a present snapshot restores nothing; a missing `cwd` gives the home directory and one log line; with the server started from an environment without `SSH_AUTH_SOCK`, before any attach `show-environment -t main SSH_AUTH_SOCK` is unset, after an attach whose client has `SSH_AUTH_SOCK=/tmp/x` it is `/tmp/x` and a new pane in `main` sees it; mutation proof: `edit_snapshot` changing one pane's `uid` turns the uid comparison red (the test runs the mutation and asserts the failure) |

**Wave B (after Wave A; merge order T4.4, T4.5).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T4.4** Resumes at restore | Spec §6: a restored pane with a `resume` runs it directly (no shell); its pty is allocated at restore and its child spawned after a deterministic delay, `paths::fnv1a64(uid) mod restore-stagger-ms`, applied by a server timer and never written into the argv (contract edit: T4.4 adds `PaneSpawn.delay` to T1.4's contract); `0` means no delay at all, and panes without a resume are never delayed; input sent to a pane inside its delay waits in the pty and reaches the process when it starts; through the stub's `restore::on_pane_exited` hook, when a process started from a resume at restore exits, or cannot be started, the pane respawns a shell in its `cwd` with the same `uid` (its resume cleared with its process, T4.2), one log line, and the usual `pane-exited` event; panes that did not come from a resume follow `remain-on-exit` | the resume and stagger path in `src/server/restore.rs` (T4.3 merged), `PaneSpawn.delay` in `src/server/pane/mod.rs`, `tests/restore_resume.rs` | T4.2, T4.3 | **The M4 done-means:** three panes in two windows run `fake-agent --session-id a`, `b` and `c`, each bound with `pane-set-resume -- fake-agent --resume <id>`; `kill_hard()`, `restart()`: every transcript reads `launch <id>` then `resume <id>`, each pane is in the window it was in, and every `uid` is unchanged; the server was started under `env -i PATH=/usr/bin:/bin` and the resumes still run although `fake-agent` sits only on the setter's `PATH`; the delay function returns the checked-in values for a fixed list of uids, and two restores of the same snapshot spawn the resumes in the same order and at the same offsets within 50 ms (from the log's monotonic timestamps); with `restore-stagger-ms 0` every resume starts within 100 ms of the restore, and at 1000 a pane without a resume has its shell within 100 ms; killing a restored `fake-agent` leaves a shell in its `cwd` with the same `uid` within 1 s and the `list-panes -F` geometry unchanged; a resume whose binary was deleted after the snapshot gives a shell with the same `uid` and a log line naming the argv; `send-keys` to a pane inside its delay reaches its process; mutation proof: `edit_snapshot` changing one pane's resume to `fake-agent --resume zz` turns that pane's transcript check red |
| **T4.5** `reboot-check` | Spec §6: `grofe reboot-check [--json]`, a CLI arm that asks the server's `reboot-check` verb, which reads the snapshot **on disk** (never the writer's memory) and compares it per pane with the live model. A pane expects a resume when it has a live resume or expectation (T4.2). Verdicts: `will resume`, `will restart without its conversation` (with its reason: the expectation's text, or "the snapshot holds an older resume"), `not saved yet`, and, for every pane when restore is off, `will not be restored`; a pane that expects nothing is listed as `restores as a shell` and never changes the status. Exit 0 when every pane that expects a resume will resume, or no server runs; 1 when any will not; 2 when persistence is misconfigured: `restore-on-start` off in the resolved config and no autostart unit (a systemd user unit in `~/.config/systemd/user/` or a launchd agent in `~/Library/LaunchAgents/` whose command runs `start-server --restore`), or the state directory is not writable. One line per pane, `<session>:<window>.<pane> <uid> <verdict> [(<reason>)]`; `--json` gives an array | the `reboot-check` arm in `src/main.rs`, `reboot-check` in `src/server/dispatch/persist.rs`, `tests/reboot_check.rs` | T4.1, T4.2 | Every resumed pane saved: exit 0; a resume set less than a second ago: that pane `not saved yet` and exit 1, then exit 0 two seconds later; `edit_snapshot` changing one pane's saved resume while the server runs: that pane `will restart without its conversation` and exit 1 (proof it reads the disk); an expectation "no session id reported": the verdict carries that reason, exit 1; only plain shells: every pane `restores as a shell`, exit 0; `restore-on-start = false` in the temp config and no unit in the temp `HOME`: exit 2 and every pane `will not be restored`; the same with a temp systemd unit running `tmux start-server --restore`: the status follows the panes again; no server: exit 0 with a message; a read-only state directory: exit 2; `--json` parses and has one entry per pane |

**Wave C.**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T4.6** Container reboot | The e2e harness every later container task uses: `scripts/e2e.sh <scenario> [image...]` (`${CONTAINER:-podman}`, rootless; builds grofe inside the image from `git archive HEAD`; mounts nothing from the host but that archive; refuses to run a scenario phase outside a container, checked by `/run/.containerenv`); `tests/e2e/Containerfile.debian` (required) and `tests/e2e/Containerfile.arch` (optional: the owner's Linux, but its image is amd64 only, so on an arm64 host it runs emulated with `--platform linux/amd64`, only when `E2E_ARCH=1`, and never gates); the `e2e` leg in `scripts/ci-local.sh`. Scenario `tests/e2e/scenarios/reboot.sh`, in two phases around `podman restart`, which kills every process, empties `/run/user/<uid>` (a tmpfs `e2e.sh` mounts) and keeps the container's disk: phase 1, as a test user, starts grofe through a `tmux` symlink, opens two sessions with three `fake-agent` panes pinned at launch and bound with `pane-set-resume` from their own shells (as an adapter would), a pane whose agent was then closed, and a plain shell after `cd`, and waits for `reboot-check`; phase 1b sends SIGTERM to every process of the test user, as a reboot or logout does before the kill, and checks `state.json` still holds every session; phase 2 "boots" with `env -i HOME=<home> PATH=/usr/bin:/bin FAKE_AGENT_HOME=<dir> tmux start-server --restore`, as a login unit would (its environment set by the unit, not the snapshot), then checks | `scripts/e2e.sh`, `tests/e2e/Containerfile.{debian,arch}`, `tests/e2e/scenarios/reboot.sh`, the `e2e` leg in `scripts/ci-local.sh` | T4.4, T4.5 | On the Debian image (and on Arch when `E2E_ARCH=1`, reported but not gating): phase 1's `reboot-check` exits 0 and names every pane; after phase 1b's SIGTERM `state.json` still lists every session and pane; after the restart each transcript is `launch <id>` then `resume <id>` in the window that held it; every `uid` equals phase 1's; the closed-agent pane and the `cd` pane come back as shells in their directories; a mutation run (phase 1 edits one pane's resume in `state.json` before the restart) fails on that pane only; `scripts/e2e.sh` exits nonzero when a scenario fails and when the host-state check of "Rules for M4 onward" finds a difference |

**Critical path inside M4** (review cycles included): stub edit → T4.3 (4 d)
→ T4.4 (3 d) → T4.6 (4 d), about 11 working days. T4.1 (3 d) and T4.2 (3 d)
run beside T4.3; T4.5 (3 d) runs beside T4.4. **One-lane sum:** T4.1 3, T4.2
3, T4.3 4, T4.4 3, T4.5 3, T4.6 4: 20 task-days.

**Exit gate:** reviewer runs `scripts/ci-local.sh` (`macos`, `linux`, `e2e`)
from a clean clone; **[owner]** a real reboot on each OS returns every
session with its `uid`s and every pane with a resume runs it, logged in
`docs/dogfood.md` (substitute: T4.6 green on the Debian image; Arch optional). Only after this
gate, and the owner's answer to Q-M37-1 (`plan-amend-2026-09-29.md`), does
the owner's machine drop resurrect and continuum.

---

## M6: events and plugins

Spec D9 and §7. Goal: a frozen control protocol and event catalog, supervised
out-of-process plugins, and sandboxed Lua, shaped by what M3.6 and M3.7 turned
out to need rather than by the candidate list in spec §4.6. The known
consumers today: TC.5's event hooks and the twelve tmux hook names behind them
(FE.3), which opensessions (TP.9) uses; the agents plugin (M7), which needs
option changes on pane options, pane focus, pane creation and exit, session
and window closing, `restore-done` and `resume-changed`; and nothing in the
status line, which stays formats over options (D8). An event with no consumer
does not enter v1.

**Before Wave A starts:** T6.0 approved; **stub edit** by the orchestrator on
`main`: `src/server/plugins.rs` and `src/server/dispatch/plugins.rs` with
their `mod` lines; `Command` variants `ListPlugins` and `PluginRestart`,
routed to stubs; a `control` arm stub in `src/main.rs`.

**Wave 0 (during M5).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T6.0** Spec §7 rewrite | From `docs/dogfood.md`, the daily `compat-miss.log` reviews, TC.5's event list and M7's needs (spec §7): rewrite spec §7's plugin part and §4.6's events paragraph into an **event catalog** (event, payload fields, emitter, first consumer), the protocol v1 rules, the `[plugins.<name>]` schema | spec §7 (plugin part), §4.6's events paragraph, §5.1's layer 5 | T4.6 green (not the owner's reboot) | Every catalog event names at least one consumer (a hook name row, the agents plugin, or a test) and no event is listed without one; every M7 need in spec §7 maps to a catalog event or a command; **[owner]** approves the rewrite |

**Wave A (after the stub edit; parallel; merge order T6.1).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T6.1** Event catalog | T6.0's catalog on the `subscribe` stream: typed variants in `event.rs`, emitted from the internal events TC.5 already raises (one emission site per event); `pane-focus` (a pane becomes the active pane of the current window of an attached client, or that client's terminal regains focus while it is); `restore-done` and `resume-changed` from M4; `subscribe <event>... [-t target]` filters; per-pane events arrive in the order of their actions; `events-dropped` unchanged | `src/event.rs`, `src/server/events.rs`, the emission sites in `src/server/mod.rs` and `src/server/hooks.rs` (TC.5 merged), `tests/events_v1.rs` | T6.0 | Each catalog event fires exactly once per triggering action with the catalog's payload (one JSON golden per event); `pane-focus` fires on `select-pane`, on switching window, on a click, and never for a session with no attached client; a `-t %3` subscription receives only `%3`'s events; T1.6's tests unchanged and green |

**Wave B (T6.2 and T6.3 after T6.1; merge order
T6.2, T6.3).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T6.2** Protocol v1 freeze | `docs/protocol-v1.md`: both hellos (`grofe: 1`, `dialect`), requests, replies, the event catalog, attach frames, errors; the rules: v1 is additive only (new optional fields, new events delivered only to subscribers that name them, new verbs), and a hello with another version gets a clean error naming the side to upgrade (spec §4.6); `tests/fixtures/protocol/v1/`, one JSON line per message and event kind; `grofe control`: a stdio bridge, JSON lines in and out, so a plugin in any language needs no socket code | `docs/protocol-v1.md`, additions to `src/protocol.rs`, the `control` arm in `src/main.rs`, `tests/protocol_v1.rs`, `tests/fixtures/protocol/v1/` | T6.1 | Every fixture parses and re-serializes byte-identically; every `Event` variant has a fixture (a test enumerates them); removing a field from one struct turns the suite red (proved once by a mutation the test applies); an unknown field in a request is ignored and an unknown verb errors; `printf '{"id":1,"argv":["list-sessions"]}\n' \| grofe control` prints one reply line; a `grofe control` subscription over 10 000 events drops only when its reader stalls, with an `events-dropped` notice |
| **T6.3** Plugin supervision | `[plugins.<name>]` with `cmd` (argv), `env`, `enabled` and `restart` (`always` default, `on-failure`, `never`); plugins start after config load and after any restore, with the server environment plus `GROFE` and `GROFE_PLUGIN=<name>`, stdin from `/dev/null`, stdout and stderr to `<state>/plugins/<name>.log` (rotated at 1 MiB), spawned off the actor under the spawn lock; restarts back off from 1 s, doubling to 60 s, reset after 60 s alive; five crashes within 60 s disable a plugin with a log line; on server exit SIGTERM, then SIGKILL after 2 s; `reload` starts, stops or restarts plugins whose config changed; `list-plugins` (name, pid, state, restarts, since) and `plugin-restart <name>` | `src/server/plugins.rs`, `src/server/dispatch/plugins.rs`, one registration line in `src/server/mod.rs`, the `[plugins]` schema in `src/config/`, `tests/plugins.rs` | T6.1 | A test plugin starts with the server and sees `GROFE`; after SIGKILL it is back within 1.5 s, and repeated kills space its restarts 1, 2 and 4 s apart; the fifth crash within 60 s leaves it `disabled` in `list-plugins` and spawns nothing more (counted); `kill-server` leaves no plugin process; a plugin that never reads its socket does not delay `new-session`; its stderr lands in its log; `reload` without its config stops it |

**Wave C.**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T6.6** In-tree plugins | `plugins/event-log/event-log.sh`, an out-of-process plugin in POSIX sh over `grofe control` that appends the catalog's events to `<state>/events.log` (a dogfooding aid; events while it is down are not replayed, and its README says so); README's plugin section with both | `plugins/event-log/`, README's plugin section, `tests/plugins_intree.rs` | T6.2, T6.3 | With `[plugins.event-log]` configured, a split and a `select-pane` append their events in order within 1 s; the plugin passes `shellcheck`. |

**Critical path inside M6** (review cycles included): T6.0 (2 d) and its
approval during M5; then stub edit → T6.1 (3 d) → T6.2/T6.3 (3 d, parallel) →
T6.6 (2 d), about 8 working days. T6.1 (3 d) then T6.2 and T6.3 (3 d each, parallel) end
on day 6. **One-lane sum:** T6.0 2, T6.1 3, T6.2 3, T6.3 3, T6.6 2: 13
task-days.

**Exit gate:** reviewer runs `scripts/ci-local.sh`; `docs/protocol-v1.md` and
its fixtures are tagged as v1 in the same merge; the owner's step was T6.0's
approval, so nothing else here needs the owner.

---

## M7: agents plugin and canopy on grofe

Spec §7, §7.1. Goal: canopy's agent layer runs on grofe with nothing
polled and nothing forked per report, and a reboot returns every agent pane
to its own conversation. canopy's `docs/adapters/contract.md` is the
authority on adapter manifests; grofe reads the same files. The agents plugin
ships in the grofe binary: `grofe __agents` is the supervised daemon (M6),
configured by canopy's fragment, and `grofe agents <adopt|next>` is a CLI arm,
a control client that needs no daemon. Tasks that own canopy files run in
canopy with its bats and have a grofe-side check.

**Stub edit, landed when T7.1 starts** (T7.1 and T7.5 may start during M6,
so this edit may precede M6's exit gate; Wave B waits for that gate), by the
orchestrator on `main`: `src/agents/{mod,manifest,state,identity,adopt}.rs`
with the `mod agents` line; in `agents/mod.rs`, the daemon loop's named hooks
`identity::on_start(&mut Ctx)` and `identity::on_event(&mut Ctx, &Event)`,
no-ops in `identity.rs`, so T7.2 owns `mod.rs` and T7.3 owns only
`identity.rs`; the `__agents` and `agents` arms stubbed in `src/main.rs`; the
`[agents]` schema in `src/config/`: `adapter-dirs` (list, first wins),
`option-prefix` (default `@agent_`), `enabled`.

**Wave A (parallel; may start during M6).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T7.1** Manifest reader | canopy's contract exactly: `key=value` lines, comments and blank lines, all six keys required, and exactly the rejections canopy's `lib/adapter.sh` makes (unknown or duplicate key, a malformed line, `id` differing from the directory, an empty `command`, `can_pin_at_launch` neither `yes` nor `no`, a `resume_template` without `{id}`, a non-empty `detect_draft` without `{pane}`, and `launch_template` without `{id}` only when `can_pin_at_launch` is `yes`, nothing checked when it is `no`), full validation on every read; `adapter-dirs` precedence with a shadowed id listed once; ids validated against `[A-Za-z0-9._:-]` on entry and again on use; literal substitution, then split on spaces into an argv, rejecting a template with shell quoting or metacharacters (spec §7); `detect_draft` run through `/bin/sh -c` with stdin from `/dev/null` | `src/agents/manifest.rs`, `tests/agents_manifest.rs`, `tests/fixtures/agents/manifests/` | stub edit | Over a corpus of valid and invalid manifests (every rejection, plus canopy's shipped `claude-code` and fake-agent manifests), each verdict equals the one canopy's `lib/adapter.sh` gives on the same file: live when `GROFE_COMPAT_CANOPY` is set, and against a checked-in golden of canopy's verdicts otherwise; a config-dir adapter shadows the store's and is listed once; id `a b` is refused and `abc-1.2:x_y` accepted |
| **T7.5** The four adapters (canopy) | canopy ships `adapters/{opencode,pi,codex}/` beside `claude-code`, each with a manifest per the contract and, where the agent reports, its reporter and installer per canopy §6.5's mappings (opencode TS plugin, pi TS extension, codex `notify` and hooks through an sh bridge); each adapter's tier recorded in canopy's docs, and an agent that cannot resume by id gets no adapter and says so (tier 3); adapters call `canopy agent report` and hold nothing grofe-specific | canopy `adapters/{opencode,pi,codex}/`, canopy's manifest and agent-report bats with hook-payload fixtures copied from each agent's own hook or event documentation (the source URL and version at the top of each fixture file), canopy design §6.5's table; canopy's `docs/adapters/contract.md`, so `command` is the program name read off the pane's argv (interpreters and `VAR=value` skipped), not the process name | none in grofe | canopy's bats: every manifest validates, and each reporter fed its payload fixtures produces §6.5's states; T7.1's corpus includes all four manifests and accepts them; **[owner]** runs each real agent once in a grofe pane and sees its state and resume set (substitute: the payload fixtures) |

**Wave B (after M6's exit gate and T7.1; T7.3 also after T7.5; parallel;
merge order T7.2, T7.3).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T7.2** State, rollups, attention | `grofe __agents` subscribes to option changes on `<prefix>state`, `<prefix>source` and `<prefix>session`, and to `pane-focus`, `pane-created`, `pane-exited`, window and session closing and `restore-done`; states outside the five (and `restored`) are logged as adapter bugs and left out of rollups; rollups as window and session `<prefix>rollup` in spec §7's urgency order, written only on change and cleared when no agent pane remains; attention: entering `blocked` or `completed` sets `<prefix>unseen` (the transition time) unless an attached client is looking at the pane, and `pane-focus` clears it; after a restore, panes with a persisted `<prefix>source` and no report yet get `<prefix>state restored`; on connect it recomputes everything from one `list-panes -a -F`; it forks nothing; `grofe agents next` selects the pane with the oldest unseen time, switching the client's session and window | `src/agents/{mod,state}.rs`, the `__agents` and `agents next` arms in `src/main.rs`, `tests/agents_state.rs` | T7.1 | With the plugin at prefix `@canopy_agent_` and states set through the `tmux` symlink as canopy's report does (`set -p ... \; set -p ...`): a table of about 15 report sequences gives the expected window and session rollups; a repeated `working` writes nothing (the `option-changed` count is unchanged); `blocked` on an unfocused pane sets unseen and on the focused pane does not; `select-pane` to it clears it; `agents next` lands on the oldest unseen pane; after a restore (T4.4's harness) persisted panes read `restored` until their first report; an invalid state logs once and leaves rollups unchanged; the daemon's child count stays 0 over 1000 reports; SIGKILLed, it is restarted (T6.3) and its first recompute matches the rollups before |
| **T7.3** Identity and resume | Spec §7, wired into the daemon through the stub's `identity::on_start` and `on_event` hooks: the plugin calls `pane-set-resume` for the adapter whenever a pane's `<prefix>session` changes (a `/clear`-style new id with an unchanged state included) and `<prefix>source` names an installed adapter, using its `resume_template`; for panes that never report, identity from `pane_current_argv` (never the command name) on `pane-created`, `restore-done` and plugin start: interpreters (`sh`, `bash`, `zsh`, `env`), options and `VAR=value` skipped, the program matched against each adapter's `command`, the id read back through `resume_template` first and `launch_template` second (canopy 5aaac53, §6.6); it sets `<prefix>source` and `<prefix>session` with `--persist`, never `<prefix>state`; an agent pane with no id gets `pane-set-resume --expect "no session id reported"` | `src/agents/identity.rs`, `tests/agents_identity.rs`, `tests/fixtures/agents/fake-agent-adapter/` (canopy's, vendored), `examples/comm_agent.rs` (a compiled fake agent, so its comm really differs from its argv) | T7.1, T7.5, T4.2 | With canopy's fake-agent adapter: a pane running `fake-agent --session-id a` that never reports has `#{pane_resume}` equal to `<abs>/fake-agent --resume a` within 1 s of plugin start; reports of session `b` and then `c` with the same state leave `--resume c`; `env FOO=1 fake-agent --session-id d` and `sh -c 'exec fake-agent --session-id e'` are recognised, and so is `comm_agent` copied to a file named `2.1.245` and started as `exec -a fake-agent ./2.1.245 --session-id f` (comm `2.1.245`, argv `fake-agent`, as Claude Code looks); each of the four adapters' launch argv (T7.5's manifests, from fixtures) is recognised; a server started under `env -i PATH=/usr/bin:/bin`, with the fake agent installed only on the pane's `PATH`, still records an absolute resume (the plugin's call has no pane context, spec §6); `fake-agent --anonymous` gets the expectation and `reboot-check` exits 1 naming it, and closing it brings exit 0; a reported id with a space is refused with a log line and sets no resume; after `kill_hard()` and `restart()` every pane with an id resumes its own conversation (transcripts), `source` and `session` are back, `state` is not |

**Wave C (T7.4 and T7.6 after Wave B, parallel; T7.7 after T7.4; merge order
T7.4, T7.6, T7.7).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T7.4** Adopt | Spec §7.1: `grofe agents adopt [--yes] [-t target]`; for each pane whose foreground argv is an adapter's command with an id recoverable from its report or argv, and whose resume is not already that adapter's resume for that id: a dry run (the default) prints `would adopt <pane> <adapter> <id>`, and `--yes` calls `pane-set-resume`; nothing is respawned, so `detect_draft` is not needed and the output says so; it never invents an id (a pane without one is named and left alone), validates ids on use, and a pane already carrying its resume counts as adopted; exit 0, or 1 when any agent pane was left alone | `src/agents/adopt.rs`, the `agents adopt` arm in `src/main.rs`, `tests/agents_adopt.rs` | T7.2, T7.3 | The dry run changes nothing (`#{pane_resume}` and `state.json` unchanged); `--yes` sets each resume, and a second `--yes` reports every pane adopted and emits no `resume-changed`; an anonymous pane is named, skipped, and gives exit 1; a pane whose `source` names a missing adapter is named with that reason; no `pane_pid` changes; after adopting, `reboot-check` exits 0 for those panes |
| **T7.6** canopy's agents fragment (canopy) | TP.1's generator gets a `40-agents` template: `[plugins.agents]` running `grofe __agents`, `[agents]` with `adapter-dirs` (`$CANOPY_CONFIG/adapters`, then `$CANOPY_STORE/adapters`) and `option-prefix = "@canopy_agent_"`; pane borders from the pane state, window status from the window rollup, status-right from the session rollup, an unseen marker, all pure formats with no `#()` (canopy §6.3); a binding for `grofe agents next` and canopy's `agent next` calling it on the grofe backend | canopy's `40-agents` template and its bats; `tests/acceptance/t7_6.rs` in grofe | T7.2 | `grofe show-config --file` on the generated tree loads with no errors; with states set by `canopy agent report` through the `tmux` symlink, the reconstructed border and status show each of the six states (screen goldens); redraws at 60 Hz spawn no process beyond the named sources (counted, TC.4's method); `canopy restore --all` removes the fragment |
| **T7.7** Autostart, doctor and wrappers (canopy) | canopy installs a login unit through its transaction (owned before written, restored by `canopy restore`): a systemd user unit on Linux and a launchd agent on macOS, each running `tmux start-server --restore` through the `tmux` symlink (tmux identity, D12) with the login `PATH`; `canopy doctor` on grofe checks `restore-on-start` or the unit, a writable state dir, `grofe reboot-check` not exiting 2, a fresh snapshot (no pane `not saved yet` two seconds after the last change), and prints the tier matrix; `canopy reboot-check` on grofe prints `grofe reboot-check` and exits with its status; `canopy adopt` runs `grofe agents adopt` with the same flags; the grofe backend carries no resurrect or continuum path | canopy's grofe-backend `bin/` wrappers, the unit templates, the doctor checks, and their bats | T7.4, T4.5 | canopy's bats run with a temp `HOME` and stub `systemctl` and `launchctl` first on `PATH` that record their calls, so no unit is ever installed or loaded on the host; against a scripted grofe server on a private `-S`: doctor passes with the unit and fails naming each missing piece, including a stale snapshot (a pane added while the state dir is read-only); `canopy reboot-check` exits as `grofe reboot-check` does in the three cases; `canopy adopt`'s dry-run output equals grofe's; with `restore-on-start` off, the installed unit keeps `grofe reboot-check` from exiting 2; `canopy restore --all` removes the unit and returns the home byte-identical |

**Wave D (after the orchestrator bumps canopy's pinned SHA, the one TC.13
and every container use, to include T7.5, T7.6 and T7.7).**

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **T7.8** Container: canopy on grofe | `tests/e2e/Containerfile.systemd` (Debian with systemd as PID 1, `--systemd=always`, a test user with lingering) and `tests/e2e/scenarios/canopy.sh`, run through T4.6's `scripts/e2e.sh`: canopy copied in with `git archive` at the pinned SHA, never a working tree or the owner's install; grofe installed, the `tmux` symlink first on the user's `PATH`; `canopy install` with the grofe backend and the login unit enabled; canopy's fake-agent adapter installed from `test/fixtures/fake-agent-adapter`. Phase 1: four agent panes over two sessions: two pinned at launch, one started anonymously that later reports an id through `canopy agent report --session-id` (the tier 2 path), one anonymous that never reports; states driven by `canopy agent report` from inside each pane, as adapter hooks do. `podman restart`: systemd boots and the unit starts grofe with restore. Phase 2 checks | `tests/e2e/Containerfile.systemd` (except its tmux lines, TE.3's), `tests/e2e/scenarios/canopy.sh`, the `--systemd=always` run line for that image in `scripts/e2e.sh` | T7.6, T7.7, T4.6 | Phase 1: rollups and unseen follow the reports; `canopy reboot-check` names the never-reporting pane and exits 1. Phase 2: each fake agent's transcript is `launch <id>` then `resume <id>`, in the window that held it (canopy scenario 7), the tier 2 pane included; the never-reporting pane is a shell in its directory and costs no other pane (scenario 8); every `uid` equals phase 1's; states read `restored` until the next report; `canopy doctor` passes. A mutation run (phase 1 edits one resume in `state.json`) fails on that pane only; the host-state check passes |

**Critical path inside M7** (review cycles included): T7.1 (2 d) and T7.5
(4 d) during M6; then stub edit → T7.2 (4 d) → T7.4 (3 d) → T7.7 (3 d) → T7.8
(4 d), about 14 working days. T7.3 (3 d) runs beside T7.2, T7.6 (3 d) beside
T7.4. **One-lane sum:** T7.1 2, T7.2 4, T7.3 3, T7.4 3, T7.5 4, T7.6 3, T7.7 3,
T7.8 4: 26 task-days.

**Exit gate:** reviewer runs `scripts/ci-local.sh` with the `e2e` leg (T4.6
and T7.8 scenarios); **[owner]** on each machine, a real reboot returns every
Claude Code pane to its own conversation and `canopy reboot-check` said so
beforehand, logged in `docs/dogfood.md` (substitute: T7.8 green).

---

## E2E and release

The gate between M7 and "fully done": every suite runs from a clean clone, in
containers where the owner's machines are Linux and natively (private
sockets) on macOS, then the owner switches for good. TE.1 and TE.2 may start
as soon as their suites exist (T4.6 and TM.7); TE.3 runs last.

| Task | Scope | Owns | Depends | Accept |
|---|---|---|---|---|
| **TE.1** Acceptance in containers | `tests/e2e/scenarios/acceptance.sh`: the spec §8 acceptance suite (`cargo test` acceptance targets and bats) inside the Debian image (required) and the Arch image (optional, `E2E_ARCH=1`, never gating), with neovim, fzf, less, htop, git, kitten where packaged, and tmux 3.7b built at a pinned tag for the nested-tmux item; the macOS leg stays `scripts/ci-local.sh macos` | `tests/e2e/scenarios/acceptance.sh`, the tool lines in `tests/e2e/Containerfile.{debian,arch}` | T4.6 | Green on the Debian image, and on Arch when run; the images' `tmux -V` prints `tmux 3.7b` (the scenario checks); a tool missing from an image fails the scenario naming it rather than skipping |
| **TE.2** Compat in containers | `tests/e2e/scenarios/compat.sh`: TC.13's suite and TM.7's copy-mode parity inside the Debian image, with canopy and athome copied in at their pinned SHAs; the goldens' re-record path (TC.0b) also runs in the image | `tests/e2e/scenarios/compat.sh` | TE.1, TM.7 | Both suites green; re-recording in the image gives byte-identical goldens; `compat-miss.log` empty |
| **TE.3** Release gate | `scripts/release-gate.sh`: `ci-local.sh macos` and `linux`, then the `e2e` scenarios `acceptance` (TE.1), `compat` (TE.2), `reboot` (T4.6) and `canopy` (T7.8), then `tests/e2e/scenarios/rollback.sh`, which needs real tmux 3.7b in the systemd image: with canopy on grofe installed in the systemd image, remove the `tmux` symlink and switch canopy back to its tmux backend; real tmux and canopy work as before and `canopy restore --all` returns the home byte-identical; README's "alias day" section with the owner's steps below, run by `tests/e2e/scenarios/readme.sh` | `scripts/release-gate.sh`, `tests/e2e/scenarios/{rollback,readme}.sh`, the tmux 3.7b build lines in `tests/e2e/Containerfile.systemd`, README's alias-day section | TE.1, TE.2, T7.8 | Exits 0 only when every required leg is green, and prints one line per leg; forcing one leg red (`GROFE_GATE_FAIL=<leg>`) makes it exit nonzero naming that leg; `readme.sh` runs the README's alias-day steps in a clean systemd container and passes |

**Critical path:** TE.3 (3 d) after M7; TE.1 (3 d) and TE.2 (2 d) run
earlier. **One-lane sum:** TE.1 3, TE.2 2, TE.3 3: 8 task-days.

**Alias day, for good.** M3.6's alias day was a trial with real tmux as the
fallback. This one makes grofe the owner's multiplexer, on the release tag
and after TE.3 is green. Each step is **[owner]**; each has its substitute in
TE.3's legs.

1. Pin grofe and canopy to the release tag and SHA in athome, and apply athome
   the way its `AGENTS.md` says: `chezmoi diff`, capture anything precious,
   then apply. The change puts the `tmux` symlink first on `PATH`, installs
   grofe's config, and switches canopy to its grofe backend. (Substitute:
   T7.8's install and TE.3's README run.)
2. `canopy install` with the grofe backend installs the login unit; `canopy
   doctor` passes; `grofe reboot-check` exits 0. (Substitute: T7.7 and T7.8's
   phase 1.)
3. Reboot for real, on each machine: every session is back with its `uid`s,
   and every Claude Code pane is back in its own conversation. (Substitute:
   T4.6 and T7.8.)
4. A week of normal work under the alias. Each difference goes in
   `docs/dogfood.md` tagged `compat` or `copy`, becomes a scenario, then gets
   fixed; `compat-miss.log` is read each day. (Substitute: TC.13, TM.7, TP.9a and TP.9b.)
5. Real tmux stays installed, reachable as `env -u TMUX <absolute path>`, until
   that week ends with no open entry; removing it is the owner's call.
   **Rollback** at any point is TE.3's drill: revert the athome change (the
   symlink goes), `canopy install` with the tmux backend; grofe's state stays in
   its own directory and touches nothing of tmux's.

**Fully done** means M7's exit gate, TE.3 green, and step 4's week ending
with no open `compat` or `copy` entry.

---

## M8 (milestone level)

**M8 Compatibility extras.** Decide after M7 whether a tmux `-CC` subset earns
its keep. (`import-tmux` moved to M3.5.)
