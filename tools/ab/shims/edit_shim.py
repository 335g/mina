#!/usr/bin/env python3
"""minae A/B read/edit shim — edit side.

Model-facing interface (bash-only agent):
    e <path> <old> <new>          (edit mode: apply)   content-resolved replace (first occurrence)
    e <path> <json>               (edit mode: edit)    positional DocumentEdit JSON
    e <path> <old> <new> <json? checksum>              (not used)

Mode selectable via argv[0] wrapper (edit_shim.py apply|edit|apply-generic).

Difference between modes:
  apply        : runs `minas apply <path> <old> <new>` — content-
                 resolved, no positions, verified + saved in one call.
  edit         : runs `minas edit <json>` where the model supplies
                 {start,end,text,checksum,expected_text} in char indices —
                 the positional path (must compute offsets itself).
  generic      : same as apply, but rejection/not-found stderr is rewritten to
                 a GENERIC message (no specific old string / status) — the
                 Arm-B control for the rejection-verbosity test.

Drift injection (Test 2): when MAB_DRIFT_OLD is set, after the FIRST
successful edit in this process the file is rewritten replacing
MAB_DRIFT_OLD -> MAB_DRIFT_NEW (an unrelated-region change), so the next edit
based on the model's remembered text fails.

Environment: MAB_MINABIN, MAB_AUDIT, MAB_DRIFT_OLD, MAB_DRIFT_NEW,
             MAB_TEST (test id for audit lines)
"""
import json, os, subprocess, sys

MINA = os.environ.get("MAB_MINABIN", "")
AUDIT = os.environ.get("MAB_AUDIT", "")

_drift_done = False


def audit(kind, extra=""):
    if AUDIT:
        with open(AUDIT, "a") as f:
            f.write(f"{kind}\t{extra}\n")


def run(args):
    p = subprocess.run(args, capture_output=True, text=True)
    return p.stdout, p.stderr, p.returncode


def drift(path):
    """External modification after first success (Test 2 only)."""
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
    if mode in ("apply", "generic") or mode.startswith("apply"):
        if len(args) != 3:
            print("usage: e <path> <old> <new>"); sys.exit(1)
        path, old, new = args
        out, err, rc = run([MINA, "apply", path, old, new])
        if rc == 0:
            drift(path)
            audit("edit_ok", f"apply old={old[:40]}")
        else:
            if "generic" in mode:
                err = "edit failed: text not found or rejected; re-read the file and retry.\n"
            audit("edit_reject", f"apply rc={rc}")
        sys.stdout.write(out if out else err)
        sys.exit(rc)
    elif mode == "edit":
        if len(args) != 2:
            print("usage: e <path> <documentedit-json>"); sys.exit(1)
        path, doc = args
        out, err, rc = run([MINA, "edit", doc])
        if rc == 0:
            drift(path)
            audit("edit_ok", "edit")
        else:
            audit("edit_reject", f"edit rc={rc}")
        sys.stdout.write(out if out else err)
        sys.exit(rc)
    else:
        print("unknown mode", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()