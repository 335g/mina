#!/usr/bin/env python3
"""rust-analyzer の `$/progress` を時刻つきでダンプする（索引完走の目印を確認する道具）。

ADR-0051 の根拠を得た計測:
- `window.workDoneProgress` を advertise **しないと 1 件も送られない**
  （`HOW=no-progress` で比較できる）
- `rustAnalyzer/Fetching` → `Building CrateGraph` → `Roots Scanned` →
  `cachePriming`(title "Indexing") の順に begin/end が来る。
  `cachePriming` の end が「ワークスペース索引完走」の目印
  （最初に 0% で即 end する空回しが 1 回あり、本番の begin が続く — 静止確認が要る理由）

usage:
  python3 docs/loop/probe_progress.py <fixture-dir> [seconds] [HOW]
    HOW = progress（既定）| no-progress（advertise しない）
  fixture-dir に Cargo.toml が無ければ同ディレクトリの l0.py の fixture を生成する。
"""
import json
import subprocess
import sys
import threading
import time
from pathlib import Path


def ensure_fixture(root: Path) -> None:
    if (root / "Cargo.toml").is_file():
        return
    sys.path.insert(0, str(Path(__file__).resolve().parent))  # 同じディレクトリの l0.py
    import l0  # noqa: E402

    root.mkdir(parents=True, exist_ok=True)
    l0.fixture_rust(root)
    print(f"# fixture を生成: {root}", file=sys.stderr)


def main() -> int:
    root = Path(sys.argv[1]).resolve()
    seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 10.0
    how = sys.argv[3] if len(sys.argv) > 3 else "progress"
    ensure_fixture(root)

    proc = subprocess.Popen(["rust-analyzer"], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    t0 = time.time()

    def send(msg):
        b = json.dumps(msg).encode()
        proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
        proc.stdin.flush()

    def reader():
        f = proc.stdout
        while True:
            n = None
            while True:
                line = f.readline()
                if not line:
                    return
                line = line.strip()
                if not line:
                    break
                if line.startswith(b"Content-Length:"):
                    n = int(line.split(b":")[1])
            if n is None:
                return
            msg = json.loads(f.read(n))
            t = time.time() - t0
            if msg.get("method") == "$/progress":
                p = msg["params"]
                v = p["value"]
                print(f"{t:7.2f}s PROGRESS {p['token']!r} {v.get('kind'):<6} "
                      f"title={v.get('title')!r} pct={v.get('percentage')} msg={v.get('message')!r}")
            elif "id" in msg and "method" not in msg:
                print(f"{t:7.2f}s RESP id={msg['id']}")
            else:
                print(f"{t:7.2f}s {msg.get('method')}")

    threading.Thread(target=reader, daemon=True).start()

    caps = {"textDocument": {
        "publishDiagnostics": {"relatedInformation": False},
        "inlayHint": {"dynamicRegistration": False},
        "documentSymbol": {"hierarchicalDocumentSymbolSupport": True}}}
    if how == "progress":
        caps["window"] = {"workDoneProgress": True}
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "processId": None, "rootUri": "file://" + str(root),
        "capabilities": caps, "positionEncodings": ["utf-8", "utf-16"]}})
    time.sleep(0.3)
    send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    time.sleep(seconds)
    proc.kill()
    return 0


if __name__ == "__main__":
    sys.exit(main())
