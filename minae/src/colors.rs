//! Colorscheme: 役割 → スタイルの名前付き写像（ADR-0018）。
//!
//! クライアントローカル（プロトコル非関与）。組み込みスキーム
//! （iceberg-dark / catppuccin-mocha）は実パレットの手合わせ値。ユーザー定義
//! スキームは `~/.config/minae/colorschemes/` の TOML ファイルで、参照された
//! 時のみロードする（遅延ロード — ADR-0022）。解決はファイル優先。
//!
//! 色は [`Color`]（Ansi/Index/Rgb）で保持し、描画時に端末の能力
//! （[`ColorCapability`]）に合わせて [`adapt_color`] で近似してから
//! ratatui の色へ変換する（ADR-0019）。

use std::collections::BTreeMap;
use std::path::Path;

use mina_protocol::HighlightGroup;
use serde::Deserialize;

/// 色の表現。`Ansi(u8)`: 16色インデックス（0-15）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Ansi(u8),
    Index(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// ratatui の色へ変換する（能力近似は呼び出し側で [`adapt_color`] 済みが前提）。
    pub fn to_ratatui(self) -> ratatui::style::Color {
        match self {
            // Ansi 0-15 は Indexed と等価に描画される
            Color::Ansi(n) => ratatui::style::Color::Indexed(n),
            Color::Index(n) => ratatui::style::Color::Indexed(n),
            Color::Rgb(r, g, b) => ratatui::style::Color::Rgb(r, g, b),
        }
    }
}

/// 色リテラルの解釈: `"#rrggbb"` → Rgb、`"ansi:N"` (0-15) → Ansi、`"index:N"` → Index。
fn parse_color(s: &str) -> Option<Color> {
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    if let Some(n) = s.strip_prefix("ansi:") {
        return n.parse::<u8>().ok().filter(|&v| v < 16).map(Color::Ansi);
    }
    if let Some(n) = s.strip_prefix("index:") {
        return n.parse().ok().map(Color::Index);
    }
    None
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_color(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("不正な色リテラル: {s:?}（#rrggbb / ansi:N / index:N）"))
        })
    }
}

/// 1つの役割に対する見た目。TOML スキームファイルでもそのまま使う
/// （`#[serde(default)]` — style テーブルは部分指定。未知キーはエラー）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub underline: bool,
    pub reverse: bool,
    /// ディム。`NO_COLOR` や低色深度でも残る属性で視認できる。
    pub dim: bool,
    /// 斜体。
    pub italic: bool,
}

impl Style {
    /// 組み込みテーブル用の構築ヘルパー。
    const fn fg(color: Color) -> Self {
        Self {
            fg: Some(color),
            bg: None,
            underline: false,
            reverse: false,
            dim: false,
            italic: false,
        }
    }
    const fn fg_underline(color: Color) -> Self {
        Self {
            fg: Some(color),
            bg: None,
            underline: true,
            reverse: false,
            dim: false,
            italic: false,
        }
    }
    const fn bg(color: Color) -> Self {
        Self {
            fg: None,
            bg: Some(color),
            underline: false,
            reverse: false,
            dim: false,
            italic: false,
        }
    }
    /// 後勝ちマージ（None は base を維持、属性は OR）。レイヤー合成で使う。
    fn merged(self, other: Style) -> Style {
        Style {
            fg: other.fg.or(self.fg),
            bg: other.bg.or(self.bg),
            underline: self.underline || other.underline,
            reverse: self.reverse || other.reverse,
            dim: self.dim || other.dim,
            italic: self.italic || other.italic,
        }
    }

    /// ratatui のスタイルへ変換する（`capability` で能力近似済み）。
    pub fn to_ratatui(self, capability: ColorCapability) -> ratatui::style::Style {
        use ratatui::style::Modifier;
        let mut style = ratatui::style::Style::default();
        if let Some(fg) = self.fg {
            style = style.fg(adapt_color(fg, capability).to_ratatui());
        }
        if let Some(bg) = self.bg {
            style = style.bg(adapt_color(bg, capability).to_ratatui());
        }
        let mut modifier = Modifier::empty();
        if self.underline {
            modifier |= Modifier::UNDERLINED;
        }
        if self.reverse {
            modifier |= Modifier::REVERSED;
        }
        if self.dim {
            modifier |= Modifier::DIM;
        }
        if self.italic {
            modifier |= Modifier::ITALIC;
        }
        style.add_modifier(modifier)
    }
}

