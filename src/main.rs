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
}

impl Shell {
    fn new() -> Self {
        Self {
            ed: LineEditor::default(),
            completion: Box::new(StubCompletion),
        }
    }
}

fn main() -> io::Result<()> {
    setup_sigint_handler()?;
    let _guard = RawModeGuard::new()?;
    let result = run();
    let _ = execute!(stdout(), Print("\r\n"));
    result
    // _guard の Drop で disable_raw_mode
}

fn run() -> io::Result<()> {
    let mut shell = Shell::new();
    redraw_prompt(&mut shell.ed)?;

    'main: loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };

        let mut pending = handle_key(&mut shell.ed, key);

        // 1 つのイベントが新たなイベントを生むため、空になるまで処理する
        while !pending.is_empty() {
            for ev in std::mem::take(&mut pending) {
                match ev {
                    ShellEvent::Exit => break 'main,
                    ShellEvent::CancelInput => {
                        shell.ed.take();
                        execute!(stdout(), Print("^C\r\n"))?;
                        pending.push(ShellEvent::RedrawPrompt);
                    }
                    ShellEvent::RedrawPrompt => redraw_prompt(&mut shell.ed)?,
                    ShellEvent::ExecuteCommand(cmd) => {
                        execute_command(&cmd)?;
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
                        // メニュー終了後、カーソルはプロンプト先頭行・列0にいる
                        shell.ed.reset_cursor_tracking();
                        pending.push(ShellEvent::RedrawPrompt);
                    }
                }
            }
        }
    }

    Ok(())
}
