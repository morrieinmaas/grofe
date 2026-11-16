# Scratchpads

A scratchpad is a floating pane for a quick command that you want out of the
way.

| Key | Does |
|---|---|
| `prefix S` | hide the scratchpad; again, show the newest hidden one (or open one if none is hidden) |
| `prefix a` | open another, centred over the tiles |
| `prefix A` | list the hidden scratchpads, newest first, each keyed `0` to `9` |

Hidden scratchpads belong to the whole server, so you can show one in any
window. Unfloat a scratchpad with `prefix f` and it becomes a normal pane.

The commands are `new-scratchpad`, `toggle-scratchpad`,
`show-scratchpad <pane-id>`, `list-scratchpads` and `choose-scratchpad`;
rebind them in `[keys]` like any other.

```toml
scratchpad-history = 10     # hidden scratchpads kept; the oldest closes past this, 0 closes on hide
```
