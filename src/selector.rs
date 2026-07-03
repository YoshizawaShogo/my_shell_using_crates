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
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
pub fn run_grid_menu(
    candidates: &[String],
    input_display: u16,
    highlight: &str,
) -> io::Result<Selection> {
    if candidates.is_empty() {
        return Ok(Selection::Dismissed);
    }

    let (term_cols, term_rows) = terminal::size()?;
    let cols = term_cols as usize;

    // 入力行のカーソル物理列 (補完点)。`input_display` は "$ " + prefix の表示幅。
    // ちょうど端末幅の倍数のときは折り返し先頭 (列0) にいるとみなす。
    let input_col = if input_display > 0 && input_display.is_multiple_of(term_cols) {
        0
    } else {
        input_display % term_cols
    };

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

    execute!(stdout(), Print("\r\n"), cursor::Hide)?;

    // 初回はフル描画。以降は選択移動時に旧・新セルだけ上書きする。
    let mut need_full = true;
    let mut prev_selected = 0usize;

    loop {
        if need_full {
            draw_grid(items, selected, n_cols, col_width, n_rows, highlight)?;
            need_full = false;
        } else if prev_selected != selected {
            redraw_grid_cell(items, prev_selected, false, n_cols, col_width, highlight)?;
            redraw_grid_cell(items, selected, true, n_cols, col_width, highlight)?;
        }
        prev_selected = selected;

        // カーソルを入力補完位置に表示する。グリッドは常に入力行の 1 行下に描かれる
        // ので、グリッド先頭行 (現在位置) から 1 行上が入力行。
        queue!(
            stdout(),
            cursor::MoveUp(1),
            cursor::MoveToColumn(input_col),
            cursor::Show
        )?;
        stdout().flush()?;

        let ev = event::read()?;

        // 次の描画のためカーソルをグリッド先頭行・列0へ戻し、描画中は隠す。
        queue!(
            stdout(),
            cursor::Hide,
            cursor::MoveDown(1),
            cursor::MoveToColumn(0)
        )?;

        let Event::Key(key) = ev else {
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
        cursor::Show,
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
    highlight: &str,
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
                    SetBackgroundColor(COLOR_SEL_BG),
                    SetForegroundColor(COLOR_SELECTED)
                )?;
                queue_highlighted(text, highlight, true)?;
                queue!(
                    stdout(),
                    Print(format!("{:pad$}", "", pad = pad)),
                    ResetColor
                )?;
            } else {
                queue!(stdout(), SetForegroundColor(COLOR_NORMAL))?;
                queue_highlighted(text, highlight, false)?;
                queue!(
                    stdout(),
                    Print(format!("{:pad$}", "", pad = pad)),
                    ResetColor
                )?;
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

/// グリッドの 1 セルだけを上書き再描画する。カーソルはグリッド先頭行・列0を基準とする。
fn redraw_grid_cell(
    items: &[String],
    idx: usize,
    is_selected: bool,
    n_cols: usize,
    col_width: usize,
    highlight: &str,
) -> io::Result<()> {
    let row = (idx / n_cols) as u16;
    let col = ((idx % n_cols) * col_width) as u16;
    let text = truncate_to_cols(&items[idx], col_width.saturating_sub(1));
    let pad = col_width.saturating_sub(text.width());

    if row > 0 {
        queue!(stdout(), cursor::MoveDown(row))?;
    }
    queue!(stdout(), cursor::MoveToColumn(col))?;

    if is_selected {
        queue!(
            stdout(),
            SetBackgroundColor(COLOR_SEL_BG),
            SetForegroundColor(COLOR_SELECTED)
        )?;
        queue_highlighted(text, highlight, true)?;
        queue!(
            stdout(),
            Print(format!("{:pad$}", "", pad = pad)),
            ResetColor
        )?;
    } else {
        queue!(stdout(), SetForegroundColor(COLOR_NORMAL))?;
        queue_highlighted(text, highlight, false)?;
        queue!(
            stdout(),
            Print(format!("{:pad$}", "", pad = pad)),
            ResetColor
        )?;
    }

    if row > 0 {
        queue!(stdout(), cursor::MoveUp(row))?;
    }
    queue!(stdout(), cursor::MoveToColumn(0))?;
    stdout().flush()
}

// ─── ハイライトヘルパー ───────────────────────────────────────────────────────

/// `query` の大小無視一致箇所を別色で出力する。
/// `is_selected` が true のとき選択セル背景上でも見えるよう色を切り替える。
/// 呼び出し前後の前景/背景色は呼び出し側が管理する。
fn queue_highlighted(text: &str, query: &str, is_selected: bool) -> io::Result<()> {
    let Some((start, end)) = find_match_range(text, query) else {
        return queue!(stdout(), Print(text));
    };
    if start > 0 {
        queue!(stdout(), Print(&text[..start]))?;
    }
    // ピッカーと同じ黄色でハイライト (選択・非選択とも同色)
    queue!(
        stdout(),
        SetForegroundColor(Color::Rgb {
            r: 0xe2,
            g: 0xd9,
            b: 0x80
        }),
        Print(&text[start..end])
    )?;
    // 呼び出し側が設定した前景色を復元する
    let restore = if is_selected {
        COLOR_SELECTED
    } else {
        COLOR_NORMAL
    };
    queue!(stdout(), SetForegroundColor(restore))?;
    if end < text.len() {
        queue!(stdout(), Print(&text[end..]))?;
    }
    Ok(())
}

/// `text` 内で `query` が（大小無視で）最初に出現するバイト範囲を返す。
fn find_match_range(text: &str, query: &str) -> Option<(usize, usize)> {
    if query.is_empty() {
        return None;
    }
    let tl = text.to_lowercase();
    let ql = query.to_lowercase();
    let start = tl.find(&ql)?;
    let end = start + ql.len();
    if end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end) {
        Some((start, end))
    } else {
        None
    }
}

/// ピッカー用: 空白区切りの複数ワードを全て色付きで出力する。
/// 各ワードの最初の出現をハイライトし、ソート・マージ後に前後の非一致部分と交互に表示。
fn queue_picker_chunk_highlighted(chunk: &str, query: &str, is_selected: bool) -> io::Result<()> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return queue!(stdout(), Print(chunk));
    }
    let ranges = compute_highlight_ranges(chunk, &words);
    if ranges.is_empty() {
        return queue!(stdout(), Print(chunk));
    }
    // ピッカーの配色に合わせたハイライト色 (iceberg yellow)
    let hl_color = Color::Rgb {
        r: 0xe2,
        g: 0xd9,
        b: 0x80,
    };
    let restore_fg = if is_selected {
        COLOR_SELECTED
    } else {
        COLOR_NORMAL
    };
    let mut pos = 0;
    for (start, end) in ranges {
        if start > pos {
            queue!(stdout(), Print(&chunk[pos..start]))?;
        }
        queue!(
            stdout(),
            SetForegroundColor(hl_color),
            Print(&chunk[start..end]),
            SetForegroundColor(restore_fg),
        )?;
        pos = end;
    }
    if pos < chunk.len() {
        queue!(stdout(), Print(&chunk[pos..]))?;
    }
    Ok(())
}

