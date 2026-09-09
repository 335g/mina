//! minae の daemon/client IPC の wire 型。
//!
//! 依存を持たない（serde のみ）。daemon とクライアントの両方が参照する。
//! フレーミングは NDJSON: 1メッセージ = JSON 1行（`docs/adr/0006-state-snapshot-ipc.md`）。

use serde::{Deserialize, Serialize};

/// IPC プロトコルのバージョン。**wire 形式が変わったら必ず上げる**。
///
/// ソケットパスに埋め込まれ（`minae-{PROTOCOL_VERSION}.sock`）、古い daemon が
/// 新しいクライアントに拾われるのを防ぐ（古い daemon は古いソケットに残り、
/// 新クライアントは新しいソケットで新 daemon を自動起動する — クライアントの
/// `ensure_daemon` と合わせて、バージョン不一致の応答を一切受けない）。
///
/// v4: `StateSnapshot.highlights` が全文ではなく可視範囲（first_line から
/// viewport_height 行。ADR-0021）になった。
/// v5: 編集コマンド拡張 — `Command::Change`（Helix の `c`）、
/// `DeleteWordBackward`/`DeleteWordForward`（単語削除）、`Movement::WordEnd` /
/// `LineStart`/`LineEnd`（単語末尾・行頭/行末）。追加のみで後方互換だが、
/// 古い daemon に新コマンドを送っても動作しないため version を上げる。
/// v6: `Command::InsertAtLineEnd` / `InsertAtLineStart`（Helix の `A`/`I`。
/// ADR-0023）。
/// v7: `Command::Rename` / `References`（ADR-0029）。
/// v8: `Command::Outline` / `EnclosingSymbol`（ADR-0031）。シンボルの階層リストを
/// 全文なしで返す Outline と、位置を囲む記号の範囲を返す EnclosingSymbol。
/// v9: `Command::HoverAt` / `WorkspaceSymbol` / `CheckDiagnostics`（ADR-0032）。
/// 位置の hover（型・シグネチャ）、ワークスペース内シンボル検索、診断 settle 待ち +
/// コンパクト診断返却の3コマンドを追加。追加のみで後方互換だが、古い daemon に
/// 新コマンドを送っても動作しないため version を上げる。
/// v10: `Command::SelectLine`（Normal の `x` を Helix 流のカーソル行選択へ）。
/// v11: Helix キーマップ対応 — 検索（`Search`/`SearchNext`/`SearchSelection`）、
/// `SelectAll`（`%`）、挿入補完（`Append`/`OpenBelow`/`OpenAbove` = `a`/`o`/`O`）、
/// `Replace`（`r`）、`ExtendLineBelow`（`x`）、`ScrollHalf`（C-d/C-u 半ページ）、
/// `Movement::FirstNonWhitespace`（`g s`）。追加のみだが wire を広げるため version を上げる。
/// v12: 接続別フォーカス分離 + 活動可視化（ADR-0037/0038/0039）— `Hello` に
/// `name`（自己申告ラベル）、`StateSnapshot` に `activity`（操作試行の成功/失敗履歴）、
/// `EventKind::Rename` を追加。bump 方式（ADR-0039）: 新旧は別ソケットで交わらない。
/// v13: 基準 root の登録/解除（#49 比較閲覧 Mode 1）— `Command::RegisterBaseRoot` /
/// `UnregisterBaseRoot`、`ServerMessage::ServerInfo` に `base_roots` を追加。
/// 読取りコマンドの形状は不変（パス指定で基準側に効く）。bump 方式（ADR-0039）。
/// v14: レビューコメントの daemon 保持（#50）— `Command::AddReviewComment` /
/// `ListReviewComments` / `ClearReviewComments`、`ServerMessage::ReviewComments`、
/// `StateSnapshot` に `review_comment_count`（件数のみ・全文は List 応答だけ）。
/// bump 方式（ADR-0039）。
/// v15: 基準診断の常駐（#52・a2）— `StateSnapshot` に `base_diagnostics`
/// （注目文書の基準側対応物の診断）、`Command::RegisterBaseRoot` に `repo`
/// （対応付け用・省略可）。bump 方式（ADR-0039）。
/// v16: `ServerMessage::Check` に `settled`（クリーン確定の根拠）を追加
/// （ADR-0045）。応答 wire の変更のため bump。
/// v17: `CheckDiagnostic` に `col`（1-origin 行内列 — at/peek/hover の住所）
/// を追加（ADR-0047）。応答 wire の変更のため bump。
/// v18: `Command::ReadPath`（パス指定の軽量テキスト読み。ADR-0048）と
/// `Command::OutlineRecursive` + `ServerMessage::Outline.truncated`（モジュール
/// 横断 outline。ADR-0049）、`ServerMetrics.read_total` / `read_bytes` を追加。
/// コマンド・応答の追加のため bump。
pub const PROTOCOL_VERSION: u32 = 18;

/// daemon が bind するソケットのパス。
///
/// プロトコルバージョンをソケット名に埋める（`minae-{PROTOCOL_VERSION}.sock`）:
/// プロトコルが変わると古い daemon は別ソケットに残り、新クライアントは新 daemon を
/// 自動起動する（クライアント側の ensure と合わせて、バージョン不一致の応答を
/// 一切受けない）。→ 上記 [`PROTOCOL_VERSION`] の doc 参照。
///
/// ソケット名は wire 契約の一部（ADR-0034）: daemon とクライアントの両方が
/// この関数を使う。
///
/// ponytail: uid をファイル名に入れていない（単一ユーザ前提）。アクセス制御
/// は socket の 0600 化 + 接続時の peer uid 検証で行う。複数ユーザを同時に
/// 扱う必要が出たら `<dir>/minae-<uid>.sock` にする。
pub fn socket_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("minae-{PROTOCOL_VERSION}.sock"))
}

/// 編集モード（wire 型。minae-view の Mode とは別に持つ — protocol は依存を持たない）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Select,
}

