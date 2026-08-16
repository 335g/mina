//! mina のユーザー設定（XDG）。現行キーは `colorscheme` のみ。
//!
//! クライアントローカル — daemon 非関与。起動時に 1 回読む（ADR-0022）。
//! 不在は無言でデフォルト、構文不正は警告してデフォルト（起動を妨げない）。
//! `mina config` サブコマンド（本モジュールの [`ConfigCmd`]）はこのファイルの
//! 確認・編集を担う。書き込みの反映は次回起動時（即時反映なし）。

use std::io;

use clap::Subcommand;
use serde::Deserialize;

/// `config.toml` の内容。未知キーはエラーにする（タイポ検出）— キーを増やす
/// ときはフィールドを足すだけで既存の設定ファイルはそのまま動く。
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 起動時に適用する Colorscheme の名前（組み込み名 or `colorschemes/` のファイル名）。
    #[serde(default)]
    pub colorscheme: Option<String>,
}

/// `mina config` のサブコマンド。
#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Print the contents of the config file
    Show {
        /// Print the effective config (defaults filled in) as TOML
        #[arg(long)]
        effective: bool,
    },
    /// Print the path to the config file
    Path,
    /// Edit keys and values interactively
    Edit,
    /// Write a value to a key (value is a TOML literal; falls back to a string)
    Set {
        /// Key name (known keys only)
        key: String,
        /// Value (TOML literal; treated as a string when it cannot be parsed)
        value: String,
    },
    /// Print a key's value as TOML (exit 1 when unset or unknown)
    Get {
        /// Key name (known keys only)
        key: String,
    },
    /// Create a template config file (existing files are only overwritten with `--force`)
    Init {
        /// Overwrite the existing file
        #[arg(long)]
        force: bool,
    },
}

pub async fn run(cmd: ConfigCmd) -> io::Result<()> {
    match cmd {
        ConfigCmd::Show { effective } => show(effective),
        ConfigCmd::Path => {
            println!("{}", config_path().display());
            Ok(())
        }
        ConfigCmd::Edit => edit().await,
        ConfigCmd::Set { key, value } => set(&key, &value),
        ConfigCmd::Get { key } => get(&key),
        ConfigCmd::Init { force } => init(force),
    }
}

/// 設定ファイルのパス: `$XDG_CONFIG_HOME/mina/config.toml`（なければ `~/.config/mina/config.toml`）。
pub fn config_path() -> std::path::PathBuf {
    config_dir().join("config.toml")
}

/// 既知のキー一覧（`deny_unknown_fields` と同じタイポ検出の思想）。
pub fn known_keys() -> &'static [&'static str] {
    &["colorscheme"]
}

/// `set` の値の解釈: TOML リテラルとして解釈できればその値、
/// できなければ文字列として扱う（`set colorscheme vivid` → `"vivid"`）。
/// 実装は `x = <raw>` の形で検証する（`toml::Value` はドキュメント全体しか
/// パースしないため）。
fn value_literal(raw: &str) -> toml::Value {
    let probe = format!("x = {raw}");
    match toml::from_str::<toml::Table>(&probe) {
        Ok(mut table) => table
            .remove("x")
            .unwrap_or_else(|| toml::Value::String(raw.to_string())),
        Err(_) => toml::Value::String(raw.to_string()),
    }
}

/// 既存の設定テキストにキーを書き込み、TOML に戻す。
/// 既存テキストは [`Config`] として検証する（未知キー・構文不正は拒否）。
/// コメントは保持されない（`toml::Value` 操作の制約）。
fn apply(text: &str, key: &str, value: toml::Value) -> io::Result<String> {
    if !known_keys().contains(&key) {
        return Err(invalid(format!("未知のキー: {key}（既知: {}）", known_keys().join(", "))));
    }
    toml::from_str::<Config>(text)
        .map_err(|e| invalid(format!("設定ファイルの検証に失敗: {e}")))?;
    let mut table: toml::Table = toml::from_str(text)
        .map_err(|e| invalid(format!("設定ファイルの検証に失敗: {e}")))?;
    table.insert(key.to_string(), value);
    let mut out = toml::to_string_pretty(&table)
        .map_err(|e| invalid(format!("設定ファイルの書き込みに失敗: {e}")))?;
    // 書き込んだ結果も Config として検証する（型不一致・未知キーを弾く）
    toml::from_str::<Config>(&out)
        .map_err(|e| invalid(format!("設定ファイルの検証に失敗: {e}")))?;
    out.push('\n');
    Ok(out)
}

