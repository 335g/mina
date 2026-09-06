//! minae TUI のアプリケーション状態と実行ループ（ADR-0035/0036）。
//!
//! 単一の `tokio::select!` ループで crossterm `EventStream`・daemon ソケット読み・
//! スピナー tick を同時待ちする（ratatui-async-template 直系。タスク分割しない）。
//! 送信はコマンド直列 + ソケット常時読み。`Response` は直近コマンドの答え、
//! `Push` は逐次適用 — どちらも全文スナップショットなので最新をそのまま状態にする。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind};
use futures_util::StreamExt;
use mina_conn as conn;
use mina_protocol::{
    ActivityRecord, ClientKind, Command, Direction, Mode, Peek, ReviewCommentView, ReviewSide,
    ServerMessage, Severity, StateSnapshot,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::colors::{self, ColorCapability, Colorscheme};
use crate::config;
use crate::git;
use crate::keymap::{Keymaps, Resolution};
use crate::render;

/// スピナーの tick 間隔。
const SPIN_INTERVAL: Duration = Duration::from_millis(80);
/// 接続断後の再接続バックオフ。
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
/// TUI の自己申告ラベル（ADR-0038）。
const CLIENT_NAME: &str = "tui";

/// daemon 接続（書き込み半分 + 読み取り半分）。
pub(crate) struct Conn {
    writer: OwnedWriteHalf,
    reader: BufReader<OwnedReadHalf>,
}

/// オーバーレイの種類（クライアントローカル表示モード — デーモンコマンド不要）。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) enum Overlay {
    #[default]
    None,
    Tree,
    Diagnostics,
    Activity,
    /// PeekDefinition の応答表示（MVP 簡易パネル）。
    Peek,
}

/// コマンドライン系プロンプトの種類（Helix 流の `:` / `/` 等）。
/// クライアントローカル — daemon には確定時だけコマンドを送る（検索はライブで送る）。
#[derive(Debug)]
pub(crate) enum Prompt {
    /// `:` コマンドライン。
    Command(String),
    /// `/`（forward=true）または `?` の検索プロンプト。キー入力のたびに
    /// [`Command::Search`] を送る（ライブ検索）。
    Search { buf: String, forward: bool },
    /// `r` 置換: 次の文字キーで選択/カーソル文字を置換する。
    Replace,
    /// `R` リネーム: カーソル位置の単語（`old`）を新しい名前に変える。
    Rename { buf: String, old: String },
    /// `K` レビューコメント（#50）: 比較表示中のカーソル行（現在側）または
    /// gap 行（基準側）へのコメント入力。Enter で Add（空は削除）、Esc で取消。
    ReviewComment { buf: String, anchor: ReviewAnchor },
}

/// レビューコメント入力が指す差分アンカー（#50）。
#[derive(Clone, Debug)]
pub(crate) struct ReviewAnchor {
    /// 対象ファイル（絶対パス。基準側は worktree 配下）。
    path: String,
    /// どちらの側を指すか。
    side: ReviewSide,
    /// 送信する行番号（1-origin。新規はカーソル行、編集は既存の保存行）。
    line: u32,
    /// 対象行の内容（送信時点）。
    snippet: String,
    /// ピン留めした基準コミット ID。
    base: String,
}

impl Prompt {
    pub(crate) fn prefix(&self) -> char {
        match self {
            Prompt::Command(_) => ':',
            Prompt::Search { forward, .. } => {
                if *forward {
                    '/'
                } else {
                    '?'
                }
            }
            Prompt::Replace => 'r',
            Prompt::Rename { .. } => 'R',
            Prompt::ReviewComment { .. } => '"',
        }
    }

    pub(crate) fn buf(&self) -> &str {
        match self {
            Prompt::Command(b) | Prompt::Search { buf: b, .. } | Prompt::Rename { buf: b, .. } | Prompt::ReviewComment { buf: b, .. } => b,
            Prompt::Replace => "",
        }
    }
}

/// コマンドラインの実行アクション（`:w` 等）。
#[derive(Debug, PartialEq, Eq)]
enum CommandLineAction {
    /// `:w` — 保存して続行。
    Save,
    /// `:q` / `:q!` — 終了。daemon が文書状態を保持し続けるので破棄はない。
    Quit,
    /// `:wq` — 保存してから終了（保存に失敗したら終了しない）。
    SaveThenQuit,
    /// `:o[pen] <path>` — ファイルを開く。
    Open(String),
    /// `:colorscheme [name]` — 引数なしは現在のスキーム名を表示。
    Colorscheme(Option<String>),
    /// 未知のコマンド。
    Unknown(String),
}

/// コマンドライン文字列を解釈する（テスト容易性のため純粋関数）。
fn parse_command(input: &str) -> CommandLineAction {
    let trimmed = input.trim();
    match trimmed {
        "w" => return CommandLineAction::Save,
        "q" | "q!" => return CommandLineAction::Quit,
        "wq" => return CommandLineAction::SaveThenQuit,
        _ => {}
    }
    let mut parts = trimmed.split_whitespace();
    match parts.next() {
        Some("colorscheme") => {
            return CommandLineAction::Colorscheme(parts.next().map(str::to_string));
        }
        Some("open") | Some("o") => {
            return CommandLineAction::Open(parts.collect::<Vec<_>>().join(" "));
        }
        _ => {}
    }
    CommandLineAction::Unknown(trimmed.to_string())
}

/// `:colorscheme [name]` の適用（純粋関数 — テスト容易性）。
///
/// 既知名は `scheme` を差し替えて None、不明名・引数なしは表示すべき flash を返す。
/// 解決は起動時と同じ規則（ユーザーファイル優先 → 組み込み — ADR-0022）。
fn apply_colorscheme(
    scheme: &mut Colorscheme,
    name: Option<&str>,
    schemes_dir: &Path,
) -> Option<String> {
    match name {
        Some(name) => match colors::resolve(name, schemes_dir) {
            Some(s) => {
                *scheme = s;
                None
            }
            None => Some(format!("unknown colorscheme: {name}")),
        },
        None => Some(format!("colorscheme: {}", scheme.name)),
    }
}

/// ファイルツリーの1行。
#[derive(Clone)]
pub(crate) struct TreeEntry {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) depth: usize,
    /// 比較中の変更種別（ファイルのみ）。
    pub(crate) status: Option<git::ChangeStatus>,
    /// 配下の変更ファイル数（ディレクトリの集約マーカー用）。
    pub(crate) subtree_changes: usize,
}

/// ファイルツリーの状態（起動時 cwd 固定・クライアント側 fs 走査）。
pub(crate) struct TreeState {
    pub(crate) root: PathBuf,
    pub(crate) expanded: HashSet<PathBuf>,
    pub(crate) selected: usize,
    pub(crate) entries: Vec<TreeEntry>,
}

impl TreeState {
    fn new(root: PathBuf) -> Self {
        let mut state = Self {
            root,
            expanded: HashSet::new(),
            selected: 0,
            entries: Vec::new(),
        };
        state.refresh();
        state
    }

