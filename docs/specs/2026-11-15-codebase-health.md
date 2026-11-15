# Codebase health: findings and workstreams

Date: 2026-11-15. Source: a read-only review of main at 1d1e1c7 (server core; commands, config and
compat; features, docs and tests), with the main claims spot-checked by hand. Main has since gained
the agents work (#13) and is about to gain Luau and `grofe pack` (#11). Line numbers below are from
that review and will drift; the names are what to search for.

Each workstream (WS) is one branch, one PR and one implementer. A WS changes behaviour only where
its section says so; everything else is a pure refactor that keeps every test green.

## Rules for every workstream

- Test first for any behaviour change. A refactor proves itself by the existing suite.
- Checks before the PR: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the full
  `cargo test` run isolated from the live server, and `bats tests/cli.bats`.
- Isolated means: `GROFE`, `GROFE_PANE`, `TMUX` and `TMUX_PANE` unset, a short fresh `TMPDIR` and
  `XDG_RUNTIME_DIR` under `/tmp` (socket paths have a 104-byte limit), and grofe stripped from `PATH`.
- Never run `tmux` without `-L`/`-S`, and start it with `-f /dev/null`.
- No em dash as a sentence break. Conventional commits. No AI attribution.
- A deliberate simplification gets a `// ponytail:` comment naming its ceiling.

## WS1 Publishability and docs hygiene

**Why.** `docs/compat-coverage.md` names the owner's private repos (canopy, athome), the chezmoi
setup and a `~/.claude/hooks` path. That breaks the rule "no hostnames, emails or machine specifics in
shipped files". Several docs are finished one-off notes that read like current docs.

**Change.**
- Split `docs/compat-coverage.md`: the coverage tables stay, with the owner-corpus prose rewritten to
  say "a private corpus of real configs" without names or paths. The corpus how-to moves to
  `docs/archive/owner-corpus.md`, also scrubbed of names and paths.
- Move to `docs/archive/`: `spike-notes.md`, `graphics-notes.md`, `dogfood.md`, `mac-trial.md`,
  `alias-day.md`, and plans whose tasks are all merged (keep `docs/plans/` for live plans only). Fix
  every link that pointed at a moved file.
- README: the "Status"/roadmap link points at a short `docs/roadmap.md` (written from this spec's
  open workstreams), not at the milestone task plan. The statements "Plugins are ordinary programs",
  "no WebAssembly plugins" and "does not build its own agent dashboard" are brought in line with
  main (Luau, `grofe pack`, native agents) once #11 has merged.
- Add the missing user docs: `docs/config-reference.md` listing every TOML key with type, default
  and the tmux option it maps to (generated from the options table by a test that fails when the doc
  is stale, the same way `tests/docs_lua.rs` guards `docs/lua.md`), and a section for
  `reboot-check` and `show-config --explain`.

**Done when.** `grep -rniE "canopy|athome|chezmoi|\.claude|/Users/" docs README.md` finds nothing
outside `docs/archive/` (and nothing personal inside it); no link in `docs/` or README is broken (a
test checks relative links); the config reference test passes.

## WS2 Test reliability

**Why.** 261 sleeps or fixed millisecond waits across 55 test files. The CI failures on PR #11
(`stale_build`, `model::drop_stashed_on_exit`, two Luau timing tests) were all this kind.

**Change.**
- One helper in `tests/common`: `wait_until(what: &str, timeout: Duration, f: impl FnMut() -> bool)`
  that polls every 10 ms and panics with `what` and the elapsed time on timeout. Default timeout 10 s,
  so a slow runner waits longer instead of failing.
- Replace fixed sleeps that wait for a state with `wait_until` on that state, starting with
  `e2e_alias_day.rs`, `frames_mouse.rs`, `acceptance/tc_10.rs`, `acceptance/float_mouse.rs`,
  `resume.rs`, `client.rs`, `acceptance/t2_5a.rs`, then the rest. A sleep that is the thing under
  test (a debounce, a timer) stays, with a comment saying so.
- Timing assertions (`elapsed() < N ms`) get bounds that hold on a loaded runner, or measure a ratio
  against a baseline instead of an absolute time.
- Fix `stale_build::attaching_to_a_server_whose_binary_was_replaced_warns_about_the_old_build` and
  `server::model::tests::drop_stashed_on_exit` at their root cause.

**Done when.** Fixed sleeps outside the documented exceptions are gone (a grep in the PR body shows
the count before and after); the full suite passes 3 runs in a row locally, including one with
`--test-threads` at twice the core count.

## WS3 Shared helpers

**Why.** The same small function is implemented several times:
- strftime and `pretty_time`: `clients.rs`, `status.rs`, `clock.rs`, `cmd/format/modify.rs`;
- glob matching: `clients.rs`, `events.rs`, `cmd/format.rs` (`fnmatch`, the only one with an
  anti-backtracking test), `import_tmux.rs`;
- `/bin/sh -c` spawn (arg0 `sh`, env handling, `SPAWN_LOCK`): `sources.rs`, `copy/pipe.rs`,
  `prompt.rs`, `pane/pipe.rs`;
- shell quoting: `dispatch/hooks.rs` `shell_escape`, `dispatch/options.rs` `quote`,
  `layouts::shell_quote`, `import_tmux.rs` and `config/cli.rs` (`quote`, `bare_or_quoted`,
  `literal`, `binding_value`);
- `char_width`: `menu.rs`, `status.rs`, `cmd/format/modify.rs`;
- an empty `NoVars: FormatVars`: 8 definitions.

**Change.** One home each: `cmd::format::time` (strftime, `pretty_time`), `cmd::format::fnmatch`
(glob), `server::spawn::sh` (shell spawn), `util::quote` (shell and tmux quoting, one function per
dialect), `cmd::format::char_width`, `cmd::format::NoVars`. Every copy becomes a call. Where copies
differ, the PR names the difference and keeps the behaviour the tests pin.

**Done when.** Each helper has exactly one definition (grep in the PR body), the glob
anti-backtracking test covers every former caller's patterns, the suite is green.

## WS4 Dead code and stale comments

**Why.** Code and comments left from the milestone build:
- `compat::tmux::structure::rewrites()` and `bindings::rewrites()` return empty lists;
- compat `Row.kind`, `Row.flags` and `golden_id` are read only by tests; `FlagMark::Cosmetic` has
  one use; `GrofeTarget::Cli` and `Grammar` rows end in "not implemented";
- the actor matches and ignores `PaneEventKind::Sixel`;
- milestone comments over finished code ("M3.6: stubs until each owning task lands" in
  `dispatch/mod.rs`, "only the options M1 to M3 need" in `options.rs`, "M2 T2.3's job" in
  `cmd/keys.rs`, "used from M4", "not config in M3.5" in `import_tmux.rs`, and hooks "raised by
  nobody yet");
- shipped code includes test fixtures: `include_str!("../../../tests/fixtures/...")` in
  `compat/tmux`.

**Change.** Delete what nothing uses; move test-only fields under `#[cfg(test)]` or into the tests;
reword or drop stale comments so they describe today's code; move the included fixtures to
`src/compat/tmux/data/` (shipped data) and point the tests there. For each hook named in a comment as
"raised by nobody", either confirm it fires (and fix the comment) or list it in
`docs/compat-coverage.md` as accepted but not fired.

**Done when.** `rg "M[0-9]+(\.[0-9]+)?[: ]" src` shows only references that are still true;
`rg "tests/fixtures" src` is empty; clippy with `-W dead_code` adds nothing new; suite green.

## WS5 Safety and blocking on the actor

**Why.**
- `graphics.rs` maps shared memory named by a pane's program; the size and offset must be checked
  against the file length before the slice is built.
- `snapshot.rs` transmutes the previous signal handler from `sigaction`.
- `restore.rs` calls `std::env::remove_var` while other threads run (unsound on Linux since 1.80's
  `unsafe` marking).
- `layout.rs` has `children.pop().unwrap()` on a production path.
- The actor blocks on: `adopt::from_live_tmux` running `tmux` with no timeout at startup; graphics
  file and shm reads; procinfo reads during format expansion for `list-panes`.

**Change.**
- shm/file graphics: reject when `offset + size` exceeds the mapped length or a cap (the existing
  image size cap); test with a short file and an overflowing offset.
- Signal handler: store and call the previous handler through the typed `sa_sigaction` union with
  the `SA_SIGINFO` flag checked, no `transmute`; test that a chained handler still runs.
- Environment: remove the variable before any thread starts (or pass a cleaned env to children
  instead of mutating the process env); a comment states which.
- `layout.rs`: replace the `unwrap` with a pattern that cannot fail (`if let [only] = ...`).
- `adopt`: run `tmux` with a 2 s timeout and kill on expiry; a test with a fake `tmux` that sleeps.
- Graphics file reads move off the actor through the existing job pattern (read on a blocking task,
  apply as a `Job`), or are capped in size with a `ponytail:` note if moving them changes ordering
  the tests pin.

**Done when.** Each item has a regression test, `rg "transmute|remove_var" src` shows none of the
old uses, suite green on macOS and Linux (CI).

## WS6 Fuzzing the parsers

**Why.** `cmd/parse.rs`, the sixel parser and the passthrough/APC scanner read bytes a pane's
program controls. None is fuzzed.

**Change.** A `fuzz/` crate (cargo-fuzz, `libfuzzer-sys`) with targets `parse_command` (the
tokenizer plus `parse`), `sixel` and `passthrough`. Each target asserts no panic and, where cheap, a
round-trip property (parse then display parses to the same command). A CI job runs each target for
60 s on Linux. Crashes found get a regression test in the normal suite and a fix.

**Done when.** `cargo +nightly fuzz run <target> -- -max_total_time=60` runs clean for all three;
the CI job exists; any crash found has its test.

## WS7 tmux parity: keys, targets, format variables, inert options

**Why.**
- `send-keys` sends `C-Enter`, `S-a`, `C-1` and `F13` and above as literal text because the key-name
  table in `cmd/keys.rs` does not know them. Two encoders exist (`cmd/keys.rs` `classic_key_bytes`
  and `server::input::{parse_key, encode}`).
- Target forms `{last}`, `{next}`, `!` and `+N` are refused as "unsupported target form".
- About 80 of tmux's 195 format variables are missing (`scroll_region_*`, `alternate_saved_*`,
  `cursor_shape`, `cursor_colour` and others).
- About 30 options are stored with no effect (`NO_EFFECT`); only `show-config --explain` says so.

**Change.**
- One key encoder: `send-keys` uses `server::input::{parse_key, encode}` for every name;
  `classic_key_bytes` goes. Before changing, record what real tmux sends for each of the names above
  (private `-L` server, `-f /dev/null`, a pane running `cat -v` or `od`) and pin those bytes in tests.
- Target forms: implement `{last}`, `{next}`, `{previous}`, `!`, `+N` and `-N` for windows and panes
  as tmux resolves them; errors use tmux's wording (`can't find window: ...`).
- Format variables: list the missing ones (a test compares grofe's `display-message -a` names with
  the pinned tmux list and fails on any new gap); implement the ones whose data grofe already has; the
  rest go in `docs/compat-coverage.md` as not provided, with the reason.
- Inert options: `show-options` in the tmux dialect stays tmux-silent, but `set-option` on an inert
  option logs once per server to `show-messages` that it has no effect; `docs/compat-coverage.md`
  lists them.

**Done when.** The key bytes match the pinned tmux output; the new target forms resolve as tmux's do
in tests; the format-variable gap test passes with the documented exceptions; suite green.

## WS8 Split the largest files

**Why.** `dispatch/options.rs` (6.5k lines) is five modules; `clients.rs` (4.9k) mixes lifecycle,
geometry, frame composition and input routing; `input.rs` (5.4k) holds a 1.5k-line default bindings
table; `layout.rs` (5.0k) mixes presets, layout strings and tree edits.

**Change.** Move code, change no behaviour:
- `dispatch/options.rs` into `dispatch/options/{table, store, reload, bindings, source_file,
  formats}.rs` along the seams: the option table and store, config load and reload, key bindings and
  `list-keys`, `source-file` and glob, the format-variable providers (`FormatCtx`, `ServerVars`) with
  `list-*` sorting.
- `clients.rs` into `clients/{lifecycle, geometry, compose, input}.rs`; wait-for channels move to
  `dispatch/client.rs`.
- `input.rs`: the `default_bindings` data moves to `input/defaults.rs`; decoder, key tables and
  encoder each get a file.
- `layout.rs` into `layout/{presets, string, tree}.rs`.
- Test modules move with the code they test.

**Done when.** No file in `src/server` is over 3,000 lines apart from test blocks; `git diff -M
--stat` shows moves; no test changed except `use` paths; suite green.

## WS9 One source of truth for options

**Why.** Option names live in `compat/tmux` `OPTION_NAMES` (about 175 identity rows) and
`config/mod.rs` `OPTION_NAMES` (TOML key to tmux name, about 25 real renames); TOML types live in a
hand list (`toml_type`) parallel to `Kind` in the options table; defaults come from `BUILTINS`,
`TMUX_DEFAULTS`, `tmux-options.txt`, `SPELLED_DEFAULTS` and the command alias table. Nothing checks
that `toml_type` agrees with `Kind`.

**Change.** The options table (`BUILTINS`) gains the TOML key (when it differs) and is the only list
of names, kinds and defaults. `config` derives its name map and types from it; the compat identity
table goes; tmux-spelled defaults are a column, not a second table. Where a second source must stay
(the pinned tmux text used for parity tests), a test checks it against the table.

**Done when.** One definition per option; a test fails if a TOML type disagrees with the option's
kind; `show-config --explain` output is byte-identical before and after (pinned in a test first);
suite green.

## WS10 Table-driven command parser

**Why.** `cmd/parse.rs` (6.3k lines) has about 110 hand-written `parse_*` functions, about 500 inline
flag tuples and a 250-line verb `match`. Flag letters and arity also live in tmux's `list-commands`
text (used by compat) and the compat rows. Verb aliases are listed in `canonical_name`, the tmux
alias table and again in `main.rs` (`attach`, `start`, `new`).

**Change.** A command table: one row per verb with its alias, flag spec (letters, which take
values), positional bounds and a builder function that maps parsed flags to the `Command` variant.
The generic part (tokenize, flag parse, arity check, tmux-worded errors) runs once from the table.
Verbs with real parsing logic (`send-keys`, `split-window`, `resize-pane`, `new-session` and the like)
keep a hand-written builder. `canonical_name` and the `main.rs` aliases read the table. A test checks
every row's flags against tmux's `list-commands` usage text. Migrate verb by verb, each commit green.

**Done when.** No verb is parsed outside the table path; `parse.rs` plus the table is at least 40%
smaller than today; the error text for each verb's bad-flag and missing-argument cases is unchanged
(pinned before the migration); suite green.

## WS11 Explicit per-command context

**Why.** `State` carries per-request fields set before a command and taken after it:
`deferred_reply`, `held`, `failed_out`, `cli_io`, `command_log`, `event_key`, `command_key`. Similar
state lives in thread-locals (`source_depth`, `source_verbose`, `TYPING`) and statics (`TIMER`,
`FROM_RESUME`, `WRITER`). Their correctness depends on call order.

**Change.** A `CommandCtx` (or extend the existing `ClientCtx`) owns the per-request fields and is
passed to dispatch; the thread-locals that track a request become fields on it; statics that are
really server state become `State` fields. Each moved field is one commit, suite green after each.

**Done when.** None of the listed fields remain on `State` or in thread-locals/statics, or the PR
says for each kept one why it must stay; suite green.

## WS12 `-f` takes a tmux.conf in the tmux dialect

**Why.** `tmux -f file` (through the `tmux` shim) loads grofe TOML, so scripts and tools that pass a
tmux.conf break. That is the most common remaining drop-in failure after control mode.

**Change.** In the tmux dialect, `-f` sources the file as a tmux config (the live `source-file`
path, with `%if`, `%hidden` and `{}` blocks) on top of the grofe defaults; the native `grofe -f` keeps
taking TOML. `-f /dev/null` means no config. Docs say which dialect does what.

**Done when.** `tmux -f tests/fixtures/... new-session -d` through the shim applies a tmux.conf's
options and bindings (test); `grofe -f x.toml` unchanged (test); README drop-in section updated.

## WS13 Copy-mode hints (quick select)

**Why.** wezterm's quick select and kitty's hints let a user copy a URL, path, hash or IP from the
screen with two keystrokes. grofe's copy mode has only tmux's motions.

**Change.** A `copy-mode -H` entry (bound to `prefix F` by default in grofe defaults, unbound in
stock tmux defaults): visible text is scanned with a configurable list of patterns (URLs, paths, git
hashes, IPv4/6, numbers of 4+ digits), each match gets a one- or two-letter label drawn over it, and
typing a label copies the match to a buffer (and the clipboard when `set-clipboard` allows);
uppercase label pastes it into the pane. Patterns are a list option `copy-hint-patterns`.

**Done when.** Tests cover label assignment (stable, shortest labels for nearest matches), copy and
paste actions, a pattern option override and a screen with no matches; documented in the copy-mode
docs and the config reference.

## WS14 Control mode (`-C` and `-CC`)

**Why.** iTerm2's tmux integration and several editor plugins drive tmux through control mode. Its
absence is the largest drop-in gap left.

**Change.** Its own spec and plan (`docs/specs/2026-11-15-control-mode.md`), written first by the
WS14 implementer from tmux's control-mode documentation and tested against real tmux on a private
socket: the line protocol (`%begin`/`%end`/`%error` blocks, `%output`, `%window-add`,
`%layout-change`, `%session-changed` and the rest of the notifications tmux 3.x sends), `-CC`'s DCS
wrapping, flow control (`refresh-client -f pause-after`, `%pause`/`%continue`), and
`refresh-client -C` sizing. Scope of the first PR: everything a plain `tmux -C attach` client sees
and sends, parity-tested line by line against real tmux; iTerm2 end-to-end checked by hand and noted.

**Done when.** The spec exists and is followed; a parity test runs the same command script against
real tmux and grofe in control mode and compares the transcripts with ids normalised; docs updated.

## Out of scope

- Splitting or growing `import_tmux.rs`. It reuses the shared helpers after WS3; its own tokenizer
  stays until WS10 lands, then a follow-up may move it onto the table.
- The owner-corpus scripts and containers stay in the repo but out of CI (WS1 documents them in the
  archive).
- Swap or stacked layouts, layout hot-reload and a command palette: not findings of this review.
