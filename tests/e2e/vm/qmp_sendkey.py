#!/usr/bin/env python3
"""Inject a key chord into the VM through QMP, with explicit press/release events.

QMP `input-send-event` with qcodes is the exact equivalent of a physical keyboard:
the guest kernel turns these into evdev events for libinput, so mutter handles them
like any other key press. This is what the container lane could not do (no seat
input in a headless container shell).
"""

import argparse
import json
import socket
import time

# evdev/QKeyCode names, not X keysyms: the super key is meta_l.
QCODES = {
    "ctrl": "ctrl",
    "super": "meta_l",
    "alt": "alt",
    "shift": "shift",
    "z": "z",
    "x": "x",
    "c": "c",
}


def qmp_call(sock, obj, reply=True):
    sock.sendall((json.dumps(obj) + "\n").encode())
    if not reply:
        return None
    buffer = b""
    while not buffer.endswith(b"\n"):
        chunk = sock.recv(4096)
        if not chunk:
            raise RuntimeError("QMP closed the connection")
        buffer += chunk
    return json.loads(buffer)


def inject(path, chord, hold=0.08):
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(10)
    sock.connect(path)
    sock.recv(4096)  # greeting
    qmp_call(sock, {"execute": "qmp_capabilities"})
    keys = [QCODES[part] for part in chord]
    events = []
    for key in keys:
        events.append({"type": "key", "data": {"down": True,
                                               "key": {"type": "qcode", "data": key}}})
    for key in reversed(keys):
        events.append({"type": "key", "data": {"down": False,
                                               "key": {"type": "qcode", "data": key}}})
    replies = []
    for event in events[:len(keys)]:
        replies.append(qmp_call(sock, {"execute": "input-send-event",
                                       "arguments": {"events": [event]}}))
        time.sleep(0.02)
    time.sleep(hold)
    for event in events[len(keys):]:
        replies.append(qmp_call(sock, {"execute": "input-send-event",
                                       "arguments": {"events": [event]}}))
        time.sleep(0.02)
    sock.close()
    return keys, replies


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--qmp", default="/tmp/sizer-vm/qmp.sock")
    parser.add_argument("--chord", default="ctrl+super+z")
    parser.add_argument("--hold", type=float, default=0.08)
    args = parser.parse_args()
    keys, replies = inject(args.qmp, args.chord.split("+"), args.hold)
    print(json.dumps({"keys": keys, "replies": replies}))


if __name__ == "__main__":
    main()
