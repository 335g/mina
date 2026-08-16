//! クライアント（TUI）: daemon にコマンドを送り、StateSnapshot を受け取って描画する。
//!
//! Open のパスはクライアント側で絶対化してから送る — daemon は常駐で cwd が
//! 起動時のディレクトリのままなので、相対パスの解決を daemon に任せると
//! 別ディレクトリから起動したクライアントの意図と食い違う（[`absolutize`]）。
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
use mina_protocol::{ClientKind, Command, Hello, InlayHint, Mode, Peek, ServerMessage, StateSnapshot};
use termina::event::{KeyCode, KeyEvent, KeyEventKind};
use termina::{Event, EventStream, PlatformTerminal, Terminal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixStream, unix::OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::colorscheme::{self, Colorscheme};
use crate::keymap::{Keymaps, Resolution};
use crate::render;

/// Open に送るパスを絶対化する。既に絶対パスならそのまま返す。
///
/// daemon の cwd は最初に spawn された場所で固定されるため（ADR-0005）、
/// 相対パスは送信側で現在のディレクトリ基準に解決してから渡す。
/// session CLI（[`crate::session`]）からも使う（pub(crate)）。
pub(crate) fn absolutize(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    std::env::current_dir()
        .map(|dir| dir.join(p))
        .map(|abs| abs.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

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
    let mut session = Session::connect(&socket).await?;

    // 初回コマンド: ファイル指定があれば Open、なければ GetState。
    // Open のパスは絶対化して送る — daemon は常駐で cwd が起動時のディレクトリの
    // ままなので、相対パスを daemon 側で解決すると別ディレクトリから起動した
    // クライアントの意図と食い違う（`cannot open` になる）。
    let first = match file {
        Some(path) => Command::Open { path: absolutize(path) },
        None => Command::GetState,
    };
    // M5: 初回応答（Open の失敗 status など）を破棄せず保持する
    let mut state = session.request(&first).await?;
    let first_status = state.status.take();

    // 現在の Colorscheme: config.toml の名前から解決（ユーザーファイル優先、次に組み込み、
    // 不明なら警告 + 組み込み DEFAULT — ADR-0022）。クライアントローカル — daemon 非関与。
    // 警告は端末セットアップ前に stderr へ出す（raw モード・代替画面中の表示崩れを避ける）。
    let config = crate::config::load();
    let schemes_dir = crate::config::schemes_dir();
    let mut scheme = match config.colorscheme.as_deref() {
        Some(name) => match colorscheme::resolve(name, &schemes_dir) {
            Some(s) => s,
            None => {
                eprintln!("warning: unknown colorscheme {name:?}, using default");
                colorscheme::DEFAULT.clone()
            }
        },
        None => colorscheme::DEFAULT.clone(),
    };

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
    let mut state = session
        .request(&Command::SetViewport {
            height: height as usize,
        })
        .await?;
    // M5: SetViewport の応答は status を持たないので、初回応答の status を引き継ぐ
    if state.status.is_none() {
        state.status = first_status;
    }

    let keymaps = Keymaps::new();
    let mut pending: Vec<KeyEvent> = Vec::new();
    // コマンドモード（Helix 流の `:` プロンプト）の入力バッファ。
    // Some の間はキー入力がプロンプト編集になり、ステータス行に `:` が表示される。
    let mut command_line: Option<String> = None;
    // クライアント側の一時メッセージ（未知コマンド等）。次のキーで消える。
    let mut flash: Option<String> = None;
    // 定義ポップアップ（Space k / PeekDefinition）の内容。次のキーで消える
    // クライアントローカルな一時表示。応答スナップショットの `peek` フィールド
    // から移し替える — スナップショット自体には残さない（push との内容比較を
    // 汚さず、`state != state` の再描画判定を壊さないため）。
    let mut peek: Option<Peek> = None;
    // 色能力と NO_COLOR（起動時に 1 回検出 — ADR-0019）。
    let (capability, no_color) = colorscheme::detect_from_env();
    let mut events = EventStream::new(terminal.event_reader(), |_| true);
    // フレーム間で再利用する LineIndex（テキストが同じ間は再構築しない）。
    let mut li_cache = render::LineIndexCache::default();

    render::draw_with_cache(
        &mut *terminal,
        &scheme,
        capability,
        no_color,
        &state,
        &pending,
        command_line.as_deref(),
        flash.as_deref(),
        width,
        height,
        &mut li_cache,
    )?;
    terminal.flush()?;

    // ADR-0013: キーイベントと daemon からの push を並列に待つ。push は
    // 他クライアント（agent 等）の変更を即時反映する。generation が最後に
    // 描画したものと同じなら捨てる（自分自身の変更は応答で描画済み）。
    loop {
        let mut redraw = false;
        tokio::select! {
            event = events.next() => {
                let event = match event {
                    Some(Ok(e)) => e,
                    Some(Err(_)) => continue,
                    None => break,
                };
                match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        // ADR-0015: 外部削除ポップアップ表示中は入力をブロックし、
                        // 任意キーで Close（空画面へ戻る）
                        flash = None; // 一時メッセージは次のキーで消える
                        peek = None; // 定義ポップアップも次のキーで消える
                        if state.deleted.is_some() {
                            command_line = None;
                            state = session.request(&Command::Close).await?;
                        } else if let Some(buf) = &mut command_line {
                            // コマンドモード: 文字はバッファへ、Backspace で1文字削除、
                            // Enter で実行、Esc / Ctrl-C でキャンセル
                            match key.code {
                                KeyCode::Char(c)
                                    if key.modifiers.is_empty()
                                        || key.modifiers == termina::event::Modifiers::SHIFT =>
                                {
                                    buf.push(c);
                                }
                                KeyCode::Char('c')
                                    if key.modifiers
                                        .contains(termina::event::Modifiers::CONTROL) =>
                                {
                                    command_line = None;
                                }
                                KeyCode::Backspace => {
                                    buf.pop();
                                }
                                KeyCode::Escape => command_line = None,
                                KeyCode::Enter => {
                                    let action = parse_command(buf);
                                    command_line = None;
                                    match action {
                                        CommandLineAction::Save => {
                                            state = session.request(&Command::Save).await?;
                                        }
                                        CommandLineAction::Quit => break,
                                        CommandLineAction::SaveThenQuit => {
                                            state = session.request(&Command::Save).await?;
                                            // 保存失敗・保存中の追記で dirty が残る場合は
                                            // 終了しない（daemon の status が理由を示す）
                                            if !state.dirty {
                                                break;
                                            }
                                        }
                                        CommandLineAction::Unknown(cmd) => {
                                            flash = Some(format!("unknown command: {cmd}"));
                                        }
                                        CommandLineAction::Colorscheme(name) => {
                                            if let Some(msg) = apply_colorscheme(
                                                &mut scheme,
                                                name.as_deref(),
                                                &schemes_dir,
                                            ) {
                                                flash = Some(msg);
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        } else if key.code == KeyCode::Char(':')
                            && state.mode != Mode::Insert
                        {
                            // `:` でコマンドモードに入る（Insert では `:` は文字入力）
                            pending.clear();
                            command_line = Some(String::new());
                        } else {
                            // 終了: 全モードで Ctrl-C（保存は :w、終了は :q）
                            let quit = key.code == KeyCode::Char('c')
                                && key.modifiers.contains(termina::event::Modifiers::CONTROL);
                            if quit {
                                break;
                            }
                            match keymaps.resolve_with_insert_fallback(
                                state.mode,
                                &mut pending,
                                key,
                            ) {
                                Resolution::Command(command) => {
                                    let is_peek = matches!(&command, Command::PeekDefinition);
                                    state = session.request(&command).await?;
                                    // PeekDefinition の応答: ポップアップ内容を
                                    // ローカルに移す（スナップショットには残さない）
                                    if let Some(p) = state.peek.take() {
                                        peek = Some(p);
                                    } else if is_peek {
                                        // 定義なし（LSP 非対応・未解析・解決不能など）
                                        flash = Some("no definition".into());
                                    }
                                }
                                _ => {} // pending 変化の描画は共通ループ末尾で行う
                            }
                        }
                        redraw = true;
                    }
                    Event::WindowResized(size) => {
                        width = size.cols;
                        height = size.rows;
                        state = session
                            .request(&Command::SetViewport {
                                height: height as usize,
                            })
                            .await?;
                        redraw = true;
                    }
                    _ => {}
                }
            }
            push = session.pushes.recv() => {
                match push {
                    // 内容が変わったときだけ再描画する。generation 比較でなく内容
                    // 比較にする理由: LSP の診断・inlay hint の反映（settle ループ）
                    // は generation を進めないが画面は変わる（#24 のフィードバック）。
                    // 自分自身の変更（応答で描画済み）は内容が同一なので捨てられる。
                    Some(snapshot) if snapshot != state => {
                        state = snapshot;
                        redraw = true;
                    }
                    Some(_) => {} // 内容が同一（自分自身の変更）: 描画済み
                    None => break, // daemon の切断（EOF）
                }
            }
        }
        if redraw {
            render::draw_with_cache(
                &mut *terminal,
                &scheme,
                capability,
                no_color,
                &state,
                &pending,
                command_line.as_deref(),
                flash.as_deref(),
                width,
                height,
                &mut li_cache,
            )?;
            // 定義ポップアップはメイン描画の後に重ねる（クライアントローカルな
            // 一時表示 — render_text に載せると全テストのシグネチャを汚す）。
            if let Some(p) = &peek {
                render::draw_peek_popup(
                    &mut *terminal,
                    &scheme,
                    capability,
                    no_color,
                    p,
                    width,
                    height,
                )?;
            }
            terminal.flush()?;
        }
    }

    // 終了処理は TerminalGuard の Drop が行う（M4: エラー経路でも必ず復旧する）
    Ok(())
}

/// コマンドラインの実行アクション（`:w` 等）。
#[derive(Debug, PartialEq, Eq)]
enum CommandLineAction {
    /// `:w` — 保存して続行。
    Save,
    /// `:q` / `:q!` — 終了。daemon が文書状態を保持し続けるので破棄はない。
    Quit,
    /// `:wq` — 保存してから終了（保存に失敗したら終了しない）。
    SaveThenQuit,
    /// `:colorscheme [name]` — 引数なしは現在のスキーム名を表示。
    Colorscheme(Option<String>),
    /// 未知のコマンド。
    Unknown(String),
}

/// コマンドライン文字列を解釈する（テスト容易性のため純粋関数）。
///
/// 引数付きコマンド（`:colorscheme <name>`）は空白区切りで解釈する。
/// 単語コマンド（`w` / `q` / `q!` / `wq`）の挙動は従来どおりで、
/// 引数が付いた入力は Unknown に丸ごと載せる（例: `w foo`）。
fn parse_command(input: &str) -> CommandLineAction {
    let trimmed = input.trim();
    match trimmed {
        "w" => return CommandLineAction::Save,
        "q" | "q!" => return CommandLineAction::Quit,
        "wq" => return CommandLineAction::SaveThenQuit,
        _ => {}
    }
    let mut parts = trimmed.split_whitespace();
    if parts.next() == Some("colorscheme") {
        return CommandLineAction::Colorscheme(parts.next().map(str::to_string));
    }
    CommandLineAction::Unknown(trimmed.to_string())
}

/// `:colorscheme [name]` の適用（純粋関数 — テスト容易性）。
///
/// 既知名は `scheme` を差し替えて None、不明名・引数なしは表示すべき flash を返す。
/// 解決は起動時と同じ規則（ユーザーファイル優先 → 組み込み — ADR-0022）。
fn apply_colorscheme(
    scheme: &mut Colorscheme,
    name: Option<&str>,
    schemes_dir: &Path,
) -> Option<String> {
    match name {
        Some(name) => match colorscheme::resolve(name, schemes_dir) {
            Some(s) => {
                *scheme = s;
                None
            }
            None => Some(format!("unknown colorscheme: {name}")),
        },
        None => Some(format!("colorscheme: {}", scheme.name)),
    }
}

/// 接続直後に Hello（クライアント種別の宣言）を送る（ADR-0012）。
/// 応答は待たない（最初のコマンドの応答スナップショットに乗る）。
pub(crate) async fn send_hello(
    write_half: &mut OwnedWriteHalf,
    kind: ClientKind,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(&Hello { kind }).expect("Hello はシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await
}

/// 任意パスの inlay hint を取得する（ADR-0020）。`Command::GetInlayHints` の
/// 応答はスナップショットでなく `ServerMessage::Hints` なので専用経路。
/// エージェント（session CLI）が全文テキストなしで型構造を参照するためのもの。
pub(crate) async fn request_hints(
    write_half: &mut OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
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
pub(crate) async fn request_peek(
    write_half: &mut OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
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
        Err(e) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("不正な応答: {e}"),
        )),
    }
}

/// メッセージを送り、応答スナップショットを1つ受け取る。TUI と session CLI の両方から使う。
pub(crate) async fn request<T: serde::Serialize>(
    write_half: &mut OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
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
        Err(e) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("不正な応答: {e}"),
        )),
    }
}

/// TUI 用の永続接続（ADR-0013）。読み取りは専用タスクに任せ、コマンド応答
/// （Response）とサーバー発の状態通知（Push）を振り分ける。Headless の
/// ワンショット CLI（[`request`]）とは別経路。
struct Session {
    write: OwnedWriteHalf,
    responses: mpsc::UnboundedReceiver<std::io::Result<StateSnapshot>>,
    pushes: mpsc::UnboundedReceiver<StateSnapshot>,
}

impl Session {
    /// 接続し、Hello（Interactive 宣言）を送り、読み取りタスクを起動する。
    async fn connect(path: &std::path::Path) -> std::io::Result<Session> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, mut write) = stream.into_split();
        // ADR-0012: 接続直後に Hello（対話型宣言）を送る
        send_hello(&mut write, ClientKind::Interactive).await?;
        let (res_tx, responses) = mpsc::unbounded_channel();
        let (push_tx, pushes) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(read_half, res_tx, push_tx));
        Ok(Session {
            write,
            responses,
            pushes,
        })
    }

    /// コマンドを送り、応答スナップショットを待つ。push は [`Session::pushes`] に届く。
    async fn request<T: serde::Serialize>(&mut self, message: &T) -> std::io::Result<StateSnapshot> {
        let mut line = serde_json::to_string(message).expect("メッセージはシリアライズ可能");
        line.push('\n');
        self.write.write_all(line.as_bytes()).await?;
        self.write.flush().await?;
        self.responses.recv().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "daemon との接続が切れた")
        })?
    }
}