/// クライアントが daemon へ送るコマンド。
///
/// 追加は後方互換（serde の外部タグ付け enum）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    /// 現在の状態スナップショットを要求する。
    GetState,
    /// 世代が `generation` を超えるまでブロックし、超えた時点のスナップショットを
    /// 返す（ADR-0012 #12）。状態は変えない。エージェントのモニタリングが
    /// ポーリングの代わりに1リクエストで変化を待てる。
    WaitFor { generation: u64 },
    /// ファイルを読み込んで開く（読み込み失敗は StateSnapshot.status に報告）。
    Open { path: String },
    /// 選択を点に潰して移動する。ただし単語移動（[`Movement::Word`] /
    /// [`Movement::WordEnd`]）は Helix 流に anchor を保持し、移動した分を
    /// 選択状態にする（単語途中の b で現在の単語が選択される）。
    Move {
        movement: Movement,
        direction: Direction,
    },
    /// anchor を保ったまま head を移動する（選択の拡張・縮小）。
    Extend {
        movement: Movement,
        direction: Direction,
    },
    /// 文書の先頭/末尾へ絶対移動する。
    Goto { target: GotoTarget },
    /// 表示範囲をページ単位でスクロールする（正で下）。高さは daemon 側が知っている。
    Scroll { pages: isize },
    /// モードを切り替える。
    SetMode { mode: Mode },
    /// ターミナルの表示高さを通知する（カーソル追従スクロールに使う）。
    SetViewport { height: usize },
    /// 選択（またはカーソル位置）にテキストを挿入する。
    Insert { text: String },
    /// 後方削除（Backspace 相当）。
    DeleteBackward,
    /// 前方削除（Delete キー相当）。
    DeleteForward,
    /// 後方単語削除（Alt-Backspace / Ctrl-w 相当）。
    DeleteWordBackward,
    /// 前方単語削除（Alt-d 相当）。
    DeleteWordForward,
    /// 選択（またはカーソル位置）を削除する。
    DeleteRange,
    /// 各 Range を head の行全体（末尾改行は含まない）へ広げる（行選択）。
    /// head は行末（改行の直前）に置く。Select モードへの移行はしない。
    SelectLine,
    /// 各 Range を行選択の形へ整えてから、その下の行の末尾（末尾改行含む）
    /// まで head を拡張する（Helix の `x` = `extend_line_below`。連打で
    /// 選択に行が追加される）。
    ExtendLineBelow,
    /// 選択全体を文書全体（0..len）の1 Range へ置き換える（Helix の `%` =
    /// `select_all`）。
    SelectAll,
    /// ヘッドを1書記素前へ進めて Insert モードへ入る（Helix の `a` =
    /// `append_mode`）。選択がある場合は選択の直後に挿入する。
    Append,
    /// ヘッドの行の下に空行を開いて Insert モードへ入る（Helix の `o`）。
    OpenBelow,
    /// ヘッドの行の上に空行を開いて Insert モードへ入る（Helix の `O`）。
    OpenAbove,
    /// 選択（カーソルの場合はその位置の1文字）を指定テキストで置換し、
    /// Normal モードのまま置換後テキストの直後にカーソルを置く（Helix の `r`）。
    Replace { text: String },
    /// `query` をカーソル位置から検索し、一致を選択する（Helix の `/` `?`）。
    /// 見つからなければ文書端を越えて折り返す。検索クエリと結果位置は
    /// daemon に保持され、後の [`Command::SearchNext`] が前進/後退する。
    /// クエリが前回と同一の場合は再検索（カーソル位置から）。
    Search { query: String, direction: Direction },
    /// 前回の検索（[`Command::Search`]）の結果から1つ進める（Helix の `n`/`N`）。
    /// 検索履歴が無ければ no-op（status で報告）。
    SearchNext { direction: Direction },
    /// 選択テキスト（カーソルの場合はその位置の単語）の全一致を複数カーソル
    /// として選択する（Helix の `*` = `search_selection`）。一致が無ければ no-op。
    SearchSelection,
    /// 表示範囲を半分ページ（viewport 高さの半分）スクロールする（Helix の
    /// C-d/C-u = `page_cursor_half_down/up`）。カーソルも同じだけ動かす。
    ScrollHalf { direction: Direction },
    /// カーソル位置から行頭まで削除する（Helix の Insert モード C-u =
    /// `kill_to_line_start`）。
    KillToLineStart,
    /// カーソル位置から行末まで削除する（Helix の Insert モード C-k =
    /// `kill_to_line_end`）。
    KillToLineEnd,
    /// 選択（またはカーソル位置）を削除して Insert モードへ入る（Helix の `c`）。
    /// カーソル上では削除なしで Insert モードに入るだけ。削除は undo グループの外。
    Change,
    /// 各 Range を head の行の行末（改行の直前）へ点に潰して Insert モードへ
    /// 入る（Helix の `A`。ADR-0023）。Select でも折りたたむ。
    InsertAtLineEnd,
    /// 各 Range を head の行の最初の非空白文字（空白のみの行は列 0）へ点に潰して
    /// Insert モードへ入る（Helix の `I`。ADR-0023）。Select でも折りたたむ。
    InsertAtLineStart,
    /// 直近の変更グループを元に戻す。
    Undo,
    /// 直近に undo された変更グループをやり直す。
    Redo,
    /// 現在の文書をファイルに書き込む（結果は status に報告）。
    Save,
    /// フォーカス文書を閉じる。残りの文書があればそこへ移り、無ければ空状態に戻る（ADR-0015）。
    Close,
    /// サーバ（daemon）のビルド世代・累積メトリクスを開示する（読み取り専用。
    /// issue #27/D1）。応答は [`ServerMessage::ServerInfo`]。スナップショットを
    /// 運ばず、世代・イベント・push を進めない。
    GetServerInfo,
    /// 任意パスの inlay hint をテキストなしで取得する（ADR-0020。読み取り専用）。
    /// 応答は [`ServerMessage::Hints`]。未開パスは daemon がディスクから読む。
    GetInlayHints { path: String },
    /// カーソル位置のシンボル定義を確認用スニペットとして返す（読み取り専用）。
    /// 定義にジャンプせず、応答スナップショットの `peek` フィールドに載る。
    PeekDefinition,
    /// 指定位置（1-origin 行:列）のシンボル定義を、全文を読まずに確認する
    /// （読み取り専用。ADR-0025）。応答は [`ServerMessage::Peek`]（軽量 —
    /// スナップショット＝全文は返さない）。エージェントのトークン削減経路。
    PeekDefinitionAt {
        path: String,
        /// 1-origin 行番号。
        line: u32,
        /// 1-origin 列番号（文字数単位）。
        col: u32,
    },
    /// シンボルの意味リネーム（ADR-0029）。内容指定: `old` の最初の識別子出現を
    /// daemon が解決し、LSP の `textDocument/rename` で全参照（複数ファイル含む）を
    /// 置換して保存する。応答は全文を運ばない軽量 [`ServerMessage::RenameResult`]。
    /// 読み取り専用でない（テキストを変える）ため、通常経路は headless の
    /// `session rename`（daemon は headless ゲートをこのコマンドに限って解放する）。
    Rename {
        path: String,
        old: String,
        new: String,
    },
    /// シンボルの参照位置の列挙（読み取り専用。ADR-0029）。`old` の最初の識別子
    /// 出現を解決し、LSP の `textDocument/references` で全参照位置を返す。
    /// 応答は全文を運ばない軽量 [`ServerMessage::ReferencesResult`]。
    References { path: String, old: String },
    /// シンボルの階層リストの取得（読み取り専用。ADR-0031）。任意パスの文書を
    /// LSP の `textDocument/documentSymbol` で解析し、名前・種別・範囲（選択範囲
    /// 含む）のツリーを返す。応答は全文を運ばない軽量 [`ServerMessage::Outline`]。
    /// エージェントが全文を読まずに構造を把握し、得られた範囲をその後の
    /// 読み・編集（range-read / apply）の住所にするための経路。
    Outline { path: String },
    /// ファイル分割モジュールを辿る Outline（読み取り専用。ADR-0049）。
    /// `depth`（>= 1）まで module 宣言 → 定義ファイルの再帰で横断し、子
    /// モジュールのシンボルを `children` に埋めて返す。応答形状は既存
    /// [`ServerMessage::Outline`] を再利用（`truncated` で打ち切りを通知）。
    OutlineRecursive { path: String, depth: u32 },
    /// パス指定の軽量テキスト読み（読み取り専用。ADR-0048）。バッファ非依存・
    /// LSP 非依存 — フォーカス・世代・push を動かさず、任意パスのテキストだけを
    /// 返す（開文書優先・未保存編集込み・ディスク fallback）。応答は全文
    /// スナップショットを運ばない軽量 [`ServerMessage::ReadPath`]。`--lines` の
    /// 範囲切出しは CLI 側で行う。
    ReadPath { path: String },
    /// 指定位置を囲むシンボルの取得（読み取り専用。ADR-0031）。`line:col`
    /// （1-origin）から、その位置を含む最も深い記号の名前・種別・正確な範囲を
    /// 返す（`documentSymbol` の selectionRange 由来）。エージェントが全文を
    /// 読まずに「この関数を丸ごと置換する」等の編集範囲を得るための経路。
    /// 応答は全文を運ばない軽量 [`ServerMessage::EnclosingSymbol`]。
    EnclosingSymbol {
        path: String,
        /// 1-origin 行番号。
        line: u32,
        /// 1-origin 列番号（文字数単位）。
        col: u32,
    },
    /// 指定位置（1-origin 行:列）の hover 情報（型・シグネチャ・doc）を全文を
    /// 読まずに取得する（読み取り専用。ADR-0032）。応答は [`ServerMessage::Hover`]
    /// （軽量 — スナップショット＝全文は返さない）。hover の無い位置
    /// （空白・コメント等）は空テキストで応答する。
    HoverAt {
        path: String,
        /// 1-origin 行番号。
        line: u32,
        /// 1-origin 列番号（文字数単位）。
        col: u32,
    },
    /// ワークスペース内のシンボル検索（読み取り専用。ADR-0032）。`path` で
    /// ワークスペース root（LSP セッション）を決め、`query` を `workspace/symbol`
    /// に投げる。応答は全文を運ばない軽量 [`ServerMessage::WorkspaceSymbols`]。
    /// 「どこで定義されているか」の探索を rg の代わりに 1 往復で済ませる経路。
    WorkspaceSymbol {
        /// ワークスペース root を決めるファイル（root 内の任意のパス）。
        path: String,
        /// 検索クエリ（空不可。LSP は空クエリを拒否する）。
        query: String,
    },
    /// 対象パスの診断が安定するまで待ち、診断だけをコンパクトに返す（読み取り
    /// 専用。ADR-0032）。全文を運ばない軽量 [`ServerMessage::Check`] — エージェント
    /// の「編集→検証」ループを 1 コマンドに圧縮する（wait + get + JSON パースの
    /// 代替）。診断の反映は generation を進めないため、内部で settle を待つ。
    CheckDiagnostics { path: String },
    /// 基準 root の登録（比較閲覧 Mode 1・#49・v13）。`root` 配下を基準側として
    /// 管理する: LSP セッションを確保し、テキスト変更を拒否し、解除時に
    /// セッションとキャッシュを破棄する。読取りコマンドは従来通りパス指定で
    /// 効く（形状追加なし）。root はクライアントが git worktree で用意した
    /// 基準コミットの実体。冪等（再登録は commit を更新する）。
    /// v15: `repo`（対応する live リポジトリ）があれば基準診断の対応付けに
    /// 使う（#52・a2。無ければ基準診断は付かない）。
    RegisterBaseRoot {
        /// 基準 root（絶対パス）。
        root: String,
        /// 基準コミット ID（表示・診断用。daemon は git を読まない）。
        commit: String,
        /// 対応する live リポジトリルート（絶対パス・#52・省略可）。
        #[serde(default)]
        repo: Option<String>,
    },
    /// 基準 root の登録解除（#49・v13）。LSP セッションを破棄し、配下パスの
    /// キャッシュ（outline/hints）を捨てる。存在しない root は無視する。
    UnregisterBaseRoot {
        /// 基準 root（絶対パス）。
        root: String,
    },
    /// レビューコメントの追加/更新（#50・v14）。差分アンカー
    /// （側・パス・行・行スナップショット＋コメント本文）を daemon 共有の箱に
    /// 置く。同一アンカー（パス・側・行）は上書き（トグルの編集が重複を
    /// 生まない）。空本文はそのアンカーの削除（per-anchor 削除はこの合体で
    /// 賄い、専用コマンドは作らない）。追加・更新・削除いずれも世代を進める。
    AddReviewComment {
        /// 対象ファイル（絶対パス。基準側は worktree 配下、現在側は実ファイル）。
        path: String,
        /// どちらの側を指すか。
        side: ReviewSide,
        /// 1-origin 行番号（追加時点）。
        line: u32,
        /// 対象行の内容（追加時点のスナップショット。stale 判定用）。
        snippet: String,
        /// コメント本文（空 = 削除）。
        body: String,
        /// ピン留めした基準コミット ID（来歴表示用。daemon は git を読まない）。
        base: String,
    },
    /// レビューコメントの一覧取得（#50・v14・読み取り専用）。応答は全文付きの
    /// 軽量 [`ServerMessage::ReviewComments`] — 現在テキストと照合した
    /// `stale`＋`resolved_line` を付けて返す（保存値は書き換えない）。
    /// headless ゲートの例外（`minas review` の抽出経路）。
    ListReviewComments,
    /// レビューコメントの全消し（#50・v14）。世代を進める。
    ClearReviewComments,
}

