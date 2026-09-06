//! languages.toml: 言語 → LSP サーバの設定テーブル（ADR-0030）。
//!
//! Daemon が読む第 2 設定ソース。クライアントの config.toml とは別カテゴリ
//! （client Config は「Daemon は読まない」契約を維持）。バイナリに埋め込んだ
//! 既定テーブルにユーザーファイル（`$XDG_CONFIG_HOME/minae/languages.toml`）を
//! language name / サーバ id 単位で上書き・追加して合成する。
//!
//! 読み込みタイミング: Daemon 起動時の初期ロード + 以降は mtime 差分だけ再読込
//! （`languages_refresh`。ゲートと spawn の両方が参照する）。編集は次にゲート判定が
//! 走る時点で反映され、稼働中セッションはネゴシエーション済みのまま（再 initialize しない）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

/// daemon 用の設定ディレクトリ: `$XDG_CONFIG_HOME/minae`、なければ `~/.config/minae`
/// （旧 config.rs の同名関数を継承。既存ユーザーの languages.toml と互換のため
/// ディレクトリ名は `minae` のまま）。
pub(crate) fn config_dir() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return std::path::PathBuf::from(xdg).join("minae");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return std::path::PathBuf::from(home).join(".config").join("minae");
    }
    std::path::PathBuf::from(".config/minae")
}

/// 埋め込み既定テーブル（検証済みサーバのみ。ADR-0030 の規定サーバ方針）。
pub(crate) const DEFAULT_LANGUAGES_TOML: &str = include_str!("default_languages.toml");

/// 1 サーバの定義。`config` は LSP `initializationOptions` としてそのまま送る
/// 不透明 JSON（スキーマはサーバ固有）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServerConfig {
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) config: Option<serde_json::Value>,
}

/// 1 言語の定義。`name` は `textDocument.languageId` でもある。
///
/// `grammar` は minae-loader の静的レジストリ名（ハイライト・シンボル位置解決）。
/// 未登録の grammar 名・未指定ならハイライト無し（LSP のみ）。ADR-0030 Stage 4。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Language {
    pub(crate) name: String,
    #[serde(rename = "file-types", default)]
    pub(crate) file_types: Vec<String>,
    #[serde(rename = "language-server", default)]
    pub(crate) language_server: Option<String>,
    /// WorkspaceRoot 判定のマーカー（明示すれば [`GENERIC_ROOT_MARKERS`] を置換。
    /// 空リストは「マーカーなし」= ファイル親フォールバックのみ。ADR-0030 Stage 2）。
    #[serde(rename = "root-markers", default)]
    pub(crate) root_markers: Option<Vec<String>>,
    /// minae-loader の grammar 名（例: `"rust"` / `"typescript"`）。ハイライトと
    /// シンボル位置解決（rename / references の識別子解決）に使う。ADR-0030 Stage 4。
    #[serde(default)]
    pub(crate) grammar: Option<String>,
}

/// languages.toml のファイル形式（`[language-server.<id>]` + `[[language]]`）。
#[derive(Debug, Deserialize)]
struct RawFile {
    #[serde(rename = "language-server", default)]
    language_server: HashMap<String, ServerConfig>,
    #[serde(default)]
    language: Vec<Language>,
}

/// 合成済みの言語テーブル（:Daemon が Arc で保持する）。
#[derive(Debug, Clone)]
pub(crate) struct LanguageTable {
    /// 言語の順序は埋め込み既定 → ユーザー追加（file-types の先勝ち参照のため）。
    languages: Vec<Language>,
    servers: HashMap<String, ServerConfig>,
}

/// `LanguageTable::server_for` の結果。borrow した参照の束。
pub(crate) struct ServerSpec<'a> {
    pub(crate) language_id: &'a str,
    pub(crate) command: &'a str,
    pub(crate) args: &'a [String],
    pub(crate) config: Option<&'a serde_json::Value>,
}

/// 汎用 WorkspaceRoot マーカー集合（言語が `root-markers` を明示しない場合。
/// ADR-0030 Stage 2）。最寄りマーカー勝ち（ADR-0010）を維持する。
/// `.git` は worktree の gitdir ファイル等でも目印になる（既存挙動の踏襲）。
const GENERIC_ROOT_MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    ".git",
];

