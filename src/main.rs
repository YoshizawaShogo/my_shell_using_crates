mod builtin;
mod completion;
mod editor;
mod events;
mod exec;
mod history;
mod provider;
mod selector;
mod term;

use crossterm::{
    cursor,
    event::{self, Event},
    execute,
    style::Print,
    terminal::{Clear, ClearType},
};
use std::io::{self, stdout};

use builtin::ShellContext;
use completion::TabContext;
use editor::{LineEditor, redraw_prompt};
use events::{ShellEvent, handle_key};
use exec::execute_command;
use history::History;
use selector::Selection;
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use term::{RawModeGuard, setup_sigint_handler};

/// RC ファイルパス (起動時に読み込む設定ファイル)
const RC_FILE: &str = "~/.my_shell_rc";

// ─── Shell ────────────────────────────────────────────────────────────────────

struct Shell {
    ed: LineEditor,
    history: History,
    git_branch: Option<String>,
    ctx: ShellContext,
    ghost: Option<String>,
    /// 履歴ナビゲーション中の位置 (None = ナビゲーション外)
    hist_idx: Option<usize>,
    /// ナビゲーション開始前の入力を保存する
    saved_input: String,
}

impl Shell {
    fn new() -> Self {
        Self {
            ed: LineEditor::default(),
            history: History::load(),
            git_branch: fetch_git_branch(),
            ctx: ShellContext::default(),
            ghost: None,
            hist_idx: None,
            saved_input: String::new(),
        }
    }

    fn redraw(&mut self) -> io::Result<()> {
        redraw_prompt(
            &mut self.ed,
            self.git_branch.as_deref(),
            self.ghost.as_deref(),
        )
    }

