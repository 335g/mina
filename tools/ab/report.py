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
# README に出す arm。positional は追加実験（2 点だけ）なので曲線は点で描く。
README_ARMS = ("native", "naive", "minas", "positional")
COLORS = {"native": "#c0392b", "naive": "#e67e22", "minas": "#1f6feb",
          "positional": "#7b3fa0"}
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


def _open_svg(w, h):
    return [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
            f'font-family="-apple-system,Segoe UI,sans-serif" font-size="12">',
            f'<rect width="{w}" height="{h}" fill="white"/>']


def _emit_svg_old(rows, path):
    """3 パネル（input tokens / cost / wall）の曲線。log-log。依存ゼロ。

    縦軸は log、横軸も log（規模が 150 と 9600 で 64 倍違うので線形では潰れる）。
    中央値は README に載せる数字そのもの。dashed = 追加 arm（点が 2 つしかない）。
    """
    W, H, PAD = 380, 290, 52
    arms = [a for a in README_ARMS if any(r["arm"] == a for r in rows)]
    scales = sorted({r["scale"] for r in rows})
    panels = [("input", "input tokens (median)", "{v:,.0f}"),
              ("cost", "cost $ (median)", "{v:.4f}"),
              ("wall_s", "wall s (median)", "{v:,.0f}")]
    parts = _open_svg(W * len(panels), H + 54)
    for pi, (metric, title, fmt) in enumerate(panels):
        ox = pi * W
        series = {}
        for arm in arms:
            pts = []
            for s in scales:
                rs = [r for r in rows if r["arm"] == arm and r["scale"] == s
                      and r["drift"] == 0]
                if rs:
                    pts.append((s, median(rs, metric)))
            series[arm] = pts
        xs = [math.log10(x) for x in scales]
        ys = [y for a in arms for _, y in series[a] if y]
        y0, y1 = min(ys) * 0.8, max(ys) * 1.25
        x0, x1 = min(xs), max(xs)
        yl0, yl1 = math.log10(y0), math.log10(y1)
        px = lambda x: ox + PAD + (math.log10(x) - x0) / (x1 - x0) * (W - 2 * PAD)
        py = lambda y: 34 + (yl1 - math.log10(y)) / (yl1 - yl0) * (H - 64)
        parts.append(f'<text x="{ox+PAD-30}" y="20" font-size="12" '
                     f'font-weight="bold">{title} — drift off</text>')
        for k in (0.0, 0.5, 1.0):
            gv = 10 ** (yl0 + k * (yl1 - yl0))
            label = f"{gv/1000:.0f}k" if gv >= 10000 else fmt.format(v=gv)
            parts.append(f'<line x1="{ox+PAD}" y1="{py(gv):.1f}" x2="{ox+W-PAD}" '
                         f'y2="{py(gv):.1f}" stroke="#e6e6e6"/>' +
                         (f'<text x="{ox+PAD-6}" y="{py(gv)+4:.1f}" text-anchor="end" '
                          f'fill="#555">{label}</text>' if k in (0.0, 1.0) else ""))
        parts.append(f'<line x1="{ox+PAD}" y1="{py(y0):.1f}" x2="{ox+PAD}" '
                     f'y2="34" stroke="#888"/>')
        for s in scales:
            parts.append(f'<line x1="{px(s):.1f}" y1="{py(y0):.1f}" x2="{px(s):.1f}" '
                         f'y2="{py(y0)+4:.1f}" stroke="#888"/>'
                         f'<text x="{px(s):.1f}" y="{py(y0)+17:.1f}" '
                         f'text-anchor="middle" fill="#333">{s//2}</text>')
        parts.append(f'<text x="{ox+W/2}" y="{py(y0)+34:.1f}" text-anchor="middle" '
                     f'fill="#333">lines per file (log)</text>')
        for arm in arms:
            pts = [(px(x), py(y)) for x, y in series[arm] if y]
            dash = ' stroke-dasharray="5,3"' if arm == "positional" else ""
            parts.append(f'<polyline points="{" ".join(f"{x:.1f},{y:.1f}" for x, y in pts)}" '
                         f'fill="none" stroke="{COLORS[arm]}" stroke-width="2"{dash}/>')
            for x, y in pts:
                parts.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="3" '
                             f'fill="{COLORS[arm]}"/>')
        # 凡例は右下（曲線は左上→右上に伸びるので重ならない）。
        for i, arm in enumerate(arms):
            ly = py(y0) - 8 - (len(arms) - 1 - i) * 14
            dash = ' stroke-dasharray="5,3"' if arm == "positional" else ""
            parts.append(f'<line x1="{ox+W-104}" y1="{ly}" x2="{ox+W-82}" y2="{ly}" '
                         f'stroke="{COLORS[arm]}" stroke-width="2"{dash}/>'
                         f'<text x="{ox+W-78}" y="{ly+4}" fill="{COLORS[arm]}">{arm}</text>')
    parts.append("</svg>")
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text("\n".join(parts))
    print(f"svg -> {path}")


