//! Colorscheme: 役割 → スタイルの名前付き写像 (ADR-0018, CONTEXT.md)。
//!
//! クライアントローカル (プロトコル非関与)。スキームは静的 const テーブルで、
//! 設定ファイル・永続化はない (#19 で名前付きレジストリと切替が入る)。

use mina_protocol::HighlightGroup;

/// 色の表現。既定スキームは Ansi のみ使用 (Rgb は #20 の色能力検出で使う)。
/// `Ansi(u8)`: 16色インデックス (0-15)。
#[allow(dead_code)] // Index/Rgb は #20 (色能力検出) で使用
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Ansi(u8),
    Index(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// 前景 SGR コード。Ansi16: `30-37` / `90-97`、256: `38;5;N`、truecolor: `38;2;r;g;b`。
    pub fn fg_sgr(self) -> String {
        match self {
            Color::Ansi(n) if n < 8 => format!("{}", 30 + n),
            Color::Ansi(n) => format!("{}", 90 + n - 8),
            Color::Index(n) => format!("38;5;{n}"),
            Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
        }
    }

    /// 背景 SGR コード。Ansi16: `40-47` / `100-107`、256: `48;5;N`、truecolor: `48;2;r;g;b`。
    pub fn bg_sgr(self) -> String {
        match self {
            Color::Ansi(n) if n < 8 => format!("{}", 40 + n),
            Color::Ansi(n) => format!("{}", 100 + n - 8),
            Color::Index(n) => format!("48;5;{n}"),
            Color::Rgb(r, g, b) => format!("48;2;{r};{g};{b}"),
        }
    }
}

/// 1つの役割に対する見た目。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub underline: bool,
    pub reverse: bool,
}

impl Style {
    /// const 構築ヘルパー（`static` テーブル用）。
    pub const fn new() -> Self {
        Self { fg: None, bg: None, underline: false, reverse: false }
    }
    /// 後勝ちマージ（None は base を維持、属性は OR）。レンダラの優先順位合成で使う。
    pub(crate) fn merged(self, other: Style) -> Style {
        Style {
            fg: other.fg.or(self.fg),
            bg: other.bg.or(self.bg),
            underline: self.underline || other.underline,
            reverse: self.reverse || other.reverse,
        }
    }
    const fn fg(color: Color) -> Self {
        Self { fg: Some(color), ..Self::new() }
    }
    const fn fg_underline(color: Color) -> Self {
        Self { fg: Some(color), underline: true, ..Self::new() }
    }
    const fn bg(color: Color) -> Self {
        Self { bg: Some(color), ..Self::new() }
    }
    const fn reverse() -> Self {
        Self { reverse: true, ..Self::new() }
    }
}

/// UI ロール (ADR-0018 の taxonomy。診断は Error/Warning に分割)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiRole {
    Cursor,
    Selection,
    DiagnosticError,
    DiagnosticWarning,
    StatusLine,
    CommandLine,
    Popup,
}

/// 役割 → スタイルの写像。未掲載の役割は既定テキスト (無色)。
pub struct Colorscheme {
    pub name: &'static str,
    pub syntax: &'static [(HighlightGroup, Style)],
    pub ui: &'static [(UiRole, Style)],
}

impl Colorscheme {
    /// ハイライトグループのスタイル（未掲載は None = 無色）。
    pub fn syntax_style(&self, group: HighlightGroup) -> Option<Style> {
        self.syntax.iter().find(|(g, _)| *g == group).map(|(_, s)| *s)
    }

    /// UI ロールのスタイル（未掲載は None = 既定）。
    pub fn ui_style(&self, role: UiRole) -> Option<Style> {
        self.ui.iter().find(|(r, _)| *r == role).map(|(_, s)| *s)
    }
}

/// 既定スキーム。M3 までのハードコード SGR をそのままデータ化し、
/// 診断 Error/Warning の色分けを加えたもの (issue #18)。
/// UI ロールの既定スタイル（DEFAULT と VIVID で共用 — 切替の実証は構文色で行う）。
const DEFAULT_UI: &[(UiRole, Style)] = &[
    (UiRole::Cursor, Style::bg(Color::Ansi(4))), // 44 青背景
    (UiRole::Selection, Style::reverse()), // 7
    (UiRole::DiagnosticError, Style::fg_underline(Color::Ansi(9))), // 4;91
    (UiRole::DiagnosticWarning, Style::fg_underline(Color::Ansi(11))), // 4;93
    (UiRole::StatusLine, Style::reverse()),
    (UiRole::CommandLine, Style::reverse()),
    (UiRole::Popup, Style::reverse()),
];