    /// 1 個の `ShellEvent` を処理する。
    ///
    /// 端末 I/O を伴うため戻り値は `io::Result`。終了要求なら `Ok(true)` を返す。
    /// 後続処理 (主に再描画) は `pending` に積んで呼び出し側のキューに委ねる。
    fn handle_event(&mut self, ev: ShellEvent, pending: &mut Vec<ShellEvent>) -> io::Result<bool> {
        match ev {
            ShellEvent::Exit => return Ok(true),

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
                // 3. バッファを取得してエディタをクリア
                let cmd = self.ed.take();
                // 4. 実行 (cwd は cd 前に記録)
                let cwd = std::env::current_dir().unwrap_or_default();
                execute_command(&cmd, &mut self.ctx, true)?;
                if !cmd.trim().is_empty() {
                    self.history.add(&cmd, &cwd);
                }
                self.git_branch = fetch_git_branch();
                self.ghost = None;
                self.hist_idx = None;
                self.saved_input.clear();
                pending.push(ShellEvent::RedrawPrompt);
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
                let cwd = std::env::current_dir().unwrap_or_default();
                let lines_above = self.ed.lines_above_cursor();
                let tab_ctx = TabContext {
                    prefix: &prefix,
                    cwd: &cwd,
                    history: &self.history,
                    lines_above_cursor: lines_above,
                };
                match completion::tab_complete(tab_ctx)? {
                    Selection::Chosen(choice) => self.ed.set(choice),
                    Selection::Aborted => {
                        self.ed.take();
                    }
                    Selection::Dismissed => {}
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
                let new_idx = match self.hist_idx {
                    None => {
                        self.saved_input = self.ed.line().to_string();
                        n - 1
                    }
                    Some(0) => 0, // 最古のエントリ、それ以上戻れない
                    Some(i) => i - 1,
                };
                self.hist_idx = Some(new_idx);
                if let Some(cmd) = self.history.get_cmd(new_idx) {
                    self.ed.set(cmd.to_string());
                }
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::HistoryNext => {
                match self.hist_idx {
                    None => {} // ナビゲーション外では無視
                    Some(i) => {
                        let n = self.history.len();
                        if i + 1 >= n {
                            // 最新エントリを超えたら保存済み入力を復元
                            self.hist_idx = None;
                            let saved = std::mem::take(&mut self.saved_input);
                            self.ed.set(saved);
                        } else {
                            self.hist_idx = Some(i + 1);
                            if let Some(cmd) = self.history.get_cmd(i + 1) {
                                self.ed.set(cmd.to_string());
                            }
                        }
                    }
                }
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::ShowHistoryFzf => {
                let query = self.ed.line().to_string();
                let cwd = std::env::current_dir().unwrap_or_default();
                execute!(stdout(), Print("\r\n"))?;
                self.ed.note_newline();
                match completion::fzf_history(&query, &cwd, &self.history) {
                    Ok(Some(s)) => self.ed.set(s),
                    Ok(None) => {}
                    Err(e) => {
                        execute!(stdout(), Print(format!("fzf: {}\r\n", e)))?;
                    }
                }
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::ShowFileFzf => {
                let cwd = std::env::current_dir().unwrap_or_default();
                // カーソル前のトークンを取得（owned にして借用を切る）
                let token: String = {
                    let before = &self.ed.line()[..self.ed.cursor()];
                    let token_start = before.rfind(' ').map(|i| i + 1).unwrap_or(0);
                    before[token_start..].to_owned()
                };
                // トークンに '/' が含まれる場合はそのディレクトリをルートにする
                let (root, initial_query, dir_part) = if !token.is_empty() && token.contains('/') {
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
                // 行頭のコマンドが cd のときはディレクトリのみに絞る
                let dirs_only = self
                    .ed
                    .line()
                    .split_whitespace()
                    .next()
                    .is_some_and(|cmd| cmd == "cd");
                execute!(stdout(), Print("\r\n"))?;
                self.ed.note_newline();
                match completion::fzf_files(&root, initial_query.as_deref(), dirs_only) {
                    Ok(Some(s)) => {
                        if let Some(ref dir) = dir_part {
                            self.ed.delete_before_cursor(token.len());
                            let filename = s.strip_prefix("./").unwrap_or(&s);
                            self.ed.insert_str(&format!("{}{}", dir, filename));
                        } else {
                            self.ed.insert_str(&s);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        execute!(stdout(), Print(format!("fzf: {}\r\n", e)))?;
                    }
                }
                pending.push(ShellEvent::RedrawPrompt);
            }

            ShellEvent::ShowRegPathFzf => {
                execute!(stdout(), Print("\r\n"))?;
                self.ed.note_newline();
                match completion::fzf_reg_paths(&self.ctx.reg_paths) {
                    Ok(Some(s)) => self.ed.insert_str(&s),
                    Ok(None) => {}
                    Err(e) => {
                        execute!(stdout(), Print(format!("fzf: {}\r\n", e)))?;
                    }
                }
                pending.push(ShellEvent::RedrawPrompt);
            }

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
    // skim が global_allocator として mimalloc を強制しており、mimalloc は
    // 起動時に既定 1GiB の arena を予約して仮想メモリ(VSZ)を肥大させる。
    // mimalloc は main より前に初期化されるため set_var では間に合わない。
    // そこで環境変数を設定して一度だけ自分自身を exec し直す
    // (exec なので PID は不変・プロセス数も増えない)。
    if std::env::var_os("MIMALLOC_ARENA_RESERVE").is_none() {
        unsafe {
            std::env::set_var("MIMALLOC_ARENA_RESERVE", "65536"); // KiB = 64MB
        }
        let exe = std::env::current_exe()?;
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        // exec が成功すればこのプロセスイメージは置き換わり、以降は実行されない。
        return Err(std::process::Command::new(exe).args(args).exec());
    }

    // glibc の malloc arena を1個に固定し、skim 使用による VSZ 肥大を防ぐ。
    // このシェルは実質シングルスレッドなので arena 分割の利点が無く、
    // 短命スレッド (skim) が arena を量産して仮想メモリが膨らむのを抑える。
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
    setup_sigint_handler()?;
    let _guard = RawModeGuard::new()?;
    let result = run();
    let _ = execute!(stdout(), Print("\r\n"));
    result
    // _guard の Drop で disable_raw_mode
    // shell.history の Drop で HISTORY_FILE へ保存
}

fn run() -> io::Result<()> {
    let mut shell = Shell::new();

    // RC ファイルを読み込んで各行を実行する
    load_rc(&mut shell.ctx);

    shell.ghost = compute_ghost(&shell.ed, &shell.history);
    shell.redraw()?;

    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };

        let mut pending = handle_key(&mut shell.ed, key);

        // イベント処理が新たなイベント (主に再描画) を生むため、空になるまで回す。
        while !pending.is_empty() {
            for ev in std::mem::take(&mut pending) {
                if shell.handle_event(ev, &mut pending)? {
                    return Ok(());
                }
            }
        }
    }
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

fn fetch_git_branch() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD")
}
