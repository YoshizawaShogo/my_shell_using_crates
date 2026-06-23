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
                // 選択中は白背景＋黒文字 (iceberg トーンの淡色で目に優しく)
                queue!(
                    stdout(),
                    SetBackgroundColor(Color::Rgb {
                        r: 0xc6,
                        g: 0xc8,
                        b: 0xd1
                    }),
                    SetForegroundColor(Color::Rgb {
                        r: 0x16,
                        g: 0x18,
                        b: 0x21
                    }),
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
fn run_picker(
    mut master: Vec<String>,
    rx: Option<Receiver<String>>,
    initial_query: Option<&str>,
) -> io::Result<Selection> {
    let mut query = initial_query.unwrap_or("").to_string();
    let mut selected = 0usize;
    let mut offset = 0usize;
    let mut filtered: Vec<String> = Vec::new();
    let mut dirty = true; // 候補かクエリが変化したら再絞り込み
    let mut outcome = Selection::Dismissed;

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
                        streaming = false; // 生成完了 → 以降はブロッキング待機
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
        }

        // 3. 描画
        draw_picker(&query, &filtered, selected, &mut offset)?;

        // 4. キー入力 (ストリーミング中はタイムアウト付きで流入を取りこぼさない)
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

    // 後始末: 描画領域を消してカーソルを開始行に戻す (redraw_prompt が続きを描く)
    execute!(
        stdout(),
        cursor::MoveToColumn(0),
        Clear(ClearType::FromCursorDown)
    )?;
    Ok(outcome)
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

/// `cand` が全ワードに「連続部分一致・順序保持」でマッチすればスコアを返す。
///
/// スコアは「短いパスほど高い (= パス長ペナルティ)」を基本に、最後のワードが
/// basename (末尾 `/` 以降) 内でマッチしたらボーナスを加える。
fn score_match(cand: &str, words: &[String]) -> Option<i64> {
    let hay = cand.to_lowercase();
    let mut pos = 0; // ここから後ろを探す (順序を保証)
    let mut last_match_end = 0;
    for w in words {
        let rel = hay[pos..].find(w.as_str())?;
        last_match_end = pos + rel + w.len();
        pos = last_match_end;
    }
    let mut score = -(hay.chars().count() as i64); // 短いほど高い
    let basename_start = hay.rfind('/').map(|i| i + 1).unwrap_or(0);
    if last_match_end > basename_start {
        score += 50; // 最後のワードが basename にかかっていれば加点
    }
    Some(score)
}

/// ピッカーを 1 フレーム描画する。
///
/// カーソルは開始行の桁0にある前提で、クエリ行＋候補を下方向に描き、
/// 最後に開始行へ戻す。`offset` は選択がウィンドウ内に収まるよう更新する。
/// 行折り返しによる `MoveUp` のずれを避けるため各行は端末幅で切り詰める。
fn draw_picker(
    query: &str,
    filtered: &[String],
    selected: usize,
    offset: &mut usize,
) -> io::Result<()> {
    let (term_cols, term_rows) = terminal::size()?;
    let cols = term_cols as usize;

    // 候補表示の高さ: 端末の約40% (最小3行)。
    // grid menu と同様、現在のカーソル行は問い合わせず (cursor::position は端末への
    // DSR 往復が必要で本コードベースは避けている)、画面の一部に収まる範囲に留める。
    let view = ((term_rows as usize) * 2 / 5).max(3);

    // 選択がウィンドウ内に収まるよう offset を調整
    if selected < *offset {
        *offset = selected;
    } else if selected >= *offset + view {
        *offset = selected + 1 - view;
    }
    let visible = filtered.len().saturating_sub(*offset).min(view);

    // クエリ行
    queue!(
        stdout(),
        cursor::MoveToColumn(0),
        Clear(ClearType::FromCursorDown),
        SetForegroundColor(Color::Cyan),
        Print(truncate_to_cols(&format!("> {}", query), cols)),
        ResetColor,
        Print("\r\n"),
    )?;

    // 候補行 (選択行を反転表示)
    for i in 0..visible {
        let idx = *offset + i;
        let text = truncate_to_cols(&filtered[idx], cols);
        if idx == selected {
            queue!(
                stdout(),
                SetBackgroundColor(Color::Blue),
                SetForegroundColor(Color::White),
                Print(text),
                ResetColor,
            )?;
        } else {
            queue!(stdout(), Print(text))?;
        }
        queue!(stdout(), Print("\r\n"))?;
    }

    // 開始行へカーソルを戻す (printした行数 = クエリ1 + visible)
    let drawn = (1 + visible) as u16;
    queue!(stdout(), cursor::MoveUp(drawn), cursor::MoveToColumn(0))?;
    stdout().flush()
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
    fn match_respects_word_order() {
        let path = "/work/AAAAA/BBBB/CCCC";
        assert!(score_match(path, &words("AAA CCC")).is_some());
        assert!(score_match(path, &words("CCC AAA")).is_none());
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
        let short = score_match("/a/foo", &words("foo")).unwrap();
        let long = score_match("/a/very/long/path/foo", &words("foo")).unwrap();
        assert!(short > long);
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