/// `words`（小文字化済み）が `text` 内に出現するバイト範囲をソート・マージして返す。
fn compute_highlight_ranges(text: &str, words: &[String]) -> Vec<(usize, usize)> {
    let tl = text.to_lowercase();
    let mut ranges: Vec<(usize, usize)> = words
        .iter()
        .filter_map(|w| {
            let start = tl.find(w.as_str())?;
            let end = start + w.len();
            if end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end) {
                Some((start, end))
            } else {
                None
            }
        })
        .collect();
    ranges.sort_by_key(|&(s, _)| s);
    // 重複・隣接範囲をマージ
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
            continue;
        }
        merged.push((s, e));
    }
    merged
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

// ─── fuzzy ピッカー (Ctrl+R / Ctrl+T / Ctrl+G) ─────────────────────────────────

/// 候補を fuzzy 絞り込みでインタラクティブに選択させる。
///
/// `initial_query` は初期クエリ。マッチングも UI も crossterm で自前実装する
/// (外部バイナリ不要・本体プロセスで完結)。絞り込み規則は `filter_candidates` を参照。
pub fn run_fzf(candidates: &[String], initial_query: Option<&str>) -> io::Result<Selection> {
    if candidates.is_empty() {
        return Ok(Selection::Dismissed);
    }
    run_picker(candidates.to_vec(), None, initial_query)
}