/// 移動の種類（wire 型）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Movement {
    Char,
    Line,
    /// 単語の先頭（Helix の `w`/`b`。Move では anchor を保持して選択を残す）。
    Word,
    /// 単語の末尾（Helix の `e`。Move では anchor を保持して選択を残す）。
    WordEnd,
    /// 行頭（列 0）。
    LineStart,
    /// 行末（改行の直前）。
    LineEnd,
    /// 行の最初の非空白文字（空白のみの行は列 0）。`g s`。
    FirstNonWhitespace,
}

/// 移動方向（wire 型）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Forward,
    Backward,
}

/// 絶対移動の目標。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GotoTarget {
    DocumentStart,
    DocumentEnd,
}

/// 選択範囲（wire 型。anchor/head は char インデックス）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub anchor: usize,
    pub head: usize,
}

/// 位置指定の文書編集（ADR-0011）。選択を読まない・変えない。
///
/// フォーカス文書に対して明示 char range で作用する:
/// - insert: `start == end`
/// - delete: `text` が空
/// `checksum` はクライアントが最後に読んだ文書全文（UTF-8 バイト列）の
/// FNV-1a 64。不一致（読み取り後に文書が変化）なら daemon は状態を変えず
/// status で拒否する。
///
/// `expected_text` は局所検証用（B2）: `Some` なら checksum に加えて対象
/// 範囲（start..end）の現テキストがこれと一致することも検証される。位置の
/// ずれは checksum（全文）では検出できず expected_text（局所）で検出する
/// ため、両者は相補的。`None` なら checksum のみ（従来どおり）。オプショナル
/// 追加なのでプロトコル破壊的変更なし（欠落フィールドは None として扱う）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub checksum: u64,
    /// 対象範囲に期待する現テキスト（局所検証）。None でチェックなし。
    #[serde(default)]
    pub expected_text: Option<String>,
}

/// FNV-1a 64 ハッシュ（[`DocumentEdit`] のチェックサム検証用）。
///
/// 安定性のため固定実装（`std::collections::DefaultHasher` は Rust バージョン
/// 間で非安定）。検証用なので暗号学的強度は不要。
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// daemon → client のメッセージ（ADR-0013）。NDJSON 1行 = メッセージ1件。
///
/// コマンドへの応答（[`ServerMessage::Response`]）と、他クライアントの変更に
/// よるサーバー発の状態通知（[`ServerMessage::Push`]）をタグで区別する。
/// 従来の応答（タグなしの素の StateSnapshot）を置き換える（CLI の互換性は
/// 考慮しない決定 — シリアライズ形状が単一で仕様が単純になる）。
/// 登録中の基準 root 1 件（#49・v13。`ServerMessage::ServerInfo` に載る）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseRootInfo {
    /// 基準 root（絶対パス）。
    pub root: String,
    /// 基準コミット ID（クライアント申告。表示・診断用）。
    pub commit: String,
}

/// レビューコメントが指す側（#50・v14）。wire 形式は小文字。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSide {
    /// 基準側（worktree 配下の不変テキスト）。
    Base,
    /// 現在側（実ファイル。編集でずれる）。
    Current,
}

/// daemon が保持するレビューコメント 1 件（#50・v14）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    /// 対象ファイル（絶対パス）。
    pub path: String,
    /// どちらの側を指すか。
    pub side: ReviewSide,
    /// 1-origin 行番号（追加時点）。
    pub line: u32,
    /// 対象行の内容（追加時点のスナップショット。stale 判定用）。
    pub snippet: String,
    /// コメント本文。
    pub body: String,
    /// ピン留めした基準コミット ID（来歴表示用）。
    pub base: String,
    /// 追加時の世代（エージェントが状態と対応付けるための目印）。
    pub generation: u64,
}