/// 接続の読み取り側: NDJSON を [`ServerMessage`] として解釈し、応答と push を
/// 振り分ける。EOF/エラーでチャネルを閉じる（受信側が None/Err を受け取る）。
async fn read_loop(
    read_half: tokio::net::unix::OwnedReadHalf,
    res_tx: mpsc::UnboundedSender<std::io::Result<StateSnapshot>>,
    push_tx: mpsc::UnboundedSender<StateSnapshot>,
) {
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // daemon の切断（EOF）: チャネルが閉じる
            Ok(_) => {}
            Err(e) => {
                let _ = res_tx.send(Err(e));
                return;
            }
        }
        match serde_json::from_str::<ServerMessage>(line.trim()) {
            Ok(ServerMessage::Response { snapshot }) => {
                if res_tx.send(Ok(snapshot)).is_err() {
                    return; // メインループが落ちた
                }
            }
            Ok(ServerMessage::Push { snapshot }) => {
                if push_tx.send(snapshot).is_err() {
                    return;
                }
            }
            // TUI は GetInlayHints / PeekDefinitionAt を送らない（エージェント専用経路）。
            // 万一届いても応答は無視する。
            Ok(ServerMessage::Hints { .. }) | Ok(ServerMessage::Peek { .. }) => {}
            Err(e) => {
                let _ = res_tx.send(Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("不正な応答: {e}"),
                )));
            }
        }
    }
}

