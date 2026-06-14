//! 補完候補の選択 UI。
//!
//! 「どう選ぶか」だけを担う。候補の生成は `provider` モジュールが担う。

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write, stdout};
use unicode_width::UnicodeWidthStr;

// ─── 選択結果 ─────────────────────────────────────────────────────────────────

pub enum Selection {
    Chosen(String),
    Dismissed, // Esc / 候補なし
    Aborted,   // Ctrl+C
}

// ─── インライン menu (Tab) ────────────────────────────────────────────────────

/// 候補をインラインメニューで表示し、1 つ選ばせる。fish 風: 枠なし。
///
/// **終了後の保証**: カーソルはプロンプト先頭行・列 0 に置かれる。
pub fn run_menu(candidates: &[String], lines_above_cursor: u16) -> io::Result<Selection> {
    if candidates.is_empty() {
        return Ok(Selection::Dismissed);
    }

    let (term_cols, _) = terminal::size()?;
    let popup_height = candidates.len() as u16;
    let mut selected = 0usize;
    let mut outcome = Selection::Dismissed;

    execute!(stdout(), Print("\r\n"))?;

    loop {
        draw_menu(candidates, selected, term_cols, popup_height)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                outcome = Selection::Aborted;
                break;
            }
            KeyCode::Esc => break,
            KeyCode::Up | KeyCode::BackTab => {
                selected = selected.checked_sub(1).unwrap_or(candidates.len() - 1);
            }
            KeyCode::Down | KeyCode::Tab => {
                selected = (selected + 1) % candidates.len();
            }
            KeyCode::Enter => {
                if let Some(s) = candidates.get(selected).cloned() {
                    outcome = Selection::Chosen(s);
                }
                break;
            }
            _ => {}
        }
    }

    execute!(
        stdout(),
        Clear(ClearType::FromCursorDown),
        cursor::MoveUp(lines_above_cursor + 1),
        cursor::MoveToColumn(0),
    )?;

    Ok(outcome)
}

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

    if popup_height > 1 {
        queue!(stdout(), cursor::MoveUp(popup_height - 1))?;
    }
    queue!(stdout(), cursor::MoveToColumn(0))?;
    stdout().flush()
}

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

// ─── skim ────────────────────────────────────────────────────────────────────

/// 候補を skim でインタラクティブに選択させる。
///
/// `initial_query` は skim の初期検索文字列として渡す。
pub fn run_fzf(candidates: &[String], initial_query: Option<&str>) -> io::Result<Selection> {
    use skim::prelude::{Skim, SkimOptionsBuilder};

    if candidates.is_empty() {
        return Ok(Selection::Dismissed);
    }

    let mut builder = SkimOptionsBuilder::default();
    builder.height("40%").reverse(true).no_sort(true);
    if let Some(q) = initial_query.filter(|q| !q.is_empty()) {
        builder.query(q);
    }
    let options = builder
        .build()
        .map_err(|e| io::Error::other(e.to_string()))?;

    // crossterm の raw mode を解除してから skim (ratatui) に端末を渡す
    terminal::disable_raw_mode()?;
    let result = Skim::run_items(options, candidates.iter().cloned())
        .map_err(|e| io::Error::other(e.to_string()));
    terminal::enable_raw_mode()?;

    match result? {
        output if !output.is_abort => match output.selected_items.first() {
            Some(item) => Ok(Selection::Chosen(item.output().to_string())),
            None => Ok(Selection::Dismissed),
        },
        _ => Ok(Selection::Dismissed),
    }
}