/// [`ServerMessage::ReviewComments`] に載る 1 件（#50・v14）。保存値は
/// 書き換えず、照合結果（`stale`＋`resolved_line`）を添える — ずれた先の
/// 判断は AI が行う。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCommentView {
    /// 対象ファイル（絶対パス）。
    pub path: String,
    /// どちらの側を指すか。
    pub side: ReviewSide,
    /// 1-origin 行番号（追加時点のまま）。
    pub line: u32,
    /// 照合時点の解決行（1-origin。一致時は `line` と同じ）。
    pub resolved_line: u32,
    /// 解決行の内容が `snippet` と食い違っているか。
    pub stale: bool,
    /// 対象行の内容（追加時点のスナップショット）。
    pub snippet: String,
    /// コメント本文。
    pub body: String,
    /// ピン留めした基準コミット ID。
    pub base: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// クライアントのコマンドに対する応答。
    Response { snapshot: StateSnapshot },
    /// サーバーが能動的に通知する最新状態（他クライアントの変更など）。
    /// 購読（Interactive クライアント）にのみ届く。
    Push { snapshot: StateSnapshot },
    /// サーバ情報（[`Command::GetServerInfo`] の応答、issue #27）。スナップショット
    /// を運ばない軽量応答 — 古いビルドの daemon が新プロトコル項目を黙殺して
    /// いないか（silent ignore）を検知可能にするための開示。`generation` は
    /// daemon のビルド世代（Git commit hash）、`daemon_build_ts` はビルド日時
    /// （Unix 秒）。`metrics` は daemon 起動からの累積カウント。
    ServerInfo {
        /// daemon のビルド世代（Git commit hash。取得不可なら "unknown"）。
        generation: String,
        /// daemon のビルド日時（Unix 秒。注入不可なら 0）。
        daemon_build_ts: u64,
        /// daemon 起動からの累積メトリクス（効果検証用）。
        metrics: ServerMetrics,
        /// 登録中の基準 root（#49・v13）。読取り専用・ライフサイクル管理対象。
        base_roots: Vec<BaseRootInfo>,
    },
    /// [`Command::GetInlayHints`] の応答（ADR-0020）。エージェントが全文
    /// テキストを読まずに型構造（type / parameter ヒント）を参照するための経路。
    Hints {
        path: String,
        /// 応答時点の世代（エージェントが状態と対応付けるための目印）。
        generation: u64,
        hints: Vec<InlayHint>,
    },
    /// [`Command::PeekDefinitionAt`] の応答（ADR-0025）。エージェントが全文
    /// テキストを読まずに定義を参照するための軽量経路 — スナップショット
    /// （全文）を運ばない。`text` が空なら定義なし・LSP 非対応。
    Peek {
        /// 定義元ファイルのパス。
        path: String,
        /// 定義の開始行（1 始まり）。
        line: u32,
        /// 定義のスニペット（数行。改行区切り）。空なら定義が見つからなかった。
        text: String,
    },
    /// [`Command::Rename`] の応答（ADR-0029）。全文スナップショットを運ばない
    /// 軽量応答 — 影響範囲（ファイル数・編集数・変更ファイル一覧）だけを返し、
    /// エージェントが「意図通りか」を確認できるようにする。
    /// 失敗は `error: Some(…)` で表す（`files`/`edits` は 0）。
    RenameResult {
        /// 応答時点の世代（編集が適用されたため進んでいる）。
        generation: u64,
        /// 変更したファイル数。
        files: usize,
        /// 適用した編集の総数。
        edits: usize,
        /// 変更したファイルのパス一覧（相対表示用。絶対パス）。
        changed: Vec<String>,
        /// 失敗理由（成功時は None）。
        error: Option<String>,
    },
    /// [`Command::References`] の応答（ADR-0029）。参照位置の軽量一覧。
    /// 失敗は `error: Some(…)` で表す（`locations` は空）。
    ReferencesResult {
        /// 参照元のファイルパス。
        path: String,
        /// 参照位置（パス・0-origin 行番号の昇順）。
        locations: Vec<ReferenceLocation>,
        /// 参照の総数。
        total: usize,
        /// 失敗理由（成功時は None）。
        error: Option<String>,
    },
    /// [`Command::Outline`] / [`Command::OutlineRecursive`] の応答（ADR-0031 /
    /// 0049）。シンボルの階層ツリーを全文なしで返す軽量応答。失敗は
    /// `error: Some(…)` で表す（`symbols` は空）。
    Outline {
        /// 対象ファイルのパス。
        path: String,
        /// 応答時点の世代（エージェントが状態と対応付けるための目印）。
        generation: u64,
        /// シンボルの階層ツリー（位置昇順）。
        symbols: Vec<OutlineSymbol>,
        /// 失敗理由（成功時は None）。
        error: Option<String>,
        /// 再帰横断時（ADR-0049）、シンボル総数上限（500）に達して途中で
        /// 打ち切られたか。非再帰・完走時は false。
        #[serde(default)]
        truncated: bool,
    },
    /// [`Command::HoverAt`] の応答（ADR-0032）。指定位置の hover テキスト（型・
    /// シグネチャ・doc を連結・切り詰め）。全文スナップショットを運ばない軽量
    /// 応答。`text` が空なら hover なし（空白・コメント位置など）。失敗は
    /// `error: Some(…)` で表す。
    Hover {
        /// 対象ファイルのパス。
        path: String,
        /// 応答時点の世代（エージェントが状態と対応付けるための目印）。
        generation: u64,
        /// hover テキスト（空 = hover なし）。
        text: String,
        /// 失敗理由（成功時は None）。
        error: Option<String>,
    },
    /// [`Command::WorkspaceSymbol`] の応答（ADR-0032）。ワークスペース内の
    /// シンボル検索結果の軽量一覧（名前・種別・パス・1-origin 行番号のみ —
    /// 行の内容は渡さない。エージェントは位置から範囲 read で引く）。
    /// 失敗は `error: Some(…)` で表す（`symbols` は空）。
    WorkspaceSymbols {
        /// 応答時点の世代。
        generation: u64,
        /// ヒットしたシンボル一覧。
        symbols: Vec<WorkspaceSymbol>,
        /// 失敗理由（成功時は None）。
        error: Option<String>,
    },
    /// [`Command::CheckDiagnostics`] の応答（ADR-0032）。対象パスの診断を安定まで
    /// 待って返すコンパクト応答。全文を運ばない。失敗は `error: Some(…)` で表す
    /// （`diagnostics` は空）。
    Check {
        /// 対象ファイルのパス。
        path: String,
        /// 応答時点の世代。
        generation: u64,
        /// 診断の総数。
        total: usize,
        /// 診断（行番号・char 範囲・メッセージ）。クリーンなら空。
        diagnostics: Vec<CheckDiagnostic>,
        /// クリーン（空）が解析完了の確認済みか（ADR-0045）。`true` = 非空が
        /// 2 回連続で安定して確定。`false` = 予算切れで返った「クリーン**未確認**」
        /// （解析未完・クロスシンボル破壊の可能性。exit 0 のまま — クリーンと
        /// クリーン未確認は別物）。
        settled: bool,
        /// 失敗理由（成功時は None）。
        error: Option<String>,
    },
    /// [`Command::ReadPath`] の応答（ADR-0048）。パス指定の軽量テキスト読み。
    /// 全文スナップショットを運ばず `text` だけを返す。読み取り専用で
    /// 状態・世代は進めない（`generation` は応答時点の目印）。失敗は
    /// `error: Some(…)` で表す（`text` は空）。`--lines` の範囲切出しは
    /// CLI 側で行う。
    ReadPath {
        /// 対象ファイルのパス。
        path: String,
        /// 応答時点の世代。
        generation: u64,
        /// ファイルのテキスト（開文書優先・未保存編集込み、なければディスク読み）。
        text: String,
        /// 失敗理由（成功時は None）。
        error: Option<String>,
    },
    /// [`Command::EnclosingSymbol`] の応答（ADR-0031）。指定位置を囲む記号の
    /// 名前・種別・正確な範囲（選択範囲 = 名前トークン）を全文なしで返す軽量応答。
    /// 位置がどの記号にも含まれない・対象が読めない場合は `found: false`（
    /// `name` は空・`range`/`selection_range` は位置 0）。
    EnclosingSymbol {
        /// 対象ファイルのパス。
        path: String,
        /// 囲む記号の名前（見つからなければ空）。
        name: String,
        /// 記号の種別。
        kind: SymbolKind,
        /// 記号全体の範囲（char インデックス）。
        range: Range,
        /// 名前トークンの範囲（char インデックス）。
        selection_range: Range,
        /// 位置を囲む記号が見つかったか（`error` が None のときだけ意味を持つ）。
        found: bool,
        /// 失敗理由（成功時は None。`error` が Some なら `found` は false）。
        error: Option<String>,
    },
    /// [`Command::ListReviewComments`] の応答（#50・v14）。レビューコメントの
    /// 全文付き軽量一覧 — 全文スナップショットは運ばない。`minas review` の
    /// 抽出経路（AI が `stale` を見てずれた先を判断する）。
    ReviewComments {
        /// 応答時点の世代。
        generation: u64,
        /// コメント一覧（追加順）。
        comments: Vec<ReviewCommentView>,
    },
}

