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

    /// Ctrl+Left: 単語単位でカーソルを左へ移動する (Ctrl+W と同じ単語定義)。
    pub fn move_word_left(&mut self) {
        self.cursor = shell_word_left_pos(&self.buf[..self.cursor]);
        self.tab_end = None;
    }

    /// Ctrl+Right: 単語単位でカーソルを右へ移動する (Ctrl+W と同じ単語定義)。
    pub fn move_word_right(&mut self) {
        self.cursor = shell_word_right_pos(&self.buf, self.cursor);
        self.tab_end = None;
    }

    /// Ctrl+W: 単語単位でカーソル直前を削除する。
    /// 区切りは空白だが、クォート (`'…'` / `"…"`) やエスケープ (`\ `) 内の
    /// 空白は区切りとみなさず、シェルのトークン同様にまとめて削除する。
    pub fn delete_word_backward(&mut self) {
        let new_pos = shell_word_left_pos(&self.buf[..self.cursor]);
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

    /// Tab 補完結果を適用する。
    /// カーソル前を `before` に置き換え、カーソル後ろの `suffix` はそのまま残す。
    /// カーソルは補完境界 (`before` の末尾) に置き、そこを補完終端として記録する。
    pub fn apply_completion(&mut self, before: String, suffix: &str) {
        self.cursor = before.len();
        self.buf = before;
        self.buf.push_str(suffix);
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

    /// ブラケットペーストで届いたテキストをカーソル位置へ挿入する。
    /// 呼び出し側 (main) が 1 行の貼り付けにだけ使う。改行入りでも中身が実質 1 行
    /// (空行を除いて 1 行以下) のケースを [`sanitize_paste`] が 1 行に均す。
    /// 実行はしない — ユーザーが Enter を押すまでバッファに留まる。
    pub fn insert_paste(&mut self, data: &str) {
        let text = sanitize_paste(data);
        self.buf.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.tab_end = None;
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

    /// skim 起動前に \r\n を出力した分だけ lines_above_cursor を +1 する。
    /// これにより redraw_prompt が正しくヘッダ行まで遡れる。
    pub fn note_newline(&mut self) {
        self.lines_above_cursor += 1;
    }

    /// プロンプト先頭行からカーソル行までの物理行数 (折り返し込み)。
    /// プロンプトの下に何行描いてよいか (= 画面に収まる残り) の計算に使う。
    pub fn lines_above(&self) -> u16 {
        self.lines_above_cursor
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

/// ペーストテキストを 1 行の入力に整える。
///
/// - 改行を含まなければそのまま返す (ペーストした空白などをそのまま尊重する)。
/// - 改行を含む場合は各行を trim して空行を捨てる。通常は 1 行しか残らない
///   (2 行以上は main の貼り付けキューが 1 行ずつ実行するのでここへ来ない)。
///   保険として複数行が来たときだけ `; ` で連結する。
fn sanitize_paste(data: &str) -> String {
    if !data.chars().any(char::is_control) {
        return data.to_string();
    }
    paste_lines(data).join("; ")
}

/// ペーストテキストを、実際に入力へ入る行 (trim 済み・空行を除く) に分解する。
/// 貼り付け前の確認 (何行貼るか / 先頭行は何か) にも使う。
///
/// - 行の区切りは `\n` / `\r` / `\r\n` のいずれも受け付ける。端末は raw mode 中の
///   ブラケットペーストで改行を **CR で送ることが多い**ため、`\n` だけで分割すると
///   複数行を 1 行と誤認して確認プロンプトが出ない。
/// - 行内に残った制御文字 (タブなど) は空白へ潰す。そのまま入れると表示幅の計算が
///   狂ってプロンプトの再描画が崩れる。
pub fn paste_lines(data: &str) -> Vec<String> {
    data.split(['\n', '\r'])
        .map(|line| {
            line.trim()
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect::<String>()
        })
        .filter(|line| !line.is_empty())
        .collect()
}

/// プロンプトを再描画する。
///
/// 画面構成:
///   行 H : user@host ~/path (branch hash)  ← ヘッダ行
///   行 H+1: \[入力バッファ\]              ← 入力行 (折り返し可)
///
/// `lines_above_cursor` は行 H からカーソル行までの距離を保持し、
/// 次回の `MoveUp` でヘッダ先頭に戻るために使う。
pub fn redraw_prompt(
    ed: &mut LineEditor,
    git_info: Option<&str>,
    ghost: Option<&str>,
) -> io::Result<()> {
    // 端末幅は最低 1 に丸める。タブ複製直後など winsize が未伝搬の一瞬は size() が 0 を
    // 返すことがあり、そのままだと後段の `% term_cols` / `/ term_cols` が 0 除算でパニック
    // → シェルが起動直後に即死する (古い端末での「新タブが壊れる」原因)。実サイズが届けば
    // Resize イベントで再描画され表示は直る。
    let (term_cols, _) = terminal::size()?;
    let term_cols = term_cols.max(1);

    // 0. カーソルを I 型 (縦棒) に再指定する。外部コマンド (vim 等) が形を変えても
    //    プロンプトへ戻ったときに I 型へ戻す。同じ ANSI の再送で冪等・数バイト。
    queue!(stdout(), cursor::SetCursorStyle::SteadyBar)?;

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
    if let Some(info) = git_info {
        let branch_text = format!(" ({})", info);
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
    queue!(stdout(), Print(PROMPT_PREFIX), Print(&ed.buf[..ed.cursor]),)?;
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

/// Ctrl+W / Ctrl+Left 用: クォート/エスケープを尊重した最後の単語の開始位置を返す。
/// クォート外の空白だけを区切りとし、最後の単語＋直後の末尾空白をまとめて
/// 消せるよう、その単語の開始バイト位置を返す。
fn shell_word_left_pos(s: &str) -> usize {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut in_word = false;
    let mut last_start = 0usize;
    for (i, c) in s.char_indices() {
        if escaped {
            // 直前の `\` がこの文字をエスケープ。単語の一部として継続。
            escaped = false;
            continue;
        }
        match quote {
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                }
                continue;
            }
            Quote::Double => {
                if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    quote = Quote::None;
                }
                continue;
            }
            Quote::None => {}
        }
        if c.is_whitespace() {
            in_word = false;
            continue;
        }
        if !in_word {
            in_word = true;
            last_start = i;
        }
        match c {
            '\\' => escaped = true,
            '\'' => quote = Quote::Single,
            '"' => quote = Quote::Double,
            _ => {}
        }
    }
    last_start
}

/// Ctrl+Right 用: クォート/エスケープを尊重した次の単語の末尾位置を返す。
/// クォート外の空白だけを区切りとする (shell_word_left_pos の対称版)。
fn shell_word_right_pos(s: &str, cursor: usize) -> usize {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut in_word = false;
    for (i, c) in s[cursor..].char_indices() {
        if escaped {
            escaped = false;
            in_word = true;
            continue;
        }
        match quote {
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                }
                in_word = true;
                continue;
            }
            Quote::Double => {
                if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    quote = Quote::None;
                }
                in_word = true;
                continue;
            }
            Quote::None => {}
        }
        if c.is_whitespace() {
            if in_word {
                return cursor + i;
            }
            continue;
        }
        in_word = true;
        match c {
            '\\' => escaped = true,
            '\'' => quote = Quote::Single,
            '"' => quote = Quote::Double,
            _ => {}
        }
    }
    s.len()
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_else(|_| "?".to_string()))
}

