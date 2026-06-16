//! コマンド実行。

use crate::builtin::{ShellContext, find_builtin};
use crossterm::{execute, style::Print, terminal};
use std::io::{self, stdout};
use std::os::unix::process::CommandExt;

/// `interactive`: true のとき実行前に `\r\n` を出力してプロンプトと出力を分離する。
/// load_rc からは false で呼ぶ (起動時に空行が大量発生するのを防ぐ)。
///
/// 外部コマンドはトークン分割せず、先頭トークンの abbr/alias 展開だけ行って
/// 残りはそのまま `sh -c` に渡す。これによりパイプ・リダイレクト・グロブ・
/// `$VAR`・クォートはすべて `sh` がネイティブに解釈する。
pub fn execute_command(cmd: &str, ctx: &mut ShellContext, interactive: bool) -> io::Result<()> {
    if interactive {
        execute!(stdout(), Print("\r\n"))?;
    }

    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    // 先頭トークンに対して abbr → alias を 1 段だけ展開する (ネスト展開なし)。
    let (first, rest) = split_first_word(trimmed);
    let line: String = match ctx.abbrs.get(first).or_else(|| ctx.aliases.get(first)) {
        Some(expansion) if rest.is_empty() => expansion.clone(),
        Some(expansion) => format!("{} {}", expansion, rest),
        None => trimmed.to_string(),
    };

    // 展開後の先頭トークンでビルトインかどうかを判定する。
    let (name, args_str) = split_first_word(&line);

    // ビルトインは sh を経由しないので、引数の ~ / $VAR はここで自前展開する。
    if let Some(builtin) = find_builtin(name) {
        let args = match shell_words::split(args_str) {
            Ok(parts) => parts,
            Err(e) => {
                execute!(stdout(), Print(format!("parse error: {}\r\n", e)))?;
                return Ok(());
            }
        };
        let expanded: Vec<String> = args
            .iter()
            .map(|a| expand_arg(a, ctx.last_status))
            .collect();
        let str_args: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();
        let status = builtin(&str_args, ctx);
        ctx.last_status = if status.is_ok() { 0 } else { 1 };
        if let Err(e) = status {
            execute!(stdout(), Print(format!("{}: {}\r\n", name, e)))?;
        }
        return Ok(());
    }

    // 外部コマンドは行をそのまま sh に渡す。先頭の `(exit N)` で直前コマンドの
    // 終了ステータスを $? に引き継ぐ (sh -c は毎回新しいシェルなので明示が必要)。
    let sh_cmd = format!("(exit {}); {}", ctx.last_status, line);
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

/// 先頭の単語と、それに続く残り (前後の空白を除く) に分割する。
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

/// 単語先頭の `~` と `$VAR` / `${VAR}` / `$$` / `$?` を展開する。
fn expand_arg(s: &str, last_status: i32) -> String {
    // ~ 展開: 単語が `~` だけ、または `~/` で始まる場合のみ (規則は expand_tilde に集約)
    let s: String = if s == "~" || s.starts_with("~/") {
        crate::history::expand_tilde(s)
            .to_string_lossy()
            .into_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_first() {
        assert_eq!(split_first_word("git status"), ("git", "status"));
        assert_eq!(split_first_word("  ls  -la  x"), ("ls", "-la  x"));
        assert_eq!(split_first_word("solo"), ("solo", ""));
        assert_eq!(split_first_word(""), ("", ""));
    }

    #[test]
    fn expand_status_and_literal() {
        assert_eq!(expand_arg("$?", 7), "7");
        assert_eq!(expand_arg("code=$?", 0), "code=0");
        assert_eq!(expand_arg("plain", 0), "plain");
        assert_eq!(expand_arg("$", 0), "$"); // 裸の $ はそのまま
    }
}
