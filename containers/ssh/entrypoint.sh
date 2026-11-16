#!/bin/sh
# Container entrypoint: authorize the key in E2E_PUBKEY for the test user,
# serve ~/www on :8000 (the port-forward scenario's target), run sshd in the
# foreground. Run under an init (--init) so killed children are reaped.
set -eu
[ -n "${E2E_PUBKEY:-}" ] || { echo "e2e-entrypoint: E2E_PUBKEY is not set" >&2; exit 1; }
printf '%s\n' "$E2E_PUBKEY" > /home/tester/.ssh/authorized_keys
chown tester:tester /home/tester/.ssh/authorized_keys
chmod 600 /home/tester/.ssh/authorized_keys
runuser -u tester -- python3 -m http.server 8000 --bind 127.0.0.1 --directory /home/tester/www \
  >/tmp/www.log 2>&1 &
exec /usr/sbin/sshd -D -e
