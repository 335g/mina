//! Colorscheme: 役割 → スタイルの名前付き写像 (ADR-0018, CONTEXT.md)。
//!
//! クライアントローカル (プロトコル非関与)。組み込みスキームは静的 const テーブル、
//! ユーザー定義スキームは XDG の colorschemes/ にある TOML ファイルで、参照された時
//! のみロードする (遅延ロード — ADR-0022)。解決はファイル優先: 同名のユーザーファイルが
//! 組み込みを shadow する。

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

use mina_protocol::HighlightGroup;
use serde::Deserialize;

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
    /// 不正な色リテラルはファイル全体のロード失敗にする（黙って無視しない）。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_color(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid color {s:?} (expected #rrggbb, ansi:N, or index:N)"
            ))
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
    /// ディム（SGR 2）。`NO_COLOR` や低色深度でも残る属性で視認できる。
    pub dim: bool,
    /// 斜体（SGR 3）。
    pub italic: bool,
}

impl Style {
    /// const 構築ヘルパー（`static` テーブル用）。
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            underline: false,
            reverse: false,
            dim: false,
            italic: false,
        }
    }
    /// 後勝ちマージ（None は base を維持、属性は OR）。レンダラの優先順位合成で使う。
    pub(crate) fn merged(self, other: Style) -> Style {
        Style {
            fg: other.fg.or(self.fg),
            bg: other.bg.or(self.bg),
            underline: self.underline || other.underline,
            reverse: self.reverse || other.reverse,
            dim: self.dim || other.dim,
            italic: self.italic || other.italic,
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
/// TOML キーは小文字（serde rename_all）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiRole {
    Cursor,
    Selection,
    DiagnosticError,
    DiagnosticWarning,
    StatusLine,
    CommandLine,
    Popup,
    /// ポップアップの枠（box 罫線 ┌─┐│└┘）。前景色だけ境界色、背景は Popup に
    /// 揃える（ADR-0024）。ポップアップ背景が端末背景と同化して見切れるのを防ぐ。
    PopupBorder,
    /// ステータス行先頭のモードマーカー（Normal / Insert / Select で色分け）。
    ModeNormal,
    ModeInsert,
    ModeSelect,
    /// 非カーソル行の行番号（ガター）。既定: 灰。
    LineNumber,
    /// カーソル行の行番号（ガター）。既定: 白。
    LineNumberActive,
    /// inlay hint の仮想テキスト（ADR-0020。既定: ディム + 斜体）。
    InlayHint,
}

/// 役割 → スタイルの写像。未掲載の役割は既定テキスト (無色)。
///
/// 組み込みスキームは borrowed (`Cow::Borrowed`) の静的データ、ユーザー定義
/// スキームはファイルからロードした owned データ (ADR-0022)。
#[derive(Clone)]
pub struct Colorscheme {
    pub name: Cow<'static, str>,
    pub syntax: Cow<'static, [(HighlightGroup, Style)]>,
    pub ui: Cow<'static, [(UiRole, Style)]>,
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
    // ポップアップ（peek 定義表示・外部削除通知）。reverse（白反転）だと背景が
    // 白く浮くため、暗い端末上で「ウィンドウ」として際立つ明示的な bg+fg にする
    // （ADR-0024）。ユーザーはスキームの `[ui] popup` で上書きできる。
    (UiRole::Popup, Style { fg: Some(Color::Ansi(15)), bg: Some(Color::Ansi(8)), ..Style::new() }),
    // ポップアップの枠線。暗い背景でも見えるシアン（浅い色深度でも視認可）。
    (UiRole::PopupBorder, Style::fg(Color::Ansi(6))),
    // モードマーカー: 黒文字 + 明るい背景（反転チップの見た目を維持しつつ色分け）
    (UiRole::ModeNormal, Style { fg: Some(Color::Ansi(0)), bg: Some(Color::Ansi(12)), ..Style::new() }), // 30;104
    (UiRole::ModeInsert, Style { fg: Some(Color::Ansi(0)), bg: Some(Color::Ansi(10)), ..Style::new() }), // 30;102
    (UiRole::ModeSelect, Style { fg: Some(Color::Ansi(0)), bg: Some(Color::Ansi(13)), ..Style::new() }), // 30;105
    (UiRole::LineNumber, Style::fg(Color::Ansi(8))), // 90 灰
    (UiRole::LineNumberActive, Style::fg(Color::Ansi(15))), // 97 白
    // ディム + 斜体: 属性のみ（NO_COLOR・ANSI16 でも視認できる）。
    // ロールの既定はここで持ち、ユーザースキームの ui テーブルでレイヤー上書きできる (ADR-0022)。
    (UiRole::InlayHint, Style { dim: true, italic: true, ..Style::new() }),
];

pub static DEFAULT: Colorscheme = Colorscheme {
    name: Cow::Borrowed("default"),
    syntax: Cow::Borrowed(&[
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
    ]),
    ui: Cow::Borrowed(DEFAULT_UI),
};

/// DEFAULT と異なる配色の軽量スキーム（`:colorscheme vivid` で切替を実証）。ANSI16 のみ。
pub static VIVID: Colorscheme = Colorscheme {
    name: Cow::Borrowed("vivid"),
    // 16 色端末では近似が元の ANSI16 色に一致するよう、xterm 16 色の RGB 値を選ぶ
    // （truecolor 端末ではより豊かな色になる — issue #20）。
    syntax: Cow::Borrowed(&[
        (HighlightGroup::Comment, Style::fg(Color::Rgb(0, 205, 0))), // 32 緑
        (HighlightGroup::Keyword, Style::fg(Color::Rgb(205, 0, 205))), // 35 マゼンタ
        (HighlightGroup::String, Style::fg(Color::Rgb(0, 205, 205))), // 36 シアン
        (HighlightGroup::Number, Style::fg(Color::Rgb(255, 255, 0))), // 93 明るい黄
        (HighlightGroup::Constant, Style::fg(Color::Rgb(205, 0, 0))), // 31 赤
        (HighlightGroup::Function, Style::fg(Color::Rgb(0, 255, 255))), // 96 明るいシアン
        (HighlightGroup::Type, Style::fg(Color::Rgb(0, 0, 238))), // 34 青
        (HighlightGroup::Field, Style::fg(Color::Rgb(205, 205, 0))), // 33 黄
        (HighlightGroup::Attribute, Style::fg(Color::Rgb(255, 0, 255))), // 95 明るいマゼンタ
        (HighlightGroup::Error, Style::fg_underline(Color::Rgb(255, 0, 0))), // 91 + 下線
    ]),
    ui: Cow::Borrowed(DEFAULT_UI),
};

/// 組み込みスキームを名前で引く（borrowed データの clone — 実体は静的なのでほぼ無コスト）。
fn builtin_by_name(name: &str) -> Option<Colorscheme> {
    match name {
        "default" => Some(DEFAULT.clone()),
        "vivid" => Some(VIVID.clone()),
        _ => None,
    }
}

/// スキームファイルの TOML 形式 (ADR-0022)。
/// キーは HighlightGroup / UiRole の小文字名（serde rename_all）。
/// `name` は表示用メタデータ — 解決キーは常にファイル名 (stem) のため、
/// 一致しない場合は警告するだけ（遅延ロードでは名前表が作れない）。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemeFile {
    name: Option<String>,
    #[serde(default)]
    syntax: BTreeMap<HighlightGroup, Style>,
    #[serde(default)]
    ui: BTreeMap<UiRole, Style>,
}

