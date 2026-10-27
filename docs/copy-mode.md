# Copy mode: scope

This is M5's scope (spec §4.11, plan task TM.0). The owner's `copy-mode-vi`
table is the whole scope: every binding in it works as it does under tmux
3.7b, and nothing outside it is built without a dogfooding reason. Every
later M5 task, TP.11 and TP.9b check themselves against the table below.

## Where the table comes from

- `tests/fixtures/compat/copy-mode.conf` holds what the owner's setup adds
  to tmux's stock table, reproduced from the sources, never from a live
  server. tmux-yank's five rows come from its vendored `yank.tmux` with the
  plugin's defaults. canopy's port of vim-tmux-navigator
  (`40-canopy-nav.toml`) defers its five copy-mode rows to M5; the plugin is
  not vendored, so their argv comes from the TC.0 container recording of the
  old config (`tests/compat/corpus/container-stored.argv`).
- `tests/fixtures/compat/golden/tm_0/list-keys-copy-mode-vi.scenario` starts
  tmux 3.7b with `tmux-defaults.conf`, sources `copy-mode.conf` and runs
  `list-keys -T copy-mode-vi`. TC.0b's recorder
  (`scripts/record-compat-goldens.sh tm_0`) wrote its `.golden`, and
  `list-keys-copy-mode-vi.txt` is that output alone.
- `scripts/check-copy-scope.sh` checks that every golden line has exactly
  one row below and every row one line, and that each row names one M5 task
  and a scenario id.

**The golden has 95 bindings, not the inventory's 93.** It is tmux 3.7b's
88 stock rows, 3 of them replaced (`MouseDragEnd1Pane` by yank, `C-h` and
`C-j` by the navigator), plus the 7 rows below that stock tmux does not
have. The M1 inventory counted 93 without listing the rows, so the gap
cannot be pinned on two particular rows; the TC.0 container recording of
the old config has 94, because yank found no clipboard command there and
bound only its error message to `y`, `Y` and `M-y`. The 7 added rows:

- Difference `y`: tmux-yank's copy, `copy-pipe-and-cancel pbcopy`.
- Difference `Y`: tmux-yank's put, which stores the selection and runs `tmux paste-buffer -p`.
- Difference `M-y`: tmux-yank's copy then put.
- Difference `!`: tmux-yank's copy without newlines, a helper for its own `copy_line.sh`.
- Difference `C-k`: vim-tmux-navigator, `select-pane -U`.
- Difference `C-l`: vim-tmux-navigator, `select-pane -R`.
- Difference `C-\\`: vim-tmux-navigator, `select-pane -l`.

The copy command in yank's rows is `pbcopy`, what the plugin picks on
macOS. On Linux it picks `wl-copy`, `xsel` or `xclip` instead, which
changes the command string but not the set of keys.

## Keymap