    /// 表示リストを作り直す（ディレクトリ優先・名前順。畳んだディレクトリは
    /// 子を持たない）。選択は範囲内にクランプする。
    pub(crate) fn refresh(&mut self) {
        let mut entries = Vec::new();
        Self::collect(&self.root, 0, &self.expanded, &mut entries);
        self.entries = entries;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    fn collect(dir: &Path, depth: usize, expanded: &HashSet<PathBuf>, out: &mut Vec<TreeEntry>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut items: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        // 並び順: ディレクトリ優先・名前順（仮置き — 仕様書 §12）
        items.sort_by(|a, b| {
            let ad = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let bd = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
            bd.cmp(&ad).then(a.file_name().cmp(&b.file_name()))
        });
        for item in items {
            let path = item.path();
            let is_dir = item.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let name = item.file_name().to_string_lossy().into_owned();
            out.push(TreeEntry {
                path: path.clone(),
                name,
                is_dir,
                depth,
                status: None,
                subtree_changes: 0,
            });
            if is_dir && expanded.contains(&path) {
                Self::collect(&path, depth + 1, expanded, out);
            }
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        self.selected = (self.selected as isize + delta).clamp(0, len - 1) as usize;
    }

    /// 選択行の決定: ディレクトリは開閉、ファイルはパスを返す。
    pub(crate) fn confirm(&mut self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?.clone();
        if entry.is_dir {
            if !self.expanded.remove(&entry.path) {
                self.expanded.insert(entry.path);
            }
            self.refresh();
            None
        } else {
            Some(entry.path)
        }
    }

    /// 開いている文書をハイライトする（フォーカス追従はしない — 将来オプション）。
    pub(crate) fn select_path(&mut self, path: Option<&str>) {
        let Some(path) = path else { return };
        if let Some(idx) = self.entries.iter().position(|e| {
            !e.is_dir && e.path.to_string_lossy() == path
        }) {
            self.selected = idx;
        }
    }

    /// 変更セットを注釈する（比較表示用 #49）。ファイルは種別、ディレクトリは
    /// 配下件数の集約。changed=None で全クリア。refresh() の後に呼ぶ。
    pub(crate) fn annotate(&mut self, changed: Option<&[git::ChangedFile]>) {
        for e in &mut self.entries {
            e.status = None;
            e.subtree_changes = 0;
        }
        let Some(changed) = changed else { return };
        for e in &mut self.entries {
            if e.is_dir {
                e.subtree_changes = changed.iter().filter(|c| c.path.starts_with(&e.path)).count();
            } else if let Some(c) = changed.iter().find(|c| c.path == e.path) {
                e.status = Some(c.status);
            }
        }
    }
}

/// 比較閲覧 Mode 1 の状態（#49）。クライアントローカル — デーモン無変更。
/// 基準はピン留めしたコミット、現在側はスナップショットのテキスト。
pub(crate) struct CompareState {
    /// ピン留めした基準コミット ID。
    base: String,
    /// リポジトリルート（絶対パス）。
    repo: PathBuf,
    /// 基準コミットの実体（固定パス worktree）。
    worktree: PathBuf,
    /// 注釈表示の ON/OFF（D で切替）。
    show: bool,
    /// 変更一覧（絶対パス）。files_gen 世代のもの。
    files: Vec<git::ChangedFile>,
    /// 一覧を作った snapshot.generation。
    files_gen: u64,
    /// 基準テキストのキャッシュ（基準不変なのでピン中は有効）。
    base_texts: HashMap<String, Option<String>>,
    /// パス → 差分キャッシュ（canvas 変化で再計算）。
    diffs: HashMap<String, CachedDiff>,
    /// 直近の git エラー（flash の重複抑止用）。
    last_err: Option<String>,
}

struct CachedDiff {
    canvas_checksum: u64,
    diff: git::FileDiff,
}

impl CompareState {
    /// ピン留めして初期化（初回 D / B）。基準オブジェクトの存在確認＋
    /// worktree 用意付き（`git stash create` は無名オブジェクトのため）。
    fn pin(repo: PathBuf) -> Result<Self, git::GitError> {
        let base = git::pin_base(&repo)?;
        git::verify_object(&repo, &base)?;
        let worktree = git::ensure_worktree(&repo, &base)?;
        Ok(Self {
            base,
            repo,
            worktree,
            show: true,
            files: Vec::new(),
            files_gen: u64::MAX,
            base_texts: HashMap::new(),
            diffs: HashMap::new(),
            last_err: None,
        })
    }

    pub(crate) fn short(&self) -> &str {
        self.base.get(..7).unwrap_or(&self.base)
    }

    /// 注目文書の基準側絶対パス（worktree 配下・#50）。repo 外は None。
    pub(crate) fn base_path_for(&self, snap_path: &str) -> Option<String> {
        let rel = Path::new(snap_path).strip_prefix(&self.repo).ok()?;
        Some(self.worktree.join(rel).to_string_lossy().into_owned())
    }

    pub(crate) fn is_showing(&self) -> bool {
        self.show
    }

    /// 変更一覧の再計算（世代が進んだら）。Dirty な注目文書は union する
    /// （未保存編集が一覧から漏れないため）。
    fn ensure_files(&mut self, snap: &StateSnapshot) {
        if snap.generation == self.files_gen {
            return;
        }
        self.files_gen = snap.generation;
        let mut files = git::changed_files(&self.repo, &self.base).unwrap_or_default();
        // Dirty な注目文書を union（未保存編集が一覧から漏れないため）。
        if let Some(p) = snap.path.as_deref().filter(|_| snap.dirty).filter(|p| {
            !files.iter().any(|f| f.path == PathBuf::from(p))
        }) {
            files.push(git::ChangedFile {
                path: PathBuf::from(p),
                status: git::ChangeStatus::Modified,
            });
        }
        self.files = files;
    }

    /// 現在文書の差分配列（表示用）。show=false・対象外・エラー時は None。
    /// エラーは内容変化時のみ flash する（毎フレームの spam 回避）。
    fn diff_for(&mut self, snap: &StateSnapshot, flash: &mut Option<String>) -> Option<git::FileDiff> {
        if !self.show {
            return None;
        }
        let path = snap.path.as_deref()?;
        let rel = Path::new(path).strip_prefix(&self.repo).ok()?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        let base = match self.base_texts.get(path) {
            Some(cached) => cached.clone(),
            None => match git::base_text(&self.repo, &self.base, &rel) {
                Err(e) => {
                    self.note_err(format!("{e}"), flash);
                    return None;
                }
                Ok(text) => {
                    self.base_texts.insert(path.to_string(), text.clone());
                    text
                }
            },
        };
        let Some(base) = base else {
            return Some(git::FileDiff::all_added(snap.text.split('\n').count()));
        };
        match self.diffs.get(path) {
            Some(cached) if cached.canvas_checksum == snap.checksum => {
                return Some(cached.diff.clone());
            }
            _ => {}
        }
        match git::diff_texts(&base, &snap.text) {
            Ok(diff) => {
                self.diffs.insert(
                    path.to_string(),
                    CachedDiff {
                        canvas_checksum: snap.checksum,
                        diff: diff.clone(),
                    },
                );
                Some(diff)
            }
            Err(e) => {
                self.diffs.remove(path);
                self.note_err(format!("{e}"), flash);
                None
            }
        }
    }

    fn note_err(&mut self, e: String, flash: &mut Option<String>) {
        if self.last_err.as_deref() != Some(e.as_str()) {
            self.last_err = Some(e.clone());
            *flash = Some(e);
        }
    }
}

/// 診断 view のフォーカス（非モーダル・アクティブ追従 — 仕様書 §13）。
/// View: 診断側を操作（j/k 移動でコード同期）。Editor: コード側を操作
/// （ナビ・編集可能 — 乖離は view に戻ったタイミングでリセットして解消）。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) enum DiagFocus {
    #[default]
    View,
    Editor,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityFilter {
    All,
    Ok,
    Fail,
}

impl ActivityFilter {
    pub(crate) fn cycle(self) -> Self {
        match self {
            ActivityFilter::All => ActivityFilter::Ok,
            ActivityFilter::Ok => ActivityFilter::Fail,
            ActivityFilter::Fail => ActivityFilter::All,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            ActivityFilter::All => "全",
            ActivityFilter::Ok => "成功のみ",
            ActivityFilter::Fail => "失敗のみ",
        }
    }

    pub(crate) fn keeps(self, rec: &ActivityRecord) -> bool {
        match self {
            ActivityFilter::All => true,
            ActivityFilter::Ok => rec.ok,
            ActivityFilter::Fail => !rec.ok,
        }
    }
}

/// gapレビュー中のカーソル（#49）。デーモン選択とは独立（読取り専用モード）。
/// 削除 gap 行の上を移動し、Enter でその位置の過去側定義を peek する。
#[derive(Clone, Copy)]
pub(crate) struct GapCursor {
    pub(crate) gap_idx: usize,
    pub(crate) line_idx: usize,
    pub(crate) col: usize,
}

/// アプリケーション状態。
pub(crate) struct App {
    pub(crate) conn: Option<Conn>,
    pub(crate) snapshot: StateSnapshot,
    pub(crate) keymaps: Keymaps,
    pub(crate) pending: Vec<crossterm::event::KeyEvent>,
    pub(crate) prompt: Option<Prompt>,
    pub(crate) overlay: Overlay,
    pub(crate) tree: TreeState,
    pub(crate) compare: Option<CompareState>,
    pub(crate) gap_review: Option<GapCursor>,
    /// daemon 保持のレビューコメント一覧のキャッシュ（#50）。マーカー表示と
    /// 編集 prefill 用。List 応答で更新し、比較終了・再ピンで捨てる。
    pub(crate) review_list: Vec<ReviewCommentView>,
    pub(crate) diag_filter: Severity,
    pub(crate) diag_index: usize,
    pub(crate) diag_key: Option<(usize, String)>,
    pub(crate) diag_focus: DiagFocus,
    pub(crate) activity_filter: ActivityFilter,
    pub(crate) activity_scroll: usize,
    pub(crate) scheme: Colorscheme,
    pub(crate) schemes_dir: PathBuf,
    pub(crate) capability: ColorCapability,
    pub(crate) no_color: bool,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) body_h: usize,
    pub(crate) sent_viewport: usize,
    pub(crate) tick: u64,
    pub(crate) flash: Option<String>,
    pub(crate) peek: Option<Peek>,
    pub(crate) quit: bool,
    pub(crate) retry_at: Option<Instant>,
}

impl App {
    pub(crate) fn new(scheme: Colorscheme, schemes_dir: PathBuf, capability: ColorCapability, no_color: bool) -> Self {
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            conn: None,
            snapshot: StateSnapshot::default(),
            keymaps: Keymaps::new(),
            pending: Vec::new(),
            prompt: None,
            overlay: Overlay::None,
            tree: TreeState::new(root),
            compare: None,
            gap_review: None,
            review_list: Vec::new(),
            diag_filter: Severity::Error,
            diag_index: 0,
            diag_key: None,
            diag_focus: DiagFocus::View,
            activity_filter: ActivityFilter::All,
            activity_scroll: 0,
            scheme,
            schemes_dir,
            capability,
            no_color,
            width: 80,
            height: 24,
            body_h: 23,
            sent_viewport: 0,
            tick: 0,
            flash: None,
            peek: None,
            quit: false,
            retry_at: None,
        }
    }

