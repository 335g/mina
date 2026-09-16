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
  pos          : the POSITIONAL path as minas actually implements it.
                 argv: pos <path> <json> with {start,end,expected_text,text}
                 (char offsets) — the model supplies the range. The shim runs
                 Open -> `minas edit --brief` -> Save (minas's positional path
                 does not persist on its own). The shim refreshes `checksum`
                 from the current file so the arm measures the model's OFFSET
                 CHOICE, not stale-checksum rejects: that is the stricter
                 (more forgiving) direction, so anything found here is a
                 lower bound on the positional path's failure.

Drift injection (Test 2): when MAB_DRIFT_OLD is set, after the FIRST
successful edit in this process the file is rewritten replacing
MAB_DRIFT_OLD -> MAB_DRIFT_NEW (an unrelated-region change), so the next edit
based on the model's remembered text fails.

Environment: MAB_MINABIN, MAB_AUDIT, MAB_DRIFT_OLD, MAB_DRIFT_NEW,
             MAB_TEST (test id for audit lines)
"""
import json, os, subprocess, sys, time

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


def _checksum(path):
    """現在のファイル全体の checksum（全文 FNV-1a64）。`minas read` の JSON から取る。"""
    out, _, _ = run([MINA, "read", path, "--lines", "1:1"])
    try:
        return json.loads(out)["checksum"]
    except (ValueError, KeyError):
        return 0


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
    elif mode == "pos":
        if len(args) != 2:
            print("usage: e pos <path> <json>"); sys.exit(1)
        path, raw = args
        try:
            doc = json.loads(raw)
        except ValueError:
            print("pos: bad json", file=sys.stderr); sys.exit(1)
        doc["checksum"] = _checksum(path)
        # 1 回だけやり直す: daemon は外部変更を非同期に検知して reload するので、
        # build_workdir 直後の read->edit が久しく競合して rc=2 になる。それは
        # モデルの位置取りではなく harness の競合なので、minas 自身の指示どおり
        # 「re-open してすぐやり直す」を 1 回だけ許す（expected_text はモデルのままなので
        # 本物の却下はやり直しても通らない）。
        for attempt in (1, 2):
            doc["checksum"] = _checksum(path)
            run([MINA, "exec", "--brief", json.dumps({"Open": {"path": path}})])
            out, err, rc = run([MINA, "edit", "--path", path, "--brief", json.dumps(doc)])
            if rc == 0 or attempt == 2:
                break
            audit("edit_retry", f"pos rc={rc}")
            time.sleep(0.5)
        if rc == 0:
            run([MINA, "exec", "--brief", '"Save"'])
            audit("edit_ok", f"pos start={doc.get('start')} end={doc.get('end')}")
        else:
            audit("edit_reject", f"pos rc={rc}")
        print((out or err)[:2000])
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