/// UI ロール（ADR-0018 の taxonomy。診断は重要度ごとに分割）。
/// TOML キーは小文字（serde rename_all）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiRole {
    Cursor,
    Selection,
    DiagnosticError,
    DiagnosticWarning,
    DiagnosticInfo,
    DiagnosticHint,
    StatusLine,
    CommandLine,
    Popup,
    /// ポップアップの枠（box 罫線 ┌─┐│└┘）。前景色だけ境界色。
    PopupBorder,
    /// ステータス行先頭のモードマーカー（Normal / Insert / Select で色分け）。
    ModeNormal,
    ModeInsert,
    ModeSelect,
    /// 非カーソル行の行番号（ガター）。既定: 灰。
    LineNumber,
    /// カーソル行の行番号（ガター）。既定: 白。
    LineNumberActive,
    /// inlay hint の仮想テキスト（既定: ディム + 斜体。MVP では描画しない）。
    InlayHint,
}

/// 役割 → スタイルの写像。未掲載の役割は既定テキスト（無色）。
#[derive(Clone, Debug)]
pub struct Colorscheme {
    pub name: String,
    pub syntax: Vec<(HighlightGroup, Style)>,
    pub ui: Vec<(UiRole, Style)>,
}

impl Colorscheme {
    /// ハイライトグループのスタイル（未掲載は None = 無色）。
    pub fn syntax_style(&self, group: HighlightGroup) -> Option<Style> {
        self.syntax.iter().find(|(g, _)| *g == group).map(|(_, s)| *s)
    }