/// ベース（DEFAULT）に上書きを後勝ちマージした表を作る（レイヤー構造）。
/// 未指定ロールは DEFAULT の値が残り、上書きの fg/bg は差し替え・属性は OR。
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
    let syntax: Vec<_> = file.syntax.into_iter().collect();
    let ui: Vec<_> = file.ui.into_iter().collect();
    Ok(Colorscheme {
        name: Cow::Owned(stem.to_string()),
        syntax: Cow::Owned(layered(&DEFAULT.syntax, &syntax)),
        ui: Cow::Owned(layered(&DEFAULT.ui, &ui)),
    })
}

/// 名前からスキームを解決する（遅延ロード — ADR-0022）。
///
/// 1. `schemes_dir/{name}.toml` が存在 → ロード（組み込みを shadow。失敗時は警告して続行）
/// 2. 組み込み名（default / vivid）→ 使用
/// 3. どちらも無ければ None（呼び出し側が警告 + DEFAULT に落とす）
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

/// 端末の対応色深度 (ADR-0019)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorCapability {
    TrueColor,
    Palette256,
    Ansi16,
}

/// 環境変数から能力と NO_COLOR を検出する（純粋関数 — テスト容易性）。
///
/// 優先順位: `NO_COLOR` (非空) > `COLORTERM` (truecolor|24bit) >
/// `TERM` ("256color" 部分一致) > ANSI16 (ADR-0019)。
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
    (0, 0, 0), // 0 黒
    (205, 0, 0), // 1 赤
    (0, 205, 0), // 2 緑
    (205, 205, 0), // 3 黄
    (0, 0, 238), // 4 青
    (205, 0, 205), // 5 マゼンタ
    (0, 205, 205), // 6 シアン
    (229, 229, 229), // 7 白
    (127, 127, 127), // 8 明るい黒
    (255, 0, 0), // 9 明るい赤
    (0, 255, 0), // 10 明るい緑
    (255, 255, 0), // 11 明るい黄
    (92, 92, 255), // 12 明るい青
    (255, 0, 255), // 13 明るいマゼンタ
    (0, 255, 255), // 14 明るいシアン
    (255, 255, 255), // 15 明るい白
];

