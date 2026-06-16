//! コマンド実行。

use crate::builtin::{ShellContext, find_builtin};
use crossterm::{execute, style::Print, terminal};
use std::collections::HashMap;
use std::io::{self, stdout};
use std::os::unix::process::CommandExt;

/// `interactive`: true のとき実行前に `\r\n` を出力してプロンプトと出力を分離する。
/// load_rc からは false で呼ぶ (起動時に空行が大量発生するのを防ぐ)。
///
/// 外部コマンドはトークン分割せず、先頭トークンの abbr 展開だけ行って残りは
/// そのまま `sh -c` に渡す。alias はすべて `sh` の `alias` 定義として前置し、
/// パイプ後 (`ls | grep`) を含むあらゆるコマンド位置で展開させる。
/// これによりパイプ・リダイレクト・グロブ・`$VAR`・クォートも `sh` が解釈する。
pub fn execute_command(cmd: &str, ctx: &mut ShellContext, interactive: bool) -> io::Result<()> {
    if interactive {
        execute!(stdout(), Print("\r\n"))?;
    }

    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    // 先頭トークンの abbr を 1 段だけ展開する (abbr は先頭のみ。alias は sh に委ねる)。
    let (first, rest) = split_first_word(trimmed);
    let abbr_line: String = match ctx.abbrs.get(first) {
        Some(expansion) if rest.is_empty() => expansion.clone(),
        Some(expansion) => format!("{} {}", expansion, rest),
        None => trimmed.to_string(),
    };

    // ビルトイン判定用に alias も 1 段展開してコマンド名を求める
    // (alias → ビルトインのケースを sh へ流さず本体で実行するため)。
    let (name0, rest0) = split_first_word(&abbr_line);
    let detect_line: String = match ctx.aliases.get(name0) {
        Some(expansion) if rest0.is_empty() => expansion.clone(),
        Some(expansion) => format!("{} {}", expansion, rest0),
        None => abbr_line.clone(),
    };
    let (name, args_str) = split_first_word(&detect_line);

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

    // 外部コマンド: alias 定義を前置し、行 (abbr 展開済み) はそのまま sh に渡す。
    // alias 展開はパイプ後を含めて sh に任せる。先頭の `(exit N)` で直前コマンドの
    // 終了ステータスを $? に引き継ぐ (sh -c は毎回新しいシェルなので明示が必要)。
    let prelude = build_alias_prelude(&ctx.aliases);
    let sh_cmd = format!("{}(exit {}); {}", prelude, ctx.last_status, abbr_line);
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

/// 全 alias を sh の `alias` 定義に変換した前文を作る。
///
/// この前文を外部コマンドの前に置くと、sh がコマンド位置 (行頭・`|`/`;`/`&&` の
/// 直後など) ごとに alias を展開する。各定義を別行に置くのは「alias は次の行から
/// 有効」という sh の規則のため。`shopt` 行は bash が `/bin/sh` の場合に展開を
/// 有効化する保険で、dash では未知コマンドとして無害に失敗する。
fn build_alias_prelude(aliases: &HashMap<String, String>) -> String {
    let mut out = String::from("shopt -s expand_aliases 2>/dev/null\n");
    for (name, value) in aliases {
        if !is_valid_alias_name(name) {
            continue;
        }
        out.push_str("alias ");
        out.push_str(name);
        out.push('=');
        out.push_str(&sh_single_quote(value));
        out.push('\n');
    }
    out
}

/// sh の alias 名として安全か (英数字・`_`・`-` のみ)。それ以外は前文から除外する。
fn is_valid_alias_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 文字列を sh のシングルクォート文字列リテラルに変換する。
fn sh_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''"); // 閉じ → エスケープした ' → 開き
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
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

    #[test]
    fn single_quote_escapes() {
        assert_eq!(sh_single_quote("ls --color=auto"), "'ls --color=auto'");
        // 埋め込み ' は「閉じ → \' → 開き」になる
        assert_eq!(sh_single_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn valid_alias_names() {
        assert!(is_valid_alias_name("grep"));
        assert!(is_valid_alias_name("ll"));
        assert!(is_valid_alias_name("git-log"));
        assert!(!is_valid_alias_name("")); // 空は不可
        assert!(!is_valid_alias_name("..")); // abbr 的な名前は sh alias 不可
        assert!(!is_valid_alias_name("a b")); // 空白を含む
    }

    #[test]
    fn prelude_contains_defs() {
        let mut aliases = HashMap::new();
        aliases.insert("grep".to_string(), "grep --color=auto".to_string());
        let prelude = build_alias_prelude(&aliases);
        assert!(prelude.contains("alias grep='grep --color=auto'\n"));
        assert!(prelude.starts_with("shopt -s expand_aliases"));
    }
}
