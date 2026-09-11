#!/usr/bin/env python3
"""L0 — LLM を使わない決定的な minas フロー費用計測（ループエンジニアリングの内側ループ）。

なぜ L0 が要るか: `tools/ab`（実LLM A/B = L2）は分単位・高分散・モデル依存で、
サブコマンドや契約のイテレーションには遅すぎる。L0 は「契約が要求する最小の往復数と
出力量」を数秒で測る。**L0 で差が出ない仮説は L2 を回さない** — これが無駄の最大の削減。

指標（外側のトークン費用の近似）:
  calls     エージェントが払う往復数。各往復はコンテキスト全体を再送するので支配項。
  out_B     フロー総出力バイト（ツール応答 = コンテキストに残る量）。
  equiv_B   トークン等価量 = Σ o_i + Σ (T-i)·o_i。**序盤の大出力ほど高い**（再送される回数が多い）。
  wall_ms   壁時計。LSP ロード・check の settle 待ち・cargo を含む（実時間の費用）。
  fails     exit != 0 のステップ数 = 回復（迷い）の発生源。

使い方:
  python3 tools/loop/l0.py                          # 全フロー
  python3 tools/loop/l0.py -f rename -v             # フロー絞り込み + ステップ別内訳
  python3 tools/loop/l0.py -f verify -n "仮説: ..." --log   # docs/benchmarks/agent-loop-log.md へ追記

計測器の分離: アームごとに専用 TMPDIR（= 専用 socket）で `minad serve` を起動し、
その daemon の `minas info` 前後差分を取る。グローバルな daemon と混ざらない。
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LOG_PATH = REPO / "docs" / "benchmarks" / "agent-loop-log.md"
OUT_DIR = REPO / "tmp" / "loop"


def resolve(name: str, env_key: str) -> str:
    """iter 中の self-build を測れるよう、先に target/debug、無ければ PATH。"""
    override = os.environ.get(env_key)
    if override:
        return override
    local = REPO / "target" / "debug" / name
    return str(local) if local.is_file() else name


MINAS = resolve("minas", "L0_MINAS")
MINAD = resolve("minad", "L0_MINAD")

# ---------------------------------------------------------------- fixtures


def fixture_rust(root: Path) -> dict:
    """LSP が効く単体クレート。`[workspace]` は親 workspace へのネストを防ぐ（T4/T5 の実測知見）。"""
    pad = "\n\n".join(
        f"/// Padding struct {i}.\n#[derive(Debug, Default)]\npub struct Pad{i} {{\n"
        f"    pub v: u64,\n}}\n\nimpl Pad{i} {{\n"
        f"    /// Increment the counter by {i}.\n    pub fn bump(&mut self) {{\n"
        f"        self.v += {i};\n    }}\n}}"
        for i in range(1, 21)
    )
    config = f'''/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {{
    pub timeout_ms: u64,
    pub max_retries: u32,
    pub verbose: bool,
}}

impl Default for Config {{
    fn default() -> Self {{
        Self {{ timeout_ms: 5000, max_retries: 3, verbose: false }}
    }}
}}

impl Config {{
    /// Reject obviously-bad configurations before use.
    pub fn validate(&self) -> Result<(), String> {{
        if self.timeout_ms == 0 {{
            return Err("timeout must be > 0".into());
        }}
        if self.max_retries > 100 {{
            return Err("too many retries".into());
        }}
        Ok(())
    }}
}}

{pad}
'''
    main = """mod config;

use config::Config;