/// 候補をストリーミングしながら fuzzy 選択させる。
///
/// `produce` は別スレッドで実行され、`emit(String)` を呼んで候補を1件ずつ送る。
/// `emit` が false を返したとき (= ピッカーが終了して rx が drop されたとき) は走査を
/// 中断してよい。これにより巨大なツリーでも「列挙したものから順に表示」でき、
/// 列挙の途中でも Ctrl+C / Esc で即中断できる。
pub fn run_fzf_streaming<F>(produce: F, initial_query: Option<&str>) -> io::Result<Selection>
where
    F: FnOnce(&mut dyn FnMut(String) -> bool) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    // 候補生成は別スレッドで。ピッカーが終了して rx が drop されると send が Err を
    // 返すので emit が false → produce 側はそこで走査を打ち切れる。
    std::thread::spawn(move || {
        let mut emit = |s: String| -> bool { tx.send(s).is_ok() };
        produce(&mut emit);
    });
    run_picker(Vec::new(), Some(rx), initial_query)
}

/// 自前 fuzzy ピッカーの本体。
///
/// `master` は初期候補、`rx` があれば候補がストリーミング流入する。
/// raw mode は呼び出し側で有効なまま使う (skim と違い端末を明け渡さない)。
/// レイアウトは reverse: 最上段にクエリ行、その下にスコア順の候補を並べる。
/// ピッカーの差分描画用レイアウト情報。フル再描画後に計算して保持する。
struct PickerLayout {
    /// 可視候補それぞれのクエリ行からの行オフセット (クエリ行 = 行0)
    cand_rows: Vec<u16>,
    /// 可視候補それぞれの物理行数
    cand_heights: Vec<u16>,
    /// このレイアウトが計算された時点の offset
    offset: usize,
}

fn run_picker(
    mut master: Vec<String>,
    rx: Option<Receiver<String>>,
    initial_query: Option<&str>,
) -> io::Result<Selection> {
    let mut query = initial_query.unwrap_or("").to_string();
    let mut selected = 0usize;
    let mut offset = 0usize;
    let mut filtered: Vec<String> = Vec::new();
    let mut dirty = true;
    let mut outcome = Selection::Dismissed;

    // 差分描画用: None のとき次ループでフル再描画
    let mut layout: Option<PickerLayout> = None;
    let mut prev_selected: Option<usize> = None;

    execute!(stdout(), cursor::Hide)?;
    loop {
        // 1. ストリーミング候補を取り込む
        let mut streaming = false;
        if let Some(rx) = &rx {
            streaming = true;
            loop {
                match rx.try_recv() {
                    Ok(s) => {
                        master.push(s);
                        dirty = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        streaming = false;
                        break;
                    }
                }
            }
        }

        // 2. 絞り込み (クエリ空なら入力順を維持)
        if dirty {
            filtered = filter_candidates(&master, &query);
            if selected >= filtered.len() {
                selected = filtered.len().saturating_sub(1);
            }
            dirty = false;
            layout = None; // 内容変化 → フル再描画
        }

        let (term_cols, term_rows) = terminal::size()?;
        let cols = term_cols as usize;
        let view = ((term_rows as usize) * 2 / 5).max(3);
        let content_width = cols.saturating_sub(2);

        // 3. スクロール判定 (offset が変わるならフル再描画)
        let old_offset = offset;
        if selected < offset {
            offset = selected;
        } else if !filtered.is_empty() && selected >= offset + view {
            offset = selected + 1 - view;
        }
        if offset != old_offset {
            layout = None;
        }

        // 描画前: カーソルを隠してクエリ行の先頭へ (差分描画は列0基準のため)。
        queue!(stdout(), cursor::Hide, cursor::MoveToColumn(0))?;

        // 4. 描画: 選択移動のみなら差分、それ以外はフル
        let moved_only = layout.as_ref().map(|l| l.offset == offset).unwrap_or(false)
            && prev_selected.map(|p| p != selected).unwrap_or(false);

        if moved_only {
            let lo = layout.as_ref().unwrap();
            let prev = prev_selected.unwrap();
            let old_vis = prev.wrapping_sub(offset);
            let new_vis = selected.wrapping_sub(offset);
            if old_vis < lo.cand_rows.len() && new_vis < lo.cand_rows.len() {
                // 旧選択行を非選択色で上書き
                redraw_picker_candidate(
                    &filtered[prev],
                    false,
                    lo.cand_rows[old_vis],
                    lo.cand_heights[old_vis],
                    content_width,
                    &query,
                )?;
                // 新選択行を選択色で上書き
                redraw_picker_candidate(
                    &filtered[selected],
                    true,
                    lo.cand_rows[new_vis],
                    lo.cand_heights[new_vis],
                    content_width,
                    &query,
                )?;
            } else {
                layout = None; // 範囲外: フォールバック
            }
        }

        if layout.is_none() {
            draw_picker(
                &query,
                &filtered,
                selected,
                &mut offset,
                master.len(),
                streaming,
            )?;
            // フル再描画後にレイアウトを計算
            let visible = filtered.len().saturating_sub(offset).min(view);
            let mut cand_rows = Vec::with_capacity(visible);
            let mut cand_heights = Vec::with_capacity(visible);
            let mut row: u16 = 1;
            for i in 0..visible {
                let h = split_display(&filtered[offset + i], content_width).len() as u16;
                cand_rows.push(row);
                cand_heights.push(h);
                row += h;
            }
            layout = Some(PickerLayout {
                cand_rows,
                cand_heights,
                offset,
            });
        }

        // カーソルをクエリ末尾に表示する ("🔍 " は表示幅 3)。検索入力欄として自然な位置。
        let qcol = (3 + query.width()).min(cols.saturating_sub(1)) as u16;
        execute!(stdout(), cursor::MoveToColumn(qcol), cursor::Show)?;

        prev_selected = Some(selected);

        // 5. キー入力 (ストリーミング中はタイムアウト付き)
        if streaming && !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                outcome = Selection::Aborted;
                break;
            }
            KeyCode::Char('g') if ctrl => {
                outcome = Selection::Aborted;
                break;
            }
            KeyCode::Esc => break,
            KeyCode::Enter => {
                if let Some(s) = filtered.get(selected) {
                    outcome = Selection::Chosen(s.clone());
                }
                break;
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('p') if ctrl => selected = selected.saturating_sub(1),
            KeyCode::Down => {
                if selected + 1 < filtered.len() {
                    selected += 1;
                }
            }
            KeyCode::Char('n') if ctrl => {
                if selected + 1 < filtered.len() {
                    selected += 1;
                }
            }
            KeyCode::Backspace => {
                query.pop();
                selected = 0;
                dirty = true;
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                query.push(c);
                selected = 0;
                dirty = true;
            }
            _ => {}
        }
    }

    execute!(
        stdout(),
        cursor::Show,
        cursor::MoveToColumn(0),
        Clear(ClearType::FromCursorDown)
    )?;
    Ok(outcome)
}

