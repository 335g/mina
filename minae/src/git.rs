//! 比較閲覧 Mode 1 のクライアント側 git 層（#49）。
//!
//! 方針: デーモン・プロトコル無変更。基準コミットのテキスト取得・変更一覧・
//! 行対応はすべて `git` CLI の shell-out で行う（新規依存なし）。
//! 行対応の真実は `git diff -U0` の hunk のみ。独自の diff 実装は持たない。
//!
//! 精度注: 差分は「基準 blob vs デーモンテキスト（canvas）」で取る。
//! ワークツリーとの差分（`git diff <base> -- <path>`）ではない — Dirty
//! （未保存編集）があると両者はずれるため、canvas を temp に書き出して
//! `git diff --no-index` で直接突き合わせる。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// git 失敗（リポジトリ外・オブジェクト不在・プロセス異常）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitError(pub(crate) String);

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "git: {}", self.0)
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| GitError(format!("spawn failed: {e}")))
}

fn run(repo: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = git(repo, args)?;
    if !out.status.success() {
        return Err(GitError(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// リポジトリルート（`git rev-parse --show-toplevel`）。cwd 基準。
pub(crate) fn repo_root(cwd: &Path) -> Result<PathBuf, GitError> {
    let out = run(cwd, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(out.trim()))
}

/// 基準のピン留め: `git stash create`（dirty を含む現在状態の無名スナップ
/// ショット。worktree に触らない）→ 空なら HEAD にフォールバック。
/// 戻り値はコミットオブジェクト ID。
pub(crate) fn pin_base(repo: &Path) -> Result<String, GitError> {
    let stash = run(repo, &["stash", "create"])?;
    if !stash.trim().is_empty() {
        return Ok(stash.trim().to_string());
    }
    let head = run(repo, &["rev-parse", "HEAD"])?;
    Ok(head.trim().to_string())
}

/// オブジェクト存在確認（`git cat-file -e`）。無名オブジェクトの gc 対策の入口。
pub(crate) fn verify_object(repo: &Path, id: &str) -> Result<(), GitError> {
    let out = git(repo, &["cat-file", "-e", id])?;
    if out.status.success() {
        Ok(())
    } else {
        Err(GitError(format!("基準 {id} がありません")))
    }
}

/// 比較用 worktree のパス（#49・v13(a1)）。`<tmp>/mina-base-<pid>-<repohash>/`。
/// pid 付きで並行 TUI と衝突しない。repohash でリポジトリを区別する。
/// daemon 側の登録キーと一致させるため正規化してから hash する。
pub(crate) fn worktree_path(repo: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let h = mina_protocol::fnv1a64(canon.to_string_lossy().as_bytes());
    // temp 自体も正規化する（macOS の /var→/private/var 等）。daemon 側の
    // 正規化済みパス・応答パスと一致させ、表示マッピングを効かせるため。
    let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    tmp.join(format!("mina-base-{}-{h:016x}", std::process::id()))
}

/// worktree が使える状態か（管理ファイルの有無）。
pub(crate) fn worktree_usable(wt: &Path) -> bool {
    wt.join(".git").exists()
}

/// worktree を用意する（#49）。既存は捨てて作り直す（clean 前提にしない）。
/// pin ごと（初回 D・B 更新時）の低頻度操作のため確実性を優先する。
/// 残骸 admin があっても `prune` で掃除してから作る。
pub(crate) fn ensure_worktree(repo: &Path, base: &str) -> Result<PathBuf, GitError> {
    let wt = worktree_path(repo);
    let _ = std::fs::remove_dir_all(&wt);
    let _ = run(repo, &["worktree", "prune"]);
    run(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            &wt.to_string_lossy(),
            base,
        ],
    )
    .map_err(|e| GitError(format!("worktree 作成失敗: {e}")))?;
    Ok(wt)
}

/// worktree を撤去する（best-effort）。
pub(crate) fn remove_worktree(repo: &Path, wt: &Path) {
    let _ = run(
        repo,
        &["worktree", "remove", "--force", &wt.to_string_lossy()],
    );
    let _ = std::fs::remove_dir_all(wt);
}

/// 死んだセッションの worktree 残骸を掃除する（起動時・best-effort）。
/// `mina-base-<pid>-<hash>` の pid が存在しなければディレクトリを消す。
/// repo 側 admin は各リポジトリの `prune` に任せる（ここでは触らない）。
/// 不正な名前・自 pid・生存 pid は消さない側に倒す。
pub(crate) fn prune_stale_worktrees() {
    let tmp = std::env::temp_dir();
    let Ok(rd) = std::fs::read_dir(&tmp) else {
        return;
    };
    let me = std::process::id().to_string();
    for e in rd.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("mina-base-") else {
            continue;
        };
        let Some((pid_s, _)) = rest.split_once('-') else {
            continue;
        };
        if pid_s == me || pid_alive(pid_s) {
            continue;
        }
        let _ = std::fs::remove_dir_all(e.path());
    }
}

/// 起動時 sweep 用: 登録済みのはずの基準 root が死んでいたら true（#49）。
/// `mina-base-<pid>-<hash>` 形式で、自 pid でなく（dir 消失 OR pid 死亡）
/// のとき。形式外・自 pid・不正 pid は消さない側に倒す。
pub(crate) fn should_unregister_dead_root(root: &str) -> bool {
    let name = Path::new(root)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let Some(rest) = name.strip_prefix("mina-base-") else {
        return false;
    };
    let Some((pid_s, _)) = rest.split_once('-') else {
        return false;
    };
    if pid_s.parse::<u32>().is_err() {
        return false;
    }
    if pid_s == std::process::id().to_string() {
        return false;
    }
    !Path::new(root).exists() || !pid_alive(pid_s)
}

/// pid の生存確認（`kill -0`。unix 前提 — daemon と同じ）。
fn pid_alive(pid: &str) -> bool {
    if pid.parse::<u32>().is_err() {
        return true;
    }
    Command::new("kill")
        .args(["-0", pid])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true)
}

/// 基準コミット中のファイル内容（`git show <base>:<rel>`）。
/// 基準側に存在しない（新規ファイル等）は `None`。
pub(crate) fn base_text(repo: &Path, base: &str, rel: &str) -> Result<Option<String>, GitError> {
    let out = git(repo, &["show", &format!("{base}:{rel}")])?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// 変更ファイルの種別（ツリーの M/A/D マーカー用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeStatus {
    Added,
    Modified,
    Deleted,
}

impl ChangeStatus {
    pub(crate) fn marker(self) -> char {
        match self {
            ChangeStatus::Added => '+',
            ChangeStatus::Modified => '~',
            ChangeStatus::Deleted => '-',
        }
    }
}

/// 変更ファイル（絶対パス＋種別）。
#[derive(Debug, Clone)]
pub(crate) struct ChangedFile {
    pub(crate) path: PathBuf,
    pub(crate) status: ChangeStatus,
}

/// 変更一覧: `git diff --name-status -z <base>` ＋ 未追跡
/// （`git ls-files --others --exclude-standard -z` を Added 扱い）。
/// リネーム/コピーは Deleted(旧)＋Added(新) に分解する。
pub(crate) fn changed_files(repo: &Path, base: &str) -> Result<Vec<ChangedFile>, GitError> {
    let out = git(repo, &["diff", "--name-status", "-z", base])?;
    if !out.status.success() {
        return Err(GitError(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let mut files = Vec::new();
    // -z: レコードは NUL 区切り。R/C は「状態・旧・新」の 3 件。
    let mut parts = out.stdout.split(|b| *b == 0);
    while let Some(status) = parts.next() {
        if status.is_empty() {
            continue;
        }
        let code = status[0] as char;
        let rel = |bs: &[u8]| {
            repo.join(String::from_utf8_lossy(bs).into_owned())
        };
        match code {
            'A' => {
                if let Some(p) = parts.next() {
                    files.push(ChangedFile {
                        path: rel(p),
                        status: ChangeStatus::Added,
                    });
                }
            }
            'M' | 'T' => {
                if let Some(p) = parts.next() {
                    files.push(ChangedFile {
                        path: rel(p),
                        status: ChangeStatus::Modified,
                    });
                }
            }
            'D' => {
                if let Some(p) = parts.next() {
                    files.push(ChangedFile {
                        path: rel(p),
                        status: ChangeStatus::Deleted,
                    });
                }
            }
            'R' | 'C' => {
                let old = parts.next();
                let new = parts.next();
                if let (Some(o), Some(n)) = (old, new) {
                    files.push(ChangedFile {
                        path: rel(o),
                        status: ChangeStatus::Deleted,
                    });
                    files.push(ChangedFile {
                        path: rel(n),
                        status: ChangeStatus::Added,
                    });
                }
            }
            _ => {
                // 未知の状態は次レコードへ（パス 1 件分を捨てる）
                let _ = parts.next();
            }
        }
    }
    // 未追跡は Added。
    let out = git(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if out.status.success() {
        for p in out.stdout.split(|b| *b == 0) {
            if p.is_empty() {
                continue;
            }
            files.push(ChangedFile {
                path: repo.join(String::from_utf8_lossy(p).into_owned()),
                status: ChangeStatus::Added,
            });
        }
    }
    Ok(files)
}

/// canvas 行（新側テキスト）の行種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    Same,
    Modified,
    Added,
}

impl RowKind {
    /// 文頭マーカー（1 列）。Same はマーカーなし。
    pub(crate) fn marker(self) -> Option<char> {
        match self {
            RowKind::Same => None,
            RowKind::Modified => Some('~'),
            RowKind::Added => Some('+'),
        }
    }
}

/// 旧側のみの行群（新側の `at` の直前に挿入表示する）。
#[derive(Debug, Clone)]
pub(crate) struct Gap {
    /// 挿入位置（新側 0-origin 行番号。この行の直前に出す。`== canvas 行数` は末尾）。
    pub(crate) at: usize,
    /// 旧側の開始行番号（1-origin。ガター表示用）。
    pub(crate) old_start: usize,
    pub(crate) lines: Vec<String>,
}

/// 1 ファイルの差分配列（canvas 基準）。
#[derive(Debug, Clone)]
pub(crate) struct FileDiff {
    /// canvas の各行の種別（canvas 行数と同長）。
    pub(crate) kinds: Vec<RowKind>,
    /// 旧側のみの行群。
    pub(crate) gaps: Vec<Gap>,
}

impl FileDiff {
    /// 全行 Same（差分なし）。
    pub(crate) fn clean(nlines: usize) -> Self {
        Self {
            kinds: vec![RowKind::Same; nlines],
            gaps: Vec::new(),
        }
    }

    /// 全行 Added（基準側に存在しない）。
    pub(crate) fn all_added(nlines: usize) -> Self {
        Self {
            kinds: vec![RowKind::Added; nlines],
            gaps: Vec::new(),
        }
    }

}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mina-cmp-{}-{}-{tag}", std::process::id(), n))
}

/// 基準テキスト vs canvas テキストの差分配列。
/// canvas を temp に書き出し `git diff --no-index -U0` で突き合わせる
/// （Dirty があっても canvas 通りに合う）。終了コード 0/1 はどちらも正常。
pub(crate) fn diff_texts(base_text: &str, canvas_text: &str) -> Result<FileDiff, GitError> {
    let base_tmp = temp_path("base");
    let canvas_tmp = temp_path("canvas");
    std::fs::write(&base_tmp, base_text).map_err(|e| GitError(format!("temp write: {e}")))?;
    std::fs::write(&canvas_tmp, canvas_text).map_err(|e| GitError(format!("temp write: {e}")))?;
    let out = Command::new("git")
        .args([
            "diff",
            "--no-index",
            "--no-color",
            "-U0",
            &base_tmp.to_string_lossy(),
            &canvas_tmp.to_string_lossy(),
        ])
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| GitError(format!("spawn failed: {e}")))?;
    let _ = std::fs::remove_file(&base_tmp);
    let _ = std::fs::remove_file(&canvas_tmp);
    match out.status.code() {
        Some(0) => Ok(FileDiff::clean(canvas_text.split('\n').count())),
        Some(1) => Ok(parse_unified_zero(
            &String::from_utf8_lossy(&out.stdout),
            canvas_text.split('\n').count(),
        )),
        _ => Err(GitError(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )),
    }
}

/// `-U0` unified diff の構文解析（純粋関数）。hunk 内の -/+ 対応が
/// 行対応の唯一の真実。- と + の先頭一致分は Modified（新テキストで表示）、
/// 余剰 + は Added、余剰 - は Gap（旧位置に挿入）。
pub(crate) fn parse_unified_zero(diff: &str, nlines: usize) -> FileDiff {
    let mut kinds: Vec<Option<RowKind>> = vec![None; nlines];
    let mut gaps: Vec<Gap> = Vec::new();
    // hunk ヘッダ前の ---/+++/diff 行は読み飛ばす（内容行との衝突回避）。
    let mut in_hunk = false;
    let mut new_idx = 0usize;
    let mut minus: Vec<String> = Vec::new();
    let mut plus: Vec<String> = Vec::new();
    // hunk 先頭の旧側行番号（1-origin）。
    let mut hunk_old = 1usize;

    let flush = |minus: &mut Vec<String>,
                     plus: &mut Vec<String>,
                     new_idx: &mut usize,
                     hunk_old: &mut usize,
                     kinds: &mut [Option<RowKind>],
                     gaps: &mut Vec<Gap>| {
        let paired = minus.len().min(plus.len());
        for _ in 0..paired {
            minus.remove(0);
            plus.remove(0);
            if *new_idx < kinds.len() {
                kinds[*new_idx] = Some(RowKind::Modified);
            }
            *new_idx += 1;
        }
        for _ in plus.drain(..) {
            if *new_idx < kinds.len() {
                kinds[*new_idx] = Some(RowKind::Added);
            }
            *new_idx += 1;
        }
        if !minus.is_empty() {
            let old_start = *hunk_old;
            let rest = std::mem::take(minus);
            *hunk_old += rest.len();
            gaps.push(Gap {
                at: (*new_idx).min(kinds.len()),
                old_start,
                lines: rest,
            });
        }
    };

    for raw in diff.split('\n') {
        if let Some(h) = raw.strip_prefix("@@ ") {
            flush(&mut minus, &mut plus, &mut new_idx, &mut hunk_old, &mut kinds, &mut gaps);
            // "@@ -a[,b] +c[,d] @@"
            let mut old_s = 1usize;
            let mut new_s = 1usize;
            for part in h.split_whitespace() {
                if let Some(o) = part.strip_prefix('-') {
                    old_s = o.split(',').next().and_then(|n| n.parse().ok()).unwrap_or(1);
                } else if let Some(n) = part.strip_prefix('+') {
                    new_s = n.split(',').next().and_then(|n| n.parse().ok()).unwrap_or(1);
                }
            }
            // hunk までの Same 行を埋める（1-origin → 0-origin）。
            let target = new_s.saturating_sub(1);
            while new_idx < target.min(nlines) {
                if kinds[new_idx].is_none() {
                    kinds[new_idx] = Some(RowKind::Same);
                }
                new_idx += 1;
            }
            hunk_old = old_s;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(content) = raw.strip_prefix('-') {
            minus.push(content.to_string());
        } else if let Some(content) = raw.strip_prefix('+') {
            plus.push(content.to_string());
        } else if raw == "\\ No newline at end of file" {
            // 内容ではない。無視。
        } else {
            // -U0 に context 行は出ないはず。到来したら Same 扱いで進める。
            flush(&mut minus, &mut plus, &mut new_idx, &mut hunk_old, &mut kinds, &mut gaps);
            new_idx += 1;
        }
    }
    flush(&mut minus, &mut plus, &mut new_idx, &mut hunk_old, &mut kinds, &mut gaps);
    // 残りは Same。
    let kinds = kinds
        .into_iter()
        .map(|k| k.unwrap_or(RowKind::Same))
        .collect();
    FileDiff { kinds, gaps }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modify_pairs_as_modified() {
        let d = parse_unified_zero(
            "@@ -1,2 +1,2 @@\n-a\n-b\n+A\n+B\n",
            2,
        );
        assert_eq!(d.kinds, vec![RowKind::Modified, RowKind::Modified]);
        assert!(d.gaps.is_empty());
    }

    #[test]
    fn pure_add_and_delete() {
        let d = parse_unified_zero("@@ -2,0 +3 @@\n+x\n", 3);
        assert_eq!(d.kinds, vec![RowKind::Same, RowKind::Same, RowKind::Added]);
        let d = parse_unified_zero("@@ -2 +2,0 @@\n-x\n", 2);
        assert_eq!(d.kinds, vec![RowKind::Same, RowKind::Same]);
        assert_eq!(d.gaps.len(), 1);
        assert_eq!(d.gaps[0].at, 1);
        assert_eq!(d.gaps[0].old_start, 2);
        assert_eq!(d.gaps[0].lines, vec!["x".to_string()]);
    }

    #[test]
    fn mixed_hunk_splits_pair_and_gap() {
        // - が + より多い: 先頭一致分 Modified、余剰 - は Gap。
        let d = parse_unified_zero("@@ -1,3 +1 @@\n-a\n-b\n-c\n+A\n", 1);
        assert_eq!(d.kinds, vec![RowKind::Modified]);
        assert_eq!(d.gaps.len(), 1);
        assert_eq!(d.gaps[0].at, 1);
        assert_eq!(d.gaps[0].lines.len(), 2);
    }

    #[test]
    fn header_like_content_lines_are_safe() {
        // 削除行の内容が "--x"（raw "---x"）でも hunk 内では内容行として扱い、
        // ヘッダと誤認しない。追加行 "+--y" と対になり Modified。
        let d = parse_unified_zero(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n---x\n+--y\n",
            1,
        );
        assert_eq!(d.kinds, vec![RowKind::Modified]);
        assert!(d.gaps.is_empty());
    }

    #[test]
    fn clean_and_added_constructors() {
        assert_eq!(FileDiff::clean(2).kinds, vec![RowKind::Same, RowKind::Same]);
        assert_eq!(
            FileDiff::all_added(1).kinds,
            vec![RowKind::Added]
        );
    }

    /// temp git リポジトリを用意する（hermetic）。
    fn init_repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mina-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let g = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        root
    }

    #[test]
    fn worktree_lifecycle_in_temp_repo() {
        let root = init_repo("wt");
        std::fs::write(root.join("a.txt"), "v1\n").unwrap();
        let g = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("LC_ALL", "C")
                .output()
                .unwrap()
        };
        assert!(g(&["add", "."]).status.success());
        assert!(g(&["commit", "-qm", "c1"]).status.success());
        // pin → worktree。パスは repo 固定で安定する。
        let base = pin_base(&root).unwrap();
        assert!(!base.is_empty());
        verify_object(&root, &base).unwrap();
        let wt1 = ensure_worktree(&root, &base).unwrap();
        let wt2 = ensure_worktree(&root, &base).unwrap();
        assert_eq!(wt1, wt2, "同一 repo は同一パス");
        assert!(worktree_usable(&wt1));
        assert_eq!(std::fs::read_to_string(wt1.join("a.txt")).unwrap(), "v1\n");
        // base_text 経由でも読める。
        assert_eq!(
            base_text(&root, &base, "a.txt").unwrap().as_deref(),
            Some("v1\n")
        );
        // 撤去。
        remove_worktree(&root, &wt1);
        assert!(!wt1.exists());
        assert!(!worktree_usable(&wt1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_removes_only_dead_sessions() {
        // 確実に死んでいる pid（spawn 直後に wait した子）。
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        let tmp = std::env::temp_dir();
        let dead_dir = tmp.join(format!("mina-base-{dead_pid}-abc"));
        let live_dir = tmp.join(format!("mina-base-{}-abc", std::process::id()));
        std::fs::create_dir_all(&dead_dir).unwrap();
        std::fs::create_dir_all(&live_dir).unwrap();
        prune_stale_worktrees();
        assert!(!dead_dir.exists(), "死 pid の残骸は消える");
        assert!(live_dir.exists(), "自 pid は残る");
        let _ = std::fs::remove_dir_all(&live_dir);
    }

    #[test]
    fn should_unregister_dead_root_cases() {
        // 確実に死んでいる pid。
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id().to_string();
        child.wait().unwrap();
        let me = std::process::id().to_string();
        let tmp = std::env::temp_dir();
        // dir あり・pid 死亡 → true。
        let d = tmp.join(format!("mina-base-{dead}-h"));
        std::fs::create_dir_all(&d).unwrap();
        assert!(should_unregister_dead_root(d.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&d);
        // dir なし・pid 死亡 → true。
        assert!(should_unregister_dead_root(
            tmp.join(format!("mina-base-{dead}-h")).to_str().unwrap()
        ));
        // 自 pid → false。形式外 → false。不正 pid → false。
        assert!(!should_unregister_dead_root(
            tmp.join(format!("mina-base-{me}-h")).to_str().unwrap()
        ));
        assert!(!should_unregister_dead_root("/tmp/other-dir"));
        assert!(!should_unregister_dead_root("/tmp/mina-base-xyz-h"));
        assert!(!should_unregister_dead_root("/tmp/mina-base-h"));
    }

    #[test]
    fn markers() {
        assert_eq!(RowKind::Same.marker(), None);
        assert_eq!(RowKind::Modified.marker(), Some('~'));
        assert_eq!(RowKind::Added.marker(), Some('+'));
        assert_eq!(ChangeStatus::Added.marker(), '+');
        assert_eq!(ChangeStatus::Modified.marker(), '~');
        assert_eq!(ChangeStatus::Deleted.marker(), '-');
    }
}