fn show(effective: bool) -> io::Result<()> {
    let path = config_path();
    if effective {
        // 実効設定: デフォルト補完済み（未設定キーはコメントで注記）
        print!("{}", effective_toml(&load()));
        return Ok(());
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => print!("{text}"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("設定ファイルはまだありません: {}", path.display());
            eprintln!("hint: `mina config init` で雛形を作成できます");
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// 実効設定を TOML 形式で表示する（未設定キーはコメントで注記）。
fn effective_toml(config: &Config) -> String {
    let mut out = String::from("# 実効設定 (mina config show --effective)\n");
    match &config.colorscheme {
        Some(name) => out.push_str(&format!("colorscheme = \"{name}\"\n")),
        None => out.push_str("# colorscheme = <未設定>\n"),
    }
    out
}

fn set(key: &str, value: &str) -> io::Result<()> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let updated = apply(&text, key, value_literal(value))?;
    ensure_config_dir(&path)?;
    std::fs::write(&path, updated)?;
    println!("{key} = {}", value_literal(value));
    Ok(())
}

fn get(key: &str) -> io::Result<()> {
    if !known_keys().contains(&key) {
        return Err(invalid(format!("未知のキー: {key}（既知: {}）", known_keys().join(", "))));
    }
    let path = config_path();
    let text = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            invalid("設定ファイルがありません（mina config init で作成）")
        } else {
            e
        }
    })?;
    let table: toml::Table = toml::from_str(&text)
        .map_err(|e| invalid(format!("設定ファイルの構文が不正です: {e}")))?;
    match table.get(key) {
        Some(value) => {
            println!("{key} = {value}");
            Ok(())
        }
        None => Err(invalid(format!("{key} は未設定です"))),
    }
}

fn init(force: bool) -> io::Result<()> {
    let path = config_path();
    if path.exists() && !force {
        eprintln!("既に存在します: {}（上書きするには --force）", path.display());
        return Ok(());
    }
    ensure_config_dir(&path)?;
    std::fs::write(&path, TEMPLATE)?;
    println!("作成しました: {}", path.display());
    Ok(())
}