Columns: the key as `list-keys` prints it; tmux's command; the `send-keys -X`
command grofe implements for it (or none, when the row runs an ordinary
command); the M5 task that builds it; the parity scenario TM.7 recorded,
`tests/fixtures/compat/golden/tm_7/<id>.scenario` (see "The tmux front
end" below). TM.1 builds motions, scrolling, cancel and counts;
TM.2 selection; TM.3 search and jumps; TM.5 copy targets; TM.6 the mouse.

| Key | tmux command | grofe command | Task | Scenario |
|---|---|---|---|---|
| `Enter` | `send-keys -X copy-pipe-and-cancel` | `copy-pipe-and-cancel` | TM.5 | `copy_mode/enter` |
| `Escape` | `send-keys -X clear-selection` | `clear-selection` | TM.2 | `copy_mode/escape` |
| `Space` | `send-keys -X begin-selection` | `begin-selection` | TM.2 | `copy_mode/space` |
| `!` | `send-keys -X copy-pipe-and-cancel "tr -d '\n' \| pbcopy"` | `copy-pipe-and-cancel` | TM.5 | `copy_mode/bang` |
| `\#` | `send-keys -FX search-backward -- "#{copy_cursor_word}"` | `search-backward` | TM.3 | `copy_mode/hash` |
| `\$` | `send-keys -X end-of-line` | `end-of-line` | TM.1 | `copy_mode/dollar` |
| `\%` | `send-keys -X next-matching-bracket` | `next-matching-bracket` | TM.1 | `copy_mode/percent` |
| `*` | `send-keys -FX search-forward -- "#{copy_cursor_word}"` | `search-forward` | TM.3 | `copy_mode/star` |
| `,` | `send-keys -X jump-reverse` | `jump-reverse` | TM.3 | `copy_mode/comma` |
| `/` | `command-prompt -T search -p "(search down)" { send-keys -X search-forward -- "%%" }` | `search-forward` | TM.3 | `copy_mode/slash` |
| `0` | `send-keys -X start-of-line` | `start-of-line` | TM.1 | `copy_mode/digit-0` |
| `1` | `command-prompt -N -I 1 -p (repeat) { send-keys -N "%%" }` | none (`send-keys -N` count) | TM.1 | `copy_mode/digit-1` |
| `2` | `command-prompt -N -I 2 -p (repeat) { send-keys -N "%%" }` | none (`send-keys -N` count) | TM.1 | `copy_mode/digit-2` |
| `3` | `command-prompt -N -I 3 -p (repeat) { send-keys -N "%%" }` | none (`send-keys -N` count) | TM.1 | `copy_mode/digit-3` |
| `4` | `command-prompt -N -I 4 -p (repeat) { send-keys -N "%%" }` | none (`send-keys -N` count) | TM.1 | `copy_mode/digit-4` |
| `5` | `command-prompt -N -I 5 -p (repeat) { send-keys -N "%%" }` | none (`send-keys -N` count) | TM.1 | `copy_mode/digit-5` |
| `6` | `command-prompt -N -I 6 -p (repeat) { send-keys -N "%%" }` | none (`send-keys -N` count) | TM.1 | `copy_mode/digit-6` |
| `7` | `command-prompt -N -I 7 -p (repeat) { send-keys -N "%%" }` | none (`send-keys -N` count) | TM.1 | `copy_mode/digit-7` |
| `8` | `command-prompt -N -I 8 -p (repeat) { send-keys -N "%%" }` | none (`send-keys -N` count) | TM.1 | `copy_mode/digit-8` |
| `9` | `command-prompt -N -I 9 -p (repeat) { send-keys -N "%%" }` | none (`send-keys -N` count) | TM.1 | `copy_mode/digit-9` |
| `:` | `command-prompt -p "(goto line)" { send-keys -X goto-line -- "%%" }` | `goto-line` | TM.1 | `copy_mode/colon` |
| `\;` | `send-keys -X jump-again` | `jump-again` | TM.3 | `copy_mode/semicolon` |
| `?` | `command-prompt -T search -p "(search up)" { send-keys -X search-backward -- "%%" }` | `search-backward` | TM.3 | `copy_mode/question` |
| `A` | `send-keys -X append-selection-and-cancel` | `append-selection-and-cancel` | TM.2 | `copy_mode/upper-a` |
| `B` | `send-keys -X previous-space` | `previous-space` | TM.1 | `copy_mode/upper-b` |
| `D` | `send-keys -X copy-pipe-end-of-line-and-cancel` | `copy-pipe-end-of-line-and-cancel` | TM.5 | `copy_mode/upper-d` |
| `E` | `send-keys -X next-space-end` | `next-space-end` | TM.1 | `copy_mode/upper-e` |
| `F` | `command-prompt -1 -p "(jump backward)" { send-keys -X jump-backward -- "%%" }` | `jump-backward` | TM.3 | `copy_mode/upper-f` |
| `G` | `send-keys -X history-bottom` | `history-bottom` | TM.1 | `copy_mode/upper-g` |
| `H` | `send-keys -X top-line` | `top-line` | TM.1 | `copy_mode/upper-h` |
| `J` | `send-keys -X scroll-down` | `scroll-down` | TM.1 | `copy_mode/upper-j` |
| `K` | `send-keys -X scroll-up` | `scroll-up` | TM.1 | `copy_mode/upper-k` |
| `L` | `send-keys -X bottom-line` | `bottom-line` | TM.1 | `copy_mode/upper-l` |
| `M` | `send-keys -X middle-line` | `middle-line` | TM.1 | `copy_mode/upper-m` |
| `N` | `send-keys -X search-reverse` | `search-reverse` | TM.3 | `copy_mode/upper-n` |
| `P` | `send-keys -X toggle-position` | `toggle-position` | TM.1 | `copy_mode/upper-p` |
| `T` | `command-prompt -1 -p "(jump to backward)" { send-keys -X jump-to-backward -- "%%" }` | `jump-to-backward` | TM.3 | `copy_mode/upper-t` |
| `V` | `send-keys -X select-line` | `select-line` | TM.2 | `copy_mode/upper-v` |
| `W` | `send-keys -X next-space` | `next-space` | TM.1 | `copy_mode/upper-w` |
| `X` | `send-keys -X set-mark` | `set-mark` | TM.1 | `copy_mode/upper-x` |
| `Y` | `send-keys -X copy-pipe-and-cancel "tmux paste-buffer -p"` | `copy-pipe-and-cancel` | TM.5 | `copy_mode/upper-y` |
| `^` | `send-keys -X back-to-indentation` | `back-to-indentation` | TM.1 | `copy_mode/caret` |
| `b` | `send-keys -X previous-word` | `previous-word` | TM.1 | `copy_mode/b` |
| `e` | `send-keys -X next-word-end` | `next-word-end` | TM.1 | `copy_mode/e` |
| `f` | `command-prompt -1 -p "(jump forward)" { send-keys -X jump-forward -- "%%" }` | `jump-forward` | TM.3 | `copy_mode/f` |
| `g` | `send-keys -X history-top` | `history-top` | TM.1 | `copy_mode/g` |
| `h` | `send-keys -X cursor-left` | `cursor-left` | TM.1 | `copy_mode/h` |
| `j` | `send-keys -X cursor-down` | `cursor-down` | TM.1 | `copy_mode/j` |
| `k` | `send-keys -X cursor-up` | `cursor-up` | TM.1 | `copy_mode/k` |
| `l` | `send-keys -X cursor-right` | `cursor-right` | TM.1 | `copy_mode/l` |
| `n` | `send-keys -X search-again` | `search-again` | TM.3 | `copy_mode/n` |
| `o` | `send-keys -X other-end` | `other-end` | TM.2 | `copy_mode/o` |
| `q` | `send-keys -X cancel` | `cancel` | TM.1 | `copy_mode/q` |
| `r` | `send-keys -X refresh-from-pane` | `refresh-from-pane` | TM.1 | `copy_mode/r` |
| `t` | `command-prompt -1 -p "(jump to forward)" { send-keys -X jump-to-forward -- "%%" }` | `jump-to-forward` | TM.3 | `copy_mode/t` |
| `v` | `send-keys -X rectangle-toggle` | `rectangle-toggle` | TM.2 | `copy_mode/v` |
| `w` | `send-keys -X next-word` | `next-word` | TM.1 | `copy_mode/w` |
| `y` | `send-keys -X copy-pipe-and-cancel pbcopy` | `copy-pipe-and-cancel` | TM.5 | `copy_mode/y` |
| `z` | `send-keys -X scroll-middle` | `scroll-middle` | TM.1 | `copy_mode/z` |
| `\{` | `send-keys -X previous-paragraph` | `previous-paragraph` | TM.1 | `copy_mode/lbrace` |
| `\}` | `send-keys -X next-paragraph` | `next-paragraph` | TM.1 | `copy_mode/rbrace` |
| `BSpace` | `send-keys -X cursor-left` | `cursor-left` | TM.1 | `copy_mode/bspace` |
| `Home` | `send-keys -X start-of-line` | `start-of-line` | TM.1 | `copy_mode/home` |
| `End` | `send-keys -X end-of-line` | `end-of-line` | TM.1 | `copy_mode/end` |
| `NPage` | `send-keys -X page-down` | `page-down` | TM.1 | `copy_mode/npage` |
| `PPage` | `send-keys -X page-up` | `page-up` | TM.1 | `copy_mode/ppage` |
| `Up` | `send-keys -X cursor-up` | `cursor-up` | TM.1 | `copy_mode/up` |
| `Down` | `send-keys -X cursor-down` | `cursor-down` | TM.1 | `copy_mode/down` |
| `Left` | `send-keys -X cursor-left` | `cursor-left` | TM.1 | `copy_mode/left` |
| `Right` | `send-keys -X cursor-right` | `cursor-right` | TM.1 | `copy_mode/right` |
| `MouseDown1Pane` | `select-pane` | none (`select-pane`) | TM.6 | `copy_mode/mousedown1pane` |
| `MouseDrag1Pane` | `select-pane \; send-keys -X begin-selection` | `begin-selection` | TM.6 | `copy_mode/mousedrag1pane` |
| `MouseDragEnd1Pane` | `send-keys -X copy-pipe-and-cancel pbcopy` | `copy-pipe-and-cancel` | TM.6 | `copy_mode/mousedragend1pane` |
| `WheelDownPane` | `select-pane \; send-keys -X -N 5 scroll-down` | `scroll-down` | TM.6 | `copy_mode/wheeldownpane` |
| `WheelUpPane` | `select-pane \; send-keys -X -N 5 scroll-up` | `scroll-up` | TM.6 | `copy_mode/wheeluppane` |
| `DoubleClick1Pane` | `select-pane \; send-keys -X select-word \; run-shell -d 0.3 \; send-keys -X copy-pipe-and-cancel` | `select-word`, `copy-pipe-and-cancel` | TM.6 | `copy_mode/doubleclick1pane` |
| `TripleClick1Pane` | `select-pane \; send-keys -X select-line \; run-shell -d 0.3 \; send-keys -X copy-pipe-and-cancel` | `select-line`, `copy-pipe-and-cancel` | TM.6 | `copy_mode/tripleclick1pane` |
| `M-x` | `send-keys -X jump-to-mark` | `jump-to-mark` | TM.1 | `copy_mode/m-x` |
| `M-y` | `send-keys -X copy-pipe-and-cancel "pbcopy; tmux paste-buffer -p"` | `copy-pipe-and-cancel` | TM.5 | `copy_mode/m-y` |
| `C-[` | `send-keys -X clear-selection` | `clear-selection` | TM.2 | `copy_mode/c-lbracket` |
| `C-\\` | `select-pane -l` | none (`select-pane`) | TM.1 | `copy_mode/c-backslash` |
| `C-b` | `send-keys -X page-up` | `page-up` | TM.1 | `copy_mode/c-b` |
| `C-c` | `send-keys -X cancel` | `cancel` | TM.1 | `copy_mode/c-c` |
| `C-d` | `send-keys -X halfpage-down` | `halfpage-down` | TM.1 | `copy_mode/c-d` |
| `C-e` | `send-keys -X scroll-down` | `scroll-down` | TM.1 | `copy_mode/c-e` |
| `C-f` | `send-keys -X page-down` | `page-down` | TM.1 | `copy_mode/c-f` |
| `C-h` | `select-pane -L` | none (`select-pane`) | TM.1 | `copy_mode/c-h` |
| `C-j` | `select-pane -D` | none (`select-pane`) | TM.1 | `copy_mode/c-j` |
| `C-k` | `select-pane -U` | none (`select-pane`) | TM.1 | `copy_mode/c-k` |
| `C-l` | `select-pane -R` | none (`select-pane`) | TM.1 | `copy_mode/c-l` |
| `C-u` | `send-keys -X halfpage-up` | `halfpage-up` | TM.1 | `copy_mode/c-u` |
| `C-v` | `send-keys -X rectangle-toggle` | `rectangle-toggle` | TM.2 | `copy_mode/c-v` |
| `C-y` | `send-keys -X scroll-up` | `scroll-up` | TM.1 | `copy_mode/c-y` |
| `C-Up` | `send-keys -X scroll-up` | `scroll-up` | TM.1 | `copy_mode/c-up` |
| `C-Down` | `send-keys -X scroll-down` | `scroll-down` | TM.1 | `copy_mode/c-down` |

The same tasks also own these bindings outside the table: `prefix [`
(`copy-mode`) and `prefix PPage` (`copy-mode -u`) for TM.1, `prefix ]`
(`paste-buffer -p`) and `prefix =` (`choose-buffer -Z`) for TM.4, and the
`root` rows `WheelUpPane` (`copy-mode -e`), `MouseDrag1Pane`
(`copy-mode -M`), `DoubleClick1Pane` and `TripleClick1Pane`
(`copy-mode -H`, then `select-word` or `select-line` and a copy) for TM.6.

## The copy and its limits

As tmux 3.7b does, copy mode works on a copy of the pane's history and
screen, taken on entry: output that arrives meanwhile moves neither the
view nor the text, and `refresh-from-pane` (`r`) takes a new copy,
keeping the view's distance from the top of history. A killed or
respawned pane leaves the mode and its copy is freed. A tab is one cell
as wide as the stop it moved to, as in tmux, so motions step over it
whole.

Where grofe differs:

- **Tab stops.** grofe finds a tab's width from stops every 8 columns,
  not from the pane's own stops, so a program that moves them (`HTS`,
  `TBC`) gets tabs whose width, motions and yank follow the 8-column
  grid instead. tmux reads its own stops.
