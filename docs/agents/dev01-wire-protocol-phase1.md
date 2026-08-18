# dev01: PostgreSQL Wire Protocol フェーズ1 設計

ステータス: 設計(実装待ち) · 対象: `tmp/dev01` · 方式: 自前最小実装(方式A)

## 0. 目的

psql が dev01 に TCP 接続し、`SELECT 1` と既存の SELECT クエリを実行して結果を受け取れる最小の PostgreSQL ワイヤプロトコル(バージョン3.0)サーバを実装する。認証は trust(no-auth)、クエリは Simple Query プロトコル + テキスト結果フォーマットのみ。

- **やらないこと(今回は)**: SCRAM/MD5 認証、Extended Query(Parse/Bind/Execute)、COPY、TLS、レプリケーション
- **布石**: `error.rs` の SQLSTATE 構造を ErrorResponse の S/C フィールドにそのまま使う

## 1. メッセージフロー

### 1.1 接続確立(startup フェーズ)

```
psql                          dev01
  |-- StartupMessage (version=196608, user=..., database=...) -->|
  |                        [AuthenticationOk]                   |--|
  |<--[AuthenticationOk]--------------------------------------------|
  |                        [ParameterStatus x N]                 |--|  (任意,複数可)
  |                        [BackendKeyData]                      |--|
  |                        [ReadyForQuery (status='I')]          |--|
```

### 1.2 クエリサイクル(Simple Query)

```
psql                          dev01
  |-- Query('Q', "SELECT 1") -->|
  |                        [RowDescription ('T')]                |--|
  |                        [DataRow ('D') x N]                   |--|
  |                        [CommandComplete ('C')]               |--|
  |<--[ReadyForQuery ('Z')]-----------------------------------------|
```

- エラー時: `[ErrorResponse ('E')]` を送り、直後に `ReadyForQuery` へ復帰

### 1.3 終了

```
psql                          dev01
  |-- Terminate ('X') -->|
  (サーバは接続を閉じる)
```

## 2. メッセージのバイトレイアウト

共通: バックエンドメッセージは `Byte1(識別子) + Int32(selfを含む長さ) + 内容`。
フロントエンドの StartupMessage と SSLRequest のみ識別子がなく `Int32(長さ) + Int32(バージョン) + 内容` で始まる。

### StartupMessage (F: フロントエンド → サーバ)
```
Int32(8)          長さ(このフィールド含む)
Int32(196608)     バージョン 3.0 = 3<<16 | 0 = 196608
String(user)      パラメータ名(UTF-8, NUL終端)
String(値)        ...
String("")        パラメータ列の終端
```
必須パラメータ: `user`。他は `database`, `client_encoding`, `application_name` など。

### SSLRequest
```
Int32(8)  Int32(80877103)
```
今回は無視(psql が SSL を要求しても、'N'(no) を返して平文へフォールバックさせる)。

### AuthenticationOk (B)
```
Byte1('R')  Int32(8)  Int32(0)
```
認証要求コード 0 = 認証OK(trust)。長さは固定 8。

### ParameterStatus (B)
```
Byte1('S')
Int32                   長さ
String(パラメータ名)   例: "server_version"
String(値)             例: "16.3" (psql が要求)
```
psql が要求する代表パラメータ: `server_version`, `client_encoding`(UTF8), `DateStyle`(ISO), `standard_conforming_strings`(on)。最低限 `server_version` と `client_encoding` を返せば psql は進む。

### BackendKeyData (B)
```
Byte1('K')  Int32(12)  Int32(プロセスID)  Int32(シークレットキー)
```
キャンセル要求用。値を適当に返す(今回はキャンセル不要)。

### ReadyForQuery (B)
```
Byte1('Z')  Int32(5)  Byte1('I')|'T'|'E'
```
`I`=トランザクション外(アイドル), `T`=トランザクション内, `E`=失敗トランザクション内。
dev01 は `Engine.in_tx` から導出: in_tx=false → 'I', in_tx=true → 'T'。

### Query (F)
```
Byte1('Q')  Int32(長さ)  String(SQL文、NUL終端)
```
(SQL 文は既存の `parser::parse` にそのまま渡す)

### RowDescription (B)
```
Byte1('T')
Int32                   長さ
Int16                   N (フィールド数 = 列数)
N 回繰り返し:
  String   フィールド名(列名)
  Int32    テーブル OID(0 = 判別不可。dev01 は 0 でよい)
  Int16    テーブルの属性番号(0 = 判別不可)
  Int32    データ型 OID(下の対応表)
  Int16    型修飾子(デフォルト -1)
  Int16    フォーマットコード(0 = テキスト)
```
最低限: 名前 + 型OID + フォーマット0 があれば psql は表示できる。

### DataRow (B)
```
Byte1('D')
Int32                  長さ
Int16                  N (列数)
N 回繰り返し:
  Int32  列値の長さ(バイト数。NULL は -1)
  Byte[] 列値(テキスト、NUL 終端しない)
```
NULL は長さ -1 で表現。テキスト値は `Value::Display` をバイト列にしたもの。

### CommandComplete (B)
```
Byte1('C')  Int32(長さ)  String(コマンドタグ)
```
タグ例: `SELECT n`(n=行数), `CREATE TABLE`, `INSERT 0 n`, `DROP TABLE`。既存 `exec` の結果文字列から導出できる(行数が取れる SELECT/INSERT は対応処理)。

