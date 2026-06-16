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

    /// Ctrl+Left: 単語または '/' 単位でカーソルを左へ移動する
    pub fn move_word_left(&mut self) {
        self.cursor = word_left_pos(&self.buf[..self.cursor]);
    }

    /// Ctrl+Right: 単語または '/' 単位でカーソルを右へ移動する
    pub fn move_word_right(&mut self) {
        self.cursor = word_right_pos(&self.buf, self.cursor);
    }

    /// Ctrl+W: 単語または '/' 単位でカーソル直前を削除する
    pub fn delete_word_backward(&mut self) {
        let new_pos = word_left_pos(&self.buf[..self.cursor]);
        self.buf.drain(new_pos..self.cursor);
        self.cursor = new_pos;
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

    pub fn lines_above_cursor(&self) -> u16 {
        self.lines_above_cursor
    }

    /// skim 起動前に \r\n を出力した分だけ lines_above_cursor を +1 する。
    /// これにより redraw_prompt が正しくヘッダ行まで遡れる。
    pub fn note_newline(&mut self) {
        self.lines_above_cursor += 1;
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

    // 3. 入力行: "$ " プレフィックス → カーソル手前 → 位置保存 → 残りを印字 → 位置復元
    //
    // wrap-pending 対策: cursor_display が term_cols の倍数のとき端末は行末待機状態
    // になる。この状態で SavePosition すると位置がずれるため \r\n で折り返しを確定。
    const PREFIX: &str = "$ ";
    const PREFIX_WIDTH: u16 = 2; // "$ " は常に 2 列
    let cursor_display = PREFIX_WIDTH + ed.buf[..ed.cursor].width() as u16;
    queue!(
        stdout(),
        SetForegroundColor(Color::White),
        Print(PREFIX),
        ResetColor,
        Print(&ed.buf[..ed.cursor]),
    )?;
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

fn is_word_sep(c: char) -> bool {
    c.is_whitespace() || c == '/'
}

fn word_left_pos(s: &str) -> usize {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let n = chars.len();
    let mut i = n;
    while i > 0 && is_word_sep(chars[i - 1].1) { i -= 1; }
    if i == 0 { return 0; }
    while i > 0 && !is_word_sep(chars[i - 1].1) { i -= 1; }
    if i == 0 { 0 } else { chars[i].0 }
}

fn word_right_pos(s: &str, cursor: usize) -> usize {
    let rest = &s[cursor..];
    let chars: Vec<(usize, char)> = rest.char_indices().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n && is_word_sep(chars[i].1) { i += 1; }
    while i < n && !is_word_sep(chars[i].1) { i += 1; }
    if i == n { s.len() } else { cursor + chars[i].0 }
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
