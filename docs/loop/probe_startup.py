#!/usr/bin/env python3
"""iteration #13 用: `minas` の起動固定費を debug と release で測る。

`minas --help`（clap の構築 + help 描画。LSP・daemon に触らない）と
`minas info`（起動 + 接続 + 1 往復）を n 回計時して中央値を出す。debug と release を
**同一セッションで交互に**測る（順序の偏りを消す）。daemon は 1 個だけ立てて両方で
共有する（デーモン側の応答は 0.27ms なので、測っているのはクライアントの固定費）。

使い方:
    cargo build --release
    python3 docs/loop/probe_startup.py            # debug vs release（n=5）
    python3 docs/loop/probe_startup.py 9          # n=9
注意: daemon は専用 TMPDIR に立てて最後に殺す（stray を残さない）。
"""
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HELLO = json.dumps({"kind": "headless", "reset_cursor_on_disconnect": True,
                    "name": "startup-probe"}) + "\n"


def timeit(cmd: list[str], n: int, env: dict) -> list[float]:
    out = []
    for _ in range(n):
        t0 = time.perf_counter()
        subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       cwd=ROOT, env=env)
        out.append((time.perf_counter() - t0) * 1000)
    return out


def main() -> int:
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 5
    debug = os.environ.get("L0_MINAS") or str(ROOT / "target/debug/minas")
    release = os.environ.get("L0_MINAS") or str(ROOT / "target/release/minas")
    minad = os.environ.get("L0_MINAD") or str(ROOT / "target/debug/minad")
    for label, b in (("debug", debug), ("release", release)):
        if not Path(b).exists():
            print(f"{label}: {b} が無い（cargo build / cargo build --release）")
            return 1
    # 計測用の daemon（専用 TMPDIR。RA は spawn しない — `info` はデーモンだけ）
    tmpdir = Path(tempfile.mkdtemp(prefix="minas-startup-"))
    env = dict(os.environ, TMPDIR=str(tmpdir))
    proc = subprocess.Popen([minad, "serve"], cwd=ROOT, env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        if list(tmpdir.glob("minae-*.sock")):
            break
        time.sleep(0.05)
    else:
        proc.kill()
        raise SystemExit("daemon socket did not appear")
    try:
        print(f"# debug={debug}\n# release={release}\n# n={n}（debug と release を交互に）")
        for args in (["--help"], ["info"]):
            d, r = [], []
            for _ in range(n):  # 交互に測る（順序の偏りを消す）
                d += timeit([debug, *args], 1, env)
                r += timeit([release, *args], 1, env)
            print(f"minas {' '.join(args):10} debug={statistics.median(d):7.1f}ms  "
                  f"release={statistics.median(r):7.1f}ms  "
                  f"（debug {[round(x,1) for x in d]} / release {[round(x,1) for x in r]}）")
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()
        shutil.rmtree(tmpdir, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