/// [`Command::WorkspaceSymbol`] の応答に含まれるシンボル 1 件（ADR-0032）。
/// パスと 1-origin 行番号のみ — 行の内容は渡さない（エージェントは位置から
/// 範囲 read で引く。T1 の原則）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// 定義元ファイルのパス（絶対パス）。
    pub path: String,
    /// 1-origin 行番号（シンボルの開始位置）。
    pub line: u32,
}

/// [`Command::CheckDiagnostics`] の応答に含まれる診断 1 件（ADR-0032）。
/// 1-origin 行番号（`--lines` の住所）・1-origin 行内列（at / peek / hover の
/// 住所。ADR-0047）・char 範囲（apply の住所）を全部載せる。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDiagnostic {
    pub severity: Severity,
    /// 1-origin 行番号（診断の開始位置）。
    pub line: u32,
    /// 1-origin 行内列（char 単位）。`at` / `peek` / `hover` の `<line>:<col>`
    /// 住所にそのまま渡せる（ADR-0047）。行頭の char を 1 とする。
    pub col: u32,
    /// char インデックス範囲（start..end）。
    pub start: usize,
    pub end: usize,
    pub message: String,
}

/// [`Command::References`] の応答に含まれる参照位置 1 件（ADR-0029）。
/// パスと 0-origin 行番号のみ — 行の内容は渡さない（エージェントは位置から
/// 範囲 read で引く。T1 の原則）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceLocation {
    pub path: String,
    pub line: u32,
}

/// シンボルの種別（ADR-0031）。LSP の `SymbolKind`（26 種）を proto 側で
/// 使う小さな集合に写像したもの — LSP を protocol に漏らさない（HighlightGroup
/// と同じ流儀）。未知の kind は [`SymbolKind::Other`] に潰す。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Module,
    Function,
    Method,
    Type,
    Enum,
    Constant,
    Variable,
    #[default]
    Other,
}

/// 記号 1 件（ADR-0031）。`range` は記号全体、`selection_range` は名前トークン
/// の範囲（char インデックス）。`children` は入れ子の記号（階層ツリー）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub range: Range,
    pub selection_range: Range,
    pub children: Vec<OutlineSymbol>,
}

/// daemon 起動からの累積メトリクス（[`ServerMessage::ServerInfo`] に載る。
/// issue #27 の効果検証用 — headless エージェントの「編集までの手順数・全文再読・
/// リトライ」を daemon 側の近似指標で観測する）。
///
/// セマンティクス: 成功数は `edits_total - edits_rejected_checksum -
/// edits_rejected_expected_text - edits_noop` で導出できる。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerMetrics {
    /// 受理した DocumentEdit の総数（拒否・no-op を含む）。
    pub edits_total: u64,
    /// checksum 不一致で拒否した数。
    pub edits_rejected_checksum: u64,
    /// expected_text 不一致で拒否した数。
    pub edits_rejected_expected_text: u64,
    /// 状態を変えなかった no-op 編集の数。
    pub edits_noop: u64,
    /// expected_text を使った編集の数（Some で届いた数。一致・不一致は問わない）。
    pub edits_expected_text_used: u64,
    /// GetState 実行回数（スナップショットは全文を返すため、
    /// headless の全文再読回数の近似になる）。
    pub get_state_total: u64,
    /// WaitFor 実行回数。
    pub wait_total: u64,
    /// Save 実行回数。
    pub save_total: u64,
    /// Outline 要求回数（ADR-0031）。
    pub outline_total: u64,
    /// Outline 応答の累積シリアライズ bytes（ADR-0031。トークン削減の実測用）。
    pub outline_bytes: u64,
    /// EnclosingSymbol 要求回数（ADR-0031）。
    pub symbol_range_total: u64,
    /// EnclosingSymbol 応答の累積シリアライズ bytes（ADR-0031）。
    pub symbol_range_bytes: u64,
    /// HoverAt 要求回数（ADR-0032）。
    pub hover_total: u64,
    /// HoverAt 応答の累積シリアライズ bytes（ADR-0032）。
    pub hover_bytes: u64,
    /// WorkspaceSymbol 要求回数（ADR-0032）。
    pub symbol_search_total: u64,
    /// WorkspaceSymbol 応答の累積シリアライズ bytes（ADR-0032）。
    pub symbol_search_bytes: u64,
    /// CheckDiagnostics 要求回数（ADR-0032）。
    pub check_total: u64,
    /// CheckDiagnostics 応答の累積シリアライズ bytes（ADR-0032）。
    pub check_bytes: u64,
    /// ReadPath 要求回数（ADR-0048）。パス指定の軽量読み — get_state_total
    /// （全文再読の近似）と対比して「全文を読まなくて済んだ量」を観測する。
    pub read_total: u64,
    /// ReadPath 応答の累積シリアライズ bytes（ADR-0048）。
    pub read_bytes: u64,
}

/// クライアント種別（接続開始時の [`Hello`] で宣言。イベントの source 判定に使う）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientKind {
    /// 対話型 TUI。
    Interactive,
    /// ヘッドレスクライアント（session exec / edit）。
    Headless,
}

/// 接続開始時のハンドシェイク（ADR-0012）。最初のメッセージでなければならない。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub kind: ClientKind,
    /// 最後の Interactive クライアント切断時に全 View のカーソルを先頭へ戻すか
    /// （ADR-0027）。デフォルト true — 旧クライアントの無指定 Hello も
    /// 「リセットする」として扱う。Headless には無意味（切断でリセットしない）。
    #[serde(default = "default_true")]
    pub reset_cursor_on_disconnect: bool,
    /// 自己申告ラベル（ADR-0038。HTTP User-Agent 的・未認証 — 「誰の操作か」を
    /// 見るための札）。`StateSnapshot.activity` の actor にスタンプされる。
    /// TUI は "tui"、minas は --name / MINAE_CLIENT_NAME（既定 "unknown"）。
    #[serde(default = "default_name")]
    pub name: String,
}

/// [`Hello::reset_cursor_on_disconnect`] のデフォルト（true）。
fn default_true() -> bool {
    true
}

/// [`Hello::name`] のデフォルト（"unknown"）。
fn default_name() -> String {
    "unknown".to_string()
}

/// イベントの発生源（ADR-0012）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSource {
    /// 対話型 TUI の操作。
    Interactive,
    /// ヘッドレスクライアントの操作。
    Headless,
    /// daemon 自身が検知した外部要因（ファイルの外部変更など）。
    External,
}

/// 状態変化イベントの種類（ADR-0012）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Insert,
    Delete,
    /// 位置指定編集（DocumentEdit）。
    ReplaceRange,
    Undo,
    Redo,
    Open,
    Save,
    /// フォーカス文書を閉じる（Command::Close）。
    Close,
    SetMode,
    /// フォーカス文書が外部ツールによって変更された。
    ExternalChange,
    /// 最後の Interactive クライアント切断時のカーソルリセット（ADR-0027）。
    SelectionReset,
    /// シンボルの意味リネーム（[`Command::Rename`]。ADR-0029）。
    Rename,
    /// 基準 root の登録/解除（#49 比較閲覧 Mode 1・v13）。
    BaseRoot,
    /// レビューコメントの追加/更新/削除・全消し（#50・v14）。
    ReviewComment,
}

/// 状態を変える操作1件の記録（ADR-0012）。bounded リングで保持され、
/// 全スナップショットに同梱される。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeEvent {
    /// このイベント適用後の世代。
    pub generation: u64,
    pub source: EventSource,
    pub kind: EventKind,
    /// 影響範囲（ある場合のみ）。
    pub range: Option<Range>,
    /// 挿入・置換テキスト（ある場合のみ）。
    pub text: Option<String>,
}

/// 診断の深刻度。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

/// 言語サーバが報告する問題（S3 で利用。StateSnapshot に含まれる）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub start: usize,
    pub end: usize,
    pub severity: Severity,
    pub message: String,
}

