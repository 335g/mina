//! daemon: 編集状態を所有し、クライアントからのコマンドを処理する常駐プロセス。
//!
//! 状態は minae-view の [`Editor`] がすべて保持する（ADR-0005）。S2 では編集
//! （insert/delete/undo/redo）と保存を扱う。ファイル I/O（Open/Save）だけは
//! ロックを握らないよう接続ハンドラ側で async 実行する。socket は
//! `<temp_dir>/minae.sock`（単一ユーザ前提）。0600 で作成し、接続時に
//! peer uid を検証して別ユーザの接続を拒否する（MEDIUM-3）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mina_text::{
    Range as CoreRange, Selection, Transaction, append_selection, extend_line_below, extend_selection,
    extend_to, find_matches, find_next, find_prev, insert_at, line_end_of, line_start_of,
    move_selection, move_selection_lines, move_selection_to_line_first_non_whitespace,
    replace_targets, select_line_selection, word_at,
};
use mina_protocol::{
    Activity, ActivityKind, ActivityRecord, BaseDiagnostic, BaseRootInfo, ChangeEvent,
    CheckDiagnostic, ClientKind, Command, Diagnostic, DocumentEdit, EventKind, EventSource,
    GotoTarget, Hello, HighlightRange, InlayHint, OutlineSymbol, Range, ReviewComment,
    ReviewCommentView, ReviewSide, ServerMessage, ServerMetrics, Severity, StateSnapshot,
    SymbolKind, WorkspaceSymbol, fnv1a64,
};
use mina_view::{DocumentId, Editor, ViewId};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream, unix::OwnedWriteHalf};
use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, timeout};

use crate::languages::LanguageTable;
use crate::lsp;
use crate::lsp::LspSession;
use crate::trace::Trace;

/// Open で受け入れる最大ファイルサイズ（ADR-0008）。
///
/// v1 は毎コマンド全文スナップショット + LSP 全文同期のため、これを超える
/// ファイルは実用外。非正規ファイル（/dev/zero 等の無限ストリーム・FIFO・
/// ディレクトリ）の無制限読み込みによる OOM もこの検証で防ぐ（SEC-1）。
const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;

/// NDJSON 1コマンド行の最大バイト数。
///
/// 改行のない無限ストリームで daemon が OOM しないよう、超過する行は
/// 接続を閉じる（SEC-1）。
const MAX_CMD_LINE: usize = 1024 * 1024;

/// SetViewport で受け入れる高さの上限。
///
/// 端末の行数はこれを超えないが、壊れた/悪意あるコマンド（usize::MAX 等）
/// で `first_line + height` の overflow panic を起こさないよう clamp する
/// （scroll_to_cursor の加算は debug ビルドで panic する）。
const MAX_VIEWPORT_HEIGHT: usize = 10_000;

/// Scroll コマンドで受け入れるページ数の上限。
///
/// 壊れた/悪意あるコマンド（isize::MAX 等）で scroll_lines の
/// `first_line + amount` の加算が overflow panic を起こさないよう
/// clamp する（debug ビルドで panic する）。上限 × 最大 viewport 高
/// （[`MAX_VIEWPORT_HEIGHT`]）が isize 範囲を超えない値にすること
/// （1_000_000 × 10_000 = 10^10 行 ≪ isize::MAX）。
const MAX_SCROLL_PAGES: isize = 1_000_000;

/// 応答スナップショット書き込みのタイムアウト（MEDIUM-2）。
///
/// クライアントが応答を読まない（SIGSTOP された TUI・停止した agent 等）と
/// socket バッファが詰まり、write_all が永久ブロックして接続スロット
/// （[`MAX_CONNECTIONS`]）を消費し続ける。超過した接続は切断する。
///
/// ponytail: 固定値。16MiB の制御文字だらけのファイルで応答が ~96MiB に
/// なっても健全なリンクなら数秒で書ける。遅い読み手が問題になったら再考する。
#[cfg(not(test))]
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// 最初のコマンドを待つタイムアウト（MEDIUM-5）。
///
/// 接続後に何も送らない無言接続がスロット（[`MAX_CONNECTIONS`]）を永久に
/// 占有する DoS を防ぐ。最初のコマンド行がこの時間内に届かなければ切断して
/// スロットを解放する。最初のコマンドを送った後のアイドル（読書中の TUI 等）
/// にはタイムアウトを付けない — アイドルは正常で切断は有害（TUI は接続直後に
/// Open/GetState を送るので影響しない）。
///
/// ponytail: 固定値。30 秒あればどんなクライアントも最初のコマンドを送れる。
/// 放置するだけで全スロットが枯渇する脆弱性は塞げる（継続的に張り直す攻撃者
/// には効かない）。
#[cfg(not(test))]
const FIRST_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const FIRST_COMMAND_TIMEOUT: Duration = Duration::from_millis(500);

/// daemon が保持する編集状態。
pub struct Daemon {
    pub(crate) editor: Editor,
    /// クライアントから通知されるターミナル表示高さ（カーソル追従スクロール用）。
    pub(crate) viewport_height: usize,
    /// WorkspaceRoot 毎の LSP セッション（初回 .rs オープン時に生成。以後は
    /// 温かいまま保持・reap なし）。各セッションは独立した Mutex で保護し、
    /// LSP の await は daemon ロック外で行う（ADR-0009/ADR-0010）。
    pub(crate) lsp_sessions: HashMap<(PathBuf, String), Arc<Mutex<LspSession>>>,
    /// 登録中の基準 root → 登録情報（#49 比較閲覧 Mode 1・v13、v15 で repo 追加）。
    /// 配下は読取り専用（テキスト変更を拒否）・ライフサイクル管理対象
    /// （解除で LSP セッション破棄＋キャッシュ破棄）。キーは正規化済み絶対パス。
    pub(crate) base_roots: HashMap<PathBuf, BaseRootReg>,
    /// 基準側ファイルの診断キャッシュ（#52・a2）。キー = 基準側の絶対パス。
    /// 基準は不変のためピン中は有効（再取得しない）。Unregister で配下を捨てる。
    /// 挿入順は `base_diag_order` で FIFO evict（上限つき）。
    pub(crate) base_diagnostics: HashMap<PathBuf, Vec<mina_protocol::Diagnostic>>,
    pub(crate) base_diag_order: VecDeque<PathBuf>,
    /// 充填中の基準診断（重複 pull 抑止）。完了・解除で消える。
    pub(crate) base_diag_inflight: HashSet<PathBuf>,
    /// レビューコメント（#50・v14）。daemon 共有（TUI の View ではなく、別接続の
    /// `minas review` から見える必要があるため）。再ピン・Unregister・明示 Clear
    /// でのみ全消しし、TUI 切断では保持する（Q12）。追加順。
    pub(crate) review_comments: Vec<ReviewComment>,
    /// 言語テーブル（ADR-0030）。最新性は [`Daemon::languages_refresh`] が管理する
    /// （languages.toml の mtime が変わったときだけ再読込）。
    pub(crate) languages: Arc<LanguageTable>,
    /// 最後に読んだ languages.toml の mtime（`languages_refresh` の再読込判定）。
    languages_mtime: Option<std::time::SystemTime>,
    /// 解析フォーカス文書ごとの診断（ADR-0037）。キー = フォーカス文書のパス。
    /// スナップショットには「自分のセッションの解析フォーカス文書」
    /// （下記 `analysis_focus` と一致）を見る View の分だけ載せる。
    pub(crate) diagnostics: HashMap<PathBuf, Vec<mina_protocol::Diagnostic>>,
    /// セッションの解析フォーカス文書（セッションキー → 文書パス。ADR-0037）。
    ///
    /// LSP セッションが現在解析している文書（`current_uri` の daemon 側ミラー）—
    /// スナップショット合成（同期コンテキスト）から非ブロッキングで引くためのもの。
    /// 更新: Open / 解析 pull（[`Daemon::set_focus_diagnostics`]）/ 復元。
    analysis_focus: HashMap<(PathBuf, String), PathBuf>,
    /// パスごとの inlay hint キャッシュ（ADR-0020）。`HintCache.text_checksum` が
    /// 現在のテキストと一致すれば新鮮（再取得不要）。一致しなくても表示には使う
    /// （Q7: stale ヒントは新ヒント到着まで保持）。挿入順は `hint_order` で FIFO evict。
    pub(crate) hints: HashMap<PathBuf, HintCache>,
    pub(crate) hint_order: VecDeque<PathBuf>,
    /// パスごとの outline キャッシュ（ADR-0031）。`OutlineCache.text_checksum` が
    /// 現在のテキストと一致すれば新鮮（再取得不要）。これは inlay hint キャッシュ
    /// （ADR-0020）と同型 — 未保存編集は影響しない（チェックサムが変われば
    /// 不一致になり再取得される）。挿入順は `outline_order` で FIFO evict。
    pub(crate) outlines: HashMap<PathBuf, OutlineCache>,
    pub(crate) outline_order: VecDeque<PathBuf>,
    /// 開いている undo グループの所有者（= SetMode(Insert) で開いたクライアント）。
    ///
    /// HIGH-1: グループは接続スコープで所有される。所有者の切断のみがグループを
    /// 閉じモードを戻し、非所有者の書き込みは後勝ちで奪取する（preempt）。
    /// 不変条件: mode == Insert ⟺ insert_owner == Some(_)。
    pub(crate) insert_owner: Option<u64>,
    /// 最後の検索（`Search` / `SearchNext` 用）。`n`/`N` はここから1つずつ進める。
    /// 検索はカーソル位置の移動と選択だけを変え、文書・世代・push には関与しない。
    last_search: Option<SearchState>,
    /// 接続中の Interactive クライアントの conn_id（ADR-0027）。
    ///
    /// 最後の Interactive の切断判定に使う。リセットの要不要は切断した
    /// クライアントの Hello 宣言（`reset_cursor_on_disconnect`）で決まる。
    pub(crate) interactive_clients: HashSet<u64>,
    /// 接続 → View の対応（ADR-0037）。Interactive 接続のみが View を持ち、
    /// Headless は View を持たず共有 idle view を観測点にする。コマンド適用は
    /// このマップで発信接続の View をフォーカスして行う。
    pub(crate) conn_views: HashMap<u64, ClientView>,
    /// 共有 idle view（ADR-0037）。パス無し GetState（`session get` 相当）の
    /// 観測点で「最後に開かれた文書」。Headless の Open はここを動かし、
    /// 最後の Interactive 切断時のカーソルリセット対象もこれ。
    pub(crate) idle_view: ViewId,
    /// 操作の試行と結果の履歴（ADR-0038。bounded 100 件。更新で generation を
    /// 進め push に乗る）。Headless 由来の状態変更意図操作（Open / 編集 /
    /// Save / Rename / Close）の成功・失敗。読み取り系は対象外。
    pub(crate) activity_log: VecDeque<ActivityRecord>,
    /// 接続の自己申告ラベル（Hello.name。ADR-0038 — activity の actor に使う）。
    conn_names: HashMap<u64, String>,
    /// 状態を変える操作ごとに増加する世代（ADR-0012）。
    generation: u64,
    /// 直近の状態変化イベントの bounded リング（ADR-0012）。
    events: VecDeque<ChangeEvent>,
    /// パスごとの進行中の非同期処理（ADR-0028）。スナップショットにはフォーカス
    /// 文書の分だけが載る。増減で generation を進める（診断・ヒントの反映は進めない）。
    activities: HashMap<PathBuf, Vec<Activity>>,
    /// 外部変更検知のベースライン（全オープン文書。Open/Save/Close で更新）。
    baselines: HashMap<PathBuf, DiskBaseline>,
    /// フォーカス文書が外部で削除され、Close を待っているパス（ADR-0015）。
    deleted: Option<String>,
    /// 文書ごとの構文ハイライトキャッシュ（ADR-0016: Syntax は Daemon 所有）。
    ///
    /// スナップショット生成時にテキストの checksum が変わっていれば再計算
    /// （minae-loader のクエリ経由）。破棄された文書（Open の上限 evict）の
    /// エントリは参照時に掃除する。
    syntax: HashMap<mina_view::DocumentId, SyntaxCache>,
    /// 起動からの累積メトリクス（issue #27。GetServerInfo で開示し、headless
    /// エージェントの検証失敗率・全文再読回数などを効果検証する）。
    metrics: ServerMetrics,
}

/// 1文書分の Syntax キャッシュ（ADR-0021）: tree-sitter ツリー・クエリ・言語を
/// 保持し、編集ごとに差分ベースのインクリメンタルパースでツリーを更新する。
///
/// `text_checksum` が現在のテキストと一致する間はツリーを再利用し、可視範囲
/// のハイライトをキャッシュから返す（窓キー = text_checksum + first_line +
/// viewport_height。カーソル移動など窓が動かないコマンドでは再計算しない）。
/// 窓が動いた（スクロール）かテキストが変わったときだけ窓クエリを走らせる
/// （窓付きクエリは 1MB で ~0.5-2.5ms）。テキストが変わったら `old_text` との
/// 差分から InputEdit を求め、`tree.edit` + インクリメンタルパースで更新する
/// （全文再パースを回避。計測: 214KB で 38ms → 0.5ms）。
struct SyntaxCache {
    text_checksum: u64,
    /// 前回パース時の全文（InputEdit の差分算出用。差分が旧→新を完全に
    /// 記述するため、編集源（コマンド/undo/redo/外部リロード）は問わない）。
    old_text: String,
    tree: tree_sitter::Tree,
    language: tree_sitter::Language,
    query: tree_sitter::Query,
    /// 可視窓のハイライトキャッシュ（(text_checksum, first_line, viewport_height) キー）。
    /// 単一エントリ（スクロール位置ごとに持ち続けない）。
    window: Option<(u64, usize, usize, Vec<HighlightRange>)>,
}

/// 基準 root 1 件の登録情報（#49・v15 で repo 追加）。
/// `repo` は対応する live リポジトリルート（基準診断の対応付け用。
/// 無い root には基準診断が付かない）。
#[derive(Clone, Debug)]
pub(crate) struct BaseRootReg {
    pub(crate) commit: String,
    pub(crate) repo: Option<PathBuf>,
}

/// 1パス分の inlay hint キャッシュ（ADR-0020）。`text_checksum` は pull 時点の
/// テキストの FNV-1a 64（一致 = 「このテキストに対して取得済み」）。
pub(crate) struct HintCache {
    pub(crate) text_checksum: u64,
    pub(crate) hints: Vec<InlayHint>,
}

/// パスごとの outline キャッシュ 1 件（ADR-0031）。テキストのチェックサムが
/// 現在と一致する間だけ新鮮 — `at` / `outline` の 2 回目以降が LSP を呼ばずに済む
/// （実測: LSP 要求は毎回の再解析で数秒かかる）。
pub(crate) struct OutlineCache {
    pub(crate) text_checksum: u64,
    pub(crate) symbols: Vec<OutlineSymbol>,
}

/// 基準診断キャッシュの上限（#52）。hints と同値（64）。超過は挿入順の最古から除去する。
const MAX_BASE_DIAG_CACHE: usize = 64;

/// ヒントキャッシュの上限（ADR-0020）。超過は挿入順の最古から除去する。
const MAX_HINT_CACHE: usize = 64;

/// 外部変更検知のベースライン（mtime+size。ADR-0012/0015 のヒューリスティック）。
#[derive(Clone, Debug)]
struct DiskBaseline {
    mtime: std::time::SystemTime,
    size: u64,
}

/// 接続 1 つ分の View（ADR-0037）。doc / selection / first_line は
/// `editor` の View が保持し、ここはその ViewId と接続が通知した表示高さを持つ。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClientView {
    pub(crate) view_id: ViewId,
    pub(crate) viewport: usize,
}

/// イベントリングの上限（ADR-0012）。超過分は古いものから破棄。
const MAX_EVENTS: usize = 128;

/// 活動リングの上限（ADR-0038。既定 100 件）。超過分は古いものから破棄。
const MAX_ACTIVITY_LOG: usize = 100;

impl Daemon {
    /// フォーカス文書の可視範囲（first_line から viewport_height 行。ADR-0021）
    /// のハイライト範囲を返す。スナップショットにはこの範囲のみが載る。
    ///
    /// ツリー・クエリ・言語は文書ごとにキャッシュし、テキストが変わったとき
    /// だけ差分ベースのインクリメンタルパースでツリーを更新する（全文再パース
    /// とクエリ再コンパイルを回避 — 計測: 214KB で 38ms + 20ms → 0.5ms）。
    /// テキストと可視窓が変わらなければ（カーソル移動等）ハイライトはキャッシュ
    /// を返すだけ（窓クエリの再計算を回避）。
    fn syntax_highlights(
        &mut self,
        doc_id: DocumentId,
        text: &str,
        checksum: u64,
        first_line: usize,
        viewport_height: usize,
    ) -> Vec<HighlightRange> {
        // 破棄された文書（Open の上限 evict）のキャッシュを落とす
        let live: Vec<_> = self.editor.document_ids().collect();
        self.syntax.retain(|id, _| live.contains(id));
        // grammar 不在の言語（スクラッチ・非対応拡張子）は空。キャッシュも
        // 落とす（文書のパスと言語は不変なので stale は理論上ないが防御）。
        let Some(language_def) = self
            .editor
            .path_for_doc(doc_id)
            .and_then(|p| self.languages.grammar_for_path(p))
        else {
            self.syntax.remove(&doc_id);
            return Vec::new();
        };
        let window = visible_window_range(text, first_line, viewport_height);

        // 初回（文書ごとに1回）: フルパース + クエリコンパイル
        // （Query::new は ~20ms — 打鍵ごとに走らせない）。
        if let std::collections::hash_map::Entry::Vacant(e) = self.syntax.entry(doc_id) {
            let mut parser = tree_sitter::Parser::new();
            if parser.set_language(&(language_def.grammar)()).is_err() {
                return Vec::new();
            }
            let Some(tree) = parser.parse(text, None) else {
                return Vec::new();
            };
            let language = (language_def.grammar)();
            let Ok(query) = tree_sitter::Query::new(&language, language_def.highlights) else {
                return Vec::new();
            };
            e.insert(SyntaxCache {
                text_checksum: checksum,
                old_text: text.to_string(),
                tree,
                language,
                query,
                window: None,
            });
        }

        let cached = self.syntax.get_mut(&doc_id).expect("上で確保した");
        if cached.text_checksum != checksum {
            // テキスト変化: 旧文との差分から InputEdit を求め、tree.edit +
            // インクリメンタルパース。差分は挿入・削除・置換・undo/redo・
            // 外部リロードの全経路を同一コードで扱える。
            let old_text = std::mem::replace(&mut cached.old_text, text.to_string());
            let edit = input_edit_from_diff(&old_text, text);
            cached.tree.edit(&edit);
            let mut parser = tree_sitter::Parser::new();
            let reparsed = if parser.set_language(&cached.language).is_ok() {
                parser
                    .parse(text, Some(&cached.tree))
                    .or_else(|| parser.parse(text, None))
            } else {
                None
            };
            match reparsed {
                Some(tree) => cached.tree = tree,
                // パース失敗（メモリ枯渇等）: stale ツリーで不正な範囲を
                // 出さないためキャッシュを落として空を返す。
                None => {
                    self.syntax.remove(&doc_id);
                    return Vec::new();
                }
            }
            cached.text_checksum = checksum;
        }
        // 可視窓キャッシュ: (checksum, first_line, viewport_height) が一致する
        // 間は再計算しない。スクロール・リサイズ・編集で窓かテキストが変わった
        // ときだけ窓クエリを走らせる（カーソル移動の打鍵コストを O(1) に）。
        let window_key = (checksum, first_line, viewport_height);
        if let Some((c, fl, h, ranges)) = &cached.window {
            if (*c, *fl, *h) == window_key {
                return ranges.clone();
            }
        }
        // 窓先頭 byte の char インデックスを渡す（loader は窓範囲限定の
        // byte→char 変換を使うため、文書内の絶対位置はここで解決する）。
        let window_char_start = text[..window.start].chars().count();
        let ranges = mina_loader::highlight_ranges_in_window(
            &cached.query,
            text,
            &cached.tree,
            window,
            window_char_start,
        );
        cached.window = Some((window_key.0, window_key.1, window_key.2, ranges.clone()));
        ranges
    }

    /// パスの inlay hint をキャッシュに書き込む（ADR-0020）。
    ///
    /// checksum は pull 時点のテキストから計算する（このテキストに対して取得
    /// 済みという意味）。上限超過は挿入順の最古から除去する（FIFO）。
    pub(crate) fn cache_hints(&mut self, path: PathBuf, text: &str, hints: Vec<InlayHint>) {
        let checksum = fnv1a64(text.as_bytes());
        if self
            .hints
            .insert(path.clone(), HintCache { text_checksum: checksum, hints })
            .is_none()
        {
            self.hint_order.push_back(path.clone());
        }
        while self.hints.len() > MAX_HINT_CACHE {
            if let Some(old) = self.hint_order.pop_front() {
                self.hints.remove(&old);
            }
        }
    }

    /// パスの outline をキャッシュに書き込む（ADR-0031）。チェックサムは取得
    /// 時点のテキストから計算する（このテキストに対して取得済みという意味）。
    /// 上限超過は挿入順の最古から除去する（HintCache と同じ FIFO）。
    pub(crate) fn cache_outline(&mut self, path: PathBuf, text: &str, symbols: Vec<OutlineSymbol>) {
        let checksum = fnv1a64(text.as_bytes());
        if self
            .outlines
            .insert(path.clone(), OutlineCache { text_checksum: checksum, symbols })
            .is_none()
        {
            self.outline_order.push_back(path.clone());
        }
        while self.outlines.len() > MAX_HINT_CACHE {
            if let Some(old) = self.outline_order.pop_front() {
                self.outlines.remove(&old);
            }
        }
    }

    /// 進行中の処理を追加する（ADR-0028）。追加で generation を進める
    /// （診断・ヒントの反映は進めない — 増減だけが待ち合わせの対象）。
    /// 同一 kind の重複追加は無視（idempotent）。
    pub(crate) fn add_activity(&mut self, path: &Path, kind: ActivityKind, label: &str) {
        let activities = self.activities.entry(path.to_path_buf()).or_default();
        if !activities.iter().any(|a| a.kind == kind) {
            activities.push(Activity {
                kind,
                label: label.to_string(),
            });
            self.generation += 1;
        }
    }

    /// 進行中の処理を除去する（ADR-0028）。除去で generation を進める。
    pub(crate) fn remove_activity(&mut self, path: &Path, kind: ActivityKind) {
        let Some(activities) = self.activities.get_mut(path) else {
            return;
        };
        let before = activities.len();
        activities.retain(|a| a.kind != kind);
        if activities.len() != before {
            self.generation += 1;
            if activities.is_empty() {
                self.activities.remove(path);
            }
        }
    }

    /// 接続の自己申告ラベル（ADR-0038。未登録 = テスト等は "unknown"）。
    fn actor_for(&self, conn_id: u64) -> String {
        self.conn_names
            .get(&conn_id)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Headless 由来の状態変更意図操作の試行と結果を記録する（ADR-0038）。
    /// 世代を進め push に乗せる（events と同じ配線）。読み取り系は記録しない。
    fn record_activity(&mut self, actor: String, kind: EventKind, ok: bool, detail: String) {
        self.generation += 1;
        self.activity_log.push_back(ActivityRecord {
            actor,
            kind,
            ok,
            detail,
        });
        while self.activity_log.len() > MAX_ACTIVITY_LOG {
            self.activity_log.pop_front();
        }
    }

    /// 状態を変える操作を記録する（世代を増やし、イベントをリングに積む）。
    fn record_event(
        &mut self,
        source: EventSource,
        kind: EventKind,
        range: Option<Range>,
        text: Option<String>,
    ) {
        self.generation += 1;
        self.events.push_back(ChangeEvent {
            generation: self.generation,
            source,
            kind,
            range,
            text,
        });
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            editor: Editor::new(),
            viewport_height: 24,
            lsp_sessions: HashMap::new(),
            base_roots: HashMap::new(),
            base_diagnostics: HashMap::new(),
            base_diag_order: VecDeque::new(),
            base_diag_inflight: HashSet::new(),
            review_comments: Vec::new(),
            // 起動時の初期ロード。以後は languages_refresh が mtime 差分だけ再読込（ADR-0030）。
            languages: LanguageTable::load().into_arc(),
            languages_mtime: languages_file_mtime(),
            diagnostics: HashMap::new(),
            analysis_focus: HashMap::new(),
            insert_owner: None,
            last_search: None,
            interactive_clients: HashSet::new(),
            conn_views: HashMap::new(),
            idle_view: ViewId(0),
            activity_log: VecDeque::new(),
            conn_names: HashMap::new(),
            generation: 0,
            events: VecDeque::new(),
            baselines: HashMap::new(),
            deleted: None,
            syntax: HashMap::new(),
            hints: HashMap::new(),
            hint_order: VecDeque::new(),
            outlines: HashMap::new(),
            outline_order: VecDeque::new(),
            activities: HashMap::new(),
            metrics: ServerMetrics::default(),
        }
    }

    /// 最新の言語テーブルを返す。languages.toml の mtime が前回読込と異なれば
    /// 再読込してキャッシュを差し替える（なければキャッシュを返すだけ）。
    ///
    /// ゲート（拡張子 → サーバ有無）と spawn（`ensure`）の**両方**がこれを使う。
    /// spawn 時だけの再読込では「新言語の追加」がゲートを通過できず、次回
    /// daemon 再起動まで反映されないため（敵対的検証で発見 — ADR-0030）。
    pub(crate) fn languages_refresh(&mut self) -> Arc<LanguageTable> {
        let mtime = languages_file_mtime();
        if mtime != self.languages_mtime {
            self.languages = LanguageTable::load().into_arc();
            self.languages_mtime = mtime;
        }
        self.languages.clone()
    }

    /// パスに対応する LSP セッション（`session_root_for` のキーで引く）。
    fn session_for(&self, path: &Path) -> Option<Arc<Mutex<LspSession>>> {
        self.session_root_for(path)
            .and_then(|key| self.lsp_sessions.get(&key).cloned())
    }

    /// パスのセッションキー（WorkspaceRoot + languageId。ADR-0030 Stage 4）。
    /// LSP サーバを持つ言語のパスのみ `Some`（markdown のようなルートマーカー専用言語は対象外）。
    fn session_key(&self, path: &Path) -> Option<(PathBuf, String)> {
        let lang = self.languages.language_for_path(path)?;
        lang.language_server.as_ref()?;
        Some((self.languages.workspace_root(path), lang.name.clone()))
    }

    /// フォーカス文書と同じ (WorkspaceRoot, languageId) で LSP 対応のときだけセッションを
    /// 借りたことになる（ADR-0010 / ADR-0030 Stage 4）。別 root・別言語ならフォーカス文書
    /// のセッションには触れていないので復元不要（借りたセッションの current_uri は次回
    /// 要求の didOpen で置き換わる）。
    fn borrows_focus_session(&mut self, focused: &Option<PathBuf>, target: &Path) -> bool {
        let languages = self.languages_refresh();
        focused.as_deref().is_some_and(|fp| {
            fp != target
                && languages.server_for(fp).is_some()
                && self.session_key(fp) == self.session_key(target)
        })
    }

    /// パスに対応する LSP セッションのキーを引く。テーブルで計算した root に
    /// セッションがあればそれを返す。なければ「パスを包含する最長の既存キー」に
    /// フォールバックする — languages.toml の root-markers を稼働中に編集すると
    /// 稼働中セッションのキーが変わり、同期スキップ・診断消失・重複 spawn が起きる
    /// ため（敵対的検証で発見: P1）。
    ///
    /// `ensure` の spawn 判定では使わない: ネストしたワークスペース（/a と /a/c の
    /// 両セッションが正当に共存）で祖先セッションを誤って再利用しない（ADR-0010）。
    fn session_root_for(&self, path: &Path) -> Option<(PathBuf, String)> {
        let fresh = self.session_key(path)?;
        if self.lsp_sessions.contains_key(&fresh) {
            return Some(fresh);
        }
        // 言語もキーの一部: フォールバックは同言語のキーのみ（.rs のセッションに .ts を乗せない）
        self.lsp_sessions
            .keys()
            .filter(|(root, lang)| *lang == fresh.1 && path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .cloned()
    }

    /// パスが登録中の基準 root 配下か（#49）。空登録時は即 false（無コスト）。
    /// 比較はコンポーネント単位（Path::starts_with）。呼び出し側で正規化済み
    /// パスを渡すこと（登録キーは正規化済み）。
    pub(crate) fn is_base_path(&self, path: &Path) -> bool {
        if self.base_roots.is_empty() {
            return false;
        }
        self.base_roots.keys().any(|r| path.starts_with(r))
    }

    /// 基準 root の登録（#49）。冪等（再登録は commit を更新）。
    /// root・repo は正規化済みであること。世代を進めイベントに積む。
    pub(crate) fn register_base_root(
        &mut self,
        root: PathBuf,
        commit: String,
        repo: Option<PathBuf>,
        source: EventSource,
    ) {
        // #50: 基準点が変わったときだけ旧コメントを破棄する（Q9）。同一ペアの
        // 再登録（表示切替・再接続）は保持する。
        let same = self.base_roots.get(&root).is_some_and(|r| {
            r.commit == commit && r.repo == repo
        });
        if !same {
            self.review_comments.clear();
        }
        self.base_roots.insert(root, BaseRootReg { commit, repo });
        self.record_event(source, EventKind::BaseRoot, None, None);
    }

    /// 基準 root の登録解除（#49）。存在しなければ何もしない（M1: 変化なし）。
    /// 破棄すべき LSP セッションを返す（呼び出し側で daemon ロック外で kill）。
    /// 配下パスのキャッシュ（outline/hints/diagnostics/解析フォーカス）を捨てる。
    /// activities は bounded・自己消去のため残す。
    pub(crate) fn unregister_base_root(
        &mut self,
        root: &Path,
        source: EventSource,
    ) -> Vec<Arc<Mutex<LspSession>>> {
        if self.base_roots.remove(root).is_none() {
            return Vec::new();
        }
        // #50: Unregister で基準点が消えるためコメントも全消しする（Q12）。
        self.review_comments.clear();
        let dead_keys: Vec<_> = self
            .lsp_sessions
            .keys()
            .filter(|(r, _)| r.starts_with(root))
            .cloned()
            .collect();
        let mut sessions = Vec::new();
        for k in dead_keys {
            if let Some(s) = self.lsp_sessions.remove(&k) {
                sessions.push(s);
            }
        }
        self.outlines.retain(|p, _| !p.starts_with(root));
        self.outline_order.retain(|p| !p.starts_with(root));
        self.hints.retain(|p, _| !p.starts_with(root));
        self.hint_order.retain(|p| !p.starts_with(root));
        self.diagnostics.retain(|p, _| !p.starts_with(root));
        // #52: 基準診断のキャッシュ・充填中も配下で捨てる。
        self.base_diagnostics.retain(|p, _| !p.starts_with(root));
        self.base_diag_order.retain(|p| !p.starts_with(root));
        self.base_diag_inflight.retain(|p| !p.starts_with(root));
        self.analysis_focus.retain(|(r, _), _| !r.starts_with(root));
        self.record_event(source, EventKind::BaseRoot, None, None);
        sessions
    }

    /// 基準配下への書込み拒否メッセージ（#49）。対象外なら None。
    /// status 報告用（DocumentEdit の checksum 拒否と同型）。
    pub(crate) fn base_reject(&self, path: &Path) -> Option<String> {
        if !self.is_base_path(path) {
            return None;
        }
        let short = self
            .base_roots
            .iter()
            .find(|(r, _)| path.starts_with(r))
            .map(|(_, c)| c.commit.get(..7).unwrap_or(&c.commit))
            .unwrap_or("?");
        Some(format!(
            "基準 {short} 配下は読取り専用です: {}",
            path.display()
        ))
    }

    /// レビューコメントの追加/更新/削除（#50）。同一アンカー（パス・側・行）は
    /// 上書きし、空本文はそのアンカーの削除（専用 Delete コマンドなし）。
    /// いずれも世代を進めイベントに積む（TUI の件数追従・push 配信用）。
    pub(crate) fn add_review_comment(&mut self, c: ReviewComment, source: EventSource) {
        if c.body.trim().is_empty() {
            self.review_comments
                .retain(|e| !(e.path == c.path && e.side == c.side && e.line == c.line));
        } else if let Some(e) = self
            .review_comments
            .iter_mut()
            .find(|e| e.path == c.path && e.side == c.side && e.line == c.line)
        {
            *e = c;
        } else {
            self.review_comments.push(c);
        }
        self.record_event(source, EventKind::ReviewComment, None, None);
    }

    /// レビューコメントの全消し（#50）。空のときは何もしない（M1: 変化なし）。
    pub(crate) fn clear_review_comments(&mut self, source: EventSource) {
        if self.review_comments.is_empty() {
            return;
        }
        self.review_comments.clear();
        self.record_event(source, EventKind::ReviewComment, None, None);
    }

    /// レビューコメント一覧の照合付き解決（#50・読み取り専用）。保存値は
    /// 書き換えず、`stale`＋`resolved_line` を添える（判断は AI 側）。
    /// 現在側は現テキスト（開いていれば editor、未開はディスク）と突き合わせ、
    /// 基準側は不変なので存在確認だけする。
    pub(crate) fn resolve_review_comments(&self) -> Vec<ReviewCommentView> {
        self.review_comments
            .iter()
            .map(|c| {
                let (resolved_line, stale) = match c.side {
                    ReviewSide::Base => (c.line, !Path::new(&c.path).exists()),
                    ReviewSide::Current => self.resolve_current_line(&c.path, c.line, &c.snippet),
                };
                ReviewCommentView {
                    path: c.path.clone(),
                    side: c.side,
                    line: c.line,
                    resolved_line,
                    stale,
                    snippet: c.snippet.clone(),
                    body: c.body.clone(),
                    base: c.base.clone(),
                }
            })
            .collect()
    }

    /// 現在側コメントの行解決（#50）。`line` 行（1-origin）の内容が `snippet` と
    /// 一致すれば ok、ずれていれば全文から `snippet` の初出を探す。見つかれば
    /// その行を stale 付きで返し、なければ追加時の行を stale 付きで返す。
    fn resolve_current_line(&self, path: &str, line: u32, snippet: &str) -> (u32, bool) {
        let text: Option<String> = self
            .editor
            .doc_id_for_path(Path::new(path))
            .map(|id| self.editor.document(id).text().to_string())
            .or_else(|| std::fs::read_to_string(path).ok());
        let Some(text) = text else {
            return (line, true);
        };
        let lines: Vec<&str> = text.lines().collect();
        let idx = line.saturating_sub(1) as usize;
        if lines.get(idx).is_some_and(|l| *l == snippet) {
            return (line, false);
        }
        match lines.iter().position(|l| *l == snippet) {
            Some(found) => (found as u32 + 1, true),
            None => (line, true),
        }
    }

    /// クライアント切断時の後始末: Insert モードで開いたままの undo グループを
    /// 閉じ、モードを Normal に戻す（ADR-0007）。閉じ忘れると、常駐 daemon の
    /// 履歴がセッションを跨いで編集を同一 undo グループに統合してしまう
    /// （H1: Insert のまま終了した次セッションの入力が undo 1回で消える）。
    ///
    /// HIGH-1: 後始末はグループを開いたクライアント（所有者）の切断に限定する。
    /// 非所有者（ワンショットの agent コマンド等）の切断は編集状態を触らない —
    /// 修正前は読み取り専用の agent コマンドが終わるたびに人間の Insert
    /// セッションが閉じられていた。
    fn on_client_disconnect(&mut self, conn_id: u64, kind: ClientKind, reset_cursor: bool) {
        if kind == ClientKind::Interactive {
            self.interactive_clients.remove(&conn_id);
        }
        if self.insert_owner == Some(conn_id) && self.editor.mode() == mina_view::Mode::Insert {
            close_insert_session(self, mina_view::Mode::Normal);
        }
        // ADR-0027 を v12 で再解釈: 接続の View は切断とともに消えるため、
        // リセット対象は共有 idle view（= パス無し GetState の観測点）のみ。
        // 状態変化なので世代とイベントを進める（WaitFor 待ちエージェントが起床）。
        // 文書・undo 履歴・LSP セッションは不変（Q1: 保持するのは Selection のみ）。
        if kind == ClientKind::Interactive && reset_cursor && self.interactive_clients.is_empty() {
            self.editor.reset_view_to_start(self.idle_view);
            self.record_event(EventSource::External, EventKind::SelectionReset, None, None);
        }
    }

    /// コマンド適用前に、発信接続の View をフォーカスへ合わせる（ADR-0037）。
    /// Interactive は自接続の View（未登録 = テスト等のフォールバックは idle view）、
    /// Headless は共有 idle view。合わせて viewport 高さを接続の値へ切り替える。
    fn focus_for(&mut self, conn_id: u64) {
        let record = self.conn_views.get(&conn_id);
        let view_id = record.map_or(self.idle_view, |r| r.view_id);
        let viewport = record.map_or(self.viewport_height, |r| r.viewport);
        self.editor.set_focused_view(view_id);
        self.viewport_height = viewport;
    }

    /// Open で開かれた文書を共有 idle view にも追従させる（ADR-0037:
    /// 「最後に開かれた文書」= パス無し GetState の観測点）。idle view の選択・
    /// 先頭行は保持し、フォーカスは元の接続 View へ戻す（応答合成は自分の View
    /// 基準のまま）。ヘッドレス（既に idle view がフォーカス中）は追従だけ。
    fn open_idle_follow(&mut self, path: &Path) {
        let conn_view = self.editor.focused_view_id();
        if conn_view == self.idle_view {
            self.editor.focus_open_path(path);
            return;
        }
        self.editor.set_focused_view(self.idle_view);
        self.editor.focus_open_path(path);
        self.editor.set_focused_view(conn_view);
    }

    /// `path` が自分のセッションの「解析フォーカス文書」か（= その文書が最後に
    /// 解析された文書 — ADR-0037）。診断・inlay はこれを見る View のスナップ
    /// ショットにのみ載せる。
    pub(crate) fn is_analysis_focus(&self, path: &Path) -> bool {
        self.session_key(path)
            .and_then(|key| self.analysis_focus.get(&key))
            .is_some_and(|f| f.as_path() == path)
    }

    /// セッションの解析フォーカスが `path` へ動いたときに更新する（Open・復元・
    /// peek）。同セッションの旧フォーカス文書の診断は捨てる（解析されない文書の
    /// 診断を保持しない — フォーカスはセッションごとに 1 文書）。
    fn set_analysis_focus(&mut self, path: &Path) {
        if let Some(key) = self.session_key(path) {
            if let Some(prev) = self.analysis_focus.insert(key, path.to_path_buf()) {
                if prev != path {
                    self.diagnostics.remove(&prev);
                }
            }
        }
    }

    /// 解析フォーカス文書の診断を保存する（pull が current_uri 一致で返した箇所）。
    /// フォーカスも同時に更新する（診断はフォーカス文書のものだけを保持する）。
    fn set_focus_diagnostics(&mut self, path: &Path, diags: Vec<mina_protocol::Diagnostic>) {
        self.set_analysis_focus(path);
        self.diagnostics.insert(path.to_path_buf(), diags);
    }

    /// スナップショットに載せる診断: 解析フォーカス文書を見る View の分だけ。
    pub(crate) fn focus_diagnostics(&self, path: &Path) -> Vec<mina_protocol::Diagnostic> {
        if !self.is_analysis_focus(path) {
            return Vec::new();
        }
        self.diagnostics
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    /// スナップショットに載せる inlay hint: 解析フォーカス文書を見る View の分だけ
    /// （キャッシュ自体は `GetInlayHints` の全パス応答とも共用するため、表示は
    /// ここで解析フォーカスに絞る — ADR-0037）。
    pub(crate) fn focus_inlay_hints(&self, path: &Path) -> Vec<mina_protocol::InlayHint> {
        if !self.is_analysis_focus(path) {
            return Vec::new();
        }
        self.hints
            .get(path)
            .map(|c| c.hints.clone())
            .unwrap_or_default()
    }

    /// 注目文書の基準側対応物（#52・a2）。登録 root のうち repo 対応が
    /// 分かるものについて、live 側パス → 基準側パスへ写す。注目文書自体が
    /// 基準配下なら自分自身（開いて読んでいる基準の診断）。重複除去済み。
    pub(crate) fn base_counterparts(&self, focused: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (root, reg) in &self.base_roots {
            let counterpart = if focused.starts_with(root) {
                focused.to_path_buf()
            } else if let Some(repo) = reg.repo.as_ref() {
                match focused.strip_prefix(repo) {
                    Ok(rel) => root.join(rel),
                    Err(_) => continue,
                }
            } else {
                continue;
            };
            if !out.contains(&counterpart) {
                out.push(counterpart);
            }
        }
        out
    }

    /// スナップショットに載せる基準診断（#52・a2）。キャッシュ済みの
    /// 対応物だけを載せる（未取得は后台充填に任せ、ここでは待たない）。
    pub(crate) fn snapshot_base_diagnostics(&self, focused: Option<&Path>) -> Vec<BaseDiagnostic> {
        let Some(focused) = focused else {
            return Vec::new();
        };
        self.base_counterparts(focused)
            .into_iter()
            .filter_map(|p| {
                self.base_diagnostics.get(&p).map(|diags| BaseDiagnostic {
                    path: p.to_string_lossy().into_owned(),
                    diagnostics: diags.clone(),
                })
            })
            .collect()
    }

    /// 未キャッシュの対応物を1件だけ引き当て、充填中印を付ける（#52・a2）。
    /// 呼び出し側（非同期）が后台 pull を起動する。無ければ None。
    pub(crate) fn take_base_pull(&mut self, focused: Option<&Path>) -> Option<PathBuf> {
        let Some(focused) = focused else {
            return None;
        };
        self.base_counterparts(focused).into_iter().find(|p| {
            !self.base_diagnostics.contains_key(p) && !self.base_diag_inflight.contains(p)
        }).map(|p| {
            self.base_diag_inflight.insert(p.clone());
            p
        })
    }

    /// 基準診断を格納する（#52・a2）。上限超過は挿入順の最古から除去する。
    /// 充填中印は外す。呼び出し側で世代は進めない（ADR-0028）。
    pub(crate) fn store_base_diagnostics(&mut self, path: PathBuf, diags: Vec<Diagnostic>) {
        self.base_diag_inflight.remove(&path);
        if !self.base_diagnostics.contains_key(&path) {
            self.base_diag_order.push_back(path.clone());
        }
        self.base_diagnostics.insert(path, diags);
        while self.base_diagnostics.len() > MAX_BASE_DIAG_CACHE {
            if let Some(old) = self.base_diag_order.pop_front() {
                self.base_diagnostics.remove(&old);
            } else {
                break;
            }
        }
    }

    /// Interactive 接続に View を割り当てる（ADR-0037）。開始文書は idle view
    /// の文書（最後に開かれた共有文書）。選択・先頭行は先頭から。
    fn register_conn_view(&mut self, conn_id: u64) {
        let doc = self.editor.view_by_id(self.idle_view).doc;
        let id = self.editor.add_view(mina_view::View {
            doc,
            selection: mina_text::Selection::point(0),
            first_line: 0,
        });
        self.conn_views
            .insert(conn_id, ClientView { view_id: id, viewport: 24 });
    }

    /// 切断時に接続の View を破棄する（ADR-0037）。破棄した View に
    /// フォーカスが残っていたら idle へ戻す — 戻さないと watch_disk 等の
    /// focused_path() が死んだ slot を掴んで panic する（#49 dogfood で発覚）。
    fn drop_conn_view(&mut self, conn_id: u64) {
        if let Some(r) = self.conn_views.remove(&conn_id) {
            let was_focused = self.editor.focused_view_id() == r.view_id;
            self.editor.remove_view(r.view_id);
            if was_focused {
                self.editor.set_focused_view(self.idle_view);
            }
        }
    }
}

/// languages.toml の現在の mtime（ファイルがなければ `None`）。
fn languages_file_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(crate::languages::config_dir().join("languages.toml"))
        .ok()
        .and_then(|m| m.modified().ok())
}

/// 可視行 [first_line, first_line+height) の byte 範囲（ADR-0021）。
///
/// 行 k の先頭 byte = k 番目の '\n' の直後（行0 = 0）。`first_line` が文書の
/// 行数を超える（空文書・末尾スクロール超過）場合は空範囲になる。
fn visible_window_range(text: &str, first_line: usize, height: usize) -> std::ops::Range<usize> {
    let mut newlines = text.match_indices('\n').map(|(i, _)| i + 1);
    let start = if first_line == 0 {
        0
    } else {
        newlines.nth(first_line - 1).unwrap_or(text.len())
    };
    let end = newlines
        .nth(height.saturating_sub(1))
        .unwrap_or(text.len());
    start..end
}

/// 旧テキストと新テキストの共通接頭辞・接尾辞から tree-sitter の `InputEdit`
/// を求める。旧→新の差分はこれ1つで完全に記述できるため、挿入・削除・置換・
/// undo/redo・外部リロードの全編集源を同一コードで扱える（複数カーソルなど
/// 非連続編集も「最初から最後の差分までの範囲」に潰して正しく記述される）。
///
/// 列は行内 byte オフセット（tree-sitter の Point 規約）。prefix/suffix は
/// UTF-8 バイト列の共通部なので char 境界で切れ、スライスは安全。
fn input_edit_from_diff(old: &str, new: &str) -> tree_sitter::InputEdit {
    let (old_b, new_b) = (old.as_bytes(), new.as_bytes());
    let prefix = old_b
        .iter()
        .zip(new_b.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old_b
        .iter()
        .rev()
        .zip(new_b.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(old_b.len() - prefix)
        .min(new_b.len() - prefix);
    let old_end = old_b.len() - suffix;
    let new_end = new_b.len() - suffix;
    tree_sitter::InputEdit {
        start_byte: prefix,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: byte_point(old, prefix),
        old_end_position: byte_point(old, old_end),
        new_end_position: byte_point(new, new_end),
    }
}

/// byte 位置の (row, col)。col は行内 byte オフセット（tree-sitter の規約）。
fn byte_point(text: &str, byte: usize) -> tree_sitter::Point {
    let before = &text[..byte];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    tree_sitter::Point {
        row: before.bytes().filter(|&b| b == b'\n').count(),
        column: byte - line_start,
    }
}

/// 常駐デーモンとして起動する（`minae daemon serve`）。
pub async fn run() -> std::io::Result<()> {
    serve(&mina_protocol::socket_path()).await
}

/// 同時に処理する接続数の上限（6b: 接続の張り放題による fd/タスク枯渇対策）。
/// 上限を超えた接続はキューに残る（accept されない）。
///
/// ponytail: 対話クライアントは実質1。ワンショットのコマンドクライアントは
/// 接続スコープの所有権（HIGH-1）で保護される。確立済みの接続にはアイドル
/// タイムアウトを付けない — TUI は読書中もアイドルになるのが正常で、切断
/// されると有害。無言接続のスロット枯渇 DoS（MEDIUM-5）は、最初のコマンド
/// だけに付けたタイムアウト（[`FIRST_COMMAND_TIMEOUT`]）で防ぐ。
const MAX_CONNECTIONS: usize = 4;

/// 接続に振る一意 ID のカウンタ（undo グループの所有者判定に使う）。
/// 0 は「接続なし」（テストの既定）なので 1 から振る。
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// `path` に bind し、socket ファイルの mode を 0600 に絞る（MEDIUM-3）。
///
/// プロセス umask が 0022 だと socket は 0755 で作られ、共有 /tmp の
/// マルチユーザ環境で別ユーザが接続・spoofing できる。bind 直後の chmod は
/// umask に関係なく mode を確定させる。stale socket の除去・再試行（M6）の
/// 二度目の bind でも同じ経路を通る。
fn bind_listener(path: &Path) -> std::io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// `path` で待ち受ける。
///
/// M6: 既存の socket を無条件に remove しない。bind が AddrInUse で失敗したら
/// connect プローブで判定し、生きている daemon の socket なら終了（スプリット
/// ブレイン防止）、前回の異常終了の残骸（stale）なら除去して再試行する。
pub async fn serve(path: &Path) -> std::io::Result<()> {
    let daemon = Arc::new(Mutex::new(Daemon::new()));
    // ADR-0013: 全購読クライアント（Interactive）へ最新 StateSnapshot を配る
    // push チャネル。watch は「最新1件だけ保持・値が変わらなければ受信側を
    // 起こさない」ので、no-op 応答で購読者に push が飛ぶことはない
    // （オーバーフローも失われるのは中間世代のみで、フルスナップショット
    // なので最新に収束する）。
    let (push_tx, _) = watch::channel((None, StateSnapshot::default()));
    // ADR-0012/0015: 外部変更監視（全オープン文書の mtime+size をポーリング）
    {
        let daemon_task = daemon.clone();
        let push_task = push_tx.clone();
        tokio::spawn(watch_disk(daemon_task, push_task));
    }
    match bind_listener(path) {
        Ok(listener) => accept_loop(listener, daemon, push_tx).await,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another daemon is running",
                ));
            }
            // stale socket: 除去して再試行
            let _ = std::fs::remove_file(path);
            let listener = bind_listener(path)?;
            accept_loop(listener, daemon, push_tx).await
        }
        Err(e) => Err(e),
    }
}

/// 外部変更検知タスク（ADR-0012/0015）: 全オープン文書の mtime+size を
/// 定期的にベースラインと照合し、乖離を検知したら自動リロードする（Dirty でも
/// 常時）。リロードは Transaction として記録されるため undo 可能で、dirty は
/// 解消される（テキストがディスクと一致するため）。外部削除はフォーカス文書を
/// `deleted` 状態として保留し、Close を待つ。検知した変更は push チャネルで
/// 即時配信する（エージェントの WaitFor も wake される）。
///
/// ponytail: mtime+size はヒューリスティック（mtime を保存するツールや粗い
/// mtime 粒度の FS では見逃しうる）。文書ごとのベースラインは Open/Save/
/// Close で更新される。
async fn watch_disk(
    daemon: Arc<Mutex<Daemon>>,
    push_tx: watch::Sender<(Option<u64>, StateSnapshot)>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        // フェーズ1（ロック内）: ベースラインと stat を照合し、リロード対象と
        // フォーカス文書の削除/復活を判定する。
        let (reload_targets, focused_deleted, focused_reappeared) = {
            let d = daemon.lock().await;
            let focused = d.editor.focused_path().map(Path::to_path_buf);
            let mut reload_targets = Vec::new();
            let mut focused_deleted = false;
            let mut focused_reappeared = false;
            for (path, baseline) in &d.baselines {
                match std::fs::metadata(path) {
                    Ok(md) => {
                        let changed = md.len() != baseline.size
                            || md.modified().unwrap_or(std::time::UNIX_EPOCH) != baseline.mtime;
                        if changed {
                            reload_targets.push(path.clone());
                        }
                        if focused.as_deref() == Some(path.as_path()) {
                            focused_reappeared = true;
                        }
                    }
                    Err(_) => {
                        // 外部で削除された: フォーカス文書なら Close 待ちへ
                        if focused.as_deref() == Some(path.as_path()) {
                            focused_deleted = true;
                        }
                    }
                }
            }
            (reload_targets, focused_deleted, focused_reappeared)
        };
        // フェーズ2（ロック外）: リロード対象のテキストを読む
        let mut contents = Vec::new();
        for path in &reload_targets {
            if let Ok(text) = tokio::fs::read_to_string(path).await {
                contents.push((path.clone(), text));
            }
        }
        // フェーズ3（ロック内）: リロード適用・削除状態更新・LSP 同期対象の決定
        let mut snap = None;
        let mut lsp_sync = None;
        {
            let mut d = daemon.lock().await;
            if focused_deleted && d.deleted.is_none() {
                if let Some(p) = d.editor.focused_path() {
                    d.deleted = Some(p.to_string_lossy().into_owned());
                    d.record_event(
                        EventSource::External,
                        EventKind::ExternalChange,
                        None,
                        None,
                    );
                }
            } else if focused_reappeared && d.deleted.is_some() {
                d.deleted = None;
            }
            let mut reloaded = false;
            for (path, text) in &contents {
                // 閉じられた文書のパスは対象外
                let Some(doc_id) = d.editor.doc_id_for_path(path) else {
                    continue;
                };
                // 外部書き込みとして Insert グループを閉じる（ADR-0007 と同原則）
                if d.insert_owner.is_some() {
                    close_insert_session(&mut d, mina_view::Mode::Normal);
                }
                if d.editor.reload_doc(doc_id, text) {
                    reloaded = true;
                    if let Ok(md) = std::fs::metadata(path) {
                        d.baselines.insert(
                            path.clone(),
                            DiskBaseline {
                                mtime: md
                                    .modified()
                                    .unwrap_or(std::time::UNIX_EPOCH),
                                size: md.len(),
                            },
                        );
                    }
                    d.record_event(
                        EventSource::External,
                        EventKind::ExternalChange,
                        None,
                        None,
                    );
                    // LSP 全文同期はフォーカス文書のときだけ（診断もフォーカス
                    // 文書のものしか保持しない — drain_into の契約）
                    if d.editor.focused_path().map(Path::to_path_buf).as_deref()
                        == Some(path.as_path())
                    {
                        lsp_sync = Some((path.clone(), text.clone()));
                    }
                }
            }
            if reloaded || focused_deleted || focused_reappeared {
                snap = Some(snapshot(
                    &mut d,
                    reloaded.then(|| "reloaded from disk".into()),
                ));
            }
        }
        // フェーズ4（ロック外）: LSP 全文同期 + pull 診断（編集と同経路）。
        // セッションの取り出しと await を分離する（if-let のスコルチニーに一時
        // MutexGuard を置くと本体までロックが生き残り、非再入 Mutex の再ロック
        // で自己デッドロックする — 実サーバで発症した）。
        if let Some((path, text)) = &lsp_sync {
            let session = {
                let d = daemon.lock().await;
                d.session_for(path)
            };
            if let Some(session) = session {
                // ADR-0028: 同期中を Activity として公開する（フォーカス文書のみ。
                // lsp_sync はフォーカス文書の場合にだけ設定される）。
                snap = Some(sync_after_edit(
                    &daemon,
                    push_tx.clone(),
                    Some((session, path.clone(), text.clone())),
                    snap.and_then(|s| s.status),
                    Some((ActivityKind::ReloadSync, "再読込同期中")),
                ).await);
            }
        }
        // 世代が進んでいれば全購読者へ配る（ADR-0013 と同条件）。発信元は
        // コマンドでない（外部監視）ため None を包み、全クライアントに届く。
        if let Some(snap) = snap {
            let changed = push_tx.borrow().1.generation != snap.generation;
            if changed {
                let _ = push_tx.send((None, snap));
            }
        }
    }
}

/// MEDIUM-3: 接続元 uid が daemon 自身の uid と一致するか（spoofing 対策）。
///
/// socket を 0600 にしても「同じ uid の別プロセスが先に bind して偽 daemon を
/// 立てる」余地は残るが、接続受付ごとに peer uid を検証することで別ユーザの
/// 偽 daemon への接続・状態破壊を拒否する。
fn is_peer_allowed(peer_uid: u32, daemon_uid: u32) -> bool {
    peer_uid == daemon_uid
}

/// 接続を受け付け、接続ごとにコマンド処理タスクを立てる。
async fn accept_loop(
    listener: UnixListener,
    daemon: Arc<Mutex<Daemon>>,
    push_tx: watch::Sender<(Option<u64>, StateSnapshot)>,
) -> std::io::Result<()> {
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    // MEDIUM-3: daemon 自身の uid。共有 /tmp では別ユーザの接続を拒否する。
    let daemon_uid = unsafe { libc::getuid() };
    loop {
        let (stream, _) = listener.accept().await?;
        // MEDIUM-3: peer uid が取れない（エラー）場合も含め、daemon の uid と
        // 一致しない接続は即切断する（fail closed）。ただし macOS の getpeereid
        // は accept 直後の短い間 ENOTCONN を返すことがあるため、数回リトライ
        // してから判断する（リトライせず fail closed だと正当な接続が落ちる）。
        let mut peer_uid = stream.peer_cred().map(|c| c.uid());
        for _ in 0..10 {
            if peer_uid.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            peer_uid = stream.peer_cred().map(|c| c.uid());
        }
        if peer_uid.map_or(true, |uid| !is_peer_allowed(uid, daemon_uid)) {
            drop(stream);
            continue;
        }
        let permit = match connections.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return Ok(()), // セマフォが閉じられた（起きない）
        };
        let daemon = daemon.clone();
        let push_tx = push_tx.clone();
        let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _permit = permit; // 接続処理中は許可を保持
            handle_connection(stream, daemon, conn_id, push_tx).await;
        });
    }
}

/// 1接続分: NDJSON でコマンドを読み、応答スナップショットを返す。
/// Interactive クライアントには他クライアントの変更も push する（ADR-0013）。
///
/// ponytail: 接続ごとに全状態スナップショットを返す（O(n)/コマンド）。
/// 巨大ファイルで問題になったら差分送信に差し替える。
async fn handle_connection(
    stream: UnixStream,
    daemon: Arc<Mutex<Daemon>>,
    conn_id: u64,
    push_tx: watch::Sender<(Option<u64>, StateSnapshot)>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // ADR-0012: 最初のメッセージは Hello（クライアント種別の宣言）でなければ
    // ならない。Hello でない・不正な kind は即切断する。
    // MEDIUM-5: 無言接続がスロットを永久に占有しないよう Hello にタイムアウトを付ける。
    let mut hello_line = String::new();
    let read = {
        let mut bounded = (&mut reader).take(MAX_CMD_LINE as u64 + 1);
        timeout(FIRST_COMMAND_TIMEOUT, bounded.read_line(&mut hello_line)).await
    };
    let (kind, reset_cursor, client_name) = match read {
        Ok(Ok(0)) => return,
        Ok(Ok(_)) => match serde_json::from_str::<Hello>(hello_line.trim()) {
            Ok(hello) => (
                hello.kind,
                hello.reset_cursor_on_disconnect,
                hello.name,
            ),
            Err(_) => return, // Hello でない・不正な kind: 切断
        },
        Ok(Err(_)) => return,
        Err(_) => return, // 無言接続: タイムアウトで切断
    };
    let source = match kind {
        ClientKind::Interactive => EventSource::Interactive,
        ClientKind::Headless => EventSource::Headless,
    };

    // ADR-0027: 最後の Interactive 切断判定用に登録しておく。
    // ADR-0037: Interactive 接続には専用の View を割り当てる（Headless は
    // 共有 idle view を使う — 接続の View は持たない）。
    if kind == ClientKind::Interactive {
        let mut d = daemon.lock().await;
        d.register_conn_view(conn_id);
        d.interactive_clients.insert(conn_id);
    }
    // ADR-0038: 自己申告ラベルを保持する（activity の actor に使う）
    daemon.lock().await.conn_names.insert(conn_id, client_name);

    // ADR-0013: Interactive クライアントだけが push を購読する。Headless の
    // ワンショット CLI は応答1行を読んで切断するので、push が混ざると壊れる。
    // 初期値を既読にしておく（購読直後に偽の push を送らない）。
    let mut push_rx = (kind == ClientKind::Interactive).then(|| push_tx.subscribe());
    if let Some(rx) = &mut push_rx {
        rx.borrow_and_update();
    }

    // SEC-1: 改行のない無限ストリームで行バッファが無制限に育たないよう、
    // 読み取りバイト数自体を take で上限する（超過行は後段で切断）。
    // lines() は next_line が cancel safe（tokio 保証）なので、push 受信との
    // select! でコマンド行を失わない（read_line は cancel unsafe のため不可）。
    let mut lines = reader.take(MAX_CMD_LINE as u64 + 1).lines();

    loop {
        let next = if let Some(rx) = &mut push_rx {
            tokio::select! {
                l = lines.next_line() => ReadNext::Command(l),
                c = rx.changed() => ReadNext::Push(c),
            }
        } else {
            ReadNext::Command(lines.next_line().await)
        };
        match next {
            // ADR-0013: 他クライアント・daemon 起動の状態変化を購読者へ配る。
            // watch は最新1件を保持するので、中間世代の欠落は許容（フルスナップ
            // ショットなので必ず最新に収束する）。
            ReadNext::Push(Ok(())) => {
                let (origin, payload) = push_rx
                    .as_mut()
                    .expect("push 分岐は購読時のみ")
                    .borrow_and_update()
                    .clone();
                // ADR-0013: 発信元自身へは push しない — 応答で同じ状態を既に
                // 持っている（自分宛 push の JSON 直列化・転送を節約）。
                // daemon 起動の push（外部リロード・LSP settle）は None で
                // 届き、全購読者が受ける。
                if origin == Some(conn_id) {
                    continue;
                }
                // v12（ADR-0037）: push は購読者ごとの View から合成する —
                // ブロードキャストされる snapshot は発信元の View 基準なので、
                // ここで自分の View のスナップショットを作り直す（状態は既に
                // daemon に反映済み）。status は変化の性質（外部リロード等）なので
                // 引き継ぐ。
                let snapshot = {
                    let mut d = daemon.lock().await;
                    let record = d.conn_views.get(&conn_id);
                    let view_id = record.map_or(d.idle_view, |r| r.view_id);
                    let viewport = record.map_or(24, |r| r.viewport);
                    snapshot_from_view(&mut d, view_id, viewport, payload.status)
                };
                if !write_message(&mut write_half, conn_id, ServerMessage::Push { snapshot })
                    .await
                {
                    break; // 切断 or 書き込みタイムアウト
                }
            }
            ReadNext::Push(Err(_)) => break, // push 送信元が消えた（daemon 終了）
            ReadNext::Command(Ok(Some(line))) => {
                if line.len() > MAX_CMD_LINE {
                    break; // 過大なコマンド行: クライアントが壊れているか悪意がある
                }
                // ADR-0020: GetInlayHints はスナップショットでなく Hints を返す
                // 読み取り専用コマンド（世代・push・イベントを進めない）。
                // process_command の戻り型を汚さないため、ここで専用処理する。
                // headless のエージェント用途が主だが、読み取り専用かつ自己修復
                // （セッション復元 + 診断再 pull）するため種別は問わない。
                if let Ok(Command::GetInlayHints { path }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_inlay_hints(&daemon, &path).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                // PeekDefinition も同様に専用処理（LSP の await はロック外で行う）。
                // 応答はスナップショット（peek フィールド付き）で通常経路と同じ形状。
                if let Ok(Command::PeekDefinition) = serde_json::from_str::<Command>(line.trim()) {
                    let message = serve_peek_definition(&daemon, conn_id).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                // PeekDefinitionAt（ADR-0025）: 任意パスの指定位置の定義を全文なしの
                // 軽量応答（ServerMessage::Peek）で返す。GetInlayHints と同じく
                // 読み取り専用コマンドなので専用処理する。
                if let Ok(Command::PeekDefinitionAt { path, line, col }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_peek_definition_at(&daemon, &path, line, col).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                // Rename / References（ADR-0029）: 内容指定の意味リネームと参照列挙。
                // LSP の await をロック外で行うため専用処理（serve_peek_definition_at
                // と同格）。Rename はテキストを変える（headless ゲートの例外）。
                if let Ok(Command::Rename { path, old, new }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_rename(&daemon, &path, &old, &new).await;
                    // ADR-0038: Headless 由来の Rename 試行を記録する（応答は
                    // 送出済み = 世代確定前のため、記録は次の push に載る）。
                    if source == EventSource::Headless {
                        if let ServerMessage::RenameResult {
                            error,
                            files,
                            edits,
                            ..
                        } = &message
                        {
                            let mut d = daemon.lock().await;
                            let actor = d.actor_for(conn_id);
                            let (ok, detail) = match error {
                                None => (
                                    true,
                                    format!("{old} → {new} in {path}: {files} files, {edits} edits"),
                                ),
                                Some(e) => (false, e.clone()),
                            };
                            d.record_activity(actor, EventKind::Rename, ok, detail);
                        }
                    }
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                if let Ok(Command::References { path, old }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_references(&daemon, &path, &old).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                // Outline / EnclosingSymbol（ADR-0031）: シンボルの階層ツリーと
                // 位置を囲む記号の範囲を全文なしの軽量応答で返す。どちらも
                // 読み取り専用 — PeekDefinitionAt と同じく専用処理する。
                if let Ok(Command::Outline { path }) = serde_json::from_str::<Command>(line.trim()) {
                    let message = serve_outline(&daemon, &path).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                // OutlineRecursive（ADR-0049）: ファイル分割モジュールを辿る
                // 階層ツリー。読み取り専用 — Outline と同じく専用処理する。
                if let Ok(Command::OutlineRecursive { path, depth }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_outline_recursive(&daemon, &path, depth).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                // ReadPath（ADR-0048）: パス指定の軽量テキスト読み。LSP 非依存・
                // バッファ非依存 — 読み取り専用で状態・世代を動かさないため、
                // PeekDefinitionAt / Outline と同じく専用処理する。
                if let Ok(Command::ReadPath { path }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_read_path(&daemon, &path).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                if let Ok(Command::EnclosingSymbol { path, line, col }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_enclosing_symbol(&daemon, &path, line, col).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                // HoverAt / WorkspaceSymbol / CheckDiagnostics（ADR-0032）: 位置の
                // hover（型・シグネチャ）、ワークスペース内シンボル検索、診断
                // settle 待ち + コンパクト診断。どれも読み取り専用 —
                // PeekDefinitionAt / Outline と同じく専用処理する。
                if let Ok(Command::HoverAt { path, line, col }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_hover_at(&daemon, &path, line, col).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                if let Ok(Command::WorkspaceSymbol { path, query }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_workspace_symbols(&daemon, &path, &query).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                if let Ok(Command::CheckDiagnostics { path }) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_check_diagnostics(&daemon, &path).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                // GetServerInfo（issue #27/D1）: daemon のビルド世代と累積メトリクスを
                // 軽量応答（ServerMessage::ServerInfo）で返す読み取り専用コマンド。
                // GetInlayHints と同じく専用処理する。
                if let Ok(Command::GetServerInfo) = serde_json::from_str::<Command>(line.trim()) {
                    let message = serve_server_info(&daemon).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                // ListReviewComments（#50）: レビューコメントの全文付き軽量応答
                // （ServerMessage::ReviewComments）で返す読み取り専用コマンド。
                // headless ゲートの例外（`minas review` の抽出経路）。TUI も
                // マーカー・編集prefill 用に同じ経路で取得する。
                if let Ok(Command::ListReviewComments) =
                    serde_json::from_str::<Command>(line.trim())
                {
                    let message = serve_review_comments(&daemon).await;
                    if !write_message(&mut write_half, conn_id, message).await {
                        break; // 切断 or 書き込みタイムアウト
                    }
                    continue;
                }
                let snapshot = process_command(&daemon, &push_tx, conn_id, source, &line).await;
                // ADR-0013: 状態が変わったときだけ全購読者へ配る。watch の send は
                // 値の等価性でなく送信ごとに受信側を起こすため、世代が進んでいない
                // 応答（GetState・拒否・no-op など）で送ると無駄な push が飛ぶ。
                // 発信元には届かない（応答で持っているため）— 他クライアント
                // と daemon 起動の購読者だけが受ける。
                let changed = push_tx.borrow().1.generation != snapshot.generation;
                if changed {
                    let _ = push_tx.send((Some(conn_id), snapshot.clone()));
                }
                // #52: 基準診断の遅延充填。注目文書の対応物が未キャッシュなら
                // 后台 pull を1件だけ起動する（重複抑止つき。世代は進めない —
                // 到着は push で届く）。応答より先に引き当てだけ済ませる。
                let base_pull = {
                    let mut d = daemon.lock().await;
                    d.take_base_pull(snapshot.path.as_deref().map(Path::new))
                };
                if !write_message(&mut write_half, conn_id, ServerMessage::Response { snapshot })
                    .await
                {
                    break; // 切断 or 書き込みタイムアウト
                }
                if let Some(bp) = base_pull {
                    let dc = daemon.clone();
                    let pc = push_tx.clone();
                    tokio::spawn(async move {
                        fill_base_diagnostics(&dc, &pc, bp).await;
                    });
                }
            }
            ReadNext::Command(Ok(None)) | ReadNext::Command(Err(_)) => break, // クライアントの切断
        }
    }
    // 切断の後始末: 接続の View を破棄し（ADR-0037）、所有者（グループを開いた
    // クライアント）ならグループを閉じモードを Normal に戻す（ADR-0007 / HIGH-1）。
    // 最後の Interactive なら Hello 宣言どおり idle view のカーソルを先頭へ戻す
    // （ADR-0027 の v12 再解釈）。
    {
        let mut d = daemon.lock().await;
        d.drop_conn_view(conn_id);
        d.conn_names.remove(&conn_id);
        d.on_client_disconnect(conn_id, kind, reset_cursor);
    }
}

/// コマンドループの1周で読み取るもの（コマンド行 or push 通知）。
enum ReadNext {
    Command(io::Result<Option<String>>),
    Push(Result<(), watch::error::RecvError>),
}

/// `Command::ListReviewComments` の処理（#50）: レビューコメントの全文付き
/// 軽量応答（[`ServerMessage::ReviewComments`]）で返す。読み取り専用:
/// 世代・push・イベントは進めない。照合（stale 判定）は解決時点の読み取りで、
/// 保存値は書き換えない。
async fn serve_review_comments(daemon: &Mutex<Daemon>) -> ServerMessage {
    let d = daemon.lock().await;
    ServerMessage::ReviewComments {
        generation: d.generation,
        comments: d.resolve_review_comments(),
    }
}

/// `Command::GetServerInfo` の処理（issue #27/D1）: daemon のビルド世代と
/// 起動からの累積メトリクスを軽量応答（[`ServerMessage::ServerInfo`]）で返す。
///
/// 目的は「古いビルドの daemon が新プロトコル項目を黙殺していないか」を
/// クライアント側で検知可能にすること（silent ignore の防止）。読み取り専用:
/// 世代・push・イベントは進めない。ビルド世代は build.rs が注入した
/// `MINA_GIT_HASH` / `MINA_BUILD_TS`（`option_env!`。取り込まれない環境向けに
/// フォールバックを持つ）。
async fn serve_server_info(daemon: &Mutex<Daemon>) -> ServerMessage {
    let d = daemon.lock().await;
    ServerMessage::ServerInfo {
        generation: option_env!("MINA_GIT_HASH").unwrap_or("unknown").to_string(),
        daemon_build_ts: option_env!("MINA_BUILD_TS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        metrics: d.metrics,
        // #49: 登録中の基準 root（表示用に root 順で安定化）。
        base_roots: {
            let mut v: Vec<BaseRootInfo> = d
                .base_roots
                .iter()
                .map(|(r, c)| BaseRootInfo {
                    root: r.to_string_lossy().into_owned(),
                    commit: c.commit.clone(),
                })
                .collect();
            v.sort_by(|a, b| a.root.cmp(&b.root));
            v
        },
    }
}

/// メッセージ1件を NDJSON で書き込む。成功なら true、切断・書き込み
/// タイムアウトなら false。
async fn write_message(
    write_half: &mut OwnedWriteHalf,
    conn_id: u64,
    message: ServerMessage,
) -> bool {
    // 応答のシリアライズ + socket 書き込み（ADR-0056 の `write` スパン）。
    // 計時ログが無効なら Instant 1 回だけで、wire は不変。
    let mut trace = Trace::new("write");
    let mut out = serde_json::to_string(&message).expect("メッセージはシリアライズ可能");
    out.push('\n');
    trace.note("bytes", out.len());
    trace.mark("serialize");
    // MEDIUM-2: 応答を読まないクライアントが socket バッファを詰まらせて
    // 接続スロットを永久に占有しないよう、書き込みにタイムアウトを付ける。
    // タイムアウト・切断のいずれも接続を閉じて後始末に進む。
    let wrote = timeout(RESPONSE_WRITE_TIMEOUT, write_half.write_all(out.as_bytes())).await;
    trace.mark("socket");
    if !matches!(wrote, Ok(Ok(()))) {
        eprintln!("[conn {}] write TIMED OUT: {:?}", conn_id, wrote);
        return false;
    }
    true
}

/// `Command::GetInlayHints` の処理（ADR-0020）: 任意パスの inlay hint を全文
/// テキストなしで返す（エージェントの LLM コスト削減経路）。
///
/// LSP セッションは同時に 1 文書しか開けないため、フォーカス文書と異なる
/// パスの要求は「対象を didOpen → pull → フォーカス文書を現在テキストで
/// didOpen し直し + 診断の再 pull」で対応する（Q10-(c)。切り替えウィンドウ中に
/// 入った編集は全文同期の復元で整合する）。LSP の await は daemon ロック外
/// （ADR-0009）。読み取り専用: 世代・push・イベントは進めない。
/// 位置指定 LSP 要求（peek_at / references / rename）の共通お膳立て:
/// 正規化 → テキスト解決（開文書優先・ディスク）→ LSP 対応ゲート →
/// フォーカス文書の記録 → セッション確保 → 対象の didOpen までを1経路に持つ。
/// 成功時、呼び出し側は LSP クエリだけを行い、借りていた場合は
/// [`restore_focus_session`] / [`restore_focus_after_semantic`] で返す
/// （Q10-(c) の自己修復）。失敗理由は呼び出し側が固有の空応答・エラー文に写像する。
///
/// serve_inlay_hints は使わない（キャッシュ高速経路と世代付き空応答が構造を
/// 分けるため、独自のお膳立てを維持する）。
struct Borrowed {
    path: PathBuf,
    /// 正規化後の表示用パス（空応答・応答の path フィールド用）。
    path_str: String,
    text: String,
    /// お膳立て時点のセッション解析フォーカス文書（復元用。テキストは復元時に最新を読む）。
    focused: Option<PathBuf>,
    session: Arc<Mutex<LspSession>>,
}

/// 借用お膳立ての失敗理由。
enum BorrowFail {
    /// 対象が開文書にもディスクにもない（メッセージは元入力をそのまま使う）。
    CannotOpen,
    /// LSP 非対応パス（.rs 以外）。表示用に正規化後のパスを渡す。
    NoServer(String),
    /// spawn + initialize 失敗。
    SpawnFailed(String),
}

async fn prepare_borrowed_session(
    daemon: &Mutex<Daemon>,
    path: &str,
) -> Result<Borrowed, BorrowFail> {
    // ADR-0056: borrow の内訳（normalize / text / languages / ensure / didOpen）。
    // didOpen は入れ子の `didOpen` スパン（ロック待ち vs 送信）で分かる。
    let mut trace = Trace::new("borrow");
    let path_buf = normalize_open_path(PathBuf::from(path)).await;
    let path_str = path_buf.to_string_lossy().into_owned();
    trace.mark("normalize");
    // 対象テキスト: Editor の開文書を優先し、なければディスク読み
    // （SEC-1 検証済み。ADR-0008 の read_open_target を再利用）。
    let text = {
        let d = daemon.lock().await;
        d.editor
            .doc_id_for_path(&path_buf)
            .map(|id| d.editor.document(id).text().to_string())
    };
    let (text, _status) = match text {
        Some(t) => (Some(t), None),
        None => read_open_target(&path_str).await,
    };
    let Some(text) = text else {
        return Err(BorrowFail::CannotOpen);
    };
    trace.mark("text");
    // LSP 非対応パス（テーブルにサーバ割当なし）: サーバを spawn しない（ADR-0030）
    let lsp_supported = daemon
        .lock()
        .await
        .languages_refresh()
        .server_for(&path_buf)
        .is_some();
    if !lsp_supported {
        return Err(BorrowFail::NoServer(path_str));
    }
    trace.mark("languages");
    // 解析フォーカス文書（復元用 — v12: 借りたセッションの共有解析フォーカスを
    // 復元対象にする。per-view のフォーカス文書ではない）。テキストは復元時に最新を読む。
    let focused = {
        let d = daemon.lock().await;
        d.session_key(&path_buf)
            .and_then(|key| d.analysis_focus.get(&key).cloned())
    };
    let session = match ensure(daemon, &path_buf).await {
        Ok(s) => s,
        Err(e) => return Err(BorrowFail::SpawnFailed(e)),
    };
    trace.mark("ensure");
    // 切り替え: 対象文書を didOpen（現在の文書にしか応えないため、対象を開く
    // ことは必須）。既に開いている場合の再 didOpen は無害。
    lsp::open_document(&session, &path_buf, &text).await;
    trace.mark("didOpen");
    Ok(Borrowed {
        path: path_buf,
        path_str,
        text,
        focused,
        session,
    })
}

/// セマンティック要求（references / rename）共通の位置解決:
/// ワークスペース内の同拡張子ファイルを didOpen してから（未開ファイルの参照を
/// 取りこぼさない — 実測: ra は開いていないファイルの参照を返さない）、識別子
/// `old` の最初の出現を LSP 座標（行:列）へ解決する。コメント・文字列内には
/// 解決しない（T3 の誤位置事故を予防）。失敗は `Err(エラーメッセージ)`
/// （シンボル未解決・セッションロック待ち）。
async fn resolve_symbol_lsp_pos(
    daemon: &Mutex<Daemon>,
    session: &Arc<Mutex<LspSession>>,
    path: &Path,
    text: &str,
    old: &str,
) -> Result<(u32, u32), String> {
    open_workspace_files(daemon, session, path).await;
    // サーバ（tsserver 等）は要求対象のファイルが開いている必要がある —
    // open_workspace_files が last 開く文書で対象が閉じられると rename に null を
    // 返す（probe 実測）。対象を**閉じずに**開き直し、ワークスペースの他ファイルも
    // 開いたまま保つ（keep-open。did_open だと他ファイルを閉じてしまう）。
    lsp::open_document_keep(session, path, text).await;
    let grammar = daemon.lock().await.languages.grammar_for_path(path);
    let Some(char_idx) = lsp::find_symbol_char_idx(grammar, text, old) else {
        return Err(format!("symbol not found: {old:?} in {}", path.display()));
    };
    let (line, character) = {
        let Ok(s) = timeout(lsp::LSP_LOCK_TIMEOUT, session.lock()).await else {
            return Err("LSP セッションのロックを取得できませんでした".into());
        };
        s.char_to_lsp_pos(text, char_idx)
    };
    Ok((line, character))
}

async fn serve_inlay_hints(daemon: &Mutex<Daemon>, path: &str) -> ServerMessage {
    // ADR-0056: hints の内訳（RA の inlayHint 計算 vs 借用・復元）。
    let mut trace = Trace::new("hints");
    let path_buf = normalize_open_path(PathBuf::from(path)).await;
    let path_str = path_buf.to_string_lossy().into_owned();
    // 要求パスのテキスト: Editor の開文書を優先し、なければディスク読み
    // （SEC-1 検証済み。ADR-0008 の read_open_target を再利用）。
    let text = {
        let d = daemon.lock().await;
        d.editor
            .doc_id_for_path(&path_buf)
            .map(|id| d.editor.document(id).text().to_string())
    };
    let (text, _status) = match text {
        Some(t) => (Some(t), None),
        None => read_open_target(&path_str).await,
    };
    let Some(text) = text else {
        // 読み込み不可（存在しない・非正規ファイル等）: 空ヒントで応答する
        let d = daemon.lock().await;
        return ServerMessage::Hints {
            path: path_str,
            generation: d.generation,
            hints: Vec::new(),
        };
    };
    // LSP 非対応パス（テーブルにサーバ割当なし）: 空ヒントで応答する（ADR-0030）
    let lsp_supported = daemon
        .lock()
        .await
        .languages_refresh()
        .server_for(&path_buf)
        .is_some();
    if !lsp_supported {
        let d = daemon.lock().await;
        return ServerMessage::Hints {
            path: path_str,
            generation: d.generation,
            hints: Vec::new(),
        };
    }
    // キャッシュが現在のテキストに対して新鮮なら LSP に触らず返す
    // （エージェントの反復要求で rust-analyzer の再解析を起こさない）。
    let checksum = fnv1a64(text.as_bytes());
    {
        let d = daemon.lock().await;
        if let Some(c) = d.hints.get(&path_buf) {
            if c.text_checksum == checksum {
                return ServerMessage::Hints {
                    path: path_str,
                    generation: d.generation,
                    hints: c.hints.clone(),
                };
            }
        }
    }
    // 解析フォーカス文書（復元用 — v12: 借りたセッションの共有解析フォーカスを
    // 復元対象にする。per-view のフォーカス文書ではない）。テキストは復元時に最新を読む。
    let focused = {
        let d = daemon.lock().await;
        d.session_key(&path_buf)
            .and_then(|key| d.analysis_focus.get(&key).cloned())
    };
    let session = match ensure(daemon, &path_buf).await {
        Ok(s) => s,
        Err(_) => {
            // spawn + initialize 失敗: 空ヒントで応答する
            let d = daemon.lock().await;
            return ServerMessage::Hints {
                path: path_str,
                generation: d.generation,
                hints: Vec::new(),
            };
        }
    };
    let target_is_focused = focused.as_deref() == Some(path_buf.as_path());
    // フォーカス文書と同じ WorkspaceRoot で LSP 対応のときだけセッションを借りた
    // ことになる（ADR-0010）。別 root ならフォーカス文書のセッションには触れて
    // いないので復元不要（借りたセッションの current_uri は次回要求の didOpen で
    // 置き換わる）。
    let borrows_focus_session = !target_is_focused
        && daemon.lock().await.borrows_focus_session(&focused, &path_buf);
    // 切り替え: 対象文書を didOpen（前の文書は閉じられる）。pull は現在の文書に
    // しか応えない（current_uri 一致チェック）ため、対象を開くことは必須。
    lsp::open_document(&session, &path_buf, &text).await;
    let hints = lsp::pull_hints_timeout(&session, &path_buf, &text)
        .await
        .unwrap_or_default();
    trace.mark("pull");
    // キャッシュ更新（daemon ロックは短時間のみ）
    {
        let mut d = daemon.lock().await;
        d.cache_hints(path_buf.clone(), &text, hints.clone());
    }
    if borrows_focus_session {
        // 復元（Q10-(c)）: フォーカス文書へ戻し、更新停止を自己修復する
        restore_focus_session(daemon, &session, &focused).await;
        trace.mark("restore");
    }
    let d = daemon.lock().await;
    ServerMessage::Hints {
        path: path_str,
        generation: d.generation,
        hints,
    }
}

/// `Command::PeekDefinition` の処理: カーソル位置のシンボル定義を確認用
/// スニペットとして返す（読み取り専用。定義にジャンプしない）。
///
/// LSP の await は daemon ロック外（ADR-0009）。応答はスナップショットの
/// `peek` フィールドに載せる（daemon 状態には持たない — 次のコマンドで消える
/// クライアント側の一時表示。push には載らない）。フォーカス文書を開き直すので
/// 借用・復元は不要（serve_peek_definition_at と違い、対象は常にフォーカス文書）。
async fn serve_peek_definition(daemon: &Mutex<Daemon>, conn_id: u64) -> ServerMessage {
    let (path, head, text, lsp_supported) = {
        let mut d = daemon.lock().await;
        d.focus_for(conn_id);
        match d.editor.focused_path().map(Path::to_path_buf) {
            Some(path) => {
                let head = d.editor.selection().primary().head();
                let text = d.editor.current_document().text().to_string();
                let lsp_supported = d.languages_refresh().server_for(&path).is_some();
                (path, head, text, lsp_supported)
            }
            // 開いていない: peek なしのスナップショットで応答
            None => return ServerMessage::Response { snapshot: snapshot(&mut d, None) },
        }
    };
    let peek = if lsp_supported {
        match ensure(daemon, &path).await {
            // サーバが definition を提供していなければ peek なし（Stage 3）
            Ok(session) if session.lock().await.caps.definition => {
                // セッションを借りた（開き直し）ので現在のテキストで didOpen する。
                // 既に開いている場合の再 didOpen は無害（idempotent）。
                lsp::open_document(&session, &path, &text).await;
                // v12: peek は自分の View の文書を解析フォーカスへ（フォーカス
                // が動いた場合のみ診断を非表示にする — 次の pull で再載る）。
                {
                    let mut d = daemon.lock().await;
                    let moved = !d.is_analysis_focus(&path);
                    d.set_analysis_focus(&path);
                    if moved {
                        d.diagnostics.remove(&path);
                    }
                }
                lsp::definition_peek_at_char(&session, &path, &text, head).await
            }
            Ok(_) => None, // definition 非対応サーバ: peek なし
            Err(_) => None, // サーバ spawn 失敗: peek なし
        }
    } else {
        None // LSP 非対応ファイル（.rs 以外）: peek なし
    };
    let mut d = daemon.lock().await;
    let mut snap = snapshot(&mut d, None);
    snap.peek = peek;
    ServerMessage::Response { snapshot: snap }
}

/// `Command::PeekDefinitionAt` の処理（ADR-0025）: 任意パスの指定位置
/// （1-origin 行:列）のシンボル定義を、全文テキストを返さず軽量応答
/// （[`ServerMessage::Peek`]）で返す（エージェントのトークン削減経路）。
///
/// LSP セッションは同時に 1 文書しか開けないため、フォーカス文書と異なる
/// パスの要求は serve_inlay_hints と同じく「対象を didOpen → 取得 →
/// フォーカス文書を復元 + 診断の再 pull」で対応する（Q10-(c)）。LSP の await は
/// daemon ロック外（ADR-0009）。読み取り専用: 世代・push・イベントは進めない。
/// 定義なし・LSP 非対応・読み込み不可は `text` 空で応答する。
async fn serve_peek_definition_at(
    daemon: &Mutex<Daemon>,
    path: &str,
    line: u32,
    col: u32,
) -> ServerMessage {
    let empty = || ServerMessage::Peek {
        path: path.to_string(),
        line: 0,
        text: String::new(),
    };
    // 読み込み不可・LSP 非対応・spawn 失敗はすべて定義なしで応答する
    // （お膳立ての理由は空応答の形では区別しない）。
    let borrowed = match prepare_borrowed_session(daemon, path).await {
        Ok(b) => b,
        Err(_) => return empty(),
    };
    // サーバが definition を提供していなければ peek なし（空応答。Stage 3）。
    // prepare が対象を didOpen 済みのため、借用していた場合はフォーカス文書へ
    // 戻す（戻さないと LSP の current_uri が対象のまま — 次回編集の同期スキップ・
    // 診断消失。敵対的検証 P1）。
    if !borrowed.session.lock().await.caps.definition {
        restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path)
            .await;
        return empty();
    }
    let borrows = daemon
        .lock()
        .await
        .borrows_focus_session(&borrowed.focused, &borrowed.path);
    let peek =
        lsp::definition_peek_at_line_col(&borrowed.session, &borrowed.path, &borrowed.text, line, col)
            .await;
    if borrows {
        // 復元（Q10-(c)）: フォーカス文書へ戻し、更新停止を自己修復する
        restore_focus_session(daemon, &borrowed.session, &borrowed.focused).await;
    }
    match peek {
        Some(p) => ServerMessage::Peek {
            path: p.path,
            line: p.line,
            text: p.text,
        },
        None => empty(),
    }
}

/// outline キャッシュの高速経路（ADR-0031）: テキストのチェックサムが一致する
/// エントリがあればクローンを返す（LSP 不問）。不一致・不在は `None`。
async fn cached_outline(daemon: &Mutex<Daemon>, path_buf: &Path) -> Option<Vec<OutlineSymbol>> {
    let text = resolve_doc_text(daemon, path_buf).await?;
    let checksum = fnv1a64(text.as_bytes());
    daemon
        .lock()
        .await
        .outlines
        .get(path_buf)
        .filter(|c| c.text_checksum == checksum)
        .map(|c| c.symbols.clone())
}

/// 対象テキストの解決（開文書優先・ディスク読み）。`prepare_borrowed_session` の
/// 前半と同じ形（ADR-0008 の `read_open_target` を再利用）。キャッシュヒット判定用。
async fn resolve_doc_text(daemon: &Mutex<Daemon>, path_buf: &Path) -> Option<String> {
    let in_memory = {
        let d = daemon.lock().await;
        d.editor
            .doc_id_for_path(path_buf)
            .map(|id| d.editor.document(id).text().to_string())
    };
    match in_memory {
        Some(t) => Some(t),
        None => read_open_target(&path_buf.to_string_lossy()).await.0,
    }
}

/// [`Command::Outline`] の処理（ADR-0031）: 任意パスのシンボル階層ツリーを
/// 全文なしの軽量応答（[`ServerMessage::Outline`]）で返す。読み取り専用 —
/// 世代・push・イベントは進めない。
///
/// お膳立て（正規化・テキスト解決・didOpen・復元）は PeekDefinitionAt /
/// References と同じ `prepare_borrowed_session` 経路。失敗は `error: Some(…)`
/// で表す: 対象が読めない・LSP 非対応・spawn 失敗は「not supported」系
/// （exit 1、再試行不可）、LSP エラーは再試行可能（exit 2）。予算切れの
/// 空ツリーは「0 件」として正直に返す（`error: None`）。
async fn serve_outline(daemon: &Mutex<Daemon>, path: &str) -> ServerMessage {
    let err = |msg: String| ServerMessage::Outline {
        path: path.to_string(),
        generation: 0,
        symbols: Vec::new(),
        error: Some(msg),
        truncated: false,
    };
    // キャッシュ高速経路: テキストのチェックサムが一致する outline があれば
    // LSP に触れずに返す（実測: セマンティック要求は毎回の再解析で数秒かかる）。
    // 正規化とテキスト解決は prepare_borrowed_session の前半と同じ形。
    let path_buf = normalize_open_path(PathBuf::from(path)).await;
    if let Some(symbols) = cached_outline(daemon, &path_buf).await {
        let msg = ServerMessage::Outline {
            generation: daemon.lock().await.generation,
            path: path_buf.to_string_lossy().into_owned(),
            symbols,
            error: None,
            truncated: false,
        };
        return record_metric(daemon, msg, true).await;
    }
    let borrowed = match prepare_borrowed_session(daemon, path).await {
        Ok(b) => b,
        Err(BorrowFail::CannotOpen) => return err(format!("cannot open {path}")),
        Err(BorrowFail::NoServer(p)) => {
            return err(format!("outline not supported for {p} (no LSP server configured)"))
        }
        Err(BorrowFail::SpawnFailed(e)) => return err(format!("LSP error: {e}")),
    };
    // サーバが documentSymbol を提供していなければ即「not supported」（exit 1）。
    // prepare が対象を didOpen 済みのため、借用していたらフォーカス文書へ戻す
    // （戻さないと current_uri が対象のまま — 同期スキップ・診断消失）。
    if !borrowed.session.lock().await.caps.document_symbols {
        restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path)
            .await;
        return err(format!(
            "outline not supported for {} (LSP server が documentSymbolProvider を advertise していません)",
            borrowed.path_str
        ));
    }
    let borrows = daemon
        .lock()
        .await
        .borrows_focus_session(&borrowed.focused, &borrowed.path);
    let fetched = lsp::document_symbols_at(&borrowed.session, &borrowed.path, &borrowed.text).await;
    if borrows {
        // 復元（Q10-(c)）: フォーカス文書へ戻し、更新停止を自己修復する
        restore_focus_session(daemon, &borrowed.session, &borrowed.focused).await;
    }
    let (symbols, error) = match fetched {
        Ok(s) => {
            daemon.lock().await.cache_outline(borrowed.path.clone(), &borrowed.text, s.clone());
            (s, None)
        }
        Err(e) => (Vec::new(), Some(e)),
    };
    let msg = ServerMessage::Outline {
        generation: daemon.lock().await.generation,
        path: borrowed.path_str,
        symbols,
        error,
        truncated: false,
    };
    record_metric(daemon, msg, true).await
}

/// 再帰展開のシンボル総数上限（ADR-0049）。応答爆発を防ぐため、横断で
/// 集めたシンボルの総数がこれを超えたら途中で打ち切り、`truncated: true`
/// を応答に載せる（エージェントは必要なら per-file の outline で補う）。
const MAX_RECURSIVE_OUTLINE_SYMBOLS: usize = 500;

/// [`Command::OutlineRecursive`] の処理（ADR-0049）: ファイル分割モジュール
/// （children が空の Module シンボル）を定義ジャンプで解決し、定義ファイルを
/// 再帰的に outline して children に埋める。読み取り専用 — 世代・push・イベント
/// は進めない。応答形状は既存 [`ServerMessage::Outline`]（`truncated` で
/// 打ち切りを通知）。
///
/// モジュール解決は「モジュール名トークン位置 → `textDocument/definition`」
/// （peek と同じ LSP 経路）。解決先が同一ファイル内なら inline `mod { }` と
/// 判断して再帰しない。防御: visited パス集合（循環防止）+ depth 上限 +
/// [`MAX_RECURSIVE_OUTLINE_SYMBOLS`]。
async fn serve_outline_recursive(
    daemon: &Mutex<Daemon>,
    path: &str,
    depth: u32,
) -> ServerMessage {
    let err = |msg: String| ServerMessage::Outline {
        path: path.to_string(),
        generation: 0,
        symbols: Vec::new(),
        error: Some(msg),
        truncated: false,
    };
    if depth == 0 {
        return err("invalid input: depth must be >= 1".into());
    }
    let borrowed = match prepare_borrowed_session(daemon, path).await {
        Ok(b) => b,
        Err(BorrowFail::CannotOpen) => return err(format!("cannot open {path}")),
        Err(BorrowFail::NoServer(p)) => {
            return err(format!("outline not supported for {p} (no LSP server configured)"))
        }
        Err(BorrowFail::SpawnFailed(e)) => return err(format!("LSP error: {e}")),
    };
    if !borrowed.session.lock().await.caps.document_symbols {
        restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path)
            .await;
        return err(format!(
            "outline not supported for {} (LSP server が documentSymbolProvider を advertise していません)",
            borrowed.path_str
        ));
    }
    let borrows = daemon
        .lock()
        .await
        .borrows_focus_session(&borrowed.focused, &borrowed.path);
    let fetched = lsp::document_symbols_at(&borrowed.session, &borrowed.path, &borrowed.text).await;
    let mut visited = std::collections::HashSet::new();
    visited.insert(borrowed.path.clone());
    let mut budget = MAX_RECURSIVE_OUTLINE_SYMBOLS;
    // fetched は 1 回だけ match する（error 応答は空シンボル + error で返す）。
    let (mut symbols, mut error, mut truncated) = (Vec::new(), None, false);
    match fetched {
        Ok(mut s) => {
            daemon.lock().await.cache_outline(borrowed.path.clone(), &borrowed.text, s.clone());
            truncated = expand_outline_modules(
                daemon,
                &borrowed.session,
                &borrowed.path,
                &borrowed.text,
                &mut s,
                depth,
                &mut visited,
                &mut budget,
            )
            .await;
            symbols = s;
        }
        Err(e) => error = Some(e),
    }
    if borrows {
        // 復元（Q10-(c)）: フォーカス文書へ戻し、更新停止を自己修復する
        restore_focus_session(daemon, &borrowed.session, &borrowed.focused).await;
    }
    let msg = ServerMessage::Outline {
        generation: daemon.lock().await.generation,
        path: borrowed.path_str,
        symbols,
        error,
        truncated,
    };
    record_metric(daemon, msg, true).await
}

/// ツリー全体を走査し、children が空の Module シンボルを定義ジャンプで
/// 解決して再帰展開する（ADR-0049）。戻り値はシンボル総数上限で打ち切られたか。
///
/// 各モジュール解決は: モジュール名トークン（selection_range 先頭）の LSP 座標
/// を求め、`textDocument/definition` で定義ファイルの URI を解決 → 同一ファイル
/// なら inline と判断してスキップ、別ファイルならそのファイルの outline を取得
/// して children に埋める（visited で循環防止、depth で深さ制限）。
///
/// ponytail: 再帰は深さ最大（既定 3）で、ボトルネックは LSP のセマンティック
/// 要求（キャッシュが効くのは同一ファイルのみ）。逐次再帰のまま — 並列化は
/// モジュール数の実測が要請してから。
fn expand_outline_modules<'a>(
    daemon: &'a Mutex<Daemon>,
    session: &'a std::sync::Arc<tokio::sync::Mutex<lsp::LspSession>>,
    owner_path: &'a std::path::Path,
    owner_text: &'a str,
    symbols: &'a mut Vec<mina_protocol::OutlineSymbol>,
    depth: u32,
    visited: &'a mut std::collections::HashSet<std::path::PathBuf>,
    budget: &'a mut usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
    Box::pin(async move {
        let mut truncated = false;
        for sym in symbols.iter_mut() {
            // children が空の Module だけがファイル分割の候補（既に children が
            // ある = inline mod { } の中身は LSP が既に展開済み）。
            if sym.kind == mina_protocol::SymbolKind::Module
                && sym.children.is_empty()
                && depth > 0
                && *budget > 0
            {
                // モジュール名トークン（selection_range 先頭）の LSP 座標。
                let char_idx = sym.selection_range.anchor;
                let (line, character) = {
                    let Ok(s) =
                        tokio::time::timeout(lsp::LSP_LOCK_TIMEOUT, session.lock()).await
                    else {
                        break;
                    };
                    s.char_to_lsp_pos(owner_text, char_idx)
                };
                // 定義ジャンプで定義ファイルを解決。解決不能（inline 含む）は
                // children 空のままスキップ（失敗ではない）。
                let Some(target_uri) =
                    lsp::definition_target_uri(session, owner_path, line, character).await
                else {
                    continue;
                };
                let Ok(target) = path_from_uri(&target_uri) else {
                    continue;
                };
                let target_norm = normalize_open_path(target).await;
                if target_norm == owner_path || !visited.insert(target_norm.clone()) {
                    continue; // 同一ファイル（inline mod { }）または訪問済み
                }
                // 定義ファイルの outline を取得（キャッシュ or LSP）。
                let target_path_str = target_norm.to_string_lossy().into_owned();
                // LSP 非対応の解決先（異なる言語等）はスキップ。
                let Ok(target_session) = prepare_borrowed_session(daemon, &target_path_str).await
                else {
                    continue;
                };
            if !target_session.session.lock().await.caps.document_symbols {
                restore_focus_after_semantic(
                    daemon,
                    &target_session.session,
                    &target_session.focused,
                    &target_session.path,
                )
                .await;
                continue;
            }
            let target_borrows = daemon
                .lock()
                .await
                .borrows_focus_session(&target_session.focused, &target_session.path);
            let target_fetched = lsp::document_symbols_at(
                &target_session.session,
                &target_session.path,
                &target_session.text,
            )
            .await;
            if target_borrows {
                restore_focus_session(daemon, &target_session.session, &target_session.focused)
                    .await;
            }
            let mut child_symbols = match target_fetched {
                Ok(s) => {
                    daemon.lock().await.cache_outline(
                        target_session.path.clone(),
                        &target_session.text,
                        s.clone(),
                    );
                    s
                }
                Err(_) => Vec::new(),
            };
            // 予算: 追加したシンボル数を消費し、超過したら打ち切り。
            let used = child_symbols.len() + 1; // +1 = モジュール自身
            if *budget < used {
                truncated = true;
                child_symbols.clear();
            } else {
                *budget -= used;
            }
            if !child_symbols.is_empty() {
                // さらに深いモジュールへ再帰。
                truncated |= expand_outline_modules(
                    daemon,
                    &target_session.session,
                    &target_session.path,
                    &target_session.text,
                    &mut child_symbols,
                    depth - 1,
                    visited,
                    budget,
                )
                .await;
                sym.children = child_symbols;
            }
        }
        // インラインの children（既に LSP が展開済みの入れ子）も再帰走査する。
        truncated |= expand_outline_modules(
            daemon,
            session,
            owner_path,
            owner_text,
            &mut sym.children,
            depth,
            visited,
            budget,
        )
        .await;
    }
    truncated
    })
}

/// [`Command::ReadPath`] の処理（ADR-0048）: パス指定の軽量テキスト読み。
/// バッファ非依存・LSP 非依存 — フォーカス・idle view・世代・push・イベントを
/// 一切動かさず、任意パスのテキストだけを返す（開文書優先・未保存編集込み・
/// ディスク fallback）。全文スナップショットを運ばない軽量応答。
///
/// 読み込み不可（存在しない・非正規ファイル等）は `error: Some(…)`。
async fn serve_read_path(daemon: &Mutex<Daemon>, path: &str) -> ServerMessage {
    let path_buf = normalize_open_path(PathBuf::from(path)).await;
    let path_str = path_buf.to_string_lossy().into_owned();
    let text = match resolve_doc_text(daemon, &path_buf).await {
        Some(t) => t,
        None => {
            return ServerMessage::ReadPath {
                path: path_str,
                generation: 0,
                text: String::new(),
                error: Some(format!("cannot open {path}")),
            }
        }
    };
    let msg = ServerMessage::ReadPath {
        path: path_str,
        generation: daemon.lock().await.generation,
        text,
        error: None,
    };
    // read 専用メトリクス: 全文スナップショット（get_state）との対比で
    // 「全文を読まなくて済んだ量」を観測する（issue #27 と同じ目的）。
    let mut d = daemon.lock().await;
    let bytes = serde_json::to_vec(&msg).map(|v| v.len() as u64).unwrap_or(0);
    d.metrics.read_total += 1;
    d.metrics.read_bytes += bytes;
    msg
}

/// [`Command::EnclosingSymbol`] の処理（ADR-0031）: 指定位置（1-origin 行:列）
/// を囲む記号の名前・種別・正確な範囲（選択範囲含む）を全文なしの軽量応答で
/// 返す。読み取り専用 — 世代・push・イベントは進めない。
///
/// documentSymbol の同じツリーから「その char 位置を含む最も深い記号」を引く
/// （範囲はエージェントが全文を読まずに編集対象を特定する住所になる）。
/// 位置がどの記号にも含まれない場合は `found: false`（`error: None`）で返す。
async fn serve_enclosing_symbol(
    daemon: &Mutex<Daemon>,
    path: &str,
    line: u32,
    col: u32,
) -> ServerMessage {
    let err = |msg: String| ServerMessage::EnclosingSymbol {
        path: path.to_string(),
        name: String::new(),
        kind: SymbolKind::Other,
        range: Range { anchor: 0, head: 0 },
        selection_range: Range { anchor: 0, head: 0 },
        found: false,
        error: Some(msg),
    };
    // キャッシュ高速経路: outline キャッシュ（テキスト一致）があれば LSP に触れず
    // 位置解決だけで済ませる。実測: LSP 要求は再解析込みで数秒 — エージェントが
    // 同一ファイルへ at を連打する場合の大半をこの経路が吸う。
    let path_buf = normalize_open_path(PathBuf::from(path)).await;
    if let Some(symbols) = cached_outline(daemon, &path_buf).await {
        let Some(text) = resolve_doc_text(daemon, &path_buf).await else {
            return err(format!("cannot open {path}"));
        };
        let char_idx = lsp::line_col_to_char_idx(&text, line, col);
        let found = lsp::enclosing_symbol(&symbols, char_idx).cloned();
        let msg = match found {
            Some(sym) => ServerMessage::EnclosingSymbol {
                path: path_buf.to_string_lossy().into_owned(),
                name: sym.name,
                kind: sym.kind,
                range: sym.range,
                selection_range: sym.selection_range,
                found: true,
                error: None,
            },
            None => ServerMessage::EnclosingSymbol {
                path: path_buf.to_string_lossy().into_owned(),
                name: String::new(),
                kind: SymbolKind::Other,
                range: Range { anchor: 0, head: 0 },
                selection_range: Range { anchor: 0, head: 0 },
                found: false,
                error: None,
            },
        };
        return record_metric(daemon, msg, false).await;
    }
    let borrowed = match prepare_borrowed_session(daemon, path).await {
        Ok(b) => b,
        Err(BorrowFail::CannotOpen) => return err(format!("cannot open {path}")),
        Err(BorrowFail::NoServer(p)) => {
            return err(format!(
                "symbol range not supported for {p} (no LSP server configured)"
            ))
        }
        Err(BorrowFail::SpawnFailed(e)) => return err(format!("LSP error: {e}")),
    };
    if !borrowed.session.lock().await.caps.document_symbols {
        restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path)
            .await;
        return err(format!(
            "symbol range not supported for {} (LSP server が documentSymbolProvider を advertise していません)",
            borrowed.path_str
        ));
    }
    let borrows = daemon
        .lock()
        .await
        .borrows_focus_session(&borrowed.focused, &borrowed.path);
    let fetched = lsp::document_symbols_at(&borrowed.session, &borrowed.path, &borrowed.text).await;
    // (line:col → char idx) → 囲む記号（最も深い）をツリーから引く。
    // 位置解決に LSP を介さず、取得済みツリーの範囲だけで決まる。
    let enclosing = fetched.as_ref().ok().and_then(|symbols| {
        let char_idx = lsp::line_col_to_char_idx(&borrowed.text, line, col);
        lsp::enclosing_symbol(symbols, char_idx).cloned()
    });
    if let Ok(symbols) = &fetched {
        daemon
            .lock()
            .await
            .cache_outline(borrowed.path.clone(), &borrowed.text, symbols.clone());
    }
    if borrows {
        // 復元（Q10-(c)）: フォーカス文書へ戻し、更新停止を自己修復する
        restore_focus_session(daemon, &borrowed.session, &borrowed.focused).await;
    }
    let msg = match (enclosing, fetched) {
        (Some(sym), _) => ServerMessage::EnclosingSymbol {
            path: borrowed.path_str,
            name: sym.name,
            kind: sym.kind,
            range: sym.range,
            selection_range: sym.selection_range,
            found: true,
            error: None,
        },
        (None, Ok(_)) => ServerMessage::EnclosingSymbol {
            path: borrowed.path_str,
            name: String::new(),
            kind: SymbolKind::Other,
            range: Range { anchor: 0, head: 0 },
            selection_range: Range { anchor: 0, head: 0 },
            found: false,
            error: None,
        },
        (None, Err(e)) => ServerMessage::EnclosingSymbol {
            path: borrowed.path_str,
            name: String::new(),
            kind: SymbolKind::Other,
            range: Range { anchor: 0, head: 0 },
            selection_range: Range { anchor: 0, head: 0 },
            found: false,
            error: Some(e),
        },
    };
    record_metric(daemon, msg, false).await
}

// ---- hover・ワークスペースシンボル検索・診断 check（ADR-0032） ----

/// [`Command::HoverAt`] の処理（ADR-0032）: 任意パスの指定位置（1-origin 行:列）
/// の hover 情報（型・シグネチャ・doc）を全文なしの軽量応答
/// （[`ServerMessage::Hover`]）で返す。読み取り専用 — 世代・push・イベントは
/// 進めない。
///
/// お膳立て・復元は PeekDefinitionAt と同じ `prepare_borrowed_session` 経路
/// （Q10-(c)）。hover なし（空白・コメント位置）は `text` 空の成功応答、
/// 入力不正・LSP 非対応・spawn 失敗は `error: Some(…)`（exit 1、再試行不可）。
async fn serve_hover_at(daemon: &Mutex<Daemon>, path: &str, line: u32, col: u32) -> ServerMessage {
    let err = |msg: String| ServerMessage::Hover {
        path: path.to_string(),
        generation: 0,
        text: String::new(),
        error: Some(msg),
    };
    let borrowed = match prepare_borrowed_session(daemon, path).await {
        Ok(b) => {
                    b
        }
        Err(e) => {
                    match e {
                BorrowFail::CannotOpen => return err(format!("cannot open {path}")),
                BorrowFail::NoServer(p) => {
                    return err(format!("hover not supported for {p} (no LSP server configured)"))
                }
                BorrowFail::SpawnFailed(e) => return err(format!("LSP error: {e}")),
            }
        }
    };
    // サーバが hover を提供していなければ即「not supported」（exit 1）。
    // prepare が対象を didOpen 済みのため、借用していたらフォーカス文書へ戻す
    // （戻さないと current_uri が対象のまま — 同期スキップ・診断消失）。
    if !borrowed.session.lock().await.caps.hover {
        restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path)
            .await;
        return err(format!(
            "hover not supported for {} (LSP server が hoverProvider を advertise していません)",
            borrowed.path_str
        ));
    }
    let borrows = daemon
        .lock()
        .await
        .borrows_focus_session(&borrowed.focused, &borrowed.path);
    let hovered =
        lsp::hover_at_line_col(&borrowed.session, &borrowed.path, &borrowed.text, line, col).await;
    if borrows {
        // 復元（Q10-(c)）: フォーカス文書へ戻し、更新停止を自己修復する
        restore_focus_session(daemon, &borrowed.session, &borrowed.focused).await;
    }
    // メッセージを先に組み立ててから計測する。引数式内で daemon.lock() を作ると
    // 一時ガードがステートメント末（record_agent_metric の .await の後）まで生き、
    // 内部の再ロックが自分自身に待たされるデッドロックになる（実測でハング）。
    let msg = ServerMessage::Hover {
        path: borrowed.path_str,
        generation: daemon.lock().await.generation,
        text: hovered.unwrap_or_default(),
        error: None,
    };
    record_agent_metric(daemon, msg).await
}

/// [`Command::WorkspaceSymbol`] の処理（ADR-0032）: `path` で決まるワークスペース
/// root の LSP セッションへ `workspace/symbol`（`query`）を投げ、ヒットした
/// シンボル（名前・種別・パス・1-origin 行番号のみ）を全文なしの軽量応答
/// （[`ServerMessage::WorkspaceSymbols`]）で返す。読み取り専用 — 世代・push・
/// イベントは進めない。
///
/// 「どこで定義されているか」の探索を rg の代わりに 1 往復で済ませる経路。
/// 失敗は `error: Some(…)`: 入力不正・LSP 非対応・spawn 失敗は exit 1（再試行
/// 不可）、LSP エラーは再試行可能（exit 2）。
async fn serve_workspace_symbols(
    daemon: &Mutex<Daemon>,
    path: &str,
    query: &str,
) -> ServerMessage {
    let err = |msg: String| ServerMessage::WorkspaceSymbols {
        generation: 0,
        symbols: Vec::new(),
        error: Some(msg),
    };
    if query.is_empty() {
        return err("invalid input: query must be non-empty".into());
    }
    let borrowed = match prepare_borrowed_session(daemon, path).await {
        Ok(b) => b,
        Err(BorrowFail::CannotOpen) => return err(format!("cannot open {path}")),
        Err(BorrowFail::NoServer(p)) => {
            return err(format!(
                "symbol search not supported for {p} (no LSP server configured)"
            ))
        }
        Err(BorrowFail::SpawnFailed(e)) => return err(format!("LSP error: {e}")),
    };
    // サーバが workspace/symbol を提供していなければ即「not supported」（exit 1）。
    if !borrowed.session.lock().await.caps.workspace_symbols {
        restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path)
            .await;
        return err(format!(
            "symbol search not supported for {} (LSP server が workspaceSymbolProvider を advertise していません)",
            borrowed.path_str
        ));
    }
    let borrows = daemon
        .lock()
        .await
        .borrows_focus_session(&borrowed.focused, &borrowed.path);
    let fetched = lsp::workspace_symbols(&borrowed.session, query).await;
    if borrows {
        // 復元（Q10-(c)）: フォーカス文書へ戻し、更新停止を自己修復する
        restore_focus_session(daemon, &borrowed.session, &borrowed.focused).await;
    }
    let (symbols, error) = match fetched {
        Ok(items) => (
            items
                .into_iter()
                .map(|(uri, kind, name, line)| WorkspaceSymbol {
                    name,
                    kind,
                    path: uri.strip_prefix("file://").unwrap_or(&uri).to_string(),
                    line: line + 1, // 0-origin → 1-origin
                })
                .collect(),
            None,
        ),
        Err(e) => (Vec::new(), Some(e)),
    };
    // メッセージを先に組み立ててから計測（一時ガードのデッドロック回避 — hover と同じ）
    let msg = ServerMessage::WorkspaceSymbols {
        generation: daemon.lock().await.generation,
        symbols,
        error,
    };
    record_agent_metric(daemon, msg).await
}

/// [`Command::CheckDiagnostics`] の処理（ADR-0032）: 対象パスの診断が安定するまで
/// 待ち、診断だけをコンパクトに（全文なしで）返す。読み取り専用 — 世代・push・
/// イベントは進めない。
///
/// エージェントの「編集→検証」ループを 1 コマンドに圧縮する経路: 従来の
/// 「wait → get → JSON から診断を読む」の往復と全文スナップショットをこれ 1 つで
/// 置き換える。安定判定は settle_open_diagnostics と同じ（非空が 2 回連続で同数 /
/// 連続 2 回の空 — 空は「クリーン**未確認**」で `settled: false`。ADR-0045/0052）。
/// 失敗は `error: Some(…)`（LSP 非対応・
/// spawn 失敗は exit 1、LSP エラーは再試行可能な exit 2）。
async fn serve_check_diagnostics(daemon: &Mutex<Daemon>, path: &str) -> ServerMessage {
    // ADR-0056: pull の待ち内訳（borrow = didOpen / pull = RA 解析待ち +
    // settle / restore）。ギャップなし check の ~800ms がどのフェーズかを見る。
    let mut trace = Trace::new("check");
    let err = |msg: String| ServerMessage::Check {
        path: path.to_string(),
        generation: 0,
        total: 0,
        diagnostics: Vec::new(),
        settled: false, // 失敗はクリーン確定の根拠なし（ADR-0045）
        error: Some(msg),
    };
    let borrowed = match prepare_borrowed_session(daemon, path).await {
        Ok(b) => b,
        Err(BorrowFail::CannotOpen) => return err(format!("cannot open {path}")),
        Err(BorrowFail::NoServer(p)) => {
            return err(format!("check not supported for {p} (no LSP server configured)"))
        }
        Err(BorrowFail::SpawnFailed(e)) => return err(format!("LSP error: {e}")),
    };
    trace.mark("borrow");
    // サーバが pull 診断を提供していなければ即「not supported」（exit 1）。
    if !borrowed.session.lock().await.caps.pull_diagnostics {
        restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path)
            .await;
        return err(format!(
            "check not supported for {} (LSP server が diagnosticProvider を advertise していません)",
            borrowed.path_str
        ));
    }
    let borrows = daemon
        .lock()
        .await
        .borrows_focus_session(&borrowed.focused, &borrowed.path);
    trace.mark("caps");
    let pulled =
        lsp::pull_diagnostics_settled(&borrowed.session, &borrowed.path, &borrowed.text).await;
    trace.mark("pull");
    if borrows {
        // 復元（Q10-(c)）: フォーカス文書へ戻し、更新停止を自己修復する
        restore_focus_session(daemon, &borrowed.session, &borrowed.focused).await;
        trace.mark("restore");
    }
    let (diags, settled) = match pulled {
        Ok(v) => v,
        // 失敗理由を潰さず返す: 回復手順が理由ごとに違う（ロック競合 = ただちに
        // 再試行、サーバ死亡 = 次の .rs Open で再 spawn、プロトコル = 再試行）。
        Err(fail) => return err(format!("LSP error: {}", fail.detail())),
    };
    trace.note("settled", settled);
    trace.note("diags", diags.len());
    // char インデックス → 1-origin 行番号・行内列（エージェントの住所）を付与
    let diagnostics = diags
        .iter()
        .map(|d| CheckDiagnostic {
            severity: d.severity,
            line: line_of_char(&borrowed.text, d.start) + 1,
            col: col_of_char(&borrowed.text, d.start),
            start: d.start,
            end: d.end,
            message: d.message.clone(),
        })
        .collect::<Vec<_>>();
    let total = diagnostics.len();
    let msg = ServerMessage::Check {
        path: borrowed.path_str,
        generation: daemon.lock().await.generation,
        total,
        diagnostics,
        settled, // ADR-0045: 空でも「クリーン確定」でなければ false のまま伝える
        error: None,
    };
    record_agent_metric(daemon, msg).await
}

/// 基準診断の后台充填（#52・a2）。借用セッションで pull し、キャッシュに
/// 載せて push で届ける（generation は進めない — ADR-0028）。基準は不変の
/// ため settle 1 回で確定する。LSP 非対応・読取不可は空で確定させて
/// 再試行の嵐を避け、spawn 失敗など一時的要因は未確定のまま残す。
async fn fill_base_diagnostics(
    daemon: &Mutex<Daemon>,
    push_tx: &watch::Sender<(Option<u64>, StateSnapshot)>,
    base_path: PathBuf,
) {
    let path_str = base_path.to_string_lossy().into_owned();
    let borrowed = match prepare_borrowed_session(daemon, &path_str).await {
        Ok(b) => b,
        Err(BorrowFail::SpawnFailed(_)) => {
            // 一時的要因: 充填中印だけ外して次回に委ねる。
            daemon.lock().await.base_diag_inflight.remove(&base_path);
            return;
        }
        Err(_) => {
            // 恒久的要因（非対応・読取不可）: 空で確定させる。
            daemon
                .lock()
                .await
                .store_base_diagnostics(base_path.clone(), Vec::new());
            let snap = snapshot(&mut *daemon.lock().await, None);
            let _ = push_tx.send((None, snap));
            return;
        }
    };
    let borrows = daemon
        .lock()
        .await
        .borrows_focus_session(&borrowed.focused, &borrowed.path);
    let pulled =
        lsp::pull_diagnostics_settled(&borrowed.session, &borrowed.path, &borrowed.text).await;
    if borrows {
        restore_focus_session(daemon, &borrowed.session, &borrowed.focused).await;
    }
    // 基準診断は「基準側のエラーを見せる」ことが目的のため、settled は使わず
    // 診断のみを取る（空 = クリーン扱いは ADR-0045 の対象外 — 基準側は表示用）。
    let diags = pulled.map(|(diags, _)| diags).unwrap_or_default();
    {
        let mut d = daemon.lock().await;
        // Unregister で先に消えていたら捨てる（古い基準の残骸を載せない）。
        if !d.base_diag_inflight.contains(&base_path) {
            return;
        }
        d.store_base_diagnostics(base_path, diags);
        let snap = snapshot(&mut d, None);
        drop(d);
        let _ = push_tx.send((None, snap));
    }
}

/// char インデックス → 0-origin 行番号（check 診断の行番号付与用）。
/// `char_idx` が文末を超えていたら最終行。
fn line_of_char(text: &str, char_idx: usize) -> u32 {
    let mut line = 0u32;
    for (i, ch) in text.chars().enumerate() {
        if i >= char_idx {
            break;
        }
        if ch == '\n' {
            line += 1;
        }
    }
    line
}

/// char インデックス → 1-origin 行内列（check 診断の col 付与用。ADR-0047）。
/// 行頭（直前の改行の直後）の char を 1 とする。`char_idx` が文末を超えて
/// いたら最終行の末尾列（クランプ）。
fn col_of_char(text: &str, char_idx: usize) -> u32 {
    let char_count = text.chars().count();
    let char_idx = char_idx.min(char_count); // 文末超過は最終行の末尾扱い
    let mut line_start = 0usize; // 行頭の char インデックス
    let mut i = 0usize;
    for ch in text.chars() {
        if i >= char_idx {
            break;
        }
        if ch == '\n' {
            line_start = i + 1; // 次の行の行頭
        }
        i += 1;
    }
    (char_idx.saturating_sub(line_start) + 1) as u32
}

/// ADR-0032 の計測: hover / symbol search / check の要求回数と応答シリアライズ
/// bytes を ServerMetrics に累積する（エラー応答も配線に乗るため計上する）。
async fn record_agent_metric(daemon: &Mutex<Daemon>, msg: ServerMessage) -> ServerMessage {
    let mut d = daemon.lock().await;
    let bytes = serde_json::to_vec(&msg).map(|v| v.len() as u64).unwrap_or(0);
    match &msg {
        ServerMessage::Hover { .. } => {
            d.metrics.hover_total += 1;
            d.metrics.hover_bytes += bytes;
        }
        ServerMessage::WorkspaceSymbols { .. } => {
            d.metrics.symbol_search_total += 1;
            d.metrics.symbol_search_bytes += bytes;
        }
        ServerMessage::Check { .. } => {
            d.metrics.check_total += 1;
            d.metrics.check_bytes += bytes;
        }
        _ => {}
    }
    msg
}

/// ADR-0031 の計測: Outline / EnclosingSymbol の要求回数と応答シリアライズ bytes
/// を ServerMetrics に累積する（エラー応答も配線に乗るため計上する）。
async fn record_metric(daemon: &Mutex<Daemon>, msg: ServerMessage, outline: bool) -> ServerMessage {
    let mut d = daemon.lock().await;
    let bytes = serde_json::to_vec(&msg).map(|v| v.len() as u64).unwrap_or(0);
    if outline {
        d.metrics.outline_total += 1;
        d.metrics.outline_bytes += bytes;
    } else {
        d.metrics.symbol_range_total += 1;
        d.metrics.symbol_range_bytes += bytes;
    }
    msg
}

/// [`Command::References`] の処理（ADR-0029）。`old` の最初の識別子出現を解決し、
/// `textDocument/references` で全参照位置（定義含む）を軽量応答で返す。
/// 読み取り専用 — テキスト・世代は変えない。
///
/// 失敗は `error: Some(…)` で表す: 入力不正・対象が読めない・LSP 非対応
/// （`rename not supported` と同型）・シンボル未解決（ロード中含む）・LSP エラー。
async fn serve_references(daemon: &Mutex<Daemon>, path: &str, old: &str) -> ServerMessage {
    if old.is_empty() {
        return err_refs(path, "invalid input: old must be non-empty".into());
    }
    let borrowed = match prepare_borrowed_session(daemon, path).await {
        Ok(b) => b,
        Err(BorrowFail::CannotOpen) => return err_refs(path, format!("cannot open {path}")),
        Err(BorrowFail::NoServer(p)) => {
            return err_refs(
                path,
                format!("references not supported for {p} (no LSP server configured)"),
            )
        }
        Err(BorrowFail::SpawnFailed(e)) => return err_refs(path, format!("LSP error: {e}")),
    };
    // サーバが references を提供していなければ即「not supported」（exit 1、再試行不可）。
    // 未 advertise のサーバに要求するとリトライ予算（約10秒）を無駄にする（Stage 3）。
    // prepare が対象を didOpen 済みのため、借用していたらフォーカス文書へ戻す
    // （戻さないと current_uri が対象のまま — 同期スキップ・診断消失。敵対的検証 P1）。
    if !borrowed.session.lock().await.caps.references {
        restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path)
            .await;
        return err_refs(
            path,
            format!(
                "references not supported for {} (LSP server が references を advertise していません)",
                borrowed.path_str
            ),
        );
    }
    // 中間エラー応答。エラー経路でも借用していたらフォーカス文書へ戻す — 戻さないと
    // current_uri が対象のまま残り、次回のフォーカス編集が sync スキップ・診断消失に
    // なる（敵対的検証 P2。err クロージャの各 return が自動的に restore する）。
    let err = |msg: String| {
        let borrowed_ref = &borrowed;
        async move {
            restore_focus_after_semantic(
                daemon,
                &borrowed_ref.session,
                &borrowed_ref.focused,
                &borrowed_ref.path,
            )
            .await;
            ServerMessage::ReferencesResult {
                path: borrowed_ref.path_str.clone(),
                locations: Vec::new(),
                total: 0,
                error: Some(msg),
            }
        }
    };
    // 識別子位置の解決（コメント・文字列内には解決しない — T3 の誤位置事故を予防）
    let (line, character) =
        match resolve_symbol_lsp_pos(daemon, &borrowed.session, &borrowed.path, &borrowed.text, old)
            .await
        {
            Ok(v) => v,
            Err(e) => return err(e).await,
        };
    let locations = match lsp::references_at(&borrowed.session, &borrowed.path, line, character).await
    {
        Ok(v) => v,
        Err(e) => return err(e).await,
    };
    // フォーカス文書を借りていたら復元（開き直し + 診断更新 — 自己修復）
    restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path).await;
    let mut out = Vec::with_capacity(locations.len());
    for (uri, line) in locations {
        let Ok(p) = path_from_uri(&uri) else {
            return err(format!(
                "LSP 応答に file:// 以外の URI が含まれています: {uri}"
            ))
            .await;
        };
        out.push(mina_protocol::ReferenceLocation {
            path: p.to_string_lossy().into_owned(),
            line,
        });
    }
    let n = out.len();
    ServerMessage::ReferencesResult {
        path: borrowed.path_str,
        locations: out,
        total: n,
        error: None,
    }
}

/// 失敗応答の共有形（references のエラー応答）。
fn err_refs(path: &str, msg: String) -> ServerMessage {
    ServerMessage::ReferencesResult {
        path: path.to_string(),
        locations: Vec::new(),
        total: 0,
        error: Some(msg),
    }
}

/// [`Command::Rename`] の処理（ADR-0029）。内容指定（`old` の最初の識別子出現）を
/// 位置に解決し、LSP rename の WorkspaceEdit を適用・保存して、影響範囲
/// （ファイル数・編集数・変更一覧）を軽量応答で返す。
///
/// 応答の意味論（作業順序）:
/// 1. 全ファイルの編集を検証（範囲・重複）してから適用を始める — 検証失敗は
///    ディスク無変更で拒否（apply の原子 bunch と同じ「Save 前失敗なら無変更」）。
/// 2. 開いている文書はメモリ（Document）へ適用し履歴に記録（undo は文書ごと
///    独立に保たれる — Q5）。開いていないファイルはディスク読み → 書換。
/// 3. 開いている文書も含め全変更をディスクへ書き、世代を進める。
async fn serve_rename(daemon: &Mutex<Daemon>, path: &str, old: &str, new: &str) -> ServerMessage {
    // ADR-0056: rename の内訳（prepare = didOpen / request = RA の WorkspaceEdit
    // 計算 / convert = char 変換 / apply = 書込み + 復元同期）。
    let mut trace = Trace::new("rename");
    // borrow 前の入力エラー（restore 不要）
    let err_input = |msg: String| ServerMessage::RenameResult {
        generation: 0,
        files: 0,
        edits: 0,
        changed: Vec::new(),
        error: Some(msg),
    };
    if old.is_empty() || new.is_empty() {
        return err_input("invalid input: old and new must be non-empty".into());
    }
    // #49: 基準配下を起点とするリネームを拒否する（書込み先は同一セッション
    // ＝同一 root に閉じるため起点検査で十分）。正規化失敗は既存経路に任せる。
    if !daemon.lock().await.base_roots.is_empty() {
        if let Ok(canon) = tokio::fs::canonicalize(path).await {
            if let Some(msg) = daemon.lock().await.base_reject(&canon) {
                return err_input(msg);
            }
        }
    }
    let borrowed = match prepare_borrowed_session(daemon, path).await {
        Ok(b) => b,
        Err(BorrowFail::CannotOpen) => return err_input(format!("cannot open {path}")),
        Err(BorrowFail::NoServer(p)) => {
            return err_input(format!(
                "rename not supported for {p} (no LSP server configured)"
            ))
        }
        Err(BorrowFail::SpawnFailed(e)) => return err_input(format!("LSP error: {e}")),
    };
    trace.mark("prepare");
    // 中間エラー応答。エラー経路でも借用していたらフォーカス文書へ戻す — 戻さないと
    // current_uri が対象のまま残り、次回のフォーカス編集が sync スキップ・診断消失に
    // なる（敵対的検証 P2。err クロージャの各 return が自動的に restore する）。
    let err = |msg: String| {
        let borrowed_ref = &borrowed;
        async move {
            restore_focus_after_semantic(
                daemon,
                &borrowed_ref.session,
                &borrowed_ref.focused,
                &borrowed_ref.path,
            )
            .await;
            ServerMessage::RenameResult {
                generation: 0,
                files: 0,
                edits: 0,
                changed: Vec::new(),
                error: Some(msg),
            }
        }
    };
    // サーバが rename を提供していなければ即「not supported」（exit 1、再試行不可）。
    // 未 advertise のサーバに要求するとワークスペース走査 + リトライ予算を無駄にする
    // （Stage 3）。prepare が対象を didOpen 済みのため、借用していたらフォーカス文書へ
    // 戻す（戻さないと current_uri が対象のまま — 同期スキップ・診断消失。敵対的検証 P1）。
    if !borrowed.session.lock().await.caps.rename {
        restore_focus_after_semantic(daemon, &borrowed.session, &borrowed.focused, &borrowed.path)
            .await;
        // err クロージャは async（restore 済みのため同形を直接返す）
        return ServerMessage::RenameResult {
            generation: 0,
            files: 0,
            edits: 0,
            changed: Vec::new(),
            error: Some(format!(
                "rename not supported for {} (LSP server が rename を advertise していません)",
                borrowed.path_str
            )),
        };
    }
    // 識別子位置の解決（コメント・文字列内には解決しない）
    let (line, character) =
        match resolve_symbol_lsp_pos(daemon, &borrowed.session, &borrowed.path, &borrowed.text, old)
            .await
        {
            Ok(v) => v,
            Err(e) => return err(e).await,
        };
    trace.mark("resolve");
    let raw = match lsp::rename_at(&borrowed.session, &borrowed.path, line, character, new).await {
        Ok(v) => v,
        Err(e) => return err(e).await,
    };
    trace.mark("request");
    // 各ファイルの編集を char インデックスへ変換・検証（適用前に全失敗を検出 —
    // 検証失敗ならディスク無変更で拒否）
    let enc = borrowed.session.lock().await.encoding();
    let mut files: Vec<lsp::RenameFile> = Vec::new();
    for f in &raw {
        let target = match path_from_uri(&f.uri) {
            Ok(p) => p,
            Err(msg) => return err(msg).await,
        };
        let text = {
            let d = daemon.lock().await;
            d.editor
                .doc_id_for_path(&target)
                .map(|id| d.editor.document(id).text().to_string())
        };
        let (text, _status) = match text {
            Some(t) => (Some(t), None),
            None => read_open_target(target.to_string_lossy().as_ref()).await,
        };
        let Some(text) = text else {
            return err(format!("cannot open {} (rename target)", target.display())).await;
        };
        let edits = match lsp::lsp_edits_to_char(&text, enc, &f.edits) {
            Ok(v) => v,
            Err(msg) => return err(format!("{}: {msg}", target.display())).await,
        };
        if !edits.is_empty() {
            files.push(lsp::RenameFile {
                path: target,
                edits,
            });
        }
    }
    if files.is_empty() {
        return err(format!(
            "symbol not found: {old:?} in {} (rename produced no edits)",
            borrowed.path.display()
        ))
        .await;
    }
    trace.mark("convert");
    trace.note("files", files.len());
    trace.note("edits", files.iter().map(|f| f.edits.len()).sum::<usize>());
    match apply_and_save_rename(daemon, &borrowed.session, &borrowed.focused, &borrowed.path, &files)
        .await
    {
        Ok(generation) => {
            trace.mark("apply");
            ServerMessage::RenameResult {
                generation,
                files: files.len(),
                edits: files.iter().map(|f| f.edits.len()).sum(),
                changed: files
                    .iter()
                    .map(|f| f.path.to_string_lossy().into_owned())
                    .collect(),
                error: None,
            }
        }
        Err(msg) => err(msg).await,
    }
}

/// LSP 応答の `file://` URI をパスへ変換する。
/// ponytail: percent-decode はしない（minae の URI 生成も encode しない）。
/// 空白等を含むパスは既存の既知制限（lsp.rs uri() と同じ）。
fn path_from_uri(uri: &str) -> Result<PathBuf, String> {
    uri.strip_prefix("file://")
        .map(PathBuf::from)
        .ok_or_else(|| format!("file:// 以外の URI です: {uri}"))
}

/// rename の WorkspaceEdit を適用（メモリ）・保存（ディスク）し、応答時の世代を返す。
///
/// - 開いている文書: `Transaction::replace_ranges` で履歴付き適用（Q5: リネーム
///   全体は undo 対象外だが、文書ごとの undo 履歴をテキストと整合させる）。
/// - 開いていないファイル: ディスクから読んだテキストに適用 → そのまま書き戻す。
/// - 全ファイルを検証済み（serve_rename 側）なので、ここでの失敗は I/O のみ。
/// - フォーカス文書の LSP 同期（restore / sync + pull）で診断を追従させる。
async fn apply_and_save_rename(
    daemon: &Mutex<Daemon>,
    session: &Mutex<lsp::LspSession>,
    focused: &Option<PathBuf>,
    target: &Path,
    files: &[lsp::RenameFile],
) -> Result<u64, String> {
    // フェーズ1（ロック内）: 開文書への適用・記録。未開ファイルのパスを収集。
    let mut to_write: Vec<(PathBuf, String)> = Vec::with_capacity(files.len());
    let mut unopened: Vec<&lsp::RenameFile> = Vec::new();
    let mut open_doc_ids: Vec<(PathBuf, mina_view::DocumentId)> = Vec::new();
    {
        let mut d = daemon.lock().await;
        for f in files {
            if let Some(doc_id) = d.editor.doc_id_for_path(&f.path) {
                // 開文書: 全編集を1トランザクションにまとめて適用（履歴付き）
                let edits: Vec<(usize, usize, String)> = f
                    .edits
                    .iter()
                    .map(|e| (e.start, e.end, e.text.clone()))
                    .collect();
                let old_doc = d.editor.document(doc_id).clone();
                let tx = mina_text::Transaction::replace_ranges(&old_doc, &edits);
                let selection_after = d.editor.selection(); // クランプは apply_document 側
                d.editor.apply_document(doc_id, tx, selection_after);
                d.record_event(
                    EventSource::Headless,
                    EventKind::ReplaceRange,
                    f.edits.first().map(|e| Range {
                        anchor: e.start,
                        head: e.end,
                    }),
                    f.edits.first().map(|e| e.text.clone()),
                );
                let new_text = d.editor.document(doc_id).text().to_string();
                to_write.push((f.path.clone(), new_text));
                open_doc_ids.push((f.path.clone(), doc_id));
            } else {
                unopened.push(f);
            }
        }
    }
    // フェーズ1b（ロック外）: 未開ファイルを読み、編集を適用して書戻し対象に加える
    for f in unopened {
        let path_str = f.path.to_string_lossy().into_owned();
        let (text, _status) = read_open_target(&path_str).await;
        let Some(text) = text else {
            return Err(format!("cannot read {} for rename", f.path.display()));
        };
        let new_text = lsp::apply_char_edits(&text, &f.edits);
        to_write.push((f.path.clone(), new_text));
    }
    // フェーズ2（ロック外）: ディスク書き込み（全検証済み。失敗は I/O のみ）
    for (path, text) in &to_write {
        if let Err(e) = tokio::fs::write(path, text.as_bytes()).await {
            return Err(format!("save failed: {}: {e}", path.display()));
        }
    }
    // フェーズ3（ロック内）: dirty クリア・ベースライン更新・Save 記録・世代確定
    let mut d = daemon.lock().await;
    for (path, doc_id) in &open_doc_ids {
        let written = to_write
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        d.editor.mark_saved_doc(*doc_id, &written);
        if let Ok(md) = std::fs::metadata(path) {
            d.baselines.insert(
                path.clone(),
                DiskBaseline {
                    mtime: md.modified().unwrap_or(std::time::UNIX_EPOCH),
                    size: md.len(),
                },
            );
        }
        d.record_event(EventSource::Headless, EventKind::Save, None, None);
    }
    let generation = d.generation;
    drop(d);
    // フェーズ4（ロック外）: フォーカス文書へセッションを戻し、診断を追従させる
    // （borrows の場合と target==focus の場合の両方を扱う — open_workspace_files
    // が current_uri を動かすため、対象がフォーカス文書でも開き直しが必要）。
    restore_focus_after_semantic(daemon, session, focused, target).await;
    Ok(generation)
}

/// セマンティック要求（rename / references）でセッションのフォーカス文書を動かした
/// 場合の復元。
///
/// 対象が (a) 同一 root の別ファイル（borrows）または (b) フォーカス文書そのもの、
/// のどちらでも、フォーカス文書を現在テキストで didOpen し直し診断を再 pull する。
/// `open_workspace_files` が current_uri を動かすため、(b) でも復元が必要（
/// serve_peek_definition_at の restore と同型。Q10-(c)）。別 root の対象なら
/// フォーカスのセッションには触れていないのでスキップ（自己修復に任せる）。
async fn restore_focus_after_semantic(
    daemon: &Mutex<Daemon>,
    session: &Mutex<lsp::LspSession>,
    focused: &Option<PathBuf>,
    target: &Path,
) {
    let borrows = daemon.lock().await.borrows_focus_session(focused, target);
    let target_is_focus = focused.as_deref() == Some(target);
    if !(borrows || target_is_focus) {
        return; // 別 root: フォーカスのセッションには触れていない
    }
    // 本体は restore_focus_session と共通（対象がフォーカス文書そのものの場合も
    // open_workspace_files が current_uri を動かしたため開き直しが必要。
    // serve_peek_definition_at の restore と同型。Q10-(c)）。
    restore_focus_session(daemon, session, focused).await;
}

/// LSP セッションを借りた場合の復元（Q10-(c)）: フォーカス文書がこの間に
/// 移動していなければ、現在テキストで didOpen し直し + 診断を再 pull して
/// 更新停止を自己修復する。serve_inlay_hints と serve_peek_definition_at で共用。
async fn restore_focus_session(
    daemon: &Mutex<Daemon>,
    session: &Mutex<lsp::LspSession>,
    focused: &Option<PathBuf>,
) {
    // v12: 復元対象はセッションの共有解析フォーカス文書（借りたセッションの
    // 元フォーカス）。要求中に他クライアントの Open がフォーカスを動かして
    // いたら復元しない（最後の操作が勝つ）。
    let restorable = {
        let d = daemon.lock().await;
        focused.as_ref().is_some_and(|fp| d.is_analysis_focus(fp))
    };
    if !restorable {
        return;
    }
    let focused_text = {
        let d = daemon.lock().await;
        focused.as_ref().and_then(|fp| {
            d.editor
                .doc_id_for_path(fp)
                .map(|id| d.editor.document(id).text().to_string())
        })
    };
    if let (Some(text), Some(fp)) = (focused_text, focused) {
        let diags = lsp::restore_focus_with_diagnostics(session, fp, &text).await;
        let mut d = daemon.lock().await;
        if let Some(diags) = diags {
            d.set_focus_diagnostics(fp, diags);
        }
        drain_into(&mut d);
    }
}

// ---- daemon 統合（lsp.rs から移設: 下記4関数は Daemon 状態を読み書くため、
// プロトコル層 lsp.rs は Daemon を知らなくてよいのが正しいモジュール境界。
// lsp.rs は LspSession と純 LSP プロトコル関数のみを残す） ----

/// 必要なら LSP セッションを spawn + initialize する（初回 .rs オープン時）。
///
/// セッションは WorkspaceRoot 毎に持つ（ADR-0010）: 異なるプロジェクトの
/// ファイルを開いても、それぞれの root で spawn されたサーバに解析させる。
/// M1: spawn + initialize（最大10秒）は daemon ロック外で行うため、この関数は
/// `&Mutex<Daemon>` を受け取り、daemon ロックは短時間だけ掴む。
/// M3: 既存セッションが死んでいたら新しいセッションで置き換える。
async fn ensure(
    daemon: &Mutex<Daemon>,
    path: &Path,
) -> Result<Arc<Mutex<LspSession>>, String> {
    // ゲートと同期させるため、まず最新テーブルを取得（mtime 差分のみ再読込）し、
    // root 判定（言語別マーカー。ADR-0030 Stage 2）と spawn の両方に使う。
    let languages = daemon.lock().await.languages_refresh();
    let spec = languages
        .server_for(path)
        .ok_or_else(|| format!("LSP 非対応のパスです: {}", path.display()))?;
    let root = languages.workspace_root(path);
    // セッションキーは (WorkspaceRoot, languageId)（ADR-0030 Stage 4）:
    // 同一 root に複数言語が混在しても言語ごとにセッションを分ける。
    let key = (root.clone(), spec.language_id.to_string());
    // 既存セッション（キーに生きていれば）を再利用する
    let reused = {
        let d = daemon.lock().await;
        d.lsp_sessions.get(&key).and_then(|session| {
            let reuse = match session.try_lock() {
                Ok(s) => !s.client.is_dead(),
                Err(_) => true, // 同期中: 生きているとみなして再利用
            };
            reuse.then(|| session.clone())
        })
    };
    if let Some(session) = reused {
        await_indexed(&session).await; // daemon ロック外で待つ
        return Ok(session);
    }
    // 未作成 or 死亡: 上の最新テーブルで spawn + initialize（M1 / ADR-0030）。
    // languages_refresh は mtime 差分だけ再読込するため、ゲート（先に呼ばれた
    // languages_refresh）と同じテーブルを参照する — 「新言語の追加」も次の
    // ゲート/ spawn で反映される（起動時 1 回読込では daemon 再起動まで効かない）。
    // ponytail: MINA_LSP_COMMAND はテスト用シーム（daemon 統合テストが mock
    // サーバを指す）。本番では languages.toml のテーブルを使う。
    let command = std::env::var("MINA_LSP_COMMAND").unwrap_or_else(|_| spec.command.to_string());
    let session = lsp::LspSession::new_with_config(
        &command,
        &root,
        spec.args,
        spec.language_id,
        spec.config.cloned(),
    )
    .await?;
    let arc = Arc::new(Mutex::new(session));
    // 保存（短いロック・await なし）。テーブルは上の languages_refresh が更新済み。
    let mut d = daemon.lock().await;
    // 同時 ensure レース: 既存が生きていればそちらを優先する
    if let Some(existing) = d.lsp_sessions.get(&key) {
        let alive = match existing.try_lock() {
            Ok(s) => !s.client.is_dead(),
            Err(_) => true, // 同期中: 生きているとみなす
        };
        if alive {
            // ponytail: 初期化中のセッションは map に未登録のため、Open 直後に
            // 意味解析コマンドを重ねると両者が spawn する（自分の分は破棄され
            // プロセスは orphan）。起動の遅い LSP で目立つなら ensure を in-flight
            // map で合流させる（今はまだ実測で問題になっていない）。
            return Ok(existing.clone());
        }
    }
    // M3/ADR-0009: 既存が死亡済みなら新セッションで置き換える
    // （修正前は無条件に既存を返し、再 spawn が毎回破棄されていた）
    d.lsp_sessions.insert(key, arc.clone());
    drop(d);
    // 索引完走を待ってから返す（ADR-0051）。新しいセッションは上のロックで先に
    // map へ登録済み — 待つ間の並行 ensure が 2 匹目のサーバを spawn しない。
    await_indexed(&arc).await;
    Ok(arc)
}

/// 索引依存の LSP 要求を出す前に、サーバの索引完走（`$/progress` が静まるまで）
/// を待つ（ADR-0051）。
///
/// LSP セッションロックは握ったまま待たない — 待ちの間 didChange を締め出すと、
/// 編集の同期がスキップされ（`LSP_LOCK_TIMEOUT` で諦める設計）、以後の解析が
/// 古いテキストのままになる。
///
/// ponytail: 待ちはセッションごとに 1 回だけ（`Progress` が確認済みを記録する）。
/// 2 回目以降の索引更新（Cargo.toml 変更などでの再 prime）は待たない — 実測で
/// 問題になってから、進捗の種類で絞る（今は「セッション生成直後の 1 回」で足りる）。
async fn await_indexed(session: &Mutex<LspSession>) {
    let progress = session.lock().await.progress();
    // 上限まで待って未確認でも進む（待ち続けて要求を止めない）。進捗を送らない
    // サーバ（work-done progress 非対応）は arm で解決する。
    progress.wait_ready(lsp::LSP_READY).await;
}

/// セマンティック要求（rename / references）の前に、ワークスペース内の同拡張子
/// ファイルを didOpen する。
///
/// 実測（M0/M1）: rust-analyzer は didOpen していないファイルの参照を
/// `textDocument/references` / `rename` の結果に含めない（開いていない
/// main.rs の使用箇所が rename で取りこぼされた）。AB ハーネス（tools/ab）も
/// 全ファイル didOpen を採用していた。対象拡張子のみ・生成ディレクトリ
/// （target/.git 等）除外・件数と合計バイトの上限で防護する。
async fn open_workspace_files(
    daemon: &Mutex<Daemon>,
    session: &Mutex<LspSession>,
    path: &Path,
) {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return;
    };
    let root = daemon.lock().await.languages.workspace_root(path);
    let mut files = Vec::new();
    collect_workspace_files(&root, ext, &mut files, 0);
    // ディスクから読む（上限付き）。開文書の未保存編集は後で優先する。
    let mut to_open: Vec<(PathBuf, String)> = Vec::new();
    for f in files {
        if let Some(text) = read_open_capped(&f).await {
            to_open.push((f, text));
        }
    }
    // 開文書の未保存編集を優先（ディスクとズレた didOpen で解析を汚さない）
    {
        let d = daemon.lock().await;
        for (path, text) in &mut to_open {
            if let Some(id) = d.editor.doc_id_for_path(path) {
                *text = d.editor.document(id).text().to_string();
            }
        }
    }
    let Ok(mut session) = timeout(lsp::LSP_LOCK_TIMEOUT, session.lock()).await else {
        return;
    };
    if session.client.is_dead() {
        return;
    }
    // 開いたファイルは閉じない（開いたまま保持）: 1セッション = 1開文書の設計では
    // 直後の rename/references 要求時に他ファイルが didClose され、tsserver 等が
    // 閉じたファイルの参照/編集を返さなくなる（probe 実測。ADR-0029 の
    // 「開いていないファイルの参照を取りこぼさない」と同じ狙いの拡張）。
    for (path, text) in to_open {
        session.did_open_keep(&path, &text).await;
    }
}

/// ワークスペース走査の上限（防護。実プロジェクトのソースは数十ファイルだが、
/// 生成物を紛れ込ませないため件数・合計バイトで切る）。
const MAX_WORKSPACE_OPEN_FILES: usize = 400;
const MAX_WORKSPACE_OPEN_BYTES: u64 = 64 * 1024 * 1024;

/// `root` 以下を再帰走査し、`ext` と同じ拡張子のファイルを収集する。
/// 生成ディレクトリ（target/.git/node_modules 等）と上限を超えた分は無視。
fn collect_workspace_files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>, mut bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_WORKSPACE_OPEN_FILES || bytes >= MAX_WORKSPACE_OPEN_BYTES {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // 生成物・メタデータのディレクトリは再帰しない
            if matches!(name, "target" | ".git" | "node_modules" | "vendor" | "build" | "dist")
            {
                continue;
            }
            collect_workspace_files(&path, ext, out, bytes);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        if let Ok(md) = std::fs::metadata(&path) {
            if md.is_file() && md.len() <= MAX_WORKSPACE_OPEN_BYTES {
                bytes += md.len();
                out.push(path);
            }
        }
    }
}

/// サイズ上限付きのディスク読み（ディレクトリ・過大は None）。
async fn read_open_capped(path: &Path) -> Option<String> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let meta = file.metadata().await.ok()?;
    if !meta.is_file() || meta.len() > MAX_WORKSPACE_OPEN_BYTES {
        return None;
    }
    let mut s = String::new();
    file.take(MAX_WORKSPACE_OPEN_BYTES + 1)
        .read_to_string(&mut s)
        .await
        .ok()?;
    if s.len() as u64 > MAX_WORKSPACE_OPEN_BYTES {
        return None;
    }
    Some(s)
}

/// 診断の実体は pull（[lsp::pull_after_edit] / [`settle_open_diagnostics`]）で更新する。
/// ここでは MEDIUM-3（サーバ死亡時のクリア）だけを行う。push（publishDiagnostics）
/// は flycheck（cargo check・ディスク基準）由来で、編集内容と食い違う stale な
/// 診断を publish することがあり、pull の結果を上書きしないよう適用しない。
fn drain_into(daemon: &mut Daemon) {
    // MEDIUM-3: セッションが死んだ解析フォーカス文書の診断・ヒントを残さない
    // （下線・カウントが文書と不整合のまま表示され続ける。再 spawn は次回
    // .rs Open 時 — ADR-0009）。フォーカス移動分は `set_analysis_focus` が
    // 同セッションの旧診断を捨て、他セッション分は `is_analysis_focus` で
    // 表示されない（ここでは掃除しない）。
    let dead: Vec<PathBuf> = daemon
        .diagnostics
        .keys()
        .filter(|p| {
            daemon
                .session_for(p)
                .map(|s| s.try_lock().ok().is_some_and(|g| g.client.is_dead()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for p in dead {
        daemon.diagnostics.remove(&p);
    }
    let dead_hints: Vec<PathBuf> = daemon
        .hints
        .keys()
        .filter(|p| {
            daemon
                .session_for(p)
                .map(|s| s.try_lock().ok().is_some_and(|g| g.client.is_dead()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for p in dead_hints {
        daemon.hints.remove(&p);
        daemon.hint_order.retain(|k| k != &p);
    }
}

/// Open 直後の診断追跡タスク: 初期解析が完了するまで pull を繰り返し、
/// 診断と inlay hint を daemon に反映する。更新のたびに購読者（TUI）へ
/// push する — ヒント・診断は generation を進めないため、クライアント側は
/// 内容比較（`snapshot != state`）で再描画する（#24 のフィードバック）。
///
/// rust-analyzer の初期解析（crate ロード）は数秒かかり、その間の pull は空を
/// 返す。非空が 2 回連続で返ったら解析完了とみなして終了する（空の連続は
/// 「解析未完」と区別できないため安定判定しない。30 秒間空ならクリーン
/// ファイルとみなして停止）。フォーカスが別文書に移ったら中断する。
/// 上限 120 回 = 60 秒。
async fn settle_open_diagnostics(
    daemon: &Mutex<Daemon>,
    session: Arc<Mutex<LspSession>>,
    path: PathBuf,
    push_tx: watch::Sender<(Option<u64>, StateSnapshot)>,
) {
    settle_open_diagnostics_loop(daemon, session, path.clone(), push_tx).await;
    // ADR-0028: ループの全出口（安定・タイムアウト・フォーカス移動・サーバ死）で
    // 活動を除去する（追加は Open 側が spawn 前に行う。idempotent なので安全）。
    let mut d = daemon.lock().await;
    d.remove_activity(&path, ActivityKind::DiagnosticsSettle);
}

async fn settle_open_diagnostics_loop(
    daemon: &Mutex<Daemon>,
    session: Arc<Mutex<LspSession>>,
    path: PathBuf,
    push_tx: watch::Sender<(Option<u64>, StateSnapshot)>,
) {
    let mut prev: Option<usize> = None;
    for i in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // 現在のテキストを掴んでから pull（解析フォーカス移動・編集の最中は中断）
        let text = {
            let d = daemon.lock().await;
            // v12: 中断条件は「この文書が解析フォーカスでなくなった」こと
            // （他接続のコマンドがフォーカス View を動かしても中断しない —
            // ADR-0037 の共有解析フォーカスは View ではなくセッション基準）。
            if !d.is_analysis_focus(&path) {
                return;
            }
            match d.editor.doc_id_for_path(&path) {
                Some(id) => d.editor.document(id).text().to_string(),
                None => return, // 文書が閉じられた: 中断
            }
        };
        let pulled = {
            let Ok(mut s) = timeout(lsp::LSP_LOCK_TIMEOUT, session.lock()).await else {
                continue;
            };
            if s.client.is_dead() {
                return;
            }
            // ヒントも診断と同じループで pull し、キャッシュに載せる（ADR-0020）。
            // 解析未完の間は空が返るが、次の反復で追いつく。
            (
                s.pull_diagnostics(&path, &text).await,
                s.pull_inlay_hints(&path, &text).await,
            )
        };
        let Some(diags) = pulled.0 else {
            continue; // 解析中のキャンセル等: 次回に持ち越し
        };
        let n = diags.len();
        let mut d = daemon.lock().await;
        // 書き込み前にフォーカスを再確認（pull 中に他コネクションの Open が
        // フォーカスを動かしたら、最新の文書の pull に任せて中断する）。
        if !d.is_analysis_focus(&path) {
            return;
        }
        d.set_focus_diagnostics(&path, diags);
        if let Some(hints) = pulled.1 {
            d.cache_hints(path.clone(), &text, hints);
        }
        // ヒント・診断の反映は generation を進めないが、見え方を変える。
        // 購読者へスナップショットを作り直して配る（クライアントは内容比較で
        // 再描画する。自分の応答と同じ内容なら捨てられる）。
        let snap = snapshot(&mut d, None);
        drop(d);
        // 発信元はコマンドでない（LSP settle）ため None を包み、全クライアントに届く。
        let _ = push_tx.send((None, snap));
        if n > 0 {
            // 非空が返った = 解析完了の確証。2回連続同じ件数なら安定とみなす
            // （誤検出: 解析未完の空（0,0,0...）を安定と誤認しないため、
            //  空の場合は安定判定しない）。
            if prev == Some(n) {
                return;
            }
        } else if i >= lsp::SEMANTIC_EMPTY_ROUNDS {
            // 空のまま数ラウンド: 解析済みでクリーン（または pull が見ない
            // エラー）とみなして停止。測定（ADR-0052）: pull は 1 回目で最終集合を
            // 返すので、以前の 30 秒（60 ラウンド × 2 pull = 120 往復）を待つ意味が
            // 無い。次の編集の pull で自己修復する。
            return;
        }
        prev = Some(n);
    }
}

/// M1/ADR-0009: 編集後の LSP 全文同期 + pull を1経路に集約する（コマンド編集・
/// DocumentEdit・外部リロード watch_disk の3箇所が同じ規律を個別に再現していた）。
///
/// ADR-0055（iteration #8）: pull（`pull_after_edit`）とその反映は Save 応答の
/// クリティカルパスから外し、背景タスク（`tokio::spawn`）で実行する。応答は現状の
/// スナップショット（= pull 前の診断・ヒント）を即返し、背景タスク完了時に push で
/// 全購読者へ編集後の診断・ヒントを配る。編集後追従の契約は「pull が起きること」
/// のみで「応答前に起きること」ではない（テストは編集後の追従を poll で検証する。
/// headless の apply 応答は診断を出力しないためエージェントへの影響は無い）。
///
/// ロック規律はこの関数だけが知る: 同期対象（セッション・パス・テキスト）の
/// 取り出しは呼び出し側のロック内で済ませておき、ここでは LSP の await を
/// ロック外で行い、結果を re-lock して反映してからスナップショットを返す。
/// 背景タスクは Open の背景 settle（`settle_open_diagnostics`）と同じ形 —
/// daemon ロックを await またぎで持たず、LSP の await はロック外（
/// `lsp::sync` / `pull_after_edit` はロック取得をタイムアウト付きで諦めるので
/// 次のコマンドを待たせない）。`target` が None でも drain_into（診断クリア・
/// サーバ死亡時の古い診断除去）は実行される。`activity` は同期の前後で公開する
/// Activity（ADR-0028。リロード時のみ使用）。追加・除去とも背景タスク側で行う
/// （応答に「同期中」が残らない）。
async fn sync_after_edit(
    daemon: &Arc<Mutex<Daemon>>,
    push_tx: watch::Sender<(Option<u64>, StateSnapshot)>,
    target: Option<(Arc<Mutex<LspSession>>, PathBuf, String)>,
    status: Option<String>,
    activity: Option<(ActivityKind, &'static str)>,
) -> StateSnapshot {
    // ADR-0056: sync = 応答側（drain + snapshot）、sync.bg = 背景の
    // didChange → 診断 pull → ヒント pull → 反映 → push。
    let trace = Trace::new("sync");
    if let Some((session, path, text)) = &target {
        // 背景タスク: LSP 同期 + pull + 反映 + push（Save 応答をブロックしない）
        let mut bg = Trace::new("sync.bg");
        let daemon_task = daemon.clone();
        let session_task = session.clone();
        let path_task = path.clone();
        let text_task = text.clone();
        tokio::spawn(async move {
            if let Some((kind, label)) = activity {
                let mut d = daemon_task.lock().await;
                d.add_activity(&path_task, kind, label);
                drop(d);
            }
            lsp::sync(&session_task, &path_task, &text_task).await;
            bg.mark("didChange");
            let (pulled_diags, pulled_hints) =
                lsp::pull_after_edit(&session_task, &path_task, &text_task).await;
            bg.mark("pull");
            let mut d = daemon_task.lock().await;
            if let Some((kind, _)) = activity {
                d.remove_activity(&path_task, kind);
            }
            if let Some(diags) = pulled_diags {
                // 編集は解析フォーカス文書に対してのみ pull される（current_uri 一致
                // ゲート — lsp::sync / pull_after_edit）ため、ここでフォーカスも更新される。
                d.set_focus_diagnostics(&path_task, diags);
            }
            if let Some(hints) = pulled_hints {
                d.cache_hints(path_task.clone(), &text_task, hints);
            }
            drain_into(&mut d);
            let snap = snapshot(&mut d, None);
            drop(d);
            bg.mark("reflect");
            // 発信元はコマンドでない（背景 pull）ため None を包み、全購読者に届く
            // （受信側は内容比較で再描画する — settle_open_diagnostics と同じ）。
            let _ = push_tx.send((None, snap));
            bg.mark("push");
        });
    }
    // 応答は現状のスナップショット（= pull 前の診断・ヒント）を即返す
    let mut d = daemon.lock().await;
    drain_into(&mut d);
    let snap = snapshot(&mut d, status);
    drop(d);
    drop(trace); // 応答側の合計（背景タスクは含まない）
    snap
}

/// 検索の継続状態（[`Command::Search`] の結果。`n`/`N` の前進元）。
#[derive(Clone)]
struct SearchState {
    query: String,
    /// 最後に見つけた一致の char 範囲（start..end）。
    found: (usize, usize),
}

/// コマンド行1件を処理して応答スナップショットを返す（ADR-0013 で
/// handle_connection から切り出し。I/O コマンドはロックを握ったまま
/// ブロックしないよう接続ハンドラ側で async 実行する）。
async fn process_command(
    daemon: &Arc<Mutex<Daemon>>,
    push_tx: &watch::Sender<(Option<u64>, StateSnapshot)>,
    conn_id: u64,
    source: EventSource,
    line: &str,
) -> StateSnapshot {
    let parsed = serde_json::from_str::<Command>(line.trim());
    // ADR-0037: コマンド適用前にフォーカスを発信接続の View（Interactive）または
    // 共有 idle view（Headless）へ合わせる。apply_from 側でも行う（テスト直呼びの
    // ため）が、Open のような process_command 直轄のアームもここで確実に揃う。
    {
        let mut d = daemon.lock().await;
        d.focus_for(conn_id);
    }
    // #13: headless クライアントは DocumentEdit 系に制限する（GetState / Save /
    // WaitFor / DocumentEdit / Open / Close のみ）。選択移動・モード変更・コマンド
    // ベース編集・undo/redo は TUI の表示・モード・カーソル・履歴を奪うため拒否
    // し、状態と世代を変えない（M1 と同じ扱い — 拒否で push も飛ばない）。
    // CONTEXT.md のドメインモデルどおり「TUI は Command のみ、agent は
    // DocumentEdit のみ」をプロトコル層で強制する。
    // #28（E1）: headless に素の Open を許可する（issue #28）。フォーカス変更は
    // 世代と push で他クライアント（TUI 含む）に伝播する — TUI は DocumentEdit
    // 由来の編集と同様に追従する。代替の「パス指定 DocumentEdit の自動オープン」
    // は将来拡張（同期 I/O のため handle 層の段組変更が必要）として範囲外。
    // #30: headless に Close も許可する（issue #30）。Open で開いた文書を
    // 外部削除された deleted 状態からでも閉じられ、空画面に戻せる。
    if source == EventSource::Headless {
        if let Ok(command) = &parsed {
            if !matches!(
                command,
                Command::GetState
                    | Command::Save
                    | Command::WaitFor { .. }
                    | Command::Open { .. }
                    | Command::Close
                    // #50: コメント抽出は headless の読み取り経路（`minas review`）。
                    // 全消しも headless に許可する（TUI 死後の掃除用。編集状態
                    // ではなくレビュー状態の管理のため #13 の趣旨と衝突しない）。
                    | Command::ListReviewComments
                    | Command::ClearReviewComments
                    // #53: 基準登録も headless に許可する（`minas exec` 駆動）。
                    // 書込みガードはパス基準で発信元を問わないため、登録で
                    // 権限が広がることはない（基準配下は自他問わず読取り専用）。
                    // sweep は `mina-base-*` 形式のみ対象のため、任意パス登録は
                    // 消されない（掃除は登録した agent が Unregister で行う）。
                    | Command::RegisterBaseRoot { .. }
                    | Command::UnregisterBaseRoot { .. }
            ) {
                let mut d = daemon.lock().await;
                return snapshot(
                    &mut d,
                    Some(
                        "headless clients can only use GetState, Save, WaitFor, DocumentEdit, Open, Close, ListReviewComments, ClearReviewComments, RegisterBaseRoot, and UnregisterBaseRoot"
                            .into(),
                    ),
                );
            }
        }
    }
    match parsed {
        // #12: 世代が `generation` を超えるまでブロックし、超えた時点の
        // スナップショットを返す（読み取り専用・状態を変えない）。
        // watch チャネル（ADR-0013）を再利用するのでポーリングも待機中の
        // 通信もない。既に超えていれば即応答する。
        //
        // ponytail: 待機中にクライアントが切断（kill 等）されても、デーモン側の
        // 待機は次の状態変化まで残る（次回の状態変化で応答書き込みが EPIPE に
        // なり後始末される）。接続スロット（MAX_CONNECTIONS=4）の枯渇は
        // 「agent 1 + TUI 1」の現実的な構成では起きないと判断。必要になったら
        // タイムアウトを追加する。
        Ok(Command::WaitFor { generation }) => {
            {
                let mut d = daemon.lock().await;
                d.metrics.wait_total += 1;
            }
            let mut rx = push_tx.subscribe();
            rx.borrow_and_update();
            loop {
                {
                    let mut d = daemon.lock().await;
                    if d.generation > generation {
                        return snapshot(&mut d, None);
                    }
                }
                // 次の状態変化を待つ（世代が進むたびに send され、値が変わら
                // なければ changed() は完了しない）。
                if rx.changed().await.is_err() {
                    // push 送信元が消えた（daemon 終了）: 現状を返して接続を
                    // 後始末に任せる。
                    let mut d = daemon.lock().await;
                    return snapshot(&mut d, Some("daemon terminated while waiting".into()));
                }
            }
        }
        // I/O コマンドはロックを握ったままブロックしないよう、接続ハンドラで処理する
        Ok(Command::Open { path }) => {
                // CRITICAL C2: 受信パスを正規化してから「既存文書の再利用判定」と
                // 「保存パス」の両方に使う。正規化しないとパス表記（相対/絶対・
                // `./x` と `x`・`a/../x`・symlink）が異なるだけで #7 の再利用が
                // 効かず、ディスク再読込で新規文書が作られ未保存編集が失われる。
                let path_buf = normalize_open_path(PathBuf::from(&path)).await;
                let path_str = path_buf.to_string_lossy().into_owned();
                // #7: 既に開かれているパスはディスク再読込せず、既存ドキュメントへ
                // フォーカスし直す。未保存編集・dirty フラグ・undo ヒストリーを
                // 保持する（再利用 = 「何も変えない」）。
                let reuse_text: Option<String> = {
                    let mut d = daemon.lock().await;
                    let reused = d.editor.focus_open_path(&path_buf);
                    // フォーカス直後のテキストを掴んでおく（後に ensure で await する
                    // 間に他接続がフォーカスを動かしても、正しい文書に通知するため）
                    reused.map(|_| d.editor.current_document().text().to_string())
                };
                if let Some(_) = reuse_text {
                    // 既に開かれているパス: フォーカスは focus_open_path 済み（#7）。
                    // 外部変更があれば自動リロードする（ADR-0015）。watch_disk の
                    // 2 秒周期より先にここで stat して検知し、LSP にはリロード後の
                    // テキストで didOpen を送る。
                    let stat = {
                        let _d = daemon.lock().await;
                        std::fs::metadata(&path_buf).ok().map(|md| {
                            (
                                md.modified().unwrap_or(std::time::UNIX_EPOCH),
                                md.len(),
                            )
                        })
                    };
                    let reload_text = match &stat {
                        Some((mtime, size)) => {
                            let diverged = {
                                let d = daemon.lock().await;
                                d.baselines.get(&path_buf).is_some_and(|b| {
                                    b.size != *size || b.mtime != *mtime
                                })
                            };
                            if diverged {
                                tokio::fs::read_to_string(&path_buf).await.ok()
                            } else {
                                None
                            }
                        }
                        None => None, // 削除済み: deleted は下のロック内で立てる
                    };
                    let text = {
                        let mut d = daemon.lock().await;
                        // ベースライン更新（削除済みなら保持し、復活検知に使う）
                        if let Some((mtime, size)) = &stat {
                            d.baselines.insert(
                                path_buf.clone(),
                                DiskBaseline {
                                    mtime: *mtime,
                                    size: *size,
                                },
                            );
                            d.deleted = None;
                        } else {
                            d.deleted = Some(path_str.clone());
                        }
                        if let Some(new_text) = &reload_text {
                            if let Some(doc_id) = d.editor.doc_id_for_path(&path_buf) {
                                // 外部書き込みとして Insert グループを閉じる
                                if d.insert_owner.is_some() {
                                    close_insert_session(&mut d, mina_view::Mode::Normal);
                                }
                                if d.editor.reload_doc(doc_id, new_text) {
                                    d.record_event(
                                        EventSource::External,
                                        EventKind::ExternalChange,
                                        None,
                                        None,
                                    );
                                }
                            }
                        }
                        d.editor.current_document().text().to_string()
                    };
                    // LSP: サーバが死んでいれば再生成し、現在のバッファ内容で didOpen を
                    // 再通知する（フルテキスト同期なので再利用への再通知は無害）。
                    // spawn + initialize + didOpen + 診断 settle は Open 応答をブロック
                    // しないようバックグラウンドで走らせる（起動時に LSP 初期化が遅いと
                    // Open 応答が数秒〜数十秒止まるため — 応答は文書状態のみ）。
                    let spawn_lsp = {
                        let mut d = daemon.lock().await;
                        let supported = d.languages_refresh().server_for(&path_buf).is_some();
                        if supported {
                            // ADR-0028: 診断取得の活動は応答スナップショットに確実に乗せる
                            // （settle 側は出口で除去）。
                            d.add_activity(&path_buf, ActivityKind::DiagnosticsSettle, "診断取得中");
                        }
                        supported
                    };
                    if spawn_lsp {
                        let daemon_task = daemon.clone();
                        let path_task = path_buf.clone();
                        let text_task = text.clone();
                        let push_task = push_tx.clone();
                        tokio::spawn(async move {
                            match ensure(&daemon_task, &path_task).await {
                                Ok(session) => {
                                    lsp::open_document(&session, &path_task, &text_task).await;
                                    settle_open_diagnostics(
                                        &daemon_task,
                                        session,
                                        path_task,
                                        push_task,
                                    )
                                    .await;
                                }
                                Err(_) => {
                                    // init 失敗: 活動だけ除去して静かに終了
                                    // （文書は保持される。次回の LSP 要求で再 spawn）
                                    let mut d = daemon_task.lock().await;
                                    d.remove_activity(&path_task, ActivityKind::DiagnosticsSettle);
                                }
                            }
                        });
                    }
                    let mut d = daemon.lock().await;
                    drain_into(&mut d);
                    // ADR-0012: 再利用も Open として記録（フォーカス変更）
                    d.record_event(source, EventKind::Open, None, None);
                    d.open_idle_follow(&path_buf);
                    // v12: 再利用も解析フォーカスとして記録（フォーカス切替）
                    d.set_analysis_focus(&path_buf);
                    // ADR-0038: Headless 由来の Open 試行を記録（読み取り系は除外）
                    if source == EventSource::Headless {
                        let actor = d.actor_for(conn_id);
                        d.record_activity(actor, EventKind::Open, true, path_str.clone());
                    }
                    snapshot(&mut d, None)
                } else {
                    // 未開パス: 従来どおりディスクから読む
                    // SEC-1: 非正規ファイル・過大ファイルを metadata で検証してから読む
                    // （ADR-0008）。失敗時は状態を変えず status で報告する。
                    let (contents, open_status) = read_open_target(&path_str).await;
                    // LSP: spawn + initialize + didOpen + 診断 settle は Open 応答を
                    // ブロックしないようバックグラウンドで走らせる（起動時に LSP
                    // 初期化が遅いと Open 応答が数秒〜数十秒止まるため — 応答は
                    // 文書状態のみ。didOpen は全文同期で後から追いつく）。失敗は
                    // Open の status に載せず、活動の除去だけで静かに終了する
                    // （文書は保持される。次回の LSP 要求で再 spawn）。
                    let lsp_supported = {
                        let mut d = daemon.lock().await;
                        let supported = contents.is_some()
                            && d.languages_refresh().server_for(&path_buf).is_some();
                        if supported {
                            // ADR-0028: 診断取得の活動は応答スナップショットに確実に乗せる
                            // （settle 側は出口で除去）。
                            d.add_activity(&path_buf, ActivityKind::DiagnosticsSettle, "診断取得中");
                        }
                        supported
                    };
                    // ロック内: 文書状態の変更のみ（await なし）
                    let mut d = daemon.lock().await;
                    let text = match &contents {
                        Some(contents) => {
                            d.editor.open_with_path(path_buf.clone(), contents);
                            let height = d.viewport_height;
                            d.editor.scroll_to_cursor(height);
                            // v12: Open = 解析フォーカスの切替（この文書の診断は
                            // settle の pull が入るまで表示しない — 旧フォーカス分で
                            // 残っているものは捨てる）。
                            d.set_analysis_focus(&path_buf);
                            d.diagnostics.remove(&path_buf);
                            // ADR-0012/0015: ベースライン更新 + Open イベント
                            if let Ok(md) = std::fs::metadata(&path_buf) {
                                d.baselines.insert(
                                    path_buf.clone(),
                                    DiskBaseline {
                                        mtime: md
                                            .modified()
                                            .unwrap_or(std::time::UNIX_EPOCH),
                                        size: md.len(),
                                    },
                                );
                            }
                            d.deleted = None;
                            d.record_event(source, EventKind::Open, None, None);
                            contents.clone()
                        }
                        None => String::new(),
                    };
                    drop(d);
                    if lsp_supported {
                        let daemon_task = daemon.clone();
                        let path_task = path_buf.clone();
                        let text_task = text;
                        let push_task = push_tx.clone();
                        tokio::spawn(async move {
                            match ensure(&daemon_task, &path_task).await {
                                Ok(session) => {
                                    lsp::open_document(&session, &path_task, &text_task).await;
                                    settle_open_diagnostics(
                                        &daemon_task,
                                        session,
                                        path_task,
                                        push_task,
                                    )
                                    .await;
                                }
                                Err(_) => {
                                    // init 失敗: 活動だけ除去して静かに終了
                                    // （文書は保持される。次回の LSP 要求で再 spawn）
                                    let mut d = daemon_task.lock().await;
                                    d.remove_activity(&path_task, ActivityKind::DiagnosticsSettle);
                                }
                            }
                        });
                    }
                    let mut d = daemon.lock().await;
                    drain_into(&mut d);
                    d.open_idle_follow(&path_buf);
                    // ADR-0038: 読み込み成否も含めて記録する（ok = 実際に文書を読めたか）
                    if source == EventSource::Headless {
                        let ok = contents.is_some();
                        let detail = open_status
                            .clone()
                            .unwrap_or_else(|| path_str.clone());
                        let actor = d.actor_for(conn_id);
                        d.record_activity(actor, EventKind::Open, ok, detail);
                    }
                    snapshot(&mut d, open_status)
                }
            }
            Ok(Command::Save) => {
                // #49: 基準配下への保存を拒否する。
                let blocked = {
                    let d = daemon.lock().await;
                    d.editor.focused_path().and_then(|p| d.base_reject(p))
                };
                if let Some(msg) = blocked {
                    let mut d = daemon.lock().await;
                    return snapshot(&mut d, Some(msg));
                }
                // 保存対象（テキスト・パス・文書 ID）を取り出してから、ロック外で書き込む
                {
                    let mut d = daemon.lock().await;
                    d.metrics.save_total += 1;
                }
                let (text, path, doc_id) = {
                    let d = daemon.lock().await;
                    let text = d.editor.current_document().text().to_string();
                    let doc_id = d.editor.focused_doc_id();
                    (text, d.editor.focused_path().map(Path::to_path_buf), doc_id)
                };
                // ADR-0028: write 前に保存の活動を追加（スナップショットに載るのは次以降）。
                if let Some(p) = &path {
                    let mut d = daemon.lock().await;
                    d.add_activity(p, ActivityKind::Save, "保存中");
                    drop(d);
                }
                let write_result = match &path {
                    Some(p) => tokio::fs::write(p, text.as_bytes()).await,
                    None => Err(io::Error::new(io::ErrorKind::NotFound, "no file name")),
                };
                let mut d = daemon.lock().await;
                // ADR-0028: 保存の活動を除去（write の前で追加済み。成功/失敗どちらも除去）。
                if let Some(p) = &path {
                    d.remove_activity(p, ActivityKind::Save);
                }
                match write_result {
                    Ok(()) => {
                        // 保存した文書そのものの dirty を消す。ただし書き込んだ
                        // テキストが現在のテキストと一致する場合のみ（H3: 書き込
                        // み中に他接続が Open してフォーカスが変わっても、保存対象
                        // の文書を正しく扱う / HIGH-1: 書き込み中に他接続が編集し
                        // たなら古いテキストを保存したことになるので、dirty を残
                        // して未保存の編集を失わせない）。
                        let clean = d.editor.mark_saved_doc(doc_id, &text);
                        let shown = path
                            .as_ref()
                            .expect("書き込み成功ならパスはある")
                            .display();
                        // ADR-0012/0015: 保存後にベースライン更新（外部変更検知の
                        // 基準を現在のディスク状態に合わせる）。ファイルを再作成する
                        // ことになるため、削除状態は解消する。
                        if let Some(p) = &path {
                            if let Ok(md) = std::fs::metadata(p) {
                                d.baselines.insert(
                                    p.clone(),
                                    DiskBaseline {
                                        mtime: md
                                            .modified()
                                            .unwrap_or(std::time::UNIX_EPOCH),
                                        size: md.len(),
                                    },
                                );
                            }
                            d.deleted = None;
                            d.record_event(source, EventKind::Save, None, None);
                        }
                        let status = if clean {
                            format!("saved: {shown}")
                        } else {
                            format!("saved: {shown} (edited during save, still dirty)")
                        };
                        // ADR-0038: Headless 由来の保存の試行を記録
                        if source == EventSource::Headless {
                            let actor = d.actor_for(conn_id);
                            let detail = path
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "(no file)".into());
                            d.record_activity(actor, EventKind::Save, true, detail);
                        }
                        snapshot(&mut d, Some(status))
                    }
                    Err(e) => {
                        // ADR-0038: 保存失敗も記録する（失敗率評価用）
                        if source == EventSource::Headless {
                            let actor = d.actor_for(conn_id);
                            d.record_activity(
                                actor,
                                EventKind::Save,
                                false,
                                format!("save failed: {e}"),
                            );
                        }
                        snapshot(&mut d, Some(format!("save failed: {e}")))
                    }
                }
            }
            Ok(Command::RegisterBaseRoot { root, commit, repo }) => {
                // #49: 基準 root の登録。LSP セッションは初回読取り時に lazy 確保
                // （borrowed ensure）。冪等（再登録は commit 更新）。
                // #52: repo（対応 live リポジトリ）があれば正規化して保持し、
                // 基準診断の対応付けに使う。無い場合は診断が付かない。
                let canon = match tokio::fs::canonicalize(&root).await {
                    Ok(p) => p,
                    Err(_) => {
                        let mut d = daemon.lock().await;
                        return snapshot(&mut d, Some(format!("基準 root がありません: {root}")));
                    }
                };
                let canon_repo = match repo {
                    None => None,
                    Some(r) => match tokio::fs::canonicalize(&r).await {
                        Ok(p) => Some(p),
                        Err(_) => {
                            let mut d = daemon.lock().await;
                            return snapshot(
                                &mut d,
                                Some(format!("対応リポジトリがありません: {r}")),
                            );
                        }
                    },
                };
                let mut d = daemon.lock().await;
                d.register_base_root(canon, commit, canon_repo, source);
                snapshot(&mut d, None)
            }
            // #49: 基準 root の登録解除。セッション破棄＋キャッシュ破棄。
            // 不在は無視（M1: 変化なし・世代不変）。worktree 削除後の順序にも
            // 対応するため、正規化失敗時は生パスで照合する。
            Ok(Command::UnregisterBaseRoot { root }) => {
                let canon = tokio::fs::canonicalize(&root)
                    .await
                    .unwrap_or_else(|_| PathBuf::from(&root));
                let sessions = {
                    let mut d = daemon.lock().await;
                    d.unregister_base_root(&canon, source)
                };
                // daemon ロック外で LSP プロセスを kill する。
                for s in sessions {
                    s.lock().await.client.kill().await;
                }
                let mut d = daemon.lock().await;
                snapshot(&mut d, None)
            }
            // #50: レビューコメントの追加/更新/削除。TUI 駆動のみ（headless は
            // #13 ゲートで拒否。#53 で基準登録は headless 解禁済みだが、Add は
            // TUI のカーソル行アンカーが前提のため対象外）。同一アンカーは上書き、
            // 空本文は削除。世代を進め push に載せる。
            Ok(Command::AddReviewComment {
                path,
                side,
                line,
                snippet,
                body,
                base,
            }) => {
                let mut d = daemon.lock().await;
                let generation = d.generation + 1;
                d.add_review_comment(
                    ReviewComment {
                        path,
                        side,
                        line,
                        snippet,
                        body,
                        base,
                        generation,
                    },
                    source,
                );
                snapshot(&mut d, None)
            }
            // #50: レビューコメントの全消し。TUI 駆動のみ（同上）。
            // 空のときは変化なし（M1: 世代不変）。
            Ok(Command::ClearReviewComments) => {
                let mut d = daemon.lock().await;
                d.clear_review_comments(source);
                snapshot(&mut d, None)
            }
            Ok(command) => {
                let mut d = daemon.lock().await;
                // ADR-0012: 状態を変える操作（編集・undo/redo・モード変更）だけを
                // イベントとして記録する。Move/Extend/Goto/Scroll/SetViewport は
                // 現在値がスナップショットに載るため対象外。
                let event = match &command {
                    Command::Insert { text } => {
                        Some((EventKind::Insert, None, Some(text.clone())))
                    }
                    // o/O は空行（改行）の挿入として記録する
                    Command::OpenBelow | Command::OpenAbove => {
                        Some((EventKind::Insert, None, Some("\n".into())))
                    }
                    Command::DeleteBackward
                    | Command::DeleteForward
                    | Command::DeleteWordBackward
                    | Command::DeleteWordForward
                    | Command::DeleteRange
                    | Command::KillToLineStart
                    | Command::KillToLineEnd => {
                        // 削除範囲 = 適用前の選択（primary）
                        let r = d.editor.selection().primary();
                        Some((
                            EventKind::Delete,
                            Some(Range {
                                anchor: r.start(),
                                head: r.end(),
                            }),
                            None,
                        ))
                    }
                    Command::Replace { text } => {
                        // 置換範囲 = 適用前の選択（primary）
                        let r = d.editor.selection().primary();
                        Some((
                            EventKind::ReplaceRange,
                            Some(Range {
                                anchor: r.start(),
                                head: r.end(),
                            }),
                            Some(text.clone()),
                        ))
                    }
                    // surround（ms/md/mr）はテキストを変える — ReplaceRange 扱いで
                    // 基準配下の書込み拒否（base_reject）と LSP 同期（is_edit）に載せる。
                    Command::SurroundAdd { .. }
                    | Command::SurroundDelete { .. }
                    | Command::SurroundReplace { .. } => {
                        let r = d.editor.selection().primary();
                        Some((
                            EventKind::ReplaceRange,
                            Some(Range {
                                anchor: r.start(),
                                head: r.end(),
                            }),
                            None,
                        ))
                    }
                    Command::Change => {
                        // 選択があれば Delete、カーソル上なら SetMode 相当のイベント
                        let r = d.editor.selection().primary();
                        if r.is_cursor() {
                            Some((EventKind::SetMode, None, None))
                        } else {
                            Some((
                                EventKind::Delete,
                                Some(Range {
                                    anchor: r.start(),
                                    head: r.end(),
                                }),
                                None,
                            ))
                        }
                    }
                    // A/I/a はテキストを変えず、モードが実際に変わる場合だけ
                    // SetMode イベント（選択移動は Move 同様スナップショットに載る）。
                    // o/O は上で Insert イベント（改行）として記録済み。
                    Command::InsertAtLineEnd | Command::InsertAtLineStart | Command::Append
                        if d.editor.mode() != mina_view::Mode::Insert =>
                    {
                        Some((EventKind::SetMode, None, None))
                    }
                    Command::Undo if d.editor.can_undo() => Some((EventKind::Undo, None, None)),
                    Command::Redo if d.editor.can_redo() => Some((EventKind::Redo, None, None)),
                    Command::Close => Some((EventKind::Close, None, None)),
                    Command::SetMode { mode } if convert_mode(*mode) != d.editor.mode() => {
                        Some((EventKind::SetMode, None, None))
                    }
                    _ => None,
                };
                let is_edit = is_edit(&command);
                // ADR-0038: Headless 由来の Close の試行記録用に、閉鎖済みかと
                // 閉鎖対象（apply 前のフォーカス文書）を確保しておく
                let is_close = matches!(&command, Command::Close);
                let closing = (source == EventSource::Headless && is_close)
                    .then(|| {
                        d.editor
                            .focused_path()
                            .map(|p| p.to_string_lossy().into_owned())
                    })
                    .flatten();
                // #49: テキスト系イベント && フォーカス文書が基準配下 → 拒否。
                // 分類済み種別で判定する（新規書込みコマンドはイベント分類への
                // 追加が必須のため追随するだけ）。拒否は状態を変えず status で
                // 報告する（M1 の拒否と同型）。
                let blocked: Option<String> = match &event {
                    Some((kind, _, _))
                        if matches!(
                            kind,
                            EventKind::Insert
                                | EventKind::Delete
                                | EventKind::ReplaceRange
                                | EventKind::Undo
                                | EventKind::Redo
                        ) =>
                    {
                        d.editor.focused_path().and_then(|p| d.base_reject(p))
                    }
                    _ => None,
                };
                if let Some(msg) = blocked {
                    return snapshot(&mut d, Some(msg));
                }
                let (applied, changed) = apply_from(&mut d, command, conn_id);
                // 失敗理由（ペアが見つからない等）は apply_from のスナップショットの
                // status に載っている。最終応答は LSP 同期後に作り直すため、
                // status だけを引き継ぐ（落とすと TUI に何も表示されない）。
                let status = applied.status.clone();
                // ADR-0012: 実際に状態が変わった場合のみイベントを記録する。
                // 拒否（サイズ超過等）・no-op（空削除・空挿入・履歴のない
                // undo/redo・モード不変の SetMode）は世代もイベントも進めない。
                if changed {
                    if let Some((kind, range, text)) = event {
                        d.record_event(source, kind, range, text);
                    }
                }
                // ADR-0038: Headless 由来の Close の試行を記録（成功・失敗とも）。
                // Close の changed = 実際に文書を閉じたか。応答は後段の
                // sync_after_edit 経路に載る（世代・activity 込み）。
                if source == EventSource::Headless && is_close {
                    let actor = d.actor_for(conn_id);
                    let detail = closing.unwrap_or_else(|| "(no file open)".into());
                    d.record_activity(actor, EventKind::Close, changed, detail);
                }
                // M1/ADR-0009: 同期対象（セッション・パス・テキスト）をロック内で
                // 取り出し、didChange はロック外で await する（サーバ遅延で全
                // クライアントがブロックしない）。
                let sync_target = if is_edit {
                    d.editor.focused_path().map(Path::to_path_buf).and_then(|path| {
                        // フォーカス文書の WorkspaceRoot に対応するセッションだけを
                        // 同期対象にする（ADR-0010）。LSP 対応以外の文書にはセッションが
                        // なく、同期スキップ + drain_into で診断が消える。
                        d.session_for(&path)
                            .map(|session| {
                                let text = d.editor.current_document().text().to_string();
                                (session, path, text)
                            })
                    })
                } else {
                    None
                };
                drop(d);
                sync_after_edit(daemon, push_tx.clone(), sync_target, status, None).await
            }
            Err(_) => {
                // ADR-0011: Command として解釈できなければ DocumentEdit を試す
                // （Command は外部タグ付き enum、DocumentEdit は構造体なので
                // JSON 形状で衝突しない）。
                match serde_json::from_str(line.trim()) {
                    Ok(edit) => {
                        // ADR-0056: minas の `apply`（DocumentEdit）の内訳。
                        // edit = 適用（apply_edit + 記録）/ sync = 応答構築。
                        let mut trace = Trace::new("edit");
                        let mut d = daemon.lock().await;
                        // 拒否（checksum 不一致）なら状態を変えず status を返す
                        let rejected = apply_edit(&mut d, &edit, conn_id, source);
                        if rejected.is_none() {
                            // ADR-0012: 適用された DocumentEdit を記録（成功時のみ）
                            d.record_event(
                                source,
                                EventKind::ReplaceRange,
                                Some(Range {
                                    anchor: edit.start,
                                    head: edit.end,
                                }),
                                Some(edit.text.clone()),
                            );
                        }
                        let sync_target = if rejected.is_none() {
                            // 編集後の LSP 同期（Command 編集と同じ経路。ADR-0009）
                            d.editor.focused_path().map(Path::to_path_buf).and_then(|path| {
                                d.session_for(&path)
                                    .map(|session| {
                                        let text =
                                            d.editor.current_document().text().to_string();
                                        (session, path, text)
                                    })
                            })
                        } else {
                            None
                        };
                        drop(d);
                        trace.mark("apply_edit");
                        if let Some(rejected) = rejected {
                            rejected
                        } else {
                            sync_after_edit(daemon, push_tx.clone(), sync_target, None, None).await
                        }
                    }
                    Err(_) => {
                        // M7: 壊れたコマンド行にも status 付きスナップショットを返す
                        // （応答なしだと送信元が永久待ちになる）
                        let mut d = daemon.lock().await;
                        snapshot(&mut d, Some("invalid command".into()))
                    }
                }
            }
        }
}

/// 編集系コマンドか（LSP 全文同期の対象）。
fn is_edit(command: &Command) -> bool {
    matches!(
        command,
        Command::Insert { .. }
            | Command::DeleteBackward
            | Command::DeleteForward
            | Command::DeleteWordBackward
            | Command::DeleteWordForward
            | Command::DeleteRange
            | Command::Change
            | Command::OpenBelow
            | Command::OpenAbove
            | Command::Replace { .. }
            | Command::SurroundAdd { .. }
            | Command::SurroundDelete { .. }
            | Command::SurroundReplace { .. }
            | Command::KillToLineStart
            | Command::KillToLineEnd
            | Command::Undo
            | Command::Redo
    )
}

/// Open パスの同一性を正規化する（CRITICAL C2: パス表記の違いで #7 の再利用が
/// 効かない問題の修正）。
///
/// 同一ファイルを指す異なる表記（相対/絶対・`./x` と `x`・`a/../x`・symlink）が
/// 同じ文書として再利用されるよう、既存ファイルは canonicalize で実体パスに
/// 解決する。ファイルが存在しない（新規ファイルの Open）場合は
/// [`lexical_normalize`] にフォールバックする — canonicalize はパスの一部でも
/// 存在しないと失敗するため。
///
/// 正規化したパスは「既存文書の再利用判定」（[`Editor::focus_open_path`]）と
/// 「保存パス」（[`Editor::open_with_path`]）の両方に使われるため、LSP の
/// workspace_root・診断・Save 先も一貫して同じ実体パスになる。
///
/// ponytail: 存在しないパスの symlink 解決はできない（canonicalize の制約）。
/// ファイルが後から作られる場合は `.`/`..` を含まない表記同士なら同一視できる。
async fn normalize_open_path(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = tokio::fs::canonicalize(&path).await {
        return canonical;
    }
    lexical_normalize(&path)
}

/// 存在しないパス向けの lexical 正規化: 絶対化 + `.` / `..` の解決。
///
/// 送信側（TUI / session CLI）は常に絶対化して送る（[`mina_conn::absolutize`]）
/// ため、相対パスが届くのは第三者の生クライアントだけ。その場合の解決基準は
/// daemon の cwd（spawn 時に固定。ADR-0005）で、従来のディスク読込と同じ解釈。
fn lexical_normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {} // . は無視
            std::path::Component::ParentDir => {
                // .. は1段戻る。ルートより上は pop が false になり無視される
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Open 対象を検証して読み込む。
///
/// 非正規ファイルは `cannot open`、[`MAX_FILE_SIZE`] 超は `file too large`、
/// 非 UTF-8 等の読み込み失敗は `cannot read` を status で報告する（SEC-1 /
/// ADR-0008）。
///
/// TOCTOU 対策: サイズ検証は open した fd の fstat で行い、その fd から
/// バイト上限付きで読む（metadata と read が別々の path を辿らない）。
///
/// ponytail: UTF-8 のみの I/O（read_to_string）。非 UTF-8 対応は v1 対象外。
/// 検証→open の間にパスが FIFO に差し替えられた場合は open でブロックし得る
/// （単一ユーザ前提。O_NONBLOCK 化は必要になってから）。
async fn read_open_target(path: &str) -> (Option<String>, Option<String>) {
    // 高速パス: 非正規ファイル（FIFO・ディレクトリ等）は open 前に弾く
    match tokio::fs::metadata(path).await {
        Ok(m) if m.len() > MAX_FILE_SIZE => {
            return (None, Some(format!("file too large: {path}")))
        }
        Ok(m) if !m.is_file() => return (None, Some(format!("cannot open {path}"))),
        _ => {}
    }
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return (None, Some(format!("cannot open {path}"))),
    };
    // 同一 fd の fstat で再検証（検証後に巨大化したファイルを丸読みしない）
    match file.metadata().await {
        Ok(m) if m.is_file() && m.len() <= MAX_FILE_SIZE => {
            let mut contents = String::new();
            // バイト上限付きで同一 fd から読む（fstat 後に伸びた分も cap）
            match file
                .take(MAX_FILE_SIZE + 1)
                .read_to_string(&mut contents)
                .await
            {
                Ok(_) if contents.len() as u64 > MAX_FILE_SIZE => {
                    (None, Some(format!("file too large: {path}")))
                }
                Ok(_) => (Some(contents), None),
                // 非 UTF-8 など decode 失敗も status で報告する（修正前は
                // 握り潰して「空文書が開けた」ように見えていた）
                Err(e) => (None, Some(format!("cannot read {path}: {e}"))),
            }
        }
        Ok(m) if m.len() > MAX_FILE_SIZE => (None, Some(format!("file too large: {path}"))),
        Ok(_) => (None, Some(format!("cannot open {path}"))),
        Err(_) => (None, Some(format!("cannot open {path}"))),
    }
}

/// ファイルを読み込んで Editor に開く（clean 状態で始まる）。テスト用。
#[cfg(test)]
fn open_in_editor(daemon: &mut Daemon, path: &str, contents: Option<String>) -> StateSnapshot {
    match contents {
        Some(contents) => {
            daemon.editor.open_with_path(PathBuf::from(path), &contents);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        None => snapshot(daemon, Some(format!("cannot open {path}"))),
    }
}

/// Open/Save 以外のコマンドを状態に適用し、新しいスナップショットを返す。
///
/// 接続 ID なし（テストの既定 = 接続 0）で適用する。
#[cfg(test)]
fn apply(daemon: &mut Daemon, command: Command) -> StateSnapshot {
    apply_from(daemon, command, 0).0
}

/// expected_text 不一致エラーに含める期待値・実値スニペットの最大 char 数
/// （C1: 全文を返すと長文ドキュメントで応答が肥大するため先頭N文字）。
const MAX_MISMATCH_SNIPPET_CHARS: usize = 40;

/// エラー応答用に文字列を先頭N文字に短縮し、切れたら `…` を付す。
fn mismatch_snippet(s: &str) -> String {
    let mut out: String = s.chars().take(MAX_MISMATCH_SNIPPET_CHARS).collect();
    if s.chars().count() > MAX_MISMATCH_SNIPPET_CHARS {
        out.push('…');
    }
    out
}

/// DocumentEdit の種類（activity の kind — 挿入/削除/置換。ADR-0038）。
fn edit_kind(edit: &DocumentEdit) -> EventKind {
    if edit.start == edit.end {
        EventKind::Insert
    } else if edit.text.is_empty() {
        EventKind::Delete
    } else {
        EventKind::ReplaceRange
    }
}

/// 位置指定編集（ADR-0011）を適用する。拒否・無変化時はスナップショットを返し、
/// 状態は一切変えない（checksum 不一致・expected_text 不一致・空置換の no-op）。
/// 成功時は `None` を返す。
///
/// 選択は読まず・変えない（履歴には before == after として記録されるので
/// undo でも選択は動かない）。挿入 = `start == end`、削除 = `text` が空。
/// 範囲クランプと Transaction 構築は minae-core の `insert_at` に委譲する（#25）。
/// `expected_text` が `Some` なら2段検証（#26/B2）: checksum に加えて対象範囲の
/// 現テキストと一致することも検証し、不一致ならチェックサム不一致とは別の
/// status（期待値・実値のスニペット付き）で拒否する（C1）。
fn apply_edit(
    daemon: &mut Daemon,
    edit: &DocumentEdit,
    conn_id: u64,
    source: EventSource,
) -> Option<StateSnapshot> {
    // #49: 基準配下への位置指定編集を拒否する（全経路の集約点）。
    // 拒否は状態を変えず status で報告する（既存の checksum 拒否と同型）。
    let blocked: Option<String> = daemon
        .editor
        .focused_path()
        .and_then(|p| daemon.base_reject(p));
    if let Some(msg) = blocked {
        daemon.metrics.edits_total += 1;
        if source == EventSource::Headless {
            let actor = daemon.actor_for(conn_id);
            daemon.record_activity(actor, edit_kind(edit), false, msg.clone());
        }
        return Some(snapshot(daemon, Some(msg)));
    }
    daemon.metrics.edits_total += 1;
    let text = daemon.editor.current_document().text().to_string();
    if fnv1a64(text.as_bytes()) != edit.checksum {
        daemon.metrics.edits_rejected_checksum += 1;
        // ADR-0038: 拒否も記録する（応答スナップショットに載るよう先に記録）
        if source == EventSource::Headless {
            let actor = daemon.actor_for(conn_id);
            daemon.record_activity(
                actor,
                edit_kind(edit),
                false,
                "document changed since read".into(),
            );
        }
        return Some(snapshot(
            daemon,
            Some("document changed since read".into()),
        ));
    }
    // 局所検証: insert_at と同じクランプ・正規化で対象範囲を特定する。
    // 位置のずれは checksum（全文）では検出できず expected_text（局所）で検出する。
    if let Some(expected) = &edit.expected_text {
        daemon.metrics.edits_expected_text_used += 1;
        let len = daemon.editor.current_document().len_chars();
        let start = edit.start.min(len);
        let end = edit.end.min(len);
        let (lo, hi) = (start.min(end), start.max(end));
        let actual = daemon.editor.current_document().text().slice(lo..hi).to_string();
        if actual != *expected {
            daemon.metrics.edits_rejected_expected_text += 1;
            // ADR-0038: 局所検証の拒否も記録する
            if source == EventSource::Headless {
                let actor = daemon.actor_for(conn_id);
                daemon.record_activity(
                    actor,
                    edit_kind(edit),
                    false,
                    format!(
                        "expected text mismatch at [{lo}, {hi}): expected {:?}, found {:?}",
                        mismatch_snippet(expected),
                        mismatch_snippet(&actual),
                    ),
                );
            }
            return Some(snapshot(
                daemon,
                Some(format!(
                    "expected text mismatch: expected {:?}, found {:?} at [{lo}, {hi})",
                    mismatch_snippet(expected),
                    mismatch_snippet(&actual),
                )),
            ));
        }
    }
    // ADR-0007: 他クライアントの書き込みとして、開いた Insert グループを閉じる
    preempt(daemon, conn_id);
    let (new_doc, tx) = insert_at(
        daemon.editor.current_document(),
        edit.start,
        edit.end,
        &edit.text,
    );
    if tx.is_noop() {
        // ADR-0012: 空範囲への空文字置換など状態を変えない編集は、拒否と同じ
        // 扱いでイベント・世代・undo 履歴を進めない（M1）。
        daemon.metrics.edits_noop += 1;
        // ADR-0038: 空振りの編集試行も失敗として記録する（応答に載る）
        if source == EventSource::Headless {
            let actor = daemon.actor_for(conn_id);
            daemon.record_activity(actor, edit_kind(edit), false, "no-op (unchanged)".into());
        }
        return Some(snapshot(daemon, None));
    }
    // ADR-0038: 適用した編集を記録する（応答 = 後段の sync_after_edit 経路に載る）
    if source == EventSource::Headless {
        let actor = daemon.actor_for(conn_id);
        let target = daemon
            .editor
            .focused_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(scratch)".into());
        let detail = format!("{} [{}..{}]", target, edit.start, edit.end);
        daemon.record_activity(actor, edit_kind(edit), true, detail);
    }
    let selection_after = daemon.editor.selection();
    // CRITICAL C1: ADR-0011 の「選択を読まず・変えない」は、選択が文書の
    // 範囲内にあることが前提。編集で文書が選択位置より短くなると選択が
    // 範囲外に残り、直後の scroll_to_cursor（char_to_line）が panic し、
    // 以降の Move/Delete も panic して daemon が wedge する。ここで適用後の
    // 文書長へクランプして状態を有効に保つ。範囲内の選択は一切変わらない
    // ので、通常時は ADR-0011 どおり不変。apply に渡すため履歴の
    // selection_after もクランプされ、redo でも範囲外に戻らない。
    let new_len = new_doc.len_chars();
    let clamp = |pos: usize| pos.min(new_len);
    let selection_after = Selection::new(
        selection_after
            .ranges()
            .iter()
            .map(|r| CoreRange::new(clamp(r.anchor()), clamp(r.head())))
            .collect(),
        selection_after.primary_index(),
    );
    daemon.editor.apply(tx, selection_after);
    daemon.editor.scroll_to_cursor(daemon.viewport_height);
    None
}

/// 書き込み競合の後勝ち奪取（HIGH-1 確定設計）: 別クライアントが開いた Insert
/// グループが開いている間に、非所有者の書き込みが来たら、先にそのグループを
/// ADR-0007/HIGH-1: 開いている Insert グループを閉じ、モードを移す。
/// 不変条件「mode == Insert ⟺ insert_owner == Some(_)」の解除を1箇所に持つ
/// （切断・外部書き込み・奪取・SetMode の4経路が以前は3行を個別に再現していた）。
/// 呼び出し側が事前に条件（所有者一致・外部書き込み・奪取）を判定する。
fn close_insert_session(daemon: &mut Daemon, mode: mina_view::Mode) {
    daemon.editor.end_group();
    daemon.editor.set_mode(mode);
    daemon.insert_owner = None;
}

/// 閉じて Normal に戻してから編集を適用する。これにより 1 つの UndoGroup に
/// 異なるクライアントの編集が混入しない（単一書き手への直列化）。
///
/// 主用途（agent が駆動し人間 TUI が介入）では agent の書き込みが後勝ちになる。
/// 所有者自身の書き込みと、グループが開いていない書き込みには影響しない。
fn preempt(daemon: &mut Daemon, conn_id: u64) {
    if let Some(owner) = daemon.insert_owner {
        if owner != conn_id && daemon.editor.mode() == mina_view::Mode::Insert {
            close_insert_session(daemon, mina_view::Mode::Normal);
        }
    }
}

/// Select モードを抜ける（Helix の `exit_select_mode`。surround 後は選択を
/// 残したまま Normal に戻る — 囲んだ全体が選択された状態になる）。
fn exit_select_mode(daemon: &mut Daemon) {
    if daemon.editor.mode() == mina_view::Mode::Select {
        daemon.editor.set_mode(mina_view::Mode::Normal);
    }
}

/// A/I（`InsertAtLineEnd` / `InsertAtLineStart`）の移動先と `o`/`O`（空行を開く）の向き。
enum LinePos {
    /// 行末（改行の直前）。
    End,
    /// 最初の非空白文字（空白のみの行は列 0）。
    FirstNonWhitespace,
    /// 現在の行の下（`o`）。
    Below,
    /// 現在の行の上（`O`）。
    Above,
}

/// Insert モードへ入る（ADR-0007 のグループ管理込み）。SetMode(Insert) と
/// 同一の経路 — 既に Insert で別クライアントが所有している場合は後勝ちで
/// 奪取する（HIGH-1）。
fn enter_insert(daemon: &mut Daemon, conn_id: u64) {
    let current = daemon.editor.mode();
    if current != mina_view::Mode::Insert {
        daemon.editor.begin_group();
        daemon.insert_owner = Some(conn_id);
    } else if daemon.insert_owner != Some(conn_id) {
        daemon.editor.end_group();
        daemon.editor.begin_group();
        daemon.insert_owner = Some(conn_id);
    }
    daemon.editor.set_mode(mina_view::Mode::Insert);
}

/// `A`/`I` 共通処理: 選択を目標位置へ点に潰して Insert モードへ入る
/// （ADR-0023）。テキストは変えないので preempt 不要。Select でも折りたたむ —
/// minae の `Transaction::insert` は選択範囲を置換するため、拡張したまま Insert
/// に入るとタイプ文字が選択範囲を置換し、Helix の「行末に追加」という観測挙動と
/// 一致しない（ADR-0023）。
fn insert_at_line(daemon: &mut Daemon, conn_id: u64, target: LinePos) -> (StateSnapshot, bool) {
    let selection = daemon.editor.selection();
    let moved = match target {
        LinePos::End => move_selection(
            daemon.editor.current_document(),
            &selection,
            mina_text::Movement::LineEnd,
            mina_text::Direction::Forward,
        ),
        LinePos::FirstNonWhitespace => {
            move_selection_to_line_first_non_whitespace(daemon.editor.current_document(), &selection)
        }
        // o/O は open_line 側で処理する（ここには来ない）
        LinePos::Below | LinePos::Above => unreachable!(),
    };
    daemon.editor.set_selection(moved);
    // 既に Insert ならモードは変わらない（changed = false → 世代もイベントも
    // 進まない。選択位置の移動だけがスナップショットに載る）。
    let changed = daemon.editor.mode() != mina_view::Mode::Insert;
    enter_insert(daemon, conn_id);
    daemon.editor.scroll_to_cursor(daemon.viewport_height);
    (snapshot(daemon, None), changed)
}

/// `o`/`O` 共通処理: ヘッド行の下/上に空行を開いて Insert モードへ入る
/// （Helix の open_below / open_above）。改行の挿入は undo グループの外（`Change`
/// と同じ — undo 1回で空行だけが戻り、以後のタイプが新グループになる）。
fn open_line(daemon: &mut Daemon, conn_id: u64, target: LinePos) -> (StateSnapshot, bool) {
    let doc = daemon.editor.current_document();
    let selection = daemon.editor.selection();
    let (insert_at, after) = match target {
        LinePos::Below => (
            move_selection(
                doc,
                &selection,
                mina_text::Movement::LineEnd,
                mina_text::Direction::Forward,
            ),
            true, // 改行を挟んだ先（新しい行の先頭）に立つ
        ),
        LinePos::Above => (
            move_selection(
                doc,
                &selection,
                mina_text::Movement::LineStart,
                mina_text::Direction::Forward,
            ),
            false, // 改行の手前（新しい空行の先頭）に立つ
        ),
        // A/I は insert_at_line 側で処理する（ここには来ない）
        LinePos::End | LinePos::FirstNonWhitespace => unreachable!(),
    };
    let tx = Transaction::insert(doc, &insert_at, "\n");
    let changed = !tx.is_noop();
    let selection_after = tx.map_selection(&insert_at, after);
    daemon.editor.apply(tx, selection_after);
    let mode_changed = daemon.editor.mode() != mina_view::Mode::Insert;
    enter_insert(daemon, conn_id);
    daemon.editor.scroll_to_cursor(daemon.viewport_height);
    (snapshot(daemon, None), changed || mode_changed)
}

/// [`Command::Search`] の適用: クエリを保存し、カーソル位置から（一致が
/// なければ文書端を越えて折り返して）最初の一致を選択する。大小文字は
/// smart-case（クエリに大文字が含まれれば区別）。一致が無ければ状態は変えず
/// status で報告する。
fn search_with(
    daemon: &mut Daemon,
    query: &str,
    direction: mina_protocol::Direction,
) -> (bool, Option<String>) {
    if query.is_empty() {
        return (false, Some("empty search".into()));
    }
    let doc = daemon.editor.current_document();
    let head = daemon.editor.selection().primary().head();
    let len = doc.len_chars();
    let case = mina_text::CaseSensitivity::Smart;
    let found = match direction {
        mina_protocol::Direction::Forward => find_next(doc, query, head, case)
            .or_else(|| find_next(doc, query, 0, case)),
        mina_protocol::Direction::Backward => find_prev(doc, query, head, case)
            .or_else(|| find_prev(doc, query, len, case)),
    };
    match found {
        Some(r) => {
            daemon.last_search = Some(SearchState {
                query: query.to_string(),
                found: (r.start(), r.end()),
            });
            daemon.editor.set_selection(mina_text::Selection::new(
                vec![CoreRange::new(r.start(), r.end())],
                0,
            ));
            (true, None)
        }
        None => {
            daemon.last_search = None;
            (false, Some(format!("no match: {query}")))
        }
    }
}

/// [`Command::SearchNext`] の適用: 保存済みの検索結果から1つ進める（`n`/`N`）。
/// 文書端を越えたら折り返す。検索履歴が無い・これ以上一致が無い場合は no-op
/// で status を載せる。
fn search_next(
    daemon: &mut Daemon,
    direction: mina_protocol::Direction,
) -> (bool, Option<String>) {
    let Some(state) = daemon.last_search.clone() else {
        return (false, Some("no previous search".into()));
    };
    let doc = daemon.editor.current_document();
    let len = doc.len_chars();
    let case = mina_text::CaseSensitivity::Smart;
    let (s, e) = state.found;
    let found = match direction {
        mina_protocol::Direction::Forward => {
            // 現在の一致の直後から。無ければ先頭から折り返し（現在位置と同じ
            // 一致に戻るのは避ける）
            find_next(doc, &state.query, e, case).or_else(|| {
                find_next(doc, &state.query, 0, case)
                    .filter(|r| r.start() != s || r.end() != e)
            })
        }
        mina_protocol::Direction::Backward => {
            // 現在の一致の手前から（現在の一致自身は除外。無ければ末尾から
            // 折り返し — それも現在位置と同じ一致なら進めない）
            find_prev(doc, &state.query, s.saturating_sub(1), case)
                .filter(|r| r.start() != s || r.end() != e)
                .or_else(|| {
                    find_prev(doc, &state.query, len, case)
                        .filter(|r| r.start() != s || r.end() != e)
                })
        }
    };
    match found {
        Some(r) => {
            daemon.last_search = Some(SearchState {
                query: state.query,
                found: (r.start(), r.end()),
            });
            daemon.editor.set_selection(mina_text::Selection::new(
                vec![CoreRange::new(r.start(), r.end())],
                0,
            ));
            (true, None)
        }
        None => (false, Some(format!("no more matches: {}", state.query))),
    }
}

/// [`Command::SearchSelection`] の適用（Helix の `*`）: 選択テキスト
/// （カーソルならその位置の単語）の全一致を複数カーソルとして選択する。
/// 検索対象のテキストが取れなければ no-op で status を載せる。
fn search_selection(daemon: &mut Daemon) -> (bool, Option<String>) {
    let doc = daemon.editor.current_document();
    let selection = daemon.editor.selection();
    // 選択テキスト（カーソルならカーソル位置の単語 — 複数カーソルの場合は
    // primary のみ。ponytail: 全カーソルの単語を集めるのは必要になってから）
    let target: String = {
        let r = selection.primary();
        if r.is_cursor() {
            match word_at(doc, r.head()) {
                Some((s, e)) => doc
                    .text()
                    .to_string()
                    .chars()
                    .skip(s)
                    .take(e - s)
                    .collect(),
                None => return (false, Some("no word under cursor".into())),
            }
        } else {
            doc.text().to_string().chars().skip(r.start()).take(r.end() - r.start()).collect()
        }
    };
    let query = target.trim();
    if query.is_empty() {
        return (false, Some("no selection to search".into()));
    }
    match find_matches(doc, query, mina_text::CaseSensitivity::Smart) {
        Some(matches) => {
            daemon.last_search = Some(SearchState {
                query: query.to_string(),
                found: (matches.primary().start(), matches.primary().end()),
            });
            daemon.editor.set_selection(matches);
            (true, None)
        }
        None => (false, Some(format!("no match: {query}"))),
    }
}

/// 接続 ID 付きでコマンドを状態に適用する（接続ハンドラから呼ばれる）。
/// `conn_id` は undo グループの所有者判定に使う。
///
/// 戻り値の `bool` は「状態を実際に変えたか」（ADR-0012: 拒否・no-op は
/// 世代/イベントの対象外。呼び出し側はこれで record_event をゲートする）。
fn apply_from(daemon: &mut Daemon, command: Command, conn_id: u64) -> (StateSnapshot, bool) {
    // ADR-0037: コマンドは発信接続の View（未登録 = テスト等は idle view）に
    // 作用させる。undo グループの所有者判定（conn_id）は従来どおり接続スコープ。
    daemon.focus_for(conn_id);
    match command {
        Command::WaitFor { .. } => {
            // process_command の専用アームで処理される（読み取り専用）。
            // ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::GetInlayHints { .. } => {
            // handle_connection で専用処理される（ServerMessage::Hints 応答。
            // ADR-0020）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::PeekDefinition => {
            // handle_connection で専用処理される（peek フィールド付き応答）。
            // ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::PeekDefinitionAt { .. } => {
            // handle_connection で専用処理される（ServerMessage::Peek 応答。
            // ADR-0025）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::Rename { .. } | Command::References { .. } => {
            // handle_connection で専用処理される（ServerMessage::RenameResult /
            // ReferencesResult 応答。ADR-0029）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::RegisterBaseRoot { .. } | Command::UnregisterBaseRoot { .. } => {
            // process_command で専用処理される（#49）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::AddReviewComment { .. } | Command::ClearReviewComments => {
            // process_command で専用処理される（#50）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::ListReviewComments => {
            // handle_connection で専用処理される（ServerMessage::ReviewComments
            // 応答。#50）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::Outline { .. } | Command::EnclosingSymbol { .. } => {
            // handle_connection で専用処理される（ServerMessage::Outline /
            // EnclosingSymbol 応答。ADR-0031）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::OutlineRecursive { .. } => {
            // handle_connection で専用処理される（ServerMessage::Outline 応答。
            // ADR-0049）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::ReadPath { .. } => {
            // handle_connection で専用処理される（ServerMessage::ReadPath 応答。
            // ADR-0048）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::HoverAt { .. } | Command::WorkspaceSymbol { .. } | Command::CheckDiagnostics { .. } => {
            // handle_connection で専用処理される（ServerMessage::Hover /
            // WorkspaceSymbols / Check 応答。ADR-0032）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::GetServerInfo => {
            // handle_connection で専用処理される（ServerMessage::ServerInfo 応答。
            // issue #27）。ここに来ることはないが網羅性のため。
            (snapshot(daemon, None), false)
        }
        Command::Insert { text } => {
            // SEC-1/ADR-0008: Insert による無制限の文書成長を防ぐ。Open と同じ
            // MAX_FILE_SIZE（バイト数）を超える挿入は状態を変えず status で
            // 拒否する（read_open_target の「status で報告して状態を触らない」
            // 流儀に合わせる）。preempt より前に判定するので undo グループの
            // 開閉（SetMode の group 管理）には一切干渉しない。
            let doc_bytes = daemon.editor.current_document().text().len_bytes() as u64;
            if doc_bytes + text.len() as u64 > MAX_FILE_SIZE {
                return (
                    snapshot(daemon, Some("file too large: insert rejected".into())),
                    false,
                );
            }
            preempt(daemon, conn_id);
            let selection = daemon.editor.selection();
            let tx = Transaction::insert(daemon.editor.current_document(), &selection, &text);
            // 空文字挿入は状態を変えない（apply 側で no-op トランザクションは
            // スキップされる）。選択範囲への空文字挿入は置換として実変更。
            let changed = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, true);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::DeleteBackward => {
            preempt(daemon, conn_id);
            let selection = daemon.editor.selection();
            let tx = mina_text::delete_backward_transaction(
                daemon.editor.current_document(),
                &selection,
            );
            // 文書先頭での Backspace 等は no-op（状態を変えない）
            let changed = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::DeleteWordBackward => {
            preempt(daemon, conn_id);
            let selection = daemon.editor.selection();
            let tx = mina_text::delete_word_backward_transaction(
                daemon.editor.current_document(),
                &selection,
            );
            // 文書先頭での単語削除は no-op
            let changed = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::DeleteForward => {
            preempt(daemon, conn_id);
            let selection = daemon.editor.selection();
            let tx = mina_text::delete_forward_transaction(
                daemon.editor.current_document(),
                &selection,
            );
            // 文末での Delete 等は no-op（状態を変えない）
            let changed = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::DeleteWordForward => {
            preempt(daemon, conn_id);
            let selection = daemon.editor.selection();
            let tx = mina_text::delete_word_forward_transaction(
                daemon.editor.current_document(),
                &selection,
            );
            // 文末での単語削除は no-op
            let changed = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::DeleteRange => {
            preempt(daemon, conn_id);
            let selection = daemon.editor.selection();
            let tx = Transaction::delete(daemon.editor.current_document(), &selection);
            // カーソル上の DeleteRange は no-op（状態を変えない）
            let changed = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            // 選択を消したら Select モードを抜ける（vim の d と同様）
            if daemon.editor.mode() == mina_view::Mode::Select {
                daemon.editor.set_mode(mina_view::Mode::Normal);
            }
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::Change => {
            // 選択（またはカーソル位置）を削除して Insert モードへ（Helix の `c`）。
            // 削除はグループの外に積み、以後の入力は SetMode(Insert) と同じく
            // 新規グループになる（ADR-0007。undo 1回で削除だけが戻る = Helix と同様）。
            preempt(daemon, conn_id);
            let selection = daemon.editor.selection();
            let tx = Transaction::delete(daemon.editor.current_document(), &selection);
            let deleted = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            let was_insert = daemon.editor.mode() == mina_view::Mode::Insert;
            if !was_insert {
                daemon.editor.begin_group();
                daemon.insert_owner = Some(conn_id);
            }
            daemon.editor.set_mode(mina_view::Mode::Insert);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), deleted || !was_insert)
        }
        Command::InsertAtLineEnd => insert_at_line(daemon, conn_id, LinePos::End),
        Command::InsertAtLineStart => {
            insert_at_line(daemon, conn_id, LinePos::FirstNonWhitespace)
        }
        Command::Undo => {
            // Undo/Redo も書き込みとして扱う（単一の共有履歴・グローバル undo）
            preempt(daemon, conn_id);
            // 履歴が無い undo は no-op（no-op トランザクションが履歴に積まれない
            // ため、can_undo が真なら undo は必ず実変更を戻す）
            let changed = daemon.editor.can_undo();
            daemon.editor.undo();
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::Redo => {
            preempt(daemon, conn_id);
            let changed = daemon.editor.can_redo();
            daemon.editor.redo();
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::Move {
            movement,
            direction,
        } => {
            let moved = {
                let doc = daemon.editor.current_document();
                let selection = daemon.editor.selection();
                match convert_movement(movement) {
                    // Helix 流: 単語移動（w/b/e）は anchor を保持して「移動した分」を
                    // 選択する。h/l/j/k や矢印は従来どおり点に潰す。
                    mina_text::Movement::Word | mina_text::Movement::WordEnd => {
                        mina_text::word_move_selection(
                            doc,
                            &selection,
                            word_move_target(movement, direction),
                        )
                    }
                    m => move_selection(doc, &selection, m, convert_direction(direction)),
                }
            };
            daemon.editor.set_selection(moved);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), false)
        }
        Command::Extend {
            movement,
            direction,
        } => {
            let extended = {
                let doc = daemon.editor.current_document();
                let selection = daemon.editor.selection();
                extend_selection(doc, &selection, convert_movement(movement), convert_direction(direction))
            };
            daemon.editor.set_selection(extended);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), false)
        }
        Command::SelectLine => {
            let selected = {
                let doc = daemon.editor.current_document();
                let selection = daemon.editor.selection();
                select_line_selection(doc, &selection)
            };
            daemon.editor.set_selection(selected);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), false)
        }
        Command::ExtendLineBelow => {
            let extended = {
                let doc = daemon.editor.current_document();
                let selection = daemon.editor.selection();
                extend_line_below(doc, &selection)
            };
            daemon.editor.set_selection(extended);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), false)
        }
        Command::SelectAll => {
            let len = daemon.editor.current_document().len_chars();
            daemon.editor
                .set_selection(mina_text::Selection::new(vec![CoreRange::new(0, len)], 0));
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), false)
        }
        Command::Append => {
            // `a`: 選択の末尾（カーソルは直後の1文字）へ点に潰して Insert モードへ
            // （Helix の append_mode。InsertAtLineEnd/Start と同じグループ管理）。
            let moved = append_selection(daemon.editor.current_document(), &daemon.editor.selection());
            daemon.editor.set_selection(moved);
            let changed = daemon.editor.mode() != mina_view::Mode::Insert;
            enter_insert(daemon, conn_id);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::OpenBelow => open_line(daemon, conn_id, LinePos::Below),
        Command::OpenAbove => open_line(daemon, conn_id, LinePos::Above),
        Command::Replace { text } => {
            // `r`: 選択（カーソルはその位置の1文字）を置換し Normal のまま
            // 置換後テキストの直後にカーソルを置く。1 undo グループ（replace_ranges）。
            preempt(daemon, conn_id);
            if text.is_empty() {
                return (snapshot(daemon, None), false);
            }
            let doc = daemon.editor.current_document();
            let selection = daemon.editor.selection();
            let edits: Vec<(usize, usize, String)> = replace_targets(doc, &selection)
                .into_iter()
                .map(|(start, end)| (start, end, text.clone()))
                .collect();
            let tx = Transaction::replace_ranges(doc, &edits);
            let changed = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, true);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::SurroundAdd { ch } => {
            // Helix の `ms`: 選択（カーソルはその位置に空ペア）を ch の対応
            // ペアで囲む。適用後は囲んだ全体を選択し、Select モードを抜ける。
            preempt(daemon, conn_id);
            let (tx, selection_after) = {
                let doc = daemon.editor.current_document();
                let selection = daemon.editor.selection();
                mina_text::surround_add(doc, &selection, ch)
            };
            let changed = !tx.is_noop();
            daemon.editor.apply(tx, selection_after);
            exit_select_mode(daemon);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::SurroundDelete { ch } => {
            // Helix の `md`: カーソルを囲む ch のペアを両側とも削除。ペアが
            // 無ければ状態を変えず status で報告する（Helix の PairNotFound）。
            preempt(daemon, conn_id);
            let result: Option<(Transaction, Selection)> = {
                let doc = daemon.editor.current_document();
                let selection = daemon.editor.selection();
                mina_text::surround_delete(doc, &selection, ch)
            };
            let Some((tx, selection_after)) = result else {
                return (snapshot(daemon, Some("surround pair not found".into())), false);
            };
            let changed = !tx.is_noop();
            daemon.editor.apply(tx, selection_after);
            exit_select_mode(daemon);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::SurroundReplace { from, to } => {
            // Helix の `mr`: from の対応ペアを to の対応ペアに置き換える。
            preempt(daemon, conn_id);
            let result: Option<(Transaction, Selection)> = {
                let doc = daemon.editor.current_document();
                let selection = daemon.editor.selection();
                mina_text::surround_replace(doc, &selection, from, to)
            };
            let Some((tx, selection_after)) = result else {
                return (snapshot(daemon, Some("surround pair not found".into())), false);
            };
            let changed = !tx.is_noop();
            daemon.editor.apply(tx, selection_after);
            exit_select_mode(daemon);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::KillToLineStart => {
            preempt(daemon, conn_id);
            let doc = daemon.editor.current_document();
            let selection = daemon.editor.selection();
            let text = doc.text().to_string();
            let ranges: Vec<CoreRange> = selection
                .ranges()
                .iter()
                .map(|r| CoreRange::new(line_start_of(&text, r.head()), r.end()))
                .collect();
            let kill = mina_text::Selection::new(ranges, selection.primary_index());
            let tx = Transaction::delete(doc, &kill);
            let changed = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::KillToLineEnd => {
            preempt(daemon, conn_id);
            let doc = daemon.editor.current_document();
            let selection = daemon.editor.selection();
            let text = doc.text().to_string();
            let ranges: Vec<CoreRange> = selection
                .ranges()
                .iter()
                .map(|r| CoreRange::new(r.start(), line_end_of(&text, r.head())))
                .collect();
            let kill = mina_text::Selection::new(ranges, selection.primary_index());
            let tx = Transaction::delete(doc, &kill);
            let changed = !tx.is_noop();
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), changed)
        }
        Command::Search { query, direction } => {
            let (changed, status) = search_with(daemon, &query, direction);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, status), changed)
        }
        Command::SearchNext { direction } => {
            let (changed, status) = search_next(daemon, direction);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, status), changed)
        }
        Command::SearchSelection => {
            let (changed, status) = search_selection(daemon);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, status), changed)
        }
        Command::Goto { target } => {
            let pos = match target {
                GotoTarget::DocumentStart => 0,
                GotoTarget::DocumentEnd => daemon.editor.current_document().len_chars(),
            };
            // Select モードでは移動でなく選択の拡張（Helix の select モードは
            // 移動系キーが extend になる — `gg`/`G` も同様）。Normal では点に潰す。
            let moved = if daemon.editor.mode() == mina_view::Mode::Select {
                extend_to(daemon.editor.current_document(), &daemon.editor.selection(), pos)
            } else {
                mina_text::Selection::point(pos)
            };
            daemon.editor.set_selection(moved);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), false)
        }
        Command::ScrollHalf { direction } => {
            // 半ページ（Helix の C-d/C-u）: 表示とカーソルを viewport 高さの半分だけ
            // 動かす。Scroll と同様にカーソルも連動させる（読み上げ位置が保たれる）。
            let delta = (daemon.viewport_height as isize / 2)
                .saturating_mul(if direction == mina_protocol::Direction::Forward { 1 } else { -1 });
            if delta != 0 {
                daemon.editor.scroll_lines(delta);
                let extend = daemon.editor.mode() == mina_view::Mode::Select;
                let moved = move_selection_lines(
                    daemon.editor.current_document(),
                    &daemon.editor.selection(),
                    delta,
                    extend,
                );
                daemon.editor.set_selection(moved);
            }
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            (snapshot(daemon, None), false)
        }
        Command::Scroll { pages } => {
            // 壊れた/悪意あるページ数でスクロール計算が overflow しないよう
            // clamp する（SetViewport と同様、daemon 側で防御する）
            let pages = pages.clamp(-MAX_SCROLL_PAGES, MAX_SCROLL_PAGES);
            let height = daemon.viewport_height;
            daemon.editor.scroll_pages(pages, height);
            // ADR-0023: カーソルもスクロールした行数と同じだけ動かす。Normal
            // では点に潰して移動、Select では head だけ拡張（h/j/k/l と同じ
            // モード分岐）。移動量 = スクロール量なので画面内の相対位置が保たれ、
            // カーソルは画面外に出ない（文書端では両者ともクランプされる）。
            let delta = pages.saturating_mul(height as isize);
            if delta != 0 {
                let selection = daemon.editor.selection();
                let extend = daemon.editor.mode() == mina_view::Mode::Select;
                let moved = move_selection_lines(
                    daemon.editor.current_document(),
                    &selection,
                    delta,
                    extend,
                );
                daemon.editor.set_selection(moved);
            }
            daemon.editor.scroll_to_cursor(height);
            (snapshot(daemon, None), false)
        }
        Command::SetMode { mode } => {
            let new_mode = convert_mode(mode);
            let current = daemon.editor.mode();
            // Insert モードの入力を1つの undo グループにまとめる（グループは
            // daemon 側で開閉する — 状態は daemon が持つため）。境界は「Insert
            // セッション」: Insert への進入で開き、Insert からの離脱（→Normal・
            // →Select のいずれでも）で閉じる。切断時は接続ハンドラ側で閉じる
            // （ADR-0007）。
            //
            // HIGH-1: グループは開いたクライアントが所有者。既に別クライアント
            // のセッションが開いている状態での SetMode(Insert) は後勝ちで奪取
            // する。モード自体はグローバルなので、離脱は誰が送ってもグループを
            // 閉じる（agent の SetMode は通常送られない — ponytail 参照）。
            // ponytail: ネストしたグループは考慮しない（v1 にその経路はない）。
            if new_mode == mina_view::Mode::Insert {
                // グループの開閉・所有者・奪取は enter_insert に集約
                // （InsertAtLineEnd / InsertAtLineStart と共通）。
                enter_insert(daemon, conn_id);
            } else {
                if current == mina_view::Mode::Insert {
                    close_insert_session(daemon, new_mode);
                } else {
                    daemon.editor.set_mode(new_mode);
                }
            }
            (snapshot(daemon, None), new_mode != current)
        }
        Command::SetViewport { height } => {
            // 壊れた/悪意ある高さでスクロール計算が overflow しないよう clamp
            let height = height.min(MAX_VIEWPORT_HEIGHT);
            // v12: 表示高さは接続ごとに持つ（ADR-0037）。未登録（headless・テスト
            // 等）はグローバル側だけ更新する。
            if let Some(r) = daemon.conn_views.get_mut(&conn_id) {
                r.viewport = height;
            }
            daemon.viewport_height = height;
            (snapshot(daemon, None), false)
        }
        Command::Close => {
            let closed = daemon.editor.close_focused_document();
            if closed {
                // 閉じた文書のベースラインを落とす（開いている文書の分だけ残す）
                let open: Vec<PathBuf> = daemon.editor.open_paths().cloned().collect();
                daemon.baselines.retain(|p, _| open.contains(p));
                daemon.deleted = None;
            }
            (snapshot(daemon, None), closed)
        }
        Command::GetState => {
            daemon.metrics.get_state_total += 1;
            (snapshot(daemon, None), false)
        }
        Command::Open { .. } | Command::Save => {
            unreachable!("I/O コマンドは接続ハンドラで処理される")
        }
    }
}

/// フォーカス中の View 基準のスナップショット（応答経路）。
///
/// v12 では [`snapshot_from_view`] に委譲する — コマンド適用直後はフォーカス =
/// 発信接続の View（[`Daemon::focus_for`]）。
pub(crate) fn snapshot(daemon: &mut Daemon, status: Option<String>) -> StateSnapshot {
    snapshot_from_view(
        daemon,
        daemon.editor.focused_view_id(),
        daemon.viewport_height,
        status,
    )
}

/// 指定 View 基準のスナップショット（ADR-0037。接続別合成）。
///
/// text / checksum / diagnostics / inlay_hints / highlights / path / dirty /
/// activities は View の文書基準、selection / first_line は View の値。
/// 診断は解析フォーカス文書（`diag_path`）を見る View にのみ載せる。
/// mode はデーモン共有（undo グループの所有者と一致する — ADR-0007）。
pub(crate) fn snapshot_from_view(
    daemon: &mut Daemon,
    view_id: ViewId,
    viewport_height: usize,
    status: Option<String>,
) -> StateSnapshot {
    let view = daemon.editor.view_by_id(view_id).clone();
    let doc_id = view.doc;
    let path = daemon
        .editor
        .path_for_doc(doc_id)
        .map(|p| p.to_path_buf());
    let text = daemon.editor.document(doc_id).text().to_string();
    let checksum = fnv1a64(text.as_bytes());
    let highlights =
        daemon.syntax_highlights(doc_id, &text, checksum, view.first_line, viewport_height);
    StateSnapshot {
        activity: daemon.activity_log.iter().cloned().collect(),
        // ADR-0012 #12: 全文の FNV-1a を同梱し、エージェントが edit の
        // checksum を再実装せずに済ませる。応答は既に全文をシリアライズ
        // するため、ハッシュ計算は相対的に無視できるコスト。
        checksum,
        text,
        selection: view
            .selection
            .ranges()
            .iter()
            .map(|r| Range {
                anchor: r.anchor(),
                head: r.head(),
            })
            .collect(),
        primary_index: view.selection.primary_index(),
        mode: convert_mode_back(daemon.editor.mode()),
        first_line: view.first_line,
        // v12: 診断・inlay は解析フォーカス文書（= 自分のセッションが最後に
        // 解析した文書）を見る View のスナップショットにのみ載せる（ADR-0037）。
        diagnostics: path
            .as_deref()
            .map(|p| daemon.focus_diagnostics(p))
            .unwrap_or_default(),
        // ADR-0020 + v12: ヒントは解析フォーカス文書のキャッシュだけを載せる
        // （キャッシュは GetInlayHints の全パス応答とも共用）。checksum 不一致
        // （編集中）でも載せる — Q7: stale ヒントは新ヒント到着まで保持する。
        inlay_hints: path
            .as_deref()
            .map(|p| daemon.focus_inlay_hints(p))
            .unwrap_or_default(),
        // 不変条件: 同じスナップショットのテキストと一致する範囲（ADR-0016）。
        highlights,
        path: path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        dirty: daemon.editor.is_doc_dirty(doc_id),
        status,
        // ADR-0028: View の文書の活動だけが載る（増減で generation を進める）。
        activities: path
            .as_deref()
            .and_then(|p| daemon.activities.get(p))
            .cloned()
            .unwrap_or_default(),
        generation: daemon.generation,
        events: daemon.events.iter().cloned().collect(),
        deleted: daemon.deleted.clone(),
        peek: None, // PeekDefinition 応答は serve_peek_definition が上書きする
        // #50: 件数のみ（全文は ListReviewComments で別取得）。
        review_comment_count: daemon.review_comments.len(),
        // #52: 基準診断（キャッシュ済み対応物のみ。未取得は后台充填が
        // push で届ける — generation は進めない）。
        base_diagnostics: path
            .as_deref()
            .map(|p| daemon.snapshot_base_diagnostics(Some(p)))
            .unwrap_or_default(),
    }
}

fn convert_movement(m: mina_protocol::Movement) -> mina_text::Movement {
    match m {
        mina_protocol::Movement::Char => mina_text::Movement::Char,
        mina_protocol::Movement::Line => mina_text::Movement::Line,
        mina_protocol::Movement::Word => mina_text::Movement::Word,
        mina_protocol::Movement::WordEnd => mina_text::Movement::WordEnd,
        mina_protocol::Movement::LineStart => mina_text::Movement::LineStart,
        mina_protocol::Movement::LineEnd => mina_text::Movement::LineEnd,
        mina_protocol::Movement::FirstNonWhitespace => mina_text::Movement::FirstNonWhitespace,
    }
}

/// 単語移動の wire 型（Movement + Direction）をコアの目標型へ変換する。
fn word_move_target(
    m: mina_protocol::Movement,
    d: mina_protocol::Direction,
) -> mina_text::WordMoveTarget {
    match (m, d) {
        (
            mina_protocol::Movement::Word,
            mina_protocol::Direction::Forward,
        ) => mina_text::WordMoveTarget::NextWordStart,
        (
            mina_protocol::Movement::Word,
            mina_protocol::Direction::Backward,
        ) => mina_text::WordMoveTarget::PrevWordStart,
        (
            mina_protocol::Movement::WordEnd,
            mina_protocol::Direction::Forward,
        ) => mina_text::WordMoveTarget::NextWordEnd,
        (
            mina_protocol::Movement::WordEnd,
            mina_protocol::Direction::Backward,
        ) => mina_text::WordMoveTarget::PrevWordEnd,
        _ => unreachable!("単語移動以外の movement はここに来ない"),
    }
}

fn convert_direction(d: mina_protocol::Direction) -> mina_text::Direction {
    match d {
        mina_protocol::Direction::Forward => mina_text::Direction::Forward,
        mina_protocol::Direction::Backward => mina_text::Direction::Backward,
    }
}

fn convert_mode(m: mina_protocol::Mode) -> mina_view::Mode {
    match m {
        mina_protocol::Mode::Normal => mina_view::Mode::Normal,
        mina_protocol::Mode::Insert => mina_view::Mode::Insert,
        mina_protocol::Mode::Select => mina_view::Mode::Select,
    }
}

fn convert_mode_back(m: mina_view::Mode) -> mina_protocol::Mode {
    match m {
        mina_view::Mode::Normal => mina_protocol::Mode::Normal,
        mina_view::Mode::Insert => mina_protocol::Mode::Insert,
        mina_view::Mode::Select => mina_protocol::Mode::Select,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use mina_protocol::{Direction, GotoTarget, HighlightGroup, Mode, Movement};

    #[test]
    fn line_and_col_of_char_are_1_origin() {
        // ADR-0047: 診断の char 位置 → 行番号(0-origin)と行内列(1-origin)が
        // at/peek/hover の `line:col` 住所にそのまま使えることを検証する。
        let text = "fn a() {}\nfn b() { TODO }";
        // "TODO" は 2 行目（0-origin 1）・行頭から 10 char 目（1-origin 10）。
        let start = char_idx_of(text, "TODO");
        assert_eq!(line_of_char(text, start), 1, "0-origin 行番号");
        assert_eq!(col_of_char(text, start), 10, "1-origin 行内列");
        // --lines の住所は 1-origin 行番号（line + 1）
        assert_eq!(line_of_char(text, start) + 1, 2);

        // 文頭は行頭（列 1）
        assert_eq!(col_of_char(text, 0), 1);
        // 文末超過はクランプ（最終行「fn b() { TODO }」= 15 chars の末尾の次 = 16）
        assert_eq!(col_of_char(text, text.len() + 5), 16);
        // 改行の直後は次の行の行頭（列 1）
        let with_nl = "ab\ncd";
        assert_eq!(col_of_char(with_nl, 3), 1); // 'c' は 2 行目 1 列目
        assert_eq!(col_of_char(with_nl, 4), 2); // 'd' は 2 行目 2 列目
    }

    #[test]
    fn col_of_char_counts_chars_not_bytes() {
        // マルチバイト（CJK）も char 単位で数える（start は char インデックス）。
        let text = "あいうTODO";
        let start = char_idx_of(text, "TODO");
        assert_eq!(start, 3, "byte 位置ではなく char 位置");
        assert_eq!(col_of_char(text, start), 4);
    }

    /// byte 位置（`str::find`）を char インデックスに変換するテストヘルパー。
    fn char_idx_of(text: &str, needle: &str) -> usize {
        text[..text.find(needle).unwrap()].chars().count()
    }

    // lsp.rs から移設（daemon 統合の ensure / drain_into を直接検証する）
    #[tokio::test]
    async fn ensure_replaces_dead_session() {
        // M3/ADR-0009: サーバが死んだら次の .rs Open で再 spawn される。
        // 死亡セッションを返し続けず、新しいセッションに置き換わることを検証する。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let daemon = Arc::new(Mutex::new(Daemon::new()));
        let session = lsp::LspSession::new(bin, Path::new("/tmp"))
            .await
            .expect("initialize");
        let dead = Arc::new(Mutex::new(session));
        let root = daemon.lock().await.languages.workspace_root(Path::new("/tmp/x.rs"));
        // セッションキーは (root, languageId)（ADR-0030 Stage 4）。テスト用コンストラクタ
        // は言語 "rust" なのでキーも "rust" で一致させる。
        daemon
            .lock()
            .await
            .lsp_sessions
            .insert((root, "rust".to_string()), dead.clone());

        // サーバを殺して is_dead になるまで待つ（reader タスクが EOF を拾う）
        dead.lock().await.client.kill().await;
        let mut is_dead = false;
        for _ in 0..100 {
            if dead.lock().await.client.is_dead() {
                is_dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(is_dead, "サーバを殺すと is_dead になる");

        let replaced = ensure(&daemon, Path::new("/tmp/x.rs"))
            .await
            .expect("再 spawn できる");
        assert!(
            !Arc::ptr_eq(&dead, &replaced),
            "新しいセッションで置き換わる"
        );
        assert!(
            !replaced.lock().await.client.is_dead(),
            "返るセッションは生きている"
        );
    }

    #[test]
    fn languages_refresh_picks_up_newly_added_language() {
        // 敵対的検証で発見した欠陥の回帰テスト: ゲートが古いテーブルを見ると
        // 「languages.toml に新言語を足しても daemon 再起動まで反映されない」。
        // mtime 差分再読込により、次のゲート/spawn で新言語が認識されることを
        // 検証する（ADR-0030 の spawn 時再読込の約束）。
        let dir = std::env::temp_dir().join(format!(
            "minae-langs-refresh-{}",
            std::process::id()
        ));
        let xdg = dir.join("xdg");
        std::fs::create_dir_all(xdg.join("minae")).expect("tmp dirs");
        let path = xdg.join("minae/languages.toml");
        // XDG_CONFIG_HOME を差し替えて daemon を構築（既定は rust のみ）
        let old = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        std::fs::write(&path, "").expect("write");
        let mut daemon = Daemon::new();
        let table = daemon.languages_refresh();
        // 組み込み既定には python は無い（typescript は Stage 4 で既定化済みのため
        // 「新規追加」の題材に使えない）→ python を追加して反映を検証する
        assert!(
            table.server_for(Path::new("/tmp/x.py")).is_none(),
            "初期状態では py 非対応"
        );
        // ユーザーが言語を追加（mtime を確実に変えるため少し待つ）
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &path,
            r#"
[language-server.pyright]
command = "pyright-langserver"

[[language]]
name = "python"
file-types = ["py"]
language-server = "pyright"
"#,
        )
        .expect("write");
        // 再起動なし・次回のゲート相当の refresh で反映される
        let table = daemon.languages_refresh();
        let spec = table
            .server_for(Path::new("/tmp/x.py"))
            .expect("追加した言語が次の refresh で認識される");
        assert_eq!(spec.language_id, "python");
        assert_eq!(spec.command, "pyright-langserver");
        // 後始末（env 復元 + 一時ディレクトリ削除）
        match old {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_keys_split_mixed_language_same_root() {
    // Stage 4: 同一 root に .rs と .ts が混在してもセッションキー (root, languageId) で
    // 分割される。session_for / session_root_for は同言語キーのみ対象（.rs のセッションに
    // .ts を乗せない・逆も然り）。
    let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
    if !std::path::Path::new(bin).exists() {
        eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
        return;
    }
    let dir = std::env::temp_dir().join(format!("minae-mixed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let rs = dir.join("a.rs");
    let ts = dir.join("a.ts");
    std::fs::write(&rs, "fn f() {}").expect("write");
    std::fs::write(&ts, "export function f() {}").expect("write");
    let canon_rs = std::fs::canonicalize(&rs).unwrap();
    let canon_ts = std::fs::canonicalize(&ts).unwrap();
    let mut daemon = Daemon::new();
    let root = daemon.languages.workspace_root(&canon_rs);
    assert_eq!(root, daemon.languages.workspace_root(&canon_ts), "同一 root の前提");
    let session_rs = Arc::new(Mutex::new(
        lsp::LspSession::new(bin, &root).await.expect("initialize"),
    ));
    let session_ts = Arc::new(Mutex::new(
        lsp::LspSession::new_with_config(
            bin, &root, &["--bare".to_string()], "typescript", None,
        )
        .await
        .expect("initialize"),
    ));
    daemon
        .lsp_sessions
        .insert((root.clone(), "rust".to_string()), session_rs.clone());
    daemon
        .lsp_sessions
        .insert((root.clone(), "typescript".to_string()), session_ts.clone());
    // 同 root でも言語ごとに別セッション
    let got_rs = daemon.session_for(&canon_rs).expect("rs セッション");
    let got_ts = daemon.session_for(&canon_ts).expect("ts セッション");
    assert!(Arc::ptr_eq(&got_rs, &session_rs));
    assert!(Arc::ptr_eq(&got_ts, &session_ts));
    assert!(!Arc::ptr_eq(&got_rs, &got_ts), "言語ごとに別セッション");
}

    #[tokio::test]
    async fn session_lookup_survives_root_marker_edit() {
        // 敵対的検証 P1 の回帰テスト: languages.toml の root-markers を稼働中に
        // 編集すると fresh root が変わるが、セッションは spawn 時のキーのまま。
        // session_root_for の prefix フォールバックで同じセッションを見失わない
        // （見失うと同期スキップ・診断消失・重複 spawn になる）。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let dir = std::env::temp_dir().join(format!("minae-langs-drift-{}", std::process::id()));
        let xdg = dir.join("xdg");
        std::fs::create_dir_all(xdg.join("minae")).expect("tmp dirs");
        let cfg = xdg.join("minae/languages.toml");
        let old = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        // 初期: text は root-markers なし → 汎用集合（tmp 内にマーカーなし → 親フォールバック）。
        // session_key が動くよう language-server 参照を持たせる（サーバ実体は使わない —
        // セッションは直接挿入する）。
        std::fs::write(
            &cfg,
            r#"
[language-server.mock]
command = "x"

[[language]]
name = "text"
file-types = ["txt"]
language-server = "mock"
"#,
        )
        .expect("write");
        let mut daemon = Daemon::new();
        let file = dir.join("proj").join("docs").join("memo.txt");
        std::fs::create_dir_all(file.parent().unwrap()).expect("dirs");
        std::fs::write(&file, "hi").expect("file");
        let old_root = daemon.languages.workspace_root(&file); // 旧テーブル: docs
        let session = Arc::new(Mutex::new(
            lsp::LspSession::new(bin, Path::new("/tmp")).await.expect("initialize"),
        ));
        // キーは (root, languageId) — file の言語は text
        daemon
            .lsp_sessions
            .insert((old_root.clone(), "text".to_string()), session.clone());
        // root-markers を追加（proj 直下に .docsroot）→ fresh root は proj に変わる
        std::thread::sleep(std::time::Duration::from_millis(20)); // mtime 分解能
        std::fs::write(dir.join("proj").join(".docsroot"), "").expect("marker");
        std::fs::write(
            &cfg,
            r#"
[language-server.mock]
command = "x"

[[language]]
name = "text"
file-types = ["txt"]
language-server = "mock"
root-markers = [".docsroot"]
"#,
        )
        .expect("write");
        let _ = daemon.languages_refresh(); // ゲート相当: キャッシュが fresh に
        let fresh = daemon.languages.workspace_root(&file);
        assert_ne!(fresh, old_root, "root-markers 編集で root が変わる前提");
        assert!(!daemon.lsp_sessions.contains_key(&(fresh, "text".to_string())));
        // 見失わない: prefix フォールバックが旧キー（docs, text）を返す
        let found = daemon
            .session_root_for(&file)
            .expect("稼働中セッションを見失わない");
        assert_eq!(found, (old_root, "text".to_string()), "旧キーにフォールバックする");
        assert!(Arc::ptr_eq(&daemon.lsp_sessions[&found], &session));
        // 後始末
        match old {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn capabilities_gate_features_for_bare_server() {
        // Stage 3 の負の経路: 能力を advertise しないサーバ（--bare の mock）には
        // 機能要求が出ず、rename / references は「not supported」（exit 1 相当）を
        // 即返し、peek は空になる。
        //
        // 実装上の注意: env（MINA_LSP_*）や XDG を変えると並列テストと競合するため、
        // bare なセッションを直接 spawn して daemon に注入する。prepare_borrowed_session
        // は ensure の再利用（root に生きたセッション）でこのセッションを掴む。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let dir = std::env::temp_dir().join(format!("minae-caps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        // 失敗時も litter を残さない（temp 直下の .rs が他テストの workspace 走査に
        // 混入してフレークの原因になる — 敵対的検証で発見）
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let path = dir.join("x.rs");
        std::fs::write(&path, "fn foo() {}").expect("write");
        // prepare_borrowed_session は normalize_open_path（canonicalize）するため、
        // root キーも正準化後のパスから計算する（/var → /private/var を取り違えると
        // ensure の再利用が外れて実サーバが spawn される）
        let canon = std::fs::canonicalize(&path).expect("canonicalize");
        let path_str = canon.to_string_lossy().into_owned();
        let daemon = Arc::new(Mutex::new(Daemon::new()));
        let root = daemon.lock().await.languages.workspace_root(&canon);
        let session = Arc::new(Mutex::new(
            lsp::LspSession::new_with_config(bin, &root, &["--bare".to_string()], "rust", None)
                .await
                .expect("bare initialize"),
        ));
        let cap_bare = {
            let s = session.lock().await;
            !s.caps.rename
                && !s.caps.references
                && !s.caps.definition
                && !s.caps.inlay_hints
                && !s.caps.pull_diagnostics
        };
        assert!(cap_bare, "--bare は何も advertise しない");
        daemon
            .lock()
            .await
            .lsp_sessions
            .insert((root, "rust".to_string()), session);
        match serve_rename(&daemon, &path_str, "foo", "bar").await {
            ServerMessage::RenameResult { error: Some(e), .. } => {
                assert!(e.starts_with("rename not supported"), "{e}")
            }
            other => panic!("rename は not supported 応答のはず: {other:?}"),
        }
        match serve_references(&daemon, &path_str, "foo").await {
            ServerMessage::ReferencesResult { error: Some(e), .. } => {
                assert!(e.starts_with("references not supported"), "{e}")
            }
            other => panic!("references は not supported 応答のはず: {other:?}"),
        }
        match serve_peek_definition_at(&daemon, &path_str, 1, 1).await {
            ServerMessage::Peek { text, .. } => assert!(text.is_empty(), "peek は空のはず: {text}"),
            other => panic!("peek は空応答のはず: {other:?}"),
        }
        // 注入した bare セッションが借用され復元されたか（借用フローが壊れていないこと）
        // 注意: 1 文に 2 つの daemon.lock() を書くと同一タスクで自己デッドロックする
        // （非再入 tokio Mutex。一時ガードは文末まで生存）ため、ガードを block で区切る。
        let alive = {
            let d = daemon.lock().await;
            let root = d.languages.workspace_root(&canon);
            d.lsp_sessions.contains_key(&(root, "rust".to_string()))
        };
        assert!(alive, "bare セッションは残っている");
    }

    #[tokio::test]
    async fn drain_into_clears_stale_diagnostics_when_server_dies() {
        // MEDIUM-3: サーバが死んだ後も古い診断（下線・カウント）が残り続けない。
        // 死んだ時点でクリアし、再 spawn 後の Open で新しく載る。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let mut daemon = Daemon::new();
        let path = PathBuf::from("/tmp/x.rs");
        daemon
            .editor
            .open_with_path(path.clone(), "fn main() { TODO }");
        let session = Arc::new(Mutex::new(
            lsp::LspSession::new(bin, Path::new("/tmp")).await.expect("initialize"),
        ));
        daemon
            .lsp_sessions
            .insert(
                (daemon.languages.workspace_root(&path), "rust".to_string()),
                session.clone(),
            );

        // pull で TODO 診断を取り込んでから殺す（MEDIUM-3 の前提: 診断がある状態）
        session
            .lock()
            .await
            .did_open(&path, "fn main() { TODO }")
            .await;
        let diags = session
            .lock()
            .await
            .pull_diagnostics(&path, "fn main() { TODO }")
            .await
            .expect("pull 診断が返る");
        daemon.set_focus_diagnostics(&path, diags);
        assert!(!daemon.focus_diagnostics(&path).is_empty(), "診断が入っている");

        // サーバを殺す → drain で古い診断がクリアされる
        session.lock().await.client.kill().await;
        let mut is_dead = false;
        for _ in 0..100 {
            if session.lock().await.client.is_dead() {
                is_dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(is_dead, "サーバを殺すと is_dead になる");
        drain_into(&mut daemon);
        assert!(
            daemon.focus_diagnostics(&path).is_empty(),
            "サーバ死亡後の古い診断は残らない"
        );
    }

    fn daemon() -> Daemon {
        Daemon::new()
    }

    fn open(d: &mut Daemon, text: &str) -> StateSnapshot {
        open_in_editor(d, "test.txt", Some(text.into()))
    }

    fn open_path(d: &mut Daemon, path: &str, text: &str) -> StateSnapshot {
        open_in_editor(d, path, Some(text.into()))
    }

    /// ハイライト範囲の不変条件（仕様書）: 昇順・重複なし・テキスト内。
    fn assert_highlights_valid(text: &str, highlights: &[HighlightRange]) {
        let n = text.chars().count();
        let mut prev_end = 0usize;
        for r in highlights {
            assert!(r.start < r.end && r.end <= n, "範囲がテキスト内: {r:?}");
            assert!(r.start >= prev_end, "昇順・重複なし: {r:?}");
            prev_end = r.end;
        }
    }

    #[test]
    fn open_loads_file_into_snapshot() {
        let mut d = daemon();
        let s = open(&mut d, "hello\nworld");
        assert_eq!(s.text, "hello\nworld");
        assert_eq!(s.path.as_deref(), Some("test.txt"));
        assert!(!s.dirty);
        assert_eq!(s.status, None);
        assert_eq!(s.selection[0], Range { anchor: 0, head: 0 });
    }

    #[test]
    fn open_failure_reports_status() {
        let mut d = daemon();
        let s = open_in_editor(&mut d, "missing", None);
        assert!(s.status.is_some());
        assert!(s.path.is_none());
    }

    #[test]
    fn rust_file_highlights_in_snapshot() {
        // ADR-0016: .rs は tree-sitter でパースされ、スナップショットに載る
        let mut d = daemon();
        let s = open_path(&mut d, "test.rs", "// comment\nfn add(a: i32) -> i32 { a + 1 }\n");
        assert_highlights_valid(&s.text, &s.highlights);
        let keyword = s
            .highlights
            .iter()
            .find(|r| &s.text[r.start..r.end] == "fn")
            .expect("fn が keyword でハイライトされる");
        assert_eq!(keyword.group, HighlightGroup::Keyword);
        let comment = s
            .highlights
            .iter()
            .find(|r| r.group == HighlightGroup::Comment)
            .expect("コメントがハイライトされる");
        assert_eq!(&s.text[comment.start..comment.end], "// comment");
    }

    #[test]
    fn activity_add_remove_bumps_generation() {
        // ADR-0028: 増減のたび generation が進む。重複追加は無視、存在しない除去は無視。
        let mut d = daemon();
        let g = d.generation;
        let a = std::path::Path::new("a.rs");
        d.add_activity(a, ActivityKind::LspInit, "LSP 初期化中");
        assert_eq!(d.generation, g + 1, "追加で世代が進む");
        d.add_activity(a, ActivityKind::Save, "保存中");
        assert_eq!(d.generation, g + 2);
        d.add_activity(a, ActivityKind::LspInit, "LSP 初期化中");
        assert_eq!(d.generation, g + 2, "同一 kind の重複追加は無視");
        d.remove_activity(a, ActivityKind::Save);
        assert_eq!(d.generation, g + 3, "除去で世代が進む");
        d.remove_activity(a, ActivityKind::Save);
        assert_eq!(d.generation, g + 3, "存在しない除去は世代を動かさない");
        d.remove_activity(a, ActivityKind::LspInit);
        assert_eq!(d.generation, g + 4);
        assert!(d.activities.is_empty(), "空になったエントリは map から消える");
    }

    #[test]
    fn snapshot_carries_focused_activities() {
        // ADR-0028: フォーカス文書の活動だけがスナップショットに載る。
        let mut d = daemon();
        let a = open_path(&mut d, "a.rs", "fn a() {}\n");
        assert!(a.activities.is_empty(), "LSP を使わないテスト経路は空: {a:?}");

        d.add_activity(
            std::path::Path::new("a.rs"),
            ActivityKind::DiagnosticsSettle,
            "診断取得中",
        );
        let s = apply(&mut d, Command::GetState);
        assert_eq!(
            s.activities.len(),
            1,
            "フォーカス文書の活動が載る: {:?}",
            s.activities
        );
        assert_eq!(s.activities[0].kind, ActivityKind::DiagnosticsSettle);

        // フォーカスが他文書へ移ると、その文書の活動だけになる
        let _ = open_path(&mut d, "b.txt", "plain text\n");
        let s = apply(&mut d, Command::GetState);
        assert!(s.activities.is_empty(), "b.txt の活動は空: {:?}", s.activities);

        d.remove_activity(
            std::path::Path::new("a.rs"),
            ActivityKind::DiagnosticsSettle,
        );
        let s = apply(&mut d, Command::GetState);
        assert!(s.activities.is_empty(), "除去後は空: {:?}", s.activities);
    }

    #[test]
    fn unknown_extension_has_no_highlights() {
        // grammar 不在の拡張子は空 Vec（ADR-0017 のフォールバックなし方針）
        let mut d = daemon();
        let s = open(&mut d, "// not a comment in txt\nfn f() {}\n");
        assert!(s.highlights.is_empty(), ".txt は空: {:?}", s.highlights);
        // スクラッチ（パスなし）も空
        let s = apply(&mut d, Command::GetState);
        assert!(s.highlights.is_empty(), "スクラッチは空: {:?}", s.highlights);
    }

    #[test]
    fn insert_edit_reparses_highlights() {
        // 編集 → スナップショットの highlights がテキストと一致（再パース）
        let mut d = daemon();
        open_path(&mut d, "test.rs", "fn f() {}\n");
        let s = apply(&mut d, Command::Insert { text: "// note\n".into() });
        assert_highlights_valid(&s.text, &s.highlights);
        assert!(
            s.highlights.iter().any(|r| r.group == HighlightGroup::Comment),
            "挿入後にコメントがハイライトされる: {:?}",
            s.highlights
        );
    }

    #[test]
    fn document_edit_reparses_highlights() {
        // headless（DocumentEdit）経路でも再パースされる（ADR-0016 の全編集源）
        let mut d = daemon();
        let before = open_path(&mut d, "test.rs", "fn f() {}\n");
        let edit = DocumentEdit {
            start: 0,
            end: 0,
            text: "// agent note\n".into(),
            checksum: before.checksum,
            expected_text: None,
        };
        assert!(apply_edit(&mut d, &edit, 0, EventSource::Interactive).is_none(), "適用に成功する");
        let s = apply(&mut d, Command::GetState);
        assert_highlights_valid(&s.text, &s.highlights);
        assert!(
            s.highlights.iter().any(|r| r.group == HighlightGroup::Comment),
            "DocumentEdit 後にコメントがハイライトされる: {:?}",
            s.highlights
        );
    }

    #[test]
    fn expected_text_match_applies() {
        // B2: expected_text が範囲の現テキストと一致すれば適用される
        let mut d = daemon();
        open(&mut d, "hello world");
        let edit = DocumentEdit {
            start: 6,
            end: 11,
            text: "minae".into(),
            checksum: fnv1a64(b"hello world"),
            expected_text: Some("world".into()),
        };
        assert!(apply_edit(&mut d, &edit, 0, EventSource::Interactive).is_none(), "一致なら適用される");
        let s = apply(&mut d, Command::GetState);
        assert_eq!(s.text, "hello minae");
    }

    #[test]
    fn expected_text_mismatch_rejected_but_checksum_passed() {
        // B2 の実証: 位置を1文字ずらす（world のつもりが worl）。checksum は
        // 一致するのに expected_text（局所）が引っかかる — 位置のずれを
        // checksum（全文）は検出できず expected_text が検出する（相補関係）。
        let mut d = daemon();
        open(&mut d, "hello world");
        let edit = DocumentEdit {
            start: 6,
            end: 11,
            text: "X".into(),
            checksum: fnv1a64(b"hello world"),
            expected_text: Some("worl".into()),
        };
        let snap = apply_edit(&mut d, &edit, 0, EventSource::Interactive)
            .expect("不一致は拒否のスナップショットを返す");
        let status = snap.status.as_deref().expect("status に拒否理由");
        // C1: checksum 不一致とは区別されるメッセージ
        assert!(
            status.starts_with("expected text mismatch"),
            "checksum 不一致と区別される: {status}"
        );
        assert!(status.contains("\"worl\""), "期待値のスニペットを含む: {status}");
        assert!(status.contains("\"world\""), "実値のスニペットを含む: {status}");
        assert_eq!(snap.text, "hello world", "状態は変わらない");
        assert!(!d.editor.can_undo(), "拒否で undo 履歴が増えない");
    }

    #[test]
    fn expected_text_none_behaves_like_before() {
        // None: checksum のみ（従来どおり）— 範囲内の別テキストでも適用される
        let mut d = daemon();
        open(&mut d, "hello world");
        let edit = DocumentEdit {
            start: 6,
            end: 11,
            text: "minae".into(),
            checksum: fnv1a64(b"hello world"),
            expected_text: None,
        };
        assert!(apply_edit(&mut d, &edit, 0, EventSource::Interactive).is_none(), "None なら適用される");
        let s = apply(&mut d, Command::GetState);
        assert_eq!(s.text, "hello minae");
    }

    #[test]
    fn expected_text_mismatch_snippet_is_truncated() {
        // C1 の粒度: 長い一致期待文字列は先頭N文字 + … に短縮され応答が肥大しない
        let mut d = daemon();
        open(&mut d, "a");
        let long = "x".repeat(200);
        let edit = DocumentEdit {
            start: 0,
            end: 1,
            text: "".into(),
            checksum: fnv1a64(b"a"),
            expected_text: Some(long.clone()),
        };
        let snap = apply_edit(&mut d, &edit, 0, EventSource::Interactive).expect("不一致で拒否");
        let status = snap.status.as_deref().unwrap();
        assert!(!status.contains(&long), "全文は含まれない");
        assert!(status.contains('…'), "切れたことを示す: {status}");
    }

    #[test]
    fn external_reload_reparses_highlights() {
        // 外部リロード（ADR-0015）後のスナップショットも highlights が一致する
        let mut d = daemon();
        open_path(&mut d, "test.rs", "fn f() {}\n");
        let doc_id = d.editor.focused_doc_id();
        let new_text = "// replaced\nfn g() {}\n";
        assert!(d.editor.reload_doc(doc_id, new_text), "リロードで置換される");
        let s = apply(&mut d, Command::GetState);
        assert_highlights_valid(&s.text, &s.highlights);
        assert!(
            s.highlights.iter().any(|r| r.group == HighlightGroup::Comment),
            "リロード後のコメントがハイライトされる: {:?}",
            s.highlights
        );
        assert!(
            s.highlights.iter().any(|r| &s.text[r.start..r.end] == "fn"),
            "リロード後の fn がハイライトされる: {:?}",
            s.highlights
        );
    }

    #[test]
    fn focus_switch_keeps_highlights_per_document() {
        // DocumentId キーのキャッシュ: フォーカスを往復しても各文書の
        // highlights がその文書のテキストと一致する（ADR-0016 の不変条件）
        let mut d = daemon();
        open_path(&mut d, "a.rs", "fn a() {}\n");
        apply(&mut d, Command::Insert { text: "// x\n".into() });
        // 別文書へ移動 → txt は空・a.rs のキャッシュは保持
        open_path(&mut d, "b.txt", "plain text\n");
        let s = apply(&mut d, Command::GetState);
        assert!(s.highlights.is_empty(), "b.txt は空: {:?}", s.highlights);
        // a.rs にフォーカスを戻す → コメントのハイライトが一致したまま
        d.editor.focus_open_path(std::path::Path::new("a.rs"));
        let s = apply(&mut d, Command::GetState);
        assert_highlights_valid(&s.text, &s.highlights);
        assert!(
            s.highlights.iter().any(|r| r.group == HighlightGroup::Comment),
            "a.rs に戻ってもコメントがハイライトされる: {:?}",
            s.highlights
        );
    }

    #[test]
    fn undo_removes_cleared_highlights() {
        // 削除方向のキャッシュ無効化: undo で消えたコメントのハイライトが
        // 残らない（checksum 不一致 → 再パース）
        let mut d = daemon();
        open_path(&mut d, "a.rs", "fn f() {}\n");
        let s = apply(&mut d, Command::Insert { text: "// x\n".into() });
        assert!(
            s.highlights.iter().any(|r| r.group == HighlightGroup::Comment),
            "挿入後はコメントがある: {:?}",
            s.highlights
        );
        let s = apply(&mut d, Command::Undo);
        assert_highlights_valid(&s.text, &s.highlights);
        assert!(
            !s.highlights.iter().any(|r| r.group == HighlightGroup::Comment),
            "undo でコメントのハイライトが消える: {:?}",
            s.highlights
        );
    }

    #[test]
    fn incremental_edits_match_fresh_parse() {
        // ADR-0021: 差分ベースのインクリメンタルパースが全文再パースと同一の
        // ハイライトを返す（挿入・削除・undo/redo・複数行挿入の全経路）。
        // ファイルは viewport（既定 24 行）未満なので窓 = 全文。
        let mut d = daemon();
        let def = mina_loader::language_by_name("rust").unwrap();
        open_path(&mut d, "test.rs", "fn a() {}\n// note\nfn b(x: i32) -> i32 { x + 1 }\n");

        let assert_matches = |d: &mut Daemon| {
            let s = apply(d, Command::GetState);
            assert_highlights_valid(&s.text, &s.highlights);
            assert_eq!(
                s.highlights,
                mina_loader::compute_highlights(def, &s.text),
                "インクリメンタル結果が全文再パースと一致: {}",
                s.text
            );
        };

        // 挿入（カーソルは開いた直後 = 先頭）
        for ch in ["x", "y", "z"] {
            apply(&mut d, Command::Insert { text: ch.into() });
            assert_matches(&mut d);
        }
        // undo/redo
        apply(&mut d, Command::Undo);
        assert_matches(&mut d);
        apply(&mut d, Command::Redo);
        assert_matches(&mut d);
        // 文末へ移動して削除（後方削除）
        apply(&mut d, Command::Goto { target: GotoTarget::DocumentEnd });
        apply(&mut d, Command::DeleteBackward);
        assert_matches(&mut d);
        apply(&mut d, Command::DeleteBackward);
        assert_matches(&mut d);
        // 複数行テキストの挿入（改行を跨ぐ差分）
        apply(&mut d, Command::Insert { text: "\n// tail\nfn z() {}\n".into() });
        assert_matches(&mut d);
    }

    #[test]
    fn highlights_cover_only_visible_window_and_follow_scroll() {
        // ADR-0021: スナップショットのハイライトは可視範囲のみ（窓の外の行に
        // 範囲を出さない）。Scroll で窓が動くとハイライトも追従する。
        let mut d = daemon();
        let mut src = String::new();
        for i in 0..60 {
            src.push_str(&format!("fn f{i}() {{}}\n"));
        }
        open_path(&mut d, "test.rs", &src);
        let s = apply(&mut d, Command::GetState);
        assert_highlights_valid(&s.text, &s.highlights);
        // 窓 = 行 0..24（既定 viewport）。ASCII のみなので byte == char。
        let line24_start = s.text.match_indices('\n').nth(23).map(|(i, _)| i + 1).unwrap();
        assert!(!s.highlights.is_empty(), "窓内にハイライトがある");
        assert!(
            s.highlights.iter().all(|r| r.start < line24_start),
            "窓の外（行24以降）に範囲を出さない: {:?}",
            s.highlights
        );

        // 1ページスクロール → first_line が 24 に進み、ハイライトも行 24..48 に移る
        let s = apply(&mut d, Command::Scroll { pages: 1 });
        assert_eq!(s.first_line, 24);
        assert_highlights_valid(&s.text, &s.highlights);
        assert!(
            s.highlights.iter().all(|r| r.start >= line24_start),
            "スクロール後は行24以降のみ: {:?}",
            s.highlights
        );
        assert!(
            s.highlights.iter().any(|r| &s.text[r.start..r.end] == "f30"),
            "行30の関数名がハイライトされる: {:?}",
            s.highlights
        );
    }

    #[tokio::test]
    async fn normalize_open_path_equates_notations_of_same_file() {
        // CRITICAL C2: 同一ファイルを指す異なる表記（絶対・./ 付き・.. 付き・
        // symlink）が同じ正規化パスに解決される（#7 の再利用判定の前提）。
        let dir = std::env::temp_dir().join(format!("minae-c2-norm-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        let file = dir.join("sub").join("a.txt");
        std::fs::write(&file, "x").unwrap();

        let plain = normalize_open_path(file.clone()).await;
        let dot = normalize_open_path(dir.join("sub").join("./a.txt")).await;
        let dotdot = normalize_open_path(dir.join("other").join("../sub/a.txt")).await;
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let via_link = normalize_open_path(link.clone()).await;

        assert!(plain.is_absolute(), "正規化後は絶対パス: {}", plain.display());
        assert_eq!(plain, dot, "./ 付き表記も同一");
        assert_eq!(plain, dotdot, ".. 付き表記も同一");
        assert_eq!(plain, via_link, "symlink 経由も同一");

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lexical_normalize_makes_absolute_and_resolves_dots() {
        // 存在しないパス（新規ファイル Open のフォールバック）は絶対化 + . / .. 解決
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            lexical_normalize(Path::new("./a/../b.txt")),
            cwd.join("b.txt")
        );
        assert_eq!(
            lexical_normalize(Path::new("/x/./y/../z.txt")),
            PathBuf::from("/x/z.txt")
        );
        // ルートより上へは行かない（/../x は /x）
        assert_eq!(lexical_normalize(Path::new("/../x")), PathBuf::from("/x"));
    }

    #[tokio::test]
    async fn open_rejects_oversized_file() {
        // SEC-1 / ADR-0008: MAX_FILE_SIZE 超のファイルは状態を変えずに拒否する
        let dir = std::env::temp_dir();
        let path = dir.join(format!("minae-sec-oversize-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // スパースファイル: 実データなしでサイズだけ上限を超える
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_FILE_SIZE + 1).unwrap();
        drop(f);

        let path_str = path.to_string_lossy().into_owned();
        let (contents, status) = read_open_target(&path_str).await;
        assert!(contents.is_none());
        let msg = status.expect("拒否メッセージが出る");
        assert!(
            msg.starts_with("file too large"),
            "専用メッセージ: {msg}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn open_rejects_non_regular_file() {
        // SEC-1: ディレクトリ（非正規ファイル）は拒否し、既存のメッセージで報告
        let dir = std::env::temp_dir().join(format!("minae-sec-nonfile-{}", std::process::id()));
        let _ = std::fs::remove_dir(&dir);
        std::fs::create_dir(&dir).unwrap();

        let path_str = dir.to_string_lossy().into_owned();
        let (contents, status) = read_open_target(&path_str).await;
        assert!(contents.is_none());
        let msg = status.expect("拒否メッセージが出る");
        assert!(msg.starts_with("cannot open"), "{msg}");
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn open_reports_non_utf8_as_cannot_read() {
        // 非 UTF-8 ファイルを無言で空文書にせず、status で報告する
        let dir = std::env::temp_dir();
        let path = dir.join(format!("minae-sec-nonutf8-{}.txt", std::process::id()));
        // 無効な UTF-8 バイト列（UTF-16 BOM に使われる 0xFF 0xFE を含む）
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x41]).unwrap();

        let path_str = path.to_string_lossy().into_owned();
        let (contents, status) = read_open_target(&path_str).await;
        assert!(contents.is_none());
        let msg = status.expect("拒否メッセージが出る");
        assert!(msg.starts_with("cannot read"), "{msg}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn oversized_command_line_closes_connection() {
        // SEC-1: 改行のない過大な行を送ると接続が閉じられ、応答が返らない
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-sec-sock-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let sock_serve = sock.clone();
        tokio::spawn(async move {
            let _ = serve(&sock_serve).await;
        });
        // socket が現れるまで待つ
        for _ in 0..100 {
            if UnixStream::connect(&sock).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        use tokio::io::AsyncReadExt;
        let mut stream = UnixStream::connect(&sock).await.expect("接続できる");
        let big = "x".repeat(MAX_CMD_LINE + 1);
        let _ = stream.write_all(big.as_bytes()).await; // 途中で EPIPE になる場合もある
        let mut buf = vec![0u8; 16];
        let n = stream.read(&mut buf).await;
        // 応答は来ず、接続は閉じられる（EOF かエラー）
        assert!(
            matches!(n, Ok(0) | Err(_)),
            "過大行に対して応答しない: {n:?}"
        );
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn set_viewport_huge_height_is_clamped() {
        // usize::MAX の高さでも overflow panic せず、上限に clamp される
        let mut d = daemon();
        open(&mut d, "a\nb\nc\nd\ne");
        apply(&mut d, Command::SetViewport { height: usize::MAX });
        assert_eq!(d.viewport_height, MAX_VIEWPORT_HEIGHT);
        // clamp 後はスクロール計算（first_line + height）が overflow しない
        let _ = apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        let _ = apply(
            &mut d,
            Command::Move {
                movement: Movement::Char,
                direction: Direction::Forward,
            },
        );
    }

    #[test]
    fn scroll_huge_pages_do_not_panic() {
        // i64::MAX / i64::MIN のページ数でも overflow panic せず snapshot が返る
        let mut d = daemon();
        open(&mut d, "a\nb\nc\nd\ne");
        for pages in [isize::MAX, isize::MIN, 0] {
            let s = apply(&mut d, Command::Scroll { pages });
            assert_eq!(s.text, "a\nb\nc\nd\ne");
        }
        // clamp 後はスクロール計算（first_line + amount）が overflow しない
        let _ = apply(&mut d, Command::Scroll { pages: isize::MAX });
    }

    #[test]
    fn insert_marks_dirty_and_moves_cursor() {
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(&mut d, Command::Insert { text: "X".into() });
        assert_eq!(s.text, "Xhello");
        assert!(s.dirty, "編集で dirty になる");
        assert_eq!(s.selection[0].head, 1);
    }

    #[test]
    fn insert_over_max_file_size_is_rejected_without_changes() {
        // SEC-1/ADR-0008: MAX_FILE_SIZE を超える挿入は拒否される。Open と同じ
        // 上限を Insert にも適用し、文書・選択・dirty を一切変えず status で
        // 報告する（修正前は繰り返し Insert でメモリが無制限に成長した）。
        let mut d = daemon();
        // 上限の 10 バイト手前まで埋めた文書
        let base = "x".repeat(MAX_FILE_SIZE as usize - 10);
        open(&mut d, &base);
        apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        let sel_before = d.editor.selection();
        // 上限を超える挿入（+11 バイト）は拒否され、状態が不変
        let s = apply(&mut d, Command::Insert { text: "y".repeat(11) });
        let msg = s.status.as_deref().expect("拒否メッセージが出る");
        assert!(msg.starts_with("file too large"), "専用メッセージ: {msg}");
        assert_eq!(s.text.len() as u64, MAX_FILE_SIZE - 10, "文書は不変");
        assert!(!s.dirty, "拒否では dirty にならない");
        let r = sel_before.ranges()[0];
        assert_eq!(
            s.selection[0],
            Range {
                anchor: r.anchor(),
                head: r.head()
            },
            "選択は不変"
        );

        // 上限ちょうどまでなら成功する（+10 バイトでぴったり MAX_FILE_SIZE）
        let s = apply(&mut d, Command::Insert { text: "z".repeat(10) });
        assert_eq!(s.text.len() as u64, MAX_FILE_SIZE);
        assert!(s.dirty, "上限内の挿入は成功して dirty になる");
    }

    #[test]
    fn rejected_insert_keeps_undo_group_open() {
        // SEC-1/ADR-0008: 拒否された Insert は undo グループの状態を変えない。
        // Insert セッション中の拒否後もグループは開いたまま（undo 1回で
        // セッション全体が戻る）。
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "a".into() });
        // 上限を超える Insert を拒否（グループは開いたまま）
        let s = apply(
            &mut d,
            Command::Insert {
                text: "x".repeat(MAX_FILE_SIZE as usize),
            },
        );
        assert!(
            s.status.as_deref().unwrap().starts_with("file too large"),
            "拒否される"
        );
        assert_eq!(s.mode, Mode::Insert, "拒否でモードも変わらない");
        // 拒否後も同じグループに追記でき、undo 1回で全体が戻る
        apply(&mut d, Command::Insert { text: "b".into() });
        let s = apply(&mut d, Command::SetMode { mode: Mode::Normal });
        assert_eq!(s.text, "ab");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "", "undo 1回でセッション全体が戻る（グループが無傷）");
    }

    #[test]
    fn rejected_and_noop_ops_keep_state_and_undo_history() {
        // ADR-0012（M1）: 拒否・no-op 操作は状態・undo 履歴を変えず、changed
        // フラグも false（record_event のゲート）になる。修正前は no-op の
        // 削除/挿入でもトランザクションが履歴に積まれ、拒否/no-op でも
        // イベントが記録されていた。（世代・イベントの観測は socket 経由の
        // m1_rejected_and_noop_ops_do_not_advance_generation_or_events で行う
        // — record_event は接続ハンドラ側の責務のため。）
        let mut d = daemon();
        open(&mut d, "hello");
        assert!(!d.editor.can_undo());
        // 文書先頭で Backspace: no-op（changed=false）
        let (s, changed) = apply_from(&mut d, Command::DeleteBackward, 0);
        assert_eq!(s.text, "hello", "状態は変わらない");
        assert!(!changed, "no-op 削除は changed=false");
        // 文末で Delete・カーソル上の DeleteRange・空文字挿入も no-op
        apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        apply(&mut d, Command::DeleteForward);
        apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentStart,
            },
        );
        apply(&mut d, Command::DeleteRange);
        apply(&mut d, Command::Insert { text: "".into() });
        apply(&mut d, Command::Undo); // 履歴なし: no-op
        let s = apply(&mut d, Command::GetState);
        assert_eq!(s.text, "hello", "状態は変わらない");
        assert!(!d.editor.can_undo(), "no-op 操作で undo 履歴が増えない");
        // 拒否された Insert（サイズ超過）も状態・履歴を変えず changed=false
        // （16MiB 応答は debug の書き込みタイムアウトと競合するため、ソケット
        // 経由ではなく changed フラグで直接検証する）
        let (s, changed) = apply_from(
            &mut d,
            Command::Insert {
                text: "x".repeat(MAX_FILE_SIZE as usize + 1),
            },
            0,
        );
        assert!(s.status.as_deref().unwrap().starts_with("file too large"));
        assert_eq!(s.text, "hello");
        assert!(!changed, "拒否は changed=false");
        assert!(!d.editor.can_undo(), "拒否で undo 履歴が増えない");
    }

    #[test]
    fn search_forward_selects_match_and_next_advances() {
        let mut d = daemon();
        open(&mut d, "foo bar foo baz");
        // `/foo`: カーソル位置から最初の一致を選択
        let s = apply(&mut d, Command::Search {
            query: "foo".into(),
            direction: Direction::Forward,
        });
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 3 }]);
        assert_eq!(s.status, None);
        // `n`: 次の一致へ
        let s = apply(&mut d, Command::SearchNext {
            direction: Direction::Forward,
        });
        assert_eq!(s.selection, vec![Range { anchor: 8, head: 11 }]);
        // `n`: 折り返して先頭へ（現在位置と同じ一致には戻らない）
        let s = apply(&mut d, Command::SearchNext {
            direction: Direction::Forward,
        });
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 3 }]);
    }

    #[test]
    fn search_backward_and_wrap_via_question() {
        let mut d = daemon();
        open(&mut d, "foo bar foo baz");
        // 文末へ移動してから `?foo`: 直前の一致へ
        apply(&mut d, Command::Goto {
            target: GotoTarget::DocumentEnd,
        });
        let s = apply(&mut d, Command::Search {
            query: "foo".into(),
            direction: Direction::Backward,
        });
        assert_eq!(s.selection, vec![Range { anchor: 8, head: 11 }], "?foo: 直前の一致");
        // `N`: 前へ（折り返して末尾の一致へ）
        let s = apply(&mut d, Command::SearchNext {
            direction: Direction::Backward,
        });
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 3 }]);
        let s = apply(&mut d, Command::SearchNext {
            direction: Direction::Backward,
        });
        assert_eq!(s.selection, vec![Range { anchor: 8, head: 11 }], "折り返し");
    }

    #[test]
    fn search_no_match_reports_status_without_changing_state() {
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(&mut d, Command::Search {
            query: "xyz".into(),
            direction: Direction::Forward,
        });
        assert!(s.status.as_deref().unwrap().contains("no match"));
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 0 }], "選択は変わらない");
        // 検索履歴なしの n は status だけ
        let s = apply(&mut d, Command::SearchNext {
            direction: Direction::Forward,
        });
        assert!(s.status.as_deref().unwrap().contains("no previous search"));
        // 空クエリは no-op
        let s = apply(&mut d, Command::Search {
            query: "".into(),
            direction: Direction::Forward,
        });
        assert!(s.status.as_deref().unwrap().contains("empty search"));
    }

    #[test]
    fn search_selection_selects_all_word_matches() {
        let mut d = daemon();
        open(&mut d, "foo bar foo baz foo");
        // `*`: カーソル位置の単語の全一致を複数カーソル選択
        apply(&mut d, Command::Move {
            movement: Movement::Word,
            direction: Direction::Forward,
        }); // 先頭の foo の末尾へ
        let s = apply(&mut d, Command::SearchSelection);
        assert_eq!(
            s.selection,
            vec![
                Range { anchor: 0, head: 3 },
                Range { anchor: 8, head: 11 },
                Range { anchor: 16, head: 19 },
            ]
        );
        // 選択があればそのテキストで検索する
        let mut d2 = daemon();
        open(&mut d2, "cat dog cat");
        apply(&mut d2, Command::Move {
            movement: Movement::Word,
            direction: Direction::Forward,
        });
        let s = apply(&mut d2, Command::SearchSelection);
        assert_eq!(s.selection.len(), 2, "cat が2箇所選択される");
    }

    #[test]
    fn search_uses_smart_case() {
        let mut d = daemon();
        open(&mut d, "Hello hello");
        let s = apply(&mut d, Command::Search {
            query: "hello".into(),
            direction: Direction::Forward,
        });
        // 小文字クエリ: 大小区別しない → 先頭の Hello に一致
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 5 }]);
        let s = apply(&mut d, Command::Search {
            query: "Hello".into(),
            direction: Direction::Forward,
        });
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 5 }]);
    }

    #[test]
    fn append_o_and_O_reach_insert_and_edit() {
        // a: カーソルの1文字後ろで Insert
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(&mut d, Command::Append);
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(s.selection, vec![Range { anchor: 1, head: 1 }]);
        let s = apply(&mut d, Command::Insert { text: "X".into() });
        assert_eq!(s.text, "hXello");

        // a: 行末では動かない（改行を跨がない）
        let mut d2 = daemon();
        open(&mut d2, "abc");
        apply(&mut d2, Command::Goto {
            target: GotoTarget::DocumentEnd,
        });
        let s = apply(&mut d2, Command::Append);
        assert_eq!(s.selection, vec![Range { anchor: 3, head: 3 }]);

        // o: 行の下に空行を開いて Insert（undo 1回で空行だけが戻る）
        let mut d3 = daemon();
        open(&mut d3, "abc\ndef");
        let s = apply(&mut d3, Command::OpenBelow);
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(s.text, "abc\n\ndef");
        assert_eq!(s.selection, vec![Range { anchor: 4, head: 4 }], "新しい行の先頭");
        apply(&mut d3, Command::Insert { text: "z".into() });
        assert_eq!(d3.editor.current_document().text().to_string(), "abc\nz\ndef");
        apply(&mut d3, Command::SetMode { mode: Mode::Normal });
        // undo 1回: タイプ分（新しい undo グループ）だけが戻る
        let s = apply(&mut d3, Command::Undo);
        assert_eq!(s.text, "abc\n\ndef");
        // undo 2回: 空行の挿入（自身のグループ）が戻る
        let s = apply(&mut d3, Command::Undo);
        assert_eq!(s.text, "abc\ndef");

        // O: 行の上に空行を開いて Insert（カーソルは新しい空行）
        let mut d4 = daemon();
        open(&mut d4, "abc\ndef");
        apply(&mut d4, Command::Move {
            movement: Movement::Line,
            direction: Direction::Forward,
        });
        let s = apply(&mut d4, Command::OpenAbove);
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(s.text, "abc\n\ndef", "2行目の上に空行");
        assert_eq!(s.selection, vec![Range { anchor: 4, head: 4 }]);
    }

    #[test]
    fn replace_swaps_selection_and_cursor_char() {
        // 選択の置換（r は複数カーソル対応: replace_ranges）
        let mut d = daemon();
        open(&mut d, "hello");
        apply(&mut d, Command::Extend {
            movement: Movement::Word,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::Replace { text: "X".into() });
        assert_eq!(s.text, "X");
        assert_eq!(s.mode, Mode::Normal, "r は Normal のまま");
        assert_eq!(s.selection, vec![Range { anchor: 1, head: 1 }], "置換後テキストの直後");
        // カーソル: その位置の1文字（改行は置換しない — 行末では no-op）
        let mut d2 = daemon();
        open(&mut d2, "abc\ndef");
        apply(&mut d2, Command::Move {
            movement: Movement::LineEnd,
            direction: Direction::Forward,
        });
        let s = apply(&mut d2, Command::Replace { text: "Z".into() });
        assert_eq!(s.text, "abc\ndef", "行末（改行前）では no-op");
        apply(&mut d2, Command::Move {
            movement: Movement::Char,
            direction: Direction::Backward,
        });
        let s = apply(&mut d2, Command::Replace { text: "Z".into() });
        assert_eq!(s.text, "abZ\ndef");
    }

    #[test]
    fn extend_line_below_repeats_and_select_all() {
        // x: 現在の行 + 下の行（連打で行が追加される）
        let mut d = daemon();
        open(&mut d, "l0\nl1\nl2\nl3");
        let s = apply(&mut d, Command::ExtendLineBelow); // カーソルは 0 行目先頭
        assert_eq!(
            s.selection,
            vec![Range { anchor: 0, head: 3 }],
            "l0（末尾改行含む）"
        );
        let s = apply(&mut d, Command::ExtendLineBelow);
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 6 }], "l1 が加わる");
        let s = apply(&mut d, Command::ExtendLineBelow);
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 9 }], "l2 が加わる");
        // すべて選択（%）
        let s = apply(&mut d, Command::SelectAll);
        assert_eq!(
            s.selection,
            vec![Range { anchor: 0, head: 11 }],
            "文書全体"
        );
    }

    #[test]
    fn surround_add_wraps_selection_and_cursor() {
        // ms(: 選択を ( で囲み、囲んだ全体を選択したまま Normal に戻る
        let mut d = daemon();
        open(&mut d, "hello world");
        apply(&mut d, Command::SetMode { mode: Mode::Select });
        apply(&mut d, Command::Extend {
            movement: Movement::Word,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::SurroundAdd { ch: '(' });
        assert_eq!(s.text, "(hello )world");
        assert_eq!(s.mode, Mode::Normal, "surround 後は Select を抜ける");
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 8 }], "囲んだ全体を選択");
        // カーソル上は空ペアを挿入
        let mut d2 = daemon();
        open(&mut d2, "abc");
        let s = apply(&mut d2, Command::SurroundAdd { ch: '[' });
        assert_eq!(s.text, "[]abc", "カーソル上は空ペアを挿入");
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 2 }]);
        // undo で戻る
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "hello world");
    }

    #[test]
    fn surround_delete_and_replace_with_status_on_missing_pair() {
        // md(: カーソルを囲む ( を両側から削除
        let mut d = daemon();
        open(&mut d, "(abc) def");
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::SurroundDelete { ch: '(' });
        assert_eq!(s.text, "abc def");
        // ペアが無い位置では状態を変えず status で報告
        let mut d2 = daemon();
        open(&mut d2, "no pair");
        let s = apply(&mut d2, Command::SurroundDelete { ch: '(' });
        assert_eq!(s.text, "no pair");
        assert!(
            s.status.as_deref().unwrap_or("").contains("surround"),
            "status に理由が載る: {:?}",
            s.status
        );
        // mr(: を [ に置き換え（後方からも可）
        let mut d3 = daemon();
        open(&mut d3, "(ab) (cd)");
        apply(&mut d3, Command::SetMode { mode: Mode::Select });
        apply(&mut d3, Command::Extend {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        let s = apply(&mut d3, Command::SurroundReplace {
            from: ')',
            to: '[',
        });
        assert_eq!(s.text, "[ab] (cd)");
    }

    #[test]
    fn select_mode_goto_extends_selection() {
        // Select モードの gg / G は移動でなく選択の拡張（Helix と同じ）
        let mut d = daemon();
        open(&mut d, "hello\nworld");
        apply(&mut d, Command::SetMode { mode: Mode::Select });
        apply(&mut d, Command::Move {
            movement: Movement::Line,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::Goto {
            target: GotoTarget::DocumentStart,
        });
        assert_eq!(
            s.selection,
            vec![Range { anchor: 6, head: 0 }],
            "anchor を保って文書先頭まで拡張"
        );
        // Normal では点に潰れる
        let mut d2 = daemon();
        open(&mut d2, "hello\nworld");
        apply(&mut d2, Command::Goto {
            target: GotoTarget::DocumentStart,
        });
        assert_eq!(d2.editor.selection(), Selection::point(0));
    }

    #[test]
    fn scroll_half_moves_by_half_viewport() {
        // 高さ 24 の半分 = 12 行スクロール（カーソルも連動）
        let mut d = daemon();
        let text = (0..40).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
        open(&mut d, &text);
        let s = apply(&mut d, Command::ScrollHalf {
            direction: Direction::Forward,
        });
        assert_eq!(s.first_line, 12);
        // C-u で戻る
        let s = apply(&mut d, Command::ScrollHalf {
            direction: Direction::Backward,
        });
        assert_eq!(s.first_line, 0);
    }

    #[test]
    fn first_non_whitespace_movement_matches_g_s() {
        let mut d = daemon();
        open(&mut d, "  hello\n\n\t indented");
        apply(&mut d, Command::Move {
            movement: Movement::FirstNonWhitespace,
            direction: Direction::Forward,
        });
        assert_eq!(
            d.editor.selection(),
            Selection::point(2),
            "2行目（空白のみ）は列 0"
        );
        // 別の行へ移動してから g s
        apply(&mut d, Command::Move {
            movement: Movement::Line,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::Move {
            movement: Movement::Line,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::Move {
            movement: Movement::FirstNonWhitespace,
            direction: Direction::Forward,
        });
        let sel = &s.selection[0];
        let line_start = 0 + "  hello\n\n".chars().count();
        assert_eq!(sel.head, line_start + 2, "タブ1文字 + 空白1文字の後");
        // Select モードでも拡張で使える
        let mut d2 = daemon();
        open(&mut d2, "  hi");
        apply(&mut d2, Command::SetMode { mode: Mode::Select });
        let s = apply(&mut d2, Command::Extend {
            movement: Movement::FirstNonWhitespace,
            direction: Direction::Forward,
        });
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 2 }]);
    }

    #[test]
    fn kill_to_line_bounds_in_insert_mode() {
        // C-u: 行頭まで削除
        let mut d = daemon();
        open(&mut d, "hello");
        apply(&mut d, Command::Goto {
            target: GotoTarget::DocumentEnd,
        });
        let s = apply(&mut d, Command::KillToLineStart);
        assert_eq!(s.text, "");
        // C-k: 行末まで削除
        let mut d2 = daemon();
        open(&mut d2, "hello");
        apply(&mut d2, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d2, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        let s = apply(&mut d2, Command::KillToLineEnd);
        assert_eq!(s.text, "he");
        // 行末での C-k は no-op
        let s = apply(&mut d2, Command::KillToLineEnd);
        assert_eq!(s.text, "he");
    }

    #[test]
    fn noop_document_edit_keeps_state_and_undo_history() {
        // ADR-0012（M1）: 状態を変えない DocumentEdit（空範囲への空文字置換）も
        // 拒否と同じ扱いでスナップショットを返し、状態・undo 履歴を変えない。
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply_edit(
            &mut d,
            &DocumentEdit {
                start: 2,
                end: 2,
                text: "".into(),
                checksum: fnv1a64(b"hello"),
                expected_text: None,
            },
            0,
            EventSource::Interactive,
        )
        .expect("no-op 編集は拒否と同じ扱いでスナップショットを返す");
        assert_eq!(s.status, None, "no-op は拒否ではない（status なし）");
        assert_eq!(s.text, "hello", "状態は変わらない");
        assert!(!d.editor.can_undo(), "undo 履歴も増えない");
    }

    #[tokio::test]
    async fn m1_rejected_and_noop_ops_do_not_advance_generation_or_events() {
        // ADR-0012（M1 回帰）: 拒否・no-op 操作は generation を進めず、偽の
        // ChangeEvent も記録しない。正常な操作は従来どおり世代・イベントが
        // 進む（修正前: 拒否/no-op でも record_event が無条件に走っていた）。
        //
        // サイズ超過の拒否は unit テスト（rejected_and_noop_ops_keep_state_and_
        // undo_history）で changed フラグを直接検証する — 16MiB の応答は debug
        // の書き込みタイムアウト（RESPONSE_WRITE_TIMEOUT）と競合してソケット
        // 経由のテストが不安定になるため。ここでは checksum 不一致の拒否経路を
        // 検証する。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m1-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-m1-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "hello").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut c, &Command::Open { path: path.clone() }).await;
        assert_eq!(snap.generation, 1, "Open で増加");

        // 文書先頭で Backspace（no-op 削除）
        let snap = request(&mut c, &Command::DeleteBackward).await;
        assert_eq!(snap.text, "hello", "状態は変わらない");
        assert_eq!(snap.generation, 1, "no-op 削除で世代は進まない");
        assert!(
            snap.events.iter().all(|e| e.kind != EventKind::Delete),
            "偽 Delete イベントが記録されない"
        );

        // 文末で Delete（no-op 削除）
        let _ = request(
            &mut c,
            &Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        )
        .await;
        let snap = request(&mut c, &Command::DeleteForward).await;
        assert_eq!(snap.generation, 1, "no-op 削除で世代は進まない");

        // カーソル上の DeleteRange・空文字挿入も no-op
        let _ = request(
            &mut c,
            &Command::Goto {
                target: GotoTarget::DocumentStart,
            },
        )
        .await;
        let snap = request(&mut c, &Command::DeleteRange).await;
        assert_eq!(snap.generation, 1, "no-op DeleteRange で世代は進まない");
        let snap = request(&mut c, &Command::Insert { text: "".into() }).await;
        assert_eq!(snap.generation, 1, "空文字挿入で世代は進まない");

        // 履歴が無いので undo も no-op
        let snap = request(&mut c, &Command::Undo).await;
        assert_eq!(snap.generation, 1, "no-op undo で世代は進まない");

        // 拒否された DocumentEdit（checksum 不一致）→ 世代不変・イベントなし
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 1,
                end: 1,
                text: "Y".into(),
                checksum: fnv1a64(b"wrong"),
                expected_text: None,
            },
        )
        .await;
        assert!(snap.status.is_some(), "拒否される");
        assert_eq!(snap.generation, 1, "拒否で世代は進まない");
        assert!(
            snap.events.iter().all(|e| e.kind != EventKind::ReplaceRange),
            "偽 ReplaceRange イベントが記録されない"
        );

        // 正常な挿入は従来どおり世代・イベントが進む
        let snap = request(&mut c, &Command::Insert { text: "X".into() }).await;
        assert_eq!(snap.text, "Xhello");
        assert_eq!(snap.generation, 2, "正常な編集は世代が進む");
        assert_eq!(snap.events.last().unwrap().kind, EventKind::Insert);

        // 正常な削除も従来どおり Delete イベントが記録される
        let _ = request(
            &mut c,
            &Command::Goto {
                target: GotoTarget::DocumentStart,
            },
        )
        .await;
        let snap = request(&mut c, &Command::DeleteForward).await;
        assert_eq!(snap.text, "hello");
        assert_eq!(snap.generation, 3, "正常な削除で世代が進む");
        assert_eq!(snap.events.last().unwrap().kind, EventKind::Delete);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn delete_backward_and_undo() {
        let mut d = daemon();
        open(&mut d, "hello");
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::DeleteBackward);
        assert_eq!(s.text, "hllo");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "hello");
        let s = apply(&mut d, Command::Redo);
        assert_eq!(s.text, "hllo");
    }

    #[test]
    fn delete_forward_deletes_next_char() {
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(&mut d, Command::DeleteForward);
        assert_eq!(s.text, "ello");
    }

    #[test]
    fn delete_word_forward_and_backward() {
        let mut d = daemon();
        open(&mut d, "hello world foo");
        // 単語の途中（hello の 2 文字目）→ 現在の単語の残りを削除
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::DeleteWordForward);
        assert_eq!(s.text, "he world foo");
        // 文頭に戻って単語後方削除 → 文頭なので no-op
        apply(&mut d, Command::Goto {
            target: GotoTarget::DocumentStart,
        });
        let s = apply(&mut d, Command::DeleteWordBackward);
        assert_eq!(s.text, "he world foo", "文頭での単語削除は no-op");
        // 末尾へ移動して単語後方削除 → 直前の単語（と空白以外）が消える
        apply(&mut d, Command::Goto {
            target: GotoTarget::DocumentEnd,
        });
        let s = apply(&mut d, Command::DeleteWordBackward);
        assert_eq!(s.text, "he world ", "直前の単語だけが消え、空白は残る");
        // undo で戻る
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "he world foo");
    }

    #[test]
    fn word_move_bw_then_delete_removes_word() {
        // 単語の途中で b → w で単語全体（空白なし）を選択 → d で削除
        let mut d = daemon();
        open(&mut d, "hello world foo");
        // hello の 2 文字目へ
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        // b: 現在の単語（hello の途中まで）が選択される
        let s = apply(&mut d, Command::Move {
            movement: Movement::Word,
            direction: Direction::Backward,
        });
        assert_eq!(
            s.selection[0].anchor,
            3,
            "b で anchor がカーソル位置（block cursor）に残る"
        );
        assert_eq!(s.selection[0].head, 0);
        // w: 単語全体（末尾まで。空白は含まない）に拡張される
        let s = apply(&mut d, Command::Move {
            movement: Movement::Word,
            direction: Direction::Forward,
        });
        assert_eq!(
            (s.selection[0].anchor, s.selection[0].head),
            (0, 5)
        );
        // d: 選択を削除（単語のみ。後続の空白は残る）
        let s = apply(&mut d, Command::DeleteRange);
        assert_eq!(s.text, " world foo", "hello だけが削除される");
        // undo で戻る
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "hello world foo");
    }

    #[test]
    fn change_deletes_selection_and_enters_insert() {
        // Helix の `c`: 選択を削除して Insert モードへ。削除はグループの外に
        // 積まれ、undo 1回で入力だけが戻る（削除と入力が別単位 = Helix と同様）。
        let mut d = daemon();
        open(&mut d, "hello world");
        apply(&mut d, Command::SetMode { mode: Mode::Select });
        apply(&mut d, Command::Extend {
            movement: Movement::Word,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::Change);
        assert_eq!(s.text, "world", "選択（hello ）が削除される");
        assert_eq!(s.mode, Mode::Insert, "Insert モードへ入る");
        let s = apply(&mut d, Command::Insert { text: "bye ".into() });
        assert_eq!(s.text, "bye world");
        apply(&mut d, Command::SetMode { mode: Mode::Normal });
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "world", "undo 1回目は入力だけを戻す");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "hello world", "undo 2回目で削除も戻る");
    }

    #[test]
    fn change_on_cursor_just_enters_insert() {
        // カーソル上の `c` は削除なしで Insert モードへ（Helix と同じ）
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(&mut d, Command::Change);
        assert_eq!(s.text, "hello");
        assert_eq!(s.mode, Mode::Insert);
        let s = apply(&mut d, Command::Insert { text: "X".into() });
        assert_eq!(s.text, "Xhello");
    }

    #[test]
    fn scroll_moves_cursor_keeping_relative_position() {
        // ADR-0023: C-f/C-b 等のスクロールでカーソルも同じ行数だけ動き、
        // 画面内の相対位置が保たれる（Normal = 点に潰す）
        let mut d = daemon();
        let mut src = String::new();
        for i in 0..80 {
            src.push_str(&format!("line{i:02}\n"));
        }
        open(&mut d, &src);
        // カーソルを行20の列2へ（"lineXX\n" は7文字）
        for _ in 0..20 {
            apply(&mut d, Command::Move {
                movement: Movement::Line,
                direction: Direction::Forward,
            });
        }
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        // 1ページ（既定 viewport 高 = 24）下へ → カーソルは行44の列2
        let s = apply(&mut d, Command::Scroll { pages: 1 });
        assert_eq!(s.first_line, 24, "viewport が1ページ進む");
        assert_eq!(
            s.selection[0],
            Range { anchor: 310, head: 310 },
            "カーソルも24行下（44*7+2）へ: {:?}",
            s.selection
        );
        // 戻る: カーソルは行20の列2へ
        let s = apply(&mut d, Command::Scroll { pages: -1 });
        assert_eq!(s.first_line, 0);
        assert_eq!(s.selection[0], Range { anchor: 142, head: 142 });
    }

    #[test]
    fn scroll_in_select_mode_extends_selection() {
        // ADR-0023: Select モードでは head だけが動き選択が拡張される
        let mut d = daemon();
        let mut src = String::new();
        for i in 0..80 {
            src.push_str(&format!("line{i:02}\n"));
        }
        open(&mut d, &src);
        // 行5の列2で Select モードへ
        for _ in 0..5 {
            apply(&mut d, Command::Move {
                movement: Movement::Line,
                direction: Direction::Forward,
            });
        }
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::SetMode { mode: Mode::Select });
        // 1ページ下へ: anchor（行5・列2 = 37）を保って head が24行下（行29・列2 = 205）へ
        let s = apply(&mut d, Command::Scroll { pages: 1 });
        assert_eq!(s.first_line, 24);
        assert_eq!(
            s.selection[0],
            Range { anchor: 37, head: 205 },
            "anchor を保って head だけが24行下へ: {:?}",
            s.selection
        );
    }

    #[test]
    fn insert_at_line_end_enters_insert_at_eol() {
        // ADR-0023: A = 行末（改行の直前）へ移動して Insert（Helix の A）
        let mut d = daemon();
        open(&mut d, "hello\nworld");
        apply(&mut d, Command::Move {
            movement: Movement::Line,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::InsertAtLineEnd);
        assert_eq!(s.mode, Mode::Insert, "Insert モードへ入る");
        assert_eq!(
            s.selection[0],
            Range { anchor: 11, head: 11 },
            "行1（'world'）の行末へ"
        );
        // 入力は行末に追加され、1つの undo グループになる
        let s = apply(&mut d, Command::Insert { text: "!".into() });
        assert_eq!(s.text, "hello\nworld!");
        apply(&mut d, Command::SetMode { mode: Mode::Normal });
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "hello\nworld");
    }

    #[test]
    fn insert_at_line_start_moves_to_first_non_whitespace() {
        // ADR-0023: I = 行頭（最初の非空白文字）へ移動して Insert（Helix の I）
        let mut d = daemon();
        open(&mut d, "  hello\n   \nworld");
        let s = apply(&mut d, Command::InsertAtLineStart);
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(
            s.selection[0],
            Range { anchor: 2, head: 2 },
            "行0の最初の非空白へ"
        );
        // 空白のみの行は列 0 へ
        apply(&mut d, Command::SetMode { mode: Mode::Normal });
        apply(&mut d, Command::Move {
            movement: Movement::Line,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::InsertAtLineStart);
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(
            s.selection[0],
            Range { anchor: 8, head: 8 },
            "空白のみの行1は列 0 へ"
        );
    }

    #[test]
    fn insert_at_line_end_in_select_collapses_to_line_end() {
        // ADR-0023: Select でも選択を折りたたんで行末で Insert（minae の Insert は
        // 選択を置換するため、拡張のままだと Helix の「行末に追加」と一致しない）
        let mut d = daemon();
        open(&mut d, "abc\ndef");
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::SetMode { mode: Mode::Select });
        apply(&mut d, Command::Extend {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::InsertAtLineEnd);
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(
            s.selection[0],
            Range { anchor: 3, head: 3 },
            "選択は折りたたまれ行0の行末へ"
        );
        let s = apply(&mut d, Command::Insert { text: "!".into() });
        assert_eq!(s.text, "abc!\ndef", "選択は置換されず行末に追加される");
    }

    #[test]
    fn insert_mode_typing_undoes_as_one_group() {
        // i → "abc" 入力 → Esc → undo 1回で元に戻る
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        for ch in ["a", "b", "c"] {
            apply(&mut d, Command::Insert { text: ch.into() });
        }
        let s = apply(&mut d, Command::SetMode { mode: Mode::Normal });
        assert_eq!(s.text, "abc");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "");
    }

    #[test]
    fn leaving_insert_for_select_closes_undo_group() {
        // H1 変種: Insert→Select でもグループを閉じる（ADR-0007）。
        // 2回の Insert セッションが別々の undo 単位になる。
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "a".into() });
        apply(&mut d, Command::SetMode { mode: Mode::Select });
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "b".into() });
        let s = apply(&mut d, Command::SetMode { mode: Mode::Normal });
        assert_eq!(s.text, "ab");

        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "a", "undo 1回目は2回目の Insert セッションだけを戻す");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "", "undo 2回目で1回目のセッションも戻る");
    }

    #[test]
    fn disconnect_in_insert_mode_closes_group_and_resets_mode() {
        // H1: Insert のまま接続が切れても、次のクライアントの編集は別グループになる
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "a".into() });
        d.on_client_disconnect(0, ClientKind::Interactive, true); // 所有者の切断（修正前はグループが開いたまま漏れた）
        assert_eq!(d.editor.mode(), mina_view::Mode::Normal, "切断で Normal に戻る");
        // ADR-0027: 切断でカーソルも先頭へ戻る。このテストは undo グループ境界の
        // 検証が目的なので、明示的に末尾（位置 1）へ戻してから続ける。
        assert_eq!(
            d.editor.selection().ranges()[0].anchor(),
            0,
            "切断でカーソルは先頭に戻る"
        );
        d.editor.set_selection(mina_text::Selection::point(1));

        // 次のクライアント: 再び Insert で入力しても別グループになる
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "b".into() });
        let s = apply(&mut d, Command::SetMode { mode: Mode::Normal });
        assert_eq!(s.text, "ab");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "a", "undo は切断後のセッションだけを戻す");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "", "切断前のセッションも別グループとして戻せる");
    }

    #[test]
    fn disconnect_in_normal_mode_keeps_document_but_resets_cursor() {
        // ADR-0027: Normal モード切断は文書・モードを保ちつつ、カーソルを
        // 先頭へ戻し世代とイベントを進める（修正前は完全な no-op だった）。
        let mut d = daemon();
        open(&mut d, "hello");
        d.editor.set_selection(mina_text::Selection::point(3));
        let gen_before = d.generation;
        d.on_client_disconnect(0, ClientKind::Interactive, true);
        assert_eq!(d.editor.mode(), mina_view::Mode::Normal);
        assert_eq!(d.editor.current_document().text().to_string(), "hello");
        assert_eq!(d.editor.selection().ranges()[0].anchor(), 0, "カーソルは先頭に戻る");
        assert_eq!(d.editor.first_line(), 0, "ビューポートも先頭に戻る");
        assert!(d.generation > gen_before, "状態変化として世代が進む");
        let snap = snapshot(&mut d, None);
        assert_eq!(snap.events.last().unwrap().kind, EventKind::SelectionReset);
        assert_eq!(snap.events.last().unwrap().source, EventSource::External);
    }

    #[test]
    fn non_owner_disconnect_keeps_insert_session() {
        // HIGH-1 主症状: 読み取り専用 agent（非所有者）の切断が人間の Insert
        // セッションを壊さない。修正前はグループが閉じ Normal に戻っていた。
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert }); // TUI（接続 0）が所有者
        apply(&mut d, Command::Insert { text: "a".into() });
        d.on_client_disconnect(9, ClientKind::Headless, true); // 非所有者（agent ワンショット）の切断
        assert_eq!(
            d.editor.mode(),
            mina_view::Mode::Insert,
            "非所有者の切断でモードは変わらない"
        );
        // セッションは無傷: 続けて入力し、undo 1回で全体が戻る
        apply(&mut d, Command::Insert { text: "b".into() });
        apply(&mut d, Command::SetMode { mode: Mode::Normal });
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "", "1 undo でセッション全体が戻る（グループが無傷）");
    }

    #[tokio::test]
    async fn last_interactive_disconnect_resets_cursor_by_default() {
        // ADR-0027: 最後の TUI 切断でカーソルは先頭へ戻る（デフォルト true）。
        // 文書・undo 履歴は保持され、世代が進み SelectionReset が積まれる。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-27-reset-sock-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        start_server(&sock).await;

        // TUI: テキストを入れ、Normal に戻る（カーソルは末尾に残る）
        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let snap = request(&mut tui, &Command::SetMode { mode: Mode::Insert }).await;
        let snap = request(&mut tui, &Command::Insert { text: "hello".into() }).await;
        assert_eq!(snap.text, "hello");
        let snap = request(&mut tui, &Command::SetMode { mode: Mode::Normal }).await;
        assert_eq!(snap.selection[0].anchor, 5, "切断前はカーソルが末尾にある");
        let gen_before = snap.generation;
        drop(tui); // 最後の Interactive の切断

        // 新しい TUI: カーソルは先頭・世代が進み・SelectionReset が積まれている
        let mut tui2 = connect_client(&sock, ClientKind::Interactive).await;
        let snap = request(&mut tui2, &Command::GetState).await;
        assert_eq!(snap.text, "hello", "文書は保持される");
        assert_eq!(snap.selection[0].anchor, 0, "カーソルは先頭に戻っている");
        assert!(snap.generation > gen_before, "世代が進む");
        assert_eq!(snap.events.last().unwrap().kind, EventKind::SelectionReset);
        assert_eq!(snap.events.last().unwrap().source, EventSource::External);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn disconnect_keeps_cursor_when_flag_disabled() {
        // ADR-0027 の v12 再解釈: カーソルリセットは共有 idle view が対象で、
        // 接続の View は切断とともに消える。reset を宣言しない接続の切断では
        // idle view は動かない（文書は追従する — 状態は共有のまま）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-27-nosock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-27-nofile-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
        std::fs::write(&file, "hello\n").unwrap();
        start_server(&sock).await;

        // reset 宣言なしの Interactive 接続（Hello 直書き）
        let stream = UnixStream::connect(&sock).await.expect("接続できる");
        let mut c = TestClient::new(stream);
        let mut hello = serde_json::to_string(&Hello {
            kind: ClientKind::Interactive,
            reset_cursor_on_disconnect: false,
            name: "test".to_string(),
        })
        .unwrap();
        hello.push('\n');
        c.send(hello.as_bytes()).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path }).await;
        let snap = request(
            &mut c,
            &Command::Move {
                movement: Movement::Line,
                direction: Direction::Forward,
            },
        )
        .await;
        assert_eq!(snap.selection[0].anchor, 6, "接続の View でカーソルが進む");
        drop(c);

        // 別の Interactive は自分の View を持つ（文書は共有・カーソルは独立）
        let mut tui2 = connect_client(&sock, ClientKind::Interactive).await;
        let snap = request(&mut tui2, &Command::GetState).await;
        assert_eq!(snap.text, "hello\n", "文書は共有");
        assert_eq!(snap.selection[0].anchor, 0, "カーソルは接続ごとに独立（引き継がない）");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn disconnect_does_not_reset_while_another_interactive_remains() {
        // ADR-0027 の v12 再解釈: 接続の View は切断とともに消えるため、リセット
        // 対象は共有 idle view。ここでは「カーソルが接続ごとに独立」で、
        // 他クライアントの切断が自分の View を踏まないこと（複数 TUI 構成）を
        // 検証する。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-27-two-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-27-two-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
        std::fs::write(&file, "hello\nworld\n").unwrap();
        start_server(&sock).await;

        let mut tui_a = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui_a, &Command::Open { path }).await;
        // tui_a の View でカーソルを進める（idle view は動かない — 観測点）
        let snap = request(
            &mut tui_a,
            &Command::Move {
                movement: Movement::Line,
                direction: Direction::Forward,
            },
        )
        .await;
        assert_eq!(snap.selection[0].anchor, 6, "tui_a の View でカーソルが進む");

        let mut tui_b = connect_client(&sock, ClientKind::Interactive).await;
        // tui_b の Hello 登録が daemon に着弾してから切る（登録と切断の競合を避ける）
        let _ = request(&mut tui_b, &Command::GetState).await;
        drop(tui_a); // まだ tui_b が残っている → リセットしない

        let mut tui_c = connect_client(&sock, ClientKind::Interactive).await;
        let _ = request(&mut tui_c, &Command::GetState).await;
        let snap = request(&mut tui_c, &Command::GetState).await;
        assert_eq!(
            snap.selection[0].anchor, 0,
            "カーソルは接続ごとに独立（tui_a の 6 を引き継がない）"
        );
        drop(tui_b); // tui_c が残っている → リセットしない
        let snap = request(&mut tui_c, &Command::GetState).await;
        assert_eq!(snap.selection[0].anchor, 0, "tui_b の切断だけでは変わらない");
        drop(tui_c); // 最後の切断 → idle view リセット

        // リセットは daemon が切断の EOF を処理してから起きる（非同期）。ここで
        // Interactive を先に接続すると「残存する Interactive」としてリセット自体を
        // 抑止してしまうので、リセットを抑止しない Headless オブザーバで伝播を待つ
        // （ADR-0027 の Interactive 判定には Headless は数えない）。
        let mut obs = connect_client(&sock, ClientKind::Headless).await;
        let observed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let s = request(&mut obs, &Command::GetState).await;
                if s.selection[0].anchor == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await;
        assert!(observed.is_ok(), "最後の切断でリセットされる（2 秒以内に伝播しない）");

        let mut tui_d = connect_client(&sock, ClientKind::Interactive).await;
        let snap = request(&mut tui_d, &Command::GetState).await;
        assert_eq!(snap.selection[0].anchor, 0, "最後の切断でリセットされる");
        assert_eq!(snap.text, "hello\nworld\n", "idle view は最後に開かれた文書を見る");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn per_client_views_are_independent_on_shared_document() {
        // ADR-0037: 同じ文書を 2 つの Interactive クライアントが開いても、
        // カーソルは接続ごとに独立（文書・undo・LSP は共有のまま）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-37-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-37-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
        std::fs::write(&file, "line one\nline two\nline three\n").unwrap();
        start_server(&sock).await;

        let mut a = connect_client(&sock, ClientKind::Interactive).await;
        let mut b = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut a, &Command::Open { path: path.clone() }).await;
        let _ = recv_push(&mut b).await; // a の Open は b へ届く（b 側で消費）
        let _ = request(&mut b, &Command::Open { path }).await;
        let _ = recv_push(&mut a).await; // b の Open は a へ届く（a 側で消費）

        // a: 2 行目へ移動・ビューポート設定
        let snap = request(
            &mut a,
            &Command::Move {
                movement: Movement::Line,
                direction: Direction::Forward,
            },
        )
        .await;
        assert_eq!(snap.selection[0].anchor, 9, "a のカーソルは 2 行目先頭");
        let _ = request(&mut a, &Command::SetViewport { height: 10 }).await;

        // b: カーソルは影響を受けない（文書は共有）
        let snap = request(&mut b, &Command::GetState).await;
        assert_eq!(snap.text, "line one\nline two\nline three\n", "文書は共有");
        assert_eq!(snap.selection[0].anchor, 0, "b のカーソルは a の移動の影響を受けない");

        // b: 3 行目へ移動（連打）
        let _ = request(
            &mut b,
            &Command::Move {
                movement: Movement::Line,
                direction: Direction::Forward,
            },
        )
        .await;
        let snap = request(
            &mut b,
            &Command::Move {
                movement: Movement::Line,
                direction: Direction::Forward,
            },
        )
        .await;
        assert_eq!(snap.selection[0].anchor, 18, "b のカーソルは 3 行目先頭");

        // a: 自分のカーソル（2 行目先頭）を保持
        let snap = request(&mut a, &Command::GetState).await;
        assert_eq!(snap.selection[0].anchor, 9, "a のカーソルも独立（b の移動で動かない）");
        assert_eq!(snap.text, "line one\nline two\nline three\n");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn non_owner_edit_takes_over_closing_owners_group() {
        // HIGH-1 症状2: agent の編集が人間のグループに混入しない。後勝ちで奪取し、
        // agent の編集は独立グループになる。
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert }); // TUI（接続 0）
        apply(&mut d, Command::Insert { text: "a".into() }); // グループ [a]
        let s = apply_from(&mut d, Command::Insert { text: "X".into() }, 9); // agent: 奪取
        assert_eq!(s.0.text, "aX");
        assert_eq!(d.editor.mode(), mina_view::Mode::Normal, "奪取で Normal に戻る");
        assert_eq!(d.insert_owner, None, "奪取後は所有者がいない");
        // undo は後勝ち順: agent の編集 [X] → 人間のセッション [a]
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "a", "agent の編集だけが戻る");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "", "次で人間のグループも戻る");
    }

    #[test]
    fn owner_edit_after_non_owner_takeover_starts_fresh_group() {
        // 奪取後、所有者（TUI）が再び入力すると新しいグループになる（旧グループに
        // 追記されない）。
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "a".into() });
        apply_from(&mut d, Command::Insert { text: "X".into() }, 9); // 奪取
        apply(&mut d, Command::Insert { text: "b".into() }); // TUI が続けて入力
        assert_eq!(d.editor.current_document().text().to_string(), "aXb");
        // b は独立グループ（a とは分かれている）
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "aX");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "a");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "");
    }

    #[test]
    fn non_owner_set_mode_insert_takes_over_open_group() {
        // 別クライアントの SetMode(Insert) は開いているグループを奪取する
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert }); // TUI（接続 0）
        apply(&mut d, Command::Insert { text: "a".into() });
        apply_from(&mut d, Command::SetMode { mode: Mode::Insert }, 9); // agent が奪取
        apply_from(&mut d, Command::Insert { text: "X".into() }, 9);
        apply_from(&mut d, Command::SetMode { mode: Mode::Normal }, 9);
        assert_eq!(d.editor.current_document().text().to_string(), "aX");
        // undo: agent のセッション [X] → 人間のセッション [a] の順
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "a");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "");
    }

    /// テスト用 daemon を一時 socket で起動し、接続できるまで待つ。
    async fn start_server(sock: &std::path::Path) {
        let sock_serve = sock.to_path_buf();
        tokio::spawn(async move {
            let _ = serve(&sock_serve).await;
        });
        for _ in 0..100 {
            if UnixStream::connect(sock).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("daemon が起動しなかった");
    }

    /// テスト用クライアント: UnixStream + 行読みバッファ（NDJSON 1行 = メッセージ1件）。
    ///
    /// 応答（Response）と他クライアント変更の通知（Push）が同じソケットに流れるため
    /// （ADR-0013）、読み取りは行単位で種別を判別する。
    struct TestClient {
        reader: BufReader<UnixStream>,
    }

    impl TestClient {
        fn new(stream: UnixStream) -> Self {
            Self {
                reader: BufReader::new(stream),
            }
        }

        async fn send(&mut self, bytes: &[u8]) {
            self.reader.get_mut().write_all(bytes).await.unwrap();
            self.reader.get_mut().flush().await.unwrap();
        }

        /// メッセージ1件（NDJSON 1行）を読む。
        async fn recv_message(&mut self) -> ServerMessage {
            let mut line = String::new();
            self.reader.read_line(&mut line).await.expect("応答を読む");
            assert!(!line.is_empty(), "応答が来ない（接続が閉じた）");
            serde_json::from_str(line.trim()).expect("ServerMessage をパース")
        }
    }

    /// NDJSON 1 コマンドを送り、応答スナップショットを受け取る。
    /// 間に push が挟まっていれば読み飛ばす（既存テストの観測対象は応答）。
    async fn request(c: &mut TestClient, cmd: &Command) -> StateSnapshot {
        let mut line = serde_json::to_string(cmd).unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        recv_snapshot(c).await
    }

    /// DocumentEdit（位置指定編集）を1つ送り、応答スナップショットを受け取る。
    async fn request_edit(c: &mut TestClient, edit: &DocumentEdit) -> StateSnapshot {
        let mut line = serde_json::to_string(edit).unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        recv_snapshot(c).await
    }

    /// 応答スナップショット1件を読む（途中の push は読み飛ばす）。
    async fn recv_snapshot(c: &mut TestClient) -> StateSnapshot {
        loop {
            match c.recv_message().await {
                ServerMessage::Response { snapshot } => return snapshot,
                ServerMessage::Push { .. } => continue,
                ServerMessage::Hints { .. } => continue,
                ServerMessage::Peek { .. } => continue,
                ServerMessage::ServerInfo { .. } => continue,
                ServerMessage::RenameResult { .. } | ServerMessage::ReferencesResult { .. } => {
                    continue
                }
                ServerMessage::Outline { .. } | ServerMessage::EnclosingSymbol { .. } => {
                    continue
                }
                ServerMessage::Hover { .. }
                | ServerMessage::WorkspaceSymbols { .. }
                | ServerMessage::Check { .. } => continue,
                ServerMessage::ReviewComments { .. } => continue,
                ServerMessage::ReadPath { .. } => continue,
            }
        }
    }

    /// push 1件を読む（途中の応答は読み飛ばす）。ADR-0013 の購読テスト用。
    async fn recv_push(c: &mut TestClient) -> StateSnapshot {
        loop {
            match c.recv_message().await {
                ServerMessage::Push { snapshot } => return snapshot,
                ServerMessage::Response { .. } => continue,
                ServerMessage::Hints { .. } => continue,
                ServerMessage::Peek { .. } => continue,
                ServerMessage::ServerInfo { .. } => continue,
                ServerMessage::RenameResult { .. } | ServerMessage::ReferencesResult { .. } => {
                    continue
                }
                ServerMessage::Outline { .. } | ServerMessage::EnclosingSymbol { .. } => {
                    continue
                }
                ServerMessage::Hover { .. }
                | ServerMessage::WorkspaceSymbols { .. }
                | ServerMessage::Check { .. } => continue,
                ServerMessage::ReviewComments { .. } => continue,
                ServerMessage::ReadPath { .. } => continue,
            }
        }
    }

    /// ReviewComments 応答1件を読む（途中の応答・push は読み飛ばす）。#50。
    async fn request_reviews(c: &mut TestClient) -> (u64, Vec<ReviewCommentView>) {
        let mut line = serde_json::to_string(&Command::ListReviewComments).unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        loop {
            match c.recv_message().await {
                ServerMessage::ReviewComments { generation, comments } => {
                    return (generation, comments)
                }
                _ => continue,
            }
        }
    }

    /// 接続して Hello（クライアント種別）を送る。イベントの source 判定に使う。
    async fn connect_client(sock: &std::path::Path, kind: ClientKind) -> TestClient {
        let stream = UnixStream::connect(sock).await.expect("接続できる");
        let mut c = TestClient::new(stream);
        let mut hello = serde_json::to_string(&Hello {
            kind,
            reset_cursor_on_disconnect: true,
            name: "test".to_string(),
        })
        .unwrap();
        hello.push('\n');
        c.send(hello.as_bytes()).await;
        c
    }

    /// 述語が満たされるまで GetState を繰り返す（非同期の LSP 診断反映待ち用）。
    async fn poll_snapshot(
        c: &mut TestClient,
        predicate: impl Fn(&StateSnapshot) -> bool,
        timeout: std::time::Duration,
    ) -> StateSnapshot {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let snap = request(c, &Command::GetState).await;
            if predicate(&snap) {
                return snap;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "タイムアウト: {snap:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn real_socket_agent_disconnect_keeps_tui_undo_group() {
        // HIGH-1 e2e: 実 socket で TUI（永続）+ agent（ワンショット）の 2 接続。
        // agent の読み取り専用コマンドと切断が TUI の Insert グループを壊さない。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-h1-sock-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        start_server(&sock).await;

        // TUI: Insert モードで "a" を入力（グループを開く）
        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let snap = request(&mut tui, &Command::SetMode { mode: Mode::Insert }).await;
        assert_eq!(snap.mode, Mode::Insert);
        let snap = request(&mut tui, &Command::Insert { text: "a".into() }).await;
        assert_eq!(snap.text, "a");

        // agent ワンショット: 読み取り専用 GetState → 切断
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let snap = request(&mut agent, &Command::GetState).await;
        assert_eq!(snap.text, "a");
        assert_eq!(snap.mode, Mode::Insert, "agent にも現状のモードが見える");
        drop(agent);

        // TUI のセッションは無傷: "b" → Esc → undo 1回で "ab" が全部戻る
        let snap = request(&mut tui, &Command::Insert { text: "b".into() }).await;
        assert_eq!(snap.text, "ab");
        assert_eq!(snap.mode, Mode::Insert, "agent の切断でモードが変わっていない");
        let _ = request(&mut tui, &Command::SetMode { mode: Mode::Normal }).await;
        let snap = request(&mut tui, &Command::Undo).await;
        assert_eq!(
            snap.text, "",
            "undo 1回でセッション全体が戻る（agent の切断がグループを壊していない）"
        );
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn reopen_diverged_path_reloads_but_undo_recovers_unsaved_edits() {
        // #7 + ADR-0015: 開き済みパスの再 Open は文書を再利用する（#7: 同一性・
        // ヒストリー保持）が、ディスクと乖離していれば自動リロードする（ADR-0015:
        // Dirty でも常時）。未保存編集はリロードの undo で回復できる。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-7-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-7-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        // TUI: ファイルを開いて編集（未保存）
        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut tui, &Command::Open { path: path.clone() }).await;
        assert_eq!(snap.text, "base\n");
        let snap = request(&mut tui, &Command::Insert { text: "X".into() }).await;
        assert_eq!(snap.text, "Xbase\n");
        assert!(snap.dirty);

        // ディスクの中身を変えておく: 再 Open でリロードされたら X が消える
        std::fs::write(&file, "changed\n").unwrap();

        // 同じパスを Open し直す → 既存文書を再利用しつつ自動リロード
        let mut agent = connect_client(&sock, ClientKind::Interactive).await;
        let snap = request(&mut agent, &Command::Open { path }).await;
        assert_eq!(snap.text, "changed\n", "乖離していれば再 Open でリロード");
        assert!(!snap.dirty, "リロード後は clean");
        assert!(snap.events.iter().any(|e| {
            e.kind == EventKind::ExternalChange && e.source == EventSource::External
        }));

        // undo で未保存編集が回復できる（ヒストリーが保持されている）
        let snap = request(&mut agent, &Command::Undo).await;
        assert_eq!(snap.text, "Xbase\n", "undo でリロードを戻し未保存編集が回復");
        assert!(snap.dirty);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn reopen_with_different_notation_reuses_document() {
        // CRITICAL C2 e2e: 同一ファイルを異なる表記（絶対・./ 付き・.. 付き・
        // symlink）で再 Open しても既存文書が再利用され、未保存編集・dirty・undo
        // ヒストリーが保持されてディスク再読込されない。修正前は表記の違いで
        // 一致せず、ディスクから新規文書が読み込まれ編集が消えた。
        let dir = std::env::temp_dir().join(format!("minae-c2-sock-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        let sock = dir.join("minae.sock");
        let file = dir.join("sub").join("file.txt");
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        // TUI: ファイルを開いて編集（未保存）
        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let plain = file.to_string_lossy().into_owned();
        let snap = request(&mut tui, &Command::Open { path: plain.clone() }).await;
        assert_eq!(snap.text, "base\n");
        let snap = request(&mut tui, &Command::Insert { text: "X".into() }).await;
        assert_eq!(snap.text, "Xbase\n");
        assert!(snap.dirty);

        // ディスクの中身を変えておく: 再 Open でリロードされたら X が消える
        std::fs::write(&file, "changed\n").unwrap();

        // 表記違いの再 Open: どれも既存文書へフォーカスし直す（C2）
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let notations = [
            plain.clone(),                                      // 絶対
            format!("{}/sub/./file.txt", dir.display()),        // ./ 付き
            format!("{}/other/../sub/file.txt", dir.display()), // .. 付き
            link.to_string_lossy().into_owned(),                // symlink 経由
        ];
        let mut agent = connect_client(&sock, ClientKind::Interactive).await;
        // 最初の再 Open はディスクと乖離しているので自動リロードされる（ADR-0015）
        let snap = request(&mut agent, &Command::Open { path: notations[0].clone() }).await;
        assert_eq!(snap.text, "changed\n", "乖離していれば再 Open でリロード");
        assert!(!snap.dirty);
        // 以降の表記違いの再 Open は乖離なし（ベースライン更新済み）: 文書を再利用
        for path in &notations[1..] {
            let snap = request(&mut agent, &Command::Open { path: path.clone() }).await;
            assert_eq!(snap.text, "changed\n", "表記違いでも同一文書へ");
        }

        // undo も効く（ヒストリーが保持されている）
        let snap = request(&mut agent, &Command::Undo).await;
        assert_eq!(snap.text, "Xbase\n", "undo でリロードを戻し未保存編集が回復");
        assert!(snap.dirty);

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// pkill を使う LSP テストの直列化（並行実行だと互いの mock を殺し合う）。
    static LSP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn reopen_rs_path_respawns_lsp_and_reannounces_current_text() {
        // #7 + ADR-0009: 再利用 Open でも LSP フロー（ensure → didOpen → settle）が
        // 走る。サーバ死亡後は再 spawn され、現在のバッファ内容で診断が返る。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        // 環境変数はプロセスグローバル。他のテストは LSP を起動しないので安全。
        // （edition 2024 のため set_var/remove_var は unsafe）
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-7lsp-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-7lsp-file-{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "fn f() { TODO }\n").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut tui, &Command::Open { path: path.clone() }).await;
        // ADR-0028: Open の応答に診断取得中の活動が載る（settle の spawn 前に確定される）
        assert!(
            snap.activities
                .iter()
                .any(|a| a.kind == ActivityKind::DiagnosticsSettle),
            "Open 直後の活動: {:?}",
            snap.activities
        );
        // 初回解析: TODO（byte 9）の診断が反映されるまで待つ
        let snap = poll_snapshot(
            &mut tui,
            |s| !s.diagnostics.is_empty(),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(snap.diagnostics[0].start, 9, "TODO は byte 9 から");
        assert_eq!(snap.diagnostics[0].message, "mock: TODO found");
        // ADR-0028: 診断が届いても settle は終了直前のため、活動はまだ active
        assert!(
            snap.activities.iter().any(|a| a.kind == ActivityKind::DiagnosticsSettle),
            "診断到着時はまだ解析中: {:?}",
            snap.activities
        );
        // 活動の除去は settle の終了後（次の push で届く）
        let snap = poll_snapshot(
            &mut tui,
            |s| s.activities.is_empty(),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert!(!snap.diagnostics.is_empty(), "診断は残っている: {:?}", snap.diagnostics);

        // 先頭に挿入して TODO を byte 13 へ移動（didChange で同期される）。
        // 応答の text は編集後（編集自体は同期対象外で即時反映）。診断の追従は
        // 背景 pull（ADR-0055）の反映後（push / GetState）に確認する。
        let snap = request(&mut tui, &Command::Insert { text: "aaaa".into() }).await;
        assert_eq!(snap.text, "aaaafn f() { TODO }\n");
        let snap = poll_snapshot(
            &mut tui,
            |s| s.diagnostics.iter().any(|d| d.start == 13),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(snap.diagnostics[0].start, 13, "didChange 後に位置が追従する");

        // サーバを殺す（reader タスクが EOF を拾い is_dead になる）
        let killed = std::process::Command::new("pkill")
            .args(["-f", "target/debug/mock-server"])
            .status()
            .expect("pkill を実行できる");
        assert!(killed.success(), "mock サーバを kill できる: {killed}");

        // 死亡検知で古い診断がクリアされるのを待つ（ADR-0009）
        let snap = poll_snapshot(
            &mut tui,
            |s| s.diagnostics.is_empty(),
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(snap.diagnostics.is_empty(), "死亡後に古い診断は残らない");

        // 同じパスを再 Open: 再利用 + リスポーン + 現在テキストで didOpen 再通知
        let snap = request(&mut tui, &Command::Open { path: path.clone() }).await;
        assert_eq!(snap.text, "aaaafn f() { TODO }\n", "再利用で状態保持");
        assert!(snap.dirty, "dirty 保持");
        // 新しいセッションからの診断が現在テキストの位置（byte 13）で返る
        let snap = poll_snapshot(
            &mut tui,
            |s| !s.diagnostics.is_empty(),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(
            snap.diagnostics[0].start, 13,
            "リスポーン後の診断は現在のバッファ内容に基づく"
        );
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn external_change_reloads_rs_file_and_syncs_lsp() {
        // ADR-0015 + 回帰: .rs ファイルの外部変更リロードは LSP 全文同期を伴う。
        // watch_disk の LSP 同期が daemon ロックを握ったまま await すると非再入
        // Mutex の再ロックで自己デッドロックし、以後の全コマンドが固まる
        // （実 rust-analyzer で発症。if-let スコルチニーの一時 MutexGuard の寿命
        // が本体まで伸びるため）。リロード後の GetState が応答することで
        // デッドロックしていないことを検証する。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-rl-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-rl-file-{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "fn f() { TODO }\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;
        // LSP セッションが生きている（診断が返る = settle が動いている）
        let snap = poll_snapshot(
            &mut c,
            |s| !s.diagnostics.is_empty(),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(snap.diagnostics[0].start, 9, "TODO は byte 9 から");

        // 外部変更 → watch_disk がリロードし LSP 全文同期 + pull する
        std::fs::write(&file, "fn g() { TODO }\n").unwrap();
        let snap = poll_snapshot(
            &mut c,
            |s| s.text == "fn g() { TODO }\n",
            std::time::Duration::from_secs(8),
        )
        .await;
        // ここに到達できる = リロード後の LSP 同期でデッドロックしていない
        assert!(!snap.dirty, "リロード後は clean");
        // pull 診断が再同期されている（TODO の位置は byte 9 のまま）
        assert!(
            snap.diagnostics.iter().any(|d| d.start == 9),
            "診断が再同期される: {:?}",
            snap.diagnostics
        );
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn document_edit_replaces_range_without_touching_selection() {
        // ADR-0011 e2e: DocumentEdit はフォーカス文書の指定 range を置換し、
        // 選択を読まず・変えない。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-8-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-8-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "hello world").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;
        // 選択を先頭に置く（編集範囲とは別の位置）
        let _ = request(&mut c, &Command::Goto { target: GotoTarget::DocumentStart }).await;

        // [6,11) "world" → "W"（スペースは index 5）
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 6,
                end: 11,
                text: "W".into(),
                checksum: fnv1a64(b"hello world"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "hello W");
        assert!(snap.dirty, "編集で dirty になる");
        assert_eq!(snap.status, None, "成功時は status なし");
        assert_eq!(snap.selection[0].anchor, 0, "選択は不変");
        assert_eq!(snap.selection[0].head, 0, "選択は不変");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn document_edit_shrinking_doc_with_cursor_after_range_does_not_panic() {
        // CRITICAL C1: カーソルが編集範囲より後方にある DocumentEdit（文書短縮）
        // で daemon が panic しない。応答が返り、generation/ChangeEvent が記録
        // され、選択は文書末尾へクランプされるので後続の Move/Insert も panic
        // しない（修正前は選択 [11,11] が範囲外に残り scroll_to_cursor の
        // char_to_line が panic して wedge していた）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-c1-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-c1-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "hello world").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;
        // カーソルを文末（11）へ — 編集範囲 [0,11) より後方
        let _ = request(&mut c, &Command::Goto { target: GotoTarget::DocumentEnd }).await;

        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 0,
                end: 11,
                text: "hi".into(),
                checksum: fnv1a64(b"hello world"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "hi");
        assert_eq!(snap.status, None, "成功時は status なし");
        assert_eq!(
            snap.selection[0].anchor, 2,
            "範囲外になった選択は文書末尾へクランプされる"
        );
        assert_eq!(snap.selection[0].head, 2, "範囲外になった選択は文書末尾へクランプされる");
        assert_eq!(snap.generation, 2, "Open + DocumentEdit で世代が進む");
        assert_eq!(
            snap.events.last().unwrap().kind,
            EventKind::ReplaceRange,
            "ChangeEvent が記録される"
        );

        // 後続の Move/Insert も panic しない（wedge しない）
        let snap = request(
            &mut c,
            &Command::Move {
                movement: Movement::Line,
                direction: Direction::Forward,
            },
        )
        .await;
        assert_eq!(snap.text, "hi");
        assert_eq!(snap.selection[0].head, 2, "クランプ済み選択から移動する");
        let snap = request(&mut c, &Command::Insert { text: "!".into() }).await;
        assert_eq!(snap.text, "hi!");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn document_edit_insert_and_delete_share_one_variant() {
        // insert = start==end、delete = text=="" が同一バリアントで動作する
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-8b-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-8b-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "abc").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;

        // insert: [1,1) に "X"
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 1,
                end: 1,
                text: "X".into(),
                checksum: fnv1a64(b"abc"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "aXbc");
        // delete: [1,2) を空文字で置換
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 1,
                end: 2,
                text: String::new(),
                checksum: fnv1a64(b"aXbc"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "abc");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn document_edit_clamps_out_of_bounds() {
        // 範囲外（start > end、end > 文書長）は拒否せず文書長へクランプされる
        // （#25: クランプはコアの insert_at が一括処理。tmp の
        // out_of_range_clamped と同じ仕様）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-8c-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-8c-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "abc").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;
        let cs = fnv1a64(b"abc");

        // start > end → クランプ後も start > end なので Range が正規化され
        // min..max が置換対象になる
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 2,
                end: 1,
                text: "X".into(),
                checksum: cs,
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.status, None, "クランプなので拒否されない");
        assert_eq!(snap.text, "aXc");
        // end > 文書長 → end が文書末尾にクランプされ置換される
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 0,
                end: 100,
                text: "X".into(),
                checksum: fnv1a64(b"aXc"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.status, None, "クランプなので拒否されない");
        assert_eq!(snap.text, "X");
        // undo で1回で元に戻る（クランプ編集も単一トランザクション）
        let snap = request(&mut c, &Command::Undo).await;
        assert_eq!(snap.text, "aXc");
        let snap = request(&mut c, &Command::Undo).await;
        assert_eq!(snap.text, "abc");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn document_edit_rejects_stale_checksum() {
        // クライアントの読み取り後に文書が変わっていたら status で拒否
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-8d-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-8d-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "abc").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;

        // 間違ったチェックサム → 拒否
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 0,
                end: 1,
                text: "X".into(),
                checksum: 12345,
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.status.as_deref(), Some("document changed since read"));
        assert_eq!(snap.text, "abc", "文書は不変");
        // 正しいチェックサム → 適用
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 0,
                end: 1,
                text: "X".into(),
                checksum: fnv1a64(b"abc"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "Xbc");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn document_edit_is_undoable_as_one_group() {
        // DocumentEdit は共有ヒストリーで1グループとして undo/redo される
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-8e-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-8e-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "abc").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 0,
                end: 0,
                text: "X".into(),
                checksum: fnv1a64(b"abc"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "Xabc");
        let snap = request(&mut c, &Command::Undo).await;
        assert_eq!(snap.text, "abc", "undo で1グループ分戻る");
        let snap = request(&mut c, &Command::Redo).await;
        assert_eq!(snap.text, "Xabc", "redo で再適用される");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn document_edit_preempts_open_insert_group() {
        // ADR-0007: エージェントの DocumentEdit は他クライアントの書き込みとして
        // 開いた Insert グループを閉じ、Normal に戻す
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-8f-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-8f-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "abc").unwrap();
        start_server(&sock).await;

        // TUI: Insert モードで "a" を入力（グループを開く）
        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path }).await;
        let _ = request(&mut tui, &Command::SetMode { mode: Mode::Insert }).await;
        let snap = request(&mut tui, &Command::Insert { text: "a".into() }).await;
        assert_eq!(snap.mode, Mode::Insert);
        assert_eq!(snap.text, "aabc");

        // agent: DocumentEdit（別接続の書き込み → preempt）
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let snap = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 3,
                end: 3,
                text: "Z".into(),
                checksum: fnv1a64(b"aabc"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "aabZc");
        assert_eq!(snap.mode, Mode::Normal, "preempt で Insert グループが閉じて Normal に戻る");

        // TUI の undo: 最後のグループ（DocumentEdit）が戻る
        let snap = request(&mut tui, &Command::Undo).await;
        assert_eq!(snap.text, "aabc", "DocumentEdit が1グループとして戻る");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn set_mode_normal_after_focus_switch_closes_orphaned_group() {
        // M3 e2e: 文書切替時に開きっぱなしの Insert undo グループが孤児化し、
        // 別クライアントの編集が混入するバグの再現・修正確認。
        //
        // X で Insert セッション中に別クライアントが Y を Open するとフォーカスが
        // Y へ移る。その状態で TUI の SetMode(Normal) は「フォーカス中の文書」で
        // はなく「グループが開いている文書 X」に対して end_group を実行する必要が
        // ある（修正前は Y への end_group が no-op で X のグループが孤児化し、
        // 後に agent が X へ書き込むと同一 undo グループに混入した）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m3-sock-{}.sock", std::process::id()));
        let file_x = dir.join(format!("minae-m3-x-{}.txt", std::process::id()));
        let file_y = dir.join(format!("minae-m3-y-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file_x, "").unwrap();
        std::fs::write(&file_y, "").unwrap();
        start_server(&sock).await;

        // TUI: X を開いて Insert モードで "a" を入力（X の履歴にグループを開く）
        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path_x = file_x.to_string_lossy().into_owned();
        let path_y = file_y.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path: path_x.clone() }).await;
        let _ = request(&mut tui, &Command::SetMode { mode: Mode::Insert }).await;
        let snap = request(&mut tui, &Command::Insert { text: "a".into() }).await;
        assert_eq!(snap.text, "a");

        // agent: Y を Open → フォーカスは Y へ（X のグループは開いたまま）
        // （#28 で headless Open は許可されたが、このテストは undo グループ境界の
        // 検証が目的で agent が Undo を使う必要があるため Interactive で演じる —
        // Undo は #13 の制限で headless には拒否される）
        let mut agent = connect_client(&sock, ClientKind::Interactive).await;
        let snap = request(&mut agent, &Command::Open { path: path_y }).await;
        assert_eq!(snap.text, "");
        assert_eq!(snap.mode, Mode::Insert, "Open ではモードが変わらない");

        // TUI: SetMode(Normal) — フォーカスは Y だが、閉じるのは X のグループ
        let snap = request(&mut tui, &Command::SetMode { mode: Mode::Normal }).await;
        assert_eq!(snap.mode, Mode::Normal);

        // agent: X にフォーカス復帰 → DocumentEdit で書き込み
        let snap = request(&mut agent, &Command::Open { path: path_x }).await;
        assert_eq!(snap.text, "a", "再利用 Open で X の内容が保持される");
        let snap = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 1,
                end: 1,
                text: "Z".into(),
                checksum: fnv1a64(snap.text.as_bytes()),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "aZ");

        // undo: agent の編集だけが戻る（修正前は TUI の "a" も一緒に戻った）
        let snap = request(&mut agent, &Command::Undo).await;
        assert_eq!(
            snap.text, "a",
            "undo は agent の編集のみを戻す（TUI の編集が混入しない）"
        );
        let snap = request(&mut agent, &Command::Undo).await;
        assert_eq!(snap.text, "", "2回目の undo で TUI のセッションも別グループとして戻る");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file_x);
        let _ = std::fs::remove_file(&file_y);
    }

    #[tokio::test]
    async fn owner_disconnect_after_focus_switch_closes_orphaned_group() {
        // M3 e2e: フォーカスが別文書（Y）に移った状態で所有者（TUI）が切断しても、
        // グループが開いている文書 X のグループが閉じる。修正前は切断ハンドラの
        // end_group がフォーカス文書 Y に対して実行され no-op となり、X のグループ
        // が孤児化して agent の書き込みが混入した。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m3d-sock-{}.sock", std::process::id()));
        let file_x = dir.join(format!("minae-m3d-x-{}.txt", std::process::id()));
        let file_y = dir.join(format!("minae-m3d-y-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file_x, "").unwrap();
        std::fs::write(&file_y, "").unwrap();
        start_server(&sock).await;

        // TUI: X を開いて Insert モードで "a" を入力（グループは X に開く）
        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path_x = file_x.to_string_lossy().into_owned();
        let path_y = file_y.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path: path_x.clone() }).await;
        let _ = request(&mut tui, &Command::SetMode { mode: Mode::Insert }).await;
        let snap = request(&mut tui, &Command::Insert { text: "a".into() }).await;
        assert_eq!(snap.text, "a");

        // agent: Y を Open（フォーカスを X から Y へ移す）
        // （#28 で headless Open は許可されたが、このテストは切断後の Undo を
        // 確認するため Interactive で演じる — Undo は headless には拒否される）
        let mut agent = connect_client(&sock, ClientKind::Interactive).await;
        let _ = request(&mut agent, &Command::Open { path: path_y }).await;

        // 所有者（TUI）の切断 → フォーカスは Y のままでも X のグループが閉じ、
        // モードが Normal に戻るまで待つ（切断処理は非同期）
        drop(tui);
        let snap = poll_snapshot(
            &mut agent,
            |s| s.mode == Mode::Normal,
            std::time::Duration::from_secs(2),
        )
        .await;
        assert_eq!(snap.mode, Mode::Normal, "切断で Insert が閉じて Normal に戻る");

        // agent: X に戻って書き込み → undo は agent の編集のみを戻す
        let snap = request(&mut agent, &Command::Open { path: path_x }).await;
        assert_eq!(snap.text, "a");
        let snap = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 1,
                end: 1,
                text: "Z".into(),
                checksum: fnv1a64(snap.text.as_bytes()),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "aZ");
        let snap = request(&mut agent, &Command::Undo).await;
        assert_eq!(
            snap.text, "a",
            "undo は agent の編集のみを戻す（切断時にグループが閉じている）"
        );
        let snap = request(&mut agent, &Command::Undo).await;
        assert_eq!(snap.text, "", "2回目の undo で切断前のセッションも戻る");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file_x);
        let _ = std::fs::remove_file(&file_y);
    }

    #[tokio::test]
    async fn document_edit_syncs_lsp_full_text() {
        // ADR-0009: DocumentEdit も編集後の全文 didChange 同期 + pull 診断に乗る
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-8lsp-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-8lsp-file-{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "fn f() { TODO }\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;
        // 初回解析: TODO（byte 9）の診断が反映されるまで待つ
        let _ = poll_snapshot(
            &mut c,
            |s| !s.diagnostics.is_empty(),
            std::time::Duration::from_secs(10),
        )
        .await;

        // DocumentEdit: 先頭に挿入 → didChange 全文同期 → 診断位置が追従する
        let snap = request_edit(
            &mut c,
            &DocumentEdit {
                start: 0,
                end: 0,
                text: "aaaa".into(),
                checksum: fnv1a64(b"fn f() { TODO }\n"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "aaaafn f() { TODO }\n");
        let snap = poll_snapshot(
            &mut c,
            |s| s.diagnostics.iter().any(|d| d.start == 13),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(snap.diagnostics[0].start, 13, "didChange 同期後の位置に追従");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn rename_applies_workspace_edit_saves_and_reports_impact() {
        // ADR-0029: 内容指定の意味リネームが mock の WorkspaceEdit（documentChanges
        // 形式）を適用・保存し、影響範囲（files/edits/changed）を軽量応答で返す。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        // フィクスチャは PID スコープのサブディレクトリに置く（temp 直下に置くと
        // workspace 走査（root=temp 全体）が他の残骸 .rs を取り込み結果が揺れる —
        // 敵対的検証で発見。Drop ガードで失敗時も残骸を残さない）。
        let work = dir.join(format!("minae-rn-work-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&work);
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(work.clone());
        let sock = dir.join(format!("minae-rn-sock-{}.sock", std::process::id()));
        let file = work.join("fixture.rs");
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "let fee = 1\nlet tax = fee + 2\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;
        // 初回解析が mock に載るのを待つ（didOpen 後の settle 用）
        let _ = poll_snapshot(
            &mut c,
            |s| s.activities.is_empty(), // 診断 settle の完了待ち（診断の有無ではない）
            std::time::Duration::from_secs(10),
        )
        .await;

        // リネーム: old の最初の識別子出現（fee の定義）を解決 → mock は全出現を
        // documentChanges で返す → daemon が適用・保存
        let mut line = serde_json::to_string(&Command::Rename {
            path: path.clone(),
            old: "fee".into(),
            new: "dues".into(),
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        let msg = c.recv_message().await;
        match msg {
            ServerMessage::RenameResult {
                generation,
                files,
                edits,
                changed,
                error,
                ..
            } => {
                assert_eq!(error, None, "成功応答");
                assert!(generation > 0, "編集+保存で世代が進む");
                assert_eq!(files, 1);
                assert_eq!(edits, 2, "定義と使用の2箇所");
                assert_eq!(changed.len(), 1);
                assert!(changed[0].ends_with(".rs"));
            }
            other => panic!("想定外の応答: {other:?}"),
        }
        // ディスクへ保存されている
        let contents = std::fs::read_to_string(&file).unwrap();
        assert_eq!(contents, "let dues = 1\nlet tax = dues + 2\n");
        // 開いている文書も更新されている（スナップショットで確認）
        let snap = request(&mut c, &Command::GetState).await;
        assert_eq!(snap.text, "let dues = 1\nlet tax = dues + 2\n");
        assert!(!snap.dirty, "保存済みなので dirty でない");

        // 未存在シンボルの rename は error（再試行可能）
        let mut line = serde_json::to_string(&Command::Rename {
            path: path.clone(),
            old: "nosuch".into(),
            new: "x".into(),
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        let msg = c.recv_message().await;
        match msg {
            ServerMessage::RenameResult { error, .. } => {
                let e = error.expect("not found は error");
                assert!(e.contains("nosuch"), "{e}");
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn references_lists_symbol_locations() {
        // ADR-0029: 内容指定の参照列挙が positions を軽量応答で返す。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        // フィクスチャは PID スコープのサブディレクトリに置く（temp 直下に置くと
        // workspace 走査（root=temp 全体）が他の残骸 .rs を取り込み結果が揺れる —
        // 敵対的検証で発見。Drop ガードで失敗時も残骸を残さない）。
        let work = dir.join(format!("minae-ref-work-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&work);
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(work.clone());
        let sock = dir.join(format!("minae-ref-sock-{}.sock", std::process::id()));
        let file = work.join("fixture.rs");
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "let fee = 1\nlet tax = fee + 2\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await;
        let _ = poll_snapshot(
            &mut c,
            |s| s.activities.is_empty(), // 診断 settle の完了待ち（診断の有無ではない）
            std::time::Duration::from_secs(10),
        )
        .await;

        let mut line = serde_json::to_string(&Command::References {
            path: path.clone(),
            old: "fee".into(),
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        let msg = c.recv_message().await;
        match msg {
            ServerMessage::ReferencesResult {
                locations,
                total,
                error,
                ..
            } => {
                assert_eq!(error, None);
                assert_eq!(total, 2);
                assert_eq!(locations.len(), 2);
                assert_eq!(locations[0].line, 0, "定義行（includeDeclaration）");
                assert_eq!(locations[1].line, 1, "使用行");
            }
            other => panic!("想定外の応答: {other:?}"),
        }
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn outline_and_enclosing_symbol_via_mock() {
        // ADR-0031: documentSymbol のツリーと位置→記号範囲が軽量応答で返る。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        let work = dir.join(format!("minae-outline-work-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&work);
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(work.clone());
        let sock = dir.join(format!("minae-outline-sock-{}.sock", std::process::id()));
        let file = work.join("fixture.rs");
        let _ = std::fs::remove_file(&sock);
        // 行ごとに `fn / struct / let` が 1 つずつ。mock は行単位のフラットな
        // アウトラインを返す（木構造の変換は lsp.rs のユニットテストが検証）。
        std::fs::write(&file, "pub fn run() {}\nstruct Thing;\nfn go() {}\nlet top = 1;\n")
            .unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();

        // Outline: 4 記号がフラットに返り、kind・selection_range（名前トークン）
        // が正しい char インデックスで載る。エラーなし。
        let mut line = serde_json::to_string(&Command::Outline {
            path: path.clone(),
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::Outline {
                symbols,
                error,
                ..
            } => {
                assert_eq!(error, None, "エラーなし: {symbols:?}");
                assert_eq!(symbols.len(), 4, "記号数: {symbols:?}");
                let thing = symbols.iter().find(|s| s.name == "Thing").unwrap();
                assert_eq!(
                    thing.kind,
                    mina_protocol::SymbolKind::Type,
                    "struct → Type: {thing:?}"
                );
                assert_eq!(
                    thing.selection_range.anchor,
                    23,
                    "名前トークン Thing の char 位置: {thing:?}"
                );
                assert_eq!(thing.selection_range.head, 28);
                assert!(symbols.iter().any(|s| s.kind == mina_protocol::SymbolKind::Function));
                assert!(symbols.iter().any(|s| s.kind == mina_protocol::SymbolKind::Variable));
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        // EnclosingSymbol: 2行目（1-origin）の構造体宣言の内部 → Thing とその範囲。
        let mut line = serde_json::to_string(&Command::EnclosingSymbol {
            path: path.clone(),
            line: 2,
            col: 9,
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::EnclosingSymbol {
                name,
                kind,
                range,
                selection_range,
                found,
                error,
                ..
            } => {
                assert_eq!(error, None);
                assert!(found, "2行目は struct Thing の中: {name}");
                assert_eq!(name, "Thing");
                assert_eq!(kind, mina_protocol::SymbolKind::Type);
                assert_eq!(range.anchor, 16, "struct 行全体: {range:?}");
                assert_eq!(range.head, 29, "行全体（`;` まで）: {range:?}");
                assert_eq!(selection_range.anchor, 23);
                assert_eq!(selection_range.head, 28);
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        // どの記号にも含まれない位置（行数超過）→ found: false の成功応答。
        let mut line = serde_json::to_string(&Command::EnclosingSymbol {
            path: path.clone(),
            line: 99,
            col: 1,
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::EnclosingSymbol {
                found, error, ..
            } => {
                assert_eq!(error, None);
                assert!(!found, "範囲外は記号なし");
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn hover_symbol_check_via_mock() {
        // ADR-0032: hover / workspace symbol / check の3コマンドが軽量応答で返る。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        let work = dir.join(format!("minae-hsc-work-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&work);
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(work.clone());
        let sock = dir.join(format!("minae-hsc-sock-{}.sock", std::process::id()));
        let file = work.join("fixture.rs");
        let _ = std::fs::remove_file(&sock);
        // fn / struct / let が行ごとに1つ。1行目に TODO（診断）を仕込む。
        std::fs::write(&file, "pub fn run() { TODO }\nstruct Thing;\nfn go() {}\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();

        // HoverAt: 1行目 8文字目（run）の hover は mock が「型シグネチャ + doc」を返す。
        let mut line = serde_json::to_string(&Command::HoverAt {
            path: path.clone(),
            line: 1,
            col: 8,
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::Hover { text, error, .. } => {
                assert_eq!(error, None, "エラーなし: {error:?}");
                assert!(text.contains("run"), "型シグネチャにシンボル名: {text:?}");
                assert!(text.contains("mock doc for run"), "doc も連結: {text:?}");
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        // WorkspaceSymbol: 1行目（run）の担当 root に対して 'Thing' を検索 →
        // struct Thing が 2行目でヒットする。
        let mut line = serde_json::to_string(&Command::WorkspaceSymbol {
            path: path.clone(),
            query: "Thing".into(),
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::WorkspaceSymbols { symbols, error, .. } => {
                assert_eq!(error, None, "エラーなし: {error:?}");
                assert_eq!(symbols.len(), 1, "Thing が1件: {symbols:?}");
                assert_eq!(symbols[0].name, "Thing");
                assert_eq!(symbols[0].kind, mina_protocol::SymbolKind::Type);
                assert_eq!(symbols[0].line, 2, "1-origin 行");
                assert!(
                    symbols[0].path.ends_with("fixture.rs"),
                    "file:// が剥がれて絶対パス: {}",
                    symbols[0].path
                );
            }
            other => panic!("想定外の応答: {other:?}"),
        }
        // 空クエリは入力エラー（exit 1 相当の error）
        let mut line = serde_json::to_string(&Command::WorkspaceSymbol {
            path: path.clone(),
            query: "".into(),
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::WorkspaceSymbols { symbols, error, .. } => {
                assert_eq!(symbols.len(), 0);
                assert!(
                    error.as_deref().unwrap_or("").starts_with("invalid input"),
                    "空クエリは invalid input: {error:?}"
                );
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        // CheckDiagnostics: 1行目の TODO が error 診断として、1-origin 行 1・
        // 行内列 16（"pub fn run() { " の 15 chars の次）で返る。
        let mut line =
            serde_json::to_string(&Command::CheckDiagnostics { path: path.clone() }).unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::Check {
                total,
                diagnostics,
                error,
                ..
            } => {
                assert_eq!(error, None, "エラーなし: {error:?}");
                assert_eq!(total, 1, "TODO 診断が1件: {diagnostics:?}");
                assert_eq!(diagnostics[0].severity, mina_protocol::Severity::Error);
                assert_eq!(diagnostics[0].line, 1, "1-origin 行");
                assert_eq!(diagnostics[0].col, 16, "1-origin 行内列（ADR-0047）");
                assert_eq!(diagnostics[0].message, "mock: TODO found");
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn generation_increments_on_state_changes_only() {
        // ADR-0012: 世代は状態を変える操作（Open/編集/undo）で増加し、
        // GetState（読み取り）では不変。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-9a-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-9a-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "abc").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut c, &Command::Open { path: path.clone() }).await;
        assert_eq!(snap.generation, 1, "Open で増加");
        let snap = request(&mut c, &Command::Insert { text: "X".into() }).await;
        assert_eq!(snap.generation, 2, "編集で増加");
        let snap = request(&mut c, &Command::Undo).await;
        assert_eq!(snap.generation, 3, "undo で増加");
        let snap = request(&mut c, &Command::SetMode { mode: Mode::Insert }).await;
        assert_eq!(snap.generation, 4, "モード変更で増加");
        let snap = request(&mut c, &Command::SetMode { mode: Mode::Insert }).await;
        assert_eq!(snap.generation, 4, "同じモードへの SetMode は増加しない");
        let snap = request(&mut c, &Command::GetState).await;
        assert_eq!(snap.generation, 4, "GetState では不変");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn events_carry_correct_source() {
        // ADR-0012: イベントの source は Hello で宣言したクライアント種別で決まる
        // （TUI = Interactive、agent = Headless）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-9b-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-9b-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "abc").unwrap();
        start_server(&sock).await;

        // TUI（Interactive）: Open + Insert
        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut tui, &Command::Open { path: path.clone() }).await;
        assert!(snap.events.iter().any(|e| {
            e.kind == EventKind::Open && e.source == EventSource::Interactive
        }));
        let snap = request(&mut tui, &Command::Insert { text: "X".into() }).await;
        assert!(snap.events.iter().any(|e| {
            e.kind == EventKind::Insert
                && e.source == EventSource::Interactive
                && e.text.as_deref() == Some("X")
        }));

        // agent（Headless）: DocumentEdit → ReplaceRange
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let snap = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 1,
                end: 1,
                text: "Y".into(),
                checksum: fnv1a64(b"Xabc"),
                expected_text: None,
            },
        )
        .await;
        assert!(snap.events.iter().any(|e| {
            e.kind == EventKind::ReplaceRange
                && e.source == EventSource::Headless
                && e.text.as_deref() == Some("Y")
        }));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn event_ring_is_bounded_and_evicts_oldest() {
        // ADR-0012: リングは bounded(128)。超過分は古いものから破棄される。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-9c-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-9c-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut c, &Command::Open { path: path.clone() }).await; // gen 1
        // 130 回編集 → 合計 131 イベント。リングは 128 に収まる
        for i in 0..130 {
            let _ = request(&mut c, &Command::Insert { text: format!("{i}") }).await;
        }
        let snap = request(&mut c, &Command::GetState).await;
        assert_eq!(snap.events.len(), 128, "リングは 128 に bounded");
        assert_eq!(snap.events[0].kind, EventKind::Insert, "Open イベントは破棄されている");
        assert_eq!(
            snap.events[127].kind, EventKind::Insert,
            "最新イベントは保持される"
        );
        assert_eq!(snap.generation, 131, "世代は全状態変化分進んでいる");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn hello_handshake_rejects_non_hello() {
        // ADR-0012: 最初のメッセージが Hello でない・不正な kind なら切断される。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-9d-sock-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        start_server(&sock).await;

        // Hello でなくコマンドを直接送る → 切断（EOF）
        let mut raw = UnixStream::connect(&sock).await.unwrap();
        raw.write_all(b"{\"GetState\": null}\n").await.unwrap();
        let mut buf = [0u8; 16];
        let n = raw.read(&mut buf).await;
        assert!(
            matches!(n, Ok(0) | Err(_)),
            "Hello でない最初のメッセージは切断される: {n:?}"
        );

        // 不正な kind → 切断（EOF）
        let mut raw = UnixStream::connect(&sock).await.unwrap();
        raw.write_all(b"{\"kind\": \"bogus\"}\n").await.unwrap();
        let mut buf = [0u8; 16];
        let n = raw.read(&mut buf).await;
        assert!(matches!(n, Ok(0) | Err(_)), "不正な kind は切断される: {n:?}");
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn external_change_auto_reloads_and_is_undoable() {
        // ADR-0015: 外部ツールによるファイル変更は自動リロードされる（Dirty でも
        // 常時）。リロードは Transaction なので undo で外部変更前の状態に戻れる。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-9e-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-9e-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut c, &Command::Open { path: path.clone() }).await;
        assert_eq!(snap.text, "base\n");
        assert!(snap.deleted.is_none());

        // 外部ツールがファイルを書き換える（daemon は知らない）
        std::fs::write(&file, "changed by external tool\n").unwrap();

        // 監視タスク（2秒周期）が検知して自動リロードするのを待つ
        let snap = poll_snapshot(
            &mut c,
            |s| s.text == "changed by external tool\n",
            std::time::Duration::from_secs(8),
        )
        .await;
        assert!(snap.events.iter().any(|e| {
            e.kind == EventKind::ExternalChange && e.source == EventSource::External
        }));
        assert!(!snap.dirty, "リロード後はテキストがディスクと一致するので clean");

        // リロードは undo 可能（外部変更前の状態に戻れる）
        let snap = request(&mut c, &Command::Undo).await;
        assert_eq!(snap.text, "base\n", "undo で外部変更前の状態に戻る");

        // Save でベースラインが更新される
        let snap = request(&mut c, &Command::Save).await;
        assert!(snap.status.as_deref().unwrap_or("").starts_with("saved:"));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn reuse_open_reloads_diverged_document() {
        // ADR-0015: 再利用 Open（既存文書へのフォーカス復帰）でも、ディスクと
        // 乖離していれば watch_disk の 2 秒周期を待たずに自動リロードする
        // （TUI 再起動のシナリオ: 閉じている間に外部編集 → 再オープンで即反映）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m2r-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-m2r-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut c, &Command::Open { path: path.clone() }).await;
        assert_eq!(snap.text, "base\n");

        // 外部ツールがファイルを書き換える（2 秒周期の検知より先に再利用 Open）
        std::fs::write(&file, "changed by external tool\n").unwrap();
        let snap = request(&mut c, &Command::Open { path: path.clone() }).await;
        assert_eq!(
            snap.text,
            "changed by external tool\n",
            "再利用 Open で即リロードされる"
        );
        assert!(snap.events.iter().any(|e| {
            e.kind == EventKind::ExternalChange && e.source == EventSource::External
        }));

        // ベースラインが新しくなっていること: 保存後は B を外部変更しても
        // フォーカス文書 A のテキストは変わらない（B は別文書としてリロード
        // されるが、スナップショットはフォーカス文書を見せる）
        let snap = request(&mut c, &Command::Save).await;
        assert!(snap.status.as_deref().unwrap_or("").starts_with("saved:"));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn external_delete_sets_deleted_until_close() {
        // ADR-0015: フォーカス文書が外部削除されたら deleted 状態を立てて保留し、
        // Command::Close で空画面（ファイル未オープン）に戻る。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m2k-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-m2k-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut c, &Command::Open { path: path.clone() }).await;
        assert!(snap.deleted.is_none());
        let norm = snap.path.clone().expect("正規化されたパス");

        // 外部ツールがファイルを削除する → 検知される
        std::fs::remove_file(&file).unwrap();
        let snap = poll_snapshot(
            &mut c,
            |s| s.deleted.is_some(),
            std::time::Duration::from_secs(8),
        )
        .await;
        assert_eq!(snap.deleted.as_deref(), Some(norm.as_str()));
        assert_eq!(snap.text, "base\n", "削除保留中はテキストを保持する");

        // Close で空画面（ファイル未オープン）に戻る
        let snap = request(&mut c, &Command::Close).await;
        assert!(snap.deleted.is_none());
        assert!(snap.path.is_none(), "空画面: ファイル未オープン");
        assert!(snap.events.iter().any(|e| e.kind == EventKind::Close));
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn close_moves_to_next_document_then_empty() {
        // ADR-0015: Close はフォーカス文書を閉じ、残りの文書があればそこへ移る。
        // 最後の文書を閉じると空画面（スクラッチ）になる。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-cls-sock-{}.sock", std::process::id()));
        let file_a = dir.join(format!("minae-cls-a-{}.txt", std::process::id()));
        let file_b = dir.join(format!("minae-cls-b-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file_a);
        let _ = std::fs::remove_file(&file_b);
        std::fs::write(&file_a, "a\n").unwrap();
        std::fs::write(&file_b, "b\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let pa = file_a.to_string_lossy().into_owned();
        let pb = file_b.to_string_lossy().into_owned();
        let snap = request(&mut c, &Command::Open { path: pa.clone() }).await;
        assert_eq!(snap.text, "a\n");
        let pa_norm = snap.path.clone().expect("正規化されたパス");
        let snap = request(&mut c, &Command::Open { path: pb.clone() }).await;
        assert_eq!(snap.text, "b\n");

        // B（フォーカス）を閉じる → 残りの A へ移る
        let snap = request(&mut c, &Command::Close).await;
        assert_eq!(
            snap.path.as_deref(),
            Some(pa_norm.as_str()),
            "残りの文書 A へ移る"
        );
        assert_eq!(snap.text, "a\n");

        // A を閉じる → 空画面
        let snap = request(&mut c, &Command::Close).await;
        assert!(snap.path.is_none(), "空画面: ファイル未オープン");
        assert_eq!(snap.text, "", "空文書");

        // スクラッチ（未保存）文書の Close は no-op
        let gen_before = request(&mut c, &Command::GetState).await.generation;
        let snap = request(&mut c, &Command::Close).await;
        assert_eq!(
            snap.generation, gen_before,
            "スクラッチの Close は世代を進めない"
        );
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file_a);
        let _ = std::fs::remove_file(&file_b);
    }

    #[tokio::test]
    async fn external_change_reloads_non_focused_open_document() {
        // ADR-0015: 監視対象は全オープン文書（フォーカス限定でない）。
        // 非フォーカスの文書が外部変更されてもリロードされ、フォーカス文書の
        // テキストは変わらない。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m8-sock-{}.sock", std::process::id()));
        let file_a = dir.join(format!("minae-m8-a-{}.txt", std::process::id()));
        let file_b = dir.join(format!("minae-m8-b-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file_a);
        let _ = std::fs::remove_file(&file_b);
        std::fs::write(&file_a, "a\n").unwrap();
        std::fs::write(&file_b, "b\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let pa = file_a.to_string_lossy().into_owned();
        let pb = file_b.to_string_lossy().into_owned();
        request(&mut c, &Command::Open { path: pa.clone() }).await;
        request(&mut c, &Command::Open { path: pb.clone() }).await;
        let snap = request(&mut c, &Command::Open { path: pa }).await; // フォーカスを A へ戻す
        assert_eq!(snap.text, "a\n");

        // 非フォーカスの B を外部変更 → 検知され B だけがリロードされる
        std::fs::write(&file_b, "b changed\n").unwrap();
        let snap = poll_snapshot(
            &mut c,
            |s| {
                s.events.iter().any(|e| {
                    e.kind == EventKind::ExternalChange && e.source == EventSource::External
                })
            },
            std::time::Duration::from_secs(8),
        )
        .await;
        assert_eq!(snap.text, "a\n", "フォーカス文書 A は変わらない");

        // B を再オープン: watch_disk が既にリロード・ベースライン更新済みなので
        // 再利用で新しいテキストが見える
        let snap = request(&mut c, &Command::Open { path: pb }).await;
        assert_eq!(snap.text, "b changed\n", "非フォーカスでもリロードされている");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file_a);
        let _ = std::fs::remove_file(&file_b);
    }

    #[tokio::test]
    async fn external_reload_closes_insert_group_and_returns_to_normal() {
        // ADR-0007/0015: 外部リロードは Insert UndoGroup を閉じモードを Normal に
        // 戻す（外部書き込みと同原則）。undo はリロードだけを戻し、挿入と混ざらない。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m9-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-m9-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut c, &Command::Open { path: path.clone() }).await;
        assert_eq!(snap.text, "base\n");
        let snap = request(&mut c, &Command::SetMode { mode: Mode::Insert }).await;
        assert_eq!(snap.mode, Mode::Insert);
        let snap = request(&mut c, &Command::Insert { text: "X".into() }).await;
        assert_eq!(snap.text, "Xbase\n");

        // Insert セッション中に外部変更 → リロードでグループが閉じ Normal に戻る
        std::fs::write(&file, "changed\n").unwrap();
        let snap = poll_snapshot(
            &mut c,
            |s| s.text == "changed\n",
            std::time::Duration::from_secs(8),
        )
        .await;
        assert_eq!(snap.mode, Mode::Normal, "外部リロードで Normal に戻る");

        // undo はリロードだけを戻す（挿入と別グループ）
        let snap = request(&mut c, &Command::Undo).await;
        assert_eq!(snap.text, "Xbase\n", "undo でリロードだけ戻る");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn save_recreates_deleted_file_and_clears_state() {
        // ADR-0015: 削除状態で Save するとファイルを再作成し、deleted が解消される。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m10-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-m10-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        request(&mut c, &Command::Open { path: path.clone() }).await;
        std::fs::remove_file(&file).unwrap();
        let snap = poll_snapshot(
            &mut c,
            |s| s.deleted.is_some(),
            std::time::Duration::from_secs(8),
        )
        .await;
        assert!(snap.deleted.is_some());

        // Save でファイルが再作成され、削除状態が解消される
        let snap = request(&mut c, &Command::Save).await;
        assert!(snap.status.as_deref().unwrap_or("").starts_with("saved:"));
        assert!(snap.deleted.is_none(), "Save で削除状態が解消される");
        assert!(std::fs::metadata(&file).is_ok(), "ファイルが再作成される");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn stalled_response_writer_is_dropped_and_daemon_recovers() {
        // MEDIUM-2: 応答を読まないクライアントは書き込みタイムアウトで切断され、
        // 接続スロット（MAX_CONNECTIONS=4）を永久に占有しない。占有は「新しい
        // クライアントが応答を受け取れること」で観測する（修正前はスロットが
        // 戻らず永久に待つ）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m2-sock-{}.sock", std::process::id()));
        let big = dir.join(format!("minae-m2-big-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&big);
        // socket バッファ（macOS 8KB / Linux ~208KB）を超える応答（8MB）を作る
        std::fs::write(&big, "a".repeat(8 * 1024 * 1024)).unwrap();
        let big_str = big.to_string_lossy().into_owned();
        start_server(&sock).await;

        // 巨大文書を読み込んでおく（GetState の応答を大きくするため）
        let mut loader = connect_client(&sock, ClientKind::Interactive).await;
        let _ = request(&mut loader, &Command::Open { path: big_str }).await;
        drop(loader);

        // DoS シナリオ: GetState を送って一切読まないクライアントを放置する
        // （書き込みがバッファで詰まり、テストでは 500ms のタイムアウトで切断）
        let mut wedged = connect_client(&sock, ClientKind::Interactive).await;
        let mut line = serde_json::to_string(&Command::GetState).unwrap();
        line.push('\n');
        wedged.send(line.as_bytes()).await;
        // wedged はこの後一切読まない（ソケットは開いたまま）

        // 新しいクライアント: タイムアウトでスロットが解放されるまで待って応答を
        // 受け取れる。修正前は wedged のスロットが戻らず 5 秒待っても応答が来ない。
        let mut fresh = connect_client(&sock, ClientKind::Interactive).await;
        let snap = timeout(
            std::time::Duration::from_secs(5),
            request(&mut fresh, &Command::GetState),
        )
        .await
        .expect("スロットが解放され応答が返る");
        assert_eq!(snap.text.len(), 8 * 1024 * 1024);
        drop(wedged);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&big);
    }

    #[tokio::test]
    async fn silent_connections_time_out_and_free_slots() {
        // MEDIUM-5: 接続後に何も送らない無言接続は、最初のコマンドのタイムアウト
        // （テスト 500ms）で切断されスロット（MAX_CONNECTIONS=4）が解放される。
        // 修正前は 4 本の無言接続で全スロットが永久に枯渇し、5 台目のクライアント
        // のリクエストが永久にハングした。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m5-sock-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        start_server(&sock).await;

        // 全スロットを無言接続で埋める（何も送らない）
        let mut silent = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            silent.push(UnixStream::connect(&sock).await.expect("接続できる"));
        }

        // 5 台目: 最初はスロット待ちだが、無言接続がタイムアウトで切断されると
        // accept されて応答が返る。修正前は永久に待つ（5 秒でタイムアウト判定）。
        let mut fresh = connect_client(&sock, ClientKind::Interactive).await;
        let snap = timeout(
            std::time::Duration::from_secs(5),
            request(&mut fresh, &Command::GetState),
        )
        .await
        .expect("無言接続が切断されスロットが解放されて応答が返る");
        assert_eq!(snap.text, "");

        drop(silent);
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn delete_selection_exits_select_mode() {
        let mut d = daemon();
        open(&mut d, "hello");
        apply(&mut d, Command::SetMode { mode: Mode::Select });
        for _ in 0..3 {
            apply(&mut d, Command::Extend {
                movement: Movement::Char,
                direction: Direction::Forward,
            });
        }
        let s = apply(&mut d, Command::DeleteRange);
        assert_eq!(s.text, "lo");
        assert_eq!(s.mode, Mode::Normal, "選択削除後は Normal に戻る");
    }

    #[test]
    fn save_writes_file_and_clears_dirty() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("minae-save-test-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_str = path.to_string_lossy().into_owned();

        let mut d = daemon();
        open_path(&mut d, &path_str, "hello");
        apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        apply(&mut d, Command::Insert { text: " world".into() });

        // Save は接続ハンドラ相当のロジック（テストでは直接実行）
        let (text, p, doc_id) = {
            let text = d.editor.current_document().text().to_string();
            (text, d.editor.focused_path().map(Path::to_path_buf), d.editor.focused_doc_id())
        };
        let p = p.expect("パスがある");
        std::fs::write(&p, text.as_bytes()).unwrap();
        assert!(d.editor.mark_saved_doc(doc_id, &text));
        let s = snapshot(&mut d, Some("saved".into()));

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
        assert!(!s.dirty);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_marks_the_saved_doc_not_the_current_focus() {
        // H3: Save の書き込み中に他接続が Open するとフォーカスが変わる。
        // 保存した文書（保存開始時点の ID）の dirty を消し、新フォーカスの
        // dirty（未保存編集）は残す — 修正前は新フォーカス文書の dirty まで
        // 消えて「保存済み」誤認（データ損失）につながった。
        let dir = std::env::temp_dir();
        let path_a = dir.join(format!("minae-h3-a-{}.txt", std::process::id()));
        let path_b = dir.join(format!("minae-h3-b-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
        let path_a_str = path_a.to_string_lossy().into_owned();
        let path_b_str = path_b.to_string_lossy().into_owned();

        let mut d = daemon();
        // A を開いて編集（dirty）
        open_path(&mut d, &path_a_str, "hello");
        apply(&mut d, Command::Insert { text: " X".into() });
        // Save の保存対象を捕捉（接続ハンドラのロック解放前の処理に相当）
        let doc_id_a = d.editor.focused_doc_id();
        let text_a = d.editor.current_document().text().to_string();
        // 書き込み中に他接続が B を開いて編集（フォーカスが B に移動）
        open_path(&mut d, &path_b_str, "world");
        apply(&mut d, Command::Insert { text: " Y".into() });
        // 書き込み完了 → 保存した文書 A の dirty を消す
        std::fs::write(&path_a, text_a.as_bytes()).unwrap();
        assert!(d.editor.mark_saved_doc(doc_id_a, &text_a));

        // スナップショットはフォーカス（B）の状態: B の未保存編集は dirty のまま
        let s = snapshot(&mut d, Some("saved".into()));
        assert!(s.dirty, "B の未保存編集が保存済み扱いにならない: {}", s.dirty);
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    #[test]
    fn save_keeps_dirty_if_edited_during_write() {
        // HIGH-1: Save の書き込み中に他接続が同じ文書を編集すると、保存した
        // のは書き込み開始時点の古いテキスト。dirty を消してしまうと未保存の
        // 編集が「保存済み」と誤表示され、quit で失われる（データ損失）。
        let dir = std::env::temp_dir();
        let path = dir.join(format!("minae-high1-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_str = path.to_string_lossy().into_owned();

        let mut d = daemon();
        open_path(&mut d, &path_str, "hello");
        // 保存対象を捕捉（接続ハンドラのロック解放前の処理に相当）
        let doc_id = d.editor.focused_doc_id();
        let text = d.editor.current_document().text().to_string();
        // 書き込み中に他接続が編集（dirty になる）
        apply(&mut d, Command::Insert { text: " X".into() });
        assert!(d.editor.is_dirty(), "書き込み中の編集で dirty");

        // 書き込み完了 → 古いテキストでは dirty が消えない
        std::fs::write(&path, text.as_bytes()).unwrap();
        assert!(!d.editor.mark_saved_doc(doc_id, &text));
        let s = snapshot(&mut d, Some("saved".into()));
        assert!(s.dirty, "書き込み中に編集された文書が保存済み扱いにならない: {}", s.dirty);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn undo_after_save_marks_dirty() {
        // バグ修正: 保存後に undo するとテキストがディスクと乖離するのに dirty が
        // false のままだった（undo/redo は dirty を触らなかった）。undo/redo は
        // 文書を変えるので常に dirty を立てる — 履歴に保存時点のマーカーを
        // 持たない割り切り（is_dirty の ponytail コメント）と整合。
        let dir = std::env::temp_dir();
        let path = dir.join(format!("minae-undo-dirty-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_str = path.to_string_lossy().into_owned();

        let mut d = daemon();
        open_path(&mut d, &path_str, "hello");
        // グループ1: "a" 挿入
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "a".into() });
        apply(&mut d, Command::SetMode { mode: Mode::Normal });
        // グループ2: "b" 挿入
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "b".into() });
        apply(&mut d, Command::SetMode { mode: Mode::Normal });

        // 保存（"abhello" をディスクに書き、dirty をクリア）
        let (text, doc_id) = {
            let text = d.editor.current_document().text().to_string();
            (text, d.editor.focused_doc_id())
        };
        std::fs::write(&path, text.as_bytes()).unwrap();
        assert!(d.editor.mark_saved_doc(doc_id, &text));
        assert!(!d.editor.is_dirty());

        // undo でグループ2 だけ戻る → text="ahello" ≠ disk="abhello" → dirty
        apply(&mut d, Command::Undo);
        assert_eq!(d.editor.current_document().text().to_string(), "ahello");
        assert!(d.editor.is_dirty(), "undo でテキストがディスクと乖離 → dirty のはず");

        // redo で復元しても dirty は残る（保存時点のマーカーを持たないため）
        apply(&mut d, Command::Redo);
        assert_eq!(d.editor.current_document().text().to_string(), "abhello");
        assert!(d.editor.is_dirty(), "redo 後も保守的に dirty");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn move_advances_cursor() {
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(
            &mut d,
            Command::Move {
                movement: Movement::Char,
                direction: Direction::Forward,
            },
        );
        assert_eq!(s.selection[0].head, 1);
    }

    #[test]
    fn goto_end_scrolls_viewport() {
        let mut d = daemon();
        let text = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\nu\nv\nw\nx\ny\nz\n";
        open(&mut d, text);
        apply(&mut d, Command::SetViewport { height: 5 }, );
        let s = apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        assert_eq!(s.selection[0].head, text.chars().count());
        assert!(s.first_line > 0, "カーソルに追従してスクロールする: {}", s.first_line);
    }

    #[tokio::test]
    async fn socket_file_mode_is_0600() {
        // MEDIUM-3: serve 後の socket ファイルは 0600。修正前はプロセス umask
        // （通常 0022）で 0755 になり、共有 /tmp の別ユーザから接続・spoofing
        // できた。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-m3-mode-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        start_server(&sock).await;

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket の mode は 0600: {mode:o}");
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn peer_uid_mismatch_is_rejected() {
        // MEDIUM-3: 純関数の uid 判定 — uid が一致する接続のみ許可する
        // （別ユーザの偽 daemon / 状態破壊を拒否）。
        let uid = unsafe { libc::getuid() };
        assert!(is_peer_allowed(uid, uid), "同一 uid は許可");
        assert!(!is_peer_allowed(uid.wrapping_add(1), uid), "別ユーザは拒否");
        assert!(!is_peer_allowed(0, uid), "root でも別 uid なら拒否");
    }

    #[tokio::test]
    async fn interactive_client_receives_push_of_headless_edit() {
        // ADR-0013: Interactive クライアントは他クライアント（headless）の編集を
        // push で受ける。push はタグ付きエンベロープで届き、テキスト・世代が
        // 応答と整合する。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-13a-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-13a-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path }).await;
        // Open の自分の push は届かない（発信元スキップ — 応答で状態を持つ）

        // agent の位置指定編集 → TUI へ push が届く
        let resp = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 0,
                end: 0,
                text: "hi".into(),
                checksum: fnv1a64(b""),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(resp.text, "hi");

        let pushed = recv_push(&mut tui).await;
        assert_eq!(pushed.text, "hi");
        assert_eq!(pushed.generation, resp.generation);
        assert!(
            pushed
                .events
                .iter()
                .any(|e| e.kind == EventKind::ReplaceRange && e.source == EventSource::Headless),
            "push に headless 編集の ChangeEvent が載る"
        );
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn own_push_is_skipped_for_originator_but_reaches_other_clients() {
        // ADR-0013: 発信元自身には push が届かない（応答で同じ状態を持つ —
        // 自分宛 push の JSON 往復を節約）。他の Interactive クライアントに
        // は届く。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-13b-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-13b-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "").unwrap();
        start_server(&sock).await;

        let mut a = connect_client(&sock, ClientKind::Interactive).await;
        let mut b = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut a, &Command::Open { path: path.clone() }).await;
        // v12（ADR-0037）: push は購読者自身の View から合成される — 自分の View
        // が対象文書を見ていないと編集は自分のスナップショットに現れない。
        let _ = request(&mut b, &Command::Open { path }).await;
        let _ = recv_push(&mut a).await; // b の Open は a へ届く（a 側で消費）

        let resp = request(&mut a, &Command::Insert { text: "x".into() }).await;
        // 発信元 a には自分の push が届かない（タイムアウトで確認）
        let own_push = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            a.recv_message(),
        )
        .await;
        assert!(own_push.is_err(), "発信元に自分の push は届かない");
        // 他クライアント b には届く（応答と同じ世代）
        let pushed = recv_push(&mut b).await;
        assert_eq!(
            pushed.generation, resp.generation,
            "他クライアントの push は応答と同じ世代"
        );
        assert_eq!(pushed.text, "x");
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn activity_ring_is_bounded() {
        // ADR-0038: activity リングは 100 件。超過分は古いものから破棄する。
        let mut d = daemon();
        for i in 0..(MAX_ACTIVITY_LOG + 5) {
            d.record_activity("agent".into(), EventKind::Insert, true, format!("edit {i}"));
        }
        assert_eq!(d.activity_log.len(), MAX_ACTIVITY_LOG);
        assert_eq!(d.activity_log.front().unwrap().detail, "edit 5");
        assert_eq!(
            d.activity_log.back().unwrap().detail,
            format!("edit {}", MAX_ACTIVITY_LOG + 4)
        );
    }

    #[tokio::test]
    async fn activity_records_headless_open_success_and_failure() {
        // ADR-0038: Headless 由来の Open の試行（成功・失敗）が activity に載る
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-act-open-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-act-open-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        // daemon はパスを正規化する（macOS の /var → /private/var 等）
        let path = std::fs::canonicalize(&file)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let resp = request(&mut agent, &Command::Open { path: path.clone() }).await;
        assert_eq!(resp.text, "base\n");
        let rec = resp.activity.last().expect("Open 成功が記録される");
        assert_eq!(rec.actor, "test");
        assert_eq!(rec.kind, EventKind::Open);
        assert!(rec.ok);
        assert_eq!(rec.detail, path);

        // 存在しないファイル: 失敗として記録される
        let mut agent2 = connect_client(&sock, ClientKind::Headless).await;
        let missing = "/nonexistent-xyz-123.txt".to_string();
        let resp = request(&mut agent2, &Command::Open { path: missing }).await;
        let rec = resp.activity.last().expect("Open 失敗が記録される");
        assert_eq!(rec.actor, "test");
        assert_eq!(rec.kind, EventKind::Open);
        assert!(!rec.ok, "読み込めないファイルは失敗として記録される");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn activity_records_document_edit_success_and_reject() {
        // ADR-0038: Headless の DocumentEdit の成功・拒否が記録される
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-act-edit-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-act-edit-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "hello world").unwrap();
        start_server(&sock).await;

        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut agent, &Command::Open { path: path.clone() }).await;

        // 成功: 挿入
        let resp = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 5,
                end: 5,
                text: "!".into(),
                checksum: fnv1a64(b"hello world"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(resp.text, "hello! world");
        let rec = resp.activity.last().expect("編集成功が記録される");
        assert_eq!(rec.actor, "test");
        assert_eq!(rec.kind, EventKind::Insert);
        assert!(rec.ok);

        // 拒否: チェックサム不一致
        let resp = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 0,
                end: 3,
                text: "bye".into(),
                checksum: 12345,
                expected_text: None,
            },
        )
        .await;
        assert_eq!(resp.text, "hello! world", "拒否で文書は変わらない");
        let rec = resp.activity.last().expect("拒否が記録される");
        assert!(!rec.ok);
        assert!(rec.detail.contains("document changed since read"));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn activity_reaches_subscribers_through_push() {
        // ADR-0038: 記録の更新は generation を進めて push に乗る — 購読者の
        // push スナップショットに activity が載る
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-act-push-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-act-push-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "abc").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path: path.clone() }).await;

        // agent の編集 → tui へ push
        let resp = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 3,
                end: 3,
                text: "X".into(),
                checksum: fnv1a64(b"abc"),
                expected_text: None,
            },
        )
        .await;
        let pushed = recv_push(&mut tui).await;
        assert_eq!(pushed.generation, resp.generation, "push は応答と同じ世代");
        let rec = pushed.activity.last().expect("push に活動記録が載る");
        assert_eq!(rec.actor, "test");
        assert!(rec.ok);
        assert!(rec.detail.contains(&path), "編集対象パス: {}", rec.detail);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn activity_excludes_read_only_commands() {
        // ADR-0038: 読み取り系（GetState / WaitFor / GetInlayHints）は記録しない
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-act-ro-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-act-ro-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "abc").unwrap();
        start_server(&sock).await;

        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();
        let resp = request(&mut agent, &Command::Open { path: path.clone() }).await;
        assert_eq!(resp.activity.len(), 1, "Open のみ記録される");

        let _ = request(&mut agent, &Command::GetState).await;
        let _ = request(&mut agent, &Command::WaitFor { generation: 0 }).await;
        let resp = request(&mut agent, &Command::GetState).await;
        assert_eq!(resp.activity.len(), 1, "読み取り系では増えない");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn headless_client_never_receives_push() {
        // ADR-0013: Headless（ワンショット CLI）は購読されない。応答の後に
        // 追加のメッセージが届かない（応答1行で切断する CLI が壊れない）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-13c-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-13c-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path }).await;
        let _ = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 0,
                end: 0,
                text: "hi".into(),
                checksum: fnv1a64(b""),
                expected_text: None,
            },
        )
        .await;

        // agent のソケットに push は混ざらない
        let extra = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            agent.recv_message(),
        )
        .await;
        assert!(extra.is_err(), "headless に push は届かない");
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn no_push_for_read_only_command() {
        // ADR-0013: 状態を変えないコマンド（GetState）では値が変わらず、
        // watch が受信側を起こさない → push は飛ばない。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-13d-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-13d-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path }).await;
        // Open の自分の push は届かない（発信元スキップ）

        // agent の読み取り（GetState）は状態を変えない
        let _ = request(&mut agent, &Command::GetState).await;
        let extra = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tui.recv_message(),
        )
        .await;
        assert!(extra.is_err(), "GetState では push は飛ばない");
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn headless_client_is_restricted_to_document_edit_family() {
        // #13: headless は GetState / Save / DocumentEdit / Open のみ（#28 で Open
        // を追加）。それ以外の Command（選択移動・モード変更・コマンドベース編集・
        // undo/redo）は拒否され、状態・世代が変わらない。Interactive は従来どおり。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-13e-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-13e-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "base\n").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path: path.clone() }).await;
        // Open の自分の push は届かない（発信元スキップ）

        // 拒否されるコマンド: 状態・世代・モードは不変（Open は #28 で許可済み）
        for cmd in [
            Command::SetMode { mode: Mode::Insert },
            Command::Move {
                movement: Movement::Char,
                direction: Direction::Forward,
            },
            Command::Insert { text: "X".into() },
            Command::Undo,
        ] {
            let snap = request(&mut agent, &cmd).await;
            assert!(
                snap.status
                    .as_deref()
                    .is_some_and(|s| s.starts_with("headless clients can only use")),
                "{cmd:?} は拒否される: {:?}",
                snap.status
            );
            assert_eq!(snap.text, "base\n", "{cmd:?} で状態が変わらない");
            assert_eq!(snap.generation, 1, "{cmd:?} で世代が進まない");
            assert_eq!(snap.mode, Mode::Normal, "{cmd:?} でモードが変わらない");
        }

        // #28: headless の Open は許可される（フォーカスを切り替えられる）
        let snap = request(&mut agent, &Command::Open { path: path.clone() }).await;
        assert!(snap.status.is_none(), "Open は許可: {:?}", snap.status);
        assert_eq!(snap.text, "base\n", "Open で内容が読める");
        // 絶対化は /private/var 等の symlink 解決で表記が変わり得るため末尾比較
        assert!(
            snap.path
                .as_deref()
                .is_some_and(|p| p.ends_with(&file.file_name().unwrap().to_string_lossy().into_owned())),
            "Open でパスが載る: {:?}",
            snap.path
        );

        // 許可される操作は従来どおり
        let snap = request(&mut agent, &Command::GetState).await;
        assert!(snap.status.is_none(), "GetState は許可");
        let snap = request_edit(
            &mut agent,
            &DocumentEdit {
                start: 4,
                end: 4,
                text: "Z".into(),
                checksum: fnv1a64(b"base\n"),
                expected_text: None,
            },
        )
        .await;
        assert_eq!(snap.text, "baseZ\n", "DocumentEdit は許可");
        let snap = request(&mut agent, &Command::Save).await;
        assert!(
            snap.status.as_deref().is_some_and(|s| s.starts_with("saved:")),
            "Save は許可: {:?}",
            snap.status
        );

        // #30: headless の Close は許可される（フォーカス文書を閉じて空画面へ）
        let snap = request(&mut agent, &Command::Close).await;
        assert!(snap.status.is_none(), "Close は許可: {:?}", snap.status);
        assert_eq!(snap.path, None, "Close で空画面（ファイル未オープン）に戻る");
        assert!(snap.text.is_empty(), "Close 後の文書は空");

        // Interactive は従来どおり全コマンドを使える
        let snap = request(&mut tui, &Command::Insert { text: "Y".into() }).await;
        assert!(snap.status.is_none(), "Interactive の Insert は許可");
        assert!(snap.text.contains("Y"));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn wait_for_generation_blocks_until_change() {
        // ADR-0012 #12: WaitFor は世代が target を超えるまでブロックし、
        // 超えた時点のスナップショットを返す（エージェントのポーリング不要）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-12a-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-12a-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut tui, &Command::Open { path }).await;
        assert_eq!(snap.generation, 1);
        assert_eq!(
            snap.checksum,
            fnv1a64(snap.text.as_bytes()),
            "snapshot に全文 checksum が載る（edit にそのまま渡せる）"
        );

        // agent: 現在の世代（1）を超えるまで待つ（ブロック）
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let mut wait = tokio::spawn(async move {
            request(&mut agent, &Command::WaitFor { generation: 1 }).await
        });
        // 編集前はまだ完了していない（ブロック中）
        let pending = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            &mut wait,
        )
        .await;
        assert!(pending.is_err(), "編集前は WaitFor がブロックしている");

        // tui の編集で世代が進む → 待機が完了して最新状態が返る
        let _ = request(&mut tui, &Command::Insert { text: "X".into() }).await;
        let snap = wait.await.unwrap();
        assert!(
            snap.generation > 1,
            "編集後の世代が返る: {}",
            snap.generation
        );
        assert_eq!(snap.text, "X");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn wait_for_generation_returns_immediately_when_already_passed() {
        // ADR-0012 #12: 既に世代が target を超えていれば即応答する。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-12b-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-12b-file-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path }).await; // gen 1

        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let snap = request(&mut agent, &Command::WaitFor { generation: 0 }).await;
        assert!(snap.generation >= 1, "既に超えていれば即応答");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    // ---- inlay hint（ADR-0020） ----

    /// GetInlayHints を送り、Hints 応答を受け取る。
    async fn request_hints(c: &mut TestClient, path: &str) -> (String, u64, Vec<InlayHint>) {
        let mut line =
            serde_json::to_string(&Command::GetInlayHints { path: path.into() }).unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        recv_hints(c).await
    }

    /// Hints 応答1件を読む（途中の応答・push は読み飛ばす）。
    async fn recv_hints(c: &mut TestClient) -> (String, u64, Vec<InlayHint>) {
        loop {
            match c.recv_message().await {
                ServerMessage::Hints { path, generation, hints } => {
                    return (path, generation, hints)
                }
                ServerMessage::Response { .. } | ServerMessage::Push { .. } => continue,
                ServerMessage::Peek { .. } | ServerMessage::ServerInfo { .. } => continue,
                ServerMessage::RenameResult { .. } | ServerMessage::ReferencesResult { .. } => {
                    continue
                }
                ServerMessage::Outline { .. } | ServerMessage::EnclosingSymbol { .. } => {
                    continue
                }
                ServerMessage::Hover { .. }
                | ServerMessage::WorkspaceSymbols { .. }
                | ServerMessage::Check { .. } => continue,
                ServerMessage::ReviewComments { .. } => continue,
                ServerMessage::ReadPath { .. } => continue,
                ServerMessage::Peek { .. } => continue,
                ServerMessage::ServerInfo { .. } => continue,
            }
        }
    }

    fn type_hint(position: usize, text: &str, padding_right: bool) -> InlayHint {
        InlayHint {
            position,
            text: text.into(),
            padding_left: false,
            padding_right,
        }
    }

    #[test]
    fn snapshot_keeps_stale_hints_until_new_pull() {
        // Q7: 編集でテキストが変わっても（checksum 不一致）、新ヒントが届くまで
        // 旧ヒントを表示に載せ続ける（タイプ中のちらつき防止）。
        let mut d = daemon();
        open_path(&mut d, "test.rs", "let x = 5");
        let path = PathBuf::from("test.rs");
        // v12（ADR-0037）: ヒントは解析フォーカス文書の表示にのみ載る。
        // ここでは Open 相当としてフォーカスを設定しておく。
        d.set_analysis_focus(&path);
        d.cache_hints(
            path.clone(),
            "let x = 5",
            vec![type_hint(5, ": i32", true)],
        );
        // 編集でテキストが変わる（キャッシュは stale になる）
        apply(&mut d, Command::Insert { text: "aaa".into() });
        let snap = snapshot(&mut d, None);
        assert_eq!(snap.inlay_hints.len(), 1, "stale ヒントは保持される: {:?}", snap.inlay_hints);
        assert_eq!(snap.inlay_hints[0].position, 5);
        // 新ヒント（空）が届けば置き換わる
        d.cache_hints(path, &snap.text, Vec::new());
        let snap = snapshot(&mut d, None);
        assert!(snap.inlay_hints.is_empty(), "新ヒントで置き換わる");
    }

    #[test]
    fn diagnostics_and_hints_follow_analysis_focus() {
        // ADR-0037: 診断・inlay は「解析フォーカス文書（= セッションが最後に解析
        // した文書）」を見る View のスナップショットにのみ載る。別セッションの
        // フォーカスは独立に保持される。
        let mut d = daemon();
        let a = PathBuf::from("/ws1/a.rs");
        let b = PathBuf::from("/ws2/b.rs");
        open_path(&mut d, "/ws1/a.rs", "fn a() {}");
        open_path(&mut d, "/ws2/b.rs", "fn b() {}");

        // 各セッションのフォーカス文書に診断・ヒントを入れる
        d.set_focus_diagnostics(
            &a,
            vec![Diagnostic {
                start: 0,
                end: 2,
                severity: Severity::Error,
                message: "a の診断".into(),
            }],
        );
        d.cache_hints(a.clone(), "fn a() {}", vec![type_hint(3, ": i32", true)]);
        d.set_focus_diagnostics(
            &b,
            vec![Diagnostic {
                start: 0,
                end: 2,
                severity: Severity::Warning,
                message: "b の診断".into(),
            }],
        );

        // a を見る View: a の診断・ヒントだけが載る
        d.editor.set_focused_view(d.idle_view);
        d.editor.focus_open_path(&a);
        let snap = snapshot(&mut d, None);
        assert_eq!(snap.path.as_deref(), Some("/ws1/a.rs"));
        assert_eq!(snap.diagnostics.len(), 1, "a の診断が載る");
        assert_eq!(snap.diagnostics[0].message, "a の診断");
        assert_eq!(snap.inlay_hints.len(), 1, "a のヒントが載る");

        // b を見る View: b の診断だけが載る（a のものは混ざらない）
        d.editor.set_focused_view(d.idle_view);
        d.editor.focus_open_path(&b);
        let snap = snapshot(&mut d, None);
        assert_eq!(snap.diagnostics.len(), 1, "b の診断が載る");
        assert_eq!(snap.diagnostics[0].message, "b の診断");
        assert!(snap.inlay_hints.is_empty(), "b のヒントは未取得なので空");

        // 共有解析フォーカス（同セッション）: focus が別文書へ動くと、旧フォーカス
        // 文書の診断は表示されなくなる（フォーカス = セッションごとに 1 文書）。
        let mut d2 = daemon();
        open_path(&mut d2, "/ws1/a.rs", "fn a() {}");
        let c = PathBuf::from("/ws1/c.rs");
        open_path(&mut d2, "/ws1/c.rs", "fn c() {}");
        d2.set_focus_diagnostics(
            &a,
            vec![Diagnostic {
                start: 0,
                end: 2,
                severity: Severity::Error,
                message: "a の診断".into(),
            }],
        );
        d2.set_analysis_focus(&c); // c の Open 相当（a の診断は捨てられる）
        d2.editor.set_focused_view(d2.idle_view);
        d2.editor.focus_open_path(&a);
        let snap = snapshot(&mut d2, None);
        assert!(snap.diagnostics.is_empty(), "c がフォーカスの間 a の診断は出ない");
    }

    #[test]
    fn hint_cache_evicts_oldest_over_cap() {
        // ADR-0020: パスキーキャッシュの上限超過は挿入順の最古から除去する。
        let mut d = daemon();
        open_path(&mut d, "test.rs", "let x = 5");
        for i in 0..(MAX_HINT_CACHE + 5) {
            let p = format!("/tmp/hint-cache-evict-{i}.rs");
            d.cache_hints(PathBuf::from(&p), "let x = 5", vec![type_hint(5, ": i32", true)]);
        }
        assert_eq!(d.hints.len(), MAX_HINT_CACHE, "上限を超えない");
        assert!(
            !d.hints.contains_key(Path::new("/tmp/hint-cache-evict-0.rs")),
            "最古が除去される"
        );
        assert!(
            d.hints.contains_key(Path::new(&format!(
                "/tmp/hint-cache-evict-{}.rs",
                MAX_HINT_CACHE + 4
            ))),
            "最新は残る"
        );
    }

    #[test]
    fn outline_cache_invalidates_on_text_change_and_evicts_fifo() {
        // ADR-0031: outline キャッシュはテキストのチェックサムで新鮮さを判定し、
        // 上限超過は最古から除去する（HintCache と同じ契約）。
        let mut d = daemon();
        let sym = OutlineSymbol {
            name: "f".into(),
            kind: SymbolKind::Function,
            range: Range { anchor: 0, head: 10 },
            selection_range: Range { anchor: 3, head: 4 },
            children: Vec::new(),
        };
        let p = PathBuf::from("/tmp/outline-cache.rs");
        d.cache_outline(p.clone(), "pub fn f() {}", vec![sym.clone()]);
        // 同じテキスト → ヒット
        assert_eq!(
            d.outlines.get(&p).map(|c| c.symbols.len()),
            Some(1),
            "同一テキストは新鮮"
        );
        // テキストが変わった（チェックサム不一致）→ 呼び出し側は再取得する（エントリは
        // 残るが、`cached_outline` のフィルタで一致しなくなる。ここではエントリの
        // checksum が取得時テキストに固定されることだけを検証する）。
        d.cache_outline(p.clone(), "pub fn g() {}", vec![]);
        assert_eq!(d.outlines.get(&p).map(|c| c.symbols.len()), Some(0), "再取得で上書き");
        // FIFO evict
        for i in 0..(MAX_HINT_CACHE + 5) {
            d.cache_outline(PathBuf::from(format!("/tmp/outline-evict-{i}.rs")), "x", vec![]);
        }
        assert_eq!(d.outlines.len(), MAX_HINT_CACHE, "上限を超えない");
        assert!(
            !d.outlines.contains_key(Path::new("/tmp/outline-evict-0.rs")),
            "最古が除去される"
        );
    }

    #[tokio::test]
    async fn inlay_hints_follow_edits_and_snapshot() {
        // Open の settle ループと編集後の pull がヒントをキャッシュに載せ、
        // snapshot に反映される（ADR-0020）。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-hint-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-hint-file-{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "let x = 5\nTODO\n").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path: path.clone() }).await;
        // settle ループがヒントを pull する: snapshot に載るまで待つ
        let snap = poll_snapshot(
            &mut tui,
            |s| !s.inlay_hints.is_empty(),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(snap.inlay_hints.len(), 1, "{:?}", snap.inlay_hints);
        assert_eq!(snap.inlay_hints[0].position, 5, "x の直後");
        assert_eq!(snap.inlay_hints[0].text, ": i32");

        // 編集 → 背景 pull（ADR-0055）: 応答は pull 前のスナップショットなので、
        // 追従（"alet x = 5" の x は char 6）は背景タスクの反映後（push / GetState）
        // に確認する。
        let _ = request(&mut tui, &Command::Insert { text: "a".into() }).await;
        let snap = poll_snapshot(
            &mut tui,
            |s| s.inlay_hints.iter().any(|h| h.position == 6),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(snap.inlay_hints.len(), 1, "{:?}", snap.inlay_hints);
        assert_eq!(snap.inlay_hints[0].position, 6, "編集後の位置: {:?}", snap.inlay_hints);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn get_inlay_hints_serves_arbitrary_path_and_restores_focus() {
        // ADR-0020: エージェントの GetInlayHints が、開いていないパスのヒントを
        // 全文テキストなしで返す。フォーカス文書の LSP セッションを借りるため、
        // 応答後はフォーカス文書へ復元され（Q10-(c)）、次回の編集が同期される。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-hint2-sock-{}.sock", std::process::id()));
        let file_a = dir.join(format!("minae-hint2-a-{}.rs", std::process::id()));
        let file_b = dir.join(format!("minae-hint2-b-{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file_a, "let x = 5\nTODO\n").unwrap();
        std::fs::write(&file_b, "foo(1)\n").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path_a = file_a.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path: path_a.clone() }).await;
        // A の診断・ヒントが settle で載るまで待つ
        let snap = poll_snapshot(
            &mut tui,
            |s| !s.diagnostics.is_empty() && !s.inlay_hints.is_empty(),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(snap.diagnostics[0].message, "mock: TODO found");
        assert_eq!(snap.inlay_hints[0].position, 5);

        // エージェント（headless）: 未開パス B のヒントを取得
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path_b = file_b.to_string_lossy().into_owned();
        let (resp_path, _gen, hints) = request_hints(&mut agent, &path_b).await;
        // normalize_open_path が canonicalize するため /private 等の正規化差がある
        // （/var → /private/var）。サフィックスで同一ファイルを検証する。
        assert!(
            resp_path.ends_with(&file_b.to_string_lossy().into_owned()),
            "応答パスが要求パスに対応する: {resp_path}"
        );
        assert_eq!(hints.len(), 1, "{hints:?}");
        assert_eq!(hints[0].position, 4, "foo( の直後: {hints:?}");
        assert_eq!(hints[0].text, "arg:");

        // キャッシュ: 同じ B への再要求は同じヒントを返す（LSP 再解析なし）
        let (_p, _g, hints2) = request_hints(&mut agent, &path_b).await;
        assert_eq!(hints, hints2);

        // 復元の実証: A への編集の didChange が mock に届く = セッションが A に
        // 戻っている（Q10-(c)）。"alet x = 5\nTODO\n" の TODO は byte 10。
        // 追従は背景 pull（ADR-0055）の反映後（push / GetState）に確認する。
        let _ = request(&mut tui, &Command::Insert { text: "a".into() }).await;
        let snap = poll_snapshot(
            &mut tui,
            |s| {
                s.diagnostics.iter().any(|d| d.start == 10)
                    && s.inlay_hints.iter().any(|h| h.position == 6)
            },
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(snap.diagnostics[0].start, 10, "復元後の編集が同期される: {snap:?}");
        assert_eq!(snap.inlay_hints[0].position, 6, "ヒントも編集後テキストに追従");

        drop(tui);
        drop(agent);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file_a);
        let _ = std::fs::remove_file(&file_b);
    }

    #[tokio::test]
    async fn peek_definition_returns_snippet_in_snapshot() {
        // Space k（PeekDefinition）: カーソル位置のシンボル定義を確認用スニペット
        // として応答スナップショットの peek フィールドに載せる（ジャンプしない）。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-peek-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-peek-file-{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "fn peek_target(x: i32) -> i32 {\n    x\n}\n").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let _ = request(&mut tui, &Command::Open { path: path.clone() }).await;
        // PeekDefinition は自前で LSP セッションを ensure + didOpen するため
        // settle を待つ必要はない。直接 peek を要求する。
        let snap = request(&mut tui, &Command::PeekDefinition).await;
        let peek = snap.peek.expect("PeekDefinition は peek を返す: {snap:?}");
        assert!(peek.path.ends_with(&file.to_string_lossy().into_owned()), "定義元ファイル: {peek:?}");
        assert_eq!(peek.line, 1, "1 始まりの開始行: {peek:?}");
        assert!(
            peek.text.contains("fn peek_target") && peek.text.contains("    x"),
            "定義行 + 本体: {peek:?}"
        );
        // 通常コマンドの応答からは peek が落ちている（一時表示）
        let snap = request(&mut tui, &Command::Move {
            movement: Movement::Char,
            direction: Direction::Backward,
        })
        .await;
        assert!(snap.peek.is_none(), "次のコマンドで peek は落ちる: {snap:?}");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn peek_definition_at_returns_lightweight_peek_for_headless() {
        // ADR-0025: エージェントの PeekDefinitionAt は全文スナップショットではなく
        // 軽量な ServerMessage::Peek（定義だけ）で応答する — トークン削減経路。
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-peekat-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-peekat-file-{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "fn peek_target(x: i32) -> i32 {\n    x\n}\n").unwrap();
        start_server(&sock).await;

        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();
        let cmd = Command::PeekDefinitionAt { path, line: 1, col: 5 };
        let mut line = serde_json::to_string(&cmd).unwrap();
        line.push('\n');
        agent.send(line.as_bytes()).await;
        // 応答は ServerMessage::Peek（途中の push は読み飛ばす）
        let peek = loop {
            match agent.recv_message().await {
                ServerMessage::Peek { path, line, text } => {
                    break (path, line, text);
                }
                ServerMessage::Response { .. }
                | ServerMessage::Push { .. }
                | ServerMessage::Hints { .. }
                | ServerMessage::ServerInfo { .. } => {
                    continue;
                }
                ServerMessage::RenameResult { .. } | ServerMessage::ReferencesResult { .. } => {
                    continue;
                }
                ServerMessage::Outline { .. } | ServerMessage::EnclosingSymbol { .. } => {
                    continue;
                }
                ServerMessage::Hover { .. }
                | ServerMessage::WorkspaceSymbols { .. }
                | ServerMessage::Check { .. } => {
                    continue;
                }
                ServerMessage::ReviewComments { .. } => {
                    continue;
                }
                ServerMessage::ReadPath { .. } => {
                    continue;
                }
            }
        };
        assert!(peek.0.ends_with(&file.to_string_lossy().into_owned()), "定義元: {peek:?}");
        assert_eq!(peek.1, 1, "1 始まりの開始行: {peek:?}");
        assert!(
            peek.2.contains("fn peek_target") && peek.2.contains("    x"),
            "定義スニペット: {peek:?}"
        );
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }

    /// #49: 基準 root の登録・書込み拒否・解除の E2E（ソケット経由・v13）。
    #[tokio::test]
    async fn base_root_register_guard_unregister_e2e() {
        let dir = std::env::temp_dir().join(format!("minae-base-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\nworld\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut c, &Command::Open { path: path.clone() }).await;
        assert_eq!(snap.generation, 1);

        // 登録: 世代が進み、イベントに載る。
        let snap = request(
            &mut c,
            &Command::RegisterBaseRoot {
                root: dir.to_string_lossy().into_owned(),
                commit: "abc1234".into(),
                repo: None,
            },
        )
        .await;
        assert_eq!(snap.generation, 2);
        assert!(
            snap.events.iter().any(|e| e.kind == EventKind::BaseRoot),
            "BaseRoot イベント"
        );

        // 位置指定編集は拒否（状態不変・世代不変・理由つき）。
        let edit = DocumentEdit {
            start: 0,
            end: 0,
            text: "X".into(),
            checksum: fnv1a64("hello\nworld\n".as_bytes()),
            expected_text: None,
        };
        let snap = request_edit(&mut c, &edit).await;
        assert_eq!(snap.text, "hello\nworld\n", "基準への編集は効かない");
        assert_eq!(snap.generation, 2, "拒否で世代は進まない");
        assert!(
            snap.status.as_deref().unwrap_or("").contains("読取り専用"),
            "拒否理由: {:?}",
            snap.status
        );

        // Save も拒否（ファイル不変）。
        let snap = request(&mut c, &Command::Save).await;
        assert_eq!(snap.generation, 2);
        assert!(
            snap.status.as_deref().unwrap_or("").contains("読取り専用"),
            "拒否理由: {:?}",
            snap.status
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "hello\nworld\n",
            "ファイルは書かれない"
        );

        // 解除: 世代が進む。解除後は編集が通る。
        let snap = request(
            &mut c,
            &Command::UnregisterBaseRoot {
                root: dir.to_string_lossy().into_owned(),
            },
        )
        .await;
        assert_eq!(snap.generation, 3);
        let snap = request_edit(&mut c, &edit).await;
        assert_eq!(snap.text, "Xhello\nworld\n", "解除後は編集できる");
        assert_eq!(snap.generation, 4);

        // 不在の解除は無視（世代不変）。
        let snap = request(
            &mut c,
            &Command::UnregisterBaseRoot {
                root: dir.join("nope").to_string_lossy().into_owned(),
            },
        )
        .await;
        assert_eq!(snap.generation, 4, "不在解除で世代は進まない");

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #53: headless（`minas exec` 想定）からの基準登録・解除の E2E。登録で
    /// 世代が進み、基準配下の書込みガードは発信元を問わず効く（自作自演の
    /// 読み取り専用化は解除で戻せる）。TUI 駆動と同型の世代進行を確認する。
    #[tokio::test]
    async fn base_root_headless_register_unregister_e2e() {
        let dir = std::env::temp_dir().join(format!("minae-base-hl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        start_server(&sock).await;

        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let path = file.to_string_lossy().into_owned();
        let snap = request(&mut agent, &Command::Open { path }).await;
        let gen0 = snap.generation;

        // headless 登録: 拒否されず世代が進む。
        let snap = request(
            &mut agent,
            &Command::RegisterBaseRoot {
                root: dir.to_string_lossy().into_owned(),
                commit: "abc1234".into(),
                repo: None,
            },
        )
        .await;
        assert!(snap.status.is_none(), "headless 登録は通る: {:?}", snap.status);
        assert_eq!(snap.generation, gen0 + 1);
        assert!(snap.events.iter().any(|e| e.kind == EventKind::BaseRoot));

        // 基準配下への headless 自身の保存も拒否（ガードは発信元不問）。
        let snap = request(&mut agent, &Command::Save).await;
        assert!(
            snap.status.as_deref().unwrap_or("").contains("読取り専用"),
            "拒否理由: {:?}",
            snap.status
        );

        // headless 解除: 世代が進み、保存が通る。
        let snap = request(
            &mut agent,
            &Command::UnregisterBaseRoot {
                root: dir.to_string_lossy().into_owned(),
            },
        )
        .await;
        assert!(snap.status.is_none(), "headless 解除は通る: {:?}", snap.status);
        let snap = request(&mut agent, &Command::Save).await;
        assert!(
            snap.status.as_deref().unwrap_or("").starts_with("saved"),
            "解除後は保存できる: {:?}",
            snap.status
        );

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #52: 基準診断の対応付け・常駐・解除消去（LSP なし単体）。
    #[tokio::test]
    async fn base_diagnostics_mapping_cache_and_clear() {
        use std::path::{Path, PathBuf};
        let mut d = Daemon::new();
        // repo 対応あり・なしの 2 root。
        d.register_base_root(
            PathBuf::from("/base"),
            "abc1234".into(),
            Some(PathBuf::from("/repo")),
            EventSource::Interactive,
        );
        d.register_base_root(
            PathBuf::from("/other"),
            "def5678".into(),
            None,
            EventSource::Interactive,
        );
        // 対応物: live → base。repo なし root は写さない。
        assert_eq!(
            d.base_counterparts(Path::new("/repo/a.rs")),
            vec![PathBuf::from("/base/a.rs")]
        );
        // 未充填は空（待たない）。
        assert!(d.snapshot_base_diagnostics(Some(Path::new("/repo/a.rs"))).is_empty());
        assert!(d.snapshot_base_diagnostics(None).is_empty());
        // 格納すると載る。
        d.store_base_diagnostics(
            PathBuf::from("/base/a.rs"),
            vec![Diagnostic {
                start: 0,
                end: 1,
                severity: Severity::Error,
                message: "e".into(),
            }],
        );
        let got = d.snapshot_base_diagnostics(Some(Path::new("/repo/a.rs")));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "/base/a.rs");
        assert_eq!(got[0].diagnostics.len(), 1);
        // take は未キャッシュのみ・充填中は重複しない。
        assert!(d.take_base_pull(Some(Path::new("/repo/a.rs"))).is_none());
        assert_eq!(
            d.take_base_pull(Some(Path::new("/repo/b.rs"))),
            Some(PathBuf::from("/base/b.rs"))
        );
        assert!(d.take_base_pull(Some(Path::new("/repo/b.rs"))).is_none());
        assert!(d.take_base_pull(None).is_none());
        // 解除で対応物・キャッシュ・充填中が消える。
        d.unregister_base_root(Path::new("/base"), EventSource::Interactive);
        assert!(d.snapshot_base_diagnostics(Some(Path::new("/repo/a.rs"))).is_empty());
        assert!(d.base_counterparts(Path::new("/repo/a.rs")).is_empty());
        assert!(d.base_diag_inflight.is_empty());
    }

    /// #52: 基準診断の后台充填 E2E（mock LSP）。Open→Register(repo付き）→
    /// GetState を回すと基準対応物の診断がスナップショットに載る。世代不変。
    #[tokio::test]
    async fn base_diagnostics_fill_e2e_via_mock() {
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;
        let dir = std::env::temp_dir().join(format!("mina-test-basediag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = dir.join("base");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(dir.join("live.rs"), "fn f() { TODO }\n").unwrap();
        std::fs::write(base.join("live.rs"), "fn f() { TODO }\n").unwrap();
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let live = dir.join("live.rs").to_string_lossy().into_owned();
        let snap = request(&mut tui, &Command::Open { path: live }).await;
        assert!(snap.status.is_none(), "open: {:?}", snap.status);
        // repo 付きで基準登録（TUI と同型）。
        let snap = request(
            &mut tui,
            &Command::RegisterBaseRoot {
                root: base.to_string_lossy().into_owned(),
                commit: "abc1234".into(),
                repo: Some(dir.to_string_lossy().into_owned()),
            },
        )
        .await;
        assert!(snap.status.is_none(), "register: {:?}", snap.status);
        assert!(snap.base_diagnostics.is_empty(), "充填前は空");
        let gen0 = snap.generation;
        // 后台充填の到着を待つ（push で届く・世代は進まない）。
        let snap = poll_snapshot(
            &mut tui,
            |s| !s.base_diagnostics.is_empty(),
            std::time::Duration::from_secs(60),
        )
        .await;
        assert_eq!(snap.generation, gen0, "基準診断の到着で世代は進まない");
        assert_eq!(snap.base_diagnostics.len(), 1);
        assert!(
            snap.base_diagnostics[0].path.ends_with("base/live.rs"),
            "{:?}",
            snap.base_diagnostics[0].path
        );
        assert!(
            snap.base_diagnostics[0].diagnostics.iter().any(|d| d.message == "mock: TODO found"),
            "{:?}",
            snap.base_diagnostics[0].diagnostics
        );
        // 解除で消える。
        let snap = request(
            &mut tui,
            &Command::UnregisterBaseRoot {
                root: base.to_string_lossy().into_owned(),
            },
        )
        .await;
        assert!(snap.base_diagnostics.is_empty());
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unregister 消去・切断保持・Clear・headless 拒否の E2E（ソケット経由・v14）。
    #[tokio::test]
    async fn review_comments_add_list_stale_clear_e2e() {
        let dir = std::env::temp_dir().join(format!("minae-review-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let file = dir.join("a.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let open_path = file.to_string_lossy().into_owned();
        let snap = request(&mut tui, &Command::Open { path: open_path }).await;
        // daemon は Open 時に正規化するため、以降は snapshot.path（正規化済み）を
        // 使う（実 TUI と同じ住所。/var → /private/var 問題の回避）。
        let path = snap.path.clone().expect("open で path が載る");
        assert_eq!(snap.review_comment_count, 0);
        let gen0 = snap.generation;

        // 追加: 世代が進み、件数に載り、イベントに載る。
        let add = |line: u32, snippet: &str, body: &str| Command::AddReviewComment {
            path: path.clone(),
            side: ReviewSide::Current,
            line,
            snippet: snippet.into(),
            body: body.into(),
            base: "abc1234".into(),
        };
        let snap = request(&mut tui, &add(2, "beta", "直して")).await;
        assert_eq!(snap.generation, gen0 + 1);
        assert_eq!(snap.review_comment_count, 1);
        assert!(
            snap.events.iter().any(|e| e.kind == EventKind::ReviewComment),
            "ReviewComment イベント"
        );

        // 同一アンカーは上書き（件数不変・本文更新）。
        let snap = request(&mut tui, &add(2, "beta", "やっぱり直さないで")).await;
        assert_eq!(snap.review_comment_count, 1, "上書きで重複しない");

        // headless（minas 想定）から List: 全文付き・非 stale・解決行一致。
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let (_, comments) = request_reviews(&mut agent).await;
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "やっぱり直さないで");
        assert!(!comments[0].stale);
        assert_eq!((comments[0].line, comments[0].resolved_line), (2, 2));

        // 先頭に1行挿入 → 2行目の beta は3行目へずれる（stale 解決）。
        let edit = DocumentEdit {
            start: 0,
            end: 0,
            text: "zero\n".into(),
            checksum: fnv1a64("alpha\nbeta\ngamma\n".as_bytes()),
            expected_text: None,
        };
        let snap = request_edit(&mut tui, &edit).await;
        assert_eq!(snap.text, "zero\nalpha\nbeta\ngamma\n");
        let (_, comments) = request_reviews(&mut agent).await;
        assert_eq!(comments.len(), 1);
        assert!(comments[0].stale, "ずれたら stale");
        assert_eq!(comments[0].resolved_line, 3, "snippet 追跡で解決");
        assert_eq!(comments[0].line, 2, "保存値は書き換えない");

        // 空本文の追加 = そのアンカーの削除。
        let snap = request(&mut tui, &add(2, "beta", "   ")).await;
        assert_eq!(snap.review_comment_count, 0);
        let (_, comments) = request_reviews(&mut agent).await;
        assert!(comments.is_empty());

        // 再追加 → 再ピン（Register）で全消し。
        let _ = request(&mut tui, &add(1, "zero", "先頭メモ")).await;
        let snap = request(
            &mut tui,
            &Command::RegisterBaseRoot {
                root: dir.to_string_lossy().into_owned(),
                commit: "def5678".into(),
                repo: None,
            },
        )
        .await;
        assert_eq!(snap.review_comment_count, 0, "再ピンで旧コメント破棄");

        // 同一ペアの再登録（表示切替・再接続）は保持する。
        let _ = request(&mut tui, &add(1, "zero", "保持される")).await;
        let snap = request(
            &mut tui,
            &Command::RegisterBaseRoot {
                root: dir.to_string_lossy().into_owned(),
                commit: "def5678".into(),
                repo: None,
            },
        )
        .await;
        assert_eq!(snap.review_comment_count, 1, "同一基準の再登録は保持");

        // 再追加 → Unregister で全消し。
        let _ = request(&mut tui, &add(1, "zero", "先頭メモ2")).await;
        let snap = request(
            &mut tui,
            &Command::UnregisterBaseRoot {
                root: dir.to_string_lossy().into_owned(),
            },
        )
        .await;
        assert_eq!(snap.review_comment_count, 0, "Unregister で全消し");

        // 再追加 → TUI 切断後も headless から読める（Q12: 切断保持）。
        let _ = request(&mut tui, &add(1, "zero", "切断後も残る")).await;
        drop(tui);
        let (_, comments) = request_reviews(&mut agent).await;
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "切断後も残る");

        // 明示 Clear で全消し。
        let mut tui2 = connect_client(&sock, ClientKind::Interactive).await;
        let snap = request(&mut tui2, &Command::ClearReviewComments).await;
        assert_eq!(snap.review_comment_count, 0);
        let snap2 = request(&mut tui2, &Command::ClearReviewComments).await;
        assert_eq!(snap2.generation, snap.generation, "空 Clear は世代不変");
        let (_, comments) = request_reviews(&mut agent).await;
        assert!(comments.is_empty());

        // headless からの Add は拒否（TUI 駆動のみ・#49 と同型）。
        let snap = request(&mut agent, &add(1, "zero", "agent書込み")).await;
        assert!(
            snap.status.as_deref().unwrap_or("").contains("headless clients can only use"),
            "headless の Add は拒否: {:?}",
            snap.status
        );
        assert_eq!(snap.review_comment_count, 0);

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #49 adversarial: 存在しない root の登録は拒否（世代不変・登録なし）。
    /// process_command 直呼び（ソケット不要）。
    #[tokio::test]
    async fn base_register_missing_root_rejected() {
        let daemon = Arc::new(Mutex::new(Daemon::new()));
        let (tx, _rx) = watch::channel((None, StateSnapshot::default()));
        let before = daemon.lock().await.generation;
        let line = serde_json::to_string(&Command::RegisterBaseRoot {
            root: "/definitely/not/here-49".into(),
            commit: "abc".into(),
            repo: None,
        })
        .unwrap();
        let snap = process_command(&daemon, &tx, 0, EventSource::Interactive, &line).await;
        assert!(
            snap.status.as_deref().unwrap_or("").contains("ありません"),
            "{:?}",
            snap.status
        );
        assert_eq!(daemon.lock().await.generation, before);
        assert!(daemon.lock().await.base_roots.is_empty());
    }

    #[test]
    fn drop_conn_view_restores_idle_focus() {
        // 切断後始末の欠落回帰（#49 dogfood）: フォーカス中の View を破棄しても
        // idle に戻るため、watch_disk 経路の focused_path() が panic しない。
        let mut d = Daemon::new();
        d.register_conn_view(7);
        let vid = d.conn_views.get(&7).expect("登録").view_id;
        assert!(d.editor.set_focused_view(vid));
        d.drop_conn_view(7);
        assert_eq!(d.editor.focused_view_id(), d.idle_view);
        let _ = d.editor.focused_path();
    }

    #[test]
    fn base_registry_prefix_caches_and_generation() {
        use std::path::{Path, PathBuf};
        let mut d = Daemon::new();
        assert!(!d.is_base_path(Path::new("/r/a.rs")));
        d.register_base_root(PathBuf::from("/r"), "abc1234".into(), None, EventSource::Interactive);
        assert_eq!(d.generation, 1, "登録で世代が進む");
        assert!(d.is_base_path(Path::new("/r/a.rs")));
        assert!(d.is_base_path(Path::new("/r")));
        assert!(!d.is_base_path(Path::new("/other/a.rs")));
        // コンポーネント単位: /r2 は /r の配下ではない。
        assert!(!d.is_base_path(Path::new("/r2/a.rs")));
        let msg = d.base_reject(Path::new("/r/a.rs")).unwrap();
        assert!(msg.contains("abc1234") && msg.contains("読取り専用"), "{msg}");
        assert!(d.base_reject(Path::new("/o/b.rs")).is_none());
        // キャッシュは prefix で捨てる（対象外は残る）。
        d.diagnostics.insert(PathBuf::from("/r/a.rs"), Vec::new());
        d.diagnostics.insert(PathBuf::from("/o/b.rs"), Vec::new());
        let before = d.generation;
        let sessions = d.unregister_base_root(Path::new("/r"), EventSource::Interactive);
        assert!(sessions.is_empty());
        assert!(!d.is_base_path(Path::new("/r/a.rs")));
        assert!(d.diagnostics.contains_key(Path::new("/o/b.rs")));
        assert!(!d.diagnostics.contains_key(Path::new("/r/a.rs")));
        assert_eq!(d.generation, before + 1, "解除で世代が進む");
        // 不在の解除は無視（世代不変）。
        let before2 = d.generation;
        assert!(
            d.unregister_base_root(Path::new("/nope"), EventSource::Interactive)
                .is_empty()
        );
        assert_eq!(d.generation, before2);
    }

    /// #49: 基準配下を起点とするリネームは内容前に拒否する（LSP 不要）。
    #[tokio::test]
    async fn base_rename_anchor_rejected() {
        use std::path::PathBuf;
        let dir = std::env::temp_dir().join(format!("minae-base-rn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn old_fn() {}\n").unwrap();
        let daemon = Mutex::new(Daemon::new());
        let canon = tokio::fs::canonicalize(&dir).await.unwrap();
        {
            let mut d = daemon.lock().await;
            d.register_base_root(canon, "abc1234".into(), None, EventSource::Interactive);
        }
        // 非正規パスで渡しても拒否できること（ガード側で正規化する）。
        let msg = serve_rename(&daemon, &file.to_string_lossy(), "old_fn", "new_fn").await;
        match msg {
            ServerMessage::RenameResult {
                error: Some(e),
                files: 0,
                edits: 0,
                ..
            } => assert!(e.contains("読取り専用"), "{e}"),
            other => panic!("拒否のはず: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0048: ReadPath は LSP 非依存・バッファ非依存の軽量テキスト読み。
    /// 実ファイルをディスクから読み、フォーカス（现在のバッファ）を動かさない。
    #[tokio::test]
    async fn read_path_returns_text_without_touching_focus() {
        let dir = std::env::temp_dir().join(format!("minae-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "line one\nline two\nline three\n").unwrap();
        let sock = dir.join("sock.sock");
        let _ = std::fs::remove_file(&sock);
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Headless).await;
        // normalize_open_path は symlink を解決する（macOS: /var → /private/var）。
        let path = tokio::fs::canonicalize(&file)
            .await
            .unwrap()
            .to_string_lossy()
            .into_owned();

        // ReadPath: 全文が text で返り、error なし。
        let mut line = serde_json::to_string(&Command::ReadPath {
            path: path.clone(),
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::ReadPath {
                path: p,
                text,
                error,
                ..
            } => {
                assert_eq!(error, None, "エラーなし: {error:?}");
                assert_eq!(p, path);
                assert_eq!(text, "line one\nline two\nline three\n");
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        // 読むだけでフォーカスを動かさない: `get`（現在のバッファ）は空のまま。
        let snap = request(&mut c, &Command::GetState).await;
        assert!(snap.path.is_none(), "ReadPath がフォーカスを動かしてはいけない: {snap:?}");

        // 存在しないパスは error（cannot open）。
        let missing = dir.join("nope.txt").to_string_lossy().into_owned();
        let mut line = serde_json::to_string(&Command::ReadPath { path: missing }).unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::ReadPath { error, .. } => {
                assert!(error.as_deref().unwrap_or("").contains("cannot open"), "{error:?}");
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        // 開文書優先: Open した後は未保存編集込みのテキストが読める。
        request(&mut c, &Command::Open { path: path.clone() }).await;
        let mut line = serde_json::to_string(&Command::ReadPath {
            path: path.clone(),
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::ReadPath { text, error, .. } => {
                assert_eq!(error, None);
                assert_eq!(text, "line one\nline two\nline three\n");
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0049: OutlineRecursive はファイル分割モジュール（`mod name;`）を
    /// 定義ジャンプで辿り、定義ファイルのシンボルを children に埋める。
    /// モックサーバ（--bare なし）が `mod` を Module として返し、その位置の
    /// definition が `<name>.rs` を指すことを使う。
    #[tokio::test]
    async fn outline_recursive_follows_file_modules_via_mock() {
        let _guard = LSP_TEST_LOCK.lock().await;
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/mock-server");
        if !mock.exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        unsafe { std::env::set_var("MINA_LSP_COMMAND", &mock) };
        struct ResetEnv;
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("MINA_LSP_COMMAND") };
            }
        }
        let _reset = ResetEnv;

        let dir = std::env::temp_dir().join(format!("minae-outline-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock.sock");
        let _ = std::fs::remove_file(&sock);
        // ルートの lib.rs: `mod model;` 宣言 + シンボル。model.rs は別ファイル。
        let lib = dir.join("lib.rs");
        std::fs::write(&lib, "mod model;\npub fn run() {}\n").unwrap();
        let model = dir.join("model.rs");
        std::fs::write(&model, "pub struct Task;\npub fn make() {}\n").unwrap();
        start_server(&sock).await;

        let mut c = connect_client(&sock, ClientKind::Headless).await;
        let lib_path = lib.to_string_lossy().into_owned();

        // 非再帰（既存 Outline）: model モジュールの children は空。
        let mut line = serde_json::to_string(&Command::Outline { path: lib_path.clone() })
            .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        let flat = match c.recv_message().await {
            ServerMessage::Outline { symbols, .. } => symbols,
            other => panic!("想定外の応答: {other:?}"),
        };
        let model_flat = flat
            .iter()
            .find(|s| s.name == "model")
            .expect("mod model; が Module として返る");
        assert!(model_flat.children.is_empty(), "非再帰では children 空: {model_flat:?}");

        // 再帰（OutlineRecursive depth 1）: model.rs のシンボルが children に埋まる。
        let mut line =
            serde_json::to_string(&Command::OutlineRecursive { path: lib_path.clone(), depth: 1 })
                .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::Outline {
                symbols,
                error,
                truncated,
                ..
            } => {
                assert_eq!(error, None, "エラーなし: {error:?}");
                assert!(!truncated, "小規模ツリーでは打ち切りなし");
                let model = symbols
                    .iter()
                    .find(|s| s.name == "model")
                    .expect("mod model; が Module として返る");
                let names: Vec<_> = model.children.iter().map(|s| s.name.as_str()).collect();
                assert!(
                    names.contains(&"Task") && names.contains(&"make"),
                    "model.rs のシンボルが children に埋まる: {names:?}"
                );
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        // depth 0 は入力エラー（invalid input）。
        let mut line = serde_json::to_string(&Command::OutlineRecursive {
            path: lib_path.clone(),
            depth: 0,
        })
        .unwrap();
        line.push('\n');
        c.send(line.as_bytes()).await;
        match c.recv_message().await {
            ServerMessage::Outline { error, .. } => {
                assert!(
                    error.as_deref().unwrap_or("").starts_with("invalid input"),
                    "depth 0 は invalid input: {error:?}"
                );
            }
            other => panic!("想定外の応答: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn surround_commands_over_the_wire_as_interactive_client() {
        // TUI と同じ経路（process_command のゲート → イベント分類 → apply_from）を
        // 実ソケット越しに通す。headless は #13 ゲートで拒否される（TUI 専用）。
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("minae-19-sock-{}.sock", std::process::id()));
        let file = dir.join(format!("minae-19-surround-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&file, "(abc)").unwrap();
        start_server(&sock).await;

        let mut tui = connect_client(&sock, ClientKind::Interactive).await;
        let path = file.to_string_lossy().into_owned();
        let s = request(&mut tui, &Command::Open { path }).await;
        assert_eq!(s.text, "(abc)");

        // カーソルを index 2（'b'）へ
        for _ in 0..2 {
            request(
                &mut tui,
                &Command::Move {
                    movement: Movement::Char,
                    direction: Direction::Forward,
                },
            )
            .await;
        }
        // md( : カーソルを囲む括弧を削除
        let s = request(&mut tui, &Command::SurroundDelete { ch: '(' }).await;
        assert_eq!(s.text, "abc", "surround_delete が wire 経由で効く");

        // ms[ : 文書全体（%）を囲む
        request(&mut tui, &Command::SelectAll).await;
        let s = request(&mut tui, &Command::SurroundAdd { ch: '[' }).await;
        assert_eq!(s.text, "[abc]");
        assert_eq!(s.selection, vec![Range { anchor: 0, head: 5 }]);
        assert_eq!(s.mode, Mode::Normal, "surround 後は囲んだ全体を選択したまま Normal");

        // mr[{ : 角括弧を波括弧へ
        let s = request(
            &mut tui,
            &Command::SurroundReplace {
                from: '[',
                to: '{',
            },
        )
        .await;
        assert_eq!(s.text, "{abc}");

        // ペアが無ければ状態を変えず status で報告
        let s = request(&mut tui, &Command::SurroundDelete { ch: '(' }).await;
        assert_eq!(s.text, "{abc}");
        assert!(
            s.status.as_deref().unwrap_or("").contains("surround pair"),
            "status: {:?}",
            s.status
        );

        // headless クライアントは拒否（状態も世代も変えない）
        let mut agent = connect_client(&sock, ClientKind::Headless).await;
        let s = request(&mut agent, &Command::SurroundAdd { ch: '(' }).await;
        assert_eq!(s.text, "{abc}");
        assert!(
            s.status.as_deref().unwrap_or("").contains("headless"),
            "status: {:?}",
            s.status
        );

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&file);
    }
}