- **Resize.** A pane resized while in copy mode gets a fresh copy at its
  new size, as `refresh-from-pane` would, on the next copy-mode command;
  the cursor keeps its place, clamped to its line. tmux instead reflows
  the old copy (`window_copy_resize`), so output that arrived since entry
  stays out of the view there and appears here.
- **Counts.** A count (`send-keys -N`, the `1` to `9` prompt) runs at
  most 100 000 times in copy mode. tmux runs every repeat and stops
  answering for a huge count (recorded with `-N 4294967295`); outside
  the mode grofe queues the repeats in chunks and stops with `pane input
  queue is full`.
- **The indicator** is `[offset/size]` in `mode-style`, without the line
  time `copy-mode-position-format` puts before it.

## Prompt marks (OSC 133)

As tmux 3.4 and later do, a pane's `OSC 133 ; A` (prompt start) and
`OSC 133 ; C` (output start) flag the line the cursor is on, and
`send-keys -X next-prompt` and `previous-prompt` (`-o` for output starts)
jump to column 0 of the next or previous flagged line, over history and
screen, extending a selection as any move does. Nothing else flags a line:
`B`, `D`, other letters, a missing letter and `OSC 133` inside a
`DCS tmux;` wrapper (which tmux passes to the outer terminal) do nothing.
A count is ignored, and a flag or argument they do not take drops the
command, as in tmux. There are no default keys for them.

