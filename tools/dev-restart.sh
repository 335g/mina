#!/usr/bin/env bash
# minad/minas/minae を再インストールし、daemon を入れ替えて 1 行で証拠を出す。
#
# なぜ要るか: `cargo install` は走っている daemon を入れ替えない。同じ
# PROTOCOL_VERSION のまま minad を直すと socket 名も同じなので、古い daemon が
# そのまま答え続け、修正を「効いた / 効かない」と誤判定する（dogfood-log §5 前提 4。
# #2 と #5 で各 2 回踏んでいる）。順序を規律ではなく 1 コマンドにする。
#
# 使い方: tools/dev-restart.sh
#   minad / minas / minae のどれかを編集したら、判定の前に必ずこれを通す。
#   TUI（minae）を開いているなら先に閉じること（daemon ごと入れ替わる）。
#   終了コード 0 = 新しい daemon が答えている（pid と build_ts の前後を表示）。
set -euo pipefail
cd "$(dirname "$0")/.."   # daemon の cwd は spawn 元で固定される（ADR-0005）ので repo 根で起動する

# 1. 入れ替え前の daemon を記録（build_ts は「minad が実際に再コンパイルされたか」の証拠）
before="$(pgrep -f 'minad serve' | tr '\n' ' ' || true)"
before_ts=""
if [ -n "$before" ]; then
  before_ts="$(minas info | python3 -c 'import json,sys; print(json.load(sys.stdin)["daemon_build_ts"])')"
fi

# 2. インストール（release。workspace の target/ を共有するので 2 つ目以降はほぼ no-op）
#    --path は 1 回しか渡せないので 1 パッケージずつ。--locked は必須: 無いと cargo が
#    独自に依存を再解決し（index 更新 + 最新版へ）、workspace の Cargo.lock と違う
#    依存でビルドしたバイナリを「テストと同じ世代」として判定してしまう
for p in minad minas minae; do cargo install --path "$p" --locked; done

# 3. 古い daemon を落とす（残った socket は daemon が bind 時に自分で除去する）
pkill -f 'minad serve' || true
for _ in $(seq 50); do
  pgrep -f 'minad serve' >/dev/null || break
  sleep 0.1
done

# 4. repo 根を cwd として新しい daemon を spawn させる（minas が自動起動する）
INFO="$(minas info)" BEFORE="$before" BEFORE_TS="$before_ts" python3 - <<'PY'
import json, os, subprocess, sys

before, ts0 = set(os.environ["BEFORE"].split()), os.environ["BEFORE_TS"]
pids = subprocess.run(["pgrep", "-f", "minad serve"], capture_output=True, text=True).stdout.split()
info = json.loads(os.environ["INFO"])
if len(pids) != 1:
    sys.exit(f"NG: daemon が {len(pids)} 個（{pids}）— 手で確認すること")
if pids[0] in before:
    sys.exit(f"NG: pid {pids[0]} は入れ替え前と同じ = 古い daemon が答えている")
ts = info["daemon_build_ts"]
print(f"ok: daemon pid {pids[0]}（入れ替え前 {sorted(before) or 'なし'}）"
      f" build_ts {ts0 or '—'} → {ts}"
      f"（{'minad は再コンパイルされていない' if ts0 == str(ts) else 'minad を再コンパイル'}）"
      f" cli {info['cli_generation']} / daemon {info['generation']}")
if info["cli_generation"] != info["generation"]:
    print("note: cli と daemon のビルド時コミットが違うのは正常（各バイナリは最後に"
          "コンパイルした時点の HEAD を持つので、自動コミットで HEAD が動くたびにずれる）。"
          "判定の根拠には daemon_build_ts を使うこと", file=sys.stderr)
PY
