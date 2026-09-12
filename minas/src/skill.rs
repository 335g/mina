//! `minas skill` — エージェント向けの軽量スキル（判断・手順の参考書）。
//!
//! 設計（docs/agent-editor-ab-results.md の実測に基づく）:
//! - `minas skill`（無引数）= **索引**。1トピック1行・約10行。薄く保ち、常時ロードしても
//!   トークン負担が小さい（T1: 範囲read vs 全文read で −51% の実測と同じ原理 —
//!   必要になったトピックだけを読む）。
//! - `minas skill <topic>` = **そのトピックの内容だけ**を返す。ロードオンリー・必要時のみ。
//! - 索引と内容を分離し、モデルが「発見（安い索引）→ 必要分だけ参照（安い内容）」できるようにした。
//!
//! 契約（session と統一）: 成功 = exit 0（stdout に内容）、未知トピック = exit 1（stderr に
//! 英語の理由＋利用可能トピック一覧）。エージェントは $? と stderr だけで制御できる。
//! 内容は英語（H4: エージェントがパースする出力は英語統一）。
//!
//! 各トピックの内容はツール判断の指針であり、tools/ab の A/B 実測（t1〜t5）と
//! ADR-0029（rename / references）・ADR-0031（outline / at）・ADR-0032
//! （hover / symbol / check）の実測・仕様に基づく。
//!
//! データは `minas/skills.json`（`[{"name","description","body"}, ...]`）に置き
//! `include_str!` で同梱する。配列順が索引の表示順、`body` が `minas skill <name>`
//! の出力全文。文言とトピック追加は JSON だけで完結し、Rust のソースを触らない。

use std::sync::LazyLock;

/// 1トピックのスキル定義。`skills.json` の1要素。
#[derive(Debug, serde::Deserialize)]
pub struct Skill {
    /// トピック名（`minas skill <topic>` の引数）。索引の左列。
    pub name: String,
    /// 索引行に出る一行説明。
    pub description: String,
    /// 本文（`minas skill <name>` が stdout に出す全文）。
    pub body: String,
}

/// 同梱した `skills.json`。初回利用時に一度だけデシリアライズする。
static SKILLS: LazyLock<Vec<Skill>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../skills.json")).expect("embedded skills.json is valid")
});

/// `minas skill [topic]` の本体。daemon は必要としない（静的コンテンツ）。
pub fn run(topic: Option<String>) -> std::io::Result<()> {
    match topic {
        None => {
            // 索引: 1トピック1行。薄く保つことが設計要件（常時ロードしても軽い）。
            for s in SKILLS.iter() {
                println!("{:<10} {}", s.name, s.description);
            }
            Ok(())
        }
        Some(t) => match SKILLS.iter().find(|s| s.name == t) {
            Some(s) => {
                println!("{}", s.body);
                Ok(())
            }
            None => {
                // 未知トピック: stderr に理由＋候補（エージェントは $?=1 で修正できる）
                let known: Vec<&str> = SKILLS.iter().map(|s| s.name.as_str()).collect();
                eprintln!(
                    "unknown skill topic: {t:?} (known topics: {})",
                    known.join(", ")
                );
                std::process::exit(1);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_lists_every_topic_once() {
        let mut names = SKILLS.iter().map(|s| s.name.as_str()).collect::<Vec<_>>();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(before, names.len(), "トピック名は重複しない");
        assert!(before >= 3, "索引は最低3トピック");
        // ADR-0032 の3コマンドは索引に載る（エージェントが発見できる）
        for required in ["check", "hover", "symbol"] {
            assert!(
                SKILLS.iter().any(|s| s.name == required),
                "索引に {required} トピックが必要"
            );
        }
    }

    #[test]
    fn every_topic_has_description_and_body() {
        for s in SKILLS.iter() {
            assert!(!s.description.is_empty(), "{}: 索引説明が空", s.name);
            assert!(!s.description.contains('\n'), "{}: 説明は1行", s.name);
            assert!(s.body.len() > 100, "{}: 内容が短すぎる", s.name);
            assert!(!s.body.trim().is_empty() && !s.body.contains('\0'));
        }
    }

    #[test]
    fn topic_lookup_by_name() {
        let found = SKILLS.iter().find(|s| s.name == "edit");
        assert!(found.is_some(), "edit トピックが存在する");
        assert!(
            found.unwrap().body.contains("minas apply"),
            "本編は行動レベルの内容"
        );
    }
}
