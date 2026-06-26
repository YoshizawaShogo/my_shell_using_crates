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

// 配色は iceberg (dark) パレットに合わせた落ち着いたトーン。
const COLOR_USER_HOST: Color = Color::Rgb {
    r: 0x84,
    g: 0xa0,
    b: 0xc6,
}; // blue
const COLOR_CWD: Color = Color::Rgb {
    r: 0x89,
    g: 0xb8,
    b: 0xc2,
}; // cyan
const COLOR_BRANCH: Color = Color::Rgb {
    r: 0xa0,
    g: 0x93,
    b: 0xc7,
}; // purple
const COLOR_GHOST: Color = Color::Rgb {
    r: 0x6b,
    g: 0x70,
    b: 0x89,
}; // muted
// `$` プロンプトは直前コマンドの終了ステータスで色を変える (成功=緑 / 失敗=赤)。
const COLOR_PROMPT_OK: Color = Color::Rgb {
    r: 0xb4,
    g: 0xbe,
    b: 0x82,
}; // green
const COLOR_PROMPT_ERR: Color = Color::Rgb {
    r: 0xe2,
    g: 0x78,
    b: 0x78,
}; // red

#[derive(Default)]
pub struct LineEditor {
    buf: String,
    /// カーソル位置 (buf 内のバイトオフセット。常に char 境界)
    cursor: usize,
    /// ヘッダ行を含む先頭行からカーソル行までの行数
    /// `redraw_prompt` が管理する。次回の `MoveUp` に使う。
    lines_above_cursor: u16,
    /// Tab 補完で挿入されたテキストの終端位置 (buf 内バイトオフセット)。
    /// これより後ろに手入力された文字がフィルタとして機能する。
    /// 単語境界 (空白) や逆方向の編集でリセットされる。
    tab_end: Option<usize>,
}

impl LineEditor {
    pub fn insert(&mut self, c: char) {
        if let Some(te) = self.tab_end {
            // 空白 → 単語確定、またはカーソルが補完終端より前 → リセット
            if c.is_whitespace() || self.cursor < te {
                self.tab_end = None;
            }
        }
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.buf.remove(prev);
            self.cursor = prev;
            if self.tab_end.is_some_and(|te| self.cursor < te) {
                self.tab_end = None;
            }
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
        self.tab_end = None;
    }

