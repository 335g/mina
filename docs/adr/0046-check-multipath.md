# minas check の複数パス対応(一括検証)

第1回検証(docs/gitignore/study/minas-dev-study.md)で指摘: `minas check` は
引数 1 ファイルのみ(複数渡しは黙って失敗)で、workspace 全体のクリーン確認
手段が無い。apply/rename で触れた複数ファイルを 1 コマンドで検証したくても、
パスごとにコマンドを繰り返す必要があった。

Status: accepted

## Decision

`minas check` を**複数パス受け付け**に拡張し、各パスの結果を集約して
compact JSON の配列で出力する。実装は **CLI 側ループ**(client-only、
プロトコル `Command::CheckDiagnostics` は単一パスのまま)。

- `minas check a.rs b.rs` → `[{"path": …}, …]`(ファイルごとに settled /
  total / diagnostics)。
- いずれかのパスに error 診断があれば exit 2(警告のみは 0)。
- いずれかのパスが失敗(not supported / LSP エラー)でも exit 2、成功分は
  出力する。
- 引数なしは「at least one path is required」で exit 1(曖昧さ回避)。
- ワークスペース全体("check ./")は対象外 — 検証対象は「編集で触れた
  ファイル集合」を明示的に渡す形に留める(第3回の実運用: apply/rename 後の
  4〜6 ファイルをまとめて check)。

プロトコル変更なし（ADR-0042 と同じ client-only の流儀）。bump 不要。

## Considered Options

- `Command::CheckDiagnostics { paths: Vec }` に拡張(daemon で並列/直列 settle):
  却下 — settle は 1 ファイルあたり最大約 10 秒(ADR-0032)で、daemon 側に
  ループと集約を持つと応答が N 倍遅くなる上、プロトコル bump が必要。
  CLI 側ループでも「1 コマンドで一括検証」という利用者は満たせ、失敗時は
  exit 2 で分岐でき、成功分の JSON は残る。将来「workspace 全体」が必要に
  なった時点で再評価する。
- ディレクトリ/glob 指定: 却下 — 検証対象は明確なファイル集合(編集した
  ファイル)であり、glob 解決はシェルに委ねればよい。

## Consequences

- エージェントは `minas check lib.rs other.rs` で 1 往復に近い形で複数
  ファイルを検証できる(実際はパス数分の往復 — daemon 対話がローカル
  ソケットのため実用上十分)。
- 出力が配列になるため、単一パス時も後方互換のため配列で統一(JSON の意味が
  パス数で変わらない)。
- skill の check 説明を複数パス対応に更新。
## 追記（2026-09-12、agent 実運用のフィードバック）

複数パスの間で **1 つでも取得に失敗すると、stdout の配列からそのパスが消えていた**
（失敗は stderr だけ）。両方失敗すると stdout は `[]` になり、エージェントは
「診断なし（クリーン未確認）」と「取得失敗」を区別できない — 実際に別セッションの
エージェントが `minas check src/lib.rs src/error.rs` で `[]` + 2 行の LSP error を
受け、「診断なし」と誤読しかけた。

失敗したパスも配列の 1 要素として返す。成功エントリと同じキーに加えて `error`
（`{"path","total":0,"diagnostics":[],"settled":false,"error":"..."}`）を持つ。
`[]` は「paths が空」以外では起こらない = 空配列は失敗の合図にならない。
exit コードは不変（分類は `error` の文面と同じ、`check_exit_code`）。
