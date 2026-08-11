//! クライアント（TUI）: daemon にコマンドを送り、StateSnapshot を受け取って描画する。
//!
//! リクエスト/レスポンスのみ（ADR-0006）。編集状態は持たない — キーイベントを
//! キーマップで Command に解決して送り、返ってきたスナップショットを描画するだけ。
//! daemon が動いていなければ自動起動する（ADR-0005）。

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use futures_lite::StreamExt;
use mina_protocol::{Command, Mode, StateSnapshot};
use termina::event::{KeyCode, KeyEvent, KeyEventKind};
use termina::{Event, EventStream, PlatformTerminal, Terminal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixStream, unix::OwnedWriteHalf};

use crate::keymap::{Keymaps, Resolution};
use crate::render;

const ALT_SCREEN_ON: &str = "\x1b[?1049h";
const ALT_SCREEN_OFF: &str = "\x1b[?1049l";
const CURSOR_HIDE: &str = "\x1b[?25l";
const CURSOR_SHOW: &str = "\x1b[?25h";

/// TUI 終了時のターミナル復旧ガード（M4）。
///
/// raw モード・代替画面・カーソル非表示を、正常終了・エラー経路（`?`）を問わず
/// 必ず元に戻す。daemon 切断等でエラー return してもシェルを壊さない。
struct TerminalGuard(PlatformTerminal);

impl std::ops::Deref for TerminalGuard {
    type Target = PlatformTerminal;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for TerminalGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.0.write_all(CURSOR_SHOW.as_bytes());
        let _ = self.0.write_all(ALT_SCREEN_OFF.as_bytes());
        let _ = self.0.flush();
        let _ = self.0.enter_cooked_mode();
    }
}

/// TUI を起動する。`file` があればそれを開く。
pub async fn run(file: Option<&str>) -> std::io::Result<()> {
    let socket = crate::daemon::socket_path();
    ensure_daemon(&socket).await?;
    let stream = UnixStream::connect(&socket).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // 初回コマンド: ファイル指定があれば Open、なければ GetState
    let first = match file {
        Some(path) => Command::Open { path: path.to_string() },
        None => Command::GetState,
    };
    // M5: 初回応答（Open の失敗 status など）を破棄せず保持する
    let mut state = request(&mut write_half, &mut reader, first).await?;
    let first_status = state.status.take();

    // 端末セットアップ（raw モード + 代替画面 + カーソル非表示）
    let terminal = PlatformTerminal::new()?;
    // M4: 以降はエラー経路（`?`）でも必ずターミナルを復旧する
    let mut terminal = TerminalGuard(terminal);
    terminal.enter_raw_mode()?;
    terminal.write_all(ALT_SCREEN_ON.as_bytes())?;
    terminal.write_all(CURSOR_HIDE.as_bytes())?;
    terminal.flush()?;

    let size = terminal.get_dimensions()?;
    let mut width = size.cols;
    let mut height = size.rows;
    let mut state = request(
        &mut write_half,
        &mut reader,
        Command::SetViewport {
            height: height as usize,
        },
    )
    .await?;
    // M5: SetViewport の応答は status を持たないので、初回応答の status を引き継ぐ
    if state.status.is_none() {
        state.status = first_status;
    }

    let keymaps = Keymaps::new();
    let mut pending: Vec<KeyEvent> = Vec::new();
    let mut events = EventStream::new(terminal.event_reader(), |_| true);

    render::draw(&mut *terminal, &state, &pending, width, height)?;
    terminal.flush()?;

    while let Some(event) = events.next().await {
        let event = match event {
            Ok(e) => e,
            Err(_) => continue,
        };
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // 終了: 全モードで Ctrl-C、Normal で q
                let quit = key.code == KeyCode::Char('c')
                    && key.modifiers.contains(termina::event::Modifiers::CONTROL)
                    || (state.mode == Mode::Normal && key.code == KeyCode::Char('q'));
                if quit {
                    break;
                }
                match keymaps.resolve_with_insert_fallback(state.mode, &mut pending, key) {
                    Resolution::Command(command) => {
                        state = request(&mut write_half, &mut reader, command).await?;
                    }
                    _ => {} // pending 変化の描画は共通ループ末尾で行う
                }
            }
            Event::WindowResized(size) => {
                width = size.cols;
                height = size.rows;
                state = request(
                    &mut write_half,
                    &mut reader,
                    Command::SetViewport {
                        height: height as usize,
                    },
                )
                .await?;
            }
            _ => continue,
        }
        render::draw(&mut *terminal, &state, &pending, width, height)?;
        terminal.flush()?;
    }

    // 終了処理は TerminalGuard の Drop が行う（M4: エラー経路でも必ず復旧する）
    Ok(())
}

/// コマンドを送り、応答スナップショットを1つ受け取る。TUI と session CLI の両方から使う。
pub(crate) async fn request(
    write_half: &mut OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    command: Command,
) -> std::io::Result<StateSnapshot> {
    let mut line = serde_json::to_string(&command).expect("command はシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    serde_json::from_str(&response).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("不正な応答: {e}"))
    })
}

/// daemon が動いていなければ自動起動し、socket が現れるまで待つ。TUI と session CLI の両方から使う。
///
/// 競合: 同時に2つのクライアントが spawn した場合、片方の daemon が bind に
/// 失敗して終了する（v1 の割り切り）。
///
/// ponytail: 起動確認は50×50ms のポーリング。遅い環境で起動が遅れたら
/// 回数を増やすか、daemon 側の ready 通知を導入する。
pub(crate) async fn ensure_daemon(path: &Path) -> std::io::Result<()> {
    if UnixStream::connect(path).await.is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut cmd = ProcessCommand::new(exe);
    cmd.arg("daemon").arg("serve");
    // 端末を閉じても daemon が死なないよう、新しいセッションへ離脱させる
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    for _ in 0..50 {
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
