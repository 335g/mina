//! クライアント（TUI）: daemon にコマンドを送り、StateSnapshot を受け取って描画する。
//!
//! Open のパスはクライアント側で絶対化してから送る — daemon は常駐で cwd が
//! 起動時のディレクトリのままなので、相対パスの解決を daemon に任せると
//! 別ディレクトリから起動したクライアントの意図と食い違う（[`mina_conn::absolutize`]）。
//!
//! リクエスト/レスポンスのみ（ADR-0006）。編集状態は持たない — キーイベントを
//! キーマップで Command に解決して送り、返ってきたスナップショットを描画するだけ。
//! daemon が動いていなければ自動起動する（ADR-0005）。

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use futures_lite::StreamExt;
use mina_protocol::{
    ClientKind, Command, Direction, Mode, Peek, ReferenceLocation,
    ServerMessage, StateSnapshot,
};
use termina::event::{KeyCode, KeyEvent, KeyEventKind, Modifiers};
use termina::{Event, EventStream, PlatformTerminal, Terminal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixStream, unix::OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::colorscheme::{self, Colorscheme};
use crate::keymap::{Keymaps, Resolution};
use mina_conn as conn;
use crate::render;

const ALT_SCREEN_ON: &str = "\x1b[?1049h";
const ALT_SCREEN_OFF: &str = "\x1b[?1049l";
const CURSOR_HIDE: &str = "\x1b[?25l";
const CURSOR_SHOW: &str = "\x1b[?25h";

/// ステータス行に入力バッファを出すプロンプト（Helix 流の `:` / `/` 等）。
/// クライアントローカル — daemon には確定時だけコマンドを送る（検索は
/// ライブで送る）。
#[derive(Debug)]
enum Prompt {
    /// `:` コマンドライン。
    Command(String),
    /// `/`（forward=true）または `?` の検索プロンプト。キー入力のたびに
    /// [`Command::Search`] を送る（ライブ検索 — Helix と同じ）。
    Search { buf: String, forward: bool },
    /// `r` 置換: 次の文字キーで選択/カーソル文字を置換する。
    Replace,
    /// `Space r` リネーム: カーソル位置の単語（`old`）を新しい名前に変える。
    Rename { buf: String, old: String },
}

impl Prompt {
    fn prefix(&self) -> char {
        match self {
            Prompt::Command(_) => ':',
            Prompt::Search { forward, .. } => {
                if *forward {
                    '/'
                } else {
                    '?'
                }
            }
            Prompt::Replace => 'r',
            Prompt::Rename { .. } => 'R',
        }
    }

    fn buf(&self) -> &str {
        match self {
            Prompt::Command(b) | Prompt::Search { buf: b, .. } | Prompt::Rename { buf: b, .. } => b,
            Prompt::Replace => "",
        }
    }
}

/// カーソル位置の単語（`*` / rename / references の対象）をスナップショットから
/// 取り出す。非単語文字の上・範囲外なら `None`。
fn word_at_cursor(state: &StateSnapshot) -> Option<String> {
    let head = state
        .selection
        .get(state.primary_index)
        .map(|r| r.head)
        .unwrap_or(0);
    let doc = mina_text::Document::from(state.text.as_str());
    let (s, e) = mina_text::word_at(&doc, head)?;
    let chars: Vec<char> = state.text.chars().collect();
    Some(chars[s..e].iter().collect())
}

/// プロンプトを開ける文字キーか（修飾キーなし / Shift のみ）。Insert では
/// 文字入力なのでプロンプトは開かない。
fn is_insertable(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(_))
        && (key.modifiers.is_empty() || key.modifiers == Modifiers::SHIFT)
}

