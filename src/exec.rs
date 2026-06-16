//! コマンド実行。

use crate::builtin::{ShellContext, find_builtin};
use crossterm::{execute, style::Print, terminal};
use std::io::{self, stdout};
use std::os::unix::process::CommandExt;

/// `interactive`: true のとき実行前に `\r\n` を出力してプロンプトと出力を分離する。
/// load_rc からは false で呼ぶ (起動時に空行が大量発生するのを防ぐ)。
pub fn execute_command(cmd: &str, ctx: &mut ShellContext, interactive: bool) -> io::Result<()> {
    if interactive {
        execute!(stdout(), Print("\r\n"))?;
    }

    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    // クォートを考慮してトークン分割する
    let mut parts: Vec<String> = match shell_words::split(trimmed) {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return Ok(()),
        Err(e) => {
            execute!(stdout(), Print(format!("parse error: {}\r\n", e)))?;
            return Ok(());
        }
    };

    // abbr → alias の順で先頭トークンを展開 (ネスト展開なし)
    let first = parts[0].clone();
    if let Some(expansion) = ctx.abbrs.get(&first).or_else(|| ctx.aliases.get(&first)) {
        let expansion = expansion.clone();
        if let Ok(mut exp_parts) = shell_words::split(&expansion) {
            let rest: Vec<String> = parts.drain(1..).collect();
            exp_parts.extend(rest);
            parts = exp_parts;
        }
    }

    if parts.is_empty() {
        return Ok(());
    }

    // ~ および $VAR を全引数に展開する (abbr 展開後に行う)
    let last_status = ctx.last_status;
    for part in parts.iter_mut() {
        *part = expand_arg(part, last_status);
    }

    let name = parts[0].as_str();
    let str_args: Vec<&str> = parts[1..].iter().map(|s| s.as_str()).collect();

    // 組み込みコマンドは raw mode を維持したまま実行する
    if let Some(builtin) = find_builtin(name) {
        let status = builtin.run(&str_args, ctx);
        ctx.last_status = if status.is_ok() { 0 } else { 1 };
        if let Err(e) = status {
            execute!(stdout(), Print(format!("{}: {}\r\n", name, e)))?;
        }
        return Ok(());
    }

    // 外部コマンド: 展開済みのトークンを shell_words::join で再クォートして sh -c へ渡す
    let sh_cmd = shell_words::join(parts.iter().map(|s| s.as_str()));
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg(&sh_cmd);
    // 親シェルは SIGINT を無視しているが、子ではデフォルト動作へ戻し、
    // Ctrl+C が実行中の外部コマンドだけに効くようにする。
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    terminal::disable_raw_mode()?;
    let status = command.status()?;
    ctx.last_status = status.code().unwrap_or(1);
    terminal::enable_raw_mode()
}

/// 単語先頭の `~` と `$VAR` / `${VAR}` / `$$` / `$?` を展開する。
fn expand_arg(s: &str, last_status: i32) -> String {
    // ~ 展開: 単語が `~` だけ、または `~/` で始まる場合のみ
    let s: String = if s == "~" {
        std::env::var("HOME").unwrap_or_else(|_| s.to_string())
    } else if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            s.to_string()
        } else {
            format!("{}/{}", home, rest)
        }
    } else {
        s.to_string()
    };

    // $VAR / ${VAR} 展開
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next(); // '{'を消費
            let name: String = chars.by_ref().take_while(|&x| x != '}').collect();
            out.push_str(&std::env::var(&name).unwrap_or_default());
        } else {
            match chars.peek() {
                Some(&'$') => {
                    chars.next();
                    out.push_str(&std::process::id().to_string());
                }
                Some(&'?') => {
                    chars.next();
                    out.push_str(&last_status.to_string());
                }
                _ => {
                    // 変数名: [a-zA-Z_][a-zA-Z0-9_]*
                    let mut name = String::new();
                    while let Some(&x) = chars.peek() {
                        if x.is_alphanumeric() || x == '_' {
                            name.push(x);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if name.is_empty() {
                        out.push('$'); // 後続が英数字でない裸の $
                    } else {
                        out.push_str(&std::env::var(&name).unwrap_or_default());
                    }
                }
            }
        }
    }
    out
}