    /// daemon へ接続し、Hello を送って GetState で同期する。起動失敗は明示エラー。
    async fn connect(&mut self) -> std::io::Result<()> {
        let socket = mina_protocol::socket_path();
        if conn::connect(&socket).await.is_err() {
            let exe = conn::daemon_exe().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "minad が見つかりません（cargo install minad、または MINAD_EXE で指定）",
                )
            })?;
            conn::spawn_daemon(&exe, &["serve"])?;
            conn::wait_ready(&socket, 50).await?;
        }
        let stream = conn::connect(&socket).await?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        conn::send_hello(&mut write_half, ClientKind::Interactive, true, CLIENT_NAME).await?;
        // GetState で初期同期する（応答 = 空でない最初のスナップショット）
        let cmd = serde_json::to_string(&Command::GetState).expect("直列化可能");
        use tokio::io::AsyncWriteExt;
        write_half.write_all(cmd.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if let Ok(ServerMessage::Response { snapshot } | ServerMessage::Push { snapshot }) =
            serde_json::from_str::<ServerMessage>(&line)
        {
            self.snapshot = snapshot;
        }
        self.conn = Some(Conn {
            writer: write_half,
            reader,
        });
        self.retry_at = None;
        self.flash = None;
        Ok(())
    }

    /// 接続断: 接続を落とし、ステータス報知 + バックオフ再接続へ。
    fn disconnect(&mut self, notice: impl Into<String>) {
        self.conn = None;
        self.retry_at = Some(Instant::now() + RECONNECT_BACKOFF);
        self.flash = Some(notice.into());
    }

    /// コマンドを直列送信する（応答は常時読みループで受ける）。切断時は落とす。
    async fn send(&mut self, cmd: &Command) {
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let mut line = serde_json::to_string(cmd).expect("直列化可能");
        line.push('\n');
        if conn.writer.write_all(line.as_bytes()).await.is_err()
            || conn.writer.flush().await.is_err()
        {
            self.disconnect("daemon との接続が切れました — 再接続します");
        }
    }

    /// 表示高さを daemon に通知する（カーソル追従スクロール用 — ADR-0023）。
    async fn send_viewport(&mut self) {
        if self.body_h == 0 || self.body_h == self.sent_viewport {
            return;
        }
        self.sent_viewport = self.body_h;
        let height = self.body_h;
        self.send(&Command::SetViewport { height }).await;
    }

    /// ソケットから 1 行読んだ結果を処理する。Response/Push はどちらも最新状態。
    async fn on_socket_line(&mut self, line: String) {
        let Ok(msg) = serde_json::from_str::<ServerMessage>(&line) else {
            return; // 壊れた行は無視する（接続は維持）
        };
        match msg {
            ServerMessage::Response { snapshot } | ServerMessage::Push { snapshot } => {
                // PeekDefinition の応答だけが peek を運ぶ — 取り出して保持する
                // （後続のスナップショットでは消さない。Esc か次の peek で消える）。
                if let Some(peek) = snapshot.peek.clone() {
                    self.peek = Some(peek);
                    self.overlay = Overlay::Peek;
                }
                self.snapshot = snapshot;
                self.reconcile_diag();
                // 比較中: ツリー表示中だけ一覧を追随させる（毎 push の git 呼び出し回避）。
                if self.overlay == Overlay::Tree
                    && self.compare.as_ref().is_some_and(|c| c.show)
                {
                    let snap = self.snapshot.clone();
                    let cmp = self.compare.as_mut().expect("checked above");
                    cmp.ensure_files(&snap);
                    let files = cmp.files.clone();
                    self.tree.annotate(Some(&files));
                }
            }
            // #50: レビューコメント一覧（マーカー・編集 prefill 用キャッシュ）。
            ServerMessage::ReviewComments { comments, .. } => {
                self.review_list = comments;
            }
            // TUI は他の軽量応答を送らない（使わない）ので無視する
            _ => {}
        }
    }

    /// スナップショット更新後に診断 view の active を追従させる。
    /// 修正 → 保存でアクティブ診断が消えたら、フィルタ内の次の診断へ自動前進する。
    fn reconcile_diag(&mut self) {
        let list = self.filtered_diags();
        if list.is_empty() {
            self.diag_index = 0;
            self.diag_key = None;
            return;
        }
        if let Some(key) = &self.diag_key {
            if let Some(pos) = list.iter().position(|(s, m)| (*s, m.as_str()) == (key.0, key.1.as_str())) {
                self.diag_index = pos;
                return;
            }
        }
        // 消えた: 同位置（= 次の診断）へ前進、末尾なら末尾へクランプ
        self.diag_index = self.diag_index.min(list.len() - 1);
        let (start, msg) = &list[self.diag_index];
        self.diag_key = Some((*start, (*msg).to_string()));
    }

    /// フィルタ内の診断一覧（スナップショット順）。
    pub(crate) fn filtered_diags(&self) -> Vec<(usize, String)> {
        self.snapshot
            .diagnostics
            .iter()
            .filter(|d| d.severity == self.diag_filter)
            .map(|d| (d.start, d.message.clone()))
            .collect()
    }

    /// アクティブ診断（reconcile 済み）。
    pub(crate) fn active_diag(&self) -> Option<(usize, String)> {
        self.filtered_diags()
            .get(self.diag_index)
            .map(|(s, m)| (*s, m.clone()))
    }

    /// エディタのカーソルを診断位置へ同期移動させる（行・列差分を Move で送る）。
    async fn sync_cursor_to_diag(&mut self) {
        let Some((start, _)) = self.active_diag() else {
            return;
        };
        let (cur_line, cur_col) = render::cursor_line_col(&self.snapshot);
        let (tgt_line, tgt_col) = render::offset_to_line_col(&self.snapshot.text, start);
        let line_delta = tgt_line as isize - cur_line as isize;
        let dir = if line_delta >= 0 {
            Direction::Forward
        } else {
            Direction::Backward
        };
        for _ in 0..line_delta.unsigned_abs() {
            self.send(&Command::Move {
                movement: mina_protocol::Movement::Line,
                direction: dir,
            })
            .await;
        }
        let col_delta = tgt_col as isize - cur_col as isize;
        let dir = if col_delta >= 0 {
            Direction::Forward
        } else {
            Direction::Backward
        };
        for _ in 0..col_delta.unsigned_abs() {
            self.send(&Command::Move {
                movement: mina_protocol::Movement::Char,
                direction: dir,
            })
            .await;
        }
    }

    /// キー入力の振り分け: プロンプト > オーバーレイ > エディタ。
    async fn handle_key(&mut self, key: KeyEvent) {
        self.flash = None;
        // gapレビュー中は専用処理（Ctrl-C の終了だけは共通）。
        if self.gap_review.is_some() {
            if key.code == crossterm::event::KeyCode::Char('c')
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                self.quit = true;
                return;
            }
            self.handle_gap_key(key).await;
            return;
        }
        if self.prompt.is_some() {
            self.handle_prompt_key(key).await;
            return;
        }
        if self.overlay != Overlay::None {
            self.handle_overlay_key(key).await;
            return;
        }
        self.handle_editor_key(key).await;
    }

    /// エディタ（オーバーレイなし・プロンプトなし）のキー処理。
    async fn handle_editor_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        // Ctrl-C は即終了（daemon は常駐なので破棄はない）
        if key.code == Char('c') && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        let mode = self.snapshot.mode;
        // Normal のオーバーレイキー（T/G/A/C — G は診断 view。Goto 末尾は `g e`）
        if mode == Mode::Normal && key.modifiers.is_empty() {
            match key.code {
                Char('T') => {
                    self.pending.clear();
                    self.tree.refresh();
                    self.tree.select_path(self.snapshot.path.as_deref());
                    self.annotate_tree();
                    self.overlay = Overlay::Tree;
                    return;
                }
                Char('G') => {
                    self.pending.clear();
                    self.reconcile_diag();
                    self.overlay = Overlay::Diagnostics;
                    self.diag_focus = DiagFocus::View;
                    self.sync_cursor_to_diag().await;
                    return;
                }
                Char('A') => {
                    self.pending.clear();
                    self.activity_scroll = usize::MAX; // 末尾（最新）から
                    self.overlay = Overlay::Activity;
                    return;
                }
                Char('C') => {
                    self.pending.clear();
                    self.cycle_colorscheme();
                    return;
                }
                // D: 比較表示の切替（初回はピン留め）。B: 基準の更新。
                // Normal 先行キー（T/G/A/C と同列 — keymap より優先）。
                Char('D') => {
                    self.pending.clear();
                    self.toggle_compare().await;
                    return;
                }
                Char('B') => {
                    self.pending.clear();
                    self.repin_compare().await;
                    return;
                }
                // K: レビューコメント入力（比較表示中のみ。現在側カーソル行）。
                Char('K') => {
                    self.pending.clear();
                    self.open_review_prompt_current();
                    return;
                }
                _ => {}
            }
        }
        // Tab: gapレビュー開始（Normal のみ。keymap より優先）。
        if mode == Mode::Normal && key.code == KeyCode::Tab && key.modifiers.is_empty() {
            self.pending.clear();
            self.enter_gap_review();
            return;
        }
        // プロンプトを開くキー（Normal/Select。Insert では文字入力）
        if mode != Mode::Insert
            && (key.modifiers.is_empty() || key.modifiers == crossterm::event::KeyModifiers::SHIFT)
        {
            match key.code {
                Char(':') => {
                    self.pending.clear();
                    self.prompt = Some(Prompt::Command(String::new()));
                    return;
                }
                Char('/') | Char('?') => {
                    self.pending.clear();
                    let forward = key.code == Char('/');
                    self.prompt = Some(Prompt::Search {
                        buf: String::new(),
                        forward,
                    });
                    return;
                }
                Char('r') => {
                    self.pending.clear();
                    self.prompt = Some(Prompt::Replace);
                    return;
                }
                Char('R') => {
                    self.pending.clear();
                    match word_at_cursor(&self.snapshot) {
                        Some(old) => {
                            self.prompt = Some(Prompt::Rename {
                                buf: String::new(),
                                old,
                            })
                        }
                        None => self.flash = Some("no symbol under cursor".into()),
                    }
                    return;
                }
                _ => {}
            }
        }
        // キーマップ解決（Insert は未バインド文字を挿入にフォールバック）
        match self
            .keymaps
            .resolve_with_insert_fallback(mode, &mut self.pending, key)
        {
            Resolution::Command(command) => {
                let is_peek = matches!(&command, Command::PeekDefinition);
                self.send(&command).await;
                // peek の応答は on_socket_line で取り出す。応答が空定義の
                // 場合は何も起きないため、ここでは何もしない。
                let _ = is_peek;
            }
            Resolution::Pending | Resolution::NoMatch => {}
        }
    }

    /// オーバーレイ表示中のキー処理。
    async fn handle_overlay_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let close_on_esc = matches!(key.code, Esc);
        match self.overlay {
            Overlay::Tree => {
                match key.code {
                    Esc => self.overlay = Overlay::None,
                    Char('T') if key.modifiers.is_empty() => self.overlay = Overlay::None,
                    Down | Char('j') if key.modifiers.is_empty() => self.tree.move_selection(1),
                    Up | Char('k') if key.modifiers.is_empty() => self.tree.move_selection(-1),
                    // n/N: 次/前の変更ファイルへジャンプ（比較中のみ有効）。
                    Char('n') => self.jump_changed(1),
                    Char('N') => self.jump_changed(-1),
                    Enter => {
                        let opened = self.tree.confirm();
                        self.annotate_tree();
                        if let Some(path) = opened {
                            self.overlay = Overlay::None;
                            self.send(&Command::Open {
                                path: conn::absolutize(&path.to_string_lossy()),
                            })
                            .await;
                        }
                    }
                    _ => {}
                }
                let _ = close_on_esc;
            }
            Overlay::Diagnostics => {
                // Tab: view ⇄ エディタのフォーカス切替（両側でナビ・編集可能）。
                // エディタ側へ戻したときはアクティブ診断位置へリセットする。
                if key.code == KeyCode::Tab && key.modifiers.is_empty() {
                    self.diag_focus = match self.diag_focus {
                        DiagFocus::View => DiagFocus::Editor,
                        DiagFocus::Editor => DiagFocus::View,
                    };
                    if self.diag_focus == DiagFocus::View {
                        self.sync_cursor_to_diag().await;
                    }
                    return;
                }
                // エディタ側にフォーカスがある間はエディタとして振る舞う
                // （Esc/Tab/G はオーバーレイ操作として横取りする）。
                if self.diag_focus == DiagFocus::Editor
                    && !matches!(key.code, KeyCode::Esc)
                    && !(key.code == KeyCode::Char('G') && key.modifiers.is_empty())
                {
                    self.handle_editor_key(key).await;
                    return;
                }
                match key.code {
                    Esc => self.overlay = Overlay::None,
                    Char('G') if key.modifiers.is_empty() => self.overlay = Overlay::None,
                    // j/k: フィルタ内の次/前へ進み、コード側も同期移動
                    Char('j') | Down if key.modifiers.is_empty() => {
                        self.advance_diag(1).await;
                    }
                    Char('k') | Up if key.modifiers.is_empty() => {
                        self.advance_diag(-1).await;
                    }
                    // e/w/i/h: 重要度フィルタ切替
                    Char('e') if key.modifiers.is_empty() => self.set_diag_filter(Severity::Error).await,
                    Char('w') if key.modifiers.is_empty() => {
                        self.set_diag_filter(Severity::Warning).await
                    }
                    Char('i') if key.modifiers.is_empty() => self.set_diag_filter(Severity::Info).await,
                    Char('h') if key.modifiers.is_empty() => self.set_diag_filter(Severity::Hint).await,
                    Enter => self.overlay = Overlay::None,
                    _ => {}
                }
                let _ = close_on_esc;
            }
            Overlay::Activity => {
                match key.code {
                    Esc => self.overlay = Overlay::None,
                    Char('A') if key.modifiers.is_empty() => self.overlay = Overlay::None,
                    Char('f') if key.modifiers.is_empty() => {
                        self.activity_filter = self.activity_filter.cycle();
                        self.activity_scroll = usize::MAX;
                    }
                    Down | Char('j') if key.modifiers.is_empty() => {
                        self.activity_scroll = self.activity_scroll.saturating_add(1);
                    }
                    Up | Char('k') if key.modifiers.is_empty() => {
                        self.activity_scroll = self.activity_scroll.saturating_sub(1);
                    }
                    _ => {}
                }
                let _ = close_on_esc;
            }
            Overlay::Peek => {
                // 何か押したら閉じる（Esc / Space k の再送は除く扱いにしない）
                self.overlay = Overlay::None;
                self.peek = None;
                // Esc 以外のキーはエディタへ転送しない（誤操作防止）
            }
            Overlay::None => {}
        }
    }

    /// 診断 view の前進/後退 + コード同期。
    async fn advance_diag(&mut self, delta: isize) {
        let list = self.filtered_diags();
        if list.is_empty() {
            return;
        }
        let len = list.len() as isize;
        self.diag_index = (self.diag_index as isize + delta).clamp(0, len - 1) as usize;
        let (start, msg) = &list[self.diag_index];
        self.diag_key = Some((*start, (*msg).clone()));
        self.sync_cursor_to_diag().await;
    }

    /// 診断フィルタの切替（先頭へ）。
    async fn set_diag_filter(&mut self, filter: Severity) {
        if self.diag_filter == filter {
            return;
        }
        self.diag_filter = filter;
        self.diag_index = 0;
        self.diag_key = self.filtered_diags().first().map(|(s, m)| (*s, m.clone()));
    }

    /// プロンプト表示中のキー処理。
    async fn handle_prompt_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        match prompt {
            Prompt::Command(mut buf) => match key.code {
                Char(c)
                    if key.modifiers.is_empty()
                        || key.modifiers == crossterm::event::KeyModifiers::SHIFT =>
                {
                    buf.push(c);
                    self.prompt = Some(Prompt::Command(buf));
                }
                Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {}
                Backspace => {
                    buf.pop();
                    self.prompt = Some(Prompt::Command(buf));
                }
                Esc => {}
                Enter => self.run_command_line(&buf).await,
                _ => self.prompt = Some(Prompt::Command(buf)),
            },
            Prompt::Search { mut buf, forward } => {
                // ライブ検索: キー入力のたびに Search を送る
                let keep = match key.code {
                    Char(c)
                        if key.modifiers.is_empty()
                            || key.modifiers == crossterm::event::KeyModifiers::SHIFT =>
                    {
                        buf.push(c);
                        true
                    }
                    Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                        false
                    }
                    Backspace => {
                        buf.pop();
                        true
                    }
                    Esc | Enter => {
                        // 確定/キャンセル: 最後のライブ検索が現状
                        false
                    }
                    _ => true,
                };
                if keep {
                    if !buf.is_empty() {
                        self.send(&Command::Search {
                            query: buf.clone(),
                            direction: if forward {
                                Direction::Forward
                            } else {
                                Direction::Backward
                            },
                        })
                        .await;
                    }
                    self.prompt = Some(Prompt::Search { buf, forward });
                }
            }
            Prompt::Replace => match key.code {
                // r: 次の文字キーで置換確定。Esc/C-c でキャンセル。
                Char(c)
                    if key.modifiers.is_empty()
                        || key.modifiers == crossterm::event::KeyModifiers::SHIFT =>
                {
                    self.send(&Command::Replace {
                        text: c.to_string(),
                    })
                    .await;
                }
                Esc => {}
                Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {}
                _ => self.prompt = Some(Prompt::Replace),
            },
            Prompt::Rename { mut buf, old } => match key.code {
                Char(c)
                    if key.modifiers.is_empty()
                        || key.modifiers == crossterm::event::KeyModifiers::SHIFT =>
                {
                    buf.push(c);
                    self.prompt = Some(Prompt::Rename { buf, old });
                }
                Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {}
                Backspace => {
                    buf.pop();
                    self.prompt = Some(Prompt::Rename { buf, old });
                }
                Esc => {}
                Enter => {
                    let new = buf.trim().to_string();
                    match (self.snapshot.path.clone(), !new.is_empty()) {
                        (Some(path), true) => {
                            self.send(&Command::Rename {
                                path,
                                old,
                                new,
                            })
                            .await;
                        }
                        (None, _) => self.flash = Some("no file open".into()),
                        (_, false) => self.flash = Some("empty replacement".into()),
                    }
                }
                _ => self.prompt = Some(Prompt::Rename { buf, old }),
            },
            // K: レビューコメント入力（#50）。Enter で Add（空は削除）、Esc で取消。
            Prompt::ReviewComment { mut buf, anchor } => match key.code {
                Char(c)
                    if key.modifiers.is_empty()
                        || key.modifiers == crossterm::event::KeyModifiers::SHIFT =>
                {
                    buf.push(c);
                    self.prompt = Some(Prompt::ReviewComment { buf, anchor });
                }
                Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {}
                Backspace => {
                    buf.pop();
                    self.prompt = Some(Prompt::ReviewComment { buf, anchor });
                }
                Esc => {}
                Enter => self.submit_review_comment(anchor, buf).await,
                _ => self.prompt = Some(Prompt::ReviewComment { buf, anchor }),
            },
        }
    }

    /// `:` コマンドラインの実行。
    async fn run_command_line(&mut self, buf: &str) {
        match parse_command(buf) {
            CommandLineAction::Save => {
                self.send(&Command::Save).await;
            }
            CommandLineAction::Quit => self.quit = true,
            CommandLineAction::SaveThenQuit => {
                self.send(&Command::Save).await;
                // 保存応答を1行読んで dirty を確認する（単一タスクなので
                // ソケット読みとの競合はない）。保存に失敗したら終了しない。
                if let Some(c) = self.conn.as_mut() {
                    let mut line = String::new();
                    if c.reader.read_line(&mut line).await.is_ok() {
                        self.on_socket_line(line).await;
                    }
                }
                if !self.snapshot.dirty {
                    self.quit = true;
                }
            }
            CommandLineAction::Colorscheme(name) => {
                if let Some(msg) = apply_colorscheme(&mut self.scheme, name.as_deref(), &self.schemes_dir.clone()) {
                    self.flash = Some(msg);
                }
            }
            CommandLineAction::Unknown(cmd) => {
                self.flash = Some(format!("unknown command: {cmd}"));
            }
            CommandLineAction::Open(path) => {
                if path.is_empty() {
                    self.flash = Some("usage: open <path>".into());
                } else {
                    self.send(&Command::Open {
                        path: conn::absolutize(&path),
                    })
                    .await;
                }
            }
        }
    }

    /// カラースキームを組み込み間でサイクルする（`C` — デバッグ用でも可）。
    fn cycle_colorscheme(&mut self) {
        use crate::colors::builtin_names;
        let names = builtin_names();
        let next = names
            .iter()
            .position(|n| *n == self.scheme.name)
            .map(|i| names[(i + 1) % names.len()])
            .unwrap_or(names[0]);
        if let Some(scheme) = colors::resolve(next, &self.schemes_dir) {
            self.scheme = scheme;
        }
    }

    /// ツリーに比較マーカーを注釈する（非表示・未開始ならクリア）。
    fn annotate_tree(&mut self) {
        let files = self
            .compare
            .as_ref()
            .filter(|c| c.is_showing())
            .map(|c| c.files.clone());
        self.tree.annotate(files.as_deref());
    }

    /// D: 比較表示の切替。初回は現在状態をピン留め＋基準登録する。
    /// レビューコメント一覧を daemon から取り直す（#50）。比較表示中のみ送る。
    /// 応答は on_socket_line で review_list に載る（マーカー・prefill 用）。
    async fn refresh_reviews(&mut self) {
        if self.compare.as_ref().is_some_and(|c| c.is_showing()) {
            self.send(&Command::ListReviewComments).await;
        }
    }

    /// K（現在側）: カーソル行へのコメント入力を開く（#50）。比較表示中のみ。
    /// 2回目は既存本文を prefill し、空 Enter で削除する（トグル・Q8）。
    fn open_review_prompt_current(&mut self) {
        let Some(cmp) = self.compare.as_ref().filter(|c| c.is_showing()) else {
            self.flash = Some("比較を開始してください（D）".into());
            return;
        };
        let Some(path) = self.snapshot.path.clone() else {
            self.flash = Some("no file open".into());
            return;
        };
        let (line0, _) = render::cursor_line_col(&self.snapshot);
        let line_no = (line0 + 1) as u32;
        let snippet = self.snapshot.text.lines().nth(line0).unwrap_or("").to_string();
        let base = cmp.base.clone();
        // 編集: 解決行 or 保存行がカーソル行に当たる既存を prefill する。
        // 送信は保存行で行い、snippet だけ現行に更新する（重複を作らない）。
        let existing = Self::find_existing(&self.review_list, ReviewSide::Current, &path, line_no);
        let (line, buf) = match existing {
            Some(e) => (e.line, e.body.clone()),
            None => (line_no, String::new()),
        };
        self.prompt = Some(Prompt::ReviewComment {
            buf,
            anchor: ReviewAnchor {
                path,
                side: ReviewSide::Current,
                line,
                snippet,
                base,
            },
        });
    }

    /// K（基準側・gapレビュー中）: gap 行へのコメント入力を開く（#50）。
    fn open_review_prompt_gap(&mut self, gap_idx: usize, line_idx: usize) {
        let Some((wt_path, line_no, snippet, base)) = (|| -> Option<(String, u32, String, String)> {
            let snap_path = self.snapshot.path.clone()?;
            let base = self.compare.as_ref()?.base.clone();
            let wt = self.compare.as_ref()?.base_path_for(&snap_path)?;
            let diff = self.compare_diff_for_render()?;
            let gap = diff.gaps.get(gap_idx)?;
            let text = gap.lines.get(line_idx)?.clone();
            Some((
                wt,
                (gap.old_start + line_idx) as u32,
                text,
                base,
            ))
        })() else {
            self.flash = Some("比較差分がありません".into());
            return;
        };
        let existing = Self::find_existing(&self.review_list, ReviewSide::Base, &wt_path, line_no);
        let (line, buf) = match existing {
            Some(e) => (e.line, e.body.clone()),
            None => (line_no, String::new()),
        };
        self.gap_review = None;
        self.prompt = Some(Prompt::ReviewComment {
            buf,
            anchor: ReviewAnchor {
                path: wt_path,
                side: ReviewSide::Base,
                line,
                snippet,
                base,
            },
        });
    }

