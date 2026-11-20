# Performance and memory

grofe is meant to cost about what tmux costs. Most of the memory a
multiplexer uses is scrollback, so that is where grofe spends its care.

## What the server keeps per pane

- The visible screen, as full cells.
- The scrollback, in a compact store. A row that scrolls off the top is
  moved out of the terminal grid into a store that keeps only the cells up to
  the row's used width, as UTF-8 text plus runs of repeated styles. A row of
  115 characters in one style takes a few hundred bytes, where a full-width
  grid row takes several kilobytes. Capture, copy mode and the history
  formats read the store and the screen as one history, and give the same
  output as before.
- At most `history-limit` lines of scrollback, which defaults to 2000, the
  same as tmux. Set it higher when you want more; the compact store keeps
  that cheap. `history-limit` applies to panes created after it is set.

## Measured

Physical footprint of the server (macOS `vmmap --summary`, or `RssAnon` on
Linux) with 80x24 panes and a 46 MB file of coloured lines written with
`cat`:

| case                                 | before | now     |
| ------------------------------------ | ------ | ------- |
| 21 idle panes                        | 7.6 MB | 7.6 MB  |
| after 46 MB, default history-limit   | 106 MB | 7 MB    |
| after 46 MB, history-limit 50000     | 106 MB | 12 MB   |

"Before" is v0.4.0, whose default history-limit was 50000 lines. Resident
size, which counts shared and cached pages as well, was higher still: 277 MB
for the same 46 MB at the old default.

Idle panes were already cheap, so they did not change. The release build also
uses thin LTO, one codegen unit and stripped symbols, which shrinks the
binary.

## Checking it yourself

Build a release binary and run the script:

```sh
cargo build --release
scripts/footprint.sh                 # target/release/grofe
scripts/footprint.sh path/to/grofe   # any other build
```

It starts a server per case on a private socket, in a fresh temporary home,
so it never touches a running server or restores saved sessions, and prints
the table above for your machine.

CI runs the same cases as budgets: `cargo test --release --test footprint --
--ignored` fails when 21 idle panes use more than 10 MB, when the 46 MB run
at the default limit uses more than 20 MB, or when the same run at
history-limit 50000 uses more than 60 MB.
