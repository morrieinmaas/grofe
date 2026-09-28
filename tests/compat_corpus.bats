#!/usr/bin/env bats
# The tmux argv corpus (plan M3.6, task TC.0): the recorder, the normalizer
# and the check. Each test works on a copy of the corpus tooling in a temp
# directory, so the committed files are never touched. Needs uv and bash;
# never runs a real tmux (the recorder is pointed at a fake one). Recorder
# calls run with TMUX unset: under a live tmux the recorder would rightly
# classify them as `pane`.

REPO="$(cd "$(dirname "$BATS_TEST_FILENAME")/.." && pwd)"

# This file is itself scanned by the publishability check, so the planted
# probes are assembled at run time rather than written out whole.
AT=@
HOMEDIR=/home
PROBE_EMAIL="tc0-probe${AT}example.invalid"

setup() {
    TMP="$(mktemp -d "${TMPDIR:-/tmp}/grofe-corpus.XXXXXX")"
    W="$TMP/repo"
    mkdir -p "$W/docs/specs" "$W/tests/compat"
    cp -R "$REPO/scripts" "$W/scripts"
    cp -R "$REPO/tests/compat/corpus" "$W/tests/compat/corpus"
    cp "$REPO/docs/compat-coverage.md" "$W/docs/"
    cp "$REPO/docs/specs/2026-09-23-grofe-design.md" "$W/docs/specs/"
    C="$W/tests/compat/corpus"
}

teardown() {
    rm -rf "$TMP"
}

check() {
    run uv run --quiet "$W/scripts/corpus.py" check
}

# Adds a line to a corpus file, keeping it sorted, with a source.
add_line() {
    local file=$1 line=$2
    { cat "$C/$file"; printf '%s\n' "$line"; } | LC_ALL=C sort -u >"$C/$file.new"
    mv "$C/$file.new" "$C/$file"
    printf '%s\t%s\t%s\n' "$file" "$line" "test" >>"$C/sources.tsv"
}

@test "the committed corpus passes the check" {
    check
    [ "$status" -eq 0 ]
    [[ "$output" == *"check: ok"* ]]
}