### ErrorResponse (B)
```
Byte1('E')  Int32(長さ)
  フィールドの繰り返し(順不同):
    Byte1(フィールドコード)  String(値)
  Byte1(0)  終端
```
主要フィールドコード:
- `S` = 重大度(severity): "ERROR"
- `C` = SQLSTATEコード(5文字): 既存 `DbError.sqlstate` をそのまま
- `M` = メッセージ: 既存 `DbError.message` をそのまま
- `P` = エラー位置(バイトオフセット、任意)

### Terminate (F)
```
Byte1('X')  Int32(4)
```

## 3. 型 OID 対応表(必要最小)

PostgreSQL の組み込み型 OID。RowDescription のデータ型 OID フィールドに使う。

| dev01 ColumnType/Value | 型名 | OID |
|---|---|---|
| Int | int4 | 23 |
| Float | float8 | 701 |
| Bool | bool | 16 |
| Text | text | 25 |
| (NULL 単独はカラム型から) | — | 型により上表 |

注意: `Value::Null` の行は RowDescription では列型 OID を使う(DataRow 側で長さ -1 が NULL を表す)。NULL 専用 OID はない。

参考: PostgreSQL 主要型 OID(int2=21, int4=23, int8=20, float4=700, float8=701, bool=16, text=25, varchar=1043, date=1082, timestamp=1114)。dev01 が持つ4型分だけで十分。

## 4. Engine の複数セッション共有設計

### 現状
`Engine { db: DbFile, in_tx: bool, tx_backup: Option<DbFile> }`
トランザクションは「Engine が1つ → その中に tx_backup クローン」という構造。

### サーバ化での整合
- サーバは**共有 Engine を1つ**持ち、全セッションがそれを参照する。
- セッションは TCP 接続ごとに1つ生成され、軽量な `Session` コンテキストを持つ:
  ```
  struct Server {
      shared: Arc<Mutex<Engine>>,   // 全セッションで共有
  }
  struct Session {
      id: usize,                    // セッション識別(ログ用)
      // トランザクション状態は Engine.in_tx / tx_backup が保持
  }
  ```
- **未コミット可視性の境界**: `in_tx` / `tx_backup` は Engine に1組しかないため、**同時に1セッションのみトランザクション可能**。これは現行の単一ライター前提と整合する。2セッション目が BEGIN したら「既に別セッションがトランザクション中」をエラーにする(または単一ライターロック)。
- README にもある「単一プロセス前提」を、ワイヤサーバでは「単一ライター+トランザクションは排他」として拡張。

### セッションごとの可視性(今後の課題として明記)
- REPL の「同じ Engine をずっと持つ」= 自分の変更が見える、を TCP セッションでも維持するには、共有 Engine が正しい。
- ただし、複数セッションで「他セッションの未コミットを見せない」を徹底するなら、将来はセッション別スナップショット(READ COMMITTED 相当)が必要。**フェーズ1では共有 Engine で十分**(単一クライアント/単一ライター前提)。

## 5. 実装ファイル構成案

既存の `parser.rs` / `exec.rs` / `store.rs` に **触れない** 新規配置:

```
tmp/dev01/src/
  server.rs       # 新規: TCP リスナー、セッションループ、メッセージの受信/送信
  wire.rs         # 新規: メッセージのバイトコードック(encode/decode)、メッセージ型定義
  oid.rs          # 新規: ColumnType → OID 対応表 + 型名
  main.rs         # 変更: --serve <addr> オプションを追加(server.rs を起動)
```

- `wire.rs` は「純粋なバイト変換」に徹し、SQL 解釈は既存 parser/exec へ委譲する(単一責任)。
- `server.rs` は tokio(既存依存)で TCP を回し、`Engine` を Arc<Mutex> で共有。
- dev01 の依存は tokio を既に使えるため、**追加依存なし**。

### エラー変換(Phase1)
```
exec::DbError { sqlstate, message }  →  ErrorResponse の 'S'='ERROR', 'C'=sqlstate, 'M'=message
```
既存 error.rs の SQLSTATE 構造をそのまま載せる。

## 6. 実装順序(2〜3日目安)

1. `wire.rs` を書く: メッセージ型(Startup, Auth, ReadyForQuery, RowDescription, DataRow, CommandComplete, ErrorResponse)と encode。単体テストでバイト列を検証。
2. `server.rs` で接続確立フロー(startup → AuthOk → ParameterStatus → BackendKeyData → ReadyForQuery)。
3. Simple Query サイクル: `Q` 受信 → parse/execute → RowDescription + DataRow + CommandComplete + ReadyForQuery。
4. ErrorResponse への SQLSTATE 載せ。
5. `main.rs` に `--serve` を追加。
6. 手動確認: `psql -h 127.0.0.1 -p <port> -U dev01` で表作成・INSERT・SELECT・DROP を実行。

## 7. テスト方針

- `wire.rs`: 各メッセージの encode が正しいバイト列を出す網羅テスト(長さ・フィールド数・NUL終端)。
- 統合: TCP で self-serve して、bash/psql 相当のクライアント(dev01 内にテスト用ミニクライアントを書くか、`nc`+バイト比較)で startup→query を確認。
  - 実 psql が手元にあれば `psql` で e2e。なければテストクライアントで代替。
- RowDescription のフィールド数・型 OID の検証テストを必ず含める。

## 8. 参照

- PostgreSQL 54.7 Message Formats: https://www.postgresql.org/docs/current/protocol-message-formats.html
- PostgreSQL 54.2 Message Flow: https://www.postgresql.org/docs/current/protocol-flow.html
- 最小サーバ手順(Python): https://ivdl.co.za/2024/03/02/pretending-to-be-postgresql-part-one
- SQLSTATE コード: https://www.postgresql.org/docs/current/errcodes-appendix.html
- pgwire(将来の載せ替え先): https://github.com/sunng87/pgwire