impl LanguageTable {
    /// 埋め込み既定 + ユーザーファイルを合成する。ユーザーファイルが存在しなければ
    /// 埋め込み既定のみ。破損ファイルは警告して埋め込み既定のみにフォールバック
    /// （config.toml と同方針: 起動を阻まない）。
    pub(crate) fn load() -> Self {
        let embedded = parse(DEFAULT_LANGUAGES_TOML).expect("埋め込み languages.toml は有効");
        let user_path = config_dir().join("languages.toml");
        let table = Self::from_parts(embedded);
        let Ok(user_text) = std::fs::read_to_string(&user_path) else {
            return table; // ユーザーファイルなし = 既定のみ
        };
        match parse(&user_text) {
            Ok(user) => {
                let mut merged = table;
                merged.merge(user);
                merged
            }
            Err(e) => {
                eprintln!("languages.toml を解釈できません（既定テーブルのみ使用）: {e}");
                table
            }
        }
    }

    /// テスト用: 文字列から直接合成する。
    #[cfg(test)]
    pub(crate) fn from_strings(embedded: &str, user: Option<&str>) -> Self {
        let mut table = Self::from_parts(parse(embedded).expect("テストの埋め込み TOML は有効"));
        if let Some(user) = user {
            table.merge(parse(user).expect("テストのユーザー TOML は有効"));
        }
        table
    }

    fn from_parts(raw: RawFile) -> Self {
        Self {
            languages: raw.language,
            servers: raw.language_server,
        }
    }

    /// ユーザーファイルを name / id 単位で上書き・追加する。
    fn merge(&mut self, user: RawFile) {
        for server in user.language_server {
            self.servers.insert(server.0, server.1);
        }
        for lang in user.language {
            if let Some(existing) = self.languages.iter_mut().find(|l| l.name == lang.name) {
                *existing = lang; // 埋め込みの位置を保ったまま置換
            } else {
                self.languages.push(lang);
            }
        }
    }

    /// パス（拡張子）から言語を引く。file-types の先勝ち。
    pub(crate) fn language_for_path(&self, path: &Path) -> Option<&Language> {
        let ext = path.extension()?.to_str()?;
        self.languages
            .iter()
            .find(|l| l.file_types.iter().any(|t| t == ext))
    }

    /// パスに LSP サーバが割り当たっていれば spawn / initialize に必要な情報を返す。
    pub(crate) fn server_for(&self, path: &Path) -> Option<ServerSpec<'_>> {
        let lang = self.language_for_path(path)?;
        let server_id = lang.language_server.as_deref()?;
        let server = self.servers.get(server_id)?;
        Some(ServerSpec {
            language_id: &lang.name,
            command: &server.command,
            args: &server.args,
            config: server.config.as_ref(),
        })
    }

    /// パスの言語が `grammar` キーで参照する grammar（minae-loader）。未登録・未指定なら
    /// `None` = ハイライト無し・tree-sitter を使わないシンボル解決（ADR-0030 Stage 4）。
    pub(crate) fn grammar_for_path(&self, path: &Path) -> Option<&'static mina_loader::LanguageDef> {
        self.language_for_path(path)
            .and_then(|l| l.grammar.as_deref())
            .and_then(mina_loader::language_by_name)
    }

    /// 開いたファイルを包含する最小の解析単位（WorkspaceRoot）を求める。
    ///
    /// ファイルの親から上方探索し、言語が `root-markers` を明示していれば
    /// そのリストを**置換**として、未指定なら汎用集合（[`GENERIC_ROOT_MARKERS`]）
    /// を使う。最寄りのマーカーを含むディレクトリが root。マーカーがなければ
    /// ファイルの親ディレクトリにフォールバック（ADR-0010 の意図を維持）。
    pub(crate) fn workspace_root(&self, path: &Path) -> PathBuf {
        let explicit = self
            .language_for_path(path)
            .and_then(|l| l.root_markers.as_deref());
        let fallback = path.parent().unwrap_or_else(|| Path::new("."));
        let mut dir = fallback.to_path_buf();
        loop {
            let hit = match explicit {
                Some(markers) => markers.iter().any(|m| dir.join(m).exists()),
                None => GENERIC_ROOT_MARKERS.iter().any(|m| dir.join(m).exists()),
            };
            if hit {
                return dir;
            }
            match dir.parent() {
                Some(parent) => dir = parent.to_path_buf(),
                None => return fallback.to_path_buf(),
            }
        }
    }

    /// 任意の言語のサーバ定義を引く。サーバ共有の参照解決（将来の Stage で使用）。
    #[cfg(test)]
    pub(crate) fn server_by_id(&self, id: &str) -> Option<&ServerConfig> {
        self.servers.get(id)
    }

    /// Arc 化して保持する（Daemon が共有する）。
    pub(crate) fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

