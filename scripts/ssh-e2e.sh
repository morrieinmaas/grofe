#!/usr/bin/env bash
# ssh-e2e.sh: the container end-to-end run for ssh remote sessions (spec
# 2026-11-15-ssh-remote.md §6), locally and in CI (.github/workflows/ssh-e2e.yml).
#
#   scripts/ssh-e2e.sh [cargo-test-filter...]   build, start, run, tear down
#   scripts/ssh-e2e.sh e2e_exec <cmd...>        run a command in the container
#   scripts/ssh-e2e.sh e2e_netem <tc args...>   `tc qdisc replace ... netem <args>`;
#                                               `e2e_netem clear` removes it
#   scripts/ssh-e2e.sh e2e_kill_sshd_children   kill every ssh session's sshd
#                                               process (the listener stays)
#
# The helpers read GROFE_E2E_RUNTIME and GROFE_E2E_CONTAINER, which a run
# exports to the tests, so a scenario calls them as
# `$GROFE_E2E_SCRIPT e2e_netem delay 200ms`. E2E_CONTAINER overrides which
# container they act on (the bare one is GROFE_E2E_BARE_CONTAINER).
#
# A run builds containers/ssh/Containerfile from the worktree's source
# (tracked and untracked files, as they are on disk) into two images, starts
# a container of each on a random localhost port with NET_ADMIN, and writes a
# throwaway ssh key, known_hosts and config into a temp dir, so tests do
# `ssh -F "$GROFE_E2E_SSH_CONFIG" "$GROFE_E2E_HOST"`. It builds a local release
# directory (scripts/ssh-e2e-release.sh) and serves it on a random localhost
# port. It runs `cargo test --test ssh_e2e` with:
#
#   GROFE_E2E_SSH_CONFIG     the ssh config file
#   GROFE_E2E_HOST           host alias of the container with grofe installed
#   GROFE_E2E_BARE_HOST      host alias of the container without grofe
#   GROFE_E2E_RELEASE_URL    http://127.0.0.1:<port>, the local release dir
#                            (v<version>/ below it, as remote-release-url wants)
#   GROFE_E2E_RUNTIME, GROFE_E2E_CONTAINER, GROFE_E2E_BARE_CONTAINER,
#   GROFE_E2E_SCRIPT         for the helpers above
#
# Everything is torn down on exit. The only host ssh ever reaches is a container
# on 127.0.0.1. Works with docker and podman (GROFE_E2E_RUNTIME picks one).
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"

runtime() {
  if [ -n "${GROFE_E2E_RUNTIME:-}" ]; then echo "$GROFE_E2E_RUNTIME"
  elif command -v podman >/dev/null; then echo podman
  elif command -v docker >/dev/null; then echo docker
  else echo "ssh-e2e: neither podman nor docker is installed" >&2; return 1; fi
}

target_container() { echo "${E2E_CONTAINER:-${GROFE_E2E_CONTAINER:?GROFE_E2E_CONTAINER is not set}}"; }

e2e_exec() { "$(runtime)" exec "$(target_container)" "$@"; }

# The interface the ssh sessions' traffic leaves by: the route to the peer of
# an established :22 connection, else the default route. Not always eth0:
# rootless podman's port forwarder connects from inside the container's own
# network namespace, so that traffic is on lo.
e2e_dev() {
  e2e_exec sh -c '
    route_dev() { sed -n "s/.* dev \([^ ]*\).*/\1/p" | head -1; }
    peer=$(ss -Htn state established "( sport = :22 )" | awk "{ print \$4 }" | sed "s/:[0-9]*\$//" | head -1)
    if [ -n "$peer" ]; then ip route get "$peer" | route_dev; else ip route show default | route_dev; fi'
}

e2e_netem() {
  local dev
  if [ "${1:-}" = clear ]; then
    e2e_exec sh -c 'for d in $(ls /sys/class/net); do tc qdisc del dev "$d" root 2>/dev/null; done; true'
    return
  fi
  dev="$(e2e_dev)"
  [ -n "$dev" ] || dev=eth0
  e2e_exec tc qdisc replace dev "$dev" root netem "$@"
}

# The per-session sshd processes run as the test user's "sshd-session: tester"
# (or "sshd: tester"); the listener, "sshd: /usr/sbin/sshd", does not match.
e2e_kill_sshd_children() {
  e2e_exec sh -c "ps -eo pid=,args= | awk '/sshd(-session)?: tester/ { print \$1 }' | xargs -r kill -9"
}

case "${1:-}" in
  e2e_exec | e2e_netem | e2e_kill_sshd_children)
    fn="$1"; shift; "$fn" "$@"; exit ;;
esac

RT="$(runtime)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/ssh-e2e.XXXXXX")"
suffix="$$"
containers=("grofe-ssh-e2e-$suffix" "grofe-ssh-e2e-bare-$suffix")
www_pid=""

