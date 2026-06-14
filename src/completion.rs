//! 補完。
//!
//! `CompletionSource` が「何を候補にするか」を抽象化し、
//! `run_completion_menu` が「候補から選ぶ UI」を担う。
//! 将来 `CompletionSource` に コマンド名 / ファイルパス / abbr / 履歴 を実装し、
//! `run_completion_menu` に fzf 風の絞り込みを足していく。

use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    style::Print,
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use std::io::{self, stdout};

pub trait CompletionSource {
    /// 行内容とカーソル位置から候補を返す
    fn complete(&self, line: &str, cursor: usize) -> Vec<String>;
}

/// 仮実装: 固定候補を返すだけ
pub struct StubCompletion;

impl CompletionSource for StubCompletion {
    fn complete(&self, _line: &str, _cursor: usize) -> Vec<String> {
        ["ls", "ls -la", "cd", "cargo build", "cargo run"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}

/// 候補から 1 つ選ぶモーダルピッカー。選択結果を返すだけで状態は変更しない。
/// (fzf 自体も独立ループの別プロセスなので、自前ループは妥当な境界)
pub fn run_completion_menu(candidates: &[String]) -> io::Result<Option<String>> {
    if candidates.is_empty() {
        return Ok(None);
    }

    let popup_height = (candidates.len() + 2) as u16; // +2: 枠線
    let mut selected = 0usize;
    let mut chosen = None;

    // プロンプト行の下に inline で展開
    execute!(stdout(), Print("\r\n"))?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(popup_height),
        },
    )?;

    loop {
        let sel = selected;
        terminal.draw(|frame| {
            let items: Vec<ListItem> = candidates
                .iter()
                .map(|s| ListItem::new(s.as_str()))
                .collect();
            let mut state = ListState::default();
            state.select(Some(sel));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" ↑↓: 移動  Tab/Enter: 確定  Esc: キャンセル "),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::Cyan)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ");
            frame.render_stateful_widget(list, frame.area(), &mut state);
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        match key.code {
            KeyCode::Esc => break,
            KeyCode::Up => selected = selected.checked_sub(1).unwrap_or(candidates.len() - 1),
            KeyCode::Down => selected = (selected + 1) % candidates.len(),
            KeyCode::Tab | KeyCode::Enter => {
                chosen = candidates.get(selected).cloned();
                break;
            }
            _ => {}
        }
    }

    terminal.clear()?;
    drop(terminal);
    // inline 先頭行 (プロンプトの 1 つ下) からプロンプト行へ戻す
    execute!(stdout(), cursor::MoveUp(1))?;
    Ok(chosen)
}