/// カーソルがクエリ行 (行0) にある状態で、指定候補を上書き再描画してクエリ行へ戻る。
fn redraw_picker_candidate(
    text: &str,
    is_selected: bool,
    start_row: u16,
    _height: u16,
    content_width: usize,
    query: &str,
) -> io::Result<()> {
    let chunks = split_display(text, content_width);
    let height = chunks.len() as u16;

    if start_row > 0 {
        queue!(stdout(), cursor::MoveDown(start_row))?;
    }
    for (j, chunk) in chunks.iter().enumerate() {
        if is_selected {
            let prefix = if j == 0 { "> " } else { "  " };
            queue!(
                stdout(),
                SetBackgroundColor(COLOR_SEL_BG),
                SetForegroundColor(COLOR_SELECTED),
                Print(prefix),
            )?;
            queue_picker_chunk_highlighted(chunk, query, true)?;
            queue!(stdout(), ResetColor, Print("\r\n"))?;
        } else {
            let prefix = if j == 0 { "# " } else { "  " };
            queue!(stdout(), SetForegroundColor(COLOR_NORMAL), Print(prefix),)?;
            queue_picker_chunk_highlighted(chunk, query, false)?;
            queue!(stdout(), ResetColor, Print("\r\n"))?;
        }
    }
    // クエリ行へ戻る
    queue!(stdout(), cursor::MoveUp(start_row + height))?;
    stdout().flush()
}

/// ピッカーと同じ規則で `candidates` を `query` 絞り込みした結果を返す (件数判定などに使う)。
pub fn filter(candidates: &[String], query: &str) -> Vec<String> {
    filter_candidates(candidates, query)
}