/// Space リーダーの途中（pending が Space 1つ）か。`Space k` はキーマップが
/// 解決し、`Space r`/`Space h` はクライアントがリネーム/参照を開く。
fn is_space_leader(pending: &[KeyEvent]) -> bool {
    pending.len() == 1
        && pending[0].code == KeyCode::Char(' ')
        && pending[0].modifiers.is_empty()
}

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
    let socket = mina_protocol::socket_path();
    // デーモンがいなければ起動する（クライアント視点のライフサイクル。spawn 元は
    // 自身 = `minae daemon serve` を持つ bin。将来の TUI リポジトリでは別ポリシー）。
    if conn::connect(&socket).await.is_err() {
        conn::spawn_daemon(&std::env::current_exe()?, &["daemon", "serve"])?;
        conn::wait_ready(&socket, 50).await?;
    }
    // ADR-0027: 切断時カーソルリセットの宣言を Hello に載せるため、config は
    // 接続前に読む（colorscheme 解決でも同じ値を使い回す）。
    let config = crate::config::load();
    let mut session = Session::connect(&socket, config.reset_cursor_on_disconnect).await?;

    // 初回コマンド: ファイル指定があれば Open、なければ GetState。
    // Open のパスは絶対化して送る — daemon は常駐で cwd が起動時のディレクトリの
    // ままなので、相対パスを daemon 側で解決すると別ディレクトリから起動した
    // クライアントの意図と食い違う（`cannot open` になる）。
    let first = match file {
        Some(path) => Command::Open { path: conn::absolutize(path) },
        None => Command::GetState,
    };
    // M5: 初回応答（Open の失敗 status など）を破棄せず保持する
    let mut state = session.request(&first).await?;
    let first_status = state.status.take();

    // 現在の Colorscheme: config.toml の名前から解決（ユーザーファイル優先、次に組み込み、
    // 不明なら警告 + 組み込み DEFAULT — ADR-0022）。クライアントローカル — daemon 非関与。
    // 警告は端末セットアップ前に stderr へ出す（raw モード・代替画面中の表示崩れを避ける）。
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
    // プロンプト（`:` コマンド / `/` `?` 検索 / `r` 置換 / `Space r` リネーム）。
    // Some の間はキー入力がプロンプト編集になり、ステータス行に表示される。
    let mut prompt: Option<Prompt> = None;
    // クライアント側の一時メッセージ（未知コマンド等）。次のキーで消える。
    let mut flash: Option<String> = None;
    // 定義ポップアップ（Space k / PeekDefinition）の内容。次のキーで消える
    // クライアントローカルな一時表示。応答スナップショットの `peek` フィールド
    // から移し替える — スナップショット自体には残さない（push との内容比較を
    // 汚さず、`state != state` の再描画判定を壊さないため）。
    let mut peek: Option<Peek> = None;
    // 参照ポップアップ（Space h / References）の内容。次キーで消える。
    let mut references: Option<(String, usize, Vec<ReferenceLocation>)> = None;
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
        prompt.as_ref().map(|p| (p.prefix(), p.buf())),
        flash.as_deref(),
        width,
        height,
        &mut li_cache,
    )?;
    terminal.flush()?;

    // ADR-0013: キーイベントと daemon からの push を並列に待つ。push は
    // 他クライアント（agent 等）の変更を即時反映する。generation が最後に
    // 描画したものと同じなら捨てる（自分自身の変更は応答で描画済み）。
    // ADR-0028: スピナー用のタイマー（~15Hz）を第3分岐に足す。Activity が
    // active な間だけ再描画を駆動する（アニメーションはローカル描画の問題で
    // daemon に一切触れない）。コマンド応答待ち中はループ自体がブロックする
    // ため回らない — Open 応答待ちの静止は設計で許容済み（ADR-0028）。
    let mut tick = tokio::time::interval(Duration::from_millis(66));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                        references = None; // 参照ポップアップも同じ
                        if state.deleted.is_some() {
                            prompt = None;
                            state = session.request(&Command::Close).await?;
                        } else if let Some(p) = prompt.take() {
                            // プロンプト編集。Enter/Esc/C-c で確定・キャンセルする。
                            match p {
                                Prompt::Replace => {
                                    // r: 次の文字キーで置換確定。Esc/C-c でキャンセル。
                                    match key.code {
                                        KeyCode::Char(c)
                                            if key.modifiers.is_empty()
                                                || key.modifiers == Modifiers::SHIFT =>
                                        {
                                            state = session
                                                .request(&Command::Replace {
                                                    text: c.to_string(),
                                                })
                                                .await?;
                                        }
                                        KeyCode::Escape => {}
                                        KeyCode::Char('c')
                                            if key.modifiers.contains(Modifiers::CONTROL) => {}
                                        _ => prompt = Some(Prompt::Replace),
                                    }
                                }
                                Prompt::Command(mut buf) => {
                                    match key.code {
                                        KeyCode::Char(c)
                                            if key.modifiers.is_empty()
                                                || key.modifiers == Modifiers::SHIFT =>
                                        {
                                            buf.push(c);
                                            prompt = Some(Prompt::Command(buf));
                                        }
                                        KeyCode::Char('c')
                                            if key.modifiers.contains(Modifiers::CONTROL) => {}
                                        KeyCode::Backspace => {
                                            buf.pop();
                                            prompt = Some(Prompt::Command(buf));
                                        }
                                        KeyCode::Escape => {}
                                        KeyCode::Enter => {
                                            let action = parse_command(&buf);
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
                                        _ => prompt = Some(Prompt::Command(buf)),
                                    }
                                }
                                Prompt::Search { mut buf, forward } => {
                                    // ライブ検索: キー入力のたびに Search を送る
                                    let keep = match key.code {
                                        KeyCode::Char(c)
                                            if key.modifiers.is_empty()
                                                || key.modifiers == Modifiers::SHIFT =>
                                        {
                                            buf.push(c);
                                            true
                                        }
                                        KeyCode::Char('c')
                                            if key.modifiers.contains(Modifiers::CONTROL) => false,
                                        KeyCode::Backspace => {
                                            buf.pop();
                                            true
                                        }
                                        KeyCode::Escape | KeyCode::Enter => {
                                            // 確定/キャンセル: 最後のライブ検索が現状
                                            false
                                        }
                                        _ => true,
                                    };
                                    if keep {
                                        if !buf.is_empty() {
                                            state = session
                                                .request(&Command::Search {
                                                    query: buf.clone(),
                                                    direction: if forward {
                                                        Direction::Forward
                                                    } else {
                                                        Direction::Backward
                                                    },
                                                })
                                                .await?;
                                        }
                                        prompt = Some(Prompt::Search { buf, forward });
                                    }
                                }
                                Prompt::Rename { mut buf, old } => {
                                    match key.code {
                                        KeyCode::Char(c)
                                            if key.modifiers.is_empty()
                                                || key.modifiers == Modifiers::SHIFT =>
                                        {
                                            buf.push(c);
                                            prompt = Some(Prompt::Rename { buf, old });
                                        }
                                        KeyCode::Char('c')
                                            if key.modifiers.contains(Modifiers::CONTROL) => {}
                                        KeyCode::Backspace => {
                                            buf.pop();
                                            prompt = Some(Prompt::Rename { buf, old });
                                        }
                                        KeyCode::Escape => {}
                                        KeyCode::Enter => {
                                            let new = buf.trim().to_string();
                                            match state.path.clone() {
                                                Some(path) if !new.is_empty() => {
                                                    match session.request_rename(&path, &old, &new).await
                                                    {
                                                        Ok(result) => {
                                                            if let Some(err) = result.error {
                                                                flash = Some(format!(
                                                                    "rename failed: {err}"
                                                                ));
                                                            } else {
                                                                flash = Some(format!(
                                                                    "renamed: {} files, {} edits",
                                                                    result.files, result.edits
                                                                ));
                                                            }
                                                            // リネームは全文を変える — バッファを最新化
                                                            state = session
                                                                .request(&Command::GetState)
                                                                .await?;
                                                        }
                                                        Err(e) => {
                                                            flash = Some(format!(
                                                                "rename error: {e}"
                                                            ));
                                                        }
                                                    }
                                                }
                                                Some(_) => {
                                                    flash = Some("rename: empty name".into())
                                                }
                                                None => flash = Some("no file open".into()),
                                            }
                                        }
                                        _ => prompt = Some(Prompt::Rename { buf, old }),
                                    }
                                }
                            }
                        } else if is_insertable(&key) && state.mode != Mode::Insert {
                            // プロンプトを開くキー（Normal/Select）。`: ` はコマンド、
                            // `/` `?` は検索、`r` は置換、Space リーダーの r/h は
                            // リネーム/参照。Insert ではすべて文字入力（fallback）。
                            match key.code {
                                KeyCode::Char(':') => {
                                    pending.clear();
                                    prompt = Some(Prompt::Command(String::new()));
                                }
                                KeyCode::Char('/') | KeyCode::Char('?') => {
                                    pending.clear();
                                    let forward = key.code == KeyCode::Char('/');
                                    prompt = Some(Prompt::Search {
                                        buf: String::new(),
                                        forward,
                                    });
                                }
                                KeyCode::Char('r') if !is_space_leader(&pending) => {
                                    pending.clear();
                                    prompt = Some(Prompt::Replace);
                                }
                                KeyCode::Char('r') if is_space_leader(&pending) => {
                                    // Space r: カーソル位置のシンボルをリネーム
                                    pending.clear();
                                    match word_at_cursor(&state) {
                                        Some(old) => {
                                            prompt = Some(Prompt::Rename {
                                                buf: String::new(),
                                                old,
                                            })
                                        }
                                        None => {
                                            flash = Some("no symbol under cursor".into())
                                        }
                                    }
                                }
                                KeyCode::Char('h') if is_space_leader(&pending) => {
                                    // Space h: カーソル位置のシンボルの参照を列挙
                                    pending.clear();
                                    match (state.path.clone(), word_at_cursor(&state)) {
                                        (Some(path), Some(old)) => {
                                            match session.request_references(&path, &old).await {
                                                Ok(result) => {
                                                    if let Some(err) = result.error {
                                                        flash = Some(format!(
                                                            "references failed: {err}"
                                                        ));
                                                    } else {
                                                        let mut locs =
                                                            result.locations.clone();
                                                        locs.truncate(20);
                                                        references = Some((
                                                            result.path.clone(),
                                                            result.total,
                                                            locs,
                                                        ));
                                                    }
                                                }
                                                Err(e) => {
                                                    flash = Some(format!(
                                                        "references error: {e}"
                                                    ));
                                                }
                                            }
                                        }
                                        (None, _) => flash = Some("no file open".into()),
                                        (_, None) => {
                                            flash = Some("no symbol under cursor".into())
                                        }
                                    }
                                }
                                _ => {
                                    // その他: キーマップに委ねる（Space k の確定等）
                                    match keymaps.resolve_with_insert_fallback(
                                        state.mode,
                                        &mut pending,
                                        key,
                                    ) {
                                        Resolution::Command(command) => {
                                            let is_peek =
                                                matches!(&command, Command::PeekDefinition);
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
                            }
                        } else {
                            // Insert モード（または修飾キー付き）: キーマップで解決
                            // （未バインドの文字は文字入力にフォールバック）
                            let quit = key.code == KeyCode::Char('c')
                                && key.modifiers.contains(Modifiers::CONTROL);
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
                                    if let Some(p) = state.peek.take() {
                                        peek = Some(p);
                                    } else if is_peek {
                                        flash = Some("no definition".into());
                                    }
                                }
                                _ => {}
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
            _ = tick.tick() => {
                if !state.activities.is_empty() {
                    redraw = true; // スピナーを回す
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
                prompt.as_ref().map(|p| (p.prefix(), p.buf())),
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
            // 参照ポップアップ（Space h）
            if let Some((path, total, locs)) = &references {
                render::draw_refs_popup(
                    &mut *terminal,
                    &scheme,
                    capability,
                    no_color,
                    path,
                    *total,
                    locs,
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

/// TUI 用の永続接続（ADR-0013）。読み取りは専用タスクに任せ、コマンド応答
/// （Response）とサーバー発の状態通知（Push）を振り分ける。Headless の
/// ワンショット CLI（[`request`]）とは別経路。
struct Session {
    write: OwnedWriteHalf,
    responses: mpsc::UnboundedReceiver<std::io::Result<StateSnapshot>>,
    pushes: mpsc::UnboundedReceiver<StateSnapshot>,
    /// 軽量応答（RenameResult / ReferencesResult など。スナップショットを運ばない）
    /// の到着口。要求コマンドを送った側が専用メソッド（`request_rename` 等）で
    /// 応答を受け取る。read_loop が種別ごとに振り分ける。
    semantic: mpsc::UnboundedReceiver<ServerMessage>,
}

/// `Space r` のリネーム結果（クライアント表示用）。
struct RenameOutcome {
    files: usize,
    edits: usize,
    error: Option<String>,
}

/// `Space h` の参照結果（クライアント表示用）。
struct RefsOutcome {
    path: String,
    locations: Vec<ReferenceLocation>,
    total: usize,
    error: Option<String>,
}

impl Session {
    /// 接続し、Hello（Interactive 宣言 + 切断時カーソルリセットの宣言）を送り、
    /// 読み取りタスクを起動する。
    async fn connect(
        path: &std::path::Path,
        reset_cursor_on_disconnect: bool,
    ) -> std::io::Result<Session> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, mut write) = stream.into_split();
        // ADR-0012: 接続直後に Hello（対話型宣言）を送る
        conn::send_hello(&mut write, ClientKind::Interactive, reset_cursor_on_disconnect).await?;
        let (res_tx, responses) = mpsc::unbounded_channel();
        let (push_tx, pushes) = mpsc::unbounded_channel();
        let (sem_tx, semantic) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(read_half, res_tx, push_tx, sem_tx));
        Ok(Session {
            write,
            responses,
            pushes,
            semantic,
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


    /// シンボルの意味リネーム（ADR-0029。`Space r`）。応答は軽量な
    /// [`ServerMessage::RenameResult`]（全文を運ばない）で、`semantic` に届く。
    /// シンボルの意味リネーム（ADR-0029。`Space r`）。応答は軽量な
    /// [`ServerMessage::RenameResult`]（全文を運ばない）で、`semantic` に届く。
    async fn request_rename(
        &mut self,
        path: &str,
        old: &str,
        new: &str,
    ) -> std::io::Result<RenameOutcome> {
        self.send_raw(&Command::Rename {
            path: path.to_string(),
            old: old.to_string(),
            new: new.to_string(),
        })
        .await?;
        match self.recv_semantic().await? {
            ServerMessage::RenameResult {
                files, edits, error, ..
            } => Ok(RenameOutcome {
                files,
                edits,
                error,
            }),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unexpected response to Rename: {other:?}"),
            )),
        }
    }

    /// シンボルの参照位置の列挙（ADR-0029。`Space h`）。応答は軽量な
    /// [`ServerMessage::ReferencesResult`]。
    /// シンボルの参照位置の列挙（ADR-0029。`Space h`）。応答は軽量な
    /// [`ServerMessage::ReferencesResult`]。
    async fn request_references(
        &mut self,
        path: &str,
        old: &str,
    ) -> std::io::Result<RefsOutcome> {
        self.send_raw(&Command::References {
            path: path.to_string(),
            old: old.to_string(),
        })
        .await?;
        match self.recv_semantic().await? {
            ServerMessage::ReferencesResult {
                path,
                locations,
                total,
                error,
            } => Ok(RefsOutcome {
                path,
                locations,
                total,
                error,
            }),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unexpected response to References: {other:?}"),
            )),
        }
    }

    /// `semantic` チャネルの次のメッセージを待つ（TUI は同時に1つの軽量応答
    /// しか要求しないので、順序保証された1件目をそのまま使う）。
    async fn recv_semantic(&mut self) -> std::io::Result<ServerMessage> {
        self.semantic.recv().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "daemon との接続が切れた")
        })
    }

    async fn send_raw<T: serde::Serialize>(&mut self, message: &T) -> std::io::Result<()> {
        let mut line = serde_json::to_string(message).expect("メッセージはシリアライズ可能");
        line.push('\n');
        self.write.write_all(line.as_bytes()).await?;
        self.write.flush().await
    }
}

/// 接続の読み取り側: NDJSON を [`ServerMessage`] として解釈し、応答と push を
/// 振り分ける。EOF/エラーでチャネルを閉じる（受信側が None/Err を受け取る）。
async fn read_loop(
    read_half: tokio::net::unix::OwnedReadHalf,
    res_tx: mpsc::UnboundedSender<std::io::Result<StateSnapshot>>,
    push_tx: mpsc::UnboundedSender<StateSnapshot>,
    sem_tx: mpsc::UnboundedSender<ServerMessage>,
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
            // ADR-0029: Rename / References の応答はスナップショットでなく軽量な
            // RenameResult / ReferencesResult。TUI の Space r / Space h 用に
            // semantic チャネルへ振り分ける（応答は要求元のこの接続にだけ返る）。
            Ok(msg @ ServerMessage::RenameResult { .. })
            | Ok(msg @ ServerMessage::ReferencesResult { .. }) => {
                if sem_tx.send(msg).is_err() {
                    return;
                }
            }
            // TUI は GetInlayHints / PeekDefinitionAt / GetServerInfo を送らない
            // （エージェント専用経路）。万一届いても応答は無視する。
            Ok(ServerMessage::Hints { .. })
            | Ok(ServerMessage::Peek { .. })
            | Ok(ServerMessage::ServerInfo { .. })
            | Ok(ServerMessage::Outline { .. })
            | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {}
            Err(e) => {
                let _ = res_tx.send(Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("不正な応答: {e}"),
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mina_protocol::Range;

    #[test]
    fn word_at_cursor_extracts_identifier_at_selection_head() {
        // `Space r` / `Space h` / `*` の対象（カーソル位置の単語）を取り出す
        let mut state = StateSnapshot {
            text: "let value = 42;\n".into(),
            selection: vec![Range { anchor: 6, head: 6 }],
            ..Default::default()
        };
        assert_eq!(word_at_cursor(&state).as_deref(), Some("value"));
        // primary でない range は無視（primary_index の range を使う）
        state.selection = vec![Range { anchor: 0, head: 0 }, Range { anchor: 6, head: 6 }];
        state.primary_index = 1;
        assert_eq!(word_at_cursor(&state).as_deref(), Some("value"));
        // 空白の上は None（"let " の末尾 3 は空白）
        state.selection = vec![Range { anchor: 3, head: 3 }];
        state.primary_index = 0;
        assert_eq!(word_at_cursor(&state), None);
        // 範囲外も None
        state.selection = vec![Range { anchor: 999, head: 999 }];
        assert_eq!(word_at_cursor(&state), None);
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
        let empty_dir = Path::new("/nonexistent/minae-test-colorschemes");
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
