//! コマンド実行。

use crate::builtin::{ShellContext, find_builtin};
use crossterm::{execute, style::Print, terminal};
use std::io::{self, stdout};

pub fn execute_command(cmd: &str, ctx: &mut ShellContext) -> io::Result<()> {
    execute!(stdout(), Print("\r\n"))?;

    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    // abbr → alias の順で先頭トークンを展開 (ネスト展開なし)
    let expanded = expand_first_token(trimmed, ctx);
    let effective = expanded.trim();

    let parts: Vec<&str> = effective.split_whitespace().collect();
    let name = parts[0];
    let args = &parts[1..];

    // 組み込みコマンドは raw mode を維持したまま実行する
    // エラーはシェルを終了させず、メッセージを表示して続行する
    if let Some(builtin) = find_builtin(name) {
        if let Err(e) = builtin.run(args, ctx) {
            execute!(stdout(), Print(format!("{}: {}\r\n", name, e)))?;
        }
        return Ok(());
    }

    // 外部コマンド: raw mode を解除して子プロセスに端末を渡す
    terminal::disable_raw_mode()?;
    std::process::Command::new("sh")
        .arg("-c")
        .arg(effective)
        .status()?;
    terminal::enable_raw_mode()
}

/// abbr → alias の順で先頭トークンを展開する。
fn expand_first_token(cmd: &str, ctx: &ShellContext) -> String {
    let (first, rest) = split_first(cmd);
    if let Some(expanded) = ctx.abbrs.get(first).or_else(|| ctx.aliases.get(first)) {
        if rest.is_empty() {
            expanded.clone()
        } else {
            format!("{} {}", expanded, rest)
        }
    } else {
        cmd.to_string()
    }
}

fn split_first(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}