/// daemon が動いていなければ自動起動し、socket が現れるまで待つ。TUI と session CLI の両方から使う。
///
/// daemon はクライアントの cwd を引き継いで spawn される。相対パスの解決は
/// この cwd 基準になるため、クライアントはパスを絶対化してから送る
/// （[`absolutize`]）。
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

    #[test]
    fn parse_command_maps_w_q_and_wq() {
        assert_eq!(parse_command("w"), CommandLineAction::Save);
        assert_eq!(parse_command(" w "), CommandLineAction::Save, "前後空白は無視");
        assert_eq!(parse_command("q"), CommandLineAction::Quit);
        assert_eq!(parse_command("q!"), CommandLineAction::Quit);
        assert_eq!(parse_command("wq"), CommandLineAction::SaveThenQuit);
        assert_eq!(
            parse_command("frobnicate"),
            CommandLineAction::Unknown("frobnicate".into())
        );
        assert_eq!(
            parse_command(""),
            CommandLineAction::Unknown("".into()),
            "空コマンドもエラー表示"
        );
        // 単語コマンドに引数が付いたら従来どおり Unknown（丸ごと flash）
        assert_eq!(
            parse_command("w foo"),
            CommandLineAction::Unknown("w foo".into())
        );
    }

    #[test]
    fn parse_command_colorscheme_with_args() {
        assert_eq!(
            parse_command("colorscheme vivid"),
            CommandLineAction::Colorscheme(Some("vivid".into()))
        );
        assert_eq!(
            parse_command(" colorscheme  vivid "),
            CommandLineAction::Colorscheme(Some("vivid".into())),
            "前後空白・複数空白を無視"
        );
        assert_eq!(
            parse_command("colorscheme"),
            CommandLineAction::Colorscheme(None),
            "引数なし"
        );
        assert_eq!(
            parse_command("colorschemefoo"),
            CommandLineAction::Unknown("colorschemefoo".into()),
            "接頭辞だけでは colorscheme と解釈しない"
        );
    }

    #[test]
    fn apply_colorscheme_switches_and_flashes() {
        // スキームファイルのない dir → 組み込みのみで解決される
        let empty_dir = Path::new("/nonexistent/mina-test-colorschemes");
        let mut scheme = colorscheme::DEFAULT.clone();
        // 既知名: 切替され flash なし
        assert_eq!(apply_colorscheme(&mut scheme, Some("vivid"), empty_dir), None);
        assert_eq!(scheme.name, "vivid");
        // 戻せる
        assert_eq!(apply_colorscheme(&mut scheme, Some("default"), empty_dir), None);
        assert_eq!(scheme.name, "default");
        // 不明名: flash を返し切替しない
        assert_eq!(
            apply_colorscheme(&mut scheme, Some("nope"), empty_dir),
            Some("unknown colorscheme: nope".into())
        );
        assert_eq!(scheme.name, "default");
        // 引数なし: 現在のスキーム名を flash
        assert_eq!(
            apply_colorscheme(&mut scheme, None, empty_dir),
            Some("colorscheme: default".into())
        );
    }
}