    /// UI ロールのスタイル（未掲載は None = 無色）。
    pub fn ui_style(&self, role: UiRole) -> Option<Style> {
        self.ui.iter().find(|(r, _)| *r == role).map(|(_, s)| *s)
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// iceberg-dark（既定）。iceberg.vim の実パレットに手合わせ。
fn iceberg_dark() -> Colorscheme {
    use HighlightGroup::*;
    use UiRole::*;
    Colorscheme {
        name: "iceberg-dark".to_string(),
        syntax: vec![
            (Comment, Style::fg(rgb(0x6B, 0x70, 0x89))),
            (Keyword, Style::fg(rgb(0x84, 0xA0, 0xC6))),
            (String, Style::fg(rgb(0xB4, 0xBE, 0x82))),
            (Number, Style::fg(rgb(0xE2, 0xA4, 0x78))),
            (Constant, Style::fg(rgb(0xE2, 0x78, 0x78))),
            (Function, Style::fg(rgb(0x84, 0xA0, 0xC6))),
            (Type, Style::fg(rgb(0xE2, 0xA4, 0x78))),
            (Parameter, Style::fg(rgb(0xA0, 0x93, 0xC7))),
            (Field, Style::fg(rgb(0x89, 0xB8, 0xC2))),
            (Operator, Style::fg(rgb(0x89, 0xB8, 0xC2))),
            (Punctuation, Style::fg(rgb(0xC6, 0xC8, 0xD1))),
            (Attribute, Style::fg(rgb(0xE2, 0xA4, 0x78))),
            (Error, Style::fg_underline(rgb(0xE2, 0x78, 0x78))),
        ],
        ui: vec![
            (Cursor, Style::bg(rgb(0xC6, 0xC8, 0xD1))),
            (Selection, Style::bg(rgb(0x1E, 0x42, 0x62))),
            (DiagnosticError, Style::fg_underline(rgb(0xE2, 0x78, 0x78))),
            (DiagnosticWarning, Style::fg_underline(rgb(0xE2, 0xA4, 0x78))),
            (DiagnosticInfo, Style::fg(rgb(0x84, 0xA0, 0xC6))),
            (DiagnosticHint, Style::fg(rgb(0x6B, 0x70, 0x89))),
            (
                StatusLine,
                Style {
                    fg: Some(rgb(0xC6, 0xC8, 0xD1)),
                    bg: Some(rgb(0x1E, 0x21, 0x32)),
                    ..Style::default()
                },
            ),
            (
                CommandLine,
                Style {
                    fg: Some(rgb(0xC6, 0xC8, 0xD1)),
                    bg: Some(rgb(0x1E, 0x21, 0x32)),
                    ..Style::default()
                },
            ),
            (
                Popup,
                Style {
                    fg: Some(rgb(0xC6, 0xC8, 0xD1)),
                    bg: Some(rgb(0x1E, 0x21, 0x32)),
                    ..Style::default()
                },
            ),
            (PopupBorder, Style::fg(rgb(0x84, 0xA0, 0xC6))),
            (
                ModeNormal,
                Style {
                    fg: Some(rgb(0x16, 0x18, 0x21)),
                    bg: Some(rgb(0x84, 0xA0, 0xC6)),
                    ..Style::default()
                },
            ),
            (
                ModeInsert,
                Style {
                    fg: Some(rgb(0x16, 0x18, 0x21)),
                    bg: Some(rgb(0xB4, 0xBE, 0x82)),
                    ..Style::default()
                },
            ),
            (
                ModeSelect,
                Style {
                    fg: Some(rgb(0x16, 0x18, 0x21)),
                    bg: Some(rgb(0xA0, 0x93, 0xC7)),
                    ..Style::default()
                },
            ),
            (LineNumber, Style::fg(rgb(0x6B, 0x70, 0x89))),
            (LineNumberActive, Style::fg(rgb(0xC6, 0xC8, 0xD1))),
            (
                InlayHint,
                Style {
                    dim: true,
                    italic: true,
                    ..Style::default()
                },
            ),
        ],
    }
}

/// catppuccin-mocha。catppuccin palette の実値に手合わせ。
fn catppuccin_mocha() -> Colorscheme {
    use HighlightGroup::*;
    use UiRole::*;
    Colorscheme {
        name: "catppuccin-mocha".to_string(),
        syntax: vec![
            (Comment, Style::fg(rgb(0x6C, 0x70, 0x86))),
            (Keyword, Style::fg(rgb(0xCB, 0xA6, 0xF7))),
            (String, Style::fg(rgb(0xA6, 0xE3, 0xA1))),
            (Number, Style::fg(rgb(0xFA, 0xB3, 0x87))),
            (Constant, Style::fg(rgb(0xFA, 0xB3, 0x87))),
            (Function, Style::fg(rgb(0x89, 0xB4, 0xFA))),
            (Type, Style::fg(rgb(0xF9, 0xE2, 0xAF))),
            (Parameter, Style::fg(rgb(0xEB, 0xA0, 0xAC))),
            (Field, Style::fg(rgb(0x94, 0xE2, 0xD5))),
            (Operator, Style::fg(rgb(0x89, 0xDC, 0xEB))),
            (Punctuation, Style::fg(rgb(0x93, 0x99, 0xB2))),
            (Attribute, Style::fg(rgb(0xF9, 0xE2, 0xAF))),
            (Error, Style::fg_underline(rgb(0xF3, 0x8B, 0xA8))),
        ],
        ui: vec![
            (Cursor, Style::bg(rgb(0xF5, 0xE0, 0xDC))),
            (Selection, Style::bg(rgb(0x58, 0x5B, 0x70))),
            (DiagnosticError, Style::fg_underline(rgb(0xF3, 0x8B, 0xA8))),
            (DiagnosticWarning, Style::fg_underline(rgb(0xFA, 0xB3, 0x87))),
            (DiagnosticInfo, Style::fg(rgb(0x89, 0xB4, 0xFA))),
            (DiagnosticHint, Style::fg(rgb(0x6C, 0x70, 0x86))),
            (
                StatusLine,
                Style {
                    fg: Some(rgb(0xCD, 0xD6, 0xF4)),
                    bg: Some(rgb(0x31, 0x32, 0x44)),
                    ..Style::default()
                },
            ),
            (
                CommandLine,
                Style {
                    fg: Some(rgb(0xCD, 0xD6, 0xF4)),
                    bg: Some(rgb(0x31, 0x32, 0x44)),
                    ..Style::default()
                },
            ),
            (
                Popup,
                Style {
                    fg: Some(rgb(0xCD, 0xD6, 0xF4)),
                    bg: Some(rgb(0x31, 0x32, 0x44)),
                    ..Style::default()
                },
            ),
            (PopupBorder, Style::fg(rgb(0x89, 0xB4, 0xFA))),
            (
                ModeNormal,
                Style {
                    fg: Some(rgb(0x1E, 0x1E, 0x2E)),
                    bg: Some(rgb(0x89, 0xB4, 0xFA)),
                    ..Style::default()
                },
            ),
            (
                ModeInsert,
                Style {
                    fg: Some(rgb(0x1E, 0x1E, 0x2E)),
                    bg: Some(rgb(0xA6, 0xE3, 0xA1)),
                    ..Style::default()
                },
            ),
            (
                ModeSelect,
                Style {
                    fg: Some(rgb(0x1E, 0x1E, 0x2E)),
                    bg: Some(rgb(0xCB, 0xA6, 0xF7)),
                    ..Style::default()
                },
            ),
            (LineNumber, Style::fg(rgb(0x58, 0x5B, 0x70))),
            (LineNumberActive, Style::fg(rgb(0xCD, 0xD6, 0xF4))),
            (
                InlayHint,
                Style {
                    dim: true,
                    italic: true,
                    ..Style::default()
                },
            ),
        ],
    }
}

/// 組み込みスキームを名前で引く。
fn builtin_by_name(name: &str) -> Option<Colorscheme> {
    match name {
        "iceberg-dark" => Some(iceberg_dark()),
        "catppuccin-mocha" => Some(catppuccin_mocha()),
        _ => None,
    }
}

/// 組み込み名の一覧（`:colorscheme` 補完・サイクル用）。
pub fn builtin_names() -> &'static [&'static str] {
    &["iceberg-dark", "catppuccin-mocha"]
}

/// 既定のスキーム（iceberg-dark）。
pub fn default_scheme() -> Colorscheme {
    iceberg_dark()
}

/// スキームファイルの TOML 形式（ADR-0022）。
/// キーは HighlightGroup / UiRole の小文字名。
/// `name` は表示用メタデータ — 解決キーは常にファイル名（stem）。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemeFile {
    name: Option<String>,
    #[serde(default)]
    syntax: BTreeMap<HighlightGroup, Style>,
    #[serde(default)]
    ui: BTreeMap<UiRole, Style>,
}