pub static DEFAULT: Colorscheme = Colorscheme {
    name: "default",
    syntax: &[
        (HighlightGroup::Comment, Style::fg(Color::Ansi(8))), // 90
        (HighlightGroup::Keyword, Style::fg(Color::Ansi(6))), // 36
        (HighlightGroup::String, Style::fg(Color::Ansi(2))), // 32
        (HighlightGroup::Number, Style::fg(Color::Ansi(3))), // 33
        (HighlightGroup::Constant, Style::fg(Color::Ansi(5))), // 35
        (HighlightGroup::Function, Style::fg(Color::Ansi(4))), // 34
        (HighlightGroup::Type, Style::fg(Color::Ansi(14))), // 96
        (HighlightGroup::Field, Style::fg(Color::Ansi(12))), // 94
        (HighlightGroup::Attribute, Style::fg(Color::Ansi(13))), // 95
        (HighlightGroup::Error, Style::fg_underline(Color::Ansi(9))), // 91 + 下線
        // parameter / operator / punctuation は無色のまま
    ],
    ui: DEFAULT_UI,
};

/// DEFAULT と異なる配色の軽量スキーム（`:colorscheme vivid` で切替を実証）。ANSI16 のみ。
pub static VIVID: Colorscheme = Colorscheme {
    name: "vivid",
    syntax: &[
        (HighlightGroup::Comment, Style::fg(Color::Ansi(2))), // 32 緑
        (HighlightGroup::Keyword, Style::fg(Color::Ansi(5))), // 35 マゼンタ
        (HighlightGroup::String, Style::fg(Color::Ansi(6))), // 36 シアン
        (HighlightGroup::Number, Style::fg(Color::Ansi(11))), // 93 明るい黄
        (HighlightGroup::Constant, Style::fg(Color::Ansi(1))), // 31 赤
        (HighlightGroup::Function, Style::fg(Color::Ansi(14))), // 96 明るいシアン
        (HighlightGroup::Type, Style::fg(Color::Ansi(4))), // 34 青
        (HighlightGroup::Field, Style::fg(Color::Ansi(3))), // 33 黄
        (HighlightGroup::Attribute, Style::fg(Color::Ansi(13))), // 95 明るいマゼンタ
        (HighlightGroup::Error, Style::fg_underline(Color::Ansi(9))), // 91 + 下線
    ],
    ui: DEFAULT_UI,
};

/// 名前付きスキームの静的レジストリ（設定ファイル・永続化なし — issue #19）。
static SCHEMES: &[&Colorscheme] = &[&DEFAULT, &VIVID];

/// 名前からスキームを引く（未登録名は None）。
pub fn scheme_by_name(name: &str) -> Option<&'static Colorscheme> {
    SCHEMES.iter().copied().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_syntax_style_matches_provisional_palette() {
        assert_eq!(
            DEFAULT.syntax_style(HighlightGroup::String),
            Some(Style::fg(Color::Ansi(2)))
        );
        assert_eq!(
            DEFAULT.syntax_style(HighlightGroup::Error),
            Some(Style::fg_underline(Color::Ansi(9)))
        );
        assert_eq!(
            DEFAULT.syntax_style(HighlightGroup::Parameter),
            None,
            "parameter は無色"
        );
    }

    #[test]
    fn default_ui_roles() {
        assert_eq!(DEFAULT.ui_style(UiRole::Cursor), Some(Style::bg(Color::Ansi(4))));
        assert_eq!(DEFAULT.ui_style(UiRole::Selection), Some(Style::reverse()));
        assert_eq!(
            DEFAULT.ui_style(UiRole::DiagnosticError),
            Some(Style::fg_underline(Color::Ansi(9)))
        );
        assert_eq!(
            DEFAULT.ui_style(UiRole::DiagnosticWarning),
            Some(Style::fg_underline(Color::Ansi(11)))
        );
    }

    #[test]
    fn color_sgr_conversion() {
        assert_eq!(Color::Ansi(4).bg_sgr(), "44", "青背景");
        assert_eq!(Color::Ansi(8).fg_sgr(), "90", "明るい黒");
        assert_eq!(Color::Ansi(9).fg_sgr(), "91", "明るい赤");
        assert_eq!(Color::Ansi(14).fg_sgr(), "96", "明るいシアン");
        assert_eq!(Color::Ansi(6).fg_sgr(), "36");
        assert_eq!(Color::Index(196).fg_sgr(), "38;5;196");
        assert_eq!(Color::Rgb(255, 0, 0).fg_sgr(), "38;2;255;0;0");
    }

    #[test]
    fn scheme_by_name_resolves_registry() {
        assert_eq!(scheme_by_name("default").map(|s| s.name), Some("default"));
        assert_eq!(scheme_by_name("vivid").map(|s| s.name), Some("vivid"));
        assert!(scheme_by_name("nope").is_none());
        assert!(scheme_by_name("").is_none());
    }

    #[test]
    fn vivid_differs_from_default_on_groups() {
        // 切替が実証できるよう、構文色が DEFAULT と異なること
        assert_ne!(
            DEFAULT.syntax_style(HighlightGroup::Keyword),
            VIVID.syntax_style(HighlightGroup::Keyword)
        );
        assert_eq!(
            VIVID.syntax_style(HighlightGroup::Keyword),
            Some(Style::fg(Color::Ansi(5))) // 35 マゼンタ
        );
        // UI ロールは共有（切替の実証は構文色で行う）
        assert_eq!(DEFAULT.ui_style(UiRole::Cursor), VIVID.ui_style(UiRole::Cursor));
    }
}
