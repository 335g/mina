#!/usr/bin/env python3
"""minae A/B shim — semantic rename via minae's OWN `session rename` (M2, ADR-0029).

    mrename <path> <old> <new>
        Runs `minas rename <path> <old> <new>`: content-addressed — minae
        resolves <old> to the first identifier occurrence (comments/strings
        excluded), the language server (rust-analyzer) rewrites every reference
        across files, and the result is applied and saved to disk. Output is
        minae's own impact line:
            renamed: <old> -> <new> (N files, M edits)
            changed: <abs path>...
        Passthrough of minae's exit code: 0 success / 1 input error (not
        supported, bad args — do not retry) / 2 retryable (symbol not found,
        LSP error, analysis incomplete — re-read and retry).
"""
import os
import subprocess
import sys

MINA = os.environ.get("MAB_MINABIN", "minae")
AUDIT = os.environ.get("MAB_AUDIT", "")


def audit(kind, extra=""):
    if AUDIT:
        with open(AUDIT, "a") as f:
            f.write(f"{kind}\t{extra}\n")


def main():
    if len(sys.argv) < 4:
        print("usage: mrename <path> <old> <new>", file=sys.stderr)
        sys.exit(1)
    path, old, new = sys.argv[1:4]
    r = subprocess.run([MINA, "rename", path, old, new],
                       capture_output=True, text=True)
    if r.returncode == 0:
        audit("rename", f"{old}->{new} ok")
        out = r.stdout.strip()
        print(out)
        # minae prints the impact on stdout; stderr may carry a dirty note — keep it
        if r.stderr.strip():
            print(r.stderr.strip(), file=sys.stderr)
    else:
        msg = (r.stderr or r.stdout).strip() or "rename failed"
        audit("rename_reject", f"{old}->{new} rc={r.returncode} {msg[:120]}")
        print(msg, file=sys.stderr)
    sys.exit(r.returncode)


if __name__ == "__main__":
    main()