/// コメント編集中の既存検索（#50・純粋関数）。現在側は解決行（ずれた先）
/// または保存行で当て、基準側は不変なので保存行で当てる。見つかれば
/// その保存行で上書き送信する（重複を作らない）。
fn find_existing<'a>(
    list: &'a [ReviewCommentView],
    side: ReviewSide,
    path: &str,
    line_no: u32,
) -> Option<&'a ReviewCommentView> {
    list.iter().find(|e| {
        e.side == side
            && e.path == path
            && (e.line == line_no
                || (side == ReviewSide::Current && e.resolved_line == line_no))
    })
}

    /// レビューコメント入力の確定（#50）。Add を送り、一覧を取り直す。
    /// 空本文はそのアンカーの削除（daemon 側の upsert-delete）。
    async fn submit_review_comment(&mut self, anchor: ReviewAnchor, body: String) {
        let deleted = body.trim().is_empty();
        self.send(&Command::AddReviewComment {
            path: anchor.path,
            side: anchor.side,
            line: anchor.line,
            snippet: anchor.snippet,
            body,
            base: anchor.base,
        })
        .await;
        self.refresh_reviews().await;
        self.flash = Some(if deleted { "コメント削除".into() } else { "コメント登録".into() });
    }

    async fn toggle_compare(&mut self) {
        if self.compare.is_some() {
            let (show_now, root, commit) = {
                let cmp = self.compare.as_mut().expect("is_some で確認済み");
                cmp.show = !cmp.show;
                (
                    cmp.show,
                    cmp.worktree.to_string_lossy().into_owned(),
                    cmp.base.clone(),
                )
            };
            if show_now {
                // worktree 検証（消えていたら作り直す。失敗時は注釈のみで続行）。
                let usable = {
                    let cmp = self.compare.as_ref().expect("is_some で確認済み");
                    if git::worktree_usable(&cmp.worktree) {
                        true
                    } else {
                        git::ensure_worktree(&cmp.repo, &cmp.base).is_ok()
                    }
                };
                if usable {
                    self.send(&Command::RegisterBaseRoot { root, commit }).await;
                } else {
                    self.flash =
                        Some("worktree を作り直せませんでした（注釈のみ表示）".into());
                }
                if let Some(cmp) = self.compare.as_mut() {
                    cmp.files_gen = u64::MAX;
                    let snap = self.snapshot.clone();
                    cmp.ensure_files(&snap);
                }
                // #50: 同一基準の再登録はコメント保持のため取り直す。
                self.refresh_reviews().await;
            } else {
                self.send(&Command::UnregisterBaseRoot { root }).await;
                // #50: Unregister で daemon 側も全消しされるため捨てる。
                self.review_list.clear();
            }
            self.annotate_tree();
            return;
        }
        let root = self.tree.root.clone();
        let repo = match git::repo_root(&root) {
            Ok(r) => r,
            Err(e) => {
                self.flash = Some(format!("比較を開始できません: {e}"));
                return;
            }
        };
        let mut cmp = match CompareState::pin(repo) {
            Ok(c) => c,
            Err(e) => {
                self.flash = Some(format!("比較を開始できません: {e}"));
                return;
            }
        };
        self.send(&Command::RegisterBaseRoot {
            root: cmp.worktree.to_string_lossy().into_owned(),
            commit: cmp.base.clone(),
        })
        .await;
        let snap = self.snapshot.clone();
        cmp.ensure_files(&snap);
        self.flash = Some(format!(
            "基準 {} にピン留め（D:表示切替 B:更新 Tab:削除レビュー）",
            cmp.short()
        ));
        self.compare = Some(cmp);
        self.annotate_tree();
        // #50: 初回ピン時は daemon 側は空のはずだが、一覧を取り直して揃える。
        self.refresh_reviews().await;
    }

    /// B: 基準を現在状態に更新する（一覧・差分キャッシュを作り直す）。
    /// 失敗時は旧状態を維持する。
    async fn repin_compare(&mut self) {
        let Some((repo, show)) = self.compare.as_ref().map(|c| (c.repo.clone(), c.show)) else {
            self.flash = Some("比較を開始してからピン留めしてください（D）".into());
            return;
        };
        let mut fresh = match CompareState::pin(repo) {
            Ok(f) => f,
            Err(e) => {
                self.flash = Some(format!("ピン留めできません（旧基準を維持）: {e}"));
                return;
            }
        };
        fresh.show = show;
        let snap = self.snapshot.clone();
        fresh.ensure_files(&snap);
        self.send(&Command::RegisterBaseRoot {
            root: fresh.worktree.to_string_lossy().into_owned(),
            commit: fresh.base.clone(),
        })
        .await;
        let short = fresh.short().to_string();
        self.compare = Some(fresh);
        self.annotate_tree();
        // #50: 再ピンで daemon 側は全消しされるため捨てる。
        self.review_list.clear();
        self.flash = Some(format!("基準 {short} に更新"));
    }

    /// ツリー上で次/前の変更ファイルへジャンプする（n/N）。
    fn jump_changed(&mut self, delta: isize) {
        let show = self.compare.as_ref().map(|c| c.show).unwrap_or(false);
        if !show {
            self.flash = Some("比較を開始してください（D）".into());
            return;
        }
        let marked: Vec<usize> = self
            .tree
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_dir && e.status.is_some())
            .map(|(i, _)| i)
            .collect();
        if marked.is_empty() {
            self.flash = Some("変更ファイルがありません".into());
            return;
        }
        let cur = self.tree.selected;
        let next = if delta > 0 {
            marked.iter().find(|&&i| i > cur).or(marked.first())
        } else {
            marked.iter().rev().find(|&&i| i < cur).or(marked.last())
        };
        self.tree.selected = *next.unwrap_or(&cur);
    }

    /// 現在文書の比較差分（render 毎フレーム用。checksum キーなので
    /// git 呼び出しは変化時のみ）。
    pub(crate) fn compare_diff_for_render(&mut self) -> Option<git::FileDiff> {
        let snap = self.snapshot.clone();
        let cmp = self.compare.as_mut()?;
        let mut flash = None;
        let d = cmp.diff_for(&snap, &mut flash);
        if let Some(msg) = flash {
            self.flash = Some(msg);
        }
        d
    }

    /// Tab: gapレビュー開始（削除行があるときのみ）。
    fn enter_gap_review(&mut self) {
        let show = self.compare.as_ref().map(|c| c.is_showing()).unwrap_or(false);
        if !show {
            self.flash = Some("比較を開始してください（D）".into());
            return;
        }
        match self.compare_diff_for_render() {
            Some(d) if !d.gaps.is_empty() => {
                self.gap_review = Some(GapCursor {
                    gap_idx: 0,
                    line_idx: 0,
                    col: 0,
                });
            }
            _ => {
                self.flash = Some("削除行がありません".into());
            }
        }
    }

    /// gapレビュー中のキー処理（#49）。移動・決定・終了のみ。編集不可。
    /// デーモン選択には触らないため、表示と daemon 状態の乖離は起きない。
    async fn handle_gap_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyCode::*;
        let Some(diff) = self.compare_diff_for_render() else {
            self.gap_review = None;
            self.flash = Some("比較差分がありません".into());
            return;
        };
        if diff.gaps.is_empty() {
            self.gap_review = None;
            self.flash = Some("削除行がありません".into());
            return;
        }
        let mut cur = self.gap_review.unwrap_or(GapCursor {
            gap_idx: 0,
            line_idx: 0,
            col: 0,
        });
        // canvas 変化で dangling したらクランプする。
        cur.gap_idx = cur.gap_idx.min(diff.gaps.len() - 1);
        let max_line = diff.gaps[cur.gap_idx].lines.len().saturating_sub(1);
        cur.line_idx = cur.line_idx.min(max_line);
        let max_col = diff.gaps[cur.gap_idx].lines[cur.line_idx].chars().count();
        cur.col = cur.col.min(max_col);
        match key.code {
            Esc | Tab => {
                self.gap_review = None;
                return;
            }
            // K: gap 行（基準側）へのコメント入力（#50）。
            Char('K') if key.modifiers.is_empty() => {
                self.open_review_prompt_gap(cur.gap_idx, cur.line_idx);
                return;
            }
            Down | Char('j') if key.modifiers.is_empty() => {
                if cur.line_idx + 1 < diff.gaps[cur.gap_idx].lines.len() {
                    cur.line_idx += 1;
                } else if cur.gap_idx + 1 < diff.gaps.len() {
                    cur.gap_idx += 1;
                    cur.line_idx = 0;
                }
                cur.col = cur
                    .col
                    .min(diff.gaps[cur.gap_idx].lines[cur.line_idx].chars().count());
            }
            Up | Char('k') if key.modifiers.is_empty() => {
                if cur.line_idx > 0 {
                    cur.line_idx -= 1;
                } else if cur.gap_idx > 0 {
                    cur.gap_idx -= 1;
                    cur.line_idx = diff.gaps[cur.gap_idx].lines.len() - 1;
                }
                cur.col = cur
                    .col
                    .min(diff.gaps[cur.gap_idx].lines[cur.line_idx].chars().count());
            }
            Left | Char('h') if key.modifiers.is_empty() => {
                cur.col = cur.col.saturating_sub(1);
            }
            Right | Char('l') if key.modifiers.is_empty() => {
                cur.col = (cur.col + 1)
                    .min(diff.gaps[cur.gap_idx].lines[cur.line_idx].chars().count());
            }
            Enter => {
                let gap = &diff.gaps[cur.gap_idx];
                let line_no = (gap.old_start + cur.line_idx) as u32;
                let col_no = (cur.col + 1) as u32; // 1-origin
                let jump = (|| {
                    let cmp = self.compare.as_ref()?;
                    let snap_path = self.snapshot.path.as_deref()?;
                    let rel = Path::new(snap_path).strip_prefix(&cmp.repo).ok()?;
                    Some((cmp.worktree.join(rel), line_no, col_no))
                })();
                match jump {
                    Some((tp, ln, co)) => {
                        self.send(&Command::PeekDefinitionAt {
                            path: tp.to_string_lossy().into_owned(),
                            line: ln,
                            col: co,
                        })
                        .await;
                    }
                    None => {
                        self.gap_review = None;
                    }
                }
                return;
            }
            _ => {
                self.flash = Some("gapレビュー中です（Escで戻る）".into());
                return;
            }
        }
        self.gap_review = Some(cur);
    }

    /// 基準側パス（worktree 配下）の表示用変換（#49）。対象外はそのまま。
    pub(crate) fn base_display_path(&self, path: &str) -> String {
        self.compare
            .as_ref()
            .and_then(|cmp| {
                Path::new(path).strip_prefix(&cmp.worktree).ok().map(|rel| {
                    format!("{} @{}", rel.display(), cmp.short())
                })
            })
            .unwrap_or_else(|| path.to_string())
    }

    /// 終了時の後始末（best-effort）: 登録解除＋worktree 撤去。
    async fn shutdown_compare(&mut self) {
        self.review_list.clear();
        let Some(cmp) = self.compare.take() else {
            return;
        };
        self.send(&Command::UnregisterBaseRoot {
            root: cmp.worktree.to_string_lossy().into_owned(),
        })
        .await;
        git::remove_worktree(&cmp.repo, &cmp.worktree);
    }
    /// 基準の再登録（再接続時・#49 adversarial）。表示中のみ送る。
    /// 接続なし・未開始・worktree 消失時は送らない。
    async fn reregister_compare(&mut self) {
        let Some((root, commit, usable)) = self.compare.as_ref().filter(|c| c.show).map(|c| {
            (
                c.worktree.to_string_lossy().into_owned(),
                c.base.clone(),
                git::worktree_usable(&c.worktree),
            )
        }) else {
            return;
        };
        if !usable {
            self.flash = Some("worktree が消えているため基準を再登録できません".into());
            return;
        }
        self.send(&Command::RegisterBaseRoot { root, commit }).await;
    }

    /// 起動時の死に基準 sweep（#49 adversarial）: daemon 側に残る登録のうち、
    /// worktree が消えているか所有者が死んでいるものを解除する（クラッシュ時の
    /// 残骸セッション回収）。push との競合を避けるため ServerInfo を最大 8 行
    /// まで探す。best-effort。
    async fn sweep_dead_base_roots(&mut self) {
        self.send(&Command::GetServerInfo).await;
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        for _ in 0..8 {
            let mut line = String::new();
            if conn.reader.read_line(&mut line).await.is_err() {
                return;
            }
            let Ok(ServerMessage::ServerInfo { base_roots, .. }) =
                serde_json::from_str::<ServerMessage>(&line)
            else {
                continue;
            };
            for b in base_roots {
                if git::should_unregister_dead_root(&b.root) {
                    self.send(&Command::UnregisterBaseRoot { root: b.root }).await;
                }
            }
            return;
        }
    }

    /// 再接続を試みる（バックオフ済み）。成功時は Hello + GetState + SetViewport。
    async fn try_reconnect(&mut self) {
        if self.conn.is_some() {
            return;
        }
        match self.connect().await {
            Ok(()) => {
                self.send_viewport().await;
                // #49 adversarial: daemon 再起動でレジストリが消えるため、
                // 比較表示中なら登録し直す（放置するとガードが効かない）。
                self.reregister_compare().await;
            }
            Err(e) => {
                self.flash = Some(format!("再接続に失敗しました ({e}) — リトライします"));
                self.retry_at = Some(Instant::now() + RECONNECT_BACKOFF);
            }
        }
    }
}

