//! 行エディタ (readline 相当)。
//!
//! 入力バッファとカーソルを保持し、行の編集操作だけを担う。
//! abbr / alias 展開や補完確定は、この型への操作として実装していく。

use crossterm::{
    cursor,
    queue,
    style::Print,
    terminal::{Clear, ClearType},
};
use std::io::{self, Write, stdout};
use unicode_width::UnicodeWidthStr;

pub const PROMPT: &str = "$ ";

#[derive(Default)]
pub struct LineEditor {
    buf: String,
    /// カーソル位置 (buf 内のバイトオフセット。常に char 境界)
    cursor: usize,
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

    /// 行を確定して取り出し、バッファを空に戻す
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    /// バッファ全体を置き換え、カーソルを末尾へ (補完確定で使用)
    pub fn set(&mut self, s: String) {
        self.buf = s;
        self.cursor = self.buf.len();
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

    /// カーソルの 1 つ前の char 境界 (バイト位置)
    fn prev_boundary(&self) -> Option<usize> {
        self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    /// プロンプトを含めたカーソルの表示カラム (全角文字を考慮)
    fn display_col(&self) -> u16 {
        let col = PROMPT.width() + self.buf[..self.cursor].width();
        col as u16
    }
}

/// プロンプト行を再描画し、カーソルを正しい表示カラムへ戻す
pub fn redraw_prompt(ed: &LineEditor) -> io::Result<()> {
    queue!(
        stdout(),
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        Print(PROMPT),
        Print(ed.line()),
        cursor::MoveToColumn(ed.display_col()),
    )?;
    stdout().flush()
}
