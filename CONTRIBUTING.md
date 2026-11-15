# Contributing to grofe

Issues, ideas and pull requests are welcome. The
[design document](docs/specs/2026-09-23-grofe-design.md) is the best place to
start; if something in it looks wrong, that is useful feedback too.

## Build

grofe needs Rust 1.88 or newer ([rustup](https://rustup.rs)) and builds with
cargo. [just](https://github.com/casey/just) runs the day-to-day tasks
(`just` lists them).

```sh
cargo build
just install    # a release build in ~/.cargo/bin
```

## The gate

Every change must pass `just check`, which runs:

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `bats tests/cli.bats`

CI runs the same on Linux and macOS, plus the README's tested steps and a
build of the docs site.

## Tests

- Write the test first, then the code that makes it pass.
- Integration tests start their own server on a private `-S` socket through
  the harness in `tests/common`, so they never touch a grofe or tmux you have
  running.
- **Never run `tmux` against the default server.** If you are working inside
  tmux, `tmux kill-session` with no target kills your own session. To compare
  with real tmux, always use a private socket and no config:
  `tmux -L grofe-<task> -f /dev/null ...`, and clean up with
  `tmux -L grofe-<task> kill-server`.
- The tmux compatibility suite (`tests/compat`) compares against recorded tmux
  output, so it mostly needs no tmux installed.

## Commits and pull requests

- Conventional commits: `type(scope): description`, such as
  `fix(layout): keep the zoomed pane on resize`.
- Never use an em dash as a sentence break, in prose, comments or commits.
- To bring a branch up to date, merge `origin/main` into it rather than
  rebasing, so review threads keep their context.
- No hostnames, emails or machine specifics in shipped files.
- Keep it simple: no speculative abstractions.

## Docs ship with the feature

A change a user can see is not done until its docs are, in the same pull
request:

- The book (`docs/book/src/`): update the page for that area, or add one and
  list it in `SUMMARY.md`.
- The README, when the change alters what grofe is or how it is installed.
- New or changed options: write the option's `doc` line and regenerate the
  options reference with `just docs`; a test fails if either is missing.
- Changed visuals: `just screenshots`.
- The landing page (`site/index.html`), when a headline feature is added.

## Docs

- The guide is an [mdBook](https://rust-lang.github.io/mdBook/) in
  `docs/book/src/`; `just docs-serve` previews it with live reload.
- The options reference (`docs/book/src/reference/options.md`) is generated
  from the option table in `src/server/dispatch/options/table.rs` (lists in `lists.rs`). Each option has
  a `doc` line; after changing the table, run `just docs` to regenerate the
  file. A test fails while it is out of date.
- The screenshots come from `docs/tapes/` and are rendered with
  [vhs](https://github.com/charmbracelet/vhs) by `just screenshots`.
- `just site` assembles the whole site in `_site/` as the Pages workflow does,
  and `just site-serve` serves it on http://localhost:8000.
- `docs/specs` and `docs/plans` hold the design and the plans behind it; they
  are not published.

## Reporting bugs

Open an issue with `grofe -V`, your OS and terminal, the steps to reproduce,
and whether it also happens under the `tmux` name. Security problems go
through [SECURITY.md](SECURITY.md) instead.
