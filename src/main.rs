mod builtin;
mod completion;
mod editor;
mod events;
mod exec;
mod history;
mod job;
mod provider;
mod selector;
mod term;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, stdout};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use builtin::ShellContext;
use completion::TabContext;
use editor::{LineEditor, redraw_prompt};
use events::{ShellEvent, handle_key};
use exec::execute_command;
use history::History;
use selector::{PickerKind, Selection};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use term::{RawModeGuard, setup_signal_handlers};

/// RC ファイルパス (起動時に読み込む設定ファイル)
const RC_FILE: &str = "~/.my_shell_rc";

// ─── Shell ────────────────────────────────────────────────────────────────────

struct Shell {
    ed: LineEditor,
    history: History,
    /// プロンプトに出す git の状態 (`main a1b2c3d` 等)
    git_info: Option<String>,
    ctx: ShellContext,
    ghost: Option<String>,
    /// 履歴ナビゲーション中の位置 (None = ナビゲーション外)
    hist_idx: Option<usize>,
    /// ナビゲーション開始前の入力を保存する
    saved_input: String,
    /// 停止ジョブがある状態で一度終了を試みたか (2 回目で実際に終了する)
    jobs_warned: bool,
}

impl Shell {
    fn new() -> Self {
        Self {
            ed: LineEditor::default(),
            history: History::load(),
            git_info: fetch_git_info(),
            ctx: ShellContext::default(),
            ghost: None,
            hist_idx: None,
            saved_input: String::new(),
            jobs_warned: false,
        }
    }

    fn redraw(&mut self) -> io::Result<()> {
        self.history.reload();
        builtin::reload_recent_paths(&mut self.ctx);
        redraw_prompt(
            &mut self.ed,
            self.git_info.as_deref(),
            self.ghost.as_deref(),
        )
    }

    /// fuzzy ピッカーを開く。ピッカー内で別種別のキー (Ctrl+R/T/G/P) が押されたら
    /// プロンプトへ戻らずその場で切り替え、クエリを引き継ぐ。
    fn show_picker(&mut self, kind: PickerKind, pending: &mut Vec<ShellEvent>) -> io::Result<()> {
        execute!(stdout(), Print("\r\n"))?;
        self.ed.note_newline();
        let mut kind = kind;
        // 切り替え時に引き継ぐクエリ。None なら各ピッカーの既定の初期クエリを使う。
        let mut carried: Option<String> = None;
        loop {
            let sel = match kind {
                PickerKind::History => self.pick_history(carried.take()),
                PickerKind::Files => self.pick_file(carried.take()),
                PickerKind::Recent => self.pick_path(PickerKind::Recent, carried.take()),
                PickerKind::DirStack => self.pick_path(PickerKind::DirStack, carried.take()),
                // ジョブ選択はキーバインドから開かないのでここには来ない
                PickerKind::Jobs => Ok(Selection::Dismissed),
            };
            match sel {
                Ok(Selection::Switch(next, query)) => {
                    kind = next;
                    carried = Some(query);
                }
                Ok(_) => break,
                Err(e) => {
                    execute!(stdout(), Print(format!("fzf: {}\r\n", e)))?;
                    break;
                }
            }
        }
        pending.push(ShellEvent::RedrawPrompt);
        Ok(())
    }

    /// Ctrl+R: 履歴ピッカー。選んだコマンドで入力行を置き換える。
    fn pick_history(&mut self, carried: Option<String>) -> io::Result<Selection> {
        // 他端末が直前に追記した履歴を取り込んでから開く
        self.history.reload();
        let query = carried.unwrap_or_else(|| self.ed.line().to_string());
        let sel = completion::fzf_history(&query, &self.history)?;
        if let Selection::Chosen(s) = &sel {
            self.ed.set(s.clone());
        }
        Ok(sel)
    }

