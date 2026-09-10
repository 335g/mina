//! キーバインドヘルプの静的コンテンツ。`?` で開く [`crate::app::Overlay::Help`]
//! に表示される。
//!
//! keymap.rs / app.rs のバインディングを変更したら必ずここを同期すること。
//! MVP は日本語固定。言語切替は将来 config の言語設定で対応する想定
//! （ponytail: 今は1言語なのでテーブルに言語を持たせない。設定が入ったら
//! `sections(lang)` にする）。

/// ヘルプの1行（左: キー / 右: 説明）。
pub(crate) struct Entry {
    pub(crate) key: &'static str,
    pub(crate) desc: &'static str,
}

/// ヘルプの1セクション（モード単位）。
pub(crate) struct Section {
    pub(crate) title: &'static str,
    pub(crate) entries: &'static [Entry],
}

/// ポップアップ末尾の操作ヒント行。
pub(crate) const FOOTER_HINT: &str = "j/k・↑/↓: 行送り   Ctrl-f/b: 頁送り   Esc/?: 閉じる";

pub(crate) fn sections() -> &'static [Section] {
    &SECTIONS
}

const SECTIONS: &[Section] = &[
    Section {
        title: "Normal モード",
        entries: &[
            // 移動
            Entry { key: "h/l/j/k", desc: "左/右/下/上に移動" },
            Entry { key: "←/→/↑/↓", desc: "上と同じ移動" },
            Entry { key: "w/b/e", desc: "次/前の単語先頭・単語末尾" },
            Entry { key: "Home/End", desc: "行頭/行末" },
            Entry { key: "g g", desc: "文書先頭へ" },
            Entry { key: "g e", desc: "文書末尾へ" },
            Entry { key: "g h / g l / g s", desc: "行頭/行末/最初の非空白へ" },
            Entry { key: "Ctrl-f / Ctrl-b / PgDn / PgUp", desc: "1ページスクロール" },
            Entry { key: "Ctrl-d / Ctrl-u", desc: "半ページスクロール" },
            // 検索
            Entry { key: "/", desc: "前方検索プロンプト" },
            Entry { key: "n / N", desc: "次の/前の検索結果へ" },
            Entry { key: "*", desc: "選択範囲を検索" },
            // 編集
            Entry { key: "i / a", desc: "カーソル位置/後ろに挿入（Insert へ）" },
            Entry { key: "A / I", desc: "行末/行頭（最初の非空白）で挿入" },
            Entry { key: "o / O", desc: "下/上に空行を開いて挿入" },
            Entry { key: "x / X", desc: "カーソル行を選択" },
            Entry { key: "d / c", desc: "選択を削除 / 変更（削除して挿入）" },
            Entry { key: "Backspace", desc: "1文字後方削除" },
            Entry { key: "u / U", desc: "アンドゥ / リドゥ" },
            Entry { key: "r / R", desc: "1文字置換 / 単語リネーム" },
            Entry { key: "m s / m d / m r", desc: "囲む / 囲みを外す / 囲みを置換" },
            Entry { key: "Space k", desc: "カーソル位置の定義を覗く" },
            Entry { key: "%", desc: "文書全体を選択" },
            Entry { key: "v", desc: "Select モードへ" },
            Entry { key: "Tab", desc: "gap レビュー開始" },
            // 表示・操作
            Entry { key: ":", desc: "コマンドライン（w/q/open/colorscheme 等）" },
            Entry { key: "?", desc: "キーバインドヘルプを開く" },
            Entry { key: "T", desc: "ツリー" },
            Entry { key: "G", desc: "診断リスト" },
            Entry { key: "A", desc: "活動履歴" },
            Entry { key: "C", desc: "配色を切替" },
            Entry { key: "D / B / M", desc: "比較表示 / 基準更新 / 2コミット比較" },
            Entry { key: "P", desc: "基準全文ブラウズ" },
            Entry { key: "E / K", desc: "コメントをAIへ / コメント入力" },
            Entry { key: "Ctrl-C", desc: "終了" },
        ],
    },
    Section {
        title: "Select モード",
        entries: &[
            Entry { key: "v / Esc", desc: "Normal モードへ戻る" },
            Entry { key: "移動キー各種", desc: "Normal と同じ移動・検索（選択が拡張される）" },
            Entry { key: "x / X", desc: "行を下へ拡張 / カーソル行を選択" },
            Entry { key: "d / c", desc: "選択を削除 / 変更" },
            Entry { key: "Backspace", desc: "選択を削除" },
            Entry { key: "a / A / I / o / O", desc: "挿入系（Normal と同じ）" },
            Entry { key: "u / U / % / Space k", desc: "アンドゥ等（Normal と同じ）" },
            Entry { key: "m s / m d / m r", desc: "選択を囲む / 囲みを外す / 囲みを置換" },
            Entry { key: ":", desc: "コマンドライン" },
        ],
    },
    Section {
        title: "Insert モード",
        entries: &[
            Entry { key: "Esc", desc: "Normal モードへ戻る" },
            Entry { key: "Enter / Tab", desc: "改行 / タブ挿入" },
            Entry { key: "Backspace / Ctrl-h", desc: "1文字後方削除" },
            Entry { key: "Delete / Ctrl-d", desc: "1文字前方削除" },
            Entry { key: "Ctrl-w / Alt-BS", desc: "単語ごと後方削除" },
            Entry { key: "Alt-d", desc: "単語ごと前方削除" },
            Entry { key: "←/→/Home/End", desc: "カーソル移動" },
            Entry { key: "↑/↓/PgUp/PgDn", desc: "移動 / スクロール" },
        ],
    },
];