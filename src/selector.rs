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
    Dismissed,        // Esc / 候補なし
    Aborted,          // Ctrl+C
    InsertChar(char), // メニュー中に文字を打った → 挿入してメニュー閉じ
    Backspace,        // メニュー中に Backspace → 削除してメニュー閉じ
}

// ─── グリッドメニュー (Tab 補完) ──────────────────────────────────────────────

/// fish スタイルの多列補完メニュー。
///
/// レイアウト (行優先): 候補を左→右→折り返しで並べる。
/// - Tab / → : 次の候補
/// - Shift+Tab / ← : 前の候補
/// - ↓ / ↑ : 同列の次/前の行
/// - Enter : 確定、Esc : キャンセル、Ctrl+C : 中断
pub fn run_grid_menu(candidates: &[String], _lines_above_cursor: u16) -> io::Result<Selection> {
    if candidates.is_empty() {
        return Ok(Selection::Dismissed);
    }

    let (term_cols, term_rows) = terminal::size()?;
    let cols = term_cols as usize;

    // 列幅 = 最長候補 + 2 スペース
    let max_item_w = candidates.iter().map(|s| s.width()).max().unwrap_or(1);
    let col_width = (max_item_w + 2).min(cols);
    let n_cols = (cols / col_width).max(1);
    // 画面の半分まで使う (最小 4 行)
    let max_rows = ((term_rows as usize) / 2).max(4);
    let n_rows = candidates.len().div_ceil(n_cols).min(max_rows);
    let visible = (n_rows * n_cols).min(candidates.len());
    let items = &candidates[..visible];

    let mut selected = 0usize;
    let mut outcome = Selection::Dismissed;

    execute!(stdout(), Print("\r\n"))?;

    loop {
        draw_grid(items, selected, n_cols, col_width, n_rows)?;

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
            KeyCode::Enter => {
                outcome = Selection::Chosen(items[selected].clone());
                break;
            }
            KeyCode::Tab | KeyCode::Right => {
                selected = (selected + 1) % items.len();
            }
            KeyCode::BackTab | KeyCode::Left => {
                selected = selected.checked_sub(1).unwrap_or(items.len() - 1);
            }
            KeyCode::Down => {
                let next = selected + n_cols;
                if next < items.len() {
                    selected = next;
                }
            }
            KeyCode::Up => {
                if selected >= n_cols {
                    selected -= n_cols;
                }
            }
            // 文字入力でメニューを閉じてエディタへ委譲
            KeyCode::Backspace => {
                outcome = Selection::Backspace;
                break;
            }
            KeyCode::Char(c) if !ctrl => {
                outcome = Selection::InsertChar(c);
                break;
            }
            _ => {}
        }
    }

    // draw_grid はカーソルをグリッド先頭行 (入力行 +1) に置いて終わる。
    // \r\n で 1 行下がっているので MoveUp(1) で入力行に戻る。
    // その後 redraw_prompt が lines_above_cursor を使ってヘッダー行まで遡る。
    execute!(
        stdout(),
        Clear(ClearType::FromCursorDown),
        cursor::MoveUp(1),
        cursor::MoveToColumn(0),
    )?;

    Ok(outcome)
}

fn draw_grid(
    items: &[String],
    selected: usize,
    n_cols: usize,
    col_width: usize,
    n_rows: usize,
) -> io::Result<()> {
    for row in 0..n_rows {
        queue!(
            stdout(),
            cursor::MoveToColumn(0),
            Clear(ClearType::CurrentLine)
        )?;

        for col in 0..n_cols {
            let idx = row * n_cols + col;
            if idx >= items.len() {
                break;
            }
            let text = truncate_to_cols(&items[idx], col_width.saturating_sub(1));
            let pad = col_width.saturating_sub(text.width());

            if idx == selected {
                queue!(
                    stdout(),
                    SetBackgroundColor(Color::Blue),
                    SetForegroundColor(Color::White),
                    Print(format!("{}{:pad$}", text, "", pad = pad)),
                    ResetColor,
                )?;
            } else {
                queue!(stdout(), Print(format!("{}{:pad$}", text, "", pad = pad)))?;
            }
        }

        if row + 1 < n_rows {
            queue!(stdout(), Print("\r\n"))?;
        }
    }

    // カーソルをグリッド先頭行へ戻す
    if n_rows > 1 {
        queue!(stdout(), cursor::MoveUp(n_rows as u16 - 1))?;
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

/// 候補をストリーミングしながら skim で選択させる。
///
/// `produce` は別スレッドで実行され、`emit(String)` を呼んで候補を1件ずつ送る。
/// `emit` が false を返したとき (= skim が終了して受信側が閉じたとき) は走査を
/// 中断してよい。これにより巨大なツリーでも「検索したものから順に表示」でき、
/// 列挙の途中でも Ctrl+C / Esc で即中断できる。
pub fn run_fzf_streaming<F>(produce: F, initial_query: Option<&str>) -> io::Result<Selection>
where
    F: FnOnce(&mut dyn FnMut(String) -> bool) + Send + 'static,
{
    use skim::prelude::{
        Arc, Skim, SkimItem, SkimItemReceiver, SkimItemSender, SkimOptionsBuilder, unbounded,
    };

    let (tx, rx): (SkimItemSender, SkimItemReceiver) = unbounded();
    // 候補生成は別スレッドで。skim が終了すると rx が閉じ、send が Err を返すので
    // emit が false → produce 側はそこで走査を打ち切れる。
    std::thread::spawn(move || {
        let mut emit =
            |s: String| -> bool { tx.send(vec![Arc::new(s) as Arc<dyn SkimItem>]).is_ok() };
        produce(&mut emit);
    });

    let mut builder = SkimOptionsBuilder::default();
    builder.height("40%").reverse(true).no_sort(true);
    if let Some(q) = initial_query.filter(|q| !q.is_empty()) {
        builder.query(q);
    }
    let options = builder
        .build()
        .map_err(|e| io::Error::other(e.to_string()))?;

    terminal::disable_raw_mode()?;
    let result = Skim::run_with(options, Some(rx)).map_err(|e| io::Error::other(e.to_string()));
    terminal::enable_raw_mode()?;

    match result? {
        output if !output.is_abort => match output.selected_items.first() {
            Some(item) => Ok(Selection::Chosen(item.output().to_string())),
            None => Ok(Selection::Dismissed),
        },
        _ => Ok(Selection::Dismissed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii() {
        assert_eq!(truncate_to_cols("hello", 3), "hel");
        assert_eq!(truncate_to_cols("hi", 10), "hi");
        assert_eq!(truncate_to_cols("", 5), "");
        assert_eq!(truncate_to_cols("abc", 0), "");
    }
}