    pub fn move_right(&mut self) {
        if let Some(c) = self.buf[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
        self.tab_end = None;
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
        self.tab_end = None;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
        self.tab_end = None;
    }

    /// Ctrl+Left: 単語または '/' 単位でカーソルを左へ移動する
    pub fn move_word_left(&mut self) {
        self.cursor = word_left_pos(&self.buf[..self.cursor]);
        self.tab_end = None;
    }

    /// Ctrl+Right: 単語または '/' 単位でカーソルを右へ移動する
    pub fn move_word_right(&mut self) {
        self.cursor = word_right_pos(&self.buf, self.cursor);
        self.tab_end = None;
    }

    /// Ctrl+W: 単語または '/' 単位でカーソル直前を削除する
    pub fn delete_word_backward(&mut self) {
        let new_pos = word_left_pos(&self.buf[..self.cursor]);
        self.buf.drain(new_pos..self.cursor);
        self.cursor = new_pos;
        self.tab_end = None;
    }

    /// Ctrl+U: カーソルより前をすべて削除する。
    pub fn kill_to_start(&mut self) {
        self.buf.drain(..self.cursor);
        self.cursor = 0;
        self.tab_end = None;
    }

    /// Ctrl+K: カーソル以降をすべて削除する。
    pub fn kill_to_end(&mut self) {
        self.buf.truncate(self.cursor);
        self.tab_end = None;
    }

    /// カーソル前の `n` バイトを削除する。
    pub fn delete_before_cursor(&mut self, n: usize) {
        let end = self.cursor;
        let start = end.saturating_sub(n);
        self.buf.drain(start..end);
        self.cursor = start;
        self.tab_end = None;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.lines_above_cursor = 0;
        self.tab_end = None;
        std::mem::take(&mut self.buf)
    }

    pub fn set(&mut self, s: String) {
        self.buf = s;
        self.cursor = self.buf.len();
        self.tab_end = None;
    }

    /// Tab 補完後に呼ぶ: 現在のカーソル位置を補完終端として記録する。
    pub fn set_tab_end(&mut self) {
        self.tab_end = Some(self.cursor);
    }

    /// Tab 補完終端位置を返す (buf 内バイトオフセット)。
    pub fn tab_end(&self) -> Option<usize> {
        self.tab_end
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
    last_status: i32,
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

    // 2. ヘッダ行を描画 (表示幅を記録して lines_above_cursor の計算に使う)
    let user = std::env::var("USER").unwrap_or_else(|_| "?".to_string());
    let host = hostname();
    let cwd = full_cwd();
    let mut header_display_width = format!("{}@{} {}", user, host, cwd).width();
    queue!(
        stdout(),
        SetForegroundColor(COLOR_USER_HOST),
        Print(format!("{}@{}", user, host)),
        ResetColor,
        Print(" "),
        SetForegroundColor(COLOR_CWD),
        Print(&cwd),
        ResetColor,
    )?;
    if let Some(branch) = git_branch {
        let branch_text = format!(" ({})", branch);
        header_display_width += branch_text.width();
        queue!(
            stdout(),
            SetForegroundColor(COLOR_BRANCH),
            Print(branch_text),
            ResetColor,
        )?;
    }
    queue!(stdout(), Print("\r\n"))?;
    // ヘッダが端末幅を超えて折り返す場合の物理行数
    let header_lines = header_physical_lines(header_display_width, term_cols);

    // 3. 入力行: "$ " プレフィックス → カーソル手前 → 残りを印字 → カーソル復元
    //
    // wrap-pending 対策: cursor_display が term_cols の倍数のとき端末は行末待機状態
    // になる。\r\n で折り返しを確定してからカーソル物理列を 0 にリセット。
    let cursor_display = PROMPT_PREFIX_WIDTH + ed.buf[..ed.cursor].width() as u16;
    let prompt_color = if last_status == 0 {
        COLOR_PROMPT_OK
    } else {
        COLOR_PROMPT_ERR
    };
    queue!(
        stdout(),
        SetForegroundColor(prompt_color),
        Print(PROMPT_PREFIX),
        ResetColor,
        Print(&ed.buf[..ed.cursor]),
    )?;
    let cursor_physical_col = if cursor_display > 0 && cursor_display.is_multiple_of(term_cols) {
        queue!(stdout(), Print("\r\n"))?;
        0u16
    } else {
        cursor_display % term_cols
    };

    // ゴーストを含む「カーソル以降」の表示幅を求め、何行下へ進むかを計算する。
    // SavePosition/RestorePosition はスクロール時に座標がズレるため使わない。
    // MoveUp + MoveToColumn は相対移動なのでスクロール後も正しく戻れる。
    let after_width = ed.buf[ed.cursor..].width() as u16;
    let ghost_str = ghost.unwrap_or("");
    let ghost_width = ghost_str.width() as u16;
    let lines_down = (cursor_physical_col + after_width + ghost_width) / term_cols;

    queue!(stdout(), Print(&ed.buf[ed.cursor..]))?;
    if !ghost_str.is_empty() {
        queue!(
            stdout(),
            SetForegroundColor(COLOR_GHOST),
            Print(ghost_str),
            ResetColor,
        )?;
    }
    if lines_down > 0 {
        queue!(stdout(), cursor::MoveUp(lines_down))?;
    }
    queue!(stdout(), cursor::MoveToColumn(cursor_physical_col))?;

    // 4. ヘッダの物理行数 + 入力行の折り返し数
    ed.lines_above_cursor = header_lines + cursor_display / term_cols;

    stdout().flush()
}

/// 表示幅が `max_cols` を超えない最長の先頭部分文字列を返す (char 境界を保つ)。
/// ヘッダテキストが端末幅 `cols` で何物理行を占めるか (最小1)。
fn header_physical_lines(display_width: usize, cols: u16) -> u16 {
    let cols = cols as usize;
    if cols == 0 || display_width == 0 {
        return 1;
    }
    display_width.div_ceil(cols) as u16
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

fn full_cwd() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "?".to_string());

    // ホームディレクトリを ~ に置換
    if !home.is_empty() && cwd.starts_with(&home) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd
    }
}

/// パスの 1 コンポーネントを先頭 1 文字に短縮する。
/// 隠しディレクトリ (`.foo`) は `.f` のように先頭 2 文字を残す。
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
