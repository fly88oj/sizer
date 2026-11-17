#!/usr/bin/env python3
"""Unit tests for the harness' applied-result matching.

The case that motivated this: picking the same preset twice. `LastResult` still
holds the previous pick's `exact:true`, so a wait that only compares labels reports
the old outcome as the new one — a stale green. No shell and no D-Bus here: the
D-Bus reads are replaced by scripted replies.
"""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "e2e"))
import gnome_extension as G  # noqa: E402

FAILURES = []


def check(name, condition, detail=""):
    if condition:
        print(f"ok   {name}")
        return
    FAILURES.append(name)
    print(f"FAIL {name} {detail}")


def result(generation, exact, label="same", rect=None):
    rect = rect or {"x": 0, "y": 32, "w": 800, "h": 600}
    return {"label": label, "generation": generation, "exact": exact,
            "requested_client": dict(rect), "achieved_client": dict(rect)}


def feed(replies):
    """Replace the harness' D-Bus reads with a scripted sequence of LastResult values."""
    state = {"index": 0}

    def fake(method, args=(), timeout_ms=None):
        if method == "LastResult":
            index = min(state["index"], len(replies) - 1)
            state["index"] += 1
            return replies[index]
        if method == "GetWindow":
            return {"window": {"id": "w", "maximized": 0,
                               "client": {"x": 0, "y": 32, "w": 800, "h": 600},
                               "frame": {"x": 14, "y": 44, "w": 772, "h": 571}}}
        return {}

    G.call_quiet = fake


def main():
    previous = result(4, True)
    pending = result(5, None)
    applied = result(5, True)
    other_rect = result(5, True, rect={"x": 0, "y": 32, "w": 430, "h": 330})

    check("the stale result is not this request's outcome",
          G.applied_result(previous, label="same", generation=5) is False)
    check("the same generation matches",
          G.applied_result(previous, label="same", generation=4) is True)
    check("a result without a measurement never matches",
          G.applied_result(pending, label="same", generation=5) is False)
    check("a different label does not match",
          G.applied_result(applied, label="other", generation=5) is False)
    check("a different requested rect does not match",
          G.applied_result(other_rect, label="same", generation=5,
                           requested={"x": 0, "y": 32, "w": 800, "h": 600}) is False)
    check("the requested rect matches",
          G.applied_result(applied, label="same", generation=5,
                           requested={"x": 0, "y": 32, "w": 800, "h": 600}) is True)

    # Two picks of the same preset: the second one never lands within the deadline.
    feed([previous, pending, pending, previous])
    second, window = G.wait_applied("w", time.time() + 0.3, label="same",
                                    activated={"generation": 5})
    check("a same-label request that never lands reports nothing",
          second == {}, f"got {second}")
    check("the window is still readable while waiting", isinstance(window, dict))

    feed([previous, pending, applied])
    landed, window = G.wait_applied("w", time.time() + 1.0, label="same",
                                    activated={"generation": 5})
    check("the request lands once its own generation reports",
          landed.get("exact") is True and landed.get("generation") == 5, f"got {landed}")
    check("preset_landed accepts the matching window",
          G.preset_landed({"ok": True}, landed, window) is True)

    print(f"SUMMARY failures={len(FAILURES)}")
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())
