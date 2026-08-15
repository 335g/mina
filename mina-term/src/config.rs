//! mina のユーザー設定（XDG）。現行キーは `colorscheme` のみ。
//!
//! クライアントローカル — daemon 非関与。起動時に 1 回読む（ADR-0022）。
//! 不在は無言でデフォルト、構文不正は警告してデフォルト（起動を妨げない）。

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
}