/// `master` を `query` で絞り込む。空クエリなら入力順 (= MRU 順) をそのまま返す。
///
/// クエリは空白区切りのワード列として扱い、各ワードを「連続部分一致・左→右の順序」で
/// 探す (大文字小文字は無視)。例: `/work/AAAAA/BBBB/CCCC` は `"AAA CCC"` に一致するが
/// `"CCC AAA"` には一致しない。マッチした候補は `score_match` のスコア降順、同点は
/// 入力順 (MRU) で並べる。
fn filter_candidates(master: &[String], query: &str) -> Vec<String> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return master.to_vec();
    }
    let mut scored: Vec<(i64, usize, &String)> = master
        .iter()
        .enumerate()
        .filter_map(|(idx, cand)| score_match(cand, &words).map(|s| (s, idx, cand)))
        .collect();
    // スコア降順、同点は idx 昇順 (master は MRU 順なので直近が先)
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, c)| c.clone()).collect()
}

/// `cand` が全ワードに部分一致すればスコアを返す。ワードの順序は問わない。
///
/// スコア基準:
/// - いずれかのワードが basename にかかれば +50
/// - 全ワードが入力順に出現すれば +20 (順序一致ボーナス)
/// - 同点は呼び出し側の MRU 順 (idx) で決まる
fn score_match(cand: &str, words: &[String]) -> Option<i64> {
    let hay = cand.to_lowercase();

    // 各ワードが独立して存在するか確認 (順序不問)
    for w in words {
        if !hay.contains(w.as_str()) {
            return None;
        }
    }

    let mut score = 0i64;

    // basename ボーナス: いずれかのワードが basename 部分に含まれる
    let basename_start = hay.rfind('/').map(|i| i + 1).unwrap_or(0);
    let basename = &hay[basename_start..];
    if words.iter().any(|w| basename.contains(w.as_str())) {
        score += 50;
    }

    // 順序ボーナス: ワードが入力順に出現する場合
    let mut pos = 0;
    let in_order = words.iter().all(|w| {
        hay[pos..]
            .find(w.as_str())
            .map(|rel| {
                pos += rel + w.len();
                true
            })
            .unwrap_or(false)
    });
    if in_order {
        score += 20;
    }

    Some(score)
}

// ─── ピッカー配色定数 ────────────────────────────────────────────────────────
const COLOR_QUERY: Color = Color::AnsiValue(117); // 淡青 (iceberg)
const COLOR_SELECTED: Color = Color::AnsiValue(253); // 薄白
const COLOR_SEL_BG: Color = Color::AnsiValue(237); // 暗灰背景
const COLOR_NORMAL: Color = Color::AnsiValue(146); // 灰色寄り淡青
const COLOR_COUNT: Color = Color::AnsiValue(243); // 灰 (件数表示)