/// ベースに上書きを後勝ちマージした表を作る（レイヤー構造）。
/// 未指定ロールはベースの値が残り、上書きの fg/bg は差し替え・属性は OR。
fn layered<R: Copy + PartialEq>(
    base: &[(R, Style)],
    overrides: &[(R, Style)],
) -> Vec<(R, Style)> {
    let mut out = base.to_vec();
    for (group, style) in overrides {
        match out.iter_mut().find(|(g, _)| g == group) {
            Some((_, base_style)) => *base_style = base_style.merged(*style),
            None => out.push((*group, *style)),
        }
    }
    out
}

/// TOML スキームファイルをロードする。構文不正・未知キー・不正な色は Err。
/// 部分指定は既定（iceberg-dark）へのレイヤーとして解釈する。
fn load_scheme_file(path: &Path, stem: &str) -> Result<Colorscheme, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
    let file: SchemeFile = toml::from_str(&text).map_err(|e| e.to_string())?;
    if let Some(name) = &file.name {
        if name != stem {
            eprintln!(
                "warning: colorscheme file {stem}.toml declares name {name:?} but is referenced \
                 as {stem:?}; using the file name"
            );
        }
    }
    let base = default_scheme();
    let syntax: Vec<_> = file.syntax.into_iter().collect();
    let ui: Vec<_> = file.ui.into_iter().collect();
    Ok(Colorscheme {
        name: stem.to_string(),
        syntax: layered(&base.syntax, &syntax),
        ui: layered(&base.ui, &ui),
    })
}

/// 名前からスキームを解決する（遅延ロード — ADR-0022）。
///
/// 1. `schemes_dir/{name}.toml` が存在 → ロード（組み込みを shadow。失敗時は警告して続行）
/// 2. 組み込み名（iceberg-dark / catppuccin-mocha）→ 使用
/// 3. どちらも無ければ None（呼び出し側が警告 + 既定に落とす）
pub fn resolve(name: &str, schemes_dir: &Path) -> Option<Colorscheme> {
    let file = schemes_dir.join(format!("{name}.toml"));
    if file.is_file() {
        return match load_scheme_file(&file, name) {
            Ok(scheme) => Some(scheme),
            Err(e) => {
                eprintln!("warning: failed to load colorscheme {}: {e}", file.display());
                builtin_by_name(name)
            }
        };
    }
    builtin_by_name(name)
}