cleanup() {
  set +e
  [ -z "$www_pid" ] || kill "$www_pid" 2>/dev/null
  for c in "${containers[@]}" "${containers[0]}-bin"; do "$RT" rm -f "$c" >/dev/null 2>&1; done
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

echo "ssh-e2e: runtime $RT, workdir $tmp"

# Build context: the source as it is on disk, minus what git ignores.
ctx="$tmp/context"
mkdir "$ctx"
git ls-files -z --cached --others --exclude-standard | while IFS= read -r -d '' f; do
  [ -e "$f" ] && printf '%s\0' "$f"
done | tar --null -T - -cf - | tar -xf - -C "$ctx"

echo "ssh-e2e: building images"
"$RT" build -q -f "$ctx/containers/ssh/Containerfile" --target preinstalled -t grofe-ssh-e2e:preinstalled "$ctx" >/dev/null
"$RT" build -q -f "$ctx/containers/ssh/Containerfile" --target bare -t grofe-ssh-e2e:bare "$ctx" >/dev/null

# A throwaway key, never committed.
ssh-keygen -q -t ed25519 -N '' -C ssh-e2e -f "$tmp/id"
pubkey="$(cat "$tmp/id.pub")"

start() { # start <container> <image>; prints the published ssh port
  "$RT" run -d --init --name "$1" --cap-add NET_ADMIN -e "E2E_PUBKEY=$pubkey" \
    -p 127.0.0.1::22 "$2" >/dev/null
  "$RT" port "$1" 22 | head -1 | sed 's/.*://'
}
port="$(start "${containers[0]}" grofe-ssh-e2e:preinstalled)"
bare_port="$(start "${containers[1]}" grofe-ssh-e2e:bare)"

cfg="$tmp/ssh_config"
host_entry() { # host_entry <alias> <port>
  cat <<EOS
Host $1
  HostName 127.0.0.1
  Port $2
  User tester
  IdentityFile $tmp/id
  IdentitiesOnly yes
  BatchMode yes
  StrictHostKeyChecking accept-new
  UserKnownHostsFile $tmp/known_hosts
  ControlMaster no
  ControlPath none
  LogLevel ERROR
EOS
}
{ host_entry grofe-e2e "$port"; host_entry grofe-e2e-bare "$bare_port"; } > "$cfg"

for h in grofe-e2e grofe-e2e-bare; do
  ok=""
  for _ in $(seq 60); do
    if ssh -F "$cfg" -o ConnectTimeout=3 "$h" true 2>/dev/null; then ok=1; break; fi
    sleep 1
  done
  [ -n "$ok" ] || { echo "ssh-e2e: $h never accepted a login" >&2; "$RT" logs "${containers[0]}" >&2 || true; exit 1; }
done

# The local release dir: the container's own binary, named for its target.
case "$("$RT" image inspect --format '{{.Architecture}}' grofe-ssh-e2e:preinstalled)" in
  arm64 | aarch64) target=aarch64-unknown-linux-musl ;;
  *) target=x86_64-unknown-linux-musl ;;
esac
"$RT" create --name "${containers[0]}-bin" grofe-ssh-e2e:preinstalled >/dev/null
"$RT" cp "${containers[0]}-bin:/usr/local/bin/grofe" "$tmp/grofe-linux"
"$RT" rm -f "${containers[0]}-bin" >/dev/null
# Laid out as the GitHub release downloads are, v<version>/<asset>, which is
# where the bootstrap install looks below remote-release-url.
tag="v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
scripts/ssh-e2e-release.sh "$tmp/release/$tag" "$tmp/grofe-linux" "$target" "$tag" >/dev/null

# ponytail: a stdlib http.server on a port the OS picks; it ends after an hour
# or at teardown, and serves one directory, no ranges or keep-alive.
python3 -I -c '
import functools, http.server, signal, socketserver, sys
signal.alarm(3600)
class Server(http.server.ThreadingHTTPServer):
    def server_bind(self):  # HTTPServer.server_bind does a slow reverse lookup
        socketserver.TCPServer.server_bind(self)
        self.server_port = self.socket.getsockname()[1]
h = functools.partial(http.server.SimpleHTTPRequestHandler, directory=sys.argv[1])
s = Server(("127.0.0.1", 0), h)
print(s.server_port, flush=True)
s.serve_forever()
' "$tmp/release" > "$tmp/www.port" 2>"$tmp/www.log" &
www_pid=$!
for _ in $(seq 50); do [ -s "$tmp/www.port" ] && break; sleep 0.1; done
[ -s "$tmp/www.port" ] || { echo "ssh-e2e: the release server did not start" >&2; exit 1; }

export GROFE_E2E_SSH_CONFIG="$cfg"
export GROFE_E2E_HOST=grofe-e2e
export GROFE_E2E_BARE_HOST=grofe-e2e-bare
export GROFE_E2E_RELEASE_URL="http://127.0.0.1:$(cat "$tmp/www.port")"
export GROFE_E2E_RUNTIME="$RT"
export GROFE_E2E_CONTAINER="${containers[0]}"
export GROFE_E2E_BARE_CONTAINER="${containers[1]}"
export GROFE_E2E_SCRIPT="$root/scripts/ssh-e2e.sh"

echo "ssh-e2e: running the scenarios"
env -u GROFE -u GROFE_PANE -u TMUX -u TMUX_PANE \
  cargo test --test ssh_e2e "$@" -- --test-threads=1 --nocapture
