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
    event::{self, Event},
    execute,
    style::Print,
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
}

// ─── エントリポイント ─────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
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

    'main: loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };

        let mut pending = handle_key(&mut shell.ed, key);

        while !pending.is_empty() {
            for ev in std::mem::take(&mut pending) {
                match ev {
                    ShellEvent::Exit => break 'main,

                    ShellEvent::CancelInput => {
                        shell.ed.take();
                        shell.ghost = None;
                        shell.hist_idx = None;
                        shell.saved_input.clear();
                        execute!(stdout(), Print("^C\r\n"))?;
                        pending.push(ShellEvent::RedrawPrompt);
                    }

                    ShellEvent::RedrawPrompt => {
                        shell.ghost = compute_ghost(&shell.ed, &shell.history);
                        shell.redraw()?;
                    }

                    ShellEvent::ExecuteCommand => {
                        // 1. abbr を視覚展開してカーソルを行末へ
                        shell.ed.move_end();
                        try_expand_abbr(&mut shell.ed, &shell.ctx.abbrs);
                        // 2. ゴーストを消してクリーンな表示にしてから改行
                        shell.ghost = None;
                        shell.redraw()?;
                        // 3. バッファを取得してエディタをクリア
                        let cmd = shell.ed.take();
                        // 4. 実行 (cwd は cd 前に記録)
                        let cwd = std::env::current_dir().unwrap_or_default();
                        execute_command(&cmd, &mut shell.ctx, true)?;
                        if !cmd.trim().is_empty() {
                            shell.history.add(&cmd, &cwd);
                        }
                        shell.git_branch = fetch_git_branch();
                        shell.ghost = None;
                        shell.hist_idx = None;
                        shell.saved_input.clear();
                        pending.push(ShellEvent::RedrawPrompt);
                    }

                    ShellEvent::AcceptGhost => {
                        if let Some(ghost) = shell.ghost.take() {
                            let new_buf = shell.ed.line().to_string() + &ghost;
                            shell.ed.set(new_buf);
                        }
                        pending.push(ShellEvent::RedrawPrompt);
                    }

                    ShellEvent::ShowCompletion => {
                        let prefix = shell.ed.line()[..shell.ed.cursor()].to_string();
                        let cwd = std::env::current_dir().unwrap_or_default();
                        let lines_above = shell.ed.lines_above_cursor();
                        let tab_ctx = TabContext {
                            prefix: &prefix,
                            cwd: &cwd,
                            history: &shell.history,
                            lines_above_cursor: lines_above,
                        };
                        match completion::tab_complete(tab_ctx)? {
                            Selection::Chosen(choice) => shell.ed.set(choice),
                            Selection::Aborted => {
                                shell.ed.take();
                            }
                            Selection::Dismissed => {}
                            Selection::InsertChar(c) => shell.ed.insert(c),
                            Selection::Backspace => shell.ed.backspace(),
                        }
                        // reset_cursor_tracking は呼ばない:
                        // グリッドメニューは MoveUp(1) で入力行に戻るので
                        // lines_above_cursor をそのまま使って redraw できる。
                        // 候補なし (Dismissed) のときは cursor が動いていないため特に重要。
                        pending.push(ShellEvent::RedrawPrompt);
                    }

                    ShellEvent::HistoryPrev => {
                        let n = shell.history.len();
                        if n == 0 {
                            continue;
                        }
                        let new_idx = match shell.hist_idx {
                            None => {
                                shell.saved_input = shell.ed.line().to_string();
                                n - 1
                            }
                            Some(0) => 0, // 最古のエントリ、それ以上戻れない
                            Some(i) => i - 1,
                        };
                        shell.hist_idx = Some(new_idx);
                        if let Some(cmd) = shell.history.get_cmd(new_idx) {
                            shell.ed.set(cmd.to_string());
                        }
                        pending.push(ShellEvent::RedrawPrompt);
                    }

                    ShellEvent::HistoryNext => {
                        match shell.hist_idx {
                            None => {} // ナビゲーション外では無視
                            Some(i) => {
                                let n = shell.history.len();
                                if i + 1 >= n {
                                    // 最新エントリを超えたら保存済み入力を復元
                                    shell.hist_idx = None;
                                    let saved = std::mem::take(&mut shell.saved_input);
                                    shell.ed.set(saved);
                                } else {
                                    shell.hist_idx = Some(i + 1);
                                    if let Some(cmd) = shell.history.get_cmd(i + 1) {
                                        shell.ed.set(cmd.to_string());
                                    }
                                }
                            }
                        }
                        pending.push(ShellEvent::RedrawPrompt);
                    }

                    ShellEvent::ShowHistoryFzf => {
                        let query = shell.ed.line().to_string();
                        let cwd = std::env::current_dir().unwrap_or_default();
                        execute!(stdout(), Print("\r\n"))?;
                        shell.ed.note_newline();
                        match completion::fzf_history(&query, &cwd, &shell.history) {
                            Ok(Some(s)) => shell.ed.set(s),
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
                            let before = &shell.ed.line()[..shell.ed.cursor()];
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
                            let query = if file_part.is_empty() { None } else { Some(file_part) };
                            (root, query, Some(dir))
                        } else {
                            (cwd.clone(), None, None)
                        };
                        execute!(stdout(), Print("\r\n"))?;
                        shell.ed.note_newline();
                        match completion::fzf_files(&root, initial_query.as_deref()) {
                            Ok(Some(s)) => {
                                if let Some(ref dir) = dir_part {
                                    shell.ed.delete_before_cursor(token.len());
                                    let filename = s.strip_prefix("./").unwrap_or(&s);
                                    shell.ed.insert_str(&format!("{}{}", dir, filename));
                                } else {
                                    shell.ed.insert_str(&s);
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
                        shell.ed.note_newline();
                        match completion::fzf_reg_paths(&shell.ctx.reg_paths) {
                            Ok(Some(s)) => shell.ed.insert_str(&s),
                            Ok(None) => {}
                            Err(e) => {
                                execute!(stdout(), Print(format!("fzf: {}\r\n", e)))?;
                            }
                        }
                        pending.push(ShellEvent::RedrawPrompt);
                    }

                    ShellEvent::InsertSpace => {
                        try_expand_abbr(&mut shell.ed, &shell.ctx.abbrs);
                        shell.ed.insert(' ');
                        pending.push(ShellEvent::RedrawPrompt);
                    }
                }
            }
        }
    }

    Ok(())
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
