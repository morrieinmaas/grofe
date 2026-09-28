#!/usr/bin/env bash
# normalize-corpus.sh: turn a tmux-argv-recorder.sh log into corpus lines
# (plan M3.6, task TC.0).
#
#   scripts/normalize-corpus.sh <argv.log> <home> [<list-keys dump> [<show-hooks dump>]]
#
# <home> is HOME where the log was recorded. Paths under it become ~, and
# hostnames, emails, sockets, session, window and client names, directories
# and user option values become <placeholders>; the lines are deduplicated
# into tests/compat/corpus/container-{cli,pane,job,stored}.argv, with their
# origins in tests/compat/corpus/sources.tsv. The rules live in
# scripts/corpus.py (normalize_argv), shared with the static extraction.
set -euo pipefail
if [ "$#" -lt 2 ]; then
  echo "usage: $0 <argv.log> <home> [<list-keys dump> [<show-hooks dump>]]" >&2
  exit 2
fi
args=(normalize --log "$1" --home "$2")
[ "$#" -ge 3 ] && args+=(--list-keys "$3")
[ "$#" -ge 4 ] && args+=(--hooks "$4")
exec uv run "$(dirname "$0")/corpus.py" "${args[@]}"
