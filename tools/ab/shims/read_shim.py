#!/usr/bin/env python3
"""mina A/B read/edit shim — read side.

Model-facing interface (bash-only agent):
    r <path> [start:end]
        read lines of <path> via mina `session get --lines` (numbered lines).
        Without a range, prints a short head (first 25 lines) to avoid dumping
        the whole file (the model must use ranges — that is the point).
        Out-of-range start prints the explained zero result (Q3/R1).

Mode selectable via argv[0] wrapper (read_shim.py range|full ...).
Mode "full": `mina session get` full snapshot is printed instead (for the
Arm-B comparison) — forces whole-file reads.

Environment:
    MAB_MINABIN   path to the mina binary
    MAB_AUDIT     audit log path (append lines)
"""
import json, os, subprocess, sys

MINA = os.environ.get("MAB_MINABIN", "")
AUDIT = os.environ.get("MAB_AUDIT", "")


def audit(kind, path, extra):
    if AUDIT:
        with open(AUDIT, "a") as f:
            f.write(f"{kind}\t{path}\t{extra}\n")


def m(args, binary=False):
    p = subprocess.run(args, capture_output=True, text=True)
    return p.stdout, p.stderr, p.returncode


def open_file(path):
    return m([MINA, "session", "exec", json.dumps({"Open": {"path": path}})])


def main():
    mode = sys.argv[1]
    args = sys.argv[2:]
    if len(args) < 1:
        print("usage: r <path> [start:end]"); sys.exit(1)
    path = args[0]
    rng = args[1] if len(args) > 1 else None
    open_file(path)
    if mode == "full" and rng is None:
        out, err, rc = m([MINA, "session", "get"])
        size = len(out)
        print(out, end="")
    elif rng is None:
        # head only: 25 lines (discourages dumping; ranges are the contract)
        out, err, rc = m([MINA, "session", "get", "--lines", "1:25"])
        size = len(out)
        print(out, end="")
        audit("read_head", path, f"bytes={size}")
    else:
        out, err, rc = m([MINA, "session", "get", "--lines", rng])
        size = len(out)
        print(out, end="")
        audit("read", path, f"range={rng} bytes={size}")
    if rc != 0 and err.strip():
        print(err, file=sys.stderr)


if __name__ == "__main__":
    main()