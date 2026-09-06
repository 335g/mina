//! minas のビルド世代情報を注入する（I4）。
//!
//! `env!` の固定環境変数にはビルド日時が無いため、ビルドスクリプトで
//! `MINA_BUILD_TS`（Unix 秒）と `MINA_GIT_HASH`（コミットハッシュ）を
//! `cargo:rustc-env` で注入する。`session info` は [`option_env!`] で読み、
//! CLI バイナリと daemon の世代を並べて開示する（古いバイナリ混在の検知）。
//! minad/build.rs と同型。

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