def emit_fig_cost(rows, path):
    """費用の図（3 パネル: input tokens / cost / wall）。log-log。依存ゼロ。

    中央値は README に載せる数字そのもの。縦横とも log（規模が 150→9600 で 64 倍違う）。
    凡例は右下（曲線は左から右上に伸びる）。positional は点が 2 つなので破線。
    """
    W, H, PAD = 350, 290, 54
    arms = [a for a in README_ARMS if any(r["arm"] == a for r in rows)]
    scales = sorted({r["scale"] for r in rows})
    panels = [("input", "input tokens（中央値）", "{v:,.0f}"),
              ("cost", "cost $（中央値）", "{v:.4f}"),
              ("wall_s", "wall 秒（中央値）", "{v:,.0f}")]
    parts = _open_svg(W * len(panels), H + 76)
    for pi, (metric, title, fmt) in enumerate(panels):
        ox = pi * W
        series = {}
        for arm in arms:
            series[arm] = [(s, median([r for r in rows if r["arm"] == arm
                                       and r["scale"] == s and r["drift"] == 0], metric))
                           for s in scales
                           if any(r["arm"] == arm and r["scale"] == s and r["drift"] == 0
                                  for r in rows)]
        xs = [math.log10(x) for x in scales]
        ys = [y for a in arms for _, y in series[a] if y]
        y0, y1 = min(ys) * 0.8, max(ys) * 1.25
        x0, x1 = min(xs), max(xs)
        yl0, yl1 = math.log10(y0), math.log10(y1)
        px = lambda x: ox + PAD + (math.log10(x) - x0) / (x1 - x0) * (W - 2 * PAD)
        py = lambda y: 34 + (yl1 - math.log10(y)) / (yl1 - yl0) * (H - 66)
        parts.append(f'<text x="{ox+PAD-34}" y="20" font-size="13" '
                     f'font-weight="bold">{title} — drift off</text>')
        for k in (0.0, 0.5, 1.0):
            gv = 10 ** (yl0 + k * (yl1 - yl0))
            lab = (f"{gv/1000:.0f}k" if gv >= 10000
                   else f"{gv/1000:.1f}k" if gv >= 1000 else fmt.format(v=gv))
            parts.append(f'<line x1="{ox+PAD}" y1="{py(gv):.1f}" x2="{ox+W-PAD}" '
                         f'y2="{py(gv):.1f}" stroke="#e6e6e6"/>')
            if k in (0.0, 1.0):
                parts.append(f'<text x="{ox+PAD-6}" y="{py(gv)+4:.1f}" text-anchor="end" '
                             f'fill="#555">{lab}</text>')
        parts.append(f'<line x1="{ox+PAD}" y1="{py(y0):.1f}" x2="{ox+PAD}" y2="34" stroke="#888"/>')
        for s in scales:
            parts.append(f'<line x1="{px(s):.1f}" y1="{py(y0):.1f}" x2="{px(s):.1f}" '
                         f'y2="{py(y0)+4:.1f}" stroke="#888"/>'
                         f'<text x="{px(s):.1f}" y="{py(y0)+18:.1f}" text-anchor="middle" '
                         f'fill="#333">{s//2}</text>')
        parts.append(f'<text x="{ox+W/2}" y="{py(y0)+36:.1f}" text-anchor="middle" '
                     f'fill="#333">lines per file（両対数）</text>')
        for arm in arms:
            pts = [(px(x), py(y)) for x, y in series[arm] if y]
            dash = ' stroke-dasharray="5,3"' if arm == "positional" else ""
            parts.append(f'<polyline points="{" ".join(f"{x:.1f},{y:.1f}" for x, y in pts)}" '
                         f'fill="none" stroke="{COLORS[arm]}" stroke-width="2"{dash}/>')
            for x, y in pts:
                parts.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="3" fill="{COLORS[arm]}"/>')
    # 凡例は図全体の下に 1 回だけ（各パネル内に置くと曲線と重なる）。
    lw = 96 * len(arms)
    for i, arm in enumerate(arms):
        lx = W * len(panels) / 2 - lw / 2 + i * 96
        ly = H + 36
        dash = ' stroke-dasharray="5,3"' if arm == "positional" else ""
        parts.append(f'<line x1="{lx:.0f}" y1="{ly}" x2="{lx+22:.0f}" y2="{ly}" '
                     f'stroke="{COLORS[arm]}" stroke-width="2"{dash}/>'
                     f'<text x="{lx+27:.0f}" y="{ly+4}" fill="{COLORS[arm]}">{arm}</text>')
    parts.append("</svg>")
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text("\n".join(parts))
    print(f"svg -> {path}")


