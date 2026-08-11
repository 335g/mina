//! agent 用のヘッドレス CLI: daemon に接続してコマンドを実行し、スナップショットを表示する。
//!
//! - `mina session get` — 現在の状態を取得する（JSON で出力）
//! - `mina session exec <JSON>` — コマンドを1つ実行する（JSON は wire の [`Command`] そのまま）
//!
//! 例: `mina session exec '{"Insert": {"text": "hello"}}'`
//!
//! daemon が動いていなければ自動起動される（TUI と同じ挙動）。成功時は exit 0、
//! トランスポート/JSON エラー時は exit 1。コマンド自体の成否はスナップショットの
//! `status` フィールドに載る。

use std::io;

use mina_protocol::{Command, StateSnapshot};
use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::client;

/// `mina session <get|exec ...>` を処理する。`args` はサブコマンド以降。
pub async fn run(args: &[String]) -> io::Result<()> {
    let command = parse_command(args)?;
    let snapshot = execute(&command).await?;
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
}

/// サブコマンドを [`Command`] に変換する。
fn parse_command(args: &[String]) -> io::Result<Command> {
    match args.first().map(String::as_str) {
        Some("get") => Ok(Command::GetState),
        Some("exec") => {
            let json = args.get(1).ok_or_else(|| {
                invalid("exec にはコマンド JSON が必要です: mina session exec '<command>'")
            })?;
            serde_json::from_str(json).map_err(|e| invalid(format!("コマンド JSON を解釈できません: {e}")))
        }
        Some(other) => Err(invalid(format!("未知の session サブコマンド: {other}（get / exec）"))),
        None => Err(invalid("session サブコマンドが必要です: get / exec")),
    }
}

/// daemon に接続し、コマンドを実行してスナップショットを受け取る。
async fn execute(command: &Command) -> io::Result<StateSnapshot> {
    let path = crate::daemon::socket_path();
    client::ensure_daemon(&path).await?;
    let stream = UnixStream::connect(&path).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    client::request(&mut write_half, &mut reader, command.clone()).await
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn get_parses() {
        assert_eq!(parse_command(&args(&["get"])).unwrap(), Command::GetState);
    }

    #[test]
    fn exec_parses_json() {
        let cmd = parse_command(&args(&["exec", r#"{"Insert": {"text": "x"}}"#])).unwrap();
        assert_eq!(cmd, Command::Insert { text: "x".into() });
    }

    #[test]
    fn bad_json_is_rejected() {
        assert!(parse_command(&args(&["exec", "not json"])).is_err());
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(parse_command(&args(&["frobnicate"])).is_err());
        assert!(parse_command(&args(&[])).is_err());
    }
}