/// ピッカーを 1 フレーム描画する。
///
/// カーソルは開始行の桁0にある前提で、クエリ行＋候補を下方向に描き、
/// 最後に開始行へ戻す。`offset` は選択がウィンドウ内に収まるよう更新する。
///
/// - クエリ行: 🔍 + テキスト
/// - 選択行:   >  + テキスト (bg=237, fg=253)、折り返しは "  "
/// - 非選択行: #  + テキスト (fg=146)、折り返しは "  "
fn draw_picker(
    query: &str,
    filtered: &[String],
    selected: usize,
    offset: &mut usize,
    total: usize,
    streaming: bool,
) -> io::Result<()> {
    let (term_cols, term_rows) = terminal::size()?;
    let cols = term_cols as usize;

    // 候補表示の高さ: 端末の約40% (最小3行)。
    let view = ((term_rows as usize) * 2 / 5).max(3);

    // 選択がウィンドウ内に収まるよう offset を調整
    if selected < *offset {
        *offset = selected;
    } else if selected >= *offset + view {
        *offset = selected + 1 - view;
    }
    let visible = filtered.len().saturating_sub(*offset).min(view);

    // 右端に「マッチ数/総数」を表示。走査途中は総数の後ろに … を付ける。
    let count_str = format!(
        "{}/{}{}",
        filtered.len(),
        total,
        if streaming { "…" } else { "" }
    );
    let count_width = count_str.width() as u16;

    // クエリ行: 🔍 は表示幅2なのでプレフィックス幅=3 ("🔍 ")
    // カウントと被らないようクエリを切り詰め、右端に右寄せで配置する。
    let query_max = cols.saturating_sub(count_str.width() + 1);
    queue!(
        stdout(),
        cursor::MoveToColumn(0),
        Clear(ClearType::FromCursorDown),
        SetForegroundColor(COLOR_QUERY),
        Print(truncate_to_cols(&format!("🔍 {}", query), query_max)),
        ResetColor,
        cursor::MoveToColumn(term_cols - count_width),
        SetForegroundColor(COLOR_COUNT),
        Print(&count_str),
        ResetColor,
        Print("\r\n"),
    )?;

    // 候補行。テキストを (cols - 2) 幅のチャンクに分割する。
    // 選択行の先頭: "> " / 非選択行の先頭: "# " / 継続行: "  "
    let content_width = cols.saturating_sub(2);
    let mut cand_lines: u16 = 0;
    for i in 0..visible {
        let idx = *offset + i;
        let text = &filtered[idx];
        let chunks = split_display(text, content_width);
        cand_lines += chunks.len() as u16;
        for (j, chunk) in chunks.iter().enumerate() {
            if idx == selected {
                let prefix = if j == 0 { "> " } else { "  " };
                queue!(
                    stdout(),
                    SetBackgroundColor(COLOR_SEL_BG),
                    SetForegroundColor(COLOR_SELECTED),
                    Print(prefix),
                )?;
                queue_picker_chunk_highlighted(chunk, query, true)?;
                queue!(stdout(), ResetColor, Print("\r\n"))?;
            } else {
                let prefix = if j == 0 { "# " } else { "  " };
                queue!(stdout(), SetForegroundColor(COLOR_NORMAL), Print(prefix),)?;
                queue_picker_chunk_highlighted(chunk, query, false)?;
                queue!(stdout(), ResetColor, Print("\r\n"))?;
            }
        }
    }

    // 開始行へカーソルを戻す (クエリ1行 + 候補の物理行数)
    let drawn = 1 + cand_lines;
    queue!(stdout(), cursor::MoveUp(drawn), cursor::MoveToColumn(0))?;
    stdout().flush()
}

/// テキストを表示幅 `max_width` ごとのチャンクに分割する。
/// 空文字列・max_width=0 は `[""]` を返す。
fn split_display(s: &str, max_width: usize) -> Vec<&str> {
    if max_width == 0 || s.is_empty() {
        return vec![s];
    }
    let mut chunks = Vec::new();
    let mut chunk_start = 0;
    let mut width = 0usize;
    for (i, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if width + w > max_width {
            chunks.push(&s[chunk_start..i]);
            chunk_start = i;
            width = w;
        } else {
            width += w;
        }
    }
    chunks.push(&s[chunk_start..]);
    chunks
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

    fn words(q: &str) -> Vec<String> {
        q.split_whitespace().map(str::to_lowercase).collect()
    }

    #[test]
    fn match_any_order() {
        let path = "/work/AAAAA/BBBB/CCCC";
        // 順序通りでもマッチ
        assert!(score_match(path, &words("AAA CCC")).is_some());
        // 逆順でもマッチ
        assert!(score_match(path, &words("CCC AAA")).is_some());
        // 順序通りの方がスコアが高い
        let in_order = score_match(path, &words("AAA CCC")).unwrap();
        let reversed = score_match(path, &words("CCC AAA")).unwrap();
        assert!(in_order > reversed);
    }

    #[test]
    fn match_is_case_insensitive() {
        assert!(score_match("/Work/Projects", &words("work pro")).is_some());
    }

    #[test]
    fn missing_word_does_not_match() {
        assert!(score_match("/a/b/c", &words("a z")).is_none());
    }

    #[test]
    fn shorter_path_scores_higher() {
        // 長短に関わらず basename マッチなら同スコア → MRU 順で決まる
        let short = score_match("/a/foo", &words("foo")).unwrap();
        let long = score_match("/a/very/long/path/foo", &words("foo")).unwrap();
        assert_eq!(short, long);
    }

    #[test]
    fn basename_match_gets_bonus() {
        // 同じパスでも basename にかかる方が高スコア
        let in_basename = score_match("/src/main", &words("main")).unwrap();
        let in_dir = score_match("/main/src", &words("main")).unwrap();
        assert!(in_basename > in_dir);
    }

    #[test]
    fn empty_query_keeps_input_order() {
        let master = vec!["b".to_string(), "a".to_string()];
        assert_eq!(filter_candidates(&master, ""), master);
    }
}