/// xterm 256 パレットのキューブ階調（95 刻み。実装前に誤った 51 刻みを
/// 提示したが、実際の xterm はこちら — ADR-0019）。
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// 256 パレットの RGB を再構成する（0-15 は ANSI16、16-231 はキューブ、232-255 はグレー）。
pub fn index_256_rgb(n: u8) -> (u8, u8, u8) {
    match n {
        0..=15 => ANSI16_RGB[n as usize],
        16..=231 => {
            let k = n - 16;
            let (r, g, b) = (k / 36, (k % 36) / 6, k % 6);
            (
                CUBE_LEVELS[r as usize],
                CUBE_LEVELS[g as usize],
                CUBE_LEVELS[b as usize],
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
        assert_eq!(DEFAULT.ui_style(UiRole::LineNumber), Some(Style::fg(Color::Ansi(8))));
        assert_eq!(DEFAULT.ui_style(UiRole::LineNumberActive), Some(Style::fg(Color::Ansi(15))));
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

    /// テスト用の独立した一時ディレクトリ（テスト間で衝突しない）。
    fn temp_schemes_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mina-cs-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_builtins_by_name() {
        let dir = temp_schemes_dir("builtin");
        assert_eq!(resolve("default", &dir).unwrap().name, "default");
        assert_eq!(resolve("vivid", &dir).unwrap().name, "vivid");
        assert!(resolve("nope", &dir).is_none());
        assert!(resolve("", &dir).is_none());
    }

    #[test]
    fn resolve_prefers_user_file_over_builtin() {
        // 組み込みと同名のユーザーファイルが shadow する (ADR-0022)
        let dir = temp_schemes_dir("shadow");
        std::fs::write(
            dir.join("vivid.toml"),
            "[syntax]\nkeyword = { fg = \"#123456\" }\n",
        )
        .unwrap();
        let scheme = resolve("vivid", &dir).unwrap();
        assert_eq!(scheme.name, "vivid");
        assert_eq!(
            scheme.syntax_style(HighlightGroup::Keyword),
            Some(Style::fg(Color::Rgb(0x12, 0x34, 0x56)))
        );
    }

    #[test]
    fn user_scheme_layers_over_default() {
        // [syntax] だけのファイル: 未指定ロールは DEFAULT の値（レイヤー構造）
        let dir = temp_schemes_dir("layer");
        std::fs::write(
            dir.join("moon.toml"),
            "[syntax]\ncomment = { fg = \"#ff0000\" }\n",
        )
        .unwrap();
        let scheme = resolve("moon", &dir).unwrap();
        assert_eq!(scheme.name, "moon");
        assert_eq!(
            scheme.syntax_style(HighlightGroup::Comment),
            Some(Style::fg(Color::Rgb(255, 0, 0)))
        );
        // 未指定の構文グループと UI ロールは DEFAULT を継承
        assert_eq!(
            scheme.syntax_style(HighlightGroup::Keyword),
            DEFAULT.syntax_style(HighlightGroup::Keyword)
        );
        assert_eq!(scheme.ui_style(UiRole::Cursor), DEFAULT.ui_style(UiRole::Cursor));
    }

    #[test]
    fn name_field_is_metadata_and_stem_wins() {
        // name フィールドは解決キーにならない（遅延ロード）。不一致は警告のみ。
        let dir = temp_schemes_dir("namefield");
        std::fs::write(dir.join("moon.toml"), "name = \"sun\"\n[syntax]\n").unwrap();
        let scheme = resolve("moon", &dir).unwrap();
        assert_eq!(scheme.name, "moon");
    }

    #[test]
    fn malformed_user_file_falls_back_to_builtin() {
        // TOML 構文不正・未知グループ名・不正な色 → 警告 + その名前の組み込みに
        // フォールバック（組み込みも無ければ None）
        let dir = temp_schemes_dir("malformed");
        std::fs::write(dir.join("vivid.toml"), "not [ valid toml").unwrap();
        assert_eq!(resolve("vivid", &dir).unwrap().name, "vivid");

        std::fs::write(dir.join("badgroup.toml"), "[syntax]\ncommentz = {}\n").unwrap();
        assert!(resolve("badgroup", &dir).is_none());

        std::fs::write(
            dir.join("badcolor.toml"),
            "[syntax]\nkeyword = { fg = \"hotpink\" }\n",
        )
        .unwrap();
        assert!(resolve("badcolor", &dir).is_none());
    }

    #[test]
    fn color_literal_parsing() {
        assert_eq!(parse_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("ansi:8"), Some(Color::Ansi(8)));
        assert_eq!(parse_color("index:196"), Some(Color::Index(196)));
        assert_eq!(parse_color("ansi:16"), None, "ansi は 0-15");
        assert_eq!(parse_color("#fff"), None);
        assert_eq!(parse_color("hotpink"), None);
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
            Some(Style::fg(Color::Rgb(205, 0, 205))) // マゼンタ (16 色近似で 35)
        );
        // UI ロールは共有（切替の実証は構文色で行う）
        assert_eq!(DEFAULT.ui_style(UiRole::Cursor), VIVID.ui_style(UiRole::Cursor));
    }

    #[test]
    fn detect_capability_precedence() {
        // NO_COLOR (非空) が最優先
        assert_eq!(
            detect_capability(Some("xterm-256color"), Some("truecolor"), Some("1")),
            (ColorCapability::Ansi16, true)
        );
        // 空の NO_COLOR は無効
        assert_eq!(
            detect_capability(Some("xterm-256color"), Some("truecolor"), Some("")),
            (ColorCapability::TrueColor, false)
        );
        assert_eq!(
            detect_capability(Some("xterm-256color"), Some("truecolor"), None),
            (ColorCapability::TrueColor, false)
        );
        // COLORTERM: truecolor / 24bit
        assert_eq!(
            detect_capability(Some("xterm"), Some("truecolor"), None),
            (ColorCapability::TrueColor, false)
        );
        assert_eq!(
            detect_capability(Some("xterm"), Some("24bit"), None),
            (ColorCapability::TrueColor, false)
        );
        // 未知の COLORTERM 値は無視して TERM 判定へ
        assert_eq!(
            detect_capability(Some("xterm-256color"), Some("1"), None),
            (ColorCapability::Palette256, false)
        );
        // TERM: 256color 部分一致
        assert_eq!(
            detect_capability(Some("screen-256color"), None, None),
            (ColorCapability::Palette256, false)
        );
        assert_eq!(
            detect_capability(Some("tmux-256color"), None, None),
            (ColorCapability::Palette256, false)
        );
        // それ以外は ANSI16
        assert_eq!(
            detect_capability(Some("xterm"), None, None),
            (ColorCapability::Ansi16, false)
        );
        assert_eq!(detect_capability(None, None, None), (ColorCapability::Ansi16, false));
    }

    #[test]
    fn rgb_to_256_conversion() {
        assert_eq!(nearest_256(255, 0, 0), 196, "赤のキューブ代表");
        assert_eq!(nearest_256(0, 0, 0), 16, "黒");
        assert_eq!(nearest_256(128, 128, 128), 244, "グレー最近傍 (128)");
        assert_eq!(index_256_rgb(196), (255, 0, 0));
        assert_eq!(index_256_rgb(244), (128, 128, 128));
        assert_eq!(index_256_rgb(16), (0, 0, 0));
        assert_eq!(index_256_rgb(3), (205, 205, 0), "ANSI16 領域はそのまま");
    }

    #[test]
    fn rgb_to_16_conversion() {
        assert_eq!(nearest_16(255, 0, 0), 9, "明るい赤");
        assert_eq!(nearest_16(205, 0, 205), 5, "マゼンタ");
        assert_eq!(nearest_16(0, 0, 0), 0, "黒");
    }

    #[test]
    fn adapt_color_per_capability() {
        let red = Color::Rgb(255, 0, 0);
        assert_eq!(adapt_color(red, ColorCapability::TrueColor), red);
        assert_eq!(adapt_color(red, ColorCapability::Palette256), Color::Index(196));
        assert_eq!(adapt_color(red, ColorCapability::Ansi16), Color::Ansi(9));
        assert_eq!(adapt_color(Color::Index(196), ColorCapability::Ansi16), Color::Ansi(9));
        assert_eq!(
            adapt_color(Color::Ansi(6), ColorCapability::Palette256),
            Color::Ansi(6),
            "Ansi は能力に関わらずそのまま"
        );
    }
}