fn main() {
    let cfg = Config::default();
    cfg.validate().expect("bad config");
    println!(
        "timeout={} retries={} verbose={}",
        cfg.timeout_ms, cfg.max_retries, cfg.verbose
    );
}
"""
    files = {
        "Cargo.toml": '[workspace]\n\n[package]\nname = "l0fix"\nversion = "0.1.0"\nedition = "2021"\n',
        "src/config.rs": config,
        "src/main.rs": main,
    }
    for rel, text in files.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)

    lines = config.splitlines()
    vline = next(i for i, l in enumerate(lines, 1) if "fn validate" in l)
    vcol = lines[vline - 1].index("fn validate") + 4  # keep on the same line
    return {
        "files": sorted(files),
        "validate_line": vline,
        "validate_pos": f"{vline}:{vcol}",
        "validate_span": f"{vline}:{vline + 10}",
        "probe": "minas symbol src/main.rs Pad1",
        "probe_need": "Pad1",
    }


FIXTURES = {"rust": fixture_rust}

# ---------------------------------------------------------------- flows
# step は `sh -c` で実行する（パイプ・stdin を使えるため）。{meta} で位置を注入する。
# `warmup: True` のフローは、「LSP がワークスペースを読み終えて応答が安定する」まで待ってから計測する
# （--cold で省略）。現行 daemon は rust-analyzer の進捗を待たないため、cold では symbol/rename/check が
# 静かに誤る（L0 実測 2026-09-12）。warm を既定にし、cold 税は --cold で別途測る。

FLOWS = {
    "explore": {
        "goal": "Config::validate の本体を読み、変更対象の行範囲を確定する",
        "fixture": "rust",
        "warmup": True,
        "need": "too many retries",  # 目的の情報を実際に受け取ったか
        "arms": {
            # 契約どおりの探索: 構造（symbol/at）→ 必要な範囲だけ読む
            "lsp": [
                "minas symbol src/main.rs validate",
                "minas at src/config.rs {validate_pos}",
                "minas read src/config.rs --lines {validate_span}",
            ],
            # 素朴な契約: 全文ダンプ
            "dump": [
                "minas read src/config.rs",
                "minas read src/main.rs",
            ],
        },
        # step ごとに「受け取れているべき文字列」。空応答 + exit 0（無言の誤り）を検出する。
        # cold では symbol が [] を返す（L0 実測 2026-09-12）— exit 0 なので rc では見えない。
        "expect": {"lsp": ["validate", "validate", "validate"]},
    },
    "verify": {
        "goal": "timeout_ms を 5000 → 30000 に変更し、検証する",
        "fixture": "rust",
        "warmup": True,
        "verify": 'cargo check --offline && grep -q "timeout_ms: 30000" src/config.rs',
        "arms": {
            # minas の編集→検証ループ
            "apply-check": [
                'minas apply src/config.rs "timeout_ms: 5000" "timeout_ms: 30000"',
                "minas check src/config.rs",
            ],
            # 検証を cargo に委ねる
            "apply-cargo": [
                'minas apply src/config.rs "timeout_ms: 5000" "timeout_ms: 30000"',
                "cargo check --offline",
            ],
            # 複数編集を 1 往復に畳む（--hunks-stdin）
            "hunks-cargo": [
                "printf '%s' '[{\"old\":\"timeout_ms: 5000\",\"new\":\"timeout_ms: 30000\"},"
                "{\"old\":\"max_retries: 3\",\"new\":\"max_retries: 5\"}]'"
                " | minas apply src/config.rs --hunks-stdin",
                "cargo check --offline",
            ],
            # 同じ 2 箇所を 1 つずつ（往復削減の対照群）
            "apply2-cargo": [
                'minas apply src/config.rs "timeout_ms: 5000" "timeout_ms: 30000"',
                'minas apply src/config.rs "max_retries: 3" "max_retries: 5"',
                "cargo check --offline",
            ],
        },
    },
    "rename": {
        "goal": "max_retries を retry_count へ全箇所リネームする",
        "fixture": "rust",
        "warmup": True,
        "verify": '! grep -rq max_retries src && cargo check --offline',
        "arms": {
            "lsp": [
                "minas rename src/config.rs max_retries retry_count",
                "cargo check --offline",
            ],
            "apply": [
                'minas apply src/config.rs "pub max_retries: u32" "pub retry_count: u32"',
                'minas apply src/config.rs "max_retries: 3" "retry_count: 3"',
                'minas apply src/config.rs "self.max_retries" "self.retry_count"',
                'minas apply src/main.rs "cfg.max_retries" "cfg.retry_count"',
                "cargo check --offline",
            ],
        },
    },
}

# ---------------------------------------------------------------- runner


def start_daemon(cwd: Path, env: dict) -> subprocess.Popen:
    log = Path(env["TMPDIR"]) / "minad.log"
    with log.open("wb") as fh:
        proc = subprocess.Popen(
            [MINAD, "serve"], cwd=cwd, env=env, stdout=fh, stderr=subprocess.STDOUT,
        )
    tmp = Path(env["TMPDIR"])
    for _ in range(200):
        if list(tmp.glob("minae-*.sock")):
            return proc
        if proc.poll() is not None:
            raise RuntimeError(f"minad exited early (rc={proc.returncode}): {log.read_text()[:400]}")
        time.sleep(0.05)
    proc.kill()
    raise RuntimeError("daemon socket did not appear in 10s")


def run(cmd: str, cwd: Path, env: dict) -> dict:
    t0 = time.perf_counter()
    p = subprocess.run(cmd, shell=True, cwd=cwd, env=env,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    wall = (time.perf_counter() - t0) * 1000
    out = p.stdout.decode("utf-8", "replace")
    return {
        "cmd": cmd,
        "rc": p.returncode,
        "out": out,
        "err": p.stderr.decode("utf-8", "replace"),
        "out_B": len(p.stdout),
        "out_lines": out.count("\n") + (1 if out and not out.endswith("\n") else 0),
        "wall_ms": round(wall, 1),
    }


def server_metrics(scratch: Path, env: dict) -> dict:
    p = run(f"{MINAS} info", scratch, env)
    try:
        return json.loads(p["out"])["metrics"]
    except Exception:
        return {}


def cost_metrics(out_Bs: list[int]) -> tuple[int, int, int]:
    """(out_B, resend_B, equiv_B)。equiv = Σ o_i + Σ (T-i)·o_i — 序盤の大出力ほど高い。"""
    t = len(out_Bs)
    out = sum(out_Bs)
    resend = sum((t - 1 - i) * b for i, b in enumerate(out_Bs))
    return out, resend, out + resend


def warm_lsp(scratch: Path, env: dict, meta: dict) -> None:
    """LSP がワークスペースを読み終えて応答が安定するまで外から待つ。

    現行 daemon は rust-analyzer の進捗を待たないので、索引未完のまま応答を返し、
    空応答 + exit 0（無言の誤り）や LSP セッションロック待ちを起こす。
    そこで「プローブが上限時間内に 2 回連続で当たる」まで待つ — 絶対時刻ではなく
    応答時間で判定するので、fixture やマシンが変わっても同じ規則で使える。
    """
    limit = meta.get("probe_max_ms", 1500)
    deadline = time.time() + 60
    fast = 0
    while time.time() < deadline:
        r = run(meta["probe"], scratch, env)
        if r["rc"] == 0 and meta["probe_need"] in r["out"] and r["wall_ms"] < limit:
            fast += 1
            if fast >= 2:
                return
        else:
            fast = 0
        time.sleep(0.5)
    raise RuntimeError("LSP did not warm up in 60s")


def run_arm(flow: str, arm: str, spec: dict, cold: bool = False) -> dict:
    scratch = REPO / "tmp" / "loop" / f"{flow}-{arm}-{time.strftime('%H%M%S')}-{os.getpid()}"
    if scratch.exists():
        shutil.rmtree(scratch)
    scratch.mkdir(parents=True)
    tmpdir = scratch / ".tmp"
    tmpdir.mkdir()
    meta = FIXTURES[spec["fixture"]](scratch)

    env = dict(os.environ)
    env["TMPDIR"] = str(tmpdir)
    env["MINAE_CLIENT_NAME"] = "l0"

    proc = start_daemon(scratch, env)
    try:
        if spec.get("warmup") and not cold:
            warm_lsp(scratch, env, meta)
        results, transcript = [], []
        expect = spec.get("expect", {}).get(arm, [])
        for i, raw in enumerate(spec["arms"][arm]):
            cmd = raw
            for k, v in meta.items():
                cmd = cmd.replace("{" + k + "}", str(v))
            before = server_metrics(scratch, env)
            r = run(cmd, scratch, env)
            after = server_metrics(scratch, env)
            r["delta"] = {k: after.get(k, 0) - before.get(k, 0) for k in after}
            # 無言の誤り: exit 0 なのに期待した内容が返っていない（cold の symbol など）
            r["silent"] = bool(i < len(expect) and expect[i] and expect[i] not in r["out"])
            results.append(r)
            transcript.append(r["out"] + r["err"])
        info = json.loads(run(f"{MINAS} info", scratch, env)["out"])
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    texts = "\n".join(transcript)
    verified = None
    if "verify" in spec:
        verified = run(spec["verify"], scratch, env)["rc"] == 0
    if "need" in spec:
        verified = verified is not False and spec["need"] in texts

    t = len(results)
    out_B, resend_B, equiv = cost_metrics([r["out_B"] for r in results])
    agg: dict[str, int] = {}
    for r in results:
        for k, v in r["delta"].items():
            agg[k] = agg.get(k, 0) + v

    return {
        "flow": flow, "arm": arm,
        "goal": spec["goal"],
        "calls": t,
        "out_B": out_B,
        "out_lines": sum(r["out_lines"] for r in results),
        "resend_B": resend_B,
        "equiv_B": equiv,
        "wall_ms": round(sum(r["wall_ms"] for r in results), 1),
        "fails": sum(1 for r in results if r["rc"] != 0 or r["silent"]),
        "silent": sum(1 for r in results if r["silent"]),
        "ok": verified,
        "cold": cold,
        "cli_generation": info.get("cli_generation"),
        "daemon_generation": info.get("generation"),
        "metrics": {k: v for k, v in agg.items() if v},
        "steps": results,
    }


def median(xs: list[float]) -> float:
    xs = sorted(xs)
    return xs[len(xs) // 2]


COLS = ["calls", "out_B", "out_lines", "resend_B", "equiv_B", "wall_ms", "fails"]


def merge(runs: list[dict]) -> dict:
    """複数回の観測 → 中央値行（wall_ms は揺れるので -r で中央値を取る）。"""
    base = dict(runs[0])
    for c in COLS:
        m = median([r[c] for r in runs])
        base[c] = int(m) if isinstance(runs[0][c], int) else round(m, 1)
    oks = {r["ok"] for r in runs}
    base["ok"] = False if False in oks else (True if True in oks else None)
    base["runs"] = len(runs)
    base["wall_ms_all"] = [r["wall_ms"] for r in runs]
    return base


# ---------------------------------------------------------------- report


def table(rows: list[dict]) -> str:
    head = f"{'flow':<8} {'arm':<13}" + "".join(f"{c:>10}" for c in COLS) + f"{'ok':>6}"
    lines = [head, "-" * len(head)]
    for r in rows:
        lines.append(
            f"{r['flow']:<8} {r['arm']:<13}"
            + "".join(f"{r[c]:>10}" for c in COLS)
            + f"{str(r['ok']):>6}"
        )
    return "\n".join(lines)


def log_block(note: str, rows: list[dict], stamp: str) -> str:
    out = [f"\n## {stamp} — {note}\n\n"]
    if any(r.get("cold") for r in rows):
        out.append("cold 測定（warmup なし・1 回観測）\n\n")
    elif any("wall_ms_all" in r for r in rows):
        out.append("warm 測定（warmup あり・wall は中央値）\n\n")
    out.append("```\n" + table(rows) + "\n```\n\n")
    for r in rows:
        if r["metrics"]:
            out.append(f"- `{r['flow']}/{r['arm']}` daemon 計測: "
                       + ", ".join(f"{k}={v}" for k, v in sorted(r["metrics"].items())) + "\n")
        for s in r["steps"]:
            mark = " (silent)" if s.get("silent") else ""
            if s["rc"] != 0 or s.get("silent"):
                err = s["err"].strip().splitlines()
                out.append(f"- `{r['flow']}/{r['arm']}` 失敗 step{mark}: `{s['cmd']}` "
                           f"rc={s['rc']} {err[0] if err else ''}\n")
    out.append("\n")
    return "".join(out)


def selftest() -> int:
    """非自明な部分（再送加重の式・fixture の位置・step テンプレート）だけを検証する。"""
    assert cost_metrics([100, 50, 10]) == (160, 250, 410)
    assert cost_metrics([7]) == (7, 0, 7)
    assert cost_metrics([]) == (0, 0, 0)

    import tempfile
    with tempfile.TemporaryDirectory() as d:
        meta = fixture_rust(Path(d))
        line = (Path(d) / "src/config.rs").read_text().splitlines()[meta["validate_line"] - 1]
        col = int(meta["validate_pos"].split(":")[1])
        assert line[col - 1 :].startswith("validate"), line

    # JSON を含む step が {meta} 置換で壊れないこと（KeyError 実例の回帰）
    raw = 'printf \'%s\' \'[{"old":"a","new":"b"}]\' | minas apply {path} --hunks-stdin'
    for k, v in {"path": "x.rs", "validate_pos": "1:1"}.items():
        raw = raw.replace("{" + k + "}", str(v))
    assert raw.endswith("minas apply x.rs --hunks-stdin") and "{\"old\"" in raw

    rows = [dict(flow="f", arm="a", calls=2, out_B=1, out_lines=1, resend_B=0,
                 equiv_B=1, wall_ms=1.0, fails=0, ok=True)]
    assert "resend_B" in table(rows)
    print("selftest ok")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("-f", "--flow", action="append", choices=sorted(FLOWS),
                    help="フロー（複数指定可。既定は全部）")
    ap.add_argument("-a", "--arm", help="アーム絞り込み")
    ap.add_argument("-n", "--note", default="(no hypothesis recorded)",
                    help="仮説。ループの 課題設定 にあたる")
    ap.add_argument("--log", action="store_true", help=f"{LOG_PATH.name} へ追記")
    ap.add_argument("--json", help="結果 JSON の出力先")
    ap.add_argument("-v", "--verbose", action="store_true", help="ステップ別内訳")
    ap.add_argument("--cold", action="store_true",
                    help="warmup を省略し、起動直後の daemon で測る（cold 税の測定）")
    ap.add_argument("-r", "--repeat", type=int, default=1,
                    help="アームごとの観測回数（中央値を報告。wall_ms の揺れ対策に 3 を推奨）")
    ap.add_argument("--selftest", action="store_true", help="計測器自体の健全性確認（daemon 不要）")
    args = ap.parse_args()
    if args.selftest:
        return selftest()

    flows = args.flow or sorted(FLOWS)
    print(f"# minas={MINAS} minad={MINAD}")
    rows = []
    for name in flows:
        spec = FLOWS[name]
        for arm in spec["arms"]:
            if args.arm and arm != args.arm:
                continue
            runs = [run_arm(name, arm, spec, args.cold) for _ in range(max(1, args.repeat))]
            rows.append(merge(runs) if len(runs) > 1 else runs[0])

    print(table(rows))

    if args.verbose:
        for r in rows:
            print(f"\n## {r['flow']}/{r['arm']} (goal: {r['goal']})")
            if "wall_ms_all" in r:
                print(f"  wall per run: {r['wall_ms_all']}")
            for s in r["steps"]:
                mark = " SILENT" if s.get("silent") else ""
                print(f"  {s['wall_ms']:>8.0f}ms  {s['out_B']:>7}B rc={s['rc']}{mark}  {s['cmd']}")

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    jpath = Path(args.json) if args.json else OUT_DIR / f"l0-{time.strftime('%Y%m%d-%H%M%S')}.json"
    jpath.write_text(json.dumps({"note": args.note, "rows": rows}, indent=2, ensure_ascii=False))
    print(f"\njson: {jpath}")

    if args.log:
        stamp = time.strftime("%Y-%m-%d %H:%M")
        with LOG_PATH.open("a") as f:
            f.write(log_block(args.note, rows, stamp))
        print(f"logged: {LOG_PATH}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
