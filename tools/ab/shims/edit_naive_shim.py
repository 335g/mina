#!/usr/bin/env python3
"""minae A/B edit shim — naive (blind replace) edit side.

Model-facing interface (bash-only agent):
    e <path> <old> <new>
        Replace the FIRST occurrence of <old> with <new> directly on disk.
        - reads the file at call time (no snapshot, no generation, no checksum)
        - <old> NOT found   -> "text not found" error, exit 1
        - multiple matches  -> replaces the first, prints a warning
        - the file may have changed underneath since the model's last read;
          nothing verifies that (silent corruption is possible)

Drift injection (mirrors edit_shim.py): when MAB_DRIFT_OLD is set, after the
FIRST successful edit in this process the file is rewritten replacing
MAB_DRIFT_OLD -> MAB_DRIFT_NEW (an unrelated-region external change), so the
next edit based on the model's remembered text fails to find its anchor.

Environment:
    MAB_AUDIT, MAB_DRIFT_OLD, MAB_DRIFT_NEW
"""
import os
import sys

AUDIT = os.environ.get("MAB_AUDIT", "")

_drift_done = False


def audit(kind, extra=""):
    if AUDIT:
        with open(AUDIT, "a") as f:
            f.write(f"{kind}\t{extra}\n")


def drift(path):
    """External modification after first success (T11 only)."""
    global _drift_done
    old = os.environ.get("MAB_DRIFT_OLD")
    new = os.environ.get("MAB_DRIFT_NEW")
    if _drift_done or not old:
        return
    _drift_done = True
    try:
        s = open(path).read()
        if old in s:
            open(path, "w").write(s.replace(old, new, 1))
            audit("drift", f"{old}->{new}")
    except OSError:
        pass


def main():
    mode = sys.argv[1]
    args = sys.argv[2:]
    if len(args) != 3:
        print("usage: e <path> <old> <new>"); sys.exit(1)
    path, old, new = args
    try:
        s = open(path).read()
    except OSError as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
    cnt = s.count(old)
    if cnt == 0:
        print("error: text not found (re-read the file and retry)", file=sys.stderr)
        audit("edit_reject", "naive not_found")
        sys.exit(1)
    s2 = s.replace(old, new, 1)
    open(path, "w").write(s2)
    if cnt > 1:
        print(f"warning: {cnt} matches; replaced the first", file=sys.stderr)
    audit("edit_ok", "naive")
    drift(path)


if __name__ == "__main__":
    main()