    /// Ctrl+T: cwd 以下のファイル/ディレクトリピッカー。
    fn pick_file(&mut self, carried: Option<String>) -> io::Result<Selection> {
        let cwd = std::env::current_dir().unwrap_or_default();
        // 入力が空のときは cd モード: ディレクトリのみ列挙し `cd <path>` で挿入する。
        let empty = self.ed.is_empty();
        // カーソル前のトークンを取得（owned にして借用を切る）
        let token: String = {
            let before = &self.ed.line()[..self.ed.cursor()];
            let token_start = before.rfind(' ').map(|i| i + 1).unwrap_or(0);
            before[token_start..].to_owned()
        };
        // トークンに '/' が含まれる場合はそのディレクトリをルートにする
        let (root, token_query, dir_part) = if !token.is_empty() && token.contains('/') {
            let last_slash = token.rfind('/').unwrap();
            let dir = token[..=last_slash].to_owned();
            let file_part = token[last_slash + 1..].to_owned();
            let root = if dir.starts_with('/') {
                std::path::PathBuf::from(&dir)
            } else if let Some(rest) = dir.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                std::path::PathBuf::from(home).join(rest)
            } else {
                cwd.join(&dir)
            };
            let query = if file_part.is_empty() {
                None
            } else {
                Some(file_part)
            };
            (root, query, Some(dir))
        } else {
            (cwd.clone(), None, None)
        };
        // 切り替えで来たときは引き継いだクエリを優先する
        let initial_query = carried.or(token_query);
        // 行頭のコマンドが cd のとき、または入力が空のときはディレクトリのみに絞る
        let dirs_only = empty
            || self
                .ed
                .line()
                .split_whitespace()
                .next()
                .is_some_and(|cmd| cmd == "cd");
        let sel = completion::fzf_files(&root, initial_query.as_deref(), dirs_only)?;
        if let Selection::Chosen(s) = &sel {
            if let Some(ref dir) = dir_part {
                self.ed.delete_before_cursor(token.len());
                let filename = s.strip_prefix("./").unwrap_or(s);
                self.ed.insert_str(&format!("{}{}", dir, filename));
            } else if empty {
                let dir = s.strip_prefix("./").unwrap_or(s);
                self.ed.insert_str(&format!("cd {}", dir));
            } else {
                self.ed.insert_str(s);
            }
        }
        Ok(sel)
    }

    /// Ctrl+G / Ctrl+P: 記録パス (MRU) / ディレクトリスタックのピッカー。
    fn pick_path(&mut self, kind: PickerKind, carried: Option<String>) -> io::Result<Selection> {
        // 入力が空のときは cd 先選択モード: 候補をディレクトリのみに絞り、
        // 選択結果を `cd <path>` として挿入する。
        let prepend_cd = self.ed.is_empty();
        let candidates: Vec<PathBuf> = if kind == PickerKind::DirStack {
            // dir_stack は古い順 (末尾が直近) なので逆順でピッカーへ渡す
            self.ctx.dir_stack.iter().rev().cloned().collect()
        } else {
            // 他端末が直前に記録したパスを取り込んでから開く
            builtin::reload_recent_paths(&mut self.ctx);
            // is_dir は記録/読込時に確定済みなので、ここで stat し直さない。
            self.ctx
                .recent_paths
                .iter()
                .filter(|rp| !prepend_cd || rp.is_dir)
                .map(|rp| rp.path.clone())
                .collect()
        };
        let sel = completion::fzf_paths(&candidates, carried.as_deref(), kind)?;
        if let Selection::Chosen(s) = &sel {
            if prepend_cd {
                self.ed.insert_str(&format!("cd {}", s));
            } else {
                self.ed.insert_str(s);
            }
        }
        Ok(sel)
    }

    /// コマンド文字列を実行し、履歴追加・終了ステータス表示・git 情報更新・ジョブ回収
    /// までの後処理をまとめて行う。`ExecuteCommand` (単一行) と `ExecutePasted`
    /// (複数行) の共通処理。終了要求 (`exit` ビルトイン) なら `Exit` を積む。
    fn finish_execute(&mut self, cmd: String, pending: &mut Vec<ShellEvent>) -> io::Result<bool> {
        execute_command(&cmd, &mut self.ctx, true)?;
        if !cmd.trim().is_empty() {
            self.history.add(&cmd);
        }
        if self.ctx.last_status != 0 {
            execute!(
                stdout(),
                SetForegroundColor(Color::Rgb {
                    r: 0xe2,
                    g: 0x78,
                    b: 0x78
                }),
                Print(format!("↳ exit {}\r\n", self.ctx.last_status)),
                ResetColor,
            )?;
        }
        self.git_info = fetch_git_info();
        self.ghost = None;
        self.hist_idx = None;
        self.saved_input.clear();
        // exit ビルトインの終了要求。jobs_warned はリセットせずに Exit へ渡す
        // (停止ジョブがあるとき、2 回目の exit で実際に終了させるため)。
        if self.ctx.exit_status.is_some() {
            pending.push(ShellEvent::Exit);
            return Ok(false);
        }
        self.jobs_warned = false;
        // 完了/停止したバックグラウンドジョブをプロンプト表示前に通知する。
        job::reap_finished(&mut self.ctx)?;
        pending.push(ShellEvent::RedrawPrompt);
        Ok(false)
    }

    /// 1 個の `ShellEvent` を処理する。
    ///
    /// 端末 I/O を伴うため戻り値は `io::Result`。終了要求なら `Ok(true)` を返す。
    /// 後続処理 (主に再描画) は `pending` に積んで呼び出し側のキューに委ねる。
    fn handle_event(&mut self, ev: ShellEvent, pending: &mut Vec<ShellEvent>) -> io::Result<bool> {
        match ev {
            ShellEvent::Exit => {
                // 停止ジョブがあれば 1 回目は警告して残す。2 回目で SIGHUP して終了。
                if !self.ctx.jobs.is_empty() && !self.jobs_warned {
                    self.jobs_warned = true;
                    // 終了要求を取り下げる。残すと次のコマンド実行後に終了してしまう。
                    self.ctx.exit_status = None;
                    execute!(
                        stdout(),
                        Print("There are stopped jobs (press Ctrl+D again to exit)\r\n")
                    )?;
                    // 警告行を残したまま下に新しいプロンプトを描く (^C と同じ要領)。
                    // reset しないと redraw の MoveUp+Clear で警告が消える。
                    self.ed.reset_lines_above();
                    pending.push(ShellEvent::RedrawPrompt);
                    return Ok(false);
                }
                job::hangup_all(&self.ctx);
                return Ok(true);
            }

            ShellEvent::CancelInput => {
                self.ed.take();
                self.ghost = None;
                self.hist_idx = None;
                self.saved_input.clear();
                execute!(stdout(), Print("^C\r\n"))?;
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::RedrawPrompt => {
                self.ghost = compute_ghost(&self.ed, &self.history);
                self.redraw()?;
            }

            ShellEvent::ExecuteCommand => {
                // 1. abbr を視覚展開してカーソルを行末へ
                self.ed.move_end();
                try_expand_abbr(&mut self.ed, &self.ctx.abbrs);
                // 2. ゴーストを消してクリーンな表示にしてから改行
                self.ghost = None;
                self.redraw()?;
                // 3. バッファを取得してエディタをクリアし、実行する
                let cmd = self.ed.take();
                return self.finish_execute(cmd, pending);
            }

            // 複数行ペーストを 1 コマンドとして実行する。エディタバッファは複数行を
            // 描画できないので経由せず、正規化済みのスクリプトを直接実行する。
            ShellEvent::ExecutePasted(script) => {
                return self.finish_execute(script, pending);
            }

            ShellEvent::AcceptGhost => {
                if let Some(ghost) = self.ghost.take() {
                    let new_buf = self.ed.line().to_string() + &ghost;
                    self.ed.set(new_buf);
                }
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::ShowCompletion => {
                let prefix = self.ed.line()[..self.ed.cursor()].to_string();
                // カーソル後ろのテキストは補完で消さずに残す。
                let suffix = self.ed.line()[self.ed.cursor()..].to_string();
                let cwd = std::env::current_dir().unwrap_or_default();
                // fg/bg 補完用にジョブのコマンド名 (先頭トークン) を渡す。
                let job_names: Vec<String> = self
                    .ctx
                    .jobs
                    .iter()
                    .filter_map(|j| j.cmd.split_whitespace().next().map(str::to_string))
                    .collect();
                let tab_ctx = TabContext {
                    prefix: &prefix,
                    cwd: &cwd,
                    history: &self.history,
                    jobs: &job_names,
                    tab_end: self.ed.tab_end(),
                };
                match completion::tab_complete(tab_ctx)? {
                    Selection::Chosen(choice) => {
                        self.ed.apply_completion(choice, &suffix);
                    }
                    // グリッドメニューは Switch を返さない
                    Selection::Aborted | Selection::Dismissed | Selection::Switch(..) => {}
                    Selection::InsertChar(c) => self.ed.insert(c),
                    Selection::Backspace => self.ed.backspace(),
                }
                // reset_cursor_tracking は呼ばない:
                // グリッドメニューは MoveUp(1) で入力行に戻るので
                // lines_above_cursor をそのまま使って redraw できる。
                // 候補なし (Dismissed) のときは cursor が動いていないため特に重要。
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::HistoryPrev => {
                let n = self.history.len();
                if n == 0 {
                    return Ok(false);
                }
                // 最初の押下時: 現在のバッファをプレフィックスとして保存
                if self.hist_idx.is_none() {
                    self.saved_input = self.ed.line().to_string();
                }
                let prefix = self.saved_input.clone();
                let cwd = std::env::current_dir().unwrap_or_default();
                let start = self.hist_idx.unwrap_or(n);
                // start より前でプレフィックスに一致する最新エントリを探す。
                // ls/cd の引数パスが消えている候補はスキップするが、
                // 直前の 1 コマンド (i == n-1) は常に候補に出す。
                let found = (0..start).rev().find(|&i| {
                    self.history
                        .get_cmd(i)
                        .map(|cmd| {
                            cmd.starts_with(&prefix)
                                && (i == n - 1 || completion::cmd_paths_exist(cmd, &cwd))
                        })
                        .unwrap_or(false)
                });
                if let Some(idx) = found {
                    self.hist_idx = Some(idx);
                    if let Some(cmd) = self.history.get_cmd(idx) {
                        self.ed.set(cmd.to_string());
                    }
                }
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::HistoryNext => {
                match self.hist_idx {
                    None => {} // ナビゲーション外では無視
                    Some(i) => {
                        let n = self.history.len();
                        let prefix = self.saved_input.clone();
                        let cwd = std::env::current_dir().unwrap_or_default();
                        // i より後でプレフィックスに一致するエントリを探す。
                        // ls/cd の引数パスが消えている候補はスキップするが、
                        // 直前の 1 コマンド (j == n-1) は常に候補に出す。
                        let found = (i + 1..n).find(|&j| {
                            self.history
                                .get_cmd(j)
                                .map(|cmd| {
                                    cmd.starts_with(&prefix)
                                        && (j == n - 1 || completion::cmd_paths_exist(cmd, &cwd))
                                })
                                .unwrap_or(false)
                        });
                        if let Some(idx) = found {
                            self.hist_idx = Some(idx);
                            if let Some(cmd) = self.history.get_cmd(idx) {
                                self.ed.set(cmd.to_string());
                            }
                        } else {
                            // 一致なし: 保存済み入力を復元
                            self.hist_idx = None;
                            let saved = std::mem::take(&mut self.saved_input);
                            self.ed.set(saved);
                        }
                    }
                }
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::ShowHistoryFzf => self.show_picker(PickerKind::History, pending)?,
            ShellEvent::ShowFileFzf => self.show_picker(PickerKind::Files, pending)?,
            ShellEvent::ShowRecentPathFzf => self.show_picker(PickerKind::Recent, pending)?,
            ShellEvent::ShowDirStackFzf => self.show_picker(PickerKind::DirStack, pending)?,

            ShellEvent::InsertSpace => {
                try_expand_abbr(&mut self.ed, &self.ctx.abbrs);
                self.ed.insert(' ');
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::ClearScreen => {
                execute!(stdout(), Clear(ClearType::All), cursor::MoveTo(0, 0))?;
                self.ed.reset_lines_above();
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::InsertLastArg => {
                let last_arg = self
                    .history
                    .last_cmd()
                    .and_then(|cmd| cmd.split_whitespace().last().map(str::to_string));
                if let Some(arg) = last_arg {
                    self.ed.insert_str(&arg);
                }
                pending.push(ShellEvent::RedrawPrompt);
            }
        }
        Ok(false)
    }
}

// ─── エントリポイント ─────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
    // glibc の malloc arena を1個に固定して VSZ 肥大を防ぐ。
    // このシェルは実質シングルスレッドで arena 分割の利点が無く、補完時に
    // 一時生成する短命スレッド (Ctrl+T のストリーミング列挙) が arena を量産して
    // 仮想メモリが膨らむのを抑える保険。
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
    setup_signal_handlers()?;
    let status = {
        let _guard = RawModeGuard::new()?;
        let result = run();
        let _ = execute!(stdout(), Print("\r\n"));
        result?
        // _guard の Drop で disable_raw_mode
        // shell.history の Drop で HISTORY_FILE へ保存
    };
    // Drop をすべて走らせてから終了する (process::exit は Drop を飛ばすので、
    // raw mode 解除と履歴保存が済んだこの位置でだけ呼ぶ)。
    std::process::exit(status);
}

/// 対話ループ。戻り値はシェルの終了ステータス (`exit [status]` が指定した値)。
fn run() -> io::Result<i32> {
    let mut shell = Shell::new();

    // 起動時に PWD を実際のカレントへ同期しておく。親から継承した PWD が古い / 未設定
    // でも `$PWD` が正しくなる (以降は cd が更新する)。
    // SAFETY: 起動直後・シングルスレッドで、environ への並行アクセスはない。
    if let Ok(cwd) = std::env::current_dir() {
        unsafe { std::env::set_var("PWD", &cwd) };
    }

    // RC ファイルを読み込んで各行を実行する
    load_rc(&mut shell.ctx);

    // 起動時の cwd を端末へ通知する (以降は cd が送る)。RC に cd があってもその
    // あとで送るので最終的な cwd が出る。
    term::notify_cwd();

    shell.ghost = compute_ghost(&shell.ed, &shell.history);
    shell.redraw()?;

    loop {
        // プロンプト入力の直前に端末状態を強制復帰する。子の異常終了で端末が cooked の
        // まま取り残されても、ここで raw を張り直し端末所有権を奪い返すので、Ctrl+C 等が
        // 効かなくなった状態から確実に立ち直る。
        term::reassert_terminal();
        let mut pending = match event::read()? {
            Event::Key(key) => handle_key(&mut shell.ed, key),
            // ブラケットペースト: 改行入りでも 1 イベントで届く。
            // 1 行なら挿入するだけ (実行は従来どおりユーザーの Enter を待つ)。
            // 2 行以上は中身をプレビューして確認し、承諾されたらそのまま 1 行ずつ実行する
            // (何が走るかは確認画面で見せているので、改めて Enter を求めない)。
            Event::Paste(data) => {
                let lines = editor::paste_lines(&data);
                if lines.len() < 2 {
                    shell.ed.insert_paste(&data);
                    vec![ShellEvent::RedrawPrompt]
                } else if confirm_multiline_paste(&mut shell.ed, &lines)? {
                    // 改行を保ったまま 1 コマンドとして実行する (行ごとの分割はしない)。
                    vec![ShellEvent::ExecutePasted(editor::normalize_newlines(&data))]
                } else {
                    vec![ShellEvent::RedrawPrompt]
                }
            }
            // 端末サイズ変更 (タブ複製直後の winsize 伝搬含む) で再描画し、幅を反映する。
            Event::Resize(..) => vec![ShellEvent::RedrawPrompt],
            _ => continue,
        };

        // イベント処理が新たなイベント (主に再描画) を生むため、空になるまで回す。
        while !pending.is_empty() {
            for ev in std::mem::take(&mut pending) {
                if shell.handle_event(ev, &mut pending)? {
                    return Ok(shell.ctx.exit_status.unwrap_or(0));
                }
            }
        }
    }
}

// ─── 複数行ペーストの確認 ─────────────────────────────────────────────────────

/// 表示幅 `max_cols` に収まるよう切り詰める。切ったときは末尾に `…` を付ける。
fn truncate_with_ellipsis(s: &str, max_cols: usize) -> String {
    if s.width() <= max_cols {
        return s.to_string();
    }
    let mut w = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        // `…` の 1 桁分を残す
        if w + cw > max_cols.saturating_sub(1) {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// 確認プロンプトのプレビュー 1 行を、飾り付きで次の行へ出力する。
fn queue_paste_preview_row(text: &str) -> io::Result<()> {
    execute!(
        stdout(),
        Print("\r\n"),
        SetForegroundColor(Color::AnsiValue(243)),
        Print(PASTE_GUTTER),
        SetForegroundColor(Color::AnsiValue(253)),
        Print(text),
        ResetColor,
    )
}

/// 確認プロンプトのプレビューに使える行数を求める。
///
/// プロンプト (折り返し込み) + 確認ヘッダー + プレビューが画面に収まる範囲に抑える。
/// これを超えて描くとプロンプト先頭行が画面外へスクロールし、再描画の `MoveUp` が
/// 届かなくなって表示が壊れる。
fn paste_preview_capacity(ed: &LineEditor, term_rows: u16) -> usize {
    let prompt_rows = ed.lines_above() as usize + 1;
    (term_rows as usize).saturating_sub(prompt_rows + 1)
}

/// プレビュー行の先頭に付ける飾り。
const PASTE_GUTTER: &str = "  │ ";

/// 確認プロンプトの操作ヒント。幅が足りないときは省く。
const PASTE_HINT: &str = "[Enter: run / Esc: cancel]";

/// 複数行の貼り付けを確認し、承諾されたか返す (呼び出し側で 2 行以上を保証する)。
///
/// ```text
/// Run 5 pasted lines?  [Enter: run / Esc: cancel]
///   │ cd src
///   │ cargo build
///   │ echo one
///   │ … +2 more
/// ```
///
/// プレビューは画面の残り行数いっぱいまで出し、あふれる分は件数にまとめる。
///
/// Enter で承諾 (呼び出し側がそのまま実行する)、Esc / Ctrl+C で破棄する。画面の扱いは
/// ピッカーと同じで、入力行の下に質問を出し、終了時にそこを消してから再描画へ戻す。
fn confirm_multiline_paste(ed: &mut LineEditor, lines: &[String]) -> io::Result<bool> {
    // どの行も折り返させない。折り返すと note_newline() で数える行数と実際の行数が
    // ずれ、再描画時の MoveUp が足りずに古いプロンプトが残る。
    let (term_cols, term_rows) = terminal::size().unwrap_or((80, 24));
    let cols = (term_cols as usize).max(2);
    let limit = cols - 1;
    let capacity = paste_preview_capacity(ed, term_rows);

    // ヘッダー行: 質問 + 操作ヒント (幅が足りなければヒントは省く)
    let head = format!("Run {} pasted lines?", lines.len());
    execute!(
        stdout(),
        Print("\r\n"),
        SetForegroundColor(Color::Rgb {
            r: 0xe2,
            g: 0xd9,
            b: 0x80
        }),
        Print(truncate_with_ellipsis(&head, limit)),
        ResetColor,
    )?;
    if limit >= head.width() + 2 + PASTE_HINT.width() {
        execute!(stdout(), Print(format!("  {}", PASTE_HINT)))?;
    }
    let mut rows = 1usize;

    // プレビュー行: 画面に入るだけ出し、収まらない分は最終行に `… +N more` とまとめる。
    let body_width = limit.saturating_sub(PASTE_GUTTER.width());
    if body_width >= 2 {
        let (shown, more) = if lines.len() <= capacity {
            (lines.len(), false)
        } else if capacity >= 2 {
            // 最終行を残り件数に使う
            (capacity - 1, true)
        } else {
            // 1 行しか出せないなら先頭行だけ (件数はヘッダーに出ている)
            (capacity, false)
        };
        for line in &lines[..shown] {
            queue_paste_preview_row(&truncate_with_ellipsis(line, body_width))?;
            rows += 1;
        }
        if more {
            let msg = format!("… +{} more", lines.len() - shown);
            queue_paste_preview_row(&truncate_with_ellipsis(&msg, body_width))?;
            rows += 1;
        }
    }
    // 出力した物理行数だけプロンプト再描画の起点をずらす
    for _ in 0..rows {
        ed.note_newline();
    }

    let accepted = loop {
        match event::read()? {
            Event::Key(key) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Enter => break true,
                    KeyCode::Esc => break false,
                    KeyCode::Char('c') if ctrl => break false,
                    _ => {} // それ以外のキーは無視して回答を待つ
                }
            }
            // 確認中に届いた別のペーストは捨てる (連続ペーストで暴発させない)
            Event::Paste(_) => {}
            _ => {}
        }
    };

    // カーソル行 (最終プレビュー行) を消して行頭へ戻す。上の行は続く再描画が
    // note_newline() 分だけ遡って消すので、ここでは触らない (ピッカーと同じ流儀)。
    execute!(
        stdout(),
        cursor::MoveToColumn(0),
        Clear(ClearType::FromCursorDown)
    )?;
    Ok(accepted)
}

// ─── abbr 展開 ────────────────────────────────────────────────────────────────

/// バッファがちょうど 1 トークン (= abbr キー) のとき展開してバッファを書き換える。
/// Space 押下時に呼ぶ (fish と同じ視覚展開)。
fn try_expand_abbr(ed: &mut LineEditor, abbrs: &HashMap<String, String>) {
    // カーソルが行末でなければ展開しない
    if ed.cursor() != ed.line().len() {
        return;
    }
    let trimmed = ed.line().trim();
    // 空か、すでに複数トークンなら展開しない
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return;
    }
    if let Some(expansion) = abbrs.get(trimmed) {
        ed.set(expansion.clone());
    }
}

// ─── RC ファイル ──────────────────────────────────────────────────────────────

fn load_rc(ctx: &mut ShellContext) {
    let path = history::expand_tilde(RC_FILE);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Err(e) = execute_command(line, ctx, false) {
            eprintln!("rc: {}: {}", line, e);
        }
    }
}

// ─── ユーティリティ ───────────────────────────────────────────────────────────

/// カーソルが行末のときだけゴーストテキストを計算する。
fn compute_ghost(ed: &LineEditor, history: &History) -> Option<String> {
    if ed.cursor() != ed.line().len() {
        return None;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    completion::get_ghost(ed.line(), &cwd, history)
}

/// プロンプトに出すコミットハッシュの文字数 (git の短縮ハッシュに合わせる)
const GIT_HASH_LEN: usize = 7;

/// `.git` の実体ディレクトリを cwd から親へ辿って探す。
fn find_git_dir() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let mut dir = cwd.as_path();
    loop {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        } else if dot_git.is_file() {
            // git worktree: ".git" ファイルに "gitdir: <path>" が入っている
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir = content.trim().strip_prefix("gitdir: ")?;
            return Some(if gitdir.starts_with('/') {
                PathBuf::from(gitdir)
            } else {
                dir.join(gitdir)
            });
        }
        dir = dir.parent()?;
    }
}

/// refs/ と packed-refs を持つディレクトリ。worktree では commondir が本体を指す。
fn git_common_dir(git_dir: &Path) -> PathBuf {
    let Ok(content) = std::fs::read_to_string(git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    let common = content.trim();
    if common.starts_with('/') {
        PathBuf::from(common)
    } else {
        git_dir.join(common)
    }
}

/// `refs/heads/main` のような ref 名をコミットハッシュへ解決する。
/// 疎ファイル → packed-refs の順に探し、symref は数段だけ辿る。
fn resolve_ref(common_dir: &Path, ref_name: &str) -> Option<String> {
    let mut name = ref_name.to_string();
    for _ in 0..4 {
        if let Ok(content) = std::fs::read_to_string(common_dir.join(&name)) {
            let value = content.trim();
            match value.strip_prefix("ref: ") {
                Some(target) => {
                    name = target.to_string();
                    continue;
                }
                None => return Some(value.to_string()),
            }
        }
        if let Ok(packed) = std::fs::read_to_string(common_dir.join("packed-refs")) {
            for line in packed.lines() {
                // "#" はヘッダ、"^" は annotated tag の指す先 (ここでは不要)
                if line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                let Some((hash, packed_name)) = line.split_once(' ') else {
                    continue;
                };
                if packed_name == name {
                    return Some(hash.to_string());
                }
            }
        }
        return None;
    }
    None
}

/// プロンプトのヘッダに出す git の状態 (例: `main a1b2c3d` / `detached a1b2c3d`)。
fn fetch_git_info() -> Option<String> {
    let git_dir = find_git_dir()?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let common_dir = git_common_dir(&git_dir);

    // ブランチ上なら "ref: refs/heads/<name>"、detached HEAD なら生のハッシュ
    let (label, hash) = match head.strip_prefix("ref: ") {
        Some(ref_name) => (
            ref_name.strip_prefix("refs/heads/")?.to_string(),
            resolve_ref(&common_dir, ref_name),
        ),
        None => ("detached".to_string(), Some(head.to_string())),
    };

    // 生成直後などコミットが 1 つも無いリポジトリではハッシュを出せない
    match hash {
        Some(hash) if hash.len() >= GIT_HASH_LEN => {
            Some(format!("{} {}", label, &hash[..GIT_HASH_LEN]))
        }
        _ => Some(label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_with_ellipsis_fits_width() {
        // 収まるならそのまま
        assert_eq!(truncate_with_ellipsis("abc", 5), "abc");
        assert_eq!(truncate_with_ellipsis("abcde", 5), "abcde");
        // 溢れたら … 込みで max_cols に収める
        assert_eq!(truncate_with_ellipsis("abcdef", 5), "abcd…");
        assert_eq!(truncate_with_ellipsis("abcdef", 5).width(), 5);
        // 全角は幅 2 として数える
        assert_eq!(truncate_with_ellipsis("あいう", 4), "あ…");
        assert!(truncate_with_ellipsis("あいう", 4).width() <= 4);
    }
}
