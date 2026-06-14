mod builtin;
mod completion;
mod editor;
mod events;
mod exec;
mod history;
mod term;

use crossterm::{
    event::{self, Event},
    execute,
    style::Print,
};
use std::io::{self, stdout};

use builtin::ShellContext;
use completion::{MenuOutcome, run_completion_menu};
use editor::{LineEditor, redraw_prompt};
use events::{ShellEvent, handle_key};
use exec::execute_command;
use history::History;
use term::{RawModeGuard, setup_sigint_handler};

/// RC ファイルパス (起動時に読み込む設定ファイル)
const RC_FILE: &str = "~/.my_shell_rc";

// ─── Shell ────────────────────────────────────────────────────────────────────

struct Shell {
    ed: LineEditor,
    history: History,
    git_branch: Option<String>,
    ctx: ShellContext,
}

impl Shell {
    fn new() -> Self {
        Self {
            ed: LineEditor::default(),
            history: History::load(),
            git_branch: fetch_git_branch(),
            ctx: ShellContext::default(),
        }
    }

    fn redraw(&mut self) -> io::Result<()> {
        redraw_prompt(&mut self.ed, self.git_branch.as_deref())
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
                        execute!(stdout(), Print("^C\r\n"))?;
                        pending.push(ShellEvent::RedrawPrompt);
                    }

                    ShellEvent::RedrawPrompt => shell.redraw()?,

                    ShellEvent::ExecuteCommand(cmd) => {
                        // cwd は実行前に記録する (cd 後に変わるため)
                        let cwd = std::env::current_dir().unwrap_or_default();
                        execute_command(&cmd, &mut shell.ctx)?;
                        shell.history.add(&cmd, &cwd);
                        shell.git_branch = fetch_git_branch();
                        pending.push(ShellEvent::RedrawPrompt);
                    }

                    ShellEvent::ShowCompletion => {
                        let prefix = shell.ed.line()[..shell.ed.cursor()].to_string();
                        let cwd = std::env::current_dir().unwrap_or_default();
                        let cands = shell.history.search_completions(&prefix, &cwd);

                        let lines_above = shell.ed.lines_above_cursor();
                        match run_completion_menu(&cands, lines_above)? {
                            MenuOutcome::Selected(choice) => shell.ed.set(choice),
                            MenuOutcome::Aborted => {
                                shell.ed.take();
                            }
                            MenuOutcome::Dismissed => {}
                        }
                        shell.ed.reset_cursor_tracking();
                        pending.push(ShellEvent::RedrawPrompt);
                    }
                }
            }
        }
    }

    Ok(())
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
        if let Err(e) = execute_command(line, ctx) {
            // RC ファイルのエラーは警告表示して続行する
            eprintln!("rc: {}: {}", line, e);
        }
    }
}

// ─── ユーティリティ ───────────────────────────────────────────────────────────

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
