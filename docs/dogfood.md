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

### 2026-10-01 Daily bindings, checked by script (TP.9a)

The owner ruled that this check runs as a script, not by hand.
`scripts/key-parity.sh` presses each daily binding once, through a client
attached on a pty, with the whole config and every plugin loaded. It does
this on real tmux 3.7b and on two grofe servers, one under the tmux name and
one under grofe's own. A key passes when grofe ran what tmux ran: the same
script or stub, with the same argv, in the same directory, in a popup of the
same size. Your own tools are stubs that log a line; the plugin scripts run
unchanged. Tick the last column once you have used the key yourself.

| Key | What it does | Script (grofe under the tmux name, and under its own) | You |
|---|---|---|---|
| `prefix s` | sessionx's picker (fzf-tmux, the session list) | [x] same as tmux | [ ] |
| `prefix K` | `~/.local/bin/tsess` | [x] same as tmux | [ ] |
| `prefix ?` | `keys-popup.sh`, the searchable key list | [x] same as tmux | [ ] |
| `prefix u` | fzf-url's picker over the pane's URLs | [x] same as tmux | [ ] |
| `prefix g` | gh-dash in a 95% by 90% popup | [x] same as tmux | [ ] |
| `prefix G` | `enhance-pr.sh` in a popup, in the pane's directory | [x] same as tmux | [ ] |
| `prefix P` | `pet-pick.sh` in a borderless popup | [x] same as tmux | [ ] |
| `prefix T` | `theme` with `THEME_POPUP=1` in a popup | [x] same as tmux | [ ] |
| `prefix W` | `net-popup.sh` in the 66 by 26 network popup | [x] same as tmux | [ ] |
| `prefix a` | claude-session-manager's agent for this directory, in a popup | [x] same as tmux | [ ] |
| `prefix A` | claude-session-manager's agent picker | [x] same as tmux | [ ] |

- grofe: built from TP.9a's branch
- What the script cannot see: how a popup looks (its border and title) and
  what you do inside it. Those are yours to judge when you tick a row.
