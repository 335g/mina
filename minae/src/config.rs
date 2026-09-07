//! minae のユーザー設定（XDG）。現行キーは `colorscheme` / `agent_command`。
//!
//! クライアントローカル — daemon 非関与。起動時に 1 回読む（ADR-0022）。
//! 不在は無言でデフォルト、構文不正・未知キーは警告してデフォルト
//! （起動を妨げない）。書き込みの反映は次回起動時（即時反映なし）。

use serde::Deserialize;

/// `config.toml` の内容。未知キーはエラーにする（タイポ検出）— キーを増やす
/// ときはフィールドを足すだけで既存の設定ファイルはそのまま動く。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 起動時に適用する Colorscheme の名前（組み込み名 or `colorschemes/` のファイル名）。
    #[serde(default)]
    pub colorscheme: Option<String>,
    /// レビューコメントを投げるエージェントコマンド（#54）。`E` で
    /// `minas review | <agent_command>` を tmux の隣ペインに投げる。
    /// パイプライン全体ではなくエージェント部分だけを書く（抽出の再利用）。
    /// 例: `agent_command = "claude -p"`。未設定なら `E` は案内のみ。
    #[serde(default)]
    pub agent_command: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            colorscheme: None,
            agent_command: None,
        }
    }
}

/// 既知のキー（`colorscheme` + `agent_command` — 他は未知キーとして拒否）。
/// パース時の `deny_unknown_fields` が実効的な拒否機構。
#[allow(dead_code)]
pub fn known_keys() -> &'static [&'static str] {
    &["colorscheme", "agent_command"]
}

/// minae の設定ディレクトリ: `$XDG_CONFIG_HOME/minae`、なければ `~/.config/minae`。
pub fn config_dir() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return std::path::PathBuf::from(xdg).join("minae");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return std::path::PathBuf::from(home).join(".config").join("minae");
    }
    // HOME も XDG_CONFIG_HOME もない環境はほぼない。相対パスなら大抵存在せず、
    // 設定なし（デフォルト）に落ちる。
    std::path::PathBuf::from(".config/minae")
}

/// ユーザー Colorscheme ファイルのディレクトリ（`*.toml` をフラットに置く）。
pub fn schemes_dir() -> std::path::PathBuf {
    config_dir().join("colorschemes")
}

/// `config.toml` を読む。不在は無言でデフォルト、読めない・構文不正・未知キーは
/// 警告してデフォルト。
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
    fn empty_config_is_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.colorscheme, None);
        assert_eq!(config.agent_command, None);
        let config: Config = toml::from_str("colorscheme = \"iceberg-dark\"").unwrap();
        assert_eq!(config.colorscheme.as_deref(), Some("iceberg-dark"));
        let config: Config = toml::from_str("agent_command = \"claude -p\"").unwrap();
        assert_eq!(config.agent_command.as_deref(), Some("claude -p"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // MVP 最小キー以外は未知キーとして拒否される（タイポ検出）
        let err = toml::from_str::<Config>("reset_cursor_on_disconnect = false").unwrap_err();
        assert!(err.to_string().contains("unknown field"));
        for key in known_keys() {
            let toml_text = format!("{key} = \"x\"");
            assert!(
                toml::from_str::<Config>(&toml_text).is_ok(),
                "{key} は既知キー"
            );
        }
    }
}
