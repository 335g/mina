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
//! - `minas skill --md` = **配布用ラッパー**。ユーザのエージェントに「minas を使わせる」
//!   ための薄いファイル（YAML frontmatter + トピック本文 + ビルド世代の刻印）を stdout に
//!   出す。エージェントは `minas skill` を知らないと pull できないので、push 側の入口を
//!   配る。索引は写さない（トピックを足すたびに腐るため。本文と『索引を引け』だけを持つ）。
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

/// トピック名で引く。未知なら候補一覧つきの理由（`run` が exit 1 にする）。
fn lookup(name: &str) -> Result<&'static Skill, String> {
    SKILLS.iter().find(|s| s.name == name).ok_or_else(|| {
        let known: Vec<&str> = SKILLS.iter().map(|s| s.name.as_str()).collect();
        format!(
            "unknown skill topic: {name:?} (known topics: {})",
            known.join(", ")
        )
    })
}

/// `--md` が出す配布用ラッパー（frontmatter + 本文 + ビルド世代の刻印）。
///
/// 正本は `skills.json` のままで、ラッパーは本文を写すだけ。刻印は「ユーザが一度貼った
/// きりで古い文言を使い続ける」事故を検出するため（`minas info` の cli_generation と比較）。
fn markdown(topic: Option<&str>) -> Result<String, String> {
    let name = topic.unwrap_or("usage");
    let skill = lookup(name)?;
    let skill_name = if name == "usage" {
        "minas".to_string()
    } else {
        format!("minas-{name}")
    };
    let generation = option_env!("MINA_GIT_HASH").unwrap_or("unknown");
    let ts = option_env!("MINA_BUILD_TS").unwrap_or("0");
    Ok(format!(
        "---\nname: {skill_name}\ndescription: {}\n---\n\n{}\n\nGenerated from minas {generation} \
         (build_ts {ts}) by `minas skill --md`. If `minas info` reports a different \
         cli_generation, regenerate.\n",
        skill.description,
        skill.body.trim_end()
    ))
}

/// 言語サーバが要るトピック（索引で `*` を付ける）。
///
/// ドッグフーディング #13: 索引に "LSP" が 1 度も出ず、no-LSP の言語（Python）の
/// 読み手は「どのコマンドが exit 1 で拒否されるか」を索引から引けなかった
/// （`read` だけは「LSP-free」と書いてあるのに、隣の `outline` は何も言わない）。
const LSP_TOPICS: &[&str] = &[
    "outline",
    "at",
    "hover",
    "symbol",
    "references",
    "rename",
    "check",
    "hints",
    "peek",
];

/// `minas skill [topic] [--md]` の本体。daemon は必要としない（静的コンテンツ）。
pub fn run(topic: Option<String>, md: bool) -> std::io::Result<()> {
    if md {
        return match markdown(topic.as_deref()) {
            Ok(text) => {
                print!("{text}");
                Ok(())
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
    }
    match topic {
        None => {
            // 索引: 1トピック1行。薄く保つことが設計要件（常時ロードしても軽い）。
            for s in SKILLS.iter() {
                let mark = if LSP_TOPICS.contains(&s.name.as_str()) {
                    "* "
                } else {
                    "  "
                };
                println!("{:<11}{mark}{}", s.name, s.description);
            }
            println!();
            println!(
                "* = needs a language server. A language without one gets an explicit `not \
                 supported` refusal (exit 1) instead of an empty answer; `minas info` lists the \
                 servers that exist. LSP-free: read, search, edit (apply / delete), wait, get, \
                 persist, errors, exec."
            );
            Ok(())
        }
        Some(t) => match lookup(&t) {
            Ok(s) => {
                println!("{}", s.body);
                Ok(())
            }
            Err(e) => {
                // 未知トピック: stderr に理由＋候補（エージェントは $?=1 で修正できる）
                eprintln!("{e}");
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

    /// 配布用ラッパー: 貼れる形（frontmatter）で、本文は正本のままで、世代が刻まれている。
    #[test]
    fn markdown_wrapper_is_pasteable_and_stamped() {
        let md = markdown(None).unwrap();
        let usage = SKILLS.iter().find(|s| s.name == "usage").unwrap();
        assert!(md.starts_with("---\nname: minas\ndescription: "), "frontmatter");
        assert_eq!(md.matches("\n---\n").count(), 1, "frontmatter は 1 つ");
        assert!(md.contains(usage.body.trim_end()), "本文は正本のまま");
        assert!(md.contains("Generated from minas"), "世代の刻印");
        assert!(!md.contains("\nread "), "索引の写しを埋め込まない");
    }

    /// トピック指定のラッパーは frontmatter 以外（name）が分かれ、未知トピックは理由を返す。
    #[test]
    fn markdown_takes_a_topic_and_rejects_unknown() {
        let read = markdown(Some("read")).unwrap();
        assert!(read.starts_with("---\nname: minas-read\ndescription: "), "name を分ける");
        let err = markdown(Some("nope")).unwrap_err();
        assert!(err.contains("unknown skill topic") && err.contains("usage"), "{err}");
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
