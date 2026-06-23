//! イベントとキーバインド。
//!
//! `handle_key` がキー入力を `ShellEvent` 列に変換する唯一の入口。
//! カーソル移動など「行エディタ内で完結する変更」はここで行い、
//! 端末 I/O を伴う操作 (実行・補完表示・再描画) はイベントとして返す。

use crate::editor::LineEditor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub enum ShellEvent {
    Exit,
    CancelInput,
    RedrawPrompt,
    ExecuteCommand,
    ShowCompletion,
    AcceptGhost,
    ShowHistoryFzf,
    ShowFileFzf,
    ShowRecentPathFzf,
    HistoryPrev,
    HistoryNext,
    /// Space キー押下。main.rs で abbr 展開を試みてからスペースを挿入する。
    InsertSpace,
    /// Ctrl+L: 画面をクリアしてプロンプトを再描画する。
    ClearScreen,
    /// Alt+.: 直前コマンドの最終引数を挿入する。
    InsertLastArg,
}

pub fn handle_key(ed: &mut LineEditor, key: KeyEvent) -> Vec<ShellEvent> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let at_end = ed.cursor() == ed.line().len();

    match key.code {
        // 空行での C-d のみ終了 (fish と同じ)、それ以外は Delete (カーソル後の1文字削除)
        KeyCode::Char('d') if ctrl && ed.is_empty() => vec![ShellEvent::Exit],
        KeyCode::Char('d') if ctrl => {
            ed.delete();
            vec![ShellEvent::RedrawPrompt]
        }
        // C-h は Backspace と同義
        KeyCode::Char('h') if ctrl => {
            ed.backspace();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Char('c') if ctrl => vec![ShellEvent::CancelInput],
        KeyCode::Char('a') if ctrl => {
            ed.move_home();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Char('e') if ctrl => {
            ed.move_end();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Char('b') if ctrl => {
            ed.move_left();
            vec![ShellEvent::RedrawPrompt]
        }
        // 行末なら ghost 受け入れ、そうでなければ右移動
        KeyCode::Char('f') if ctrl => {
            if at_end {
                vec![ShellEvent::AcceptGhost]
            } else {
                ed.move_right();
                vec![ShellEvent::RedrawPrompt]
            }
        }
        KeyCode::Char('w') if ctrl => {
            ed.delete_word_backward();
            vec![ShellEvent::RedrawPrompt]
        }
        // Ctrl+U: カーソルより前を削除 / Ctrl+K: カーソル以降を削除
        KeyCode::Char('u') if ctrl => {
            ed.kill_to_start();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Char('k') if ctrl => {
            ed.kill_to_end();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Char('l') if ctrl => vec![ShellEvent::ClearScreen],
        KeyCode::Char('p') if ctrl => vec![ShellEvent::HistoryPrev],
        KeyCode::Char('n') if ctrl => vec![ShellEvent::HistoryNext],
        KeyCode::Char('g') if ctrl => vec![ShellEvent::ShowRecentPathFzf],
        KeyCode::Char('r') if ctrl => vec![ShellEvent::ShowHistoryFzf],
        KeyCode::Char('t') if ctrl => vec![ShellEvent::ShowFileFzf],

        KeyCode::Tab => vec![ShellEvent::ShowCompletion],
        // ゴースト消去・abbr 展開は main.rs 側で行うため take() しない
        KeyCode::Enter => vec![ShellEvent::ExecuteCommand],

        KeyCode::Left if ctrl => {
            ed.move_word_left();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Left => {
            ed.move_left();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Right if ctrl => {
            ed.move_word_right();
            vec![ShellEvent::RedrawPrompt]
        }
        // 行末なら ghost 受け入れ、そうでなければ右移動
        KeyCode::Right => {
            if at_end {
                vec![ShellEvent::AcceptGhost]
            } else {
                ed.move_right();
                vec![ShellEvent::RedrawPrompt]
            }
        }
        KeyCode::Up => vec![ShellEvent::HistoryPrev],
        KeyCode::Down => vec![ShellEvent::HistoryNext],
        KeyCode::Home => {
            ed.move_home();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::End => {
            ed.move_end();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Backspace => {
            ed.backspace();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Delete => {
            ed.delete();
            vec![ShellEvent::RedrawPrompt]
        }

        // Alt+.: 直前コマンドの最終引数を挿入する
        KeyCode::Char('.') if alt => vec![ShellEvent::InsertLastArg],

        // Space: abbr 展開の機会を main.rs に委譲する
        KeyCode::Char(' ') if !ctrl && !alt => vec![ShellEvent::InsertSpace],

        // 通常の文字入力 (制御・Alt 修飾は除外)
        KeyCode::Char(c) if !ctrl && !alt => {
            ed.insert(c);
            vec![ShellEvent::RedrawPrompt]
        }

        _ => vec![],
    }
}
