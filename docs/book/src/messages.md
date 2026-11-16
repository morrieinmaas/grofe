# Messages and the prompt

A message (`display-message`, or an error such as `Can't find window`) is one
wide pill over the left of the status line: the session and the window list
step aside while it shows, and `status-right` stays where it is.

- blue with `ℹ` for information,
- amber with `▲` for a warning (alerts),
- red with `✗` for an error.

It goes after `display-time` milliseconds, as in tmux, or at the next key.

The command prompt (`prefix :`, `command-prompt`, `confirm-before`) opens as
the same pill, in the information colour, with what you type scrolling inside
it, or as a rounded box a third down the window:

```toml
prompt-position = "status"  # "centre" (or "center"): a box in the middle of the window

[message]
pill = true                 # false: tmux's full-width bar in message-style
pill-ends = ""              # two glyphs for the pill's ends; empty: the theme's, else round caps
```

The pill follows a theme that sets these user options, as the canopy status
line does:

| Option | Used for |
|---|---|
| `@theme_blue`, `@theme_yellow`, `@theme_red` | the information, warning and error colour |
| `@theme_bg` | the text on the pill |
| `@pill_l`, `@pill_r` | the pill's two ends |

Without them the colours are the terminal's named blue, yellow and red, and
the ends are the Nerd Font round caps U+E0B6 and U+E0B4. `pill-ends` takes
any two one-cell glyphs instead; two spaces give square ends:

```toml
[message]
pill-ends = "  "
```

Under tmux identity `pill` defaults to `false`, so a message looks as it does
in tmux; the config can still turn it on.