tmux keeps the flags on its grid lines. alacritty's grid has no spare line
flag, so `src/server/pane/marks.rs` keeps them beside it, keyed by a
line's distance from the oldest history line plus the number of lines the
history has dropped: a line scrolling into history keeps its place, the
history limit and `ED 3` drop the flags of the lines they drop. The marks
follow tmux's rules, each recorded in `tests/fixtures/compat/golden/tm_1`
(`prompts*`): a whole-line clear resets a line's flags (`EL`, `ED`, `ECH`,
but `EL 0` and `EL 2` leave a line with no cells alone), a partial one
keeps them, and `ED 2` scrolls the used lines into history with theirs. A
resize that changes the width moves them as `grid_reflow` moves a line's
flags: a split line keeps them on its first piece, a line joined into the
one before loses them, and a line only part consumed keeps them on its
remainder.

Where grofe differs:

- **Scroll regions.** A line moved by a scroll region, `IL` or `DL` keeps
  its position, so its flags stay behind where tmux moves them with it.
  Full-screen scrolling, the case for a shell, is exact.
- **The alternate screen.** Marks are made on the normal screen only, and
  a resize that changes the width while the alternate screen is up drops
  them all, as the normal screen's lines cannot be seen to reflow.
- **Synchronized updates.** A mark inside a `CSI ? 2026` block is made
  where the block ends, not where the sequence is.
