//! daemon のビルド世代情報を注入する（issue #27 / D1）。
//!
//! `env!` の固定環境変数にはビルド日時が無いため、ビルドスクリプトで
//! `MINA_BUILD_TS`（Unix 秒）と `MINA_GIT_HASH`（コミットハッシュ）を
//! `cargo:rustc-env` で注入する。daemon は [`option_env!`] で読み、古いビルドの
//! daemon が新プロトコル項目を黙殺していないか（silent ignore）を検知できる
//! 開示に使う。

use std::process::Command;

fn main() {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=MINA_BUILD_TS={}", ts);

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MINA_GIT_HASH={}", hash);

    println!("cargo:rerun-if-changed=build.rs");
}