/// LSP の inlay hint（ADR-0020。CONTEXT.md の InlayHint 定義）。
///
/// 読み取り専用の注釈: Document のテキストの一部ではなく、選択・編集・undo・
/// checksum に一切関与しない。位置は char インデックス。`padding_left` /
/// `padding_right` はサーバ指定の前後空白（LSP の `paddingLeft` / `paddingRight`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlayHint {
    /// ヒントを挟み込む char インデックス。
    pub position: usize,
    /// 表示するテキスト（label が parts 配列なら連結済み）。
    pub text: String,
    pub padding_left: bool,
    pub padding_right: bool,
}

/// 構文ハイライトのグループ（ADR-0018: フラットな正規集合）。
///
/// wire 形式は小文字（serde `rename_all`）。tree-sitter のハイライトクエリの
/// capture 名と一致させる（minae-loader のクエリで使用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HighlightGroup {
    Comment,
    Keyword,
    String,
    Number,
    Constant,
    Function,
    Type,
    Parameter,
    Field,
    Operator,
    Punctuation,
    Attribute,
    Error,
}

/// テキストの1区間に割り当てられたハイライトグループ（char インデックス）。
///
/// 範囲は重複しない（同じ char は1つのグループに属する。仕様書の不変条件）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightRange {
    pub start: usize, // char index (inclusive)
    pub end: usize,   // char index (exclusive)
    pub group: HighlightGroup,
}

/// 進行中の非同期処理の種別（ADR-0028）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// LSP セッションの spawn + initialize。
    LspInit,
    /// 診断・inlay hint の settle（Open 後 or 編集後）。
    DiagnosticsSettle,
    /// 外部変更 Reload 後の LSP 同期 + pull。
    ReloadSync,
    /// 保存（write）。
    Save,
}

/// 進行中の非同期処理の単位（ADR-0028）。開始で追加・終了で除去され、
/// 結果の成否は語らない。空の集合 = 処理中なし。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub kind: ActivityKind,
    /// 表示用の短いラベル（"LSP 初期化中" など）。クライアントはそのまま表示する。
    pub label: String,
}

/// 操作の試行と結果の記録（ADR-0038。`StateSnapshot.activity` に載る）。
///
/// Headless 由来の「状態を変えようとした操作」（Open / 編集 / Save / Rename /
/// Close …）の成功・失敗を bounded リング（既定 100 件）で保持する。読み取り系は
/// 記録しない。ChangeEvent（状態変更の事後記録）とは別に並存 — こちらは
/// 「誰が・どの操作を・成功/失敗したか」の視点。更新で generation を進め push に乗る。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRecord {
    /// 操作を発行したクライアントの自己申告ラベル（[`Hello::name`]）。
    pub actor: String,
    /// 操作の種類（[`ChangeEvent`] と同じ語彙 — 状態を変えようとした意図）。
    pub kind: EventKind,
    /// 成功したか（失敗 = checksum / expected_text 不一致拒否・Open 失敗など）。
    pub ok: bool,
    /// 補足（対象パス・失敗理由など。表示用の短い文字列）。
    pub detail: String,
}

/// daemon が返す編集状態の全体像（ADR-0006: 毎コマンドに全量を返す）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub text: String,
    /// 全文の FNV-1a 64（[`DocumentEdit`] の checksum 検証用）。エージェントは
    /// これをそのまま edit に渡すだけでよい（FNV-1a の再実装不要。ADR-0012 #12）。
    pub checksum: u64,
    pub selection: Vec<Range>,
    pub primary_index: usize,
    pub mode: Mode,
    pub first_line: usize,
    pub diagnostics: Vec<Diagnostic>,
    /// フォーカス文書の inlay hint（ADR-0020。同じスナップショットのテキストと
    /// 一致する位置。LSP 非対応・未取得の文書は空）。
    pub inlay_hints: Vec<InlayHint>,
    /// フォーカス文書の可視範囲の構文ハイライト（ADR-0021。可視範囲は
    /// `first_line` から viewport_height 行。窓の上端を跨ぐトークンは範囲が
    /// 窓より前に始まることもある）。範囲は昇順・重複しない。grammar 不在の
    /// 言語は空。
    pub highlights: Vec<HighlightRange>,
    /// 開いているファイルのパス（未開なら None）。
    pub path: Option<String>,
    /// 保存済み状態から編集されているか。
    pub dirty: bool,
    /// 一時的なメッセージ（Open の失敗など）。ステータス行に表示される。
    pub status: Option<String>,
    /// 進行中の非同期処理の集合（ADR-0028。空 = 処理中なし）。増減は
    /// generation を進める（診断・ヒントの反映は進めない）。
    pub activities: Vec<Activity>,
    /// 操作の試行と結果の履歴（ADR-0038。bounded・既定 100 件）。更新で
    /// generation を進め push に乗る。Headless 由来の状態変更意図操作の
    /// 成功・失敗（読み取り系は対象外）。ChangeEvent とは別に並存。
    pub activity: Vec<ActivityRecord>,
    /// 状態を変える操作ごとに増加する世代（ADR-0012）。
    pub generation: u64,
    /// 直近の状態変化イベント（bounded リング。古いものから破棄）。
    pub events: Vec<ChangeEvent>,
    /// フォーカス文書が外部で削除され、Close を待っている状態（ADR-0015）。
    /// 値は削除されたパス。
    pub deleted: Option<String>,
    /// 定義の確認表示（[`Command::PeekDefinition`] の応答にのみ載る。それ以外は None）。
    pub peek: Option<Peek>,
    /// レビューコメントの件数（#50・v14）。全文は載せない — 一覧は
    /// [`Command::ListReviewComments`] で別取得する（push 肥大化の回避）。
    pub review_comment_count: usize,
    /// 注目文書の基準側対応物の診断（#52・a2・v15）。基準は不変のため
    /// ピン中は有効（再取得しない）。未取得・対象外は空。取得は daemon が
    /// 后台で行い、到着は push で届く（generation は進めない — ADR-0028）。
    pub base_diagnostics: Vec<BaseDiagnostic>,
}

/// 基準側ファイル1件分の診断（#52・a2・v15）。`path` は基準側の絶対パス。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseDiagnostic {
    /// 基準側ファイルの絶対パス（worktree 配下）。
    pub path: String,
    /// そのファイルの診断（live の [`StateSnapshot::diagnostics`] と同型）。
    pub diagnostics: Vec<Diagnostic>,
}

/// 定義の確認表示（[`Command::PeekDefinition`] の結果。ジャンプしない簡易確認用）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peek {
    /// 定義元ファイルのパス。
    pub path: String,
    /// 定義の開始行（1 始まり）。
    pub line: u32,
    /// 定義のスニペット（数行。改行区切り）。
    pub text: String,
}

