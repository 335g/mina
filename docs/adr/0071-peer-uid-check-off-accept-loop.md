# peer uid 検査を accept ループの外へ出す — 全 `minas` 呼び出しの ~68ms 固定費を消す

iteration #9 の計時ログ（ADR-0056）で、L0 の**すべての flow の wall に
`minas` 呼び出し 1 回あたり ~65ms の固定費**が乗っていることが判明した。デーモンの
応答そのものは 0.27ms（Python で同一プロトコルを直叩き）なので、費用は接続経路にあった:

1. `minas` の `open_one_shot` は「デーモンが生きているか」の確認に**捨てる接続**を
   1 本開き、`conn::connect` が成功したら即 close してから、`open_session` で本命の
   接続を張り直していた（1 コマンド = 2 接続）。
2. `minad` の `accept_loop` は接続ごとの **peer uid 検査（MEDIUM-3）を accept ループの
   中**で行い、`stream.peer_cred()` が ENOTCONN（＝相手が既に close した捨て接続。
   macOS では accept 直後の正当な接続でも一瞬 ENOTCONN になる）のとき
   `5ms × 最大 10 回` sleep する。この間 accept が止まるため、直後に到着した本命接続は
   backlog に座ったまま**最初の往復が ~68ms 遅れる**。

iteration #10 の A/B（Python プローブ、4 回再現）:

| 接続パターン | デーモン（修正前） | デーモン（修正後） |
|---|---|---|
| 単一接続（Hello + GetServerInfo） | 0.71–0.95ms | 0.43–0.52ms |
| 捨て接続 → 本命接続 | **68–72ms** | 0.52ms |

Status: accepted

## Decision

**2 箇所を両方直す**（片方だけでも固定費は消えるが、役割が違う）。

- **`minad::accept_loop`**: accept 直後に `tokio::spawn` し、peer uid 検査を
  **接続タスクの先頭**で行う。accept ループは accept と spawn だけになる。
  - fail closed は維持（uid が取れない・不一致なら stream を drop して即切断）。
    検査の**位置**が変わるだけで契約は変えない。
  - 接続スロット（`MAX_CONNECTIONS=4`）は検査通過後に取得する（従来どおり
    拒否された接続はスロットを消費しない）。accept は止まらないので、スロット枯渇時も
    カーネルの backlog で待ち、タスクが acquire で待つ形になる。
  - `NEXT_CONN_ID` は検査通過後に採番する（拒否された接続は id を消費しない）。
- **`minas::open_one_shot`**: 死活確認に開いた接続を**捨てずに本命**に使う
  （`connect` → 失敗時のみ spawn + `wait_ready` → `connect` → Hello）。
  1 コマンド = 1 接続になり、デーモン側の無駄な accept/drop も消える。

## Considered Options

- **デーモン側だけ直す / minas 側だけ直す**: 測定上はどちらか片方で固定費は消える
  （L0 `explore/lsp` 310ms → 82ms、`dump` 164 → 28、両修正でも同じ）。だが
  デーモン側は**他のクライアント**（古い minas・`MINAE_CLIENT_NAME` を付けた TUI・
  将来の接続）が connect→close するたびに accept が止まる穴なので、位置の修正が本質。
  minas 側は「接続 1 本/コマンド」という契約の整理と無駄な accept/drop の削除。
  両方入れるのは 20 行程度で、片方を将来消す理由が無い。
- **リトライの sleep を `tokio::time::sleep` から `yield_now` に変える**: 却下 —
  ENOTCONN が解けるのに必要な実時間を無駄に回し、accept を止めないという本質を
  直していない（accept ループの外に出すのが正しい）。
- **uid 検査を接続の最初のメッセージ（Hello）処理後に行う**: 却下 — 検査前に
  読み取りを始めることになり、「不一致なら何も読まない」という fail closed の形が崩れる。
- **捨て接続をやめずに、`minas` から捨て接続の close を遅らせる**: 却下 — 調査で
  見つかった症状（accept が止まる）をクライアント側のタイミング頼みにするだけ。

## Consequences

- `minas` 1 呼び出しの固定費が ~68ms → 0.5–9ms。L0 の flow は `calls` に比例して下がる
  （`explore/lsp` **310 → 82ms（−74%）**、`dump` **164 → 28ms（−83%）**、
  `minas info` 76.6 → 9.1ms）。`calls`/`out_B`/`equiv_B` は不変（契約は触らない）。
- **計測器側の穴も判明した**: L0 の flow の step コマンドは `minas` を名前で書いており、
  **PATH の minas**（インストール済みバイナリ）が実行されていた。`L0_MINAS` は
  `server_metrics` にしか効いておらず、**クライアント側の変更は L0 で一度も測られて
  いなかった**。`l0.py` の `run()` で `minas` を解決済みパスへ置換するよう修正した
  （`| minas` のパイプ形も同様）。§3 の旧参照値は「PATH minas」での値なので、
  新参照値とは out_B もずれる（例 `explore/lsp` 1017 → 1082 バイト）。
- L0 の step がビルド済みバイナリを使うようになったため、**今後は `cargo build` してから
  測る**という前提が計測器に組み込まれた（method.md の注意書きは据え置き）。
- cold の `verify-broken/check` は `clean-unverified`（`settled:false`・診断空・rc=0）を
  返し、期待（rc=2 + Syntax Error）に届かない。**この修正の前後で同じ**なので回帰ではなく、
  HEAD の別の穴（iteration #11 の候補）。