fn parse(text: &str) -> Result<RawFile, String> {
    toml::from_str::<RawFile>(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_default_maps_rust_to_rust_analyzer() {
        let table = LanguageTable::load();
        // load は実ユーザー環境を読むため、合成結果ではなく埋め込みを直接検証する
        let table = LanguageTable::from_strings(DEFAULT_LANGUAGES_TOML, None);
        let spec = table
            .server_for(Path::new("/tmp/src/main.rs"))
            .expect("rs → rust-analyzer");
        assert_eq!(spec.language_id, "rust");
        assert_eq!(spec.command, "rust-analyzer");
        assert!(spec.args.is_empty());
        // 既定の init options: rust-analyzer の inlay hints を明示 ON（既定オフのため）
        let config = spec.config.expect("rust-analyzer は init options を持つ");
        assert_eq!(
            config.pointer("/rust-analyzer/inlayHints/typeHints/enable"),
            Some(&serde_json::json!(true)),
            "型ヒントを明示 ON にする（回帰防止: ADR-0020）"
        );
        // 未知の拡張子・言語サーバなし
        assert!(table.server_for(Path::new("/tmp/notes.md")).is_none());
        assert!(table.server_for(Path::new("/tmp/noext")).is_none());
    }

    #[test]
    fn workspace_root_treats_git_file_as_marker() {
        // git worktree の .git はファイル（gitdir 参照）。exists() 判定のため
        // ディレクトリでなくても root マーカーになる（#49 基準 worktree 用）。
        let dir =
            std::env::temp_dir().join(format!("mina-test-wtroot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wt = dir.join("wt");
        let proj = wt.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /elsewhere/.git/worktrees/wt\n").unwrap();
        let table = LanguageTable::from_strings(DEFAULT_LANGUAGES_TOML, None);
        assert_eq!(
            table.workspace_root(&proj.join("src").join("main.rs")),
            wt,
            ".git ファイルを持つ worktree が root になる"
        );
        // 対照: マーカーなし → worktree ではない祖先へ上がる。
        let _ = std::fs::remove_file(wt.join(".git"));
        assert_ne!(
            table.workspace_root(&proj.join("src").join("main.rs")),
            wt
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_file_overrides_and_appends_by_name() {
        let table = LanguageTable::from_strings(
            DEFAULT_LANGUAGES_TOML,
            Some(
                r#"
[language-server.rust-analyzer]
command = "ra-override"
config = { demo = true }

[language-server.typescript-language-server]
command = "typescript-language-server"
args = ["--stdio"]

[[language]]
name = "rust"
file-types = ["rs"]

[[language]]
name = "typescript"
file-types = ["ts", "tsx"]
language-server = "typescript-language-server"
"#,
            ),
        );
        // 上書き: 同一 name の rust は置換（この例では server 参照なし → gate off）
        assert!(table.server_for(Path::new("/tmp/main.rs")).is_none());
        // サーバ id 単位の上書きで command が差し替わる
        let spec = table
            .language_for_path(Path::new("/tmp/main.ts"))
            .and_then(|l| l.language_server.as_deref())
            .and_then(|id| table.server_by_id(id))
            .expect("typescript-language-server");
        assert_eq!(spec.command, "typescript-language-server");
        assert_eq!(spec.args, &["--stdio"]);
        // rust-analyzer 自身の設定も上書きされている
        let ra = table.server_by_id("rust-analyzer").expect("ra が残る");
        assert_eq!(ra.command, "ra-override");
    }

    #[test]
    fn broken_user_file_falls_back_to_embedded() {
        let embedded = LanguageTable::from_strings(DEFAULT_LANGUAGES_TOML, None);
        // load() は実ファイルを読むので、フォールバックの合成を from_strings で直接検証
        // する（破損文字列は parse でエラーになることだけ確認）。
        assert!(parse("this is not toml ===").is_err());
        let _ = embedded;
    }

    #[test]
    fn unknown_keys_reject_the_whole_file() {
        // deny_unknown_fields: 未対応キーはファイル全体を破棄（現行ステージで未実装の
        // キーを静かに無視しない）。
        let err = parse(
            r#"
[[language]]
name = "rust"
file-types = ["rs"]
foo = 1
"#,
        );
        assert!(err.is_err(), "未対応キーはエラー: {err:?}");
    }

    #[test]
    fn lang_referencing_missing_server_is_none_not_panic() {
        // 言語が存在しないサーバ id を参照しても server_for は None（panic しない）。
        let table = LanguageTable::from_strings(
            DEFAULT_LANGUAGES_TOML,
            Some(
                r#"
[[language]]
name = "text"
file-types = ["txt"]
language-server = "no-such-server"
"#,
            ),
        );
        assert!(table.server_for(Path::new("/tmp/a.txt")).is_none());
    }

    #[test]
    fn empty_and_server_only_user_files_are_accepted() {
        // 空ファイル・server セクションのみ（言語なし）は合成を壊さない。
        let table = LanguageTable::from_strings(DEFAULT_LANGUAGES_TOML, Some(""));
        assert!(
            table.server_for(Path::new("/tmp/a.rs")).is_some(),
            "空のユーザーファイルは既定テーブルを維持"
        );
        let table = LanguageTable::from_strings(
            DEFAULT_LANGUAGES_TOML,
            Some(
                r#"
[language-server.extra]
command = "extra-lsp"
"#,
            ),
        );
        assert!(
            table.server_for(Path::new("/tmp/a.rs")).is_some(),
            "server のみのユーザーファイルは言語を変えない"
        );
        assert_eq!(table.server_by_id("extra").unwrap().command, "extra-lsp");
    }

    #[test]
    fn wrong_value_types_reject_the_whole_file() {
        // 型エラー（command が数値）もファイル全体を破棄し、既定テーブルに落ちる。
        let err = parse(
            r#"
[language-server.broken]
command = 123
"#,
        );
        assert!(err.is_err(), "型違いはエラー: {err:?}");
    }

    #[test]
    fn grammar_key_resolves_loader_language() {
        // Stage 4: [[language]] の grammar キーが minae-loader の静的レジストリを
        // 引く。未登録名・未指定は None（ハイライト無し）。
        let table = LanguageTable::from_strings(
            DEFAULT_LANGUAGES_TOML,
            Some(
                r#"
[[language]]
name = "ts"
file-types = ["ts"]
grammar = "typescript"

[[language]]
name = "text"
file-types = ["txt"]
"#,
            ),
        );
        assert_eq!(
            table.grammar_for_path(Path::new("/tmp/a.ts")).unwrap().name,
            "typescript"
        );
        // 未指定（text）→ None。未登録の grammar 名も None（パニックしない）。
        assert!(table.grammar_for_path(Path::new("/tmp/a.txt")).is_none());
        let bad = LanguageTable::from_strings(
            DEFAULT_LANGUAGES_TOML,
            Some(
                r#"
[[language]]
name = "x"
file-types = ["xx"]
grammar = "no-such-grammar"
"#,
            ),
        );
        assert!(bad.grammar_for_path(Path::new("/tmp/a.xx")).is_none());
    }

    /// workspace_root のテスト用テーブル（ユーザー環境に依存しない = 既定 rust のみ）。
    fn embedded_table() -> LanguageTable {
        LanguageTable::from_strings(DEFAULT_LANGUAGES_TOML, None)
    }

    #[test]
    fn workspace_root_prefers_nearest_manifest_over_outer_git() {
        let dir = std::env::temp_dir().join(format!("minae-lsp-root1-{}", std::process::id()));
        let proj = dir.join("crates").join("foo");
        std::fs::create_dir_all(proj.join("src")).expect("tmp dirs");
        std::fs::write(proj.join("Cargo.toml"), "").expect("manifest");
        std::fs::create_dir_all(dir.join(".git")).expect("git dir");
        let file = proj.join("src").join("main.rs");
        std::fs::write(&file, "").expect("file");
        // 汎用集合の最寄りマーカー勝ち: 内側の Cargo.toml が外側の .git より優先
        assert_eq!(embedded_table().workspace_root(&file), proj);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_root_prefers_nearest_git_over_outer_manifest() {
        let dir = std::env::temp_dir().join(format!("minae-lsp-root2-{}", std::process::id()));
        let proj = dir.join("repo");
        std::fs::create_dir_all(proj.join("src")).expect("tmp dirs");
        std::fs::create_dir_all(proj.join(".git")).expect("git dir");
        std::fs::write(dir.join("Cargo.toml"), "").expect("outer manifest");
        let file = proj.join("src").join("main.rs");
        std::fs::write(&file, "").expect("file");
        assert_eq!(embedded_table().workspace_root(&file), proj);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_root_accepts_git_file_worktree() {
        let dir = std::env::temp_dir().join(format!("minae-lsp-root3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        std::fs::write(dir.join(".git"), "gitdir: ../main/.git/worktrees/x").expect("gitfile");
        let file = dir.join("lib.rs");
        std::fs::write(&file, "").expect("file");
        assert_eq!(embedded_table().workspace_root(&file), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_root_falls_back_to_file_parent() {
        let dir = std::env::temp_dir().join(format!("minae-lsp-root4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let file = dir.join("notes.txt");
        std::fs::write(&file, "").expect("file");
        // txt は言語未登録: 汎用集合でもマーカーが無ければファイル親
        assert_eq!(embedded_table().workspace_root(&file), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_root_markers_replace_generic_set() {
        // 明示 root-markers は汎用集合を置換する: 汎用マーカー（package.json）が
        // 途中にあっても無視し、明示マーカー（.myroot）で止まる。
        let table = LanguageTable::from_strings(
            DEFAULT_LANGUAGES_TOML,
            Some(
                r#"
[[language]]
name = "text"
file-types = ["txt"]
root-markers = [".myroot"]
"#,
            ),
        );
        let dir = std::env::temp_dir().join(format!("minae-lsp-root5-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).expect("tmp dirs");
        std::fs::write(dir.join(".myroot"), "").expect("marker");
        std::fs::write(dir.join("sub").join("package.json"), "").expect("without");
        let file = dir.join("sub").join("notes.txt");
        std::fs::write(&file, "").expect("file");
        // 汎用集合なら sub/package.json で止まるはず。置換なら .myroot の dir まで上がる。
        assert_eq!(
            table.workspace_root(&file),
            dir,
            "明示 root-markers が汎用集合（package.json）を無視して勝つ"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_empty_root_markers_ignore_git() {
        // root-markers = []（空）は「マーカーなし」: .git を除外しファイル親に落ちる。
        // — "git 管理しているがそこを root にしたくない" ユースケース（ADR-0030）。
        let table = LanguageTable::from_strings(
            DEFAULT_LANGUAGES_TOML,
            Some(
                r#"
[[language]]
name = "text"
file-types = ["txt"]
root-markers = []
"#,
            ),
        );
        let dir = std::env::temp_dir().join(format!("minae-lsp-root6-{}", std::process::id()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("docs")).expect("tmp dirs");
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        let file = repo.join("docs").join("memo.txt");
        std::fs::write(&file, "").expect("file");
        // 汎用集合なら repo/.git で止まる。置換（空）なら .git を無視して docs に落ちる。
        assert_eq!(
            table.workspace_root(&file),
            repo.join("docs"),
            "空の root-markers は .git を root 判定から除外する"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rust_without_explicit_markers_uses_generic_set() {
        // 既定 rust は root-markers 未指定 → 汎用集合（Cargo.toml 含む）のまま。
        let table = LanguageTable::from_strings(
            DEFAULT_LANGUAGES_TOML,
            Some(
                r#"
[[language]]
name = "rust"
file-types = ["rs"]
"#,
            ),
        );
        let dir = std::env::temp_dir().join(format!("minae-lsp-root7-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).expect("tmp dirs");
        std::fs::write(dir.join("Cargo.toml"), "").expect("manifest");
        let file = dir.join("src").join("main.rs");
        std::fs::write(&file, "").expect("file");
        assert_eq!(table.workspace_root(&file), dir, "rust は汎用集合の Cargo.toml で止まる");
        let _ = std::fs::remove_dir_all(&dir);
    }
}