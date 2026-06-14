mod builtin;
mod completion;
mod editor;
mod events;
mod exec;
mod term;

use crossterm::{
    event::{self, Event},
    execute,
    style::Print,
};
use std::io::{self, stdout};

use completion::{CompletionSource, StubCompletion, run_completion_menu};
use editor::{LineEditor, redraw_prompt};
use events::{ShellEvent, handle_key};
use exec::execute_command;
use term::{RawModeGuard, setup_sigint_handler};

struct Shell {
    ed: LineEditor,
    completion: Box<dyn CompletionSource>,
    git_branch: Option<String>,
}

impl Shell {
    fn new() -> Self {
        Self {
            ed: LineEditor::default(),
            completion: Box::new(StubCompletion),
            git_branch: fetch_git_branch(),
        }
    }

    fn redraw(&mut self) -> io::Result<()> {
        redraw_prompt(&mut self.ed, self.git_branch.as_deref())
    }
}

fn main() -> io::Result<()> {
    setup_sigint_handler()?;
    let _guard = RawModeGuard::new()?;
    let result = run();
    let _ = execute!(stdout(), Print("\r\n"));
    result
}

fn run() -> io::Result<()> {
    let mut shell = Shell::new();
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
                        execute_command(&cmd)?;
                        // cd などで作業ディレクトリが変わるためブランチを再取得
                        shell.git_branch = fetch_git_branch();
                        pending.push(ShellEvent::RedrawPrompt);
                    }
                    ShellEvent::ShowCompletion => {
                        let cands = shell
                            .completion
                            .complete(shell.ed.line(), shell.ed.cursor());
                        let lines_above = shell.ed.lines_above_cursor();
                        if let Some(choice) = run_completion_menu(&cands, lines_above)? {
                            shell.ed.set(choice);
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