/// 設定ディレクトリを確実に作る（set / edit / init 共通）。
fn ensure_config_dir(path: &std::path::Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// `config init` が書き出す雛形。既知キーと既定値のドキュメントを兼ねる。
pub const TEMPLATE: &str = "# mina 設定ファイル\n\n\
# 既知のキーのみ指定できます（未知キーはタイポとしてエラーになります）。\n\n\
# 起動時に適用する Colorscheme の名前。\n\
# 組み込み名（DEFAULT）または colorschemes/ ディレクトリのファイル名（拡張子なし）。\n\
# colorscheme = \"DEFAULT\"\n";

/// 選択肢として提示できるスキーム名: 組み込み DEFAULT + ユーザーファイル名。
fn available_schemes() -> Vec<String> {
    let mut names = vec!["DEFAULT".to_string()];
    if let Ok(entries) = std::fs::read_dir(schemes_dir()) {
        let mut user: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
            .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        user.sort();
        names.extend(user);
    }
    names
}

/// 対話編集: キーを select → 値を select / カスタム入力 → 書き込み。
async fn edit() -> io::Result<()> {
    use cliclack::{input, outro, outro_cancel, select};

    // 現状の値を先に読む（壊れたファイルなら cliclack を出す前にエラー）
    let path = config_path();
    let current: Option<Config> = match std::fs::read_to_string(&path) {
        Ok(text) => Some(
            toml::from_str(&text)
                .map_err(|e| invalid(format!("設定ファイルの構文が不正です: {e}")))?,
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let current_scheme = current.as_ref().and_then(|c| c.colorscheme.clone());

    let key = select::<&str>("編集するキー")
        .item("colorscheme", "colorscheme", "起動時に適用する Colorscheme 名")
        .interact()?;

    // 値: 利用可能スキーム（組み込み + ユーザーファイル）+ カスタム入力。
    // 現在値が選択肢に含まれていれば initial_value でマークする。
    let custom = "custom";
    let schemes = available_schemes();
    let mut selector = select::<String>("Colorscheme を選択");
    for name in &schemes {
        let mark = if Some(name.as_str()) == current_scheme.as_deref() {
            "（現在）"
        } else {
            ""
        };
        selector = selector.item(name.clone(), format!("{name} {mark}"), "");
    }
    let selected = selector
        .item(
            custom.to_string(),
            "カスタム入力…",
            "組み込みや colorschemes/ のファイル名を直接指定",
        )
        .initial_value(
            current_scheme
                .as_deref()
                .filter(|n| schemes.iter().any(|s| s == n))
                .unwrap_or(custom)
                .to_string(),
        )
        .interact()?;

    let value = if selected == custom {
        let entered = input("Colorscheme 名（カスタム）")
            .placeholder("例: DEFAULT や colorschemes/ のファイル名")
            .default_input(current_scheme.as_deref().unwrap_or("DEFAULT"))
            .interact::<String>()?;
        if entered.trim().is_empty() {
            outro_cancel("空のためキャンセルしました")?;
            return Ok(());
        }
        entered.trim().to_string()
    } else {
        selected
    };

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let updated = apply(&text, key, toml::Value::String(value.clone()))?;
    ensure_config_dir(&path)?;
    std::fs::write(&path, updated)?;
    outro(format!("{key} を \"{value}\" に設定しました"))?;
    Ok(())
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

/// mina の設定ディレクトリ: `$XDG_CONFIG_HOME/mina`、なければ `~/.config/mina`。
pub fn config_dir() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return std::path::PathBuf::from(xdg).join("mina");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return std::path::PathBuf::from(home).join(".config").join("mina");
    }
    // HOME も XDG_CONFIG_HOME もない環境はほぼない。相対パスなら大抵存在せず、
    // 設定なし（デフォルト）に落ちる。
    std::path::PathBuf::from(".config/mina")
}

/// ユーザー Colorscheme ファイルのディレクトリ（`*.toml` をフラットに置く）。
pub fn schemes_dir() -> std::path::PathBuf {
    config_dir().join("colorschemes")
}

/// `config.toml` を読む。不在は無言でデフォルト、読めない・構文不正は警告してデフォルト。
pub fn load() -> Config {
    let path = config_dir().join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    match toml::from_str(&text) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("warning: invalid config file {}: {e}", path.display());
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_missing_config_are_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.colorscheme, None);
        // キー未記入も同じ（Option は None のまま）
        let config: Config = toml::from_str("colorscheme = \"vivid\"").unwrap();
        assert_eq!(config.colorscheme.as_deref(), Some("vivid"));
    }

    #[test]
    fn unknown_key_is_rejected() {
        // deny_unknown_fields: タイポ（colorschme）を黙って無視しない
        assert!(toml::from_str::<Config>("colorschme = \"vivid\"").is_err());
    }

    #[test]
    fn template_is_a_valid_empty_config() {
        // 雛形はコメントのみなので Config::default() と同値でパースできる
        let config: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(config.colorscheme, None);
    }

    #[test]
    fn value_literal_interprets_toml_falling_back_to_string() {
        // 裸の単語は TOML リテラルとして不正 → 文字列
        assert_eq!(value_literal("vivid"), toml::Value::String("vivid".into()));
        // 引用符付きは TOML 文字列リテラルとしてパースされ、引用符が剥がれる
        assert_eq!(
            value_literal("\"vivid\""),
            toml::Value::String("vivid".into())
        );
        // 型付きリテラルはそのまま解釈される（将来の bool/int キー用）
        assert_eq!(value_literal("true"), toml::Value::Boolean(true));
        assert_eq!(value_literal("42"), toml::Value::Integer(42));
    }

    #[test]
    fn apply_writes_and_validates() {
        // 空ファイルへの書き込み
        let out = apply("", "colorscheme", toml::Value::String("vivid".into())).unwrap();
        assert!(out.contains("colorscheme = \"vivid\""), "{out}");
        // 既存値の更新
        let out = apply("colorscheme = \"old\"", "colorscheme", toml::Value::String("new".into())).unwrap();
        assert!(out.contains("colorscheme = \"new\""), "{out}");
        // 未知キーは拒否（タイポ検出）
        assert!(apply("", "colorschme", toml::Value::String("x".into())).is_err());
        // 壊れた既存ファイルは拒否（set で上書きしない）
        assert!(apply("colorscheme = ", "colorscheme", toml::Value::String("x".into())).is_err());
    }

    #[test]
    fn effective_toml_notes_unset_keys() {
        let unset = effective_toml(&Config::default());
        assert!(unset.contains("# colorscheme = <未設定>"), "{unset}");
        let set = effective_toml(&Config {
            colorscheme: Some("vivid".into()),
        });
        assert!(set.contains("colorscheme = \"vivid\""), "{set}");
        assert!(!set.contains("<未設定>"), "{set}");
    }

    #[test]
    fn available_schemes_always_include_default() {
        // ユーザーファイルが無い環境でも DEFAULT は必ず選択肢に載る
        let names = available_schemes();
        assert!(names.iter().any(|n| n == "DEFAULT"), "{names:?}");
    }

    #[test]
    fn known_keys_matches_config_fields() {
        // known_keys の各キーが Config としてパース可能なことを保証（網羅性の目安）
        for key in known_keys() {
            let text = format!("{key} = \"dummy\"");
            assert!(toml::from_str::<Config>(&text).is_ok(), "{key}");
        }
    }
}