impl Default for StateSnapshot {
    /// 空文書・位置0の単一カーソル・Normal モード。
    fn default() -> Self {
        Self {
            text: String::new(),
            checksum: fnv1a64(b""),
            selection: vec![Range { anchor: 0, head: 0 }],
            primary_index: 0,
            mode: Mode::Normal,
            first_line: 0,
            diagnostics: Vec::new(),
            inlay_hints: Vec::new(),
            highlights: Vec::new(),
            path: None,
            dirty: false,
            status: None,
            activities: Vec::new(),
            activity: Vec::new(),
            generation: 0,
            events: Vec::new(),
            deleted: None,
            peek: None,
            review_comment_count: 0,
            base_diagnostics: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_types_round_trip_with_lowercase_names() {
        // wire 形式は小文字（HighlightGroup の rename_all）
        let json = serde_json::to_string(&HighlightGroup::Function).unwrap();
        assert_eq!(json, "\"function\"");
        let back: HighlightGroup = serde_json::from_str(&json).unwrap();
        assert_eq!(back, HighlightGroup::Function);

        let range = HighlightRange {
            start: 4,
            end: 7,
            group: HighlightGroup::Keyword,
        };
        let json = serde_json::to_string(&range).unwrap();
        assert_eq!(json, "{\"start\":4,\"end\":7,\"group\":\"keyword\"}");
        let back: HighlightRange = serde_json::from_str(&json).unwrap();
        assert_eq!(back, range);
    }

    #[test]
    fn state_snapshot_round_trip() {
        let snapshot = StateSnapshot {
            text: "hello\nworld".to_string(),
            checksum: fnv1a64(b"hello\nworld"),
            selection: vec![Range { anchor: 2, head: 5 }],
            primary_index: 0,
            mode: Mode::Insert,
            first_line: 1,
            diagnostics: vec![Diagnostic {
                start: 0,
                end: 5,
                severity: Severity::Warning,
                message: "unused".to_string(),
            }],
            inlay_hints: vec![InlayHint {
                position: 3,
                text: ": i32".to_string(),
                padding_left: false,
                padding_right: true,
            }],
            highlights: vec![HighlightRange {
                start: 0,
                end: 5,
                group: HighlightGroup::Comment,
            }],
            path: Some("test.rs".to_string()),
            dirty: true,
            status: Some("ok".to_string()),
            generation: 7,
            events: vec![ChangeEvent {
                generation: 7,
                source: EventSource::Interactive,
                kind: EventKind::Insert,
                range: None,
                text: Some("x".to_string()),
            }],
            deleted: Some("test.rs".to_string()),
            review_comment_count: 2,
            base_diagnostics: vec![BaseDiagnostic {
                path: "/tmp/mina-base-1-abc/a.rs".to_string(),
                diagnostics: vec![Diagnostic {
                    start: 0,
                    end: 5,
                    severity: Severity::Error,
                    message: "base".to_string(),
                }],
            }],
            activities: vec![Activity {
                kind: ActivityKind::LspInit,
                label: "LSP 初期化中".to_string(),
            }],
            activity: vec![ActivityRecord {
                actor: "agent-1".to_string(),
                kind: EventKind::Open,
                ok: false,
                detail: "no such file".to_string(),
            }],
            peek: Some(Peek {
                path: "lib.rs".to_string(),
                line: 42,
                text: "fn frobnicate() {}".to_string(),
            }),
        };
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let back: StateSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, snapshot);
    }

    #[test]
    fn command_round_trip() {
        let json = serde_json::to_string(&Command::GetState).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, Command::GetState);

        // ADR-0025: 位置指定の定義確認も round-trip する
        let peek = Command::PeekDefinitionAt {
            path: "src/main.rs".into(),
            line: 12,
            col: 5,
        };
        let json = serde_json::to_string(&peek).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, peek);

