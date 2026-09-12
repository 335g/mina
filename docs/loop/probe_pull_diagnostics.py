#!/usr/bin/env python3
"""編集後に `textDocument/diagnostic`（pull）を繰り返し、
「どのエラーが pull に見えるか」と「何回目で安定するか」を測る道具。

ADR-0052 の根拠を得た計測（2026-09-12）:
- 編集直後の **round0 で最終集合が返る**（round0 == round11 = 待つ意味が無い）
- pull が見る: 構文エラー（同じファイル）/ 型エラー（`no such field` 等）
- pull が見ない: **メソッド解決エラー**（`cfg.validate2()`）、クロスファイル型エラー
  → 空応答は「クリーンの根拠にならない」（ADR-0045 の `settled:false` が要る理由）

ケースごとに**新しい fixture と新しい rust-analyzer**を使う（前ケースの変更が
残ると診断が混ざり、誤診する — 実際に踏んだ落とし穴）。

usage:
  python3 docs/loop/probe_pull_diagnostics.py <work-dir> [rounds] [gap_sec]
  work-dir はケースごとのサブディレクトリを作る親（空でなくてよい）。
"""
import json
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

ROUNDS = 12
GAP = 0.25
IDENT = "rust-analyzer"

CASES = [
    ("T1 型エラー（メソッド解決: cfg.validate2()）", "src/main.rs", "cfg.validate()", "cfg.validate2()"),
    ("T2 構文エラー（main.rs）", "src/main.rs", "fn main() {", "fn main( {"),
    ("T3 フィールド削除（config.rs）", "src/config.rs", "    pub timeout_ms: u64,\n", ""),
    ("T4 未知フィールド（config.rs）", "src/config.rs", "verbose: false }", "verbose: false, extra: 1 }"),
]


def build_fixture(dest: Path) -> None:
    sys.path.insert(0, str(Path(__file__).resolve().parent))  # 同じディレクトリの l0.py
    import l0  # noqa: E402

    dest.mkdir(parents=True, exist_ok=True)
    l0.fixture_rust(dest)


def uri(root: Path, rel: str) -> str:
    return f"file://{root}/{rel}"


class Ra:
    def __init__(self, root: Path):
        self.root = root
        self.p = subprocess.Popen(["rust-analyzer"], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.notes, self.id, self.pending = [], 0, {}
        self.lock = threading.Lock()
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        f = self.p.stdout
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
            if "id" in msg and "method" not in msg:
                with self.lock:
                    ev = self.pending.pop(msg["id"], None)
                if ev is not None:
                    ev.append(msg.get("result"))
                continue
            with self.lock:
                self.notes.append((msg.get("method"), msg.get("params") or {}))

    def send(self, msg):
        b = json.dumps(msg).encode()
        self.p.stdin.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
        self.p.stdin.flush()

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method, params, timeout=15.0):
        self.id += 1
        i = self.id
        ev = []
        with self.lock:
            self.pending[i] = ev
        self.send({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
        end = time.time() + timeout
        while time.time() < end:
            if ev:
                return ev[0]
            time.sleep(0.005)
        return None

    def _outstanding(self):
        with self.lock:
            notes = list(self.notes)
        out, seen = set(), False
        for m, p in notes:
            if m != "$/progress":
                continue
            seen = True
            kind = (p.get("value") or {}).get("kind")
            if kind == "begin":
                out.add(p.get("token"))
            elif kind == "end":
                out.discard(p.get("token"))
        return seen, out

    def wait_idle(self, timeout=40.0):
        """索引完走を待つ（outstanding が空 + 静止確認）。daemon の Progress と同じ規則。"""
        seen, end = False, time.time() + timeout
        while time.time() < end:
            s, out = self._outstanding()
            seen = seen or s
            if seen and not out:
                time.sleep(0.3)
                if not self._outstanding()[1]:
                    return True
            time.sleep(0.05)
        return False

    def did_change(self, rel, text):
        self.notify("textDocument/didChange", {
            "textDocument": {"uri": uri(self.root, rel), "version": 2},
            "contentChanges": [{"text": text}]})

    def pull(self, rel):
        r = self.request("textDocument/diagnostic",
                         {"textDocument": {"uri": uri(self.root, rel)}, "identifier": IDENT})
        if not isinstance(r, dict):
            return None
        return [((i.get("message") or "").split("\n")[0][:44]) for i in (r.get("items") or [])]


def run_case(work: Path, label: str, rel: str, old: str, new: str, rounds: int, gap: float) -> None:
    root = work / f"case-{abs(hash(label)) % 100000}"
    if root.exists():
        shutil.rmtree(root)
    build_fixture(root)

    texts = {r: (root / r).read_text() for r in ("src/config.rs", "src/main.rs")}
    mutated = texts[rel].replace(old, new)
    if mutated == texts[rel]:
        print(f"[{label}] mutation が一致しない — fixture の形が変わった？")
        return

    ra = Ra(root)
    ra.request("initialize", {"processId": None, "rootUri": f"file://{root}",
                              "capabilities": {"window": {"workDoneProgress": True},
                                               "textDocument": {
                                                   "publishDiagnostics": {"relatedInformation": False},
                                                   "inlayHint": {"dynamicRegistration": False},
                                                   "documentSymbol": {"hierarchicalDocumentSymbolSupport": True}}},
                              "positionEncodings": ["utf-8", "utf-16"]})
    ra.notify("initialized", {})
    for r, t in texts.items():
        ra.notify("textDocument/didOpen", {"textDocument": {
            "uri": uri(root, r), "languageId": "rust", "version": 1, "text": t}})
    if not ra.wait_idle():
        print(f"[{label}] 索引完走を待てなかった")
        ra.p.kill()
        return

    (root / rel).write_text(mutated)
    ra.did_change(rel, mutated)

    first_main = None
    counts = []
    for i in range(rounds):
        main = ra.pull("src/main.rs")
        conf = ra.pull("src/config.rs")
        if main and first_main is None:
            first_main = i
        counts.append((len(main), len(conf)))
        print(f"[{label}] round{i} main={main} config={conf}")
        if i == 0 and main is None:
            break
        time.sleep(gap)
    stable = len(set(counts)) == 1
    print(f"[{label}] → main.rs の初出非空: round {first_main} / {rounds} 回で"
          f"{'安定（round0 で確定）' if stable else '変化あり'}")
    print()
    ra.p.kill()


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    work = Path(sys.argv[1]).resolve()
    rounds = int(sys.argv[2]) if len(sys.argv) > 2 else ROUNDS
    gap = float(sys.argv[3]) if len(sys.argv) > 3 else GAP
    work.mkdir(parents=True, exist_ok=True)
    for case in CASES:
        run_case(work, *case, rounds, gap)
    return 0


if __name__ == "__main__":
    sys.exit(main())
