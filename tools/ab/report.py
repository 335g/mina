#!/usr/bin/env python3
"""L2 掃引の履歴（docs/benchmarks/l2/history.jsonl）を集計して読める形にする。

  使い方:
    python3 tools/ab/report.py                     # 表 + paired 統計
    python3 tools/ab/report.py --csv tmp/l2.csv    # 素の表を書き出す（外部で作図する用）
    python3 tools/ab/report.py --svg docs/benchmarks/l2/curve.svg

設計:
  - 1 run = 1 行を追記する履歴なので、同一 key（task/scale/drift/arm/idx）の
    **最後の行**を採用する（NC や失敗 run を走り直したときに置き換わる）。
  - 集計は **comp=C のみ**。除外した run は必ず件数を出す（黙って落とさない）。
  - 効果量は **同一 idx の paired 比**で出す。arm を交互に走らせているので idx が
    対応している。比の中央値 + bootstrap 95% CI を出す — n=5 では CI が広いこと
    自体が結論なので、p 値だけを出して「有意」と言わない。
  - 検定は符号検定（分布を仮定しない）。n=5 では両側で最小 p=0.0625 なので、
    そこに達しない場合は「n が足りない」と明示する。

依存ゼロ（標準ライブラリのみ）。matplotlib は入れない。
"""
import argparse
import json
import math
import random
import statistics as st
from pathlib import Path

HISTORY = (Path(__file__).resolve().parent.parent.parent
           / "docs" / "benchmarks" / "l2" / "history.jsonl")
ARMS = ("native", "naive", "minas")
METRICS = (("input", "input tokens", 0), ("cost", "cost $", 4),
           ("wall_s", "wall s", 0), ("steps", "steps", 0))

# 履歴に無い（= 壊れた）run を除いた件数の内訳を必ず出す。
def load(path, need_comp_c=True):
    rows = {}
    for line in open(path):
        line = line.strip()
        if line:
            r = json.loads(line)
            rows[(r["task"], r["scale"], r["drift"], r["arm"], r["idx"])] = r
    kept, dropped = [], []
    for r in rows.values():
        if r.get("invalid"):
            dropped.append(r)          # 計器が壊れていた時期の測定は使わない
        elif need_comp_c and r.get("comp") != "C":
            dropped.append(r)
        else:
            kept.append(r)
    return kept, dropped


def median(rows, key):
    v = [r[key] for r in rows if r.get(key) is not None]
    return st.median(v) if v else None


def paired(rows, metric, a, b, cond=None):
    """同一 (drift, scale, idx) で a/b を対応させ、比 a/b のリストを返す。"""
    idx = {}
    for r in rows:
        if cond and not cond(r):
            continue
        idx.setdefault((r["drift"], r["scale"], r["idx"]), {})[r["arm"]] = r
    out = []
    for k, d in sorted(idx.items()):
        ra, rb = d.get(a), d.get(b)
        va = (ra or {}).get(metric)
        vb = (rb or {}).get(metric)
        if va and vb:
            out.append((k, va / vb))
    return out


def sign_test(ratios):
    """符号検定（両側・対称なので片側 p×2）。"""
    pos = sum(1 for _, r in ratios if r > 1)
    neg = sum(1 for _, r in ratios if r < 1)
    n = pos + neg
    if n == 0:
        return None, pos, neg
    k = min(pos, neg)
    p = 2 * sum(math.comb(n, i) for i in range(k + 1)) / (2 ** n)
    return min(p, 1.0), pos, neg


def boot_ci(ratios, iters=4000, seed=20260916):
    """比の中央値の bootstrap 95% CI（percentile）。決定的（seed 固定）。"""
    rnd = random.Random(seed)
    v = [r for _, r in ratios]
    if len(v) < 2:
        return None, None
    meds = []
    for _ in range(iters):
        meds.append(st.median([rnd.choice(v) for _ in v]))
    meds.sort()
    return meds[int(0.025 * iters)], meds[int(0.975 * iters) - 1]


def fmt_pct(x):
    return "n/a" if x is None else f"{100 * (x - 1):+.0f}%"


