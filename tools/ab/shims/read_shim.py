#!/usr/bin/env python3
"""minae A/B read/edit shim — read side.

Model-facing interface (bash-only agent):
    r <path> [start:end]
        read lines of <path> via minas `read --lines` (numbered lines JSON).
        Without a range, prints a short head (first 25 lines) to avoid dumping
        the whole file (the model must use ranges — that is the point).
        Out-of-range start prints the explained zero result (Q3/R1).

Mode selectable via argv[0] wrapper (read_shim.py range|range_off|full ...).
Mode "full": `minas read` full text is printed instead (for the
Arm-B comparison) — forces whole-file reads.
Mode "range_off": same as range, plus each line carries `off` = the char offset
where that line starts (computed locally from the file — no extra tokens). It
exists for the positional arm, whose edit tool takes char offsets: without it
the arm would measure "can the model count characters from scratch" instead of
"does a position-addressed edit land where the model intended".

Environment:
    MAB_MINABIN   path to the minae binary
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


def add_offsets(full_json, ranged_json):
    """行頭 char offset を `off` として足す。

    オフセットは **`minas read` が返した全文** から計算する（ディスクからではない）。
    `minas read` は daemon がそのファイルを開いていれば buffer を返すので、ディスクを
    見るとモデルが見たテキストとずれたオフセットを渡してしまう — それでは「モデルの
    位置取り」ではなく「harness のバグ」を測ってしまう。
    """
    try:
        full = json.loads(full_json)
        doc = json.loads(ranged_json)
    except ValueError:
        return ranged_json
    starts, off = {}, 0
    for ln in full.get("lines", []):
        starts[ln["n"]] = off
        off += len(ln["text"]) + 1
    for ln in doc.get("lines", []):
        ln["off"] = starts.get(ln["n"], -1)
    return json.dumps(doc, indent=1, ensure_ascii=False)


def main():
    mode = sys.argv[1]
    args = sys.argv[2:]
    if len(args) < 1:
        print("usage: r <path> [start:end]"); sys.exit(1)
    path = args[0]
    rng = args[1] if len(args) > 1 else None
    if mode == "full" and rng is None:
        out, err, rc = m([MINA, "read", path])
        size = len(out)
        print(out, end="")
    elif rng is None:
        # head only: 25 lines (discourages dumping; ranges are the contract)
        out, err, rc = m([MINA, "read", path, "--lines", "1:25"])
        size = len(out)
        print(out, end="")
        audit("read_head", path, f"bytes={size}")
    else:
        out, err, rc = m([MINA, "read", path, "--lines", rng])
        size = len(out)
        if mode == "range_off":
            # `minas read` をそのまま呼ぶと全文テキスト（JSON ではない）なので `--lines 1:`
            full, _, _ = m([MINA, "read", path, "--lines", "1:"])
            out = add_offsets(full, out)
        print(out, end="")
        audit("read", path, f"range={rng} bytes={size}")
    if rc != 0 and err.strip():
        print(err, file=sys.stderr)


if __name__ == "__main__":
    main()