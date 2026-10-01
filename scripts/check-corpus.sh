#!/usr/bin/env bash
# check-corpus.sh: the tmux argv corpus is well formed, publishable and fully
# mapped (plan M3.6, task TC.0). Exits nonzero when:
#
#   - a corpus file is missing, empty, unsorted, has duplicate, blank or
#     unparseable lines, or a line with no source in sources.tsv;
#   - a corpus file, sources.tsv or docs/compat-coverage.md holds this
#     machine's home path, hostname or git email (a denylist built now), any
#     email address, or an absolute home path;
#   - a distinct form maps to no spec 4.5 row (scripts/compat-rows.tsv), or a
#     row there names none;
#   - no argv came from a hook or run-shell job (container-job.argv);
#   - the generated part of docs/compat-coverage.md is stale.
set -euo pipefail
exec uv run "$(dirname "$0")/corpus.py" check
