#!/usr/bin/env python3
"""mina A/B read shim — naive (full-file) read side.

Model-facing interface (bash-only agent):
    r <path>
        prints the ENTIRE file — the generic-agent "cat" contract. No line
        numbers, no ranges, no head cap: the model always pays for the whole
        file, and cannot target a re-read at a region.

Mode selectable via argv[0] wrapper (read_naive_shim.py x ...).

Environment:
    MAB_AUDIT   audit log path (append lines)
"""
import os
import sys

AUDIT = os.environ.get("MAB_AUDIT", "")


def audit(kind, path, extra):
    if AUDIT:
        with open(AUDIT, "a") as f:
            f.write(f"{kind}\t{path}\t{extra}\n")


def main():
    mode = sys.argv[1]
    args = sys.argv[2:]
    if len(args) < 1:
        print("usage: r <path>"); sys.exit(1)
    path = args[0]
    try:
        s = open(path).read()
    except OSError as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
    size = len(s.encode())
    print(s, end="")
    audit("read_full", path, f"bytes={size}")


if __name__ == "__main__":
    main()