pub fn full_cwd() -> String {
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
    fn sanitize_paste_converts_newlines() {
        // 改行なしはそのまま (空白も尊重する)
        assert_eq!(sanitize_paste("git status"), "git status");
        assert_eq!(sanitize_paste("  spaced  "), "  spaced  ");
        // 複数行は各行 trim + 空行除去して "; " で連結
        assert_eq!(
            sanitize_paste("ls -la\ncd src\ncargo build"),
            "ls -la; cd src; cargo build"
        );
        // CRLF・先頭末尾/連続の空行は区切りを増やさない
        assert_eq!(sanitize_paste("\n\na\r\n\r\nb\n\n"), "a; b");
        // インデントは除去
        assert_eq!(sanitize_paste("a\n    b"), "a; b");
    }

    #[test]
    fn paste_lines_accepts_cr_lf_and_crlf() {
        // 端末は raw mode のペーストで改行を CR で送ることが多い。
        // どの区切りでも同じ行数に分解できないと確認プロンプトが出ない。
        let expect = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(paste_lines("a\nb\nc"), expect);
        assert_eq!(paste_lines("a\rb\rc"), expect);
        assert_eq!(paste_lines("a\r\nb\r\nc"), expect);
        // 先頭末尾・連続の空行、行頭インデントは落とす
        assert_eq!(paste_lines("\r\n  a  \r\n\r\n b\r\n"), vec!["a", "b"]);
        // 1 行のみ (末尾改行つき) は 1 要素
        assert_eq!(paste_lines("solo\r"), vec!["solo"]);
        // 行内のタブなど制御文字は空白へ潰す (表示幅の計算が狂うため)
        assert_eq!(paste_lines("echo\ta\tb"), vec!["echo a b"]);
    }

    #[test]
    fn shell_word_left_ignores_slash_and_respects_quotes() {
        // '/' は区切りにしない（パスはまとめて1単語）
        assert_eq!(shell_word_left_pos("cd /foo/bar"), 3);
        // 通常の空白区切り
        assert_eq!(shell_word_left_pos("foo bar"), 4);
        // 末尾の空白ごと最後の単語を削除対象にする
        assert_eq!(shell_word_left_pos("foo bar  "), 4);
        // クォート内の空白は区切りにしない
        assert_eq!(shell_word_left_pos("a 'b c'"), 2);
        assert_eq!(shell_word_left_pos("a \"b c\""), 2);
        // エスケープした空白も区切りにしない
        assert_eq!(shell_word_left_pos("a b\\ c"), 2);
        // 空・単一単語
        assert_eq!(shell_word_left_pos(""), 0);
        assert_eq!(shell_word_left_pos("foo"), 0);
    }

    #[test]
    fn shell_word_right_advances() {
        // 通常の空白区切り
        assert_eq!(shell_word_right_pos("foo bar", 0), 3);
        assert_eq!(shell_word_right_pos("foo bar", 3), 7);
        // '/' は区切りにしない（パスはまとめて1単語）
        assert_eq!(shell_word_right_pos("cd /foo/bar baz", 3), 11);
        // クォート内の空白は区切りにしない
        assert_eq!(shell_word_right_pos("'foo bar' baz", 0), 9);
        // エスケープした空白も区切りにしない
        assert_eq!(shell_word_right_pos("foo\\ bar baz", 0), 8);
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
