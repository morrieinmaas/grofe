# Messages and the prompt

A message (`display-message`, or an error such as `Can't find window`) is a
pill alone in the middle of the status line. While it shows, the status
line's own parts (session, windows, the right-hand segments) step aside:

- blue with `ℹ` for information,
- amber with `▲` for a warning (alerts),
- red with `✗` for an error.

It goes after `display-time` milliseconds, as in tmux, or at the next key.

The command prompt (`prefix :`, `command-prompt`, `confirm-before`) opens as
the same kind of pill on the status line, as wide as what you type (it
scrolls past the window's width), or as a rounded box a third down the
window:

```toml
prompt-position = "status"  # "centre" (or "center"): a box in the middle of the window

[message]
pill = true                 # false: tmux's full-width bar in message-style
pill-ends = ""              # two glyphs for the pill's ends; empty: square ends
```

`pill-ends` takes any two one-cell glyphs, drawn in the pill's colour. With a
Nerd font, the rounded caps U+E0B6 and U+E0B4 match a status line of pills:

```toml
[message]
pill-ends = ""
```

Under tmux identity `pill` defaults to `false`, so a message looks as it does
in tmux; the config can still turn it on.
