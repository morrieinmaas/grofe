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
command); the M5 task that builds it; the parity scenario TM.7 records under
`tests/compat/scenarios/`. TM.1 builds motions, scrolling, cancel and counts;
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

## Selection over wrapped lines

A selection runs over logical lines, not screen rows. Rows that alacritty
marks `WRAPLINE` (a soft wrap) join into one line with nothing between them;
a hard newline stays a newline. Trailing blanks on each line are trimmed. A
wide character is copied whole when the selection touches either of its
cells, and combining marks stay with their base character. A rectangle
(`v`, `C-v`) is the exception: it copies the same columns from each screen
row, one line per row. A selection is stored as positions in logical lines,
so it survives a resize that reflows them. Where tmux's own result differs
in a detail (a trailing newline after a line selection, a wide character cut
by a rectangle edge), TM.2's goldens decide and grofe follows tmux.

## Search

`/` and `?` prompt through `command-prompt -T search` and run
`search-forward` or `search-backward`; `n` and `N` repeat it; `#` and `*`
search for `#{copy_cursor_word}`. As in tmux, a pattern with no upper-case
letter matches without regard to case, `wrap-search` decides whether `n`
wraps at the end, and a match may span a soft wrap but never a hard newline.
The search runs on a copy of the grid, off the server's actor.

tmux compiles the pattern with POSIX `regcomp` (extended syntax); the golden
recorder runs on Debian, so the goldens show glibc's dialect. grofe uses the
`regex` crate. Where they differ, grofe behaves as follows. TM.3 records
each case from tmux 3.7b and tests grofe's side; a case that turns out not
to differ is dropped from this list.

1. **Back-references** (`\(a\)\1`, `(a)\1`): glibc accepts them, the crate
   does not. grofe treats the pattern as invalid and finds nothing.
2. **Alternation**: POSIX takes the longest match at a position, the crate
   the first alternative that matches. `a|ab` on `ab` matches `ab` in tmux
   and `a` in grofe. The cursor lands in the same place; the highlighted
   match and `search-forward`'s end position can differ.
3. **Perl classes**: `\d`, `\D` and `\pL` are digit, non-digit and letter in
   grofe. glibc reads `\d` as a plain `d`.
4. **Unicode**: `\w`, `\s`, `\b` and `.` follow Unicode in grofe. glibc
   follows the locale (`C.UTF-8` in the recorder), which agrees for ASCII
   and can differ for other scripts.

`\<` and `\>`, bracket expressions and `[[:alpha:]]` classes behave the
same. The `-text` forms take the pattern as plain text in both.

## Copy targets

`copy-pipe`, `copy-pipe-and-cancel` and `copy-pipe-no-clear` store a buffer
and run their command, or the `copy-command` option when they have none
(empty by default, which only stores the buffer). The command runs through
`/bin/sh -c` off the actor, with the server's environment and the selection
on stdin, so a slow command never delays a key or a frame. yank's `!` and
`M-y` rows rely on the shell (a pipe, a `;`), and its `Y` and `M-y` rows run
`tmux paste-buffer -p` from inside the job, which reaches grofe through the
`tmux` name. With `set-clipboard` `on` or `external` every copy also goes to
the attached clients as OSC 52; a pane's own OSC 52 makes a buffer only
under `on`.

## Mouse

The wheel in a pane that has not asked for the mouse enters copy mode
(`copy-mode -e`, so scrolling back to the bottom leaves it) and scrolls five
lines at a time. A pane in mouse mode, or showing the alternate screen, is
passed the event instead (`send-keys -M`), as tmux's stock `root` row says. A drag enters copy mode and selects; releasing runs
`MouseDragEnd1Pane`, yank's copy. A double click selects a word
(`word-separators`) and a triple click a line, and both copy after 0.3 s, as
tmux's stock rows do. A click in copy mode moves the cursor. This replaces
the read-only scrollback view from T3.2 and the shift-drag fallback.

## Out of scope

- **The emacs table** (`copy-mode`). `mode-keys` stays `vi`. yank also binds
  its keys in `copy-mode`; those rows land in a table grofe never enters.
- **`copy-mode -M` beyond the root drag row**, and `-d`, `-q`, `-s`, `-S`,
  the scrollbar rows of the stock `root` table (grofe draws no pane
  scrollbar) and view mode.
- **`-X` commands no row or plan task names**, such as the incremental
  search commands, `scroll-to-mouse` and `copy-line`.

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
4. **The regex dialect.** grofe finds nothing for a back-reference, takes
   the first alternative rather than the longest, and reads `\d` as a digit.
   The proposal is to keep these differences rather than write a POSIX
   matcher.
5. **Default bindings.** The stub edit is to load "the 93 rows from TM.0's
   golden" as grofe's default table. The proposal is that grofe's default
   table holds only the 88 stock rows; yank's five come from the plugin
   (TP.11) and the navigator's five from canopy's port, so a plain grofe
   user never gets `pbcopy` bound to `y`.
