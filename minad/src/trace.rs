//! 計時ログ（iteration #9 の計測基盤。ADR-0056）。
//!
//! `MINAD_TRACE=1` のときだけ stderr に 1 行ずつ出す（既定は無効 = 出力なし・
//! `Instant::now()` 1 回と分岐だけ）。daemon は応答を返さないので、ログは
//! `minase`/`minas` を邪魔しない（stderr に出して stdout は使わない）。
//!
//! ```text
//! minad.trace <span> <phase> <ms>      フェーズの所要（直前の mark から）
//! minad.trace <span> <key>=<value>     補足値（リトライ回数・rounds・バイト数）
//! minad.trace <span> total <ms>        スパン全体（Drop で必ず出る）
//! ```
//!
//! `docs/loop/l0.py` のアームは専用 `TMPDIR` で daemon を立てるため、ログは
//! `tmp/loop/<flow>-<arm>-*/.tmp/minad.log` に落ちる（読み方は method.md §4）。
//!
//! ponytail: `tracing` クレートを足さない。欲しいのは「どこで何 ms 使ったか」だけで、
//! span の親子関係・レベルフィルタ・subscriber は要らない（`Instant` + `eprintln!`
//! で足りる）。親子のツリーが要るようになったら tracing へ移す。

use std::fmt::Display;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// `MINAD_TRACE=1` で有効（`OnceLock` で env は 1 回だけ読む）。
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("MINAD_TRACE").is_ok_and(|v| v == "1"))
}

/// 1 フェーズ分の行（`l0.py` / method.md が読む形式）。
fn phase_line(name: &str, phase: &str, elapsed: Duration) -> String {
    format!("minad.trace {name} {phase} {}", elapsed.as_millis())
}

/// 補足値の行。
fn note_line(name: &str, key: &str, value: &dyn Display) -> String {
    format!("minad.trace {name} {key}={value}")
}

/// スパン合計の行（`Drop` で出す）。
fn total_line(name: &str, elapsed: Duration) -> String {
    format!("minad.trace {name} total {}", elapsed.as_millis())
}

/// スパン（1 コマンド・1 LSP 要求など）のフェーズ計時。
///
/// `Drop` で `total` を出すため、途中 return でも合計が残る（`mark` を忘れても
/// 計測が壊れない）。無効時は何も出さない。
pub struct Trace {
    name: &'static str,
    start: Instant,
    last: Instant,
    on: bool,
}

impl Trace {
    pub fn new(name: &'static str) -> Trace {
        let now = Instant::now();
        Trace {
            name,
            start: now,
            last: now,
            on: enabled(),
        }
    }

    /// 直前の `mark`（無ければ開始）からの経過 ms を出す。
    pub fn mark(&mut self, phase: &str) {
        if !self.on {
            return;
        }
        let now = Instant::now();
        eprintln!("{}", phase_line(self.name, phase, now - self.last));
        self.last = now;
    }

    /// 補足値（リトライ回数・rounds・バイト数など）を出す。
    pub fn note(&self, key: &str, value: impl Display) {
        if self.on {
            eprintln!("{}", note_line(self.name, key, &value));
        }
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        if self.on {
            eprintln!("{}", total_line(self.name, self.start.elapsed()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 出力形式は計測スクリプト（method.md §4）が読む契約。3 行とも分解可能であること。
    #[test]
    fn trace_lines_are_parseable() {
        let ms = Duration::from_millis(780);
        assert_eq!(
            phase_line("check", "pull", ms).split_whitespace().collect::<Vec<_>>(),
            ["minad.trace", "check", "pull", "780"]
        );
        assert_eq!(
            note_line("lsp.retry", "retries", &2).split_whitespace().collect::<Vec<_>>(),
            ["minad.trace", "lsp.retry", "retries=2"]
        );
        assert_eq!(
            total_line("rename", Duration::from_millis(1150))
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["minad.trace", "rename", "total", "1150"]
        );
        // 無効時に出力しない（既定）
        if std::env::var("MINAD_TRACE").is_err() {
            assert!(!enabled());
        }
    }
}
