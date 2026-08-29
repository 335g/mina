//! モード: モーダル編集の状態。

/// モーダル編集のモード。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// ナビゲーションと編集コマンド。
    #[default]
    Normal,
    /// テキスト入力。
    Insert,
    /// 移動で選択が伸びる（extend）モード。
    Select,
}
