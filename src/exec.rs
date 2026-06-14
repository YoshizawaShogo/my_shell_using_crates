//! コマンド実行。

use crate::builtin::find_builtin;
use crossterm::{execute, style::Print, terminal};
use std::io::{self, stdout};

pub fn execute_command(cmd: &str) -> io::Result<()> {
    execute!(stdout(), Print("\r\n"))?;

    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let name = parts[0];
    let args = &parts[1..];

    // 組み込みコマンドは raw mode を維持したまま実行する
    if let Some(builtin) = find_builtin(name) {
        return builtin.run(args);
    }

    // 外部コマンド: raw mode を解除して子プロセスに端末を渡す
    //
    // TODO(job-control): 現状は子とシェルが同一プロセスグループのため、
    //   実行中の Ctrl+C で SIGINT がシェルにも届く。setpgid/tcsetpgrp で
    //   子を独立したプロセスグループに置く必要がある。
    terminal::disable_raw_mode()?;
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()?;
    terminal::enable_raw_mode()
}