- **`clear-history`** is not a grofe command; `ED 3` drops the marks of
  the history it drops, as tmux does.

## Selection over wrapped lines

A selection runs over logical lines, not screen rows, as tmux 3.7b's
`window_copy_get_selection` does, checked byte for byte against TM.2's
goldens (`tests/fixtures/compat/golden/tm_2/`). Rows that alacritty marks
`WRAPLINE` (a soft wrap) join into one line with nothing between them; a
hard newline stays a newline. Trailing blanks on each line are trimmed,
whatever their colour, but a soft-wrapped row keeps every cell, so a
blank at the wrap point stays, and so does the empty cell a wide
character too wide for the edge left. A wide character is copied whole
when the selection includes its first cell, combining marks stay with
their base character, and a tab comes back as a tab. A rectangle (`v`,
`C-v`) copies the same columns from each row, one line per row; an edge
through a wide character's second cell drops the character rather than
rounding outward.

Where tmux's details differ from what one might expect, grofe follows
tmux:

- **The final newline.** A selection whose end runs past the end of its
  line keeps a trailing newline: `select-line` always does, a stream
  selection ending at `$` does not. A selection ending part way into a
  soft-wrapped row that is the end of its line's text keeps one too.
- **Resize.** A resize clears the selection, as in tmux
  (`window_copy_size_changed`); selecting the line again after the
  reflow copies the same text.
- **`append-selection`** appends to the newest automatic buffer with no
  separator and stores the result by that buffer's name, which makes it a
  named buffer (so `show-buffer` with no `-b` skips it); with no buffer
  it makes a new automatic one.
- The copy commands' `prefix` argument is accepted and ignored, and a
  copy with no selection copies nothing (tmux copies the search match at
  the cursor, which waits on TM.3).

## Search

`/` and `?` prompt through `command-prompt -T search` and run
`search-forward` or `search-backward`; `n` and `N` repeat it; `#` and `*`
search for `#{copy_cursor_word}`. As in tmux, a pattern with no upper-case
letter matches without regard to case, `wrap-search` decides whether `n`
wraps at the end, and a match may span a soft wrap but never a hard newline.
A new copy mode starts with the pane's last search, upwards, so `n` repeats
it. Matches are drawn in `copy-mode-match-style`, the one under the cursor
in `copy-mode-current-match-style`.

The search runs off the server's actor, on a shared copy of the pane. Until
it ends, the mode's next commands wait in order, and so does the reply to
whichever request sent them, so `send -X search-forward x \; display -p
'#{copy_cursor_x}'` prints where the search landed. The marks are worked
out for the rows on view only, so drawing them costs the pane's size, not
the history's. A pattern bigger than 1 MiB compiled is refused.

tmux 3.7b's own quirks are kept, because the goldens show them
(`tests/fixtures/compat/golden/tm_3/`):

