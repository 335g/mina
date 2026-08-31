#!/usr/bin/env bash
# ADR-0031 の検証: outline / at が「全文読み」に対してどれだけトークン（bytes）
# を削減するかを実測する。daemon は自動起動（温まった状態で計測するため、
# ウォームアップ呼び出しを使う）。
#
# 使い方: tools/verify_outline.sh [file]
#   省略時は minae-term/src/daemon.rs（~7900 行）で計測する。
set -euo pipefail
cd "$(dirname "$0")/.."

FILE="${1:-minae-term/src/daemon.rs}"
MINAE=./target/debug/minae

[ -x "$MINAE" ] || { echo "build してから実行: cargo build"; exit 1; }
[ -f "$FILE" ] || { echo "対象ファイルがありません: $FILE"; exit 1; }

echo "== 対象: $FILE ($(wc -l < "$FILE") 行, $(wc -c < "$FILE") bytes)"

# ウォームアップ（温まった rust-analyzer で測る。コールドは含めない）
"$MINAE" session outline "$FILE" > /dev/null 2>&1 || true

# 「全文読み」の比較対象は、ファイルを開いた状態の session get
"$MINAE" session exec "{\"Open\": {\"path\": \"$FILE\"}}" > /dev/null 2>&1 || true

echo
echo "== A) 従来: 全文読み (session get)"
get_raw=$("$MINAE" session get 2>/dev/null | python3 -c 'import json,sys; print(len(json.dumps(json.load(sys.stdin))))' | tr -d '\n')
echo "   session get        : $get_raw bytes（全文スナップショット・raw JSON）"

echo
echo "== B) outline（階層ツリー・compact JSON）"
outline_out=$("$MINAE" session outline "$FILE" 2>/dev/null)
outline_bytes=$(printf '%s' "$outline_out" | python3 -c 'import json,sys; print(len(json.dumps(json.load(sys.stdin))))' | tr -d '\n')
n_syms=$(printf '%s' "$outline_out" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(sum(1+len(x.get("children",[])) for x in d))')
echo "   session outline    : $outline_bytes bytes / $n_syms 記号"

echo
echo "== C) at（位置を囲む記号: 最初の関数定義の名前の上）"
probe_line=$(grep -nE "^\s*(pub( async)? )?fn |^\s*fn " "$FILE" | head -1 | cut -d: -f1)
probe_line=${probe_line:-1}
at_out=$("$MINAE" session at "$FILE" "$probe_line:1" 2>/dev/null)
at_bytes=$(printf '%s' "$at_out" | python3 -c 'import json,sys; print(len(json.dumps(json.load(sys.stdin))))' | tr -d '\n')
at_name=$(printf '%s' "$at_out" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("%s (found=%s)" % (d["name"], d["found"]))')
echo "   session at ${probe_line}:1 : $at_bytes bytes / $at_name"

echo
echo "== D) 構造把握フローの比較（tool コール 1 往復あたり）"
echo "   従来  : get(全文)                      = $get_raw bytes"
echo "   Outline: outline + at                 = $((outline_bytes + at_bytes)) bytes"
if [ "$get_raw" -gt 0 ]; then
  pct=$(python3 -c "print(round((1 - ($outline_bytes + $at_bytes) / $get_raw) * 100))")
  echo "   削減率: ${pct}%"
fi

echo
echo "== E) daemon 累積メトリクス（outline 経路の実績）"
"$MINAE" session info 2>/dev/null | python3 -c \
  'import json,sys; m=json.load(sys.stdin)["metrics"]; print("   outline_total=%d outline_bytes=%d symbol_range_total=%d symbol_range_bytes=%d" % (m["outline_total"], m["outline_bytes"], m["symbol_range_total"], m["symbol_range_bytes"]))'