/// 端末の対応色深度（ADR-0019）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorCapability {
    TrueColor,
    Palette256,
    Ansi16,
}

/// 環境変数から能力と NO_COLOR を検出する（純粋関数 — テスト容易性）。
///
/// 優先順位: `NO_COLOR`（非空）> `COLORTERM`（truecolor|24bit）>
/// `TERM`（"256color" 部分一致）> ANSI16（ADR-0019）。
pub fn detect_capability(
    term: Option<&str>,
    colorterm: Option<&str>,
    no_color: Option<&str>,
) -> (ColorCapability, bool) {
    let no_color = no_color.is_some_and(|v| !v.is_empty());
    if no_color {
        return (ColorCapability::Ansi16, true);
    }
    if colorterm.is_some_and(|v| v == "truecolor" || v == "24bit") {
        return (ColorCapability::TrueColor, false);
    }
    if term.is_some_and(|v| v.contains("256color")) {
        return (ColorCapability::Palette256, false);
    }
    (ColorCapability::Ansi16, false)
}

/// 起動時に環境から検出する（クライアントが 1 回呼ぶ）。
pub fn detect_from_env() -> (ColorCapability, bool) {
    detect_capability(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
        std::env::var("NO_COLOR").ok().as_deref(),
    )
}

/// 16 色の基準 RGB（xterm の既定値。8 標準色 + 8 明るい色）。
const ANSI16_RGB: [(u8, u8, u8); 16] = [
    (0, 0, 0),       // 0 黒
    (205, 0, 0),     // 1 赤
    (0, 205, 0),     // 2 緑
    (205, 205, 0),   // 3 黄
    (0, 0, 238),     // 4 青
    (205, 0, 205),   // 5 マゼンタ
    (0, 205, 205),   // 6 シアン
    (229, 229, 229), // 7 白
    (127, 127, 127), // 8 明るい黒
    (255, 0, 0),     // 9 明るい赤
    (0, 255, 0),     // 10 明るい緑
    (255, 255, 0),   // 11 明るい黄
    (92, 92, 255),   // 12 明るい青
    (255, 0, 255),   // 13 明るいマゼンタ
    (0, 255, 255),   // 14 明るいシアン
    (255, 255, 255), // 15 明るい白
];

/// xterm 256 パレットのキューブ階調（95 刻み）。
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// 256 パレットの RGB を再構成する（0-15 は ANSI16、16-231 はキューブ、232-255 はグレー）。
pub fn index_256_rgb(n: u8) -> (u8, u8, u8) {
    match n {
        0..=15 => ANSI16_RGB[n as usize],
        16..=231 => {
            let n = n - 16;
            (
                CUBE_LEVELS[(n / 36) as usize],
                CUBE_LEVELS[((n % 36) / 6) as usize],
                CUBE_LEVELS[(n % 6) as usize],
            )
        }
        _ => {
            let gray = 8 + (n - 232) * 10;
            (gray, gray, gray)
        }
    }
}

/// RGB を 256 パレットのインデックスへ（ユークリッド距離の最近傍）。
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2) as u32;
    let mut best = 16u8;
    let mut best_d = u32::MAX;
    // キューブ 216 色
    for (ri, &lr) in CUBE_LEVELS.iter().enumerate() {
        for (gi, &lg) in CUBE_LEVELS.iter().enumerate() {
            for (bi, &lb) in CUBE_LEVELS.iter().enumerate() {
                let dist = d(r, lr) + d(g, lg) + d(b, lb);
                if dist < best_d {
                    best_d = dist;
                    best = 16 + (ri * 36 + gi * 6 + bi) as u8;
                }
            }
        }
    }
    // グレー 24 段（8 から 10 刻み）
    for k in 0..24u8 {
        let gray = 8 + k * 10;
        let dist = d(r, gray) + d(g, gray) + d(b, gray);
        if dist < best_d {
            best_d = dist;
            best = 232 + k;
        }
    }
    best
}

