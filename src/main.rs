use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{self, Clear, ClearType},
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use signal_hook::{consts::SIGINT, iterator::Signals};
use std::io::{self, Write, stdout};

const PROMPT: &str = "$ ";

// ─── イベント ────────────────────────────────────────────────────────────────

enum ShellEvent {
    Exit,
    CancelInput,
    RedrawPrompt,
    ExecuteCommand(String),
    ShowCompletion,
}

// ─── 状態 ────────────────────────────────────────────────────────────────────

struct ShellContext {
    input: String,
    cursor: usize,
}

impl ShellContext {
    fn new() -> Self {
        Self {
            input: String::new(),
            cursor: 0,
        }
    }

    fn insert(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.remove(prev);
            self.cursor = prev;
        }
    }

    fn take_input(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }
}

// ─── 入力処理: キー → イベント列 ─────────────────────────────────────────────

fn handle_key(ctx: &mut ShellContext, key: KeyEvent) -> Vec<ShellEvent> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => vec![ShellEvent::Exit],
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => vec![ShellEvent::CancelInput],
        (KeyCode::Tab, _) => vec![ShellEvent::ShowCompletion],
        (KeyCode::Enter, _) => vec![ShellEvent::ExecuteCommand(ctx.take_input())],
        (KeyCode::Char(c), _) => {
            ctx.insert(c);
            vec![ShellEvent::RedrawPrompt]
        }
        (KeyCode::Backspace, _) => {
            ctx.backspace();
            vec![ShellEvent::RedrawPrompt]
        }
        _ => vec![],
    }
}

// ─── イベント実行 ─────────────────────────────────────────────────────────────

fn redraw_prompt(ctx: &ShellContext) -> io::Result<()> {
    let col = (PROMPT.len() + ctx.cursor) as u16;
    queue!(
        stdout(),
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        Print(PROMPT),
        Print(&ctx.input),
        cursor::MoveToColumn(col),
    )?;
    stdout().flush()
}

fn execute_command(cmd: &str) -> io::Result<()> {
    execute!(stdout(), Print("\r\n"))?;
    terminal::disable_raw_mode()?;
    if !cmd.trim().is_empty() {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .status()?;
    }
    terminal::enable_raw_mode()
}

fn show_completion(ctx: &mut ShellContext) -> io::Result<()> {
    // TODO: 入力を元に実際の補完候補を生成する
    let candidates: Vec<String> = vec![
        "ls".to_string(),
        "ls -la".to_string(),
        "cd".to_string(),
        "cargo build".to_string(),
        "cargo run".to_string(),
    ];
    if candidates.is_empty() {
        return Ok(());
    }

    let popup_height = (candidates.len() + 2) as u16;
    let mut selected = 0usize;

    execute!(stdout(), Print("\r\n"))?;

    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(popup_height),
        },
    )?;

    loop {
        let sel = selected;
        terminal.draw(|frame| {
            let items: Vec<ListItem> = candidates
                .iter()
                .map(|s| ListItem::new(s.as_str()))
                .collect();
            let mut state = ListState::default();
            state.select(Some(sel));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" ↑↓: 移動  Tab/Enter: 確定  Esc: キャンセル "),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::Cyan)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ");
            frame.render_stateful_widget(list, frame.area(), &mut state);
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        match key.code {
            KeyCode::Esc => break,
            KeyCode::Up => selected = selected.checked_sub(1).unwrap_or(candidates.len() - 1),
            KeyCode::Down => selected = (selected + 1) % candidates.len(),
            KeyCode::Tab | KeyCode::Enter => {
                if let Some(item) = candidates.get(selected) {
                    ctx.input = item.clone();
                    ctx.cursor = ctx.input.len();
                }
                break;
            }
            _ => {}
        }
    }

    terminal.clear()?;
    drop(terminal);
    execute!(stdout(), cursor::MoveUp(1))
    // RedrawPrompt は呼び出し元のイベントループが発行する
}

// ─── メインループ ─────────────────────────────────────────────────────────────

/// 別スレッドで SIGINT を監視し、外部からのシグナルでも raw mode を確実に解除する
fn setup_sigint_handler() -> io::Result<()> {
    let mut signals = Signals::new([SIGINT])?;
    std::thread::spawn(move || {
        for _ in signals.forever() {
            let _ = terminal::disable_raw_mode();
            std::process::exit(130); // 慣例: 128 + SIGINT(2)
        }
    });
    Ok(())
}

fn main() -> io::Result<()> {
    setup_sigint_handler()?;
    terminal::enable_raw_mode()?;
    let result = run();
    let _ = execute!(stdout(), Print("\r\n"));
    terminal::disable_raw_mode()?;
    result
}

fn run() -> io::Result<()> {
    let mut ctx = ShellContext::new();
    redraw_prompt(&ctx)?;

    'main: loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };

        let mut pending = handle_key(&mut ctx, key);

        while !pending.is_empty() {
            for ev in std::mem::take(&mut pending) {
                match ev {
                    ShellEvent::Exit => break 'main,
                    ShellEvent::CancelInput => {
                        ctx.take_input();
                        execute!(stdout(), Print("^C\r\n"))?;
                        pending.push(ShellEvent::RedrawPrompt);
                    }
                    ShellEvent::RedrawPrompt => redraw_prompt(&ctx)?,
                    ShellEvent::ExecuteCommand(cmd) => {
                        execute_command(&cmd)?;
                        pending.push(ShellEvent::RedrawPrompt);
                    }
                    ShellEvent::ShowCompletion => {
                        show_completion(&mut ctx)?;
                        pending.push(ShellEvent::RedrawPrompt);
                    }
                }
            }
        }
    }

    Ok(())
}