        let cmd = Command::Move {
            movement: Movement::Line,
            direction: Direction::Backward,
        };
        let json = serde_json::to_string(&cmd).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cmd);

        let wait = Command::WaitFor { generation: 42 };
        let json = serde_json::to_string(&wait).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, wait);

        let del_word = Command::DeleteWordBackward;
        let json = serde_json::to_string(&del_word).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, del_word);

        let change = Command::Change;
        let json = serde_json::to_string(&change).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, change);

        for cmd in [Command::InsertAtLineEnd, Command::InsertAtLineStart] {
            let json = serde_json::to_string(&cmd).expect("serialize");
            let back: Command = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, cmd);
        }

        let end = Command::Move {
            movement: Movement::LineEnd,
            direction: Direction::Forward,
        };
        let json = serde_json::to_string(&end).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, end);

        let hints = Command::GetInlayHints {
            path: "src/main.rs".into(),
        };
        let json = serde_json::to_string(&hints).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, hints);
    }

    #[test]
    fn server_message_round_trip() {
        let snap = StateSnapshot::default();
        for msg in [
            ServerMessage::Response {
                snapshot: snap.clone(),
            },
            ServerMessage::Push { snapshot: snap },
        ] {
            let json = serde_json::to_string(&msg).expect("serialize");
            let back: ServerMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, msg);
            assert!(
                json.starts_with("{\"type\":\"") && json.contains("\"snapshot\":"),
                "タグ付きエンベロープ: {json}"
            );
        }

        let hints = ServerMessage::Hints {
            path: "src/main.rs".into(),
            generation: 7,
            hints: vec![InlayHint {
                position: 1,
                text: "i32".into(),
                padding_left: false,
                padding_right: false,
            }],
        };
        let json = serde_json::to_string(&hints).expect("serialize");
        let back: ServerMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, hints);
        assert!(json.contains("\"type\":\"hints\""), "タグ: {json}");
    }

    #[test]
    fn inlay_hint_round_trip() {
        let hint = InlayHint {
            position: 10,
            text: ": Vec<u8>".into(),
            padding_left: false,
            padding_right: true,
        };
        let json = serde_json::to_string(&hint).expect("serialize");
        let back: InlayHint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, hint);
    }

    #[test]
    fn server_info_round_trip() {
        // ServerInfo 応答（issue #27）の wire 形状が安定していること
        let msg = ServerMessage::ServerInfo {
            generation: "abc1234".into(),
            daemon_build_ts: 1699999999,
            metrics: ServerMetrics {
                edits_total: 10,
                edits_rejected_checksum: 1,
                edits_rejected_expected_text: 2,
                edits_noop: 3,
                edits_expected_text_used: 4,
                get_state_total: 5,
                wait_total: 6,
                save_total: 7,
                outline_total: 8,
                outline_bytes: 9,
                symbol_range_total: 10,
                symbol_range_bytes: 11,
                hover_total: 12,
                hover_bytes: 13,
                symbol_search_total: 14,
                symbol_search_bytes: 15,
                check_total: 16,
                check_bytes: 17,
                read_total: 18,
                read_bytes: 19,
            },
            base_roots: vec![BaseRootInfo {
                root: "/tmp/mina-base-abc".into(),
                commit: "abc1234".into(),
            }],
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ServerMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, msg);
        // GetServerInfo コマンドも外部タグ付きで通る
        let back: Command =
            serde_json::from_str(&serde_json::to_string(&Command::GetServerInfo).unwrap()).unwrap();
        assert_eq!(back, Command::GetServerInfo);
    }

    #[test]
    fn base_commands_round_trip() {
        // #49: Register/UnregisterBaseRoot の wire 形状が安定していること。
        for cmd in [
            Command::RegisterBaseRoot {
                root: "/tmp/mina-base-1-abc".into(),
                commit: "abc1234".into(),
                repo: Some("/repo".into()),
            },
            Command::UnregisterBaseRoot {
                root: "/tmp/mina-base-1-abc".into(),
            },
        ] {
            let back: Command =
                serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
            assert_eq!(back, cmd);
        }
        // v15: repo 省略の旧形状も読める（デフォルト None）。
        let back: Command =
            serde_json::from_str(r#"{"RegisterBaseRoot":{"root":"/r","commit":"c"}}"#).unwrap();
        assert_eq!(
            back,
            Command::RegisterBaseRoot {
                root: "/r".into(),
                commit: "c".into(),
                repo: None,
            }
        );
    }

    #[test]
    fn review_commands_round_trip() {
        // #50: レビューコメント系コマンドと応答の wire 形状が安定していること。
        for cmd in [
            Command::AddReviewComment {
                path: "/tmp/wt/src/main.rs".into(),
                side: ReviewSide::Base,
                line: 12,
                snippet: "let x = 1;".into(),
                body: "ここは消さないで".into(),
                base: "abc1234".into(),
            },
            Command::ListReviewComments,
            Command::ClearReviewComments,
        ] {
            let back: Command =
                serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
            assert_eq!(back, cmd);
        }
        assert_eq!(
            serde_json::to_string(&ReviewSide::Current).unwrap(),
            "\"current\""
        );
        let msg = ServerMessage::ReviewComments {
            generation: 9,
            comments: vec![ReviewCommentView {
                path: "/tmp/wt/src/main.rs".into(),
                side: ReviewSide::Current,
                line: 12,
                resolved_line: 14,
                stale: true,
                snippet: "let x = 1;".into(),
                body: "直して".into(),
                base: "abc1234".into(),
            }],
        };
        let back: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn outline_types_round_trip() {
        // SymbolKind の wire 形式は小文字
        assert_eq!(
            serde_json::to_string(&SymbolKind::Method).unwrap(),
            "\"method\""
        );
        // OutlineSymbol はツリーとして round-trip する
        let symbol = OutlineSymbol {
            name: "frobnicate".into(),
            kind: SymbolKind::Function,
            range: Range {
                anchor: 0,
                head: 30,
            },
            selection_range: Range {
                anchor: 3,
                head: 13,
            },
            children: vec![OutlineSymbol {
                name: "inner".into(),
                kind: SymbolKind::Variable,
                range: Range {
                    anchor: 10,
                    head: 20,
                },
                selection_range: Range {
                    anchor: 14,
                    head: 19,
                },
                children: Vec::new(),
            }],
        };
        let json = serde_json::to_string(&symbol).unwrap();
        let back: OutlineSymbol = serde_json::from_str(&json).unwrap();
        assert_eq!(back, symbol);
        assert!(json.contains("\"kind\":\"function\""));

        // Outline / EnclosingSymbol コマンドの round-trip
        for cmd in [
            Command::Outline {
                path: "src/lib.rs".into(),
            },
            Command::OutlineRecursive {
                path: "src/lib.rs".into(),
                depth: 3,
            },
            Command::ReadPath {
                path: "src/lib.rs".into(),
            },
            Command::EnclosingSymbol {
                path: "src/lib.rs".into(),
                line: 12,
                col: 5,
            },
        ] {
            let back: Command =
                serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
            assert_eq!(back, cmd);
        }

        // HoverAt / WorkspaceSymbol / CheckDiagnostics コマンドの round-trip
        // （ADR-0032）
        for cmd in [
            Command::HoverAt {
                path: "src/lib.rs".into(),
                line: 12,
                col: 5,
            },
            Command::WorkspaceSymbol {
                path: "src/lib.rs".into(),
                query: "frobnicate".into(),
            },
            Command::CheckDiagnostics {
                path: "src/lib.rs".into(),
            },
        ] {
            let back: Command =
                serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
            assert_eq!(back, cmd);
        }

        // Outline 応答を phrase として round-trip する
        let msg = ServerMessage::Outline {
            path: "src/lib.rs".into(),
            generation: 3,
            symbols: vec![symbol],
            error: None,
            truncated: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
        assert!(json.contains("\"type\":\"outline\""));
        // truncated はデフォルト false（後方互換）
        assert!(json.contains("\"truncated\":false"));
        let truncated: ServerMessage = serde_json::from_str(
            &serde_json::to_string(&ServerMessage::Outline {
                path: "src/lib.rs".into(),
                generation: 3,
                symbols: vec![],
                error: None,
                truncated: true,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            truncated,
            ServerMessage::Outline {
                path: "src/lib.rs".into(),
                generation: 3,
                symbols: vec![],
                error: None,
                truncated: true,
            }
        );

        // ReadPath 応答の round-trip（ADR-0048）
        let read_msg = ServerMessage::ReadPath {
            path: "src/lib.rs".into(),
            generation: 4,
            text: "fn main() {}\n".into(),
            error: None,
        };
        let read_back: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&read_msg).unwrap()).unwrap();
        assert_eq!(read_back, read_msg);

        let msg = ServerMessage::EnclosingSymbol {
            path: "src/lib.rs".into(),
            name: "frobnicate".into(),
            kind: SymbolKind::Function,
            range: Range {
                anchor: 0,
                head: 30,
            },
            selection_range: Range {
                anchor: 3,
                head: 13,
            },
            found: true,
            error: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
        assert!(json.contains("\"type\":\"enclosing_symbol\""));

        // Hover / WorkspaceSymbols / Check 応答の round-trip（ADR-0032）
        let hover = ServerMessage::Hover {
            path: "src/lib.rs".into(),
            generation: 3,
            text: "fn frobnicate() -> i32".into(),
            error: None,
        };
        let json = serde_json::to_string(&hover).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, hover);
        assert!(json.contains("\"type\":\"hover\""));

        let ws = ServerMessage::WorkspaceSymbols {
            generation: 3,
            symbols: vec![WorkspaceSymbol {
                name: "frobnicate".into(),
                kind: SymbolKind::Function,
                path: "/abs/src/lib.rs".into(),
                line: 12,
            }],
            error: None,
        };
        let json = serde_json::to_string(&ws).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ws);
        assert!(json.contains("\"type\":\"workspace_symbols\""));

        let check = ServerMessage::Check {
            path: "src/lib.rs".into(),
            generation: 3,
            total: 1,
            diagnostics: vec![CheckDiagnostic {
                severity: Severity::Error,
                line: 3,
                col: 7,
                start: 20,
                end: 24,
                message: "mock: TODO found".into(),
            }],
            settled: true,
            error: None,
        };
        let json = serde_json::to_string(&check).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, check);
        assert!(json.contains("\"type\":\"check\""));
        assert!(json.contains("\"settled\":true"));

        // ADR-0045: 予算切れの空（クリーン未確認）も settled: false で表現できる
        let unverified = ServerMessage::Check {
            path: "src/lib.rs".into(),
            generation: 3,
            total: 0,
            diagnostics: vec![],
            settled: false,
            error: None,
        };
        let back: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&unverified).unwrap()).unwrap();
        assert_eq!(back, unverified, "settled=false の空応答（クリーン未確認）");
    }

    #[test]
    fn document_edit_round_trip() {
        let edit = DocumentEdit {
            start: 2,
            end: 5,
            text: "x".into(),
            checksum: 42,
            expected_text: Some("hello".into()),
        };
        let json = serde_json::to_string(&edit).expect("serialize");
        let back: DocumentEdit = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, edit);
    }

    #[test]
    fn document_edit_expected_text_is_optional() {
        // None は null で通る
        let edit = DocumentEdit {
            start: 0,
            end: 0,
            text: "".into(),
            checksum: 0,
            expected_text: None,
        };
        let back: DocumentEdit =
            serde_json::from_str(&serde_json::to_string(&edit).unwrap()).unwrap();
        assert_eq!(back.expected_text, None);
        // フィールドなしの旧クライアント JSON も None として受信できる（非破壊）
        let legacy = r#"{"start":1,"end":2,"text":"x","checksum":9}"#;
        let back: DocumentEdit = serde_json::from_str(legacy).unwrap();
        assert_eq!(back.expected_text, None);
        assert_eq!(back.start, 1);
    }

    #[test]
    fn workspace_symbol_and_check_diagnostic_round_trip() {
        // 個別型の wire 形状（ADR-0032）
        let sym = WorkspaceSymbol {
            name: "run".into(),
            kind: SymbolKind::Function,
            path: "/abs/main.rs".into(),
            line: 4,
        };
        let json = serde_json::to_string(&sym).unwrap();
        assert!(json.contains("\"kind\":\"function\""));
        let back: WorkspaceSymbol = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sym);

        let diag = CheckDiagnostic {
            severity: Severity::Error,
            line: 2,
            col: 5,
            start: 10,
            end: 14,
            message: "mock: TODO found".into(),
        };
        let json = serde_json::to_string(&diag).unwrap();
        assert!(json.contains("\"severity\":\"Error\""));
        let back: CheckDiagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(back, diag);
    }

    #[test]
    fn fnv1a64_is_stable_and_byte_based() {
        // 実装がバージョン間で変わらないこと（クライアント/daemon 両側で一致が前提）
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"hi\n"), fnv1a64("hi\n".as_bytes()));
    }

    #[test]
    fn hello_name_defaults_to_unknown() {
        // ADR-0038: name は自己申告ラベル。旧クライアントの無指定 Hello は "unknown"
        let legacy = r#"{"kind":"headless","reset_cursor_on_disconnect":false}"#;
        let hello: Hello = serde_json::from_str(legacy).unwrap();
        assert_eq!(hello.name, "unknown");
        // 指定すれば通る
        let json = serde_json::to_string(&Hello {
            kind: ClientKind::Interactive,
            reset_cursor_on_disconnect: true,
            name: "tui".into(),
        })
        .unwrap();
        assert!(json.contains("\"name\":\"tui\""));
        let back: Hello = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "tui");
    }

    #[test]
    fn activity_record_round_trip() {
        // ADR-0038: 操作試行の成功/失敗の履歴（StateSnapshot.activity）
        let rec = ActivityRecord {
            actor: "agent-1".into(),
            kind: EventKind::Open,
            ok: false,
            detail: "no such file".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: ActivityRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);
        assert!(json.contains("\"ok\":false"));
        // EventKind は snake_case（rename_all 適用後の wire 形式）
        assert!(json.contains("\"kind\":\"open\""));
    }
}
