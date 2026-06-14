//! 行エディタ (readline 相当)。
//!
//! 入力バッファとカーソルを保持し、行の編集操作だけを担う。
//! abbr / alias 展開や補完確定は、この型への操作として実装していく。

use crossterm::{
    cursor,
    queue,
    style::Print,
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write, stdout};
use unicode_width::UnicodeWidthStr;

pub const PROMPT: &str = "$ ";

#[derive(Default)]
pub struct LineEditor {
    buf: String,
    /// カーソル位置 (buf 内のバイトオフセット。常に char 境界)
    cursor: usize,
    /// プロンプト先頭行からカーソル行までの行数 (折り返し対応)
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

    /// 行を確定して取り出し、バッファを空に戻す
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.lines_above_cursor = 0;
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

    /// 補完メニュー後にカーソル追跡をリセットする
    /// (カーソルがプロンプト先頭行・列0にいるとき呼ぶ)
    pub fn reset_cursor_tracking(&mut self) {
        self.lines_above_cursor = 0;
    }

    /// `run_completion_menu` が MoveUp 量を決めるために参照する
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

/// プロンプト行を再描画し、カーソルを正しい位置に置く。
///
/// 折り返し対応:
///   - `lines_above_cursor` 行上がってプロンプト先頭行へ移動
///   - `Clear(FromCursorDown)` で折り返し分を含む旧描画を一括消去
///   - `SavePosition`/`RestorePosition` でカーソル位置を確定
///     (カーソル手前まで印字 → 位置保存 → 残りを印字 → 位置復元)
pub fn redraw_prompt(ed: &mut LineEditor) -> io::Result<()> {
    let (term_cols, _) = terminal::size()?;

    // 1. プロンプト先頭行へ
    if ed.lines_above_cursor > 0 {
        queue!(stdout(), cursor::MoveUp(ed.lines_above_cursor))?;
    }
    queue!(stdout(), cursor::MoveToColumn(0), Clear(ClearType::FromCursorDown))?;

    // 2. カーソル手前を印字 → 位置保存 → 残りを印字 → 位置復元
    //
    // wrap-pending 対策:
    //   cursor_display が term_cols の倍数のとき、端末は「行末で折り返し待機」
    //   状態になる。SavePosition がこの状態を保存すると、RestorePosition 後の
    //   cursor 位置が前の行末 (col = term_cols-1) になり、次回の MoveUp が
    //   1 行分ずれる (y軸ドリフト)。
    //   → 折り返しを \r\n で明示して位置を確定してから SavePosition する。
    let cursor_display = (PROMPT.width() + ed.buf[..ed.cursor].width()) as u16;
    queue!(stdout(), Print(PROMPT), Print(&ed.buf[..ed.cursor]))?;
    if cursor_display > 0 && cursor_display % term_cols == 0 {
        queue!(stdout(), Print("\r\n"))?;
    }
    queue!(
        stdout(),
        cursor::SavePosition,
        Print(&ed.buf[ed.cursor..]),
        cursor::RestorePosition,
    )?;

    // 3. 次回の MoveUp 量を更新
    ed.lines_above_cursor = cursor_display / term_cols;

    stdout().flush()
}