@test "every corpus file is sorted with Python's ordering and parses" {
    for f in "$C"/*.argv; do
        [ -s "$f" ]
        ! grep -q $'\r' "$f"
    done
    run uv run --quiet python -c '
import shlex, sys, pathlib
for p in pathlib.Path(sys.argv[1]).glob("*.argv"):
    lines = p.read_text().splitlines()
    assert lines == sorted(set(lines)), p
    for l in lines:
        shlex.split(l)
' "$C"
    [ "$status" -eq 0 ]
}

@test "an absolute /Users or /home path fails the check" {
    add_line static-cli.argv "source-file $HOMEDIR/someone/.tmux.conf"
    uv run --quiet "$W/scripts/corpus.py" coverage >/dev/null
    check
    [ "$status" -ne 0 ]
    [[ "$output" == *"absolute home path"* ]]
}

@test "the home directory is denylisted even outside /Users and /home" {
    fake="$TMP/fakehome"
    mkdir -p "$fake"
    add_line static-cli.argv "source-file $fake/.tmux.conf"
    HOME="$fake" uv run --quiet "$W/scripts/corpus.py" coverage >/dev/null
    HOME="$fake" run uv run --quiet "$W/scripts/corpus.py" check
    [ "$status" -ne 0 ]
    [[ "$output" == *"denylisted string (home)"* ]]
}

@test "the hostname is denylisted as a word" {
    host=$(hostname -s)
    case $host in main|rec|demo|side|drive|tmux|home|work|default|server|root|user|??|?|'') skip "generic hostname" ;; esac
    add_line static-cli.argv "display-message $host"
    uv run --quiet "$W/scripts/corpus.py" coverage >/dev/null
    check
    [ "$status" -ne 0 ]
    [[ "$output" == *"denylisted string (hostname)"* ]]
}

@test "the git email is denylisted" {
    printf '[user]\n\temail = %s\n' "$PROBE_EMAIL" >"$TMP/gitconfig"
    add_line static-cli.argv "display-message $PROBE_EMAIL"
    cd "$TMP"
    GIT_CONFIG_GLOBAL="$TMP/gitconfig" uv run --quiet "$W/scripts/corpus.py" coverage >/dev/null
    GIT_CONFIG_GLOBAL="$TMP/gitconfig" run uv run --quiet "$W/scripts/corpus.py" check
    [ "$status" -ne 0 ]
    [[ "$output" == *"denylisted string (git email)"* ]]
}

@test "a term from the private denylist file fails the check, in any scanned file" {
    printf '# client names\nAcmeWidgetCorp\n' >"$TMP/denylist"
    printf '# source: manual:x\ndisplay-message acmewidgetcorp\n' >>"$W/scripts/corpus-manual.txt"
    GROFE_COMPAT_DENYLIST="$TMP/denylist" run uv run --quiet "$W/scripts/corpus.py" check
    [ "$status" -ne 0 ]
    [[ "$output" == *"scripts/corpus-manual.txt: contains a denylisted string (private denylist)"* ]]
    # and the file itself must live outside the repository
    cp "$TMP/denylist" "$W/denylist"
    GROFE_COMPAT_DENYLIST="$W/denylist" run uv run --quiet "$W/scripts/corpus.py" check
    [ "$status" -ne 0 ]
    [[ "$output" == *"outside the repository"* ]]
}

@test "a generic hostname (main, rec, tmux) does not fail a clean corpus; names match as whole words" {
    run uv run --quiet python -c '
import sys; sys.path.insert(0, sys.argv[1])
import corpus
for host in ("main", "rec", "tmux"):
    corpus.socket.gethostname = lambda: host
    corpus._cmd_out = lambda argv: host if argv[0] == "hostname" else ""
    dl = corpus.build_denylist()
    hits = [(p.name, h) for p in corpus.published_files() for h in corpus.deny_hits(p.read_text(), dl) if h[1] == "hostname"]
    assert not hits, (host, hits)
dl = corpus.Denylist([], [("corp1", "hostname")])
assert not corpus.deny_hits("corp1x xcorp1 corp1-a", dl)
assert corpus.deny_hits("at CORP1.", dl)
print("ok")
' "$W/scripts"
    [ "$status" -eq 0 ]
    [ "$output" = ok ]
}

@test "the staging screen uses the same denylist" {
    mkdir -p "$TMP/stage/sub"
    printf 'nothing here\n' >"$TMP/stage/a"
    GROFE_COMPAT_DENYLIST="$TMP/deny2" run uv run --quiet "$W/scripts/corpus.py" screen "$TMP/stage"
    [ "$status" -ne 0 ]  # a named denylist file that does not exist is an error
    printf 'AcmeWidgetCorp\n' >"$TMP/deny2"
    GROFE_COMPAT_DENYLIST="$TMP/deny2" run uv run --quiet "$W/scripts/corpus.py" screen "$TMP/stage"
    [ "$status" -eq 0 ]
    printf 'made for acmewidgetcorp\n' >"$TMP/stage/sub/b"
    GROFE_COMPAT_DENYLIST="$TMP/deny2" run uv run --quiet "$W/scripts/corpus.py" screen "$TMP/stage"
    [ "$status" -ne 0 ]
    [[ "$output" == *"sub/b (private denylist)"* ]]
}

@test "a line holding an email address fails the check" {
    add_line static-cli.argv "display-message someone${AT}example.org"
    uv run --quiet "$W/scripts/corpus.py" coverage >/dev/null
    check
    [ "$status" -ne 0 ]
    [[ "$output" == *"email address"* ]]
}

@test "a form with no spec 4.5 row fails the check" {
    add_line static-cli.argv "clock-mode -t %N"
    uv run --quiet "$W/scripts/corpus.py" coverage >/dev/null
    check
    [ "$status" -ne 0 ]
    [[ "$output" == *"'clock-mode -t' maps to no spec 4.5 row"* ]]
}

@test "a row id that names nothing in the spec fails the check" {
    printf 'clock-mode\tarea:No Such Area\n' >>"$W/scripts/compat-rows.tsv"
    check
    [ "$status" -ne 0 ]
    [[ "$output" == *"names no spec 4.5 row"* ]]
}

@test "no argv from a hook or run-shell job fails the check" {
    : >"$C/container-job.argv"
    check
    [ "$status" -ne 0 ]
    [[ "$output" == *"no argv from a hook or run-shell"* ]]
}

@test "an unsorted corpus file fails the check" {
    { tail -1 "$C/static-conf.argv"; cat "$C/static-conf.argv"; } >"$C/x" && mv "$C/x" "$C/static-conf.argv"
    check
    [ "$status" -ne 0 ]
    [[ "$output" == *"not sorted and unique"* ]]
}

@test "a line with no source fails the check" {
    { cat "$C/static-conf.argv"; printf 'unbind-key zz\n'; } | LC_ALL=C sort >"$C/x" && mv "$C/x" "$C/static-conf.argv"
    uv run --quiet "$W/scripts/corpus.py" coverage >/dev/null
    check
    [ "$status" -ne 0 ]
    [[ "$output" == *"no source in sources.tsv"* ]]
}

@test "a stale coverage document fails the check" {
    add_line static-conf.argv "unbind-key zz"
    check
    [ "$status" -ne 0 ]
    [[ "$output" == *"generated section is stale"* ]]
}

@test "the recorder logs argv and execs the real tmux by its fixed path" {
    fake="$TMP/real/tmux-real"
    mkdir -p "$TMP/real"
    printf '#!/bin/sh\nprintf "real:%%s|" "$@" >"%s"\n' "$TMP/ran" >"$fake"
    chmod +x "$fake"
    run bash "$W/scripts/tmux-argv-recorder.sh" install "$TMP/bin" "$fake" "$TMP/argv.log"
    [ "$status" -eq 0 ]
    # PATH holds only the recorder's dir and the system: the fake is not on it
    PATH="$TMP/bin:/usr/bin:/bin" run env -u TMUX -u TMUX_PANE tmux -L x new-session -d 'sleep 1; echo "a b"' \; set -g @a 1
    [ "$status" -eq 0 ]
    [ "$(cat "$TMP/ran")" = "real:-L|real:x|real:new-session|real:-d|real:sleep 1; echo \"a b\"|real:;|real:set|real:-g|real:@a|real:1|" ]
    [ "$(wc -l <"$TMP/argv.log" | tr -d ' ')" = 1 ]
    [ "$(cut -f1 "$TMP/argv.log")" = cli ]
    # the logged argv reads back to the same words
    run uv run --quiet python -c '
import sys; sys.path.insert(0, sys.argv[1])
import corpus
line = open(sys.argv[2]).read().rstrip("\n").split("\t", 1)[1]
print(corpus.parse_q_line(line))
' "$W/scripts" "$TMP/argv.log"
    [ "$output" = "['-L', 'x', 'new-session', '-d', 'sleep 1; echo \"a b\"', ';', 'set', '-g', '@a', '1']" ]
}

@test "the recorder logs a bare tmux as an empty argv" {
    printf '#!/bin/sh\nexit 0\n' >"$TMP/fake" && chmod +x "$TMP/fake"
    bash "$W/scripts/tmux-argv-recorder.sh" install "$TMP/bin" "$TMP/fake" "$TMP/argv.log"
    PATH="$TMP/bin:/usr/bin:/bin" env -u TMUX -u TMUX_PANE tmux
    [ "$(cat "$TMP/argv.log")" = "cli	" ]
}

@test "the recorder refuses a relative path, itself, or its own target as the real tmux" {
    printf '#!/bin/sh\nexit 0\n' >"$TMP/fake" && chmod +x "$TMP/fake"
    run bash "$W/scripts/tmux-argv-recorder.sh" install "$TMP/bin" fake "$TMP/argv.log"
    [ "$status" -ne 0 ]
    [[ "$output" == *"absolute path"* ]]
    bash "$W/scripts/tmux-argv-recorder.sh" install "$TMP/bin" "$TMP/fake" "$TMP/argv.log"
    run bash "$W/scripts/tmux-argv-recorder.sh" install "$TMP/bin2" "$TMP/bin/tmux" "$TMP/argv.log"
    [ "$status" -ne 0 ]
    [[ "$output" == *"is a recorder itself"* ]]
    run bash "$W/scripts/tmux-argv-recorder.sh" --help
    [ "$status" -eq 2 ]
}

@test "normalize scrubs a recording into the container files" {
    log="$TMP/argv.log"
    {
        printf 'cli\t-L canopy-verify-123 -f /dev/null new-session -d sleep\\ 600 \\; source-file /home/rec/.config/tmux/tmux.conf \n'
        printf 'pane\tset -p -t %%12 @canopy_agent_state working \\; set -p -t %%12 @canopy_agent_ts 1759000000 \n'
        printf 'job\tswitch-client -c /dev/pts/3 -t =work: \n'
        printf 'job\tnew-session -ds proj -c /home/rec/src/proj \n'
    } >"$log"
    printf 'bind-key -T prefix B run-shell /home/rec/.local/bin/cockpit\n' >"$TMP/keys.txt"
    printf 'after-select-pane[0] run-shell -b "curl -s http://127.0.0.1:1/focus"\nsession-closed\n' >"$TMP/hooks.txt"
    run bash "$W/scripts/normalize-corpus.sh" "$log" /home/rec "$TMP/keys.txt" "$TMP/hooks.txt"
    [ "$status" -eq 0 ]
    [ "$(cat "$C/container-cli.argv")" = '-L <socket> -f /dev/null new-session -d '"'"'sleep 600'"'"' \; source-file <file>' ]
    [ "$(cat "$C/container-pane.argv")" = 'set -p -t %N @canopy_agent_state <value> \; set -p -t %N @canopy_agent_ts <value>' ]
    [ "$(sed -n 1p "$C/container-job.argv")" = 'new-session -d -s <name> -c <dir>' ]
    [ "$(sed -n 2p "$C/container-job.argv")" = 'switch-client -c <client> -t =<session>:' ]
    [ "$(sed -n 1p "$C/container-stored.argv")" = 'bind-key -T prefix B run-shell ~/.local/bin/cockpit' ]
    [ "$(sed -n 2p "$C/container-stored.argv")" = 'set-hook -g after-select-pane '"'"'run-shell -b "curl -s http://127.0.0.1:1/focus"'"'"'' ]
    grep -q $'^container-job.argv\tswitch-client -c <client> -t =<session>:\tcontainer:job$' "$C/sources.tsv"
    # the static files and their sources are left alone
    cmp "$C/static-cli.argv" "$REPO/tests/compat/corpus/static-cli.argv"
    grep -q '^static-cli.argv' "$C/sources.tsv"
}

@test "a nested pane id stays %N, and a nested window id @N" {
    run uv run --quiet python -c '
import sys; sys.path.insert(0, sys.argv[1])
import corpus
ctx = corpus.ScrubContext([], [], [], [])
out = corpus.normalize_argv(["if", "-F", "-t", "%3", "#{window_zoomed_flag}", "resize-pane -Z -t %12", "select-window -t @7"], ctx)
print(corpus.to_line(out))
' "$W/scripts"
    [ "$status" -eq 0 ]
    [ "$output" = "if -F -t %N #{window_zoomed_flag} 'resize-pane -Z -t %N' 'select-window -t @N'" ]
}

@test "normalize is idempotent over every committed corpus file" {
    run uv run --quiet python -c '
import sys; sys.path.insert(0, sys.argv[1])
import corpus
ctx = corpus.ScrubContext([], [], [], [])
bad = 0
for name, lines in corpus.read_corpus().items():
    conf = name in ("static-conf.argv", "container-stored.argv")
    for line in lines:
        again = corpus.to_line(corpus.normalize_argv(corpus.from_line(line), ctx, conf=conf))
        if again != line:
            bad += 1
            print(f"{name}: {line}\n  -> {again}")
print(f"changed {bad}")
sys.exit(1 if bad else 0)
' "$W/scripts"
    [ "$status" -eq 0 ]
}

@test "mktemp suffixes are normalized, ordinary words under /tmp are not" {
    run uv run --quiet python -c '
import sys; sys.path.insert(0, sys.argv[1])
import corpus
ctx = corpus.ScrubContext([], [], [], [])
for s in ("/tmp/fzf-tmux-noGkZt", "/tmp/fzf-tmux-am1Eyv", "/tmp/sessionx-Preview", "/tmp/opensessions-install.log"):
    print(corpus.scrub(s, ctx))
' "$W/scripts"
    [ "$status" -eq 0 ]
    [ "$output" = $'/tmp/fzf-tmux-XXXXXX\n/tmp/fzf-tmux-XXXXXX\n/tmp/sessionx-Preview\n/tmp/opensessions-install.log' ]
}

@test "a denylisted name after a dash is caught" {
    run uv run --quiet python -c '
import sys; sys.path.insert(0, sys.argv[1])
import corpus
dl = corpus.Denylist([], [("someuser", "user name")])
assert corpus.deny_hits("/tmp/x-someuser.sock", dl)
assert not corpus.deny_hits("/tmp/x-someuserz", dl)
print("ok")
' "$W/scripts"
    [ "$output" = ok ]
}

# The drivers start and drive a tmux server, so they must refuse to run on a
# host. GROFE_TC0_CONTAINER_MARKER (tests only) replaces the container marker
# files; every case below stops at a guard before any tmux is run.
@test "drive.py and scenario.sh refuse to run outside a container" {
    GROFE_TC0_CONTAINER_MARKER="$TMP/absent" run env -u TMUX TMUX_TMPDIR="$TMP" python3 "$REPO/containers/compat/drive.py" --out "$TMP" --real /nonexistent
    [ "$status" -ne 0 ]
    [[ "$output" == *"refusing to run outside a container"* ]]
    GROFE_TC0_CONTAINER_MARKER="$TMP/absent" run env -u TMUX bash "$REPO/containers/compat/scenario.sh"
    [ "$status" -ne 0 ]
    [[ "$output" == *"refusing to run outside a container"* ]]
}

@test "drive.py and scenario.sh refuse to run inside tmux, and drive.py without TMUX_TMPDIR" {
    touch "$TMP/marker"
    GROFE_TC0_CONTAINER_MARKER="$TMP/marker" TMUX=/tmp/fake,1,0 TMUX_TMPDIR="$TMP" run python3 "$REPO/containers/compat/drive.py" --out "$TMP" --real /nonexistent
    [ "$status" -ne 0 ]
    [[ "$output" == *"refusing to run inside tmux"* ]]
    GROFE_TC0_CONTAINER_MARKER="$TMP/marker" TMUX=/tmp/fake,1,0 run bash "$REPO/containers/compat/scenario.sh"
    [ "$status" -ne 0 ]
    [[ "$output" == *"refusing to run inside tmux"* ]]
    GROFE_TC0_CONTAINER_MARKER="$TMP/marker" run env -u TMUX -u TMUX_TMPDIR python3 "$REPO/containers/compat/drive.py" --out "$TMP" --real /nonexistent
    [ "$status" -ne 0 ]
    [[ "$output" == *"without a private TMUX_TMPDIR"* ]]
}

@test "with the test marker and every guard passing, the drivers exit 0 without running anything" {
    touch "$TMP/marker"
    GROFE_TC0_CONTAINER_MARKER="$TMP/marker" run env -u TMUX TMUX_TMPDIR="$TMP" python3 "$REPO/containers/compat/drive.py" --out "$TMP" --real /nonexistent
    [ "$status" -eq 0 ]
    [ "$output" = "drive: guards passed, test mode" ]
    GROFE_TC0_CONTAINER_MARKER="$TMP/marker" HOME="$TMP/h" run env -u TMUX TMUX_TMPDIR="$TMP" bash "$REPO/containers/compat/scenario.sh"
    [ "$status" -eq 0 ]
    [ "$output" = "scenario: guards passed, test mode" ]
    [ ! -e "$TMP/h" ]
}
