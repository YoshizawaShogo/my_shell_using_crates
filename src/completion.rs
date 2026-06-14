//! 補完。
//!
//! `CompletionSource` が「何を候補にするか」を抽象化し、
//! `run_completion_menu` が「候補から選ぶ UI」を担う。

use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute, queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write, stdout};
use unicode_width::UnicodeWidthStr;

// ─── 補完ソース ───────────────────────────────────────────────────────────────

pub trait CompletionSource {
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

// ─── 補完メニュー ─────────────────────────────────────────────────────────────

/// 候補から 1 つ選ぶモーダルピッカー。fish shell 風: 枠なし・選択行のみハイライト。
///
/// **終了後の保証**: カーソルはプロンプト先頭行・列 0 に置かれる。
/// 呼び出し元は `ed.reset_cursor_tracking()` を呼んだ後に `redraw_prompt` すること。
pub fn run_completion_menu(
    candidates: &[String],
    lines_above_cursor: u16,
) -> io::Result<Option<String>> {
    if candidates.is_empty() {
        return Ok(None);
    }

    let (term_cols, _) = terminal::size()?;
    let popup_height = candidates.len() as u16;
    let mut selected = 0usize;
    let mut chosen = None;

    // プロンプト行の 1 行下 (= メニュー先頭行) へ移動
    execute!(stdout(), Print("\r\n"))?;

    loop {
        draw_menu(candidates, selected, term_cols, popup_height)?;

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

    // メニューを消去してプロンプト先頭行・列 0 へ戻る
    //
    // 現在地: メニュー先頭行 = プロンプト先頭行 + lines_above_cursor + 1
    // MoveUp(lines_above_cursor + 1) でプロンプト先頭行へ
    execute!(
        stdout(),
        Clear(ClearType::FromCursorDown),
        cursor::MoveUp(lines_above_cursor + 1),
        cursor::MoveToColumn(0),
    )?;

    Ok(chosen)
}

/// 候補リストを 1 フレーム描く (fish 風: 枠なし、選択行を青背景でハイライト)。
/// 描画後、カーソルをメニュー先頭行・列 0 に戻す (次フレームのため)。
fn draw_menu(
    candidates: &[String],
    selected: usize,
    term_cols: u16,
    popup_height: u16,
) -> io::Result<()> {
    let cols = term_cols as usize;
    let n = candidates.len();

    for (i, cand) in candidates.iter().enumerate() {
        let text = truncate_to_cols(cand, cols);
        let pad = cols.saturating_sub(text.width());

        queue!(
            stdout(),
            cursor::MoveToColumn(0),
            Clear(ClearType::CurrentLine)
        )?;

        if i == selected {
            queue!(
                stdout(),
                SetBackgroundColor(Color::Blue),
                SetForegroundColor(Color::White),
                Print(format!("{}{:pad$}", text, "", pad = pad)),
                ResetColor,
            )?;
        } else {
            queue!(stdout(), Print(text))?;
        }

        if i + 1 < n {
            queue!(stdout(), Print("\r\n"))?;
        }
    }

    // カーソルをメニュー先頭行に戻す
    if popup_height > 1 {
        queue!(stdout(), cursor::MoveUp(popup_height - 1))?;
    }
    queue!(stdout(), cursor::MoveToColumn(0))?;

    stdout().flush()
}

/// 文字列を表示幅 `max_cols` に収まるよう切り詰める
fn truncate_to_cols(s: &str, max_cols: usize) -> &str {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0;
    let mut end = 0;
    for (byte_idx, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if width + w > max_cols {
            break;
        }
        width += w;
        end = byte_idx + c.len_utf8();
    }
    &s[..end]
}
