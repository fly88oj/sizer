#!/usr/bin/env bash
# From-zero proof of the Ubuntu 26.04 (GNOME 50) lane.
#
# Boots the desktop VM, lets cloud-init install the desktop, installs the deb that
# this checkout builds, enables the extension, and then asserts the two claims that
# a container could not make:
#   1. a key chord injected through QMP reaches mutter (key_count grows, menu opens);
#   2. a preset lands exactly on the focused window (`landed` reported by the same
#      matcher the CI harness uses);
#   3. a multi-window macro (`proc[...]:cap[...]:desktop*`) lays out both windows
#      and every new D-Bus member the executor uses exists and behaves the same
#      (tests/e2e/vm/macro_check.py, rerunnable with run_macro_check.sh).
#
# Any exit path powers the VM off and removes its overlay, so no qemu process and no
# 40G disk are left behind. Use --keep to inspect a failure (then run stop-vm.sh).
#
# Usage: tests/e2e/vm/prove.sh [--keep]
# Env: DIR (default /tmp/sizer-vm), VNC, SSH_PORT, MEM, SMP
set -euo pipefail

DIR="${DIR:-/tmp/sizer-vm}"
VNC="${VNC:-:1}"
SSH_PORT="${SSH_PORT:-2222}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

say() { printf '\n== %s ==\n' "$*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }

SSH_OPTS=(-p "$SSH_PORT" -i "$DIR/id_ed25519" -o BatchMode=yes
          -o ConnectTimeout=10 -o ServerAliveInterval=30 -o ServerAliveCountMax=4
          -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
# scp spells the port differently; passing the ssh option list here makes it treat
# the port number as a source file.
SCP_OPTS=(-P "$SSH_PORT" -i "$DIR/id_ed25519"
          -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)

vm_ssh() { ssh "${SSH_OPTS[@]}" sizer@127.0.0.1 "$@"; }
vm_scp() { scp "${SCP_OPTS[@]}" "$@" sizer@127.0.0.1:/tmp/sizer-e2e/; }
# Copy one file under a chosen name: the versioned deb name would otherwise be
# installed from a path the install step does not know.
vm_put() { scp "${SCP_OPTS[@]}" "$1" "sizer@127.0.0.1:$2"; }

# The guest session the assertions talk to (uid 1000 is the autologin user).
SESSION_ENV='XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus'
session_check() {
    vm_ssh "env $SESSION_ENV python3 /tmp/sizer-e2e/session_check.py $*"
}

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if [ "$KEEP" = 1 ]; then
        echo "VM kept for inspection: qmp=$DIR/qmp.sock ssh=127.0.0.1:$SSH_PORT"
        echo "stop it with $HERE/stop-vm.sh"
    else
        bash "$HERE/stop-vm.sh" || echo "stop-vm.sh failed" >&2
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

say "build the deb this checkout ships"
(cd "$ROOT" && packaging/build-deb.sh) || fail "packaging/build-deb.sh"
DEB="$(ls -t "$ROOT"/target/deb/sizer_*.deb | head -n1)"
[ -f "$DEB" ] || fail "no deb produced"
echo "deb: $DEB"

say "start the VM"
bash "$HERE/start-vm.sh"

say "wait for ssh"
ready=0
for _ in $(seq 1 180); do
    if vm_ssh true 2>/dev/null; then ready=1; break; fi
    sleep 2
done
[ "$ready" = 1 ] || { tail -n 20 "$DIR/qemu.log" >&2 || true; fail "ssh never came up"; }

say "wait for cloud-init to finish"
# The guest-side timeout only covers a slow install; a dead ssh channel would leave
# this wait hanging with no output, so the call also has a host-side deadline. The
# extra minute lets the guest-side timeout report first in the normal slow case.
if ! timeout 3660 ssh "${SSH_OPTS[@]}" sizer@127.0.0.1 \
        'sudo timeout 3600 cloud-init status --wait >/dev/null 2>&1 || true'; then
    vm_ssh 'sudo tail -n 60 /var/log/cloud-init-output.log' >&2 || true
    fail "cloud-init did not finish before the host-side timeout"
fi

say "assert the desktop really installed"
if ! vm_ssh 'bash -s' <<'EOF'
set -euo pipefail
status="$(cloud-init status 2>/dev/null || true)"
printf '%s\n' "$status"
grep -qE 'status:[[:space:]]+done' <<<"$status"
for package in ubuntu-desktop-minimal gnome-shell gdm3; do
    test "$(dpkg-query -W -f='${Status}' "$package" 2>/dev/null)" = "install ok installed"
done
test "$(systemctl is-active gdm3)" = "active"
echo CLOUD_INIT_AND_DESKTOP_OK
EOF
then
    vm_ssh 'sudo tail -n 60 /var/log/cloud-init-output.log' >&2 || true
    fail "cloud-init or desktop packages are not installed; see the log tail above"
fi

say "push the deb and the harness"
vm_ssh 'rm -rf /tmp/sizer-e2e && mkdir -p /tmp/sizer-e2e'
vm_put "$DEB" /tmp/sizer-e2e/sizer.deb
vm_scp "$ROOT"/tests/e2e/*.py "$HERE/session_check.py" "$HERE/run_harness_in_vm.sh" \
    "$HERE/macro_check.py"
vm_ssh 'test -f /tmp/sizer-e2e/sizer.deb && test -f /tmp/sizer-e2e/session_check.py' \
    || fail "push did not land"

say "install the deb"
vm_ssh 'sudo apt-get install -y /tmp/sizer-e2e/sizer.deb' || fail "deb install"

say "restart the session so the shell scans the installed extension"
# The shell builds its extension list at startup, so enabling an extension that
# appeared while it was running reports "does not exist". Install first, then restart.
vm_ssh 'sudo systemctl restart gdm3'

say "wait for the shell to list the extension"
listed=0
for _ in $(seq 1 120); do
    listed_out="$(vm_ssh "env $SESSION_ENV gnome-extensions list 2>/dev/null" || true)"
    if grep -q '^sizer@sizerlinux.dev$' <<<"$listed_out"; then listed=1; break; fi
    sleep 1
done
[ "$listed" = 1 ] || fail "the shell never listed sizer@sizerlinux.dev"

say "enable the extension"
vm_ssh "env $SESSION_ENV gnome-extensions enable sizer@sizerlinux.dev" \
    || fail "gnome-extensions enable"

say "wait for the session bus"
bus=0
for _ in $(seq 1 150); do
    probe="$(session_check wait-bus 2 2>/dev/null || true)"
    if grep -q '"bus": true' <<<"$probe"; then bus=1; break; fi
    sleep 1
done
[ "$bus" = 1 ] || fail "org.sizerlinux.Shell never appeared on the session bus"

say "assert the extension is ACTIVE"
info="$(vm_ssh "env $SESSION_ENV gnome-extensions info sizer@sizerlinux.dev")"
printf '%s\n' "$info"
grep -q 'State: ACTIVE' <<<"$info" || fail "extension is not ACTIVE"

say "start the target window"
vm_ssh 'bash -s' <<'EOF'
set -euo pipefail
export XDG_RUNTIME_DIR=/run/user/1000
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
if compgen -G "/run/user/1000/wayland-*" >/dev/null; then
    export WAYLAND_DISPLAY="$(basename "$(ls /run/user/1000/wayland-* | head -n1)")"
else
    export DISPLAY=:0
fi
cd /tmp/sizer-e2e
setsid python3 gnome_client.py --title SIZER-SPIKE-CLIENT --width 800 --height 600 \
    >/tmp/client.log 2>&1 < /dev/null &
for _ in $(seq 1 150); do
    grep -q '"event": "ready"' /tmp/client.log 2>/dev/null && break
    sleep 0.2
done
cat /tmp/client.log
grep -q '"event": "ready"' /tmp/client.log
EOF

say "inject ctrl+super+z through QMP"
before="$(session_check keys)"
before_count="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["keys"].get("key_count") or 0)' <<<"$before")"
python3 "$HERE/qmp_sendkey.py" --qmp "$DIR/qmp.sock" --chord ctrl+super+z
# `key_count` is the extension's own counter, so it only grows when mutter delivered
# the chord to the binding — not when QMP merely accepted it.
keys_ok() {
    python3 -c 'import json,sys
state = json.loads(sys.argv[1])
keys = state.get("keys") or {}
menu = state.get("menu") or {}
sys.exit(0 if (keys.get("key_count") or 0) > int(sys.argv[2]) and bool(menu.get("open")) else 1)' "$1" "$2"
}
after=""
for _ in $(seq 1 50); do
    after="$(session_check keys)"
    keys_ok "$after" "$before_count" && break
    sleep 0.2
done
keys_ok "$after" "$before_count" || fail "the injected chord did not reach the extension"
python3 -c 'import json,sys
state = json.loads(sys.argv[1])
keys = state.get("keys") or {}
menu = state.get("menu") or {}
print(json.dumps({"before": int(sys.argv[2]), "key_count": keys.get("key_count"),
                  "menu_open": menu.get("open"), "last_key_time": keys.get("last_key_time")}))' \
    "$after" "$before_count"

say "push a preset and assert it lands exactly"
# The JSON goes over stdin: `{`/`}` and `"` do not survive being pasted into the
# remote shell's command line.
vm_ssh "env $SESSION_ENV python3 /tmp/sizer-e2e/session_check.py presets -" \
    <<< '[{"label":"800x600","size":"800x600","anchor":"center"}]'
applied="$(session_check apply 0)"
printf '%s\n' "$applied"
python3 -c 'import json,sys; sys.exit(0 if json.loads(sys.argv[1]).get("landed") is True else 1)' \
    "$applied" || fail "the preset did not land on the focused window"

say "prove the macro path on this GNOME version"
bash "$HERE/run_macro_check.sh" || fail "the macro path did not hold on this GNOME version"

say "PASS"
