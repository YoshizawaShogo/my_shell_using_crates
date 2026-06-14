//! 行エディタ (readline 相当)。

use crossterm::{
    cursor, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write, stdout};
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub struct LineEditor {
    buf: String,
    /// カーソル位置 (buf 内のバイトオフセット。常に char 境界)
    cursor: usize,
    /// ヘッダ行を含む先頭行からカーソル行までの行数
    /// `redraw_prompt` が管理する。次回の `MoveUp` に使う。
    lines_above_cursor: u16,
}

impl LineEditor {
    pub fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.buf.remove(prev);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.cursor = prev;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(c) = self.buf[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    /// Ctrl+W: カーソル直前の単語 (空白を含む) を削除する (Unix word rubout)
    pub fn delete_word_backward(&mut self) {
        let new_end = self.buf[..self.cursor]
            .char_indices()
            .rev()
            .skip_while(|(_, c)| c.is_whitespace())
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        self.buf.drain(new_end..self.cursor);
        self.cursor = new_end;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.lines_above_cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub fn set(&mut self, s: String) {
        self.buf = s;
        self.cursor = self.buf.len();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.buf.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn line(&self) -> &str {
        &self.buf
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn reset_cursor_tracking(&mut self) {
        self.lines_above_cursor = 0;
    }

    pub fn lines_above_cursor(&self) -> u16 {
        self.lines_above_cursor
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }
}

/// プロンプトを再描画する。
///
/// 画面構成:
///   行 H : user@host ~/path (branch)  ← ヘッダ行
///   行 H+1: [入力バッファ]              ← 入力行 (折り返し可)
///
/// `lines_above_cursor` は行 H からカーソル行までの距離を保持し、
/// 次回の `MoveUp` でヘッダ先頭に戻るために使う。
pub fn redraw_prompt(
    ed: &mut LineEditor,
    git_branch: Option<&str>,
    ghost: Option<&str>,
) -> io::Result<()> {
    let (term_cols, _) = terminal::size()?;

    // 1. ヘッダ先頭行へ移動してから全消去
    if ed.lines_above_cursor > 0 {
        queue!(stdout(), cursor::MoveUp(ed.lines_above_cursor))?;
    }
    queue!(
        stdout(),
        cursor::MoveToColumn(0),
        Clear(ClearType::FromCursorDown)
    )?;

    // 2. ヘッダ行を描画
    let user = std::env::var("USER").unwrap_or_else(|_| "?".to_string());
    let host = hostname();
    let cwd = abbreviated_cwd();
    queue!(
        stdout(),
        SetForegroundColor(Color::Green),
        Print(format!("{}@{}", user, host)),
        ResetColor,
        Print(" "),
        SetForegroundColor(Color::Cyan),
        Print(cwd),
        ResetColor,
    )?;
    if let Some(branch) = git_branch {
        queue!(
            stdout(),
            Print(" "),
            SetForegroundColor(Color::Blue),
            Print(format!("({})", branch)),
            ResetColor,
        )?;
    }
    queue!(stdout(), Print("\r\n"))?;

    // 3. 入力行: カーソル手前を印字 → 位置保存 → 残りを印字 → 位置復元
    //
    // wrap-pending 対策: cursor_display が term_cols の倍数のとき端末は行末待機状態
    // になる。この状態で SavePosition すると位置がずれるため \r\n で折り返しを確定。
    let cursor_display = ed.buf[..ed.cursor].width() as u16;
    queue!(stdout(), Print(&ed.buf[..ed.cursor]))?;
    if cursor_display > 0 && cursor_display.is_multiple_of(term_cols) {
        queue!(stdout(), Print("\r\n"))?;
    }
    queue!(stdout(), cursor::SavePosition, Print(&ed.buf[ed.cursor..]))?;
    if let Some(g) = ghost {
        queue!(
            stdout(),
            SetForegroundColor(Color::DarkGrey),
            Print(g),
            ResetColor,
        )?;
    }
    queue!(stdout(), cursor::RestorePosition)?;

    // 4. +1 はヘッダ行の分
    ed.lines_above_cursor = 1 + cursor_display / term_cols;

    stdout().flush()
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_else(|_| "?".to_string()))
}

fn abbreviated_cwd() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "?".to_string());

    // ホームディレクトリを ~ に置換
    let path = if !home.is_empty() && cwd.starts_with(&home) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd
    };

    // 最終コンポーネント以外を先頭 1 文字に短縮 (fish 方式)
    // 隠しディレクトリ (.foo) は ".f" に短縮
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 2 {
        return path;
    }
    let mut result = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            result.push('/');
        }
        if i == parts.len() - 1 || part.is_empty() {
            result.push_str(part);
        } else {
            let mut chars = part.chars();
            if let Some(c) = chars.next() {
                result.push(c);
                if c == '.'
                    && let Some(c2) = chars.next()
                {
                    result.push(c2);
                }
            }
        }
    }
    result
}
