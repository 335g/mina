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
use tokio::time::timeout;

use crate::colors::{self, ColorCapability, Colorscheme};
use crate::config;
use crate::git;
use crate::keymap::{Keymaps, Resolution, normalize as normalize_key};
use crate::render;

/// スピナーの tick 間隔。
const SPIN_INTERVAL: Duration = Duration::from_millis(80);
/// 接続断後の再接続バックオフ。
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
/// 起動時 Open 応答の待ち上限。daemon が LSP 初期化（rust-analyzer の
/// spawn + initialize）に時間をかけても UI を固めないための安全弁。
/// 超過した応答はメインループの read_sock_line が拾って反映する。
const OPEN_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
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
    /// コミット選択（#51・Mode 2）。o=旧側・n=新側に設定する。
    Commits,
    /// 基準全文ブラウズ（#52・a2）。フォーカスを動かさない読取り専用表示。
    BaseBrowse,
    /// キーバインドヘルプ（`?` で開く。クライアントローカル。Esc/? で閉じる）。
    Help,
}

/// コマンドライン系プロンプトの種類（Helix 流の `:` / `/` 等）。
/// クライアントローカル — daemon には確定時だけコマンドを送る（検索はライブで送る）。
#[derive(Debug)]
pub(crate) enum Prompt {
    /// `:` コマンドライン。
    Command(String),
    /// `/` の検索プロンプト。キー入力のたびに [`Command::Search`] を送る
    /// （ライブ検索）。後方検索は `?` をヘルプに譲ったため無い（N で後方探索）。
    Search(String),
    /// `r` 置換: 次の文字キーで選択/カーソル文字を置換する。
    Replace,
    /// `ms`/`mr`/`md` 囲み文字（surround）。`m` リーダーの後続キー
    /// （`s`/`r`/`d` + 文字）を buf に溜め、揃ったら daemon へ送る。
    Surround(String),
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
            Prompt::Search(_) => '/',
            Prompt::Replace => 'r',
            Prompt::Surround(_) => 'm',
            Prompt::Rename { .. } => 'R',
            Prompt::ReviewComment { .. } => '"',
        }
    }

    pub(crate) fn buf(&self) -> &str {
        match self {
            Prompt::Command(b) | Prompt::Search(b) | Prompt::Rename { buf: b, .. } | Prompt::ReviewComment { buf: b, .. } | Prompt::Surround(b) => b,
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

/// 比較閲覧の状態（#49 Mode 1・#51 Mode 2）。クライアントローカル。
/// `base`/`worktree` は旧側（Mode 1 の基準と同義）。`new_*` が Some のとき
/// Mode 2（過去2コミット・両側読取り専用）で、canvas は新側テキストになる。
pub(crate) struct CompareState {
    /// 旧側コミット ID（Mode 1 の基準）。
    base: String,
    /// リポジトリルート（絶対パス）。
    repo: PathBuf,
    /// 旧側コミットの実体（固定パス worktree）。
    worktree: PathBuf,
    /// 新側コミット ID（Mode 2 のみ Some）。
    new_base: Option<String>,
    /// 新側コミットの実体（Mode 2 のみ Some）。
    new_worktree: Option<PathBuf>,
    /// 注釈表示の ON/OFF（D で切替）。
    show: bool,
    /// 変更一覧（絶対パス）。files_gen 世代のもの。
    files: Vec<git::ChangedFile>,
    /// 一覧を作った snapshot.generation。
    files_gen: u64,
    /// 旧側テキストのキャッシュ（基準不変なのでピン中は有効）。
    base_texts: HashMap<String, Option<String>>,
    /// 新側テキストのキャッシュ（Mode 2 の canvas。ピン中は有効）。
    new_texts: HashMap<String, Option<String>>,
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
            new_base: None,
            new_worktree: None,
            show: true,
            files: Vec::new(),
            files_gen: u64::MAX,
            base_texts: HashMap::new(),
            new_texts: HashMap::new(),
            diffs: HashMap::new(),
            last_err: None,
        })
    }

    /// Mode 2 でピン留め（#51）。旧=HEAD~1・新=HEAD の即ピン。両側の
    /// worktree（-old/-new）を用意し、一覧は 2 コミット間の差分にする。
    /// 未追跡・Dirty の union はしない（両側コミット済みのため）。
    fn pin_mode2(repo: PathBuf) -> Result<Self, git::GitError> {
        let old = git::run(&repo, &["rev-parse", "HEAD~1"])?;
        let new = git::run(&repo, &["rev-parse", "HEAD"])?;
        Self::pin_mode2_pair(repo, old.trim(), new.trim())
    }

    /// Mode 2 を明示ペアでピン留め（#51・ピッカー用）。失敗時は作らない。
    fn pin_mode2_pair(repo: PathBuf, old: &str, new: &str) -> Result<Self, git::GitError> {
        if old == new {
            return Err(git::GitError("旧側と新側が同じコミットです".into()));
        }
        git::verify_object(&repo, old)?;
        git::verify_object(&repo, new)?;
        let old_wt = git::worktree_path_tagged(&repo, "old");
        let new_wt = git::worktree_path_tagged(&repo, "new");
        // 2 側の worktree を用意する（既存は捨てて作り直す・pin と同型）。
        git::ensure_worktree_tagged(&repo, old, &old_wt)?;
        if let Err(e) = git::ensure_worktree_tagged(&repo, new, &new_wt) {
            git::remove_worktree(&repo, &old_wt);
            return Err(e);
        }
        let files = git::changed_files_between(&repo, old, new).unwrap_or_default();
        Ok(Self {
            base: old.to_string(),
            repo,
            worktree: old_wt,
            new_base: Some(new.to_string()),
            new_worktree: Some(new_wt),
            show: true,
            files,
            // ピン固定のため再計算しない（ensure_files は Mode 2 を素通し）。
            files_gen: 0,
            base_texts: HashMap::new(),
            new_texts: HashMap::new(),
            diffs: HashMap::new(),
            last_err: None,
        })
    }

    /// Mode 2 か（新側ピン留めあり・両側読取り専用）。
    pub(crate) fn is_mode2(&self) -> bool {
        self.new_base.is_some()
    }

    pub(crate) fn short(&self) -> &str {
        self.base.get(..7).unwrap_or(&self.base)
    }

    /// 新側の短縮ハッシュ（Mode 2 のみ Some）。
    pub(crate) fn short_new(&self) -> Option<&str> {
        self.new_base.as_ref().map(|n| n.get(..7).unwrap_or(n))
    }

    /// ステータス行の比較マーク（Mode 1: ◈基準 / Mode 2: ◈旧..新）。
    pub(crate) fn label(&self) -> String {
        match self.short_new() {
            Some(n) => format!("{}..{}", self.short(), n),
            None => self.short().to_string(),
        }
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
    /// （未保存編集が一覧から漏れないため）。Mode 2 はピン固定のため
    /// 何もしない（一覧は pin/rebuild 時に作る）。
    fn ensure_files(&mut self, snap: &StateSnapshot) {
        if self.is_mode2() {
            return;
        }
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
    /// Mode 2 は旧側 vs 新側（canvas は新側テキスト）。
    fn diff_for(&mut self, snap: &StateSnapshot, flash: &mut Option<String>) -> Option<git::FileDiff> {
        if !self.show {
            return None;
        }
        let path = snap.path.as_deref()?;
        let rel = Path::new(path).strip_prefix(&self.repo).ok()?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        if self.is_mode2() {
            return self.diff_for_mode2(path, &rel, flash);
        }
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

    /// Mode 2 の差分配列（#51）。旧側テキスト vs 新側テキストで、canvas は
    /// 新側。旧欠落→全 Added、新欠落→全 Gap＋空 canvas。ピン固定のため
    /// パス単位でキャッシュする（rebuild 時に捨てる）。
    fn diff_for_mode2(
        &mut self,
        path: &str,
        rel: &str,
        flash: &mut Option<String>,
    ) -> Option<git::FileDiff> {
        if let Some(cached) = self.diffs.get(path) {
            return Some(cached.diff.clone());
        }
        let new_commit = self.new_base.clone()?;
        let old_commit = self.base.clone();
        let repo = self.repo.clone();
        let old_text = match self.base_texts.get(path) {
            Some(cached) => cached.clone(),
            None => match git::base_text(&repo, &old_commit, rel) {
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
        let new_text = match self.new_texts.get(path) {
            Some(cached) => cached.clone(),
            None => match git::base_text(&repo, &new_commit, rel) {
                Err(e) => {
                    self.note_err(format!("{e}"), flash);
                    return None;
                }
                Ok(text) => {
                    self.new_texts.insert(path.to_string(), text.clone());
                    text
                }
            },
        };
        let diff = match (old_text, new_text) {
            (None, None) => return None,
            (None, Some(n)) => git::FileDiff::all_added(n.split('\n').count()),
            (Some(o), None) => git::FileDiff {
                kinds: Vec::new(),
                gaps: vec![git::Gap {
                    at: 0,
                    old_start: 1,
                    lines: o.split('\n').map(str::to_string).collect(),
                }],
            },
            (Some(o), Some(n)) => match git::diff_texts(&o, &n) {
                Ok(d) => d,
                Err(e) => {
                    self.note_err(format!("{e}"), flash);
                    return None;
                }
            },
        };
        // canvas 不変（ピン固定）のため checksum は行数で代用する。
        let checksum = diff.kinds.len() as u64;
        self.diffs.insert(
            path.to_string(),
            CachedDiff {
                canvas_checksum: checksum,
                diff: diff.clone(),
            },
        );
        Some(diff)
    }

    /// Mode 2 の canvas テキスト（新側ファイル内容・キャッシュ付き）。
    /// render が snap.text の代わりに描く。新側欠落時は空文字（全 Gap 表示）。
    /// 対象外・取得失敗時は None。
    pub(crate) fn mode2_canvas_text(&mut self, snap: &StateSnapshot) -> Option<String> {
        let new_commit = self.new_base.clone()?;
        let path = snap.path.as_deref()?;
        let rel = Path::new(path).strip_prefix(&self.repo).ok()?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        if let Some(cached) = self.new_texts.get(path) {
            return cached.clone();
        }
        match git::base_text(&self.repo, &new_commit, &rel) {
            Err(_) => None,
            Ok(text) => {
                self.new_texts.insert(path.to_string(), text.clone());
                text
            }
        }
    }

    /// Mode 2 の canvas 行数（v2 カーソルのクランプ用）。新側欠落時は
    /// 空 canvas の 1 行。対象外のときは None。
    pub(crate) fn mode2_canvas_len(&mut self, snap: &StateSnapshot) -> Option<usize> {
        if self.new_base.is_none() {
            return None;
        }
        let path = snap.path.as_deref()?;
        Path::new(path).strip_prefix(&self.repo).ok()?;
        Some(
            self.mode2_canvas_text(snap)
                .map(|t| t.split('\n').count())
                .unwrap_or(1),
        )
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

/// エージェント起動の行き先（#54）。
pub(crate) enum AgentLaunch {
    /// `tmux split-window` の argv（spawn 用）。
    Tmux(Vec<String>),
    /// tmux 外: 手動実行用の文面（flash 表示）。
    Manual(String),
}

/// 基準全文ブラウズの状態（#52・a2）。テキストは開いた時点の snapshot。
#[derive(Clone, Default)]
pub(crate) struct BaseBrowse {
    pub(crate) title: String,
    pub(crate) lines: Vec<String>,
    pub(crate) line: usize,
    pub(crate) first: usize,
}
/// 自前カーソル行とビューポート先頭（どちらも canvas 基準・0-origin）。
/// Mode 2 の表示位置（render へ渡す値オブジェクト・#51）。
#[derive(Clone, Copy)]
pub(crate) struct V2View {
    pub(crate) line: usize,
    pub(crate) first: usize,
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
    /// Mode 2 の自前カーソル（canvas 行・0-origin）。デーモン選択と無関係。
    pub(crate) v2_line: usize,
    /// Mode 2 の自前ビューポート（canvas 先頭行）。
    pub(crate) v2_first: usize,
    /// v2 カーソルの対象パス（切替わりで 0 に戻す）。
    pub(crate) v2_path: Option<String>,
    /// コミット一覧（#51・ピッカー用）。要素は (フルハッシュ, subject)。
    pub(crate) commits: Vec<(String, String)>,
    /// コミット一覧の選択位置。
    pub(crate) commit_sel: usize,
    /// 基準全文ブラウズ（#52・a2）。None 以外のときだけ描く。
    pub(crate) base_browse: Option<BaseBrowse>,
    /// エージェント起動コマンド（#54・config `agent_command`）。
    /// `E` で `minas review | <agent_command>` を tmux 隣ペインに投げる。
    pub(crate) agent_command: Option<String>,
    /// daemon 保持のレビューコメント一覧のキャッシュ（#50）。マーカー表示と
    /// 編集 prefill 用。List 応答で更新し、比較終了・再ピンで捨てる。
    pub(crate) review_list: Vec<ReviewCommentView>,
    pub(crate) diag_filter: Severity,
    pub(crate) diag_index: usize,
    pub(crate) diag_key: Option<(usize, String)>,
    pub(crate) diag_focus: DiagFocus,
    pub(crate) activity_filter: ActivityFilter,
    pub(crate) activity_scroll: usize,
    /// ヘルプオーバーレイのスクロール位置（0-origin）。
    pub(crate) help_scroll: usize,
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
    /// 起動時 Open の応答待ち表示（タイムアウト時はメインループが拾うまで残る。
    /// スナップショットが届いたら on_socket_line で消える）。
    pub(crate) pending_open: Option<String>,
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
            v2_line: 0,
            v2_first: 0,
            v2_path: None,
            commits: Vec::new(),
            commit_sel: 0,
            base_browse: None,
            agent_command: None,
            review_list: Vec::new(),
            diag_filter: Severity::Error,
            diag_index: 0,
            diag_key: None,
            diag_focus: DiagFocus::View,
            activity_filter: ActivityFilter::All,
            activity_scroll: 0,
            help_scroll: 0,
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
            pending_open: None,
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
        self.pending_open = None; // 未応答の起動ファイルは再接続後に追わない
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
                // 起動時 Open の遅延応答（全文スナップショット）が届いた: 待ち表示を消す
                self.pending_open = None;
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
        // 大文字キーは Shift 付きで届く（keymap と同じ正規化）。これをしないと
        // T/G/A/C 等の `modifiers.is_empty()` ガードに弾かれて開かない。
        let key = normalize_key(key);
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

    /// オーバーレイ系ホットキー（T/G/A/C）。処理したら true。Mode 2 からも
    /// live 文書側の表示として委譲される（#51・読取り専用に触れないもののみ）。
    async fn overlay_hotkey(&mut self, code: KeyCode) -> bool {
        use KeyCode::*;
        match code {
            Char('T') => {
                self.tree.refresh();
                self.tree.select_path(self.snapshot.path.as_deref());
                self.annotate_tree();
                self.overlay = Overlay::Tree;
                true
            }
            Char('G') => {
                self.reconcile_diag();
                self.overlay = Overlay::Diagnostics;
                self.diag_focus = DiagFocus::View;
                self.sync_cursor_to_diag().await;
                true
            }
            Char('A') => {
                self.activity_scroll = usize::MAX; // 末尾（最新）から
                self.overlay = Overlay::Activity;
                true
            }
            Char('C') => {
                self.cycle_colorscheme();
                true
            }
            _ => false,
        }
    }

    /// エディタ（オーバーレイなし・プロンプトなし）のキー処理。
    async fn handle_editor_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        // Ctrl-C は即終了（daemon は常駐なので破棄はない）
        if key.code == Char('c') && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        // Mode 2 表示中は読取り専用ナビ（#51）。編集系キー（:・/・挿入・
        // 移動の daemon 送信）はここで遮断し、後段へ届けない。
        if self.mode2_active() {
            self.handle_mode2_key(key).await;
            return;
        }
        let mode = self.snapshot.mode;
        // Normal のオーバーレイキー（T/G/A/C — G は診断 view。Goto 末尾は `g e`）
        if mode == Mode::Normal && key.modifiers.is_empty() {
            if self.overlay_hotkey(key.code).await {
                return;
            }
            match key.code {
                // D: 比較表示の切替（初回はピン留め）。B: 基準の更新。
                // M: 過去2コミット比較（Mode 2）の切替（#51）。
                // Normal 先行キー（T/G/A/C と同列 — keymap より優先）。
                Char('D') => {
                    self.pending.clear();
                    self.toggle_compare().await;
                    return;
                }
                Char('B') => {
                    self.pending.clear();
                    if self.compare.as_ref().is_some_and(|c| c.is_mode2()) {
                        self.flash = Some("コミット選択は次の更新で対応".into());
                    } else {
                        self.repin_compare().await;
                    }
                    return;
                }
            Char('M') => {
                    self.pending.clear();
                    self.toggle_mode2().await;
                    return;
                }
                // P: 基準全文ブラウズ（注目文書の旧側・フォーカス不動）。
                Char('P') => {
                    self.pending.clear();
                    self.open_base_browse();
                    return;
                }
                // E: レビューコメントをエージェントに投げる（#54）。
                Char('E') => {
                    self.pending.clear();
                    self.launch_agent().await;
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
                Char('/') => {
                    self.pending.clear();
                    self.prompt = Some(Prompt::Search(String::new()));
                    return;
                }
                // ヘルプ（`?`）は後方検索を譲って貰った。後方探索は n/N で行う。
                Char('?') => {
                    self.pending.clear();
                    self.overlay = Overlay::Help;
                    self.help_scroll = 0;
                    return;
                }
                // `m` リーダー: Helix の surround（ms = 囲む / md = 外す /
                // mr = 置換）。r/R と同じくクライアント側で後続キーを読む。
                Char('m') => {
                    self.pending.clear();
                    self.prompt = Some(Prompt::Surround(String::new()));
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
            Overlay::Commits => {
                // #51: コミットピッカー（o=旧側・n=新側に設定）。
                match key.code {
                    Esc => self.overlay = Overlay::None,
                    Down | Char('j') if key.modifiers.is_empty() => {
                        if !self.commits.is_empty() {
                            self.commit_sel = (self.commit_sel + 1) % self.commits.len();
                        }
                    }
                    Up | Char('k') if key.modifiers.is_empty() => {
                        if !self.commits.is_empty() {
                            self.commit_sel =
                                (self.commit_sel + self.commits.len() - 1) % self.commits.len();
                        }
                    }
                    Char('o') if key.modifiers.is_empty() => {
                        if let Some((hash, _)) = self.commits.get(self.commit_sel).cloned() {
                            self.set_mode2_side(true, hash).await;
                        }
                    }
                    Char('n') if key.modifiers.is_empty() => {
                        if let Some((hash, _)) = self.commits.get(self.commit_sel).cloned() {
                            self.set_mode2_side(false, hash).await;
                        }
                    }
                    _ => {}
                }
            }
            Overlay::BaseBrowse => {
                // #52・a2: 基準全文ブラウズ（j/k 移動・Esc/P で復帰）。
                match key.code {
                    Esc => {
                        self.overlay = Overlay::None;
                        self.base_browse = None;
                    }
                    Char('P') if key.modifiers.is_empty() => {
                        self.overlay = Overlay::None;
                        self.base_browse = None;
                    }
                    Down | Char('j') if key.modifiers.is_empty() => self.move_browse(1),
                    Up | Char('k') if key.modifiers.is_empty() => self.move_browse(-1),
                    _ => {}
                }
            }
            Overlay::Help => {
                // 1頁 = ヘルプ本文の内側高さ（popup 高 = body_h/2、枠2 + フッター1）
                let page = (self.body_h / 2).saturating_sub(3).max(1);
                match key.code {
                    Esc | Char('?') => self.overlay = Overlay::None,
                    Down | Char('j') if key.modifiers.is_empty() => self.help_scroll += 1,
                    Up | Char('k') if key.modifiers.is_empty() => {
                        self.help_scroll = self.help_scroll.saturating_sub(1);
                    }
                    Char('f') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                        self.help_scroll += page;
                    }
                    Char('b') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                        self.help_scroll = self.help_scroll.saturating_sub(page);
                    }
                    _ => {}
                }
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
            Prompt::Search(mut buf) => {
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
                            direction: Direction::Forward,
                        })
                        .await;
                    }
                    self.prompt = Some(Prompt::Search(buf));
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
            Prompt::Surround(mut buf) => {
                // m リーダー: s/r/d の後に文字を足して、揃ったら送信。バッファは
                // `s`/`d` + 1文字、`r` + 2文字で確定。Esc/C-c でキャンセル。
                let keep = match key.code {
                    Char(c)
                        if key.modifiers.is_empty()
                            || key.modifiers == crossterm::event::KeyModifiers::SHIFT =>
                    {
                        buf.push(c);
                        match surround_command(&buf) {
                            Some(cmd) => {
                                self.send(&cmd).await;
                                false
                            }
                            None if surround_pending(&buf) => true,
                            _ => {
                                self.flash = Some("surround: s/r/d + 文字".into());
                                false
                            }
                        }
                    }
                    Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                        false
                    }
                    Esc => false,
                    _ => true,
                };
                if keep {
                    self.prompt = Some(Prompt::Surround(buf));
                }
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
        // #51: Mode 2 は単一 base スキーマのためコメント不可。
        if cmp.is_mode2() {
            self.flash = Some("過去比較は読取り専用です（コメントは Mode 1 で）".into());
            return;
        }
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
        // #51: Mode 2 の gap からもコメントは付けない（単一 base スキーマ）。
        if self.compare.as_ref().is_some_and(|c| c.is_mode2()) {
            self.flash = Some("過去比較は読取り専用です（コメントは Mode 1 で）".into());
            return;
        }
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
            let (show_now, root, commit, repo) = {
                let cmp = self.compare.as_mut().expect("is_some で確認済み");
                cmp.show = !cmp.show;
                (
                    cmp.show,
                    cmp.worktree.to_string_lossy().into_owned(),
                    cmp.base.clone(),
                    cmp.repo.to_string_lossy().into_owned(),
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
                    self.send(&Command::RegisterBaseRoot {
                        root,
                        commit,
                        repo: Some(repo),
                    })
                    .await;
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
            repo: Some(cmp.repo.to_string_lossy().into_owned()),
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
            repo: Some(fresh.repo.to_string_lossy().into_owned()),
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
    /// #51: 新側 worktree 配下は新側短縮ハッシュで表示する。
    pub(crate) fn base_display_path(&self, path: &str) -> String {
        if let Some(mapped) = self.compare.as_ref().and_then(|cmp| {
            Path::new(path).strip_prefix(&cmp.worktree).ok().map(|rel| {
                format!("{} @{}", rel.display(), cmp.short())
            })
        }) {
            return mapped;
        }
        if let Some(mapped) = self.compare.as_ref().and_then(|cmp| {
            let new_wt = cmp.new_worktree.as_ref()?;
            let short = cmp.short_new()?;
            Path::new(path).strip_prefix(new_wt).ok().map(|rel| {
                format!("{} @{}", rel.display(), short)
            })
        }) {
            return mapped;
        }
        path.to_string()
    }

    /// 比較状態の破棄（終了・切替時・best-effort）: 両側の登録解除＋
    /// worktree 撤去＋レビュー一覧の破棄。Mode 2 は 2 root 分行う。
    async fn drop_compare(&mut self) {
        self.review_list.clear();
        self.gap_review = None;
        let Some(cmp) = self.compare.take() else {
            return;
        };
        self.send(&Command::UnregisterBaseRoot {
            root: cmp.worktree.to_string_lossy().into_owned(),
        })
        .await;
        git::remove_worktree(&cmp.repo, &cmp.worktree);
        if let Some(new_wt) = cmp.new_worktree {
            self.send(&Command::UnregisterBaseRoot {
                root: new_wt.to_string_lossy().into_owned(),
            })
            .await;
            git::remove_worktree(&cmp.repo, &new_wt);
        }
    }

    /// 終了時の後始末（best-effort）: 登録解除＋worktree 撤去。
    async fn shutdown_compare(&mut self) {
        self.drop_compare().await;
    }

    /// M: Mode 2（過去2コミット比較）の切替（#51）。表示中なら終了し、
    /// 未開始なら旧=HEAD~1・新=HEAD で即ピンする。Mode 1 とは相互排他。
    async fn toggle_mode2(&mut self) {
        if self.compare.as_ref().is_some_and(|c| c.is_mode2()) {
            self.drop_compare().await;
            self.annotate_tree();
            self.flash = Some("過去比較を終了".into());
            return;
        }
        let root = self.tree.root.clone();
        let repo = match git::repo_root(&root) {
            Ok(r) => r,
            Err(e) => {
                self.flash = Some(format!("過去比較を開始できません: {e}"));
                return;
            }
        };
        let cmp = match CompareState::pin_mode2(repo) {
            Ok(c) => c,
            Err(e) => {
                self.flash = Some(format!("過去比較を開始できません: {e}"));
                return;
            }
        };
        // 先に旧状態（Mode 1 の場合あり）を捨ててから両側を登録する。
        self.drop_compare().await;
        let repo_str = cmp.repo.to_string_lossy().into_owned();
        self.send(&Command::RegisterBaseRoot {
            root: cmp.worktree.to_string_lossy().into_owned(),
            commit: cmp.base.clone(),
            repo: Some(repo_str.clone()),
        })
        .await;
        if let (Some(nb), Some(nwt)) = (cmp.new_base.clone(), cmp.new_worktree.clone()) {
            self.send(&Command::RegisterBaseRoot {
                root: nwt.to_string_lossy().into_owned(),
                commit: nb,
                repo: Some(repo_str),
            })
            .await;
        }
        self.flash = Some(format!(
            "過去比較 {}（M:終了 B:コミット選択 Tab:削除レビュー）",
            cmp.label()
        ));
        self.compare = Some(cmp);
        self.annotate_tree();
    }

    /// Mode 2 表示中か（注釈ONのときのみ canvas が切り替わる）。
    pub(crate) fn mode2_active(&self) -> bool {
        self.compare
            .as_ref()
            .is_some_and(|c| c.is_mode2() && c.is_showing())
    }

    /// v2 カーソル位置の同期（パス切替で先頭へ・canvas 長でクランプ・追従）。
    /// render 前に呼ぶ。非 Mode 2 では None。
    pub(crate) fn v2_view(&mut self, height: usize) -> Option<V2View> {
        if !self.mode2_active() {
            return None;
        }
        let snap = self.snapshot.clone();
        if self.v2_path != snap.path {
            self.v2_path = snap.path.clone();
            self.v2_line = 0;
            self.v2_first = 0;
        }
        let len = self
            .compare
            .as_mut()?
            .mode2_canvas_len(&snap)
            .unwrap_or(1)
            .max(1);
        self.v2_line = self.v2_line.min(len - 1);
        if self.v2_line < self.v2_first {
            self.v2_first = self.v2_line;
        }
        let h = height.max(1);
        if self.v2_line >= self.v2_first + h {
            self.v2_first = self.v2_line - h + 1;
        }
        Some(V2View {
            line: self.v2_line,
            first: self.v2_first,
        })
    }

    /// v2 カーソル移動（j/k）。canvas 長でクランプする。
    fn v2_move(&mut self, delta: isize) {        let snap = self.snapshot.clone();
        let len = self
            .compare
            .as_mut()
            .and_then(|c| c.mode2_canvas_len(&snap))
            .unwrap_or(1)
            .max(1);
        self.v2_line = (self.v2_line as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    /// B（Mode 2）: コミットピッカーを開く（#51）。`git log -50` の
    /// フラット一覧。o=旧側・n=新側に設定、Esc で閉じる。
    fn open_commit_picker(&mut self) {
        let Some(repo) = self.compare.as_ref().filter(|c| c.is_mode2()).map(|c| c.repo.clone())
        else {
            return;
        };
        match git::commit_list(&repo, 50) {
            Err(e) => {
                self.flash = Some(format!("コミット一覧を取得できません: {e}"));
            }
            Ok(commits) => {
                if commits.is_empty() {
                    self.flash = Some("コミットがありません".into());
                    return;
                }
                self.commit_sel = 0;
                self.commits = commits;
                self.overlay = Overlay::Commits;
            }
        }
    }

    /// ピッカーで旧側/新側を差し替える（#51）。該当側の worktree を作り直し、
    /// 再登録してキャッシュを捨てる。old==new は拒否する。
    async fn set_mode2_side(&mut self, old_side: bool, commit: String) {
        let Some((repo, cur_old, cur_new)) = self
            .compare
            .as_ref()
            .filter(|c| c.is_mode2())
            .map(|c| (c.repo.clone(), c.base.clone(), c.new_base.clone()))
        else {
            return;
        };
        let other = if old_side { cur_new } else { Some(cur_old) };
        if Some(commit.clone()) == other {
            self.flash = Some("旧側と新側が同じコミットです".into());
            return;
        }
        let tag = if old_side { "old" } else { "new" };
        let wt = git::worktree_path_tagged(&repo, tag);
        if let Err(e) = git::ensure_worktree_tagged(&repo, &commit, &wt) {
            self.flash = Some(format!("worktree を作り直せません: {e}"));
            return;
        }
        self.send(&Command::RegisterBaseRoot {
            root: wt.to_string_lossy().into_owned(),
            commit: commit.clone(),
            repo: Some(repo.to_string_lossy().into_owned()),
        })
        .await;
        let short = commit.get(..7).unwrap_or(&commit).to_string();
        let (old_id, new_id) = if old_side {
            (commit.clone(), other.clone().unwrap_or_default())
        } else {
            (other.clone().unwrap_or_default(), commit.clone())
        };
        let files = git::changed_files_between(&repo, &old_id, &new_id).unwrap_or_default();
        if let Some(cmp) = self.compare.as_mut().filter(|c| c.is_mode2()) {
            if old_side {
                cmp.base = commit;
                cmp.worktree = wt;
            } else {
                cmp.new_base = Some(commit);
                cmp.new_worktree = Some(wt);
            }
            cmp.base_texts.clear();
            cmp.new_texts.clear();
            cmp.diffs.clear();
            cmp.files = files;
        }
        self.v2_line = 0;
        self.v2_first = 0;
        self.annotate_tree();
        self.flash = Some(format!(
            "{}側を {short} に更新",
            if old_side { "旧" } else { "新" }
        ));
    }

    /// エージェント起動計画（#54・純粋部）。tmux の隣ペインに投げる。
    /// pipeline は `minas review | <agent>`（#50 の抽出コマンドを再利用）。
    /// argv 渡しのためクォート escape は不要（再分割されない）。
    /// 起動計画を立てる（#54）。tmux 判定は呼び出し側（テスト容易性）。
    pub(crate) fn agent_launch_plan(
        agent_cmd: &str,
        cwd: &Path,
        under_tmux: bool,
    ) -> AgentLaunch {
        let pipeline = format!("minas review | {agent_cmd}");
        if !under_tmux {
            return AgentLaunch::Manual(pipeline);
        }
        AgentLaunch::Tmux(vec![
            "tmux".into(),
            "split-window".into(),
            "-h".into(),
            "-c".into(),
            cwd.to_string_lossy().into_owned(),
            "sh".into(),
            "-c".into(),
            pipeline,
        ])
    }

    /// E: レビューコメントをエージェントに投げる（#54）。daemon 保持の
    /// 一覧を `minas review` で渡す。tmux 外では実行文面を案内するだけ。
    async fn launch_agent(&mut self) {
        let under_tmux = std::env::var("TMUX").is_ok_and(|v| !v.is_empty());
        self.launch_agent_with(under_tmux).await;
    }

    /// `launch_agent` の本体（tmux 判定を注入可能にし、テストする）。
    async fn launch_agent_with(&mut self, under_tmux: bool) {
        let Some(cmd) = self.agent_command.clone().filter(|s| !s.trim().is_empty()) else {
            self.flash =
                Some("agent_command を config.toml に設定してください".into());
            return;
        };
        match Self::agent_launch_plan(&cmd, &self.tree.root.clone(), under_tmux) {
            AgentLaunch::Manual(pipeline) => {
                self.flash =
                    Some(format!("tmux 外です: 隣ペインで実行してください: {pipeline}"));
            }
            AgentLaunch::Tmux(argv) => {
                match std::process::Command::new(&argv[0]).args(&argv[1..]).output() {
                    Ok(out) if out.status.success() => {
                        self.flash = Some("エージェントを隣ペインに起動しました".into());
                    }
                    Ok(out) => {
                        self.flash = Some(format!(
                            "tmux 起動失敗: {}",
                            String::from_utf8_lossy(&out.stderr).trim()
                        ));
                    }
                    Err(e) => {
                        self.flash = Some(format!("tmux を起動できません: {e}"));
                    }
                }
            }
        }
    }

    /// P: 基準全文ブラウズを開く（#52・a2）。注目文書の旧側テキストを
    /// フォーカスを動かさずに読む（復帰は Esc/P）。Mode 2 では旧側を見る。
    fn open_base_browse(&mut self) {
        let Some((repo, base, short, rel)) = (|| {
            let cmp = self.compare.as_ref().filter(|c| c.is_showing())?;
            let path = self.snapshot.path.as_deref()?;
            let rel = Path::new(path).strip_prefix(&cmp.repo).ok()?;
            Some((
                cmp.repo.clone(),
                cmp.base.clone(),
                cmp.short().to_string(),
                rel.to_string_lossy().replace('\\', "/"),
            ))
        })() else {
            self.flash = Some("比較を開始してください（D/M）".into());
            return;
        };
        match git::base_text(&repo, &base, &rel) {
            Err(e) => {
                self.flash = Some(format!("基準テキストを取得できません: {e}"));
            }
            Ok(None) => {
                self.flash = Some(format!("基準側に存在しません: {rel}"));
            }
            Ok(Some(text)) => {
                self.base_browse = Some(BaseBrowse {
                    title: format!("{rel} @{short}"),
                    lines: text.split('\n').map(str::to_string).collect(),
                    line: 0,
                    first: 0,
                });
                self.overlay = Overlay::BaseBrowse;
            }
        }
    }

    /// 基準ブラウズ内の移動（j/k）。body 高で追従する。
    fn move_browse(&mut self, delta: isize) {
        let Some(b) = self.base_browse.as_mut() else {
            return;
        };
        let len = b.lines.len().max(1);
        b.line = (b.line as isize + delta).clamp(0, len as isize - 1) as usize;
        let h = self.body_h.max(1);
        if b.line < b.first {
            b.first = b.line;
        }
        if b.line >= b.first + h {
            b.first = b.line - h + 1;
        }
    }

    /// Mode 2 のキー処理（#51）。移動・ジャンプ・終了のみ。編集不可。
    /// オーバーレイ系（T/G/A/C）は live 文書の表示として許可する。
    async fn handle_mode2_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        match key.code {
            Esc | Char('M') if key.modifiers.is_empty() => {
                self.pending.clear();
                self.toggle_mode2().await;
                return;
            }
            Char('D') if key.modifiers.is_empty() => {
                // Mode 2 中の D は Mode 1 に入らず終了する（toggle 放置防止）。
                self.pending.clear();
                self.toggle_mode2().await;
                return;
            }
            Tab if key.modifiers.is_empty() => {
                self.pending.clear();
                self.enter_gap_review();
                return;
            }
            Char('B') if key.modifiers.is_empty() => {
                self.pending.clear();
                self.open_commit_picker();
                return;
            }
            Char('P') if key.modifiers.is_empty() => {
                self.pending.clear();
                self.open_base_browse();
                return;
            }
            Down | Char('j') if key.modifiers.is_empty() => {
                self.pending.clear();
                self.v2_move(1);
                return;
            }
            Up | Char('k') if key.modifiers.is_empty() => {
                self.pending.clear();
                self.v2_move(-1);
                return;
            }
            Char('n') if key.modifiers.is_empty() => {
                self.jump_changed(1);
                return;
            }
            Char('N') if key.modifiers.is_empty() => {
                self.jump_changed(-1);
                return;
            }
            Char('T') | Char('G') | Char('A') | Char('C')
                if key.modifiers.is_empty() =>
            {
                self.pending.clear();
                // live 文書側の表示として既存アームへ委譲する。
                self.overlay_hotkey(key.code).await;
                return;
            }
            Char('K') if key.modifiers.is_empty() => {
                self.pending.clear();
                self.flash = Some("過去比較は読取り専用です（コメントは Mode 1 で）".into());
                return;
            }
            Char('E') if key.modifiers.is_empty() => {
                self.pending.clear();
                self.launch_agent().await;
                return;
            }
            _ => {
                self.pending.clear();
                self.flash = Some("読取り専用です（M:終了）".into());
                return;
            }
        }
    }
    /// 基準の再登録（再接続時・#49 adversarial）。表示中のみ送る。
    /// 接続なし・未開始・worktree 消失時は送らない。Mode 2 は両側。
    async fn reregister_compare(&mut self) {
        let Some(cmp) = self.compare.as_ref().filter(|c| c.show) else {
            return;
        };
        let repo_str = cmp.repo.to_string_lossy().into_owned();
        let sides: Vec<(String, String, bool)> = {
            let mut v = vec![(
                cmp.worktree.to_string_lossy().into_owned(),
                cmp.base.clone(),
                git::worktree_usable(&cmp.worktree),
            )];
            if let (Some(nb), Some(nwt)) = (cmp.new_base.clone(), cmp.new_worktree.clone()) {
                v.push((
                    nwt.to_string_lossy().into_owned(),
                    nb,
                    git::worktree_usable(&nwt),
                ));
            }
            v
        };
        for (root, commit, usable) in sides {
            if !usable {
                self.flash = Some("worktree が消えているため基準を再登録できません".into());
                return;
            }
            self.send(&Command::RegisterBaseRoot {
                root,
                commit,
                repo: Some(repo_str.clone()),
            })
            .await;
        }
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

/// `m` リーダーのバッファ（`s`/`d` + 文字、`r` + 2文字）をコマンドへ解決。
/// 未確定（まだ文字が足りない）なら None。
fn surround_command(buf: &str) -> Option<Command> {
    let chars: Vec<char> = buf.chars().collect();
    match chars.as_slice() {
        ['s', ch] => Some(Command::SurroundAdd { ch: *ch }),
        ['d', ch] => Some(Command::SurroundDelete { ch: *ch }),
        ['r', from, to] => Some(Command::SurroundReplace { from: *from, to: *to }),
        _ => None,
    }
}

/// バッファがまだ入力途中か（続きの文字を受け付ける）。
fn surround_pending(buf: &str) -> bool {
    let chars: Vec<char> = buf.chars().collect();
    matches!(chars.as_slice(), ['s'] | ['d'] | ['r'] | ['r', _])
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
    // #54: エージェント起動コマンド（config のみ・daemon 非関与）。
    app.agent_command = config.agent_command.clone();
    // 起動時: 死んだセッションの比較 worktree 残骸を掃除する（best-effort）。
    git::prune_stale_worktrees();
    // 起動: daemon へ接続（失敗は明示エラーで終了）
    app.connect().await.map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("daemon に接続できませんでした: {e}"),
        )
    })?;
    // 起動時ファイルを開く。初回描画を Open 応答より先に出しておく（応答待ちで
    // 真っ暗にならないように）。応答待ちはタイムアウト付き — daemon が LSP
    // 初期化（rust-analyzer の spawn + initialize）に時間をかけていても UI を
    // 固めず、遅延応答はメインループの read_sock_line が拾って反映する。
    for file in &files {
        app.pending_open = Some(file.clone());
        terminal.draw(|f| render::render(f, &mut app))?;
        app.send(&Command::Open {
            path: conn::absolutize(file),
        })
        .await;
        // 応答を1行読んで状態に反映する（起動直後の空画面を避ける）
        let mut line = String::new();
        if let Some(c) = app.conn.as_mut() {
            match timeout(OPEN_RESPONSE_TIMEOUT, c.reader.read_line(&mut line)).await {
                Ok(Ok(_)) => app.on_socket_line(line).await,
                Ok(Err(_)) => {
                    app.disconnect("daemon との接続が切れました — 再接続します");
                    break;
                }
                Err(_) => {
                    // 応答遅延: pending_open 表示を出したままメインループへ進む。
                    // （起動ファイルは実質 0〜1 枚。2 枚目以降はタイムアウト時に
                    // 保留され `:o` で開く — ponytail: 複数起動ファイル対応は
                    // 応答順の追跡が必要になるため割り切る）
                    break;
                }
            }
        }
    }
    // 端末サイズを記録して SetViewport を送る
    // 起動時: 死んだセッションの基準登録を掃除する（#49 adversarial）。
    // Open 応答が遅延中（pending_open が残っている）なら sweep は後回し —
    // sweep の逐次読みが遅延応答を吸ってしまうため（応答はメインループで拾う）。
    if app.pending_open.is_none() {
        app.sweep_dead_base_roots().await;
    }
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
    fn find_existing_matches_resolved_or_stored_line() {
        // #50: 編集 prefill の当て方（現在側は解決行・保存行のどちらでも当て、
        // 基準側は保存行のみ）。送信は保存行で行い重複を作らない。
        use mina_protocol::ReviewCommentView;
        let list = vec![
            ReviewCommentView {
                path: "/r/a.rs".into(),
                side: ReviewSide::Current,
                line: 2,
                resolved_line: 5,
                stale: true,
                snippet: "beta".into(),
                body: "直して".into(),
                base: "abc".into(),
            },
            ReviewCommentView {
                path: "/w/a.rs".into(),
                side: ReviewSide::Base,
                line: 7,
                resolved_line: 7,
                stale: false,
                snippet: "old".into(),
                body: "消さないで".into(),
                base: "abc".into(),
            },
        ];
        // ずれた先（解決行）で当て → 保存行 2 で上書き送信する。
        let hit = App::find_existing(&list, ReviewSide::Current, "/r/a.rs", 5).unwrap();
        assert_eq!((hit.line, hit.body.as_str()), (2, "直して"));
        // 保存行でも当たる。
        assert!(App::find_existing(&list, ReviewSide::Current, "/r/a.rs", 2).is_some());
        // 無関係行・他パス・他側は当たらない。
        assert!(App::find_existing(&list, ReviewSide::Current, "/r/a.rs", 3).is_none());
        assert!(App::find_existing(&list, ReviewSide::Current, "/r/b.rs", 5).is_none());
        assert!(App::find_existing(&list, ReviewSide::Base, "/r/a.rs", 5).is_none());
        // 基準側は保存行で当たる。
        let hit = App::find_existing(&list, ReviewSide::Base, "/w/a.rs", 7).unwrap();
        assert_eq!(hit.body.as_str(), "消さないで");
    }

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
            // 実端末は Shift+T を Char('T')+SHIFT（や Char('t')+SHIFT）で報告する
            for code in [KeyCode::Char('T'), KeyCode::Char('t')] {
                let shift_t = KeyEvent::new(code, KeyModifiers::SHIFT);
                app.handle_key(shift_t).await;
                assert_eq!(app.overlay, Overlay::Tree, "Shift+T({code:?})でツリーが開く");
                app.handle_key(esc).await;
                assert_eq!(app.overlay, Overlay::None, "Esc で閉じる");
            }

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

            // プロンプトを開くキー（? はヘルプに譲ったので含めない）
            for (ch, check) in [
                (':', "command"),
                ('/', "search"),
                ('r', "replace"),
            ] {
                let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
                app.handle_key(key).await;
                assert!(app.prompt.is_some(), "{ch} でプロンプト ({check})");
                app.handle_key(esc).await;
                assert!(app.prompt.is_none(), "Esc でキャンセル");
            }

            // ?: 後方検索ではなくヘルプ（Overlay::Help）を開く。j でスクロール、
            // 再び ? や Esc で閉じる。
            let q = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
            app.handle_key(q).await;
            assert_eq!(app.overlay, Overlay::Help, "? でヘルプが開く");
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
                .await;
            assert_eq!(app.help_scroll, 1, "j でスクロール");
            app.handle_key(q).await;
            assert_eq!(app.overlay, Overlay::None, "再び ? で閉じる");
            app.handle_key(q).await;
            app.handle_key(esc).await;
            assert_eq!(app.overlay, Overlay::None, "Esc でも閉じる");

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
    fn surround_m_reader_flow() {
        // m リーダー: ms/d/r + 文字で確定し、未確定・無効入力はプロンプトに残る
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
            let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            let plain = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

            // m でプロンプト、s までは残り、( で確定（接続なしなので送信は no-op）
            app.handle_key(plain('m')).await;
            assert!(matches!(&app.prompt, Some(Prompt::Surround(b)) if b.is_empty()));
            app.handle_key(plain('s')).await;
            assert!(matches!(&app.prompt, Some(Prompt::Surround(b)) if b == "s"));
            app.handle_key(plain('(')).await;
            assert!(app.prompt.is_none(), "ms( で確定して閉じる");

            // mr: 2文字目までは pending、3文字目で確定
            app.handle_key(plain('m')).await;
            app.handle_key(plain('r')).await;
            app.handle_key(plain('(')).await;
            assert!(matches!(&app.prompt, Some(Prompt::Surround(b)) if b == "r("));
            app.handle_key(plain(']')).await;
            assert!(app.prompt.is_none(), "mr(] で確定");

            // Esc でキャンセル、無効なリーダーは flash 付きで閉じる
            app.handle_key(plain('m')).await;
            app.handle_key(esc).await;
            assert!(app.prompt.is_none(), "Esc でキャンセル");
            app.handle_key(plain('m')).await;
            app.handle_key(plain('x')).await;
            assert!(app.prompt.is_none(), "無効なリーダーで閉じる");
            assert!(app.flash.is_some(), "理由が flash に載る");
        });
    }

    #[test]
    fn surround_command_and_pending_parse() {
        assert_eq!(
            surround_command("s("),
            Some(Command::SurroundAdd { ch: '(' })
        );
        assert_eq!(
            surround_command("d\""),
            Some(Command::SurroundDelete { ch: '"' })
        );
        assert_eq!(
            surround_command("r(]"),
            Some(Command::SurroundReplace { from: '(', to: ']' })
        );
        assert!(surround_pending("s"));
        assert!(surround_pending("d"));
        assert!(surround_pending("r("));
        assert!(!surround_pending("s("));
        assert!(!surround_pending("x"));
    }

    #[test]
    fn command_line_parses() {
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

    /// #51: Mode 2 の pin・canvas・差分（旧欠落/新欠落を含む）。
    #[test]
    fn mode2_pin_pair_and_diff() {
        let root = git::init_repo("m2diff");
        let g = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        std::fs::write(root.join("a.txt"), "v1\n").unwrap();
        std::fs::write(root.join("del.txt"), "gone\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "c1"]);
        let c1 = g(&["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(root.join("a.txt"), "v2\n").unwrap();
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        std::fs::remove_file(root.join("del.txt")).unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "c2"]);
        let c2 = g(&["rev-parse", "HEAD"]).trim().to_string();

        let mut cmp = CompareState::pin_mode2_pair(root.clone(), &c1, &c2).unwrap();
        assert!(cmp.is_mode2());
        assert!(cmp.label().contains(".."), "label: {}", cmp.label());
        assert_eq!(cmp.files.len(), 3, "M/A/D");
        // worktree 実体が両側にある（後で撤去する）。
        assert!(cmp.worktree.exists());
        assert!(cmp.new_worktree.as_ref().unwrap().exists());
        let snap_for = |name: &str| StateSnapshot {
            path: Some(root.join(name).to_string_lossy().into_owned()),
            ..StateSnapshot::default()
        };
        let mut flash = None;
        // 変更あり: Modified（末尾空行は Same）。canvas は新側。
        let d = cmp.diff_for(&snap_for("a.txt"), &mut flash).unwrap();
        assert_eq!(d.kinds, vec![git::RowKind::Modified, git::RowKind::Same]);
        let canvas = cmp.mode2_canvas_text(&snap_for("a.txt")).unwrap();
        assert_eq!(canvas, "v2\n");
        // 旧側のみ: 全 Gap＋空 canvas。
        let d = cmp.diff_for(&snap_for("del.txt"), &mut flash).unwrap();
        assert!(d.kinds.is_empty());
        assert_eq!(d.gaps.len(), 1);
        assert_eq!(d.gaps[0].lines[0], "gone");
        // 新側のみ: 全 Added。
        let d = cmp.diff_for(&snap_for("b.txt"), &mut flash).unwrap();
        assert!(d.kinds.iter().all(|k| *k == git::RowKind::Added));
        // 同一ペアは拒否する。
        assert!(CompareState::pin_mode2_pair(root.clone(), &c1, &c1).is_err());
        git::remove_worktree(&root, &cmp.worktree.clone());
        git::remove_worktree(&root, cmp.new_worktree.as_ref().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #51: Mode 2 は読取り専用（編集系キーは遮断・j/k は自前カーソル）。
    #[test]
    fn mode2_readonly_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let root = git::init_repo("m2keys");
        let g = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        std::fs::write(root.join("a.txt"), "l1\nl2\nl3\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "c1"]);
        std::fs::write(root.join("a.txt"), "l1\nL2\nl3\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "c2"]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = test_app();
            app.tree = TreeState::new(root.clone());
            // M で Mode 2 に入る（HEAD~1/HEAD 即ピン）。
            let m = KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE);
            app.handle_key(m).await;
            assert!(app.compare.as_ref().is_some_and(|c| c.is_mode2()), "{:?}", app.flash);
            // git の toplevel は正規化済みで返るため path も正規化する。
            let canon = std::fs::canonicalize(&root).unwrap();
            app.snapshot.path = Some(canon.join("a.txt").to_string_lossy().into_owned());
            // i は遮断（プロンプトなし・flash）。
            let i = KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE);
            app.handle_key(i).await;
            assert!(app.prompt.is_none());
            assert!(app.flash.as_deref().unwrap_or("").contains("読取り専用"));
            // : も遮断する。
            let colon = KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT);
            app.handle_key(colon).await;
            assert!(app.prompt.is_none());
            // j/k は自前カーソルを動かす。
            let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
            app.handle_key(j).await;
            assert_eq!(app.v2_line, 1);
            let k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
            app.handle_key(k).await;
            assert_eq!(app.v2_line, 0);
            // K は flash のみ（プロンプトなし）。
            let kk = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE);
            app.handle_key(kk).await;
            assert!(app.prompt.is_none());
            // Esc で終了（worktree 撤去まで確認）。
            let wt_old = app.compare.as_ref().unwrap().worktree.clone();
            let wt_new = app.compare.as_ref().unwrap().new_worktree.clone().unwrap();
            let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            app.handle_key(esc).await;
            assert!(app.compare.is_none());
            assert!(!wt_old.exists() && !wt_new.exists());
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #51: ピッカーの o/n 差し替え（同値は拒否・別値は作り直し）。
    #[test]
    fn mode2_picker_assign() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let root = git::init_repo("m2pick");
        let g = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        for (i, body) in ["one", "two", "three"].iter().enumerate() {
            std::fs::write(root.join("a.txt"), format!("{body}\n")).unwrap();
            g(&["add", "."]);
            g(&["commit", "-qm", &format!("c{i}")]);
        }
        let c1 = g(&["rev-parse", "HEAD~2"]).trim().to_string();
        let c2 = g(&["rev-parse", "HEAD~1"]).trim().to_string();
        let c3 = g(&["rev-parse", "HEAD"]).trim().to_string();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = test_app();
            app.tree = TreeState::new(root.clone());
            app.compare = Some(CompareState::pin_mode2_pair(root.clone(), &c1, &c3).unwrap());
            // B でピッカーが開く（3 件）。
            let b = KeyEvent::new(KeyCode::Char('B'), KeyModifiers::NONE);
            app.handle_key(b).await;
            assert_eq!(app.overlay, Overlay::Commits);
            assert_eq!(app.commits.len(), 3);
            // 先頭（c3＝新側と同値）を旧側に → 拒否。
            app.commit_sel = 0;
            let o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE);
            app.handle_key(o).await;
            assert!(app.flash.as_deref().unwrap_or("").contains("同じコミット"));
            assert_eq!(app.compare.as_ref().unwrap().base, c1);
            // c2 を旧側に → 作り直し。
            app.commit_sel = 1;
            app.handle_key(o).await;
            let cmp = app.compare.as_ref().unwrap();
            assert_eq!(cmp.base, c2);
            assert!(app.flash.as_deref().unwrap_or("").contains("旧側"));
            // 後始末。
            app.drop_compare().await;
            assert!(app.compare.is_none());
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #51: Mode 2 の描画（新側 canvas のマーカー＋旧..新ラベル）。
    #[test]
    fn mode2_render_markers() {
        let root = git::init_repo("m2render");
        let g = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        std::fs::write(root.join("a.txt"), "keep\nold\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "c1"]);
        std::fs::write(root.join("a.txt"), "keep\nnew\nadded\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "c2"]);
        let mut app = test_app();
        let c1 = git::run(&root, &["rev-parse", "HEAD~1"]).unwrap();
        let c2 = git::run(&root, &["rev-parse", "HEAD"]).unwrap();
        let cmp = CompareState::pin_mode2_pair(root.clone(), c1.trim(), c2.trim()).unwrap();
        app.compare = Some(cmp);
        app.snapshot.path = Some(root.join("a.txt").to_string_lossy().into_owned());
        draw_to_test_backend(&mut app, |rows| {
            let nospace: Vec<String> = rows.iter().map(|r| r.replace(' ', "")).collect();
            assert!(nospace.iter().any(|r| r.contains('~')), "modified marker: {rows:?}");
            assert!(nospace.iter().any(|r| r.contains('+')), "added marker: {rows:?}");
            // 100 桁 backend では長いパスでラベルが切れるため記号だけ見る。
            assert!(rows.iter().any(|r| r.contains("◈") && r.contains("..")), "label: {rows:?}");
            assert!(rows.iter().any(|r| r.contains("new")), "canvas は新側: {rows:?}");
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #52・a2: 基準ブラウズの開閉・移動（フォーカス不動）。
    #[test]
    fn base_browse_open_move_close() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let root = git::init_repo("m2browse");
        let g = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        std::fs::write(root.join("a.txt"), "b1\nb2\nb3\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "c1"]);
        std::fs::write(root.join("a.txt"), "c1\n").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut app = test_app();
            app.tree = TreeState::new(root.clone());
            // D で Mode 1 に入る（stash create で dirty を含めてピン）。
            let d = KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE);
            app.handle_key(d).await;
            assert!(app.compare.is_some(), "pin: {:?}", app.flash);
            let canon = std::fs::canonicalize(&root).unwrap();
            app.snapshot.path = Some(canon.join("a.txt").to_string_lossy().into_owned());
            app.snapshot.text = "c1\n".to_string();
            // P で開く（D ピンは dirty 込みのため旧側も c1・フォーカス文書は不変）。
            let p = KeyEvent::new(KeyCode::Char('P'), KeyModifiers::NONE);
            app.handle_key(p).await;
            assert_eq!(app.overlay, Overlay::BaseBrowse);
            let b = app.base_browse.as_ref().unwrap();
            assert_eq!(b.lines[0], "c1");
            assert!(b.title.contains("a.txt"));
            assert_eq!(app.snapshot.text, "c1\n", "live 文書は不変");
            // j で移動・Esc で復帰。
            let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
            app.handle_key(j).await;
            assert_eq!(app.base_browse.as_ref().unwrap().line, 1);
            let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            app.handle_key(esc).await;
            assert_eq!(app.overlay, Overlay::None);
            assert!(app.base_browse.is_none());
            app.drop_compare().await;
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #54: 起動計画（tmux argv / 手動文面）。
    #[test]
    fn agent_launch_plan_shapes() {
        let cwd = PathBuf::from("/repo");
        match App::agent_launch_plan("claude -p", &cwd, true) {
            AgentLaunch::Tmux(argv) => {
                assert_eq!(
                    argv,
                    vec![
                        "tmux",
                        "split-window",
                        "-h",
                        "-c",
                        "/repo",
                        "sh",
                        "-c",
                        "minas review | claude -p",
                    ]
                );
            }
            AgentLaunch::Manual(_) => panic!("tmux 下では分割する"),
        }
        match App::agent_launch_plan("claude -p", &cwd, false) {
            AgentLaunch::Manual(text) => {
                assert_eq!(text, "minas review | claude -p");
            }
            AgentLaunch::Tmux(_) => panic!("tmux 外では文面だけ"),
        }
    }

    /// #54: E の振る舞い（未設定→案内 / tmux 外→文面 / spawn 経路）。
    #[test]
    fn agent_launch_key_behaviors() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // 未設定: 案内のみ。
            let mut app = test_app();
            app.launch_agent_with(false).await;
            assert!(
                app.flash.as_deref().unwrap_or("").contains("agent_command"),
                "{:?}",
                app.flash
            );
            // tmux 外: 実行文面を案内する（起動しない）。
            let mut app = test_app();
            app.agent_command = Some("claude -p".into());
            app.launch_agent_with(false).await;
            let flash = app.flash.as_deref().unwrap_or("");
            assert!(flash.contains("minas review | claude -p"), "{flash:?}");
            // E キーからも同じ経路（未設定→案内）。
            let mut app = test_app();
            let e = KeyEvent::new(KeyCode::Char('E'), KeyModifiers::NONE);
            app.handle_key(e).await;
            assert!(
                app.flash.as_deref().unwrap_or("").contains("agent_command"),
                "{:?}",
                app.flash
            );
            // spawn 経路: tmux 非搭載環境でのみ失敗文面を検証する
            // （搭載環境では実際に分割してしまうため開かない）。
            if std::process::Command::new("tmux").arg("-V").output().is_err() {
                let mut app = test_app();
                app.agent_command = Some("true".into());
                app.launch_agent_with(true).await;
                let flash = app.flash.as_deref().unwrap_or("");
                assert!(flash.contains("tmux"), "{flash:?}");
            }
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
                new_base: None,
                new_worktree: None,
                show: false,
                files: Vec::new(),
                files_gen: 0,
                base_texts: HashMap::new(),
                new_texts: HashMap::new(),
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
                new_base: None,
                new_worktree: None,
                show: true,
                files: Vec::new(),
                diffs: HashMap::new(),
                base_texts: HashMap::new(),
                new_texts: HashMap::new(),
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
                // 言語サーバ一覧（rust2 #49 で追加）。このテストは base root の
                // sweep だけを見るので空でよい（フィールドの存在が要求される）。
                servers: Vec::new(),
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
            new_base: None,
            new_worktree: None,
            show: true,
            files: Vec::new(),
            files_gen: 0,
            base_texts: HashMap::new(),
            new_texts: HashMap::new(),
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
            new_base: None,
            new_worktree: None,
            show: true,
            files: Vec::new(),
            files_gen: 0,
            base_texts: HashMap::new(),
            new_texts: HashMap::new(),
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
