//! daemon との接続・リクエスト送信の共通配線（TUI と session CLI が共用）。
//!
//! daemon の起動確認・Hello 送信・コマンド送信は、TUI とヘッドレス（session CLI）の
//! 両方が同じ経路を通る。この crate は bin クレート（minae）から独立しており、
//! 将来の TUI リポジトリ（別バイナリ）からも使う想定（ADR-0034）。
//!
//! スコープは「クライアント→daemon」の一方向層のみ: daemon プロセスの
//! 生殺与奪（serve ループ・状態・LSP）は daemon 本体の責務で、ここが持つのは
//! クライアント視点のライフサイクル（必要なら spawn する）とチャネルの開設・
//! 送受信だけ。自動起動の組み立て（どの実行ファイルを spawn するか）は
//! 呼び出し側の責務（connect + spawn_daemon + wait_ready を組み合わせる）。
//! ソケットパスの契約は minae-protocol の [`mina_protocol::socket_path`]。

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use mina_protocol::{ClientKind, Command, Hello, InlayHint, ServerMessage, StateSnapshot};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf};

/// Open に送るパスを絶対化する。既に絶対パスならそのまま返す。
///
/// daemon の cwd は最初に spawn された場所で固定されるため（ADR-0005）、
/// 相対パスは送信側で現在のディレクトリ基準に解決してから渡す。
pub fn absolutize(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    std::env::current_dir()
        .map(|dir| dir.join(p))
        .map(|abs| abs.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// 接続直後に Hello（クライアント種別の宣言）を送る（ADR-0012）。
/// 応答は待たない（最初のコマンドの応答スナップショットに乗る）。
pub async fn send_hello(
    write_half: &mut OwnedWriteHalf,
    kind: ClientKind,
    reset_cursor_on_disconnect: bool,
    name: &str,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(&Hello {
        kind,
        reset_cursor_on_disconnect,
        name: name.to_string(),
    })
    .expect("Hello はシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await
}

/// daemon のソケットへ接続を1回試みる。成功すれば stream を返す。
///
/// 自動起動（spawn して socket が出るまで待つ）は呼び出し側が
/// [`daemon_exe`] + [`spawn_daemon`] + [`wait_ready`] で組み立てる —
/// daemon を持つのは `minad` バイナリで、この crate は仮定しない。
pub async fn connect(path: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(path).await
}

/// daemon 実行ファイルを探す（分離バイナリ用の自動起動解決）。
///
/// 1. `MINAD_EXE` 環境変数（明示指定・開発/CI 用）
/// 2. PATH 上の `minad`（通常 install は ~/.cargo/bin に 3 bin が並ぶ）
/// 見つからなければ None — 呼び出し側は「minad が見つからない」旨の
/// 明示エラーにする（旧「daemon が起動しなかった」より特定しやすい）。
pub fn daemon_exe() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::var("MINAD_EXE") {
        if !exe.is_empty() {
            return Some(std::path::PathBuf::from(exe));
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join("minad");
            candidate.is_file().then_some(candidate)
        })
    })
}

/// daemon を新規セッションで起動する（端末を閉じても死なない、setsid）。
///
/// spawn は detached で、起動確認はしない（[`wait_ready`]）。競合: 同時に
/// 2つのクライアントが spawn した場合、片方の daemon が bind に失敗して
/// 終了する（v1 の割り切り）。
pub fn spawn_daemon(exe: &Path, args: &[&str]) -> std::io::Result<()> {
    let mut cmd = ProcessCommand::new(exe);
    cmd.args(args);
    // 端末を閉じても daemon が死なないよう、新しいセッションへ離脱させる
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    Ok(())
}

/// socket が現れるまで待つ（spawn した daemon の起動確認）。
///
/// ponytail: 50×50ms のポーリング。遅い環境で起動が遅れたら回数を増やすか、
/// daemon 側の ready 通知を導入する。
pub async fn wait_ready(path: &Path, attempts: u32) -> std::io::Result<()> {
    for _ in 0..attempts {
        if UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "daemon が起動しなかった",
    ))
}

