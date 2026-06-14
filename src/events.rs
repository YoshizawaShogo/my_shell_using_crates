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
    ExecuteCommand(String),
    ShowCompletion,
}

pub fn handle_key(ed: &mut LineEditor, key: KeyEvent) -> Vec<ShellEvent> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        // 空行での C-d のみ終了 (fish と同じ)
        KeyCode::Char('d') if ctrl && ed.is_empty() => vec![ShellEvent::Exit],
        KeyCode::Char('c') if ctrl => vec![ShellEvent::CancelInput],
        KeyCode::Char('a') if ctrl => {
            ed.move_home();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Char('e') if ctrl => {
            ed.move_end();
            vec![ShellEvent::RedrawPrompt]
        }

        KeyCode::Tab => vec![ShellEvent::ShowCompletion],
        KeyCode::Enter => vec![ShellEvent::ExecuteCommand(ed.take())],

        KeyCode::Left => {
            ed.move_left();
            vec![ShellEvent::RedrawPrompt]
        }
        KeyCode::Right => {
            ed.move_right();
            vec![ShellEvent::RedrawPrompt]
        }
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

        // 通常の文字入力 (制御・Alt 修飾は除外)
        KeyCode::Char(c) if !ctrl && !alt => {
            ed.insert(c);
            vec![ShellEvent::RedrawPrompt]
        }

        _ => vec![],
    }
}
