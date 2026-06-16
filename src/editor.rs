//! 行エディタ (readline 相当)。

use crossterm::{
    cursor, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write, stdout};
use unicode_width::UnicodeWidthStr;

// ─── 配色・プロンプト定数 ─────────────────────────────────────────────────────

const PROMPT_PREFIX: &str = "$ ";
const PROMPT_PREFIX_WIDTH: u16 = 2; // "$ " は常に 2 列
const COLOR_USER_HOST: Color = Color::Green;
const COLOR_CWD: Color = Color::Cyan;
const COLOR_BRANCH: Color = Color::Blue;
const COLOR_PROMPT: Color = Color::White;
const COLOR_GHOST: Color = Color::DarkGrey;

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

    /// Ctrl+U: カーソルより前をすべて削除する。
    pub fn kill_to_start(&mut self) {
        self.buf.drain(..self.cursor);
        self.cursor = 0;
    }

    /// Ctrl+K: カーソル以降をすべて削除する。
    pub fn kill_to_end(&mut self) {
        self.buf.truncate(self.cursor);
    }

    /// カーソル前の `n` バイトを削除する。
    pub fn delete_before_cursor(&mut self, n: usize) {
        let end = self.cursor;
        let start = end.saturating_sub(n);
        self.buf.drain(start..end);
        self.cursor = start;
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

    /// 画面クリア (Ctrl+L) 後に呼ぶ: カーソルが最上段に移ったので追跡をリセットする。
    pub fn reset_lines_above(&mut self) {
        self.lines_above_cursor = 0;
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
///   行 H+1: \[入力バッファ\]              ← 入力行 (折り返し可)
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
        SetForegroundColor(COLOR_USER_HOST),
        Print(format!("{}@{}", user, host)),
        ResetColor,
        Print(" "),
        SetForegroundColor(COLOR_CWD),
        Print(cwd),
        ResetColor,
    )?;
    if let Some(branch) = git_branch {
        queue!(
            stdout(),
            Print(" "),
            SetForegroundColor(COLOR_BRANCH),
            Print(format!("({})", branch)),
            ResetColor,
        )?;
    }
    queue!(stdout(), Print("\r\n"))?;

    // 3. 入力行: "$ " プレフィックス → カーソル手前 → 位置保存 → 残りを印字 → 位置復元
    //
    // wrap-pending 対策: cursor_display が term_cols の倍数のとき端末は行末待機状態
    // になる。この状態で SavePosition すると位置がずれるため \r\n で折り返しを確定。
    let cursor_display = PROMPT_PREFIX_WIDTH + ed.buf[..ed.cursor].width() as u16;
    queue!(
        stdout(),
        SetForegroundColor(COLOR_PROMPT),
        Print(PROMPT_PREFIX),
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
            SetForegroundColor(COLOR_GHOST),
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
    while i > 0 && is_word_sep(chars[i - 1].1) {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    while i > 0 && !is_word_sep(chars[i - 1].1) {
        i -= 1;
    }
    if i == 0 { 0 } else { chars[i].0 }
}

fn word_right_pos(s: &str, cursor: usize) -> usize {
    let rest = &s[cursor..];
    let chars: Vec<(usize, char)> = rest.char_indices().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n && is_word_sep(chars[i].1) {
        i += 1;
    }
    while i < n && !is_word_sep(chars[i].1) {
        i += 1;
    }
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
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 2 {
        return path;
    }
    let last = parts.len() - 1;
    parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            if i == last || part.is_empty() {
                part.to_string()
            } else {
                shorten_component(part)
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// パスの 1 コンポーネントを先頭 1 文字に短縮する。
/// 隠しディレクトリ (`.foo`) は `.f` のように先頭 2 文字を残す。
fn shorten_component(part: &str) -> String {
    let mut chars = part.chars();
    let mut out = String::new();
    if let Some(c) = chars.next() {
        out.push(c);
        if c == '.'
            && let Some(c2) = chars.next()
        {
            out.push(c2);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_left_skips_separators() {
        assert_eq!(word_left_pos("foo bar"), 4);
        assert_eq!(word_left_pos("foo bar "), 4);
        assert_eq!(word_left_pos("foo"), 0);
        assert_eq!(word_left_pos("a/b/c"), 4);
        assert_eq!(word_left_pos(""), 0);
    }

    #[test]
    fn word_right_advances() {
        assert_eq!(word_right_pos("foo bar", 0), 3);
        assert_eq!(word_right_pos("foo bar", 3), 7);
        assert_eq!(word_right_pos("a/b", 0), 1);
    }

    #[test]
    fn shorten_handles_hidden() {
        assert_eq!(shorten_component("usr"), "u");
        assert_eq!(shorten_component(".config"), ".c");
        assert_eq!(shorten_component(""), "");
    }

    #[test]
    fn kill_to_start_and_end() {
        let mut ed = LineEditor::default();
        ed.set("hello world".to_string());
        ed.move_home();
        ed.move_word_right(); // cursor after "hello"
        let cur = ed.cursor();
        ed.kill_to_end();
        assert_eq!(ed.line(), "hello");
        assert_eq!(ed.cursor(), cur);

        let mut ed2 = LineEditor::default();
        ed2.set("hello world".to_string()); // cursor at end
        ed2.kill_to_start();
        assert_eq!(ed2.line(), "");
        assert_eq!(ed2.cursor(), 0);
    }
}
