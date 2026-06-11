use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal,
};
use std::io::{self, Write};

fn main() -> io::Result<()> {
    terminal::enable_raw_mode()?;

    let result = run();

    terminal::disable_raw_mode()?;
    result
}

fn run() -> io::Result<()> {
    let mut stdout = io::stdout();

    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };

        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => break,
            (KeyCode::Enter, _) => {
                // raw mode では \r\n を明示する必要がある
                print!("\r\n");
                stdout.flush()?;
            }
            (KeyCode::Char(c), _) => {
                print!("{c}");
                stdout.flush()?;
            }
            _ => {}
        }
    }

    Ok(())
}
