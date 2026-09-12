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
//! 各トピックは `minas/skills/<topic>.md` に置き `include_str!` で同梱する
//! （ハイライトの `highlights/*.scm` と同じ流儀）。ファイル形式は
//! **1行目 = 索引に出る一行説明 / 空行 / 本文**。文言の修正は Markdown だけで
//! 完結し、Rust のソースを触らない。

/// 1トピックのスキル定義。
///
/// 生テキストを丸ごと持つので、説明と本文はその場で切り出す（11件×1回の
/// `split_once` は無視できるコスト）。
pub struct Skill {
    /// トピック名（`minas skill <topic>` の引数）。索引の左列。
    pub name: &'static str,
    /// `skills/<name>.md` の中身（1行目 = 説明、空行、本文）。
    raw: &'static str,
}

impl Skill {
    /// 索引行に出る一行説明（ファイルの1行目）。
    pub fn description(&self) -> &'static str {
        self.raw
            .split_once('\n')
            .map_or(self.raw, |(d, _)| d)
            .trim_end()
    }

    /// 本文（説明行と区切りの空行を除いた残り）。
    pub fn body(&self) -> &'static str {
        self.split().1
    }

    fn split(&self) -> (&'static str, &'static str) {
        match self.raw.split_once('\n') {
            Some((d, rest)) => (
                d.trim_end(),
                rest.trim_start_matches(['\n', '\r']).trim_end(),
            ),
            None => (self.raw, ""),
        }
    }
}

/// 全トピック。索引の順序がそのまま表示順。
const SKILLS: &[Skill] = &[
    Skill {
        name: "read",
        raw: include_str!("../skills/read.md"),
    },
    Skill {
        name: "edit",
        raw: include_str!("../skills/edit.md"),
    },
    Skill {
        name: "outline",
        raw: include_str!("../skills/outline.md"),
    },
    Skill {
        name: "at",
        raw: include_str!("../skills/at.md"),
    },
    Skill {
        name: "rename",
        raw: include_str!("../skills/rename.md"),
    },
    Skill {
        name: "references",
        raw: include_str!("../skills/references.md"),
    },
    Skill {
        name: "check",
        raw: include_str!("../skills/check.md"),
    },
    Skill {
        name: "hover",
        raw: include_str!("../skills/hover.md"),
    },
    Skill {
        name: "symbol",
        raw: include_str!("../skills/symbol.md"),
    },
    Skill {
        name: "persist",
        raw: include_str!("../skills/persist.md"),
    },
    Skill {
        name: "errors",
        raw: include_str!("../skills/errors.md"),
    },
];

/// `minas skill [topic]` の本体。daemon は必要としない（静的コンテンツ）。
pub fn run(topic: Option<String>) -> std::io::Result<()> {
    match topic {
        None => {
            // 索引: 1トピック1行。薄く保つことが設計要件（常時ロードしても軽い）。
            for s in SKILLS {
                println!("{:<10} {}", s.name, s.description());
            }
            Ok(())
        }
        Some(t) => match SKILLS.iter().find(|s| s.name == t) {
            Some(s) => {
                println!("{}", s.body());
                Ok(())
            }
            None => {
                // 未知トピック: stderr に理由＋候補（エージェントは $?=1 で修正できる）
                let known: Vec<&str> = SKILLS.iter().map(|s| s.name).collect();
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
        let mut names = SKILLS.iter().map(|s| s.name).collect::<Vec<_>>();
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
        for s in SKILLS {
            let (desc, body) = (s.description(), s.body());
            assert!(!desc.is_empty(), "{}: 索引説明が空", s.name);
            assert!(!desc.contains('\n'), "{}: 説明は1行", s.name);
            assert!(body.len() > 100, "{}: 内容が短すぎる", s.name);
            assert!(!body.contains('\0'));
        }
    }

    #[test]
    fn topic_lookup_by_name() {
        let found = SKILLS.iter().find(|s| s.name == "edit");
        assert!(found.is_some(), "edit トピックが存在する");
        assert!(
            found.unwrap().body().contains("minas apply"),
            "本編は行動レベルの内容"
        );
    }
}
