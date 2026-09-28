# Dogfood log

The dogfood week runs grofe as the daily multiplexer before alias day
(plan M3.6). Every rough edge goes in this file as one entry, so M3.6 works
from a list rather than from memory. Add entries at the bottom, newest last.

## Before you start

- **Terminals:** Ghostty and WezTerm are the first-class outer terminals
  (spec D11). WezTerm speaks kitty keys only with
  `enable_kitty_keyboard = true` in its config.
- **Copy mode: use shift-drag until M5.** grofe has no copy mode yet. With
  `mouse` on, the outer terminal hands clicks and drags to grofe; hold Shift
  while dragging to use the terminal's own selection and copy instead. The
  wheel opens a read-only scrollback view (`q` or Escape leaves it); it is
  not tmux's copy mode and cannot select.
- **Where things are written** (`<state dir>` is `$XDG_STATE_HOME/grofe`,
  else `~/.local/state/grofe`; `<instance>` is the socket's instance name,
  `default` unless you used `-L` or `-S`):
  - the server log: `<state dir>/<instance>.log`
  - panic reports: `<state dir>/<instance>.panic.<unix-ms>.log`, one per
    panic in the server, an attach client or a CLI call. Each names the
    message, the source location, the thread, a backtrace, the grofe
    version and the OS. Attach the report to the entry.
  - `compat-miss.log` (from M3.6, under tmux identity): `<state
    dir>/compat-miss.log`, every tmux verb, flag or format variable grofe
    did not know. Review it at the end of each day.
- **Falling back to tmux.** Detach grofe (`prefix d`) and start tmux as
  usual; grofe keeps running and `grofe attach` returns to it. Inside a
  grofe pane, run tmux as `env -u TMUX <absolute path to tmux>`, since
  `$TMUX` may point at grofe (spec D12). Log every fallback: the reason is
  the most useful part of an entry.

## Known gaps at the start of the week

Found by the T3.6 acceptance suite; no need to log them again unless they
hurt more than described.

- **VS16 emoji width.** An emoji made wide by VS16 (`❤️`) is one cell in
  grofe's emulator and two in Ghostty and WezTerm. grofe moves the cursor
  absolutely after it, so the next cell does not drift, but the emoji can
  overlap its right neighbour.
- **`kitten icat --unicode-placeholder`** runs through grofe in the
  suite only where `kitten` is installed (T3.8's rule); the recorded
  placeholder fixture always runs. Try one image preview by hand in each
  terminal on day one and log what you see.
- **XTVERSION** (`CSI > q`) from an app gets no reply (spec §8). An app that
  waits for one without a DA1 fence would hang until its own timeout.

## Entry template

Copy this block for each entry.

```markdown
### <YYYY-MM-DD> <one-line summary>

- Terminal: Ghostty | WezTerm (version)
- OS: macOS | Linux (version)
- grofe: the commit it was built from (`git rev-parse --short HEAD`)
- What hurt: what you did, what you expected, what happened
- Severity: blocker (alias day cannot happen) | annoying (daily friction) |
  cosmetic
- tmux fallback used: no | yes, because ...
- Evidence: panic report, server log lines, `compat-miss.log` lines, a
  screenshot, or the smallest command that reproduces it
```

## Entries
