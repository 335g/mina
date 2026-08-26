#!/usr/bin/env python3
"""mina A/B shim — checksum query (positional-edit arm needs it).

    mcheck <path>
        opens <path> via mina and prints only the current checksum (one number),
        so the model can build a positional DocumentEdit JSON.
        Output is tiny (no full text) — keeps reads ranged.
Env: MAB_MINABIN
"""
import json
import os
import subprocess
import sys

MINA = os.environ.get("MAB_MINABIN", "")


def main():
    if len(sys.argv) < 2:
        print("usage: mcheck <path>"); sys.exit(1)
    # wrapper passes a mode placeholder as argv[1]; the path is the last arg
    path = sys.argv[-1]
    subprocess.run([MINA, "session", "exec", json.dumps({"Open": {"path": path}})],
                   capture_output=True, text=True)
    p = subprocess.run([MINA, "session", "get"], capture_output=True, text=True)
    if p.returncode != 0:
        print(p.stderr, file=sys.stderr); sys.exit(1)
    try:
        d = json.loads(p.stdout)
        print(d["checksum"])
    except Exception as e:
        print(f"checksum unavailable: {e}", file=sys.stderr); sys.exit(1)


if __name__ == "__main__":
    main()