- A pattern with none of `^$*+()?[].\` is searched as plain text even by
  `search-forward`, so `ab|abab` and `x{3}f` look for those characters.
- A line is searched padded to the pane's width with blanks, so `here$`
  matches nothing and `.*` takes the whole width.
- An empty match counts as none, so `x*` finds only lines starting with x.

tmux compiles the pattern with POSIX `regcomp` (extended syntax); the golden
recorder runs on Debian, so the goldens show glibc's dialect in `C.UTF-8`.
grofe reads the pattern as glibc does and runs it with `regex-automata`,
taking the longest match at the leftmost position as POSIX does (`d(e|ee)`
matches all of `dee`). These behave as in tmux, checked by the `tm_3/dialect`,
`tm_3/differs` and `tm_3/unicode` goldens:

- `\w`, `\W`, `\s`, `\S`, `\b`, `\B`, `\<` and `\>` are GNU's classes and
  boundaries. Any other letter or digit after `\` (`\d`, `\n`, `\t`,
  `\pL`, `\x41`) makes the pattern invalid, so it finds nothing. Before
  any other character, `\` makes it literal.
- In a bracket expression, `\` is itself, so `[\d]` matches a backslash or
  a d, and `]` first is itself. A range with an end outside ASCII
  (`[α-γ]`) is invalid, as in glibc.
- The classes follow glibc's tables. Letters of every script and digits of
  other scripts (`١`) are `alpha`, only `0-9` are `digit`, and symbols
  (`∑`) are `punct`. Ignoring case, `upper` and `lower` match every letter.
- Intervals work, and `{,2}` is `{0,2}`. A `(?` and a repetition with
  nothing to repeat are invalid.

These still differ:

1. **Back-references** (`(ab)\1`): glibc accepts them, but neither crate
   can match them, so grofe finds nothing, as an ERE would.
2. **Unicode tables**: grofe maps glibc's classes and `\w` to Unicode
   properties (`alpha` is `\p{Alphabetic}` plus other scripts' digits,
   `punct` is `\p{P}\p{S}`, and so on). They agree on every case the golden
   records, but glibc's own tables can differ for rarer characters.
3. **Longest matches next to non-ASCII text**: a pattern with both an
   alternation and a word boundary falls back to the first alternative's
   match when the text there is not ASCII. The lazy DFA that finds the
   longest match gives up on Unicode word boundaries.

A search over the history is one pass of a prefilter over the whole copy.
That copy is kept as one string of about a byte a cell for the mode's next
search, so the rows with no match cost no search of their own. A
100 000-line search takes a few milliseconds in a release build.

The incremental searches (`search-forward-incremental`, emacs `C-s`) are
out of scope, as is the timestamp `copy-mode-position-format` can show.

## Buffers

Buffers follow tmux 3.7b's goldens (`tests/fixtures/compat/golden/tm_4/`)
where the plan's TM.4 row said otherwise. An automatic buffer is named
`buffer<N>` from a counter that only grows, so `buffer0` is the oldest one
kept, not the newest; lists run newest first. `set-buffer -a` appends only
to a `-b` buffer, and without `-b` makes a new one. Writing to a buffer by
name, or renaming it, makes it a named buffer, which `buffer-limit` never
drops and which commands with no `-b` skip. `paste-buffer` sanitizes
control characters as tmux does (`^X`), unless `-S` is given, so buffer
data cannot end a `-p` bracketed paste early. A copy (TM.2, TM.5) adds an
automatic buffer. A buffer name with a control character is refused, as
in tmux; grofe also refuses the C1 controls (U+0080 to U+009F), which tmux
accepts, so a name `list-buffers` prints cannot start an escape sequence.

tmux bounds no buffer; grofe holds at most 32 MiB in one buffer and
128 MiB in all, which a copy of a whole 100 000-line scrollback fits.
A copy past 32 MiB stops extracting there and fails with `selection too
large`, shown on the status line, rather than storing part of it.

`load-buffer` and `save-buffer` follow tmux 3.7b (recorded,
`golden/tm_7/load-save`): a path is the CLI's, relative to its cwd, and
`-` is the CLI's stdin or stdout; `-b` names the buffer, `save-buffer -a`
appends, and `load-buffer -w` also sends the data to the clipboard of
`-t`'s client (else the current one) as OSC 52, while a `-t` that names
no client is ignored. Empty data makes no buffer. A file `save-buffer`
creates is `0644` under the CLI's umask, as tmux's client creates it, and
an existing file keeps its mode; errors are tmux's (`No such file or
directory: <path>`). grofe's server does the reading and writing for the
CLI, as the same user. Where grofe differs: a `load-buffer -` reads at
most 12 MiB of stdin, the most one request carries; `save-buffer -`
prints bytes that are not UTF-8 replaced; and from a binding `-` reads
nothing, where tmux reads the server's own stdin.

The file is read or written off the server's main loop, so a path that
blocks holds only the request that named it. `load-buffer` from a FIFO
waits for its writer, as tmux's CLI does; `save-buffer` opens without
blocking, so a FIFO with no reader fails at once (`No such device or
address`), as tmux's does. grofe gives up on either after 30 s, which tmux
never does.

## Copy targets

`copy-pipe`, `copy-pipe-no-clear` and `copy-pipe-and-cancel`, and the
`copy-pipe-end-of-line` and `copy-pipe-line` forms (`D` is the first), copy
as their `copy-selection`, `copy-end-of-line` or `copy-line` form does, so
they store a buffer (unless `-P`) and set the clipboard (unless `-C`), and
also run their command, or the `copy-command` option when they have none
or an empty one. `copy-command` is empty by default, which only copies.
yank's `!` and `M-y` rows rely on the shell (a pipe, a `;`), and its `Y`
and `M-y` rows run `tmux paste-buffer -p` from inside the job, which
reaches grofe through the `tmux` name. On Linux the copy reaches the
clipboard tool yank detected (`wl-copy`, `xsel`, `xclip`) and, through
`set-clipboard`, the terminal as OSC 52.

As tmux 3.7b does (recorded with a private server):

- The command runs through `/bin/sh -c` in a task of its own, so a slow
  command never delays a key or a frame, and `-and-cancel` leaves the
  mode at once. It gets the session's environment as `run-shell` does
  (the global environment, the session's, `TMUX`), no `TMUX_PANE`, the
  server's working directory, and the selection on stdin, which is then
  closed. Its stdout and stderr go to `/dev/null`. The selection is never
  part of the command line.
- The command argument is a format, expanded against the pane before the
  copy (`#{selection_present}` still reads 1); `copy-command` is not
  expanded.
- With no selection the command still runs, with nothing on stdin, and no
  buffer is stored.
- Three positionals (`command`, `prefix` and one more) make the command do
  nothing.

**Quote pane formats in a pipe command.** The command is a format and the
result goes to a shell, so a format a pane controls reaches that shell as
it is, in grofe as in tmux. A program in the pane sets `#{pane_title}`
(OSC 2) and can put `;`, `$(...)` or a quote in it; `#{pane_path}` comes
from OSC 7 the same way. Wrap such a format in `#{q:...}`, which escapes
it for the shell, for example
`copy-pipe "cat > /tmp/#{q:pane_title}.txt"`, or leave it out of the
command.

The clipboard follows `set-clipboard`, which is `external` by default, as
in tmux 3.7b:

- Under `on` and `external` every copy (all the copy commands, the pipes
  and `append-selection`) is sent as `ESC ] 52 ; ; <base64> BEL`, with an
  empty selection parameter as tmux sends it, to every client whose
  current window holds the pane. A client of another session, or one on
  another window, gets nothing. Under `off` nothing is sent.
- A pane's own OSC 52 store makes a new buffer and reaches the clients
  showing the pane's window, with the pane's own selection parameter (kept
  to tmux's `cpqs01234567`), only under `on`. Under `external` (the
  default) and `off` it is dropped whole, as tmux's `input_osc_52` does,
  so a program in a pane cannot replace the newest buffer or the
  terminal's clipboard unless you `set -s set-clipboard on`.
- A pane cannot read the clipboard: an OSC 52 query (`?`) is neither
  forwarded nor answered. tmux answers one from the newest buffer under
  its default `get-clipboard buffer`; grofe has no `get-clipboard`.

Where grofe differs:

- **Bounds.** A pane's store is acted on only up to what tmux's default
  `input-buffer-size` (1 MiB of sequence) holds, as tmux does. A copy is
  sent as OSC 52 only up to 8 MiB; tmux sends any size. An OSC 52 that
  does not fit a blocked client's held output (16 MiB) is dropped and
  logged; tmux never drops one. A pipe command
  still running after 60 s has its process group killed, so a hung
  command cannot linger; tmux never kills one. A clipboard tool that
  stays to serve the selection forks first, so it is not touched.
- **No `pane-set-clipboard` hook** runs.
- **The `prefix` argument** is accepted and ignored, as for the copy
  commands.

## Mouse

grofe runs tmux 3.7b's stock mouse rows, checked against TM.6's goldens
(`tests/fixtures/compat/golden/tm_6/`). The `root` rows are grofe's
built-in mouse model, which runs their commands for an unbound key; the
`copy-mode-vi` rows are ordinary bindings. Over a pane in copy mode a
mouse key is looked up in the mode's table first, then `root`, as in
tmux.

- **The wheel** over a pane that is in no mode, did not ask for the mouse
  and is not on the alternate screen is `copy-mode -e`. The first tick only
  enters the mode at the bottom (`[0/N]`); each later tick is the mode's
  `WheelUpPane`, `send -X -N 5 scroll-up`, and scrolling down past the
  bottom leaves. Any other pane is sent the wheel (`send-keys -M`), so
  Neovim scrolls itself.
- **A drag** with the left button over such a pane is `copy-mode -M`: it
  enters the mode and starts a selection where the button went down. In
  copy mode the mode's `MouseDrag1Pane` row does the same through
  `begin-selection`, which starts a drag whenever a mouse key runs it.
  Until the release the pointer moves the selection's end; held on the
  pane's top or bottom row it scrolls a line every 50 ms. The release is
  `MouseDragEnd1Pane`, stock `copy-pipe-and-cancel`, or whatever binds it
  (tmux-yank's copy).
- **A double or triple click** is `copy-mode -H` (the indicator hidden),
  `select-word` (`word-separators`) or `select-line`, then
  `copy-pipe-and-cancel`, both outside the mode and in it. As in tmux, the
  `run-shell -d 0.3` between them holds the copy, and the client's later
  input, while the selection shows: a binding's `run-shell` without `-b`
  holds the commands after it and the client's keys until it finishes
  (recorded, `tm_6/run-shell`).
- **A middle click** over such a pane focuses it and pastes the newest
  buffer (`paste-buffer -p`), tmux's `MouseDown2Pane` row. A pane in copy
  mode, or one that asked for the mouse, gets the click instead.
- **Any `send-keys -X`** a mouse key runs first moves the cursor to the
  pointer, unless the key is the wheel (tmux's `window_copy_move_mouse`).
- **A click in copy mode** is the mode's `MouseDown1Pane`, `select-pane`
  alone: it focuses the pane and leaves the cursor and the view where they
  were (recorded). The plan's "a click in copy mode moves the cursor" is not
  what tmux does.

This replaces the read-only scrollback view from T3.2 and the shift-drag
fallback.

Where grofe differs:

- **A drag over a pane whose search is still running** does not move its
  selection until the search ends; tmux has no searches in flight.

## Bindings wait

As tmux 3.7b's per-client command queue does (recorded,
`tests/fixtures/compat/golden/tm_6/run-shell` and `waits`), a key, mouse
or prompt binding waits for its commands that finish later, and holds the
rest of its commands and all of the client's later input until they do:

- `run-shell` without `-b` (with `-d` too), until its shell exits;
- `command-prompt`, `confirm-before`, `display-menu`, `display-panes` and
  `display-popup` without `-b`, until they close. A prompt's or a
  confirm's command runs before the binding's rest, a menu item's after it;
- an `after-*` hook of one of its commands, while the hook's own
  `run-shell` runs.

The prompt, menu or popup that is open still gets its keys. Everything
else waits, including the prefix and `detach-client`'s key, so a
`run-shell` that never exits freezes that client's input, as it does in
tmux. The ways out:

- detach the client from elsewhere: `grofe detach-client -t <client>` from
  the CLI or another client (`list-clients` names it);
- kill the shell (`pkill -f '<its command>'`), after which the held keys
  run in order;
- from another client, `kill-server` as a last resort.

## The tmux front end

Run as `tmux`, grofe reads tmux's copy-mode and buffer forms through the
rows in `src/compat/tmux/copy.rs` (TM.7): `copy-mode` with `-u`, `-e`, `-H`
and `-M`, `send-keys -X` with `-F` and `-N`, `command-prompt -T`, and
`set-buffer`, `show-buffer`, `list-buffers`, `delete-buffer`,
`paste-buffer`, `choose-buffer`, `load-buffer` and `save-buffer`, each
passed to the native command of the same name. `copy-mode -d` (a page
down) and `-q` (leave every mode) are refused: tmux's stock rows use them
only from the pane scrollbar and the pane menu, which grofe does not
have. So are `-s` and `-S`. The copy options keep their tmux names (`mode-keys`,
`set-clipboard`, `copy-command`, `word-separators`, `wrap-search`,
`buffer-limit`). A `copy-pipe*` string bound with `bind -T copy-mode-vi`
keeps tmux's dialect and runs through the `send-keys -X` row, so
tmux-yank's rows bind and run unchanged. Under the tmux name, `list-keys
-T copy-mode-vi` prints the stock rows as tmux does, prompt templates as
`{ }` lists and mouse keys in tmux's order.

The parity suite (`tests/compat/scenarios/copy_mode/`) holds one scenario
per keymap row above, in `golden/tm_7/copy_mode/`, plus the front end's
own forms, buffers, options, `#{pane_path}` and `#{bracket_paste_flag}`
in `golden/tm_7/`. Each starts from TM.1's fixture with `copy-mode.conf`
sourced, presses the row's key (with `send-keys`, or typed on an attached
client for the prompts and the mouse) and reads back the cursor or the
buffer. The goldens were recorded from tmux 3.7b in TC.0b's container,
and grofe, reached through the `tmux` symlink, must match each byte for
byte, with nothing in `compat-miss.log`. On the grofe side `pbcopy` and
the other clipboard tools are stubs, so the suite never reaches the
machine's clipboard; the container has none of them, which the
scenarios do not depend on.

## Out of scope

- **The emacs table** (`copy-mode`). `mode-keys` stays `vi`. yank also binds
  its keys in `copy-mode`; those rows land in a table grofe never enters.
- **`copy-mode -M` and `-H` beyond the stock mouse rows' meaning**, and
  `-d`, `-q`, `-s`, `-S`,
  the scrollbar rows of the stock `root` table (grofe draws no pane
  scrollbar) and view mode.
- **`-X` commands no row or plan task names**, such as the incremental
  search commands and `scroll-to-mouse`. TM.2 also routes `copy-line`,
  `rectangle-on`, `rectangle-off` and `stop-selection` as tmux does.

## Decisions for the owner

1. **95 rows, not 93.** The table includes vim-tmux-navigator's five rows,
   which canopy's port defers to M5, taken from the container recording
   because the plugin is not vendored. Without them the table would be 92
   and `C-h`, `C-j` would keep tmux's stock meaning in copy mode.
2. **`pbcopy` in the fixture.** The golden and TM.7's scenarios use the
   macOS copy command. A Linux machine binds `wl-copy` instead.
3. **`copy-mode -M` and `-H`.** The plan puts `copy-mode -M` out of scope,
   but tmux 3.7b's stock `root` drag row is `copy-mode -M`, and its double
   and triple click rows use `copy-mode -H`. The proposal is that TM.6
   accepts both with only those meanings (start a selection at the mouse;
   hide the position indicator), so the stock rows run unchanged.
4. **The regex dialect.** Settled by the M5 addendum: grofe reads patterns
   as glibc does where that is cheap (`\d` finds nothing, as in tmux), and
   keeps the differences the Search section lists.
5. **Default bindings.** The stub edit is to load "the 93 rows from TM.0's
   golden" as grofe's default table. The proposal is that grofe's default
   table holds only the 88 stock rows; yank's five come from the plugin
   (TP.11) and the navigator's five from canopy's port, so a plain grofe
   user never gets `pbcopy` bound to `y`.
