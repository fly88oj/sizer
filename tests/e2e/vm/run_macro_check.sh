#!/usr/bin/env bash
# Run the macro proof against a VM that prove.sh left running (prove.sh --keep),
# or from any machine that can ssh to the VM. Pushes the checker, so no manual
# copy step is needed and a rerun after a rebuild is one command.
#
# Usage: tests/e2e/vm/run_macro_check.sh
# Env: DIR (default /tmp/sizer-vm), SSH_PORT, SIZER, CLIENT
set -euo pipefail

DIR="${DIR:-/tmp/sizer-vm}"
SSH_PORT="${SSH_PORT:-2222}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SSH_OPTS=(-p "$SSH_PORT" -i "$DIR/id_ed25519" -o BatchMode=yes
          -o ConnectTimeout=10 -o ServerAliveInterval=30 -o ServerAliveCountMax=4
          -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
SCP_OPTS=(-P "$SSH_PORT" -i "$DIR/id_ed25519"
          -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
SESSION_ENV='XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus'

vm_ssh() { ssh "${SSH_OPTS[@]}" sizer@127.0.0.1 "$@"; }
vm_put() { scp "${SCP_OPTS[@]}" "$1" "sizer@127.0.0.1:$2"; }

vm_ssh 'test -x /usr/bin/sizer' || {
    echo "sizer is not installed in the VM; run tests/e2e/vm/prove.sh first" >&2
    exit 1
}
vm_ssh 'mkdir -p /tmp/sizer-e2e'
vm_put "$HERE/macro_check.py" /tmp/sizer-e2e/macro_check.py
vm_put "$HERE/../gnome_extension.py" /tmp/sizer-e2e/gnome_extension.py

# `${SIZER:-/usr/bin/sizer}` is resolved inside the guest, so a custom install
# path only has to exist there. The client is a GTK app: it needs the real
# session's Wayland socket (or DISPLAY when the session is X11), which the
# session bus alone does not provide.
vm_ssh "env $SESSION_ENV SIZER=${SIZER:-/usr/bin/sizer} \
    CLIENT=${CLIENT:-/tmp/sizer-e2e/gnome_client.py} bash -s" <<'EOF'
set -euo pipefail
if compgen -G "/run/user/1000/wayland-*" >/dev/null; then
    export WAYLAND_DISPLAY="$(basename "$(ls /run/user/1000/wayland-* | head -n1)")"
else
    export DISPLAY=:0
fi
exec python3 /tmp/sizer-e2e/macro_check.py
EOF