/// カーソル位置の単語を取り出す（リネームの `old` の既定値）。
/// 非単語文字の上・範囲外なら `None`。daemon 側が最初の識別子出現を解決する
/// ため、ここは単なる既定値（英数字 + `_` の走査）。
fn word_at_cursor(state: &StateSnapshot) -> Option<String> {
    let head = state
        .selection
        .get(state.primary_index)
        .map(|r| r.head)
        .unwrap_or(0);
    let chars: Vec<char> = state.text.chars().collect();
    if head >= chars.len() || !is_word_char(chars[head]) {
        return None;
    }
    let mut start = head;
    while start > 0 && is_word_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = head;
    while end < chars.len() && is_word_char(chars[end]) {
        end += 1;
    }
    Some(chars[start..end].iter().collect())
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// ソケットから 1 行読む（切断時は `Err`）。接続なしは永遠に待つ。
async fn read_sock_line(conn: &mut Option<Conn>) -> Result<String, ()> {
    match conn {
        Some(c) => {
            let mut buf = String::new();
            match c.reader.read_line(&mut buf).await {
                Ok(0) | Err(_) => Err(()),
                Ok(_) => Ok(buf),
            }
        }
        None => std::future::pending().await,
    }
}

/// スピナー tick 用の sleep（活動中だけ有効）。
async fn tick_sleep(active: bool) {
    if active {
        tokio::time::sleep(SPIN_INTERVAL).await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// 再接続バックオフ用の sleep（切断中だけ有効）。
async fn retry_sleep(disconnected: bool, wait: Option<Duration>) {
    if !disconnected {
        std::future::pending::<()>().await;
        return;
    }
    match wait {
        Some(d) => tokio::time::sleep(d).await,
        None => {}
    }
}

/// TUI を実行する。`files` は起動時に開くパス（複数可）。
pub async fn run(files: Vec<String>) -> std::io::Result<()> {
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    // 設定 + カラースキーム（起動時に 1 回）
    let config = config::load();
    let schemes_dir = config::schemes_dir();
    let (capability, no_color) = colors::detect_from_env();
    let scheme_name = config.colorscheme.as_deref().unwrap_or("iceberg-dark");
    let scheme = colors::resolve(scheme_name, &schemes_dir).unwrap_or_else(|| {
        eprintln!("warning: unknown colorscheme {scheme_name:?}, falling back to iceberg-dark");
        colors::default_scheme()
    });

    // 端末の準備（raw + 代替画面）。終了/パニック時に必ず復元する。
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
        }
    }
    let _guard = Guard;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let mut app = App::new(scheme, schemes_dir, capability, no_color);
    // 起動時: 死んだセッションの比較 worktree 残骸を掃除する（best-effort）。
    git::prune_stale_worktrees();
    // 起動: daemon へ接続（失敗は明示エラーで終了）
    app.connect().await.map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("daemon に接続できませんでした: {e}"),
        )
    })?;
    // 起動時ファイルを開く
    for file in &files {
        app.send(&Command::Open {
            path: conn::absolutize(file),
        })
        .await;
        // 応答を1行読んで状態に反映する（起動直後の空画面を避ける）
        if let Some(c) = app.conn.as_mut() {
            let mut line = String::new();
            if c.reader.read_line(&mut line).await.is_ok() {
                app.on_socket_line(line).await;
            } else {
                app.disconnect("daemon との接続が切れました — 再接続します");
                break;
            }
        }
    }
    // 端末サイズを記録して SetViewport を送る
    // 起動時: 死んだセッションの基準登録を掃除する（#49 adversarial）。
    app.sweep_dead_base_roots().await;
    if let Ok(size) = terminal.size() {
        app.width = size.width;
        app.height = size.height;
        app.body_h = (size.height as usize).saturating_sub(1);
        app.send_viewport().await;
    }

    let mut key_stream = EventStream::new();
    loop {
        terminal.draw(|f| render::render(f, &mut app))?;
        // 描画後に高さが変わっていれば SetViewport を送る
        app.send_viewport().await;
        if app.quit {
            break;
        }
        let want_tick = app.conn.is_some() && !app.snapshot.activities.is_empty();
        let now = Instant::now();
        let (disconnected, wait) = match (app.conn.is_none(), app.retry_at) {
            (true, Some(t)) => (true, Some(t.saturating_duration_since(now))),
            (true, None) => (true, None),
            (false, _) => (false, None),
        };
        tokio::select! {
            biased;
            evt = key_stream.next() => {
                let Some(evt) = evt else { break };
                match evt {
                    Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        app.handle_key(key).await;
                    }
                    Ok(Event::Resize(w, h)) => {
                        app.width = w;
                        app.height = h;
                        app.body_h = (h as usize).saturating_sub(1);
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            res = read_sock_line(&mut app.conn) => {
                match res {
                    Ok(line) => app.on_socket_line(line).await,
                    Err(()) => app.disconnect("daemon との接続が切れました — 再接続します"),
                }
            }
            _ = tick_sleep(want_tick) => {
                app.tick += 1;
            }
            _ = retry_sleep(disconnected, wait) => {
                app.try_reconnect().await;
            }
        }
    }
    // 終了時: 基準登録の解除＋worktree 撤去（best-effort）。
    app.shutdown_compare().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_keys_open_overlays_in_normal() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        // dispatch のためだけの App（接続なしでもキー処理できる）
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = App::new(
                crate::colors::default_scheme(),
                PathBuf::from("/tmp"),
                ColorCapability::TrueColor,
                false,
            );
            assert_eq!(app.snapshot.mode, Mode::Normal);
            let key = KeyEvent::new(KeyCode::Char('T'), KeyModifiers::NONE);
            app.handle_key(key).await;
            assert_eq!(app.overlay, Overlay::Tree, "T でツリーが開く");

            // Esc で閉じる
            let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            app.handle_key(esc).await;
            assert_eq!(app.overlay, Overlay::None, "Esc で閉じる");

            // G/A/C
            let key = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE);
            app.handle_key(key).await;
            assert_eq!(app.overlay, Overlay::Diagnostics, "G で診断 view");
            app.handle_key(esc).await;
            let key = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE);
            app.handle_key(key).await;
            assert_eq!(app.overlay, Overlay::Activity, "A で活動履歴");
            app.handle_key(esc).await;
            let before = app.scheme.name.clone();
            let key = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE);
            app.handle_key(key).await;
            assert_ne!(app.scheme.name, before, "C で配色が切り替わる");

            // プロンプトを開くキー
            for (ch, check) in [
                (':', "command"),
                ('/', "search"),
                ('?', "search"),
                ('r', "replace"),
            ] {
                let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
                app.handle_key(key).await;
                assert!(app.prompt.is_some(), "{ch} でプロンプト ({check})");
                app.handle_key(esc).await;
                assert!(app.prompt.is_none(), "Esc でキャンセル");
            }

            // R: カーソル位置の単語を old にリネームプロンプト
            app.snapshot.text = "hello world".into();
            app.snapshot.selection = vec![mina_protocol::Range { anchor: 6, head: 6 }];
            let key = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE);
            app.handle_key(key).await;
            match &app.prompt {
                Some(Prompt::Rename { old, .. }) => assert_eq!(old, "world"),
                other => panic!("R でリネームプロンプト: {other:?}"),
            }
            app.handle_key(esc).await;
        });
    }

    #[test]
    fn command_line_parses() {
        assert_eq!(parse_command("w"), CommandLineAction::Save);
        assert_eq!(parse_command("q"), CommandLineAction::Quit);
        assert_eq!(parse_command("q!"), CommandLineAction::Quit);
        assert_eq!(parse_command("wq"), CommandLineAction::SaveThenQuit);
        assert_eq!(
            parse_command("colorscheme"),
            CommandLineAction::Colorscheme(None)
        );
        assert_eq!(
            parse_command("colorscheme iceberg-dark"),
            CommandLineAction::Colorscheme(Some("iceberg-dark".into()))
        );
        assert_eq!(
            parse_command("open foo.rs"),
            CommandLineAction::Open("foo.rs".into())
        );
        assert_eq!(
            parse_command("w foo"),
            CommandLineAction::Unknown("w foo".into())
        );
    }

    fn test_app() -> App {
        App::new(
            crate::colors::default_scheme(),
            PathBuf::from("/tmp"),
            ColorCapability::TrueColor,
            false,
        )
    }

    fn changed(path: &str, status: git::ChangeStatus) -> git::ChangedFile {
        git::ChangedFile {
            path: PathBuf::from(path),
            status,
        }
    }

    #[test]
    fn repin_without_compare_hints_d() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = test_app();
            let key = KeyEvent::new(KeyCode::Char('B'), KeyModifiers::NONE);
            app.handle_key(key).await;
            assert!(app.compare.is_none());
            assert!(app.flash.as_deref().unwrap_or("").contains('D'));
        });
    }

    #[test]
    fn tree_annotate_marks_files_and_dir_aggregates() {
        let mut tree = TreeState {
            root: PathBuf::from("/r"),
            expanded: HashSet::new(),
            selected: 0,
            entries: vec![
                TreeEntry {
                    path: PathBuf::from("/r/src"),
                    name: "src".into(),
                    is_dir: true,
                    depth: 0,
                    status: None,
                    subtree_changes: 0,
                },
                TreeEntry {
                    path: PathBuf::from("/r/a.rs"),
                    name: "a.rs".into(),
                    is_dir: false,
                    depth: 0,
                    status: None,
                    subtree_changes: 0,
                },
            ],
        };
        let changed = vec![changed("/r/a.rs", git::ChangeStatus::Modified)];
        tree.annotate(Some(&changed));
        assert_eq!(tree.entries[1].status, Some(git::ChangeStatus::Modified));
        // ルート直下の src/ 配下に変更はない。/r 自体の集約は entries にない。
        assert_eq!(tree.entries[0].subtree_changes, 0);
        // クリア
        tree.annotate(None);
        assert_eq!(tree.entries[1].status, None);
    }

    #[test]
    fn tree_annotate_dir_aggregate_counts_subtree() {
        let mut tree = TreeState {
            root: PathBuf::from("/r"),
            expanded: HashSet::new(),
            selected: 0,
            entries: vec![TreeEntry {
                path: PathBuf::from("/r/src"),
                name: "src".into(),
                is_dir: true,
                depth: 0,
                status: None,
                subtree_changes: 0,
            }],
        };
        let changed = vec![
            changed("/r/src/a.rs", git::ChangeStatus::Added),
            changed("/r/src/b.rs", git::ChangeStatus::Deleted),
        ];
        tree.annotate(Some(&changed));
        assert_eq!(tree.entries[0].subtree_changes, 2);
    }

    /// 比較 E2E（headless）: 実 git でピン留め→差分→TestBackend 描画まで通す。
    /// worktree を汚さない（canvas は worktree と独立に seed する）。
    /// クリーンなリポジトリでは `git stash create` は何も作らない。
    #[test]
    fn compare_e2e_pin_diff_render() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = test_app();
            // D でピン留め（実 git。失敗時は環境要因なので明示する）。
            let key = KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE);
            app.handle_key(key).await;
            assert!(app.compare.is_some(), "pin failed: {:?}", app.flash);
            let path = format!("{}/src/main.rs", env!("CARGO_MANIFEST_DIR"));
            let orig = include_str!("main.rs");

            // 追加行あり canvas → '+' マーカー行が出る。
            app.snapshot.text = format!("{orig}\n// e2e-marker-added");
            app.snapshot.path = Some(path.clone());
            app.snapshot.checksum = 111;
            let diff = app.compare_diff_for_render();
            assert!(diff.is_some(), "flash: {:?}", app.flash);
            let diff = diff.unwrap();
            assert_eq!(*diff.kinds.last().unwrap(), git::RowKind::Added);
            draw_to_test_backend(&mut app, |rows| {
                assert!(
                    rows.iter().any(|r| r.starts_with('+')),
                    "added marker missing: {rows:?}"
                );
            });

            // 削除あり canvas → '-' の gap 行に旧テキストが出る（3行ブロック）。
            let mut gone: Vec<&str> = orig.split('\n').collect();
            assert!(gone.len() > 8);
            let removed: Vec<&str> = gone.drain(5..8).collect();
            app.snapshot.text = gone.join("\n");
            app.snapshot.checksum = 222;
            let diff = app.compare_diff_for_render();
            assert!(diff.is_some(), "flash: {:?}", app.flash);
            {
                let d = diff.unwrap();
                assert_eq!(d.gaps.len(), 1);
                assert_eq!(d.gaps[0].lines.len(), 3);
            }
            draw_to_test_backend(&mut app, |rows| {
                let nospace: Vec<String> =
                    rows.iter().map(|r| r.replace(' ', "")).collect();
                let target = removed[0].replace(' ', "");
                assert!(
                    nospace
                        .iter()
                        .any(|r| r.starts_with('-') && r.contains(&target)),
                    "deleted gap missing: {rows:?}"
                );
            });

            // Tab で gapレビュー開始 → j/k/h/l 移動 → Enter(jump送信) → Esc 終了。
            // worktree は D 時に実作成されている（後で撤去する）。
            let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
            app.handle_key(tab).await;
            assert!(app.gap_review.is_some(), "gap mode: {:?}", app.flash);
            let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
            app.handle_key(j).await;
            app.handle_key(j).await;
            assert_eq!(app.gap_review.unwrap().line_idx, 2);
            let k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
            app.handle_key(k).await;
            assert_eq!(app.gap_review.unwrap().line_idx, 1);
            // 編集キーはブロックされる（daemon 選択は不変）。
            let before = app.snapshot.text.clone();
            let x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
            app.handle_key(x).await;
            assert!(app.gap_review.is_some(), "gap mode 維持");
            assert_eq!(app.snapshot.text, before, "daemon 側は不変");
            assert!(
                app.flash.as_deref().unwrap_or("").contains("gapレビュー"),
                "block flash: {:?}",
                app.flash
            );
            // Enter は jump 送信（接続なしでも落ちない）。
            let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            app.handle_key(enter).await;
            assert!(app.gap_review.is_some());
            // Esc で終了。
            let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            app.handle_key(esc).await;
            assert!(app.gap_review.is_none());

            // 後始末: worktree 撤去（除去パス自体の検証も兼ねる）。
            let cmp = app.compare.as_ref().expect("compare");
            let wt = cmp.worktree.clone();
            assert!(wt.exists(), "D で worktree 実作成");
            git::remove_worktree(&cmp.repo, &wt);
            assert!(!wt.exists());
        });
    }

    /// #49 adversarial: git リポジトリ外では D は失敗し、状態を作らない。
    #[test]
    fn toggle_outside_git_repo_fails_clean() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let dir = std::env::temp_dir().join(format!("mina-test-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = test_app();
            app.tree = TreeState::new(dir.clone());
            let key = KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE);
            app.handle_key(key).await;
            assert!(app.compare.is_none());
            assert!(
                app.flash.as_deref().unwrap_or("").contains("比較を開始できません"),
                "{:?}",
                app.flash
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #49 adversarial: worktree を作れない状態での再表示は注釈のみで続行する。
    #[test]
    fn toggle_on_with_broken_worktree_stays_graceful() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = test_app();
            app.compare = Some(CompareState {
                base: "abc".into(),
                repo: PathBuf::from("/definitely/not/a-repo-49"),
                worktree: PathBuf::from("/definitely/not/a-repo-49-wt"),
                show: false,
                files: Vec::new(),
                files_gen: 0,
                base_texts: HashMap::new(),
                diffs: HashMap::new(),
                last_err: None,
            });
            let key = KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE);
            app.handle_key(key).await;
            let cmp = app.compare.as_ref().expect("状態は残る");
            assert!(cmp.show, "表示は続行する");
            assert!(
                app.flash.as_deref().unwrap_or("").contains("注釈のみ"),
                "{:?}",
                app.flash
            );
        });
    }

    /// #49 adversarial: 再登録は接続なしでも安全（状態不変・panic なし）。
    #[test]
    fn reregister_without_conn_is_noop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = test_app();
            app.compare = Some(CompareState {
                base: "abc".into(),
                repo: PathBuf::from("/r"),
                worktree: PathBuf::from("/tmp/wt"),
                show: true,
                files: Vec::new(),
                diffs: HashMap::new(),
                base_texts: HashMap::new(),
                files_gen: 0,
                last_err: None,
            });
            app.reregister_compare().await;
            assert!(app.compare.as_ref().unwrap().show);
            // worktree がないため再登録できず、その旨が報知される（状態は維持）。
            assert!(
                app.flash.as_deref().unwrap_or("").contains("worktree"),
                "{:?}",
                app.flash
            );
            app.flash = None;
            // 非表示では何も送らない（flash なし）。
            app.compare.as_mut().unwrap().show = false;
            app.reregister_compare().await;
            assert!(app.flash.is_none());
        });
    }

    /// #49 adversarial: 起動時 sweep が死んだ基準登録を解除する（socketpair で hermetic）。
    #[test]
    fn sweep_dead_base_roots_unregisters() {
        use tokio::io::AsyncWriteExt;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = test_app();
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let dead = child.id();
            child.wait().unwrap();
            let dead_root = format!("/tmp/mina-base-{dead}-abc");
            let live_root = format!(
                "/tmp/mina-base-{}-abc",
                std::process::id()
            );
            let info = mina_protocol::ServerMessage::ServerInfo {
                generation: "x".into(),
                daemon_build_ts: 0,
                metrics: mina_protocol::ServerMetrics::default(),
                base_roots: vec![
                    mina_protocol::BaseRootInfo {
                        root: dead_root.clone(),
                        commit: "abc".into(),
                    },
                    mina_protocol::BaseRootInfo {
                        root: live_root,
                        commit: "abc".into(),
                    },
                ],
            };
            let (a, mut b) = tokio::net::UnixStream::pair().unwrap();
            let (ar, aw) = a.into_split();
            app.conn = Some(Conn {
                writer: aw,
                reader: BufReader::new(ar),
            });
            let mut line = serde_json::to_string(&info).unwrap();
            line.push('\n');
            b.write_all(line.as_bytes()).await.unwrap();
            app.sweep_dead_base_roots().await;
            // peer 側の1行目は sweep 自身の GetServerInfo 要求。読み飛ばす。
            let mut br = BufReader::new(b);
            let mut out = String::new();
            br.read_line(&mut out).await.unwrap();
            assert!(out.contains("GetServerInfo"), "{out}");
            // 死 root の Unregister が届く。生存分は送らない。
            out.clear();
            br.read_line(&mut out).await.unwrap();
            let cmd: mina_protocol::Command = serde_json::from_str(out.trim()).unwrap();
            match cmd {
                mina_protocol::Command::UnregisterBaseRoot { root } => {
                    assert_eq!(root, dead_root)
                }
                other => panic!("Unregister のはず: {other:?}"),
            }
            let second = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                br.read_line(&mut out),
            )
            .await;
            assert!(second.is_err(), "生存分は送らない");
        });
    }

    #[test]
    fn base_display_path_maps_worktree() {
        let mut app = test_app();
        app.compare = Some(CompareState {
            base: "abcdef123456".into(),
            repo: PathBuf::from("/r"),
            worktree: PathBuf::from("/tmp/wt"),
            show: true,
            files: Vec::new(),
            files_gen: 0,
            base_texts: HashMap::new(),
            diffs: HashMap::new(),
            last_err: None,
        });
        assert_eq!(
            app.base_display_path("/tmp/wt/src/a.rs"),
            "src/a.rs @abcdef1"
        );
        assert_eq!(app.base_display_path("/other/b.rs"), "/other/b.rs");
    }

    /// TestBackend に描画して各行の文字列を渡す。
    fn draw_to_test_backend(app: &mut App, check: impl FnOnce(Vec<String>)) {
        use ratatui::{backend::TestBackend, Terminal};
        let backend = TestBackend::new(100, 48);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::render::render(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut rows = Vec::new();
        for y in 0..48 {
            let line: String = (0..100).map(|x| buf[(x, y)].symbol()).collect();
            rows.push(line.trim_end().to_string());
        }
        check(rows);
    }

    #[test]
    fn tree_jump_moves_to_marked_files() {
        let mut app = test_app();
        app.tree.entries = vec![
            TreeEntry {
                path: PathBuf::from("/r/a.rs"),
                name: "a.rs".into(),
                is_dir: false,
                depth: 0,
                status: None,
                subtree_changes: 0,
            },
            TreeEntry {
                path: PathBuf::from("/r/b.rs"),
                name: "b.rs".into(),
                is_dir: false,
                depth: 0,
                status: Some(git::ChangeStatus::Modified),
                subtree_changes: 0,
            },
            TreeEntry {
                path: PathBuf::from("/r/c.rs"),
                name: "c.rs".into(),
                is_dir: false,
                depth: 0,
                status: Some(git::ChangeStatus::Added),
                subtree_changes: 0,
            },
        ];
        app.compare = Some(CompareState {
            base: "abc".into(),
            repo: PathBuf::from("/r"),
            worktree: PathBuf::from("/tmp/mina-base-test"),
            show: true,
            files: Vec::new(),
            files_gen: 0,
            base_texts: HashMap::new(),
            diffs: HashMap::new(),
            last_err: None,
        });
        app.tree.selected = 0;
        app.jump_changed(1);
        assert_eq!(app.tree.selected, 1);
        app.jump_changed(1);
        assert_eq!(app.tree.selected, 2);
        // 末尾で循環する
        app.jump_changed(1);
        assert_eq!(app.tree.selected, 1);
        app.jump_changed(-1);
        assert_eq!(app.tree.selected, 2);
    }
}
