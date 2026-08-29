# エージェント向け定義確認: 全文を返さない軽量経路 (PeekDefinitionAt)

エージェント（headless）が「その位置のシンボル定義」を確認する経路として `Command::PeekDefinitionAt { path, line, col }`（1-origin 行:列）を追加し、応答は **全文スナップショットを運ばない** `ServerMessage::Peek { path, line, text }`（定義元パス・開始行・スニペットのみ）とする。CLI は `minae session peek <path> <line>:<col>`。

TUI の `Space k`（`Command::PeekDefinition`）はカーソル基準・スナップショット応答のまま維持する。エージェントはカーソル概念を持たないため位置指定が必要であり、スナップショット（全文テキスト込み）を受け取ると逆にトークンを消費する — 定義だけを返すことで「全文を読まない」を実現する。これは inlay hint を全文なしで返す `GetInlayHints`（ADR-0020）と同じ思想の延長で、minae の開発動機（トークンの有効活用）に沿う。

実装の要点: (1) 位置は行内の文字数（1-origin）で受け、LSP 座標へはセッションのネゴシエート済み encoding で変換する（`char_col_to_lsp_character`）。(2) LSP セッションは同時に 1 文書しか開けないため、フォーカス文書と異なるパスの要求は serve_inlay_hints と同じ「対象を didOpen → 取得 → フォーカス文書へ復元 + 診断の再 pull」で対応する。復元は serve_inlay_hints から `restore_focus_session` / `borrows_focus_session` として抽出し共用する。(3) 定義なし・LSP 非対応・読み込み不可は `text` 空で応答する（エージェントは空判定で「定義なし」と解釈できる）。