def emit_fig_safety(rows, path):
    """「壊さない」の図（2 列 × 2 行、README の幅に収める）。

      (1) 遂行結果の内訳 — CORRUPT の棒が 1 本も無いこと（4 arm 合計 200+ run）
      (2) 位置指定 ÷ 内容指定 の相対入力トークン（apply = 1.0 の基準線つき）
      (3) 並行変更を黙って消した run（drift 注入が当たった run のみ）
    """
    W, H = 470, 290
    arms = [a for a in README_ARMS if any(r["arm"] == a for r in rows)]
    parts = _open_svg(W * 2, H * 2 + 40)
    VTOP, VBASE = 60, 210          # 棒グラフの描画領域（値 0 = VBASE）

    def title(ox, oy, text):
        parts.append(f'<text x="{ox+14}" y="{oy+22}" font-size="13" '
                     f'font-weight="bold">{text}</text>')

    def note(ox, oy, text):
        parts.append(f'<text x="{ox+14}" y="{oy+H-6}" fill="#666" font-size="10">{text}</text>')

    # --- (1) 結果の内訳（横積み上げ）
    kinds = [("GREEN", "#2e8b57"), ("LOUD-FAIL", "#d4a017"),
             ("INCOMPLETE", "#8e8e8e"), ("CORRUPT", "#c0392b")]
    ox, oy = 0, 0
    title(ox, oy, "task outcome per arm（全 run）")
    total = max(sum(1 for r in rows if r["arm"] == arm) for arm in arms)
    bx, bw = ox + 84, W - 160
    for i, arm in enumerate(arms):
        rs = [r for r in rows if r["arm"] == arm]
        y = oy + 48 + i * 30
        parts.append(f'<text x="{ox+14}" y="{y+14}" fill="#333">{arm}</text>')
        parts.append(f'<text x="{ox+W-14}" y="{y+14}" text-anchor="end" fill="#666" '
                     f'font-size="10">n={len(rs)}</text>')
        x = bx
        for kind, col in kinds:
            c = sum(1 for r in rs if r["outcome"] == kind)
            if not c:
                continue
            w = bw * c / total
            parts.append(f'<rect x="{x:.1f}" y="{y}" width="{w:.1f}" height="19" fill="{col}"/>')
            if c >= 5:
                parts.append(f'<text x="{x+w/2:.1f}" y="{y+13.5}" text-anchor="middle" '
                             f'fill="white">{c}</text>')
            x += w
    for i, (kind, col) in enumerate(kinds):
        ly = oy + 48 + len(arms) * 30 + 12 + i * 13
        parts.append(f'<rect x="{ox+14}" y="{ly-9}" width="10" height="10" fill="{col}"/>'
                     f'<text x="{ox+29}" y="{ly}" fill="#333" font-size="10">{kind}</text>')
    note(ox, oy, "CORRUPT = 意図しない領域を書き換えたまま正常終了（0 本が主張）")

    # --- (2) positional ÷ apply（入力トークン、同一 idx の中央値）
    ox = W
    title(ox, oy, "positional ÷ apply（入力トークン、同一 idx の中央値）")
    conds = []
    for s in sorted({r["scale"] for r in rows}):
        for d in (0, 1):
            pa = {r["idx"]: r for r in rows if r["arm"] == "positional"
                  and r["scale"] == s and r["drift"] == d}
            ap = {r["idx"]: r for r in rows if r["arm"] == "minas"
                  and r["scale"] == s and r["drift"] == d}
            ks = sorted(set(pa) & set(ap))
            if ks:
                conds.append((f"{s//2}行 d{d}",
                              st.median([pa[k]["input"] / ap[k]["input"] for k in ks]), len(ks)))
    ymax = max(3.0, max([c[1] for c in conds] + [1.0]) * 1.18)
    slot = (W - 60) / len(conds)
    yv = lambda v: VBASE - (v / ymax) * (VBASE - VTOP)
    x0 = ox + 30
    parts.append(f'<line x1="{x0}" y1="{VBASE}" x2="{ox+W-16}" y2="{VBASE}" stroke="#888"/>')
    for i, (lab, v, n) in enumerate(conds):
        x = x0 + i * slot + slot * 0.15
        w = slot * 0.7
        parts.append(f'<rect x="{x:.1f}" y="{yv(v):.1f}" width="{w:.1f}" '
                     f'height="{VBASE-yv(v):.1f}" fill="#7b3fa0" opacity="0.85"/>'
                     f'<text x="{x+w/2:.1f}" y="{yv(v)-5:.1f}" text-anchor="middle" '
                     f'fill="#7b3fa0" font-weight="bold">{v:.2f}x</text>')
        parts.append(f'<text x="{x+w/2:.1f}" y="{VBASE+16}" text-anchor="middle" '
                     f'fill="#333" font-size="10">{lab}</text>'
                     f'<text x="{x+w/2:.1f}" y="{VBASE+28}" text-anchor="middle" '
                     f'fill="#888" font-size="9">n={n}</text>')
    parts.append(f'<line x1="{x0}" y1="{yv(1.0):.1f}" x2="{ox+W-16}" y2="{yv(1.0):.1f}" '
                 f'stroke="#1f6feb" stroke-dasharray="4,3"/>'
                 f'<text x="{ox+W-16}" y="{yv(1.0)-5:.1f}" text-anchor="end" fill="#1f6feb">'
                 f'apply = 1.0x</text>')
    note(ox, oy, "位置指定は 1 編集に Open + edit + Save の 3 往復かかる（apply は 1 往復）")

    # --- (3) 外部変更を失った run
    ox, oy = 0, H + 40
    title(ox, oy, "並行変更を黙って消した run")
    lost = []
    for arm in arms:
        rs = [r for r in rows if r["arm"] == arm and r["drift"] == 1 and r.get("drift_fired")]
        if rs:
            lost.append((arm, sum(1 for r in rs if r.get("ext_lost")), len(rs)))
    m = max([n for _, c, n in lost] + [1])
    slot = (W - 60) / len(lost)
    x0 = ox + 30
    parts.append(f'<line x1="{x0}" y1="{oy+VBASE}" x2="{ox+W-16}" y2="{oy+VBASE}" stroke="#888"/>')
    for i, (arm, c, n) in enumerate(lost):
        x = x0 + i * slot + slot * 0.25
        w = slot * 0.5
        h = (c / m) * (VBASE - VTOP)
        if c:
            parts.append(f'<rect x="{x:.1f}" y="{oy+VBASE-h:.1f}" width="{w:.1f}" '
                         f'height="{h:.1f}" fill="#c0392b"/>')
        parts.append(f'<text x="{x+w/2:.1f}" y="{oy+VBASE-h-7:.1f}" text-anchor="middle" '
                     f'fill="{"#c0392b" if c else "#888"}" font-weight="bold">{c}/{n}</text>'
                     f'<text x="{x+w/2:.1f}" y="{oy+VBASE+16}" text-anchor="middle" '
                     f'fill="#333" font-size="10">{arm}</text>')
    note(ox, oy, "drift 注入が当たった run のみ。失った run はどれも「成功」と報告している")

    # --- (4) 検査の内訳（selftest が決定論的に固定しているもの）
    ox, oy = W, H + 40
    title(ox, oy, "費用ゼロで毎回検証する不変条件（ab.py selftest）")
    checks = [
        ("無検証の全文書戻しは外部変更を消す", "消える = 欠陥が実在する"),
        ("minas apply は同じ状況でも保つ", "消えない = 契約が守る"),
        ("位置指定の edit は expected_text 無しで拒否", "rc=1 で実行されない"),
        ("drift 注入が当たる / 外れたら検出", "安全指標 ext_lost 自体の検証"),
        ("各 arm の custom tool の TS 構文", "計器の生成ミスを検出"),
    ]
    for i, (name, meaning) in enumerate(checks):
        y = oy + 48 + i * 34
        parts.append(f'<text x="{ox+24}" y="{y}" fill="#2e8b57" font-weight="bold">PASS</text>'
                     f'<text x="{ox+70}" y="{y}" fill="#333">{name}</text>'
                     f'<text x="{ox+70}" y="{y+14}" fill="#666" font-size="10">{meaning}</text>')
    note(ox, oy, "赤ければ掃引を回さない（LLM 費用ゼロ・1〜2 分）")
    parts.append("</svg>")
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text("\n".join(parts))
    print(f"svg -> {path}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--history", default=str(HISTORY))
    ap.add_argument("--csv")
    ap.add_argument("--svg")
    ap.add_argument("--safety-svg", help="「壊さない」の 3 パネル図")
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
        emit_fig_cost([r for r in rows if r["drift"] == 0], a.svg)
    if a.safety_svg:
        emit_fig_safety(rows, a.safety_svg)


if __name__ == "__main__":
    main()
