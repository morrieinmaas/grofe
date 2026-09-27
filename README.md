# grofe

**tmux, if it were designed today.** Same sessions, windows and panes, same
prefix key, same `split-window` and `send-keys`. Your muscle memory transfers in
five minutes. What changes is everything tmux users have been working around for
twenty years.

> **Status: early development.** The foundations are built and tested (milestone
> M0); nothing is usable day to day yet. Everything below is the design, each
> feature tagged with the milestone that delivers it. See the
> [design](docs/specs/2026-09-23-grofe-design.md) and the
> [plan](docs/plans/2026-09-23-milestones.md).

## Why grofe

### 1. Reboot, and your panes come back running the same things (M4)

tmux loses everything when the server dies. The standard fix is
resurrect plus continuum: a save file, a regex of which processes may be
restored, a strategy script per program, and a timer that only saves while a
client is attached.

In grofe, a pane has a **durable identity** and a **resume command**. They are
fields on the pane, not a convention between plugins. A Claude Code session, an
nvim session or a REPL registers how to bring itself back, and after a crash or
a reboot the pane returns with the same identity, running the same thing, in the
same place. `grofe reboot-check` tells you, before you reboot, exactly which
panes will come back and why the others will not.

### 2. A config you can reason about (M3)

tmux config is an untyped command stream with `if-shell` as its only
conditional. Errors are reported without a file or line, `source-file` with a
glob reports success if anything matched, and the only way to learn what a
setting actually resolved to is to ask a running server.

grofe's config is TOML with a schema:

- **Typed.** A string where a number belongs is an error naming the file,
  line and key, never a silent default. Unknown keys get a "did you mean".
- **Layered, with provenance.** `grofe show-config status.right --explain` tells
  you the value and which file set it. `--file` answers without a running server.
- **Your file always wins.** Tools drop fragments into `conf.d/`; your own file
  loads last.
- **Reload is atomic.** Every layer is validated first; either all of it applies
  or none of it does, and you see every error at once.
- **Lua when you need logic** (M6), instead of shell snippets inside strings.

### 3. A status line that cannot slow you down (M2)

The real performance problem in tmux setups is not the redraw, it is `#()` in
the status line: a fork per segment, per redraw, forever. grofe has **no
subprocesses in the status line**, by design. Segments are formats over options,
and anything dynamic is pushed in by whatever knows the value. On a real setup,
removing 20 forks from one status line cut render time from ~97 ms to ~67 ms.

### 4. Modern terminals, by default (M2 to M3)

- **The kitty keyboard protocol, per pane.** nvim, helix and coding agents ask
  for it; tmux's `extended-keys` quietly does not deliver it, which is why a
  bare `q` sometimes does nothing inside tmux. grofe encodes keys for each pane
  in the mode that pane asked for.
- True colour, synchronized output (no tearing), OSC 52 clipboard, hyperlinks,
  focus events and bracketed paste, all on.
- **Saner defaults:** windows and panes numbered from 1, mouse on, 25 ms escape
  time instead of 500, new panes open in the current directory, a large
  scrollback.
- **An honest `TERM`.** Panes get a terminfo entry whose every capability grofe
  actually implements, instead of a promise it does not keep.

### 5. Scriptable all the way down (M1, M6)

Every action is a command, and every command works the same from the CLI, a key
binding, or `grofe control`: a JSON-lines protocol with request ids, structured
errors and an **event stream** (panes exiting, focus changing, options set).
Scripts and tests drive grofe headlessly. Plugins (M6) are ordinary programs in
any language speaking that same protocol, or Lua for the synchronous cases. No
polling, no WASM runtime.

### 6. Made for running agents (M7)

If you run several coding agents at once, grofe is built for you. Pane identity,
resume commands and pane options are core; on top, a first-party `agents` plugin
tracks five normalized states (`idle`, `working`, `blocked`, `completed`,
`exited`) for Claude Code, opencode, pi and codex, shows them in borders and the
status line without a single fork, and jumps you to the agent that needs you.
After a reboot, each agent pane returns to its own conversation.

## Why Rust

Not for speed; tmux is already fast. For a **single static binary**, for
mature terminal emulation libraries (grofe builds on `alacritty_terminal`), and
for memory safety in a long-lived daemon that holds your live work. A client
crash cannot take your panes with it: all state lives in the server.

## What grofe is not

- **Not a tmux drop-in.** It does not read `tmux.conf` or run tmux plugins. The
  object model, keys and command names are familiar; the config is new.
- **Not Zellij.** tmux semantics, no modal UI, no WASM plugins.
- **Not for Windows**, and in v1 it does not pass through image protocols (sixel,
  kitty graphics): inline images from agents fall back to half-blocks.

## Compared

| | tmux | Zellij | grofe |
|---|---|---|---|
| tmux keys and command names | yes | no | yes |
| Panes survive reboot running their programs | via plugins | session resurrection (layout and commands) | native (M4) |
| Typed config with provenance | no | KDL, no provenance | TOML + schema (M3) |
| Status line forks | per segment, per redraw | plugins | none, by design |
| Kitty keyboard protocol per pane | limited | yes | yes (M2) |
| Scripting | CLI + `-CC` | CLI + WASM | CLI + JSON events (M1), plugins in any language (M6) |
| Agent awareness | via plugins | no | first-party plugin (M7) |

## Try it

Not yet. The single-pane spike runs today (`cargo run --example spike`, quit
with `Ctrl-]`); the first daily-drivable build is milestone M3.