/// ワンショット接続を開く: socket へ接続し、Hello（ClientKind 宣言）を送って
/// (write_half, reader) を返す。
///
/// headless の session CLI が1コマンドごとに開く形。永続接続（push 購読、
/// ADR-0013）の TUI は独自の接続ループを持つため、この crate はワンショットを
/// 提供する。daemon の自動起動は呼び出し側の責務（connect / spawn_daemon /
/// wait_ready を組み合わせる）。
pub async fn open_session(
    path: &Path,
    kind: ClientKind,
    reset_cursor_on_disconnect: bool,
    name: &str,
) -> std::io::Result<(OwnedWriteHalf, BufReader<OwnedReadHalf>)> {
    let stream = connect(path).await?;
    let (read_half, mut write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    send_hello(&mut write_half, kind, reset_cursor_on_disconnect, name).await?;
    Ok((write_half, reader))
}

/// 任意パスの inlay hint を取得する（ADR-0020）。`Command::GetInlayHints` の
/// 応答はスナップショットでなく `ServerMessage::Hints` なので専用経路。
/// エージェント（session CLI）が全文テキストなしで型構造を参照するためのもの。
pub async fn request_hints(
    write_half: &mut OwnedWriteHalf,
    reader: &mut BufReader<OwnedReadHalf>,
    path: &str,
) -> std::io::Result<(String, u64, Vec<InlayHint>)> {
    let message = Command::GetInlayHints {
        path: path.to_string(),
    };
    let mut line = serde_json::to_string(&message).expect("メッセージはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::Hints { path, generation, hints }) => Ok((path, generation, hints)),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected snapshot response",
            ))
        }
        // #23 と同様: Peek 応答はここでは期待しない
        Ok(ServerMessage::Peek { .. }) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected peek response",
        )),
        // #27: GetServerInfo の応答（ServerInfo）はこの経路では期待しない
        Ok(ServerMessage::ServerInfo { .. }) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected server info response",
        )),
        // ADR-0029: Rename / References 応答もこの経路では期待しない
        Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected semantic response",
            ))
        }
        Err(e) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("不正な応答: {e}"),
        )),
    }
}

/// 指定位置（1-origin 行:列）のシンボル定義を取得する（ADR-0025）。
/// `Command::PeekDefinitionAt` の応答はスナップショットでなく軽量な
/// `ServerMessage::Peek`（全文なし）なので専用経路。
/// エージェント（session CLI）が全文を読まずに定義を参照するためのもの。
pub async fn request_peek(
    write_half: &mut OwnedWriteHalf,
    reader: &mut BufReader<OwnedReadHalf>,
    path: &str,
    line: u32,
    col: u32,
) -> std::io::Result<mina_protocol::Peek> {
    let message = Command::PeekDefinitionAt {
        path: path.to_string(),
        line,
        col,
    };
    let mut line = serde_json::to_string(&message).expect("メッセージはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::Peek { path, line, text }) => Ok(mina_protocol::Peek { path, line, text }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected snapshot response",
            ))
        }
        // この経路は Peek 専用: hints 応答は期待しない
        Ok(ServerMessage::Hints { .. }) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected inlay hints response",
        )),
        // #27: ServerInfo 応答はこの経路では期待しない
        Ok(ServerMessage::ServerInfo { .. }) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected server info response",
        )),
        // ADR-0029: Rename / References 応答もこの経路では期待しない
        Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected semantic response",
            ))
        }
        Err(e) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("不正な応答: {e}"),
        )),
    }
}

/// メッセージを送り、応答スナップショットを1つ受け取る。TUI と session CLI の両方から使う。
pub async fn request<T: serde::Serialize>(
    write_half: &mut OwnedWriteHalf,
    reader: &mut BufReader<OwnedReadHalf>,
    message: &T,
) -> std::io::Result<StateSnapshot> {
    let mut line = serde_json::to_string(message).expect("メッセージはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    // ADR-0013: 応答はタグ付きエンベロープになった。Headless のワンショット
    // CLI は push を受け取らない（購読されない）ので常に Response だが、
    // 形状の違いに依存しないようどちらでも取り出す。
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::Response { snapshot }) | Ok(ServerMessage::Push { snapshot }) => {
            Ok(snapshot)
        }
        // #23: GetInlayHints の応答はヘッドレスクライアントの別経路で扱う
        Ok(ServerMessage::Hints { .. }) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected inlay hints response",
        )),
        // ADR-0025: Peek 応答も別経路（request_peek）で扱う
        Ok(ServerMessage::Peek { .. }) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected peek response",
        )),
        // #27: GetServerInfo の応答（ServerInfo）はこの経路では期待しない
        //（専用経路: session info）
        Ok(ServerMessage::ServerInfo { .. }) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected server info response",
        )),
        // ADR-0029: Rename / References 応答もこの経路では期待しない
        Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected semantic response",
            ))
        }
        Err(e) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("不正な応答: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_keeps_absolute_path() {
        assert_eq!(absolutize("/abs/path/file.rs"), "/abs/path/file.rs");
    }

    #[test]
    fn absolutize_resolves_relative_to_cwd() {
        let abs = absolutize("rel/file.rs");
        assert!(std::path::Path::new(&abs).is_absolute());
        assert!(abs.ends_with("rel/file.rs"));
    }
}