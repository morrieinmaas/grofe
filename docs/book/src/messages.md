# Messages and the prompt

A message (`display-message`, or an error such as `Can't find window`) is a
pill in the middle of the status line, between its left and right parts,
which stay visible:

- grey with `ℹ` for information,
- amber with `▲` for a warning (alerts),
- red with `✗` for an error.

It goes after `display-time` milliseconds, as in tmux, or at the next key.
The pill's ends are `pane-frame-tab-ends`, the frames' glyphs.

The command prompt (`prefix :`, `command-prompt`, `confirm-before`) opens as
the same kind of pill on the status line, or as a rounded box a third down
the window:

```toml
prompt-position = "status"  # "centre" (or "center"): a box in the middle of the window

[message]
pill = true                 # false: tmux's full-width bar in message-style
```

Under tmux identity `pill` defaults to `false`, so a message looks as it does
in tmux; the config can still turn it on.
