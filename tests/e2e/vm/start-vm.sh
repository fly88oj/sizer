#!/usr/bin/env bash
# Start the Ubuntu 26.04 desktop VM that proves real key delivery: keys go in through
# QMP at the virtual hardware level, so they reach libinput and mutter like a
# physical keyboard. The base image is verified (https, pinned sha256) and stays
# pristine; every run works on a fresh qcow2 overlay. Use stop-vm.sh to tear down.
#
# Env: DIR, VNC, SSH_PORT, MEM, SMP, IMAGE (path to the cloud image)
set -euo pipefail

DIR="${DIR:-/tmp/sizer-vm}"
IMAGE="${IMAGE:-$DIR/ubuntu-26.04-server-cloudimg-amd64.img}"
IMAGE_URL="https://cloud-images.ubuntu.com/releases/26.04/release/ubuntu-26.04-server-cloudimg-amd64.img"
IMAGE_SHA256="5645d0318ad21f643ebb1ac4567514620e9893380b07b0201fda163baa5b7eb5"
IMAGE_VIRTUAL_SIZE=3758096384
DISK="$DIR/sizer-2604.qcow2"
SEED="$DIR/seed.iso"
QMP="$DIR/qmp.sock"
SSH_KEY="$DIR/id_ed25519"
VNC="${VNC:-:1}"
SSH_PORT="${SSH_PORT:-2222}"
MEM="${MEM:-6144}"
SMP="${SMP:-4}"
HOSTNAME="sizer-2604"
VM_USER="sizer"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$DIR"

if [ ! -f "$IMAGE" ]; then
    echo "fetching $IMAGE_URL" >&2
    curl -fsSL --retry 3 -o "$IMAGE.part" "$IMAGE_URL"
    mv "$IMAGE.part" "$IMAGE"
fi

# The checksum pins the release: a re-published image or a truncated download is a
# hard stop, not something to discover after the desktop install.
actual="$(sha256sum "$IMAGE" | cut -d' ' -f1)"
if [ "$actual" != "$IMAGE_SHA256" ]; then
    echo "image checksum mismatch: $actual (expected $IMAGE_SHA256)" >&2
    echo "delete $IMAGE to re-fetch $IMAGE_URL" >&2
    exit 1
fi
virtual="$(qemu-img info --output=json "$IMAGE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["virtual-size"])')"
if [ "$virtual" != "$IMAGE_VIRTUAL_SIZE" ]; then
    echo "image virtual size $virtual, expected $IMAGE_VIRTUAL_SIZE" >&2
    exit 1
fi
if qemu-img info "$IMAGE" | grep -q 'file format: raw'; then
    echo "image must be a qcow2 cloud image" >&2
    exit 1
fi
echo "image verified: $IMAGE sha256=$actual virtual-size=$virtual"

# The seed is regenerated whenever the template changes. The SSH key is generated
# again on every run, so a stale key on disk can never be trusted by a fresh guest
# and no credential has to live in the repository.
rm -f "$SSH_KEY" "$SSH_KEY.pub"
ssh-keygen -t ed25519 -N '' -f "$SSH_KEY" -C "sizer-vm" >/dev/null
PUBKEY="$(cat "$SSH_KEY.pub")"
sed -e "s#__SSH_KEY__#$PUBKEY#" -e "s#__HOSTNAME__#$HOSTNAME#" -e "s#__USER__#$VM_USER#" \
    "$HERE/user-data.in" > "$DIR/user-data"
printf 'instance-id: %s-1\nlocal-hostname: %s\n' "$HOSTNAME" "$HOSTNAME" > "$DIR/meta-data"
(
    cd "$DIR"
    xorriso -as mkisofs -output "$SEED" -volid cidata -joliet -rock user-data meta-data >/dev/null
)

if pgrep -f "qemu-system-x86_64.*$DISK" >/dev/null; then
    echo "a VM is already running on $DISK; run stop-vm.sh first" >&2
    exit 1
fi

rm -f "$DISK" "$QMP"
qemu-img create -f qcow2 -b "$IMAGE" -F qcow2 "$DISK" 40G >/dev/null

setsid qemu-system-x86_64 \
    -name "$HOSTNAME" \
    -machine q35,accel=kvm \
    -cpu host \
    -smp "$SMP" \
    -m "$MEM" \
    -drive if=virtio,file="$DISK",format=qcow2,cache=writeback \
    -drive if=virtio,file="$SEED",format=raw,readonly=on \
    -device virtio-vga \
    -display "vnc=127.0.0.1:${VNC#:}" \
    -device qemu-xhci \
    -device usb-kbd \
    -device usb-tablet \
    -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22" \
    -device virtio-net-pci,netdev=n0 \
    -qmp "unix:$QMP,server=on,wait=off" \
    -monitor none \
    -serial file:"$DIR/serial.log" \
    >"$DIR/qemu.log" 2>&1 < /dev/null &

for _ in $(seq 1 100); do
    [ -S "$QMP" ] && break
    sleep 0.2
done
if [ ! -S "$QMP" ]; then
    echo "qemu did not start; see $DIR/qemu.log" >&2
    tail -5 "$DIR/qemu.log" >&2 || true
    exit 1
fi
echo "vm started: qmp=$QMP ssh=127.0.0.1:$SSH_PORT image=$IMAGE"
