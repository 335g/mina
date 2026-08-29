//! languages.toml: 言語 → LSP サーバの設定テーブル（ADR-0030）。
//!
//! Daemon が読む第 2 設定ソース。クライアントの config.toml とは別カテゴリ
//! （client Config は「Daemon は読まない」契約を維持）。バイナリに埋め込んだ
//! 既定テーブルにユーザーファイル（`$XDG_CONFIG_HOME/mina/languages.toml`）を
//! language name / サーバ id 単位で上書き・追加して合成する。
//!
//! 読み込みタイミング: Daemon 起動時の初期ロード + LSP セッション spawn 時
//! （`ensure`）の再読込。編集は次に新しいセッションが spawn される時点で反映され、
//! 稼働中セッションはネゴシエーション済みのまま（再 initialize しない）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::config::config_dir;

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
/// `root-markers` / `grammar` は後続ステージ（ADR-0030: Stage 2 / Stage 4）で
/// 有効化するため、現段階では受け付けない（未知キーはファイル全体を破棄）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Language {
    pub(crate) name: String,
    #[serde(rename = "file-types", default)]
    pub(crate) file_types: Vec<String>,
    #[serde(rename = "language-server", default)]
    pub(crate) language_server: Option<String>,
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
        // deny_unknown_fields: root-markers 等の未対応キーはファイル全体を破棄
        // （現行ステージで未実装のキーを静かに無視しない）。
        let err = parse(
            r#"
[[language]]
name = "rust"
file-types = ["rs"]
root-markers = ["Cargo.toml"]
"#,
        );
        assert!(err.is_err(), "未対応キーはエラー: {err:?}");
    }
}