def table(rows, drift):
    out = [f"### drift={drift}", "",
           "| scale | arm | input | cost $ | wall s | steps | reads | edits | rejects |",
           "| ---: | :-- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    scales = sorted({r["scale"] for r in rows if r["drift"] == drift})
    for s in scales:
        for arm in ARMS:
            rs = [r for r in rows if r["scale"] == s and r["arm"] == arm
                  and r["drift"] == drift]
            if not rs:
                continue
            g = lambda k, f="{:,.0f}": f.format(median(rs, k) or 0)
            out.append(f"| {s:,} | {arm} | {g('input')} | {g('cost', '{:.4f}')} | "
                       f"{g('wall_s')} | {g('steps')} | {g('reads')} | {g('edits')} | "
                       f"{g('rejects')} |")
    return "\n".join(out)


def outcome_counts(rows):
    out = []
    for dr in (0, 1):
        for arm in ARMS:
            rs = [r for r in rows if r["arm"] == arm and r["drift"] == dr]
            if not rs:
                continue
            c = {k: sum(1 for r in rs if r["outcome"] == k)
                 for k in ("GREEN", "LOUD-FAIL", "CORRUPT", "INCOMPLETE")}
            bad = sum(1 for r in rs if r["noise"] == "DAMAGED")
            out.append(f"| {dr} | {arm} | {len(rs)} | {c['GREEN']} | {c['LOUD-FAIL']} | "
                       f"{c['INCOMPLETE']} | {c['CORRUPT']} ({bad} noise) |")
    return ("| drift | arm | n | GREEN | LOUD-FAIL | INCOMPLETE | CORRUPT |\n"
            "| ---: | :-- | ---: | ---: | ---: | ---: | ---: |\n" + "\n".join(out))


def drift_section(rows):
    """drift（= 外部プロセスによる並行変更）が発火した run だけを見る。

    指標は **ext_lost**: 外部変更が最後のファイルから消えているのに、run は
    正常終了（t11 の 5 項目は満たし cargo check も通る）している件数。
    「タスクの外側を黙って壊した」を直接数える唯一の指標。
    """
    out = ["| scale | arm | n(発火) | ext_lost | rejects | wall s | input |",
           "| ---: | :-- | ---: | ---: | ---: | ---: | ---: |"]
    for s in sorted({r["scale"] for r in rows if r["drift"] == 1}):
        for arm in ARMS:
            rs = [r for r in rows if r["drift"] == 1 and r["scale"] == s
                  and r["arm"] == arm and r.get("drift_fired")]
            if not rs:
                continue
            lost = sum(1 for r in rs if r.get("ext_lost"))
            g = lambda k, f="{:,.0f}": f.format(median(rs, k) or 0)
            out.append(f"| {s:,} | {arm} | {len(rs)} | **{lost}** | {g('rejects')} | "
                       f"{g('wall_s')} | {g('input')} |")
    missed = sum(1 for r in rows if r["drift"] == 1 and not r.get("drift_fired"))
    fired = sum(1 for r in rows if r["drift"] == 1 and r.get("drift_fired"))
    out.append(f"\n注入発火: {fired} run / 未発火（分析から除外）: {missed} run")
    return "\n".join(out)


def ratios_table(rows, a, b):
    """a を b と比べる（同一 idx の paired 比）。scale ごと + 全体。"""
    out = [f"### {a} / {b}（同一 idx の paired 比。1 未満 = {a} が小さい）", "",
           "| drift | scale | n | input | cost | wall | 符号検定 p |",
           "| ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for dr in (0, 1):
        for s in sorted({r["scale"] for r in rows if r["drift"] == dr}):
            cond = lambda r, dr=dr, s=s: r["drift"] == dr and r["scale"] == s
            cells = []
            for m in ("input", "cost", "wall_s"):
                pr = paired(rows, m, a, b, cond)
                cells.append(fmt_pct(st.median([x for _, x in pr])) if pr else "n/a")
            pr = paired(rows, "input", a, b, cond)
            p, pos, neg = sign_test(pr) if pr else (None, 0, 0)
            n = len(pr)
            out.append(f"| {dr} | {s:,} | {n} | {cells[0]} | {cells[1]} | {cells[2]} | "
                       f"{'n/a' if p is None else f'{p:.3f} ({pos}/{n}+)'} |")
        pr = paired(rows, "input", a, b, lambda r, dr=dr: r["drift"] == dr)
        if not pr:
            continue
        lo, hi = boot_ci(pr)
        p, pos, neg = sign_test(pr)
        out.append(f"| {dr} | **all** | {len(pr)} | "
                   f"**{fmt_pct(st.median([x for _, x in pr]))}** "
                   f"[{fmt_pct(lo)}, {fmt_pct(hi)}] | | | "
                   f"{'n/a' if p is None else f'{p:.3f}'} |")
    return "\n".join(out)


def emit_csv(rows, path):
    cols = ["drift", "scale", "arm", "idx", "input", "cost", "wall_s", "steps",
            "outcome", "facts", "rejects"]
    with open(path, "w") as f:
        f.write(",".join(cols) + "\n")
        for r in sorted(rows, key=lambda r: (r["drift"], r["scale"], r["arm"], r["idx"])):
            f.write(",".join(str(r.get(c, "")) for c in cols) + "\n")
    print(f"csv -> {path}")


def emit_svg(rows, path):
    """3 arm の曲線を 2 パネル（input tokens / wall）で描く。log-log。依存ゼロ。

    数字は履歴から直接取る。軸の目盛りは最小限（点の数が少ないので線と点で足りる）。
    """
    W, H, PAD = 420, 300, 46
    colors = {"native": "#c0392b", "naive": "#e67e22", "minas": "#2471a3"}
    scales = sorted({r["scale"] for r in rows})
    panels = [("input", "input tokens (median)"), ("wall_s", "wall (s, median)")]
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W*len(panels)}" '
             f'height="{H+40}" font-family="sans-serif" font-size="11">']
    for pi, (metric, title) in enumerate(panels):
        ox = pi * W
        series = {}
        for arm in ARMS:
            pts = []
            for s in scales:
                rs = [r for r in rows if r["arm"] == arm and r["scale"] == s
                      and r["drift"] == 0]
                if rs:
                    pts.append((s, median(rs, metric)))
            series[arm] = pts
        xs = [math.log10(x) for x in scales]
        ys = [y for a in ARMS for _, y in series[a] if y]
        x0, x1 = min(xs), max(xs)
        y0, y1 = min(ys) * 0.85, max(ys) * 1.15
        yl0, yl1 = math.log10(y0), math.log10(y1)
        px = lambda x: ox + PAD + (math.log10(x) - x0) / (x1 - x0) * (W - 2 * PAD)
        py = lambda y: 30 + (yl1 - math.log10(y)) / (yl1 - yl0) * (H - 60)
        parts.append(f'<text x="{ox+PAD}" y="18">{title} — drift off</text>')
        parts.append(f'<line x1="{ox+PAD}" y1="{py(y0):.1f}" x2="{ox+W-PAD}" '
                     f'y2="{py(y0):.1f}" stroke="#888"/><line x1="{ox+PAD}" y1="30" '
                     f'x2="{ox+PAD}" y2="{py(y0):.1f}" stroke="#888"/>')
        for yv in (y0, y1):
            parts.append(f'<text x="{ox+PAD-6}" y="{py(yv)+3:.1f}" '
                         f'text-anchor="end">{yv:,.0f}</text>')
        for s in scales:
            parts.append(f'<line x1="{px(s):.1f}" y1="{py(y0):.1f}" x2="{px(s):.1f}" '
                         f'y2="{py(y0)+4:.1f}" stroke="#888"/>'
                         f'<text x="{px(s):.1f}" y="{py(y0)+17:.1f}" '
                         f'text-anchor="middle">{s//2}</text>')
        parts.append(f'<text x="{ox+W/2}" y="{py(y0)+32:.1f}" text-anchor="middle">'
                     f'lines per file (log)</text>')
        for arm in ARMS:
            pts = [(px(x), py(y)) for x, y in series[arm] if y]
            d = " ".join(f"{x:.1f},{y:.1f}" for x, y in pts)
            parts.append(f'<polyline points="{d}" fill="none" stroke="{colors[arm]}" '
                         f'stroke-width="2"/>')
            for x, y in pts:
                parts.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="3" '
                             f'fill="{colors[arm]}"/>')
            lx, ly = pts[-1]
            parts.append(f'<text x="{lx+6:.0f}" y="{ly+4:.0f}" fill="{colors[arm]}">'
                         f'{arm}</text>')
    parts.append("</svg>")
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text("\n".join(parts))
    print(f"svg -> {path}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--history", default=str(HISTORY))
    ap.add_argument("--csv")
    ap.add_argument("--svg")
    ap.add_argument("--all", action="store_true",
                    help="comp=C 以外も含める（既定は除外）")
    a = ap.parse_args()
    rows, dropped = load(a.history, need_comp_c=not a.all)
    if not rows:
        print("履歴が空です（掃引がまだ走っていない）")
        return
    meta = rows[0]
    print(f"履歴: {a.history}")
    print(f"run : 採用 {len(rows)} / 除外 {len(dropped)}"
          + ("（遵守 NC など）" if dropped else ""))
    print(f"計器: commit={meta.get('commit')} generation={meta.get('generation')} "
          f"build={meta.get('build')} model={meta.get('model')} "
          f"runner={meta.get('runner')}")
    if dropped:
        print(f"除外した内訳: " + ", ".join(
            f"{r['arm']}@s{r['scale']}d{r['drift']}#{r['idx']}={r['comp']}"
            for r in dropped[:10]))
    print("\n## 中央値（採用 run のみ）\n")
    print(table(rows, 0))
    if any(r["drift"] for r in rows):
        print()
        print(table(rows, 1))
    print("\n## 結果の内訳（CORRUPT = 意図しない領域を書き換えたまま正常終了）\n")
    print(outcome_counts(rows))
    print()
    print("## drift（並行変更）: 外部変更を黙って消したか\n")
    print(drift_section(rows))
    print()
    print(ratios_table(rows, "minas", "native"))
    print()
    print(ratios_table(rows, "minas", "naive"))
    if a.csv:
        emit_csv(rows, a.csv)
    if a.svg:
        emit_svg([r for r in rows if r["drift"] == 0], a.svg)


if __name__ == "__main__":
    main()