/// RGB を 16 色のインデックスへ（xterm 基準 RGB へのユークリッド最近傍）。
pub fn nearest_16(r: u8, g: u8, b: u8) -> u8 {
    let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2) as u32;
    (0..16u8)
        .min_by_key(|&i| {
            let (cr, cg, cb) = ANSI16_RGB[i as usize];
            d(r, cr) + d(g, cg) + d(b, cb)
        })
        .unwrap()
}

/// 色を端末の能力に合わせて変換する（能力が足りなければ近似。ADR-0019）。
pub fn adapt_color(color: Color, capability: ColorCapability) -> Color {
    match capability {
        ColorCapability::TrueColor => color,
        ColorCapability::Palette256 => match color {
            Color::Rgb(r, g, b) => Color::Index(nearest_256(r, g, b)),
            other => other,
        },
        ColorCapability::Ansi16 => match color {
            Color::Rgb(r, g, b) => Color::Ansi(nearest_16(r, g, b)),
            Color::Index(n) => {
                let (r, g, b) = index_256_rgb(n);
                Color::Ansi(nearest_16(r, g, b))
            }
            other => other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_literals_parse() {
        assert_eq!(parse_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("ansi:8"), Some(Color::Ansi(8)));
        assert_eq!(parse_color("index:196"), Some(Color::Index(196)));
        assert_eq!(parse_color("ansi:16"), None, "ansi は 0-15");
        assert_eq!(parse_color("#fff"), None);
        assert_eq!(parse_color("hotpink"), None);
    }

    #[test]
    fn builtins_cover_all_groups_and_key_roles() {
        use HighlightGroup::*;
        for scheme in [iceberg_dark(), catppuccin_mocha()] {
            for group in [
                Comment, Keyword, String, Number, Constant, Function, Type, Parameter, Field,
                Operator, Punctuation, Attribute, Error,
            ] {
                assert!(
                    scheme.syntax_style(group).is_some(),
                    "{} に {:?} がない",
                    scheme.name,
                    group
                );
            }
            for role in [
                UiRole::LineNumber,
                UiRole::StatusLine,
                UiRole::ModeNormal,
                UiRole::ModeInsert,
                UiRole::ModeSelect,
                UiRole::DiagnosticError,
                UiRole::DiagnosticWarning,
                UiRole::DiagnosticInfo,
                UiRole::DiagnosticHint,
                UiRole::PopupBorder,
            ] {
                assert!(
                    scheme.ui_style(role).is_some(),
                    "{} に {:?} がない",
                    scheme.name,
                    role
                );
            }
        }
    }

    #[test]
    fn capability_detection_order() {
        assert_eq!(
            detect_capability(None, None, Some("1")),
            (ColorCapability::Ansi16, true)
        );
        assert_eq!(
            detect_capability(Some("xterm"), Some("truecolor"), None),
            (ColorCapability::TrueColor, false)
        );
        assert_eq!(
            detect_capability(Some("xterm-256color"), None, None),
            (ColorCapability::Palette256, false)
        );
        assert_eq!(
            detect_capability(Some("xterm"), None, None),
            (ColorCapability::Ansi16, false)
        );
    }

    #[test]
    fn adapt_downgrades_rgb() {
        assert_eq!(
            adapt_color(Color::Rgb(255, 0, 0), ColorCapability::TrueColor),
            Color::Rgb(255, 0, 0)
        );
        assert_eq!(
            adapt_color(Color::Rgb(255, 0, 0), ColorCapability::Palette256),
            Color::Index(196)
        );
        assert_eq!(
            adapt_color(Color::Rgb(255, 0, 0), ColorCapability::Ansi16),
            Color::Ansi(9)
        );
    }

    #[test]
    fn scheme_file_layers_over_default() {
        // 部分指定は既定へのレイヤーとして解釈される
        let dir = std::env::temp_dir().join("minae-test-schemes");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("mine.toml"), "[ui.linenumber]\nfg = \"#123456\"\n").unwrap();
        let scheme = resolve("mine", &dir).expect("読める");
        assert_eq!(scheme.name, "mine");
        // 上書きした役割だけ変わり、他は既定が残る
        assert_eq!(
            scheme.ui_style(UiRole::LineNumber),
            Some(Style::fg(Color::Rgb(0x12, 0x34, 0x56)))
        );
        assert!(scheme.ui_style(UiRole::StatusLine).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
