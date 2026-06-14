//! コマンド実行。

use crossterm::{execute, style::Print, terminal};
use std::io::{self, stdout};

/// raw mode を一時解除して子プロセスに端末を渡し、完了後に raw mode へ戻す。
///
/// TODO(job-control): 現状は子とシェルが同一プロセスグループのため、
///   実行中の Ctrl+C で SIGINT がシェルにも届く。setpgid/tcsetpgrp で
///   子を独立したプロセスグループに置く必要がある。
pub fn execute_command(cmd: &str) -> io::Result<()> {
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
