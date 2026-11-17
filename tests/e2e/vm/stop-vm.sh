#!/usr/bin/env bash
# Tear the 26.04 VM down and remove everything the run created. Idempotent: safe to
# call twice when a trap fires after an explicit stop. The base cloud image is kept
# (it is verified by start-vm.sh and reused), the overlay disk is not.
#
# Env: DIR
set -uo pipefail

DIR="${DIR:-/tmp/sizer-vm}"
DISK="$DIR/sizer-2604.qcow2"
QMP="$DIR/qmp.sock"

poweroff_qmp() {
    python3 - "$QMP" <<'PY' || return 1
import json, socket, sys
path = sys.argv[1]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(path)
sock.recv(4096)  # greeting
for command in ({"execute": "qmp_capabilities"},
                {"execute": "system_powerdown"}):
    sock.sendall((json.dumps(command) + "\n").encode())
    sock.recv(4096)
sock.close()
PY
}

# qemu runs under setsid, so there is no pid to wait on. Ask it to power off, give the
# guest time to reach the QMP quit, then terminate whatever is still attached to the
# overlay: a leftover qemu would keep the disk busy and the RAM allocated.
if [ -S "$QMP" ] && poweroff_qmp; then
    for _ in $(seq 1 60); do
        pgrep -f "qemu-system-x86_64.*$DISK" >/dev/null || break
        sleep 0.5
    done
fi

pids="$(pgrep -f "qemu-system-x86_64.*$DISK" || true)"
if [ -n "$pids" ]; then
    echo "terminating qemu: $pids" >&2
    kill $pids 2>/dev/null || true
    for _ in $(seq 1 20); do
        pgrep -f "qemu-system-x86_64.*$DISK" >/dev/null || break
        sleep 0.5
    done
    pids="$(pgrep -f "qemu-system-x86_64.*$DISK" || true)"
    [ -n "$pids" ] && kill -9 $pids 2>/dev/null
fi

rm -f "$DISK" "$QMP" "$DIR/seed.iso" "$DIR/user-data" "$DIR/meta-data"
echo "vm stopped: removed overlay and seed, kept $(ls "$DIR"/*.img 2>/dev/null || echo 'no image')"
