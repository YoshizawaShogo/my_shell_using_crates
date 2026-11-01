//! プロンプト文字列の組み立て。
//!
//! 既定はヘッダ行 (`user@host cwd (git)`) と、入力行の前に付く `$ ` の 2 行。
//! 環境変数 `MY_SHELL_PS1` (無ければ `PS1`) が設定されていれば、bash の `${PS1@P}` で
//! 展開した文字列を使う。コマンドの実行を bash の書き方に合わせているのと同じく、PS1 も
//! bash の書式 (`\u` `\w` `\e[..m` `$(...)` など) がそのまま使える。
//!
//! 展開はコマンドを実行するたびに 1 回だけ行い、キー入力ごとの再描画では使い回す
//! (再描画のたびに bash を起動すると入力が重くなるため)。

use crossterm::style::{Color, Stylize};
use std::process::{Command, Stdio};
use unicode_width::UnicodeWidthChar;

// 配色は iceberg (dark) パレットに合わせた落ち着いたトーン。
const COLOR_USER_HOST: Color = Color::Rgb {
    r: 0x84,
    g: 0xa0,
    b: 0xc6,
}; // blue
const COLOR_CWD: Color = Color::Rgb {
    r: 0x89,
    g: 0xb8,
    b: 0xc2,
}; // cyan
const COLOR_BRANCH: Color = Color::Rgb {
    r: 0xa0,
    g: 0x93,
    b: 0xc7,
}; // purple

/// 描画するプロンプト。最後の行 (`prefix`) が入力行の前に付き、それより上が `header`。
/// どちらも色などのエスケープシーケンスを含んでよい (表示幅は [`visible_width`] で数える)。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prompt {
    pub header: Vec<String>,
    pub prefix: String,
}

impl Prompt {
    /// 改行区切りの文字列から作る。readline が `\[` `\]` の目印に使う `\x01` `\x02` が
    /// 混ざっていたら取り除く (表示幅はエスケープシーケンスを読んで数えるので不要)。
    pub fn from_text(text: &str) -> Self {
        let cleaned: String = text
            .chars()
            .filter(|&c| c != '\x01' && c != '\x02')
            .collect();
        let mut lines: Vec<String> = cleaned
            .split('\n')
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect();
        let prefix = lines.pop().unwrap_or_default();
        Prompt {
            header: lines,
            prefix,
        }
    }
}

/// 既定のプロンプト: `user@host cwd (branch hash)` のヘッダ行と `$ `。
pub fn default_prompt(git_info: Option<&str>) -> Prompt {
    let user = std::env::var("USER").unwrap_or_else(|_| "?".to_string());
    let host = crate::editor::hostname();
    let cwd = crate::editor::full_cwd();
    let mut header = format!(
        "{} {}",
        format!("{}@{}", user, host).with(COLOR_USER_HOST),
        cwd.with(COLOR_CWD)
    );
    if let Some(info) = git_info {
        header.push_str(&format!(" ({})", info).with(COLOR_BRANCH).to_string());
    }
    Prompt {
        header: vec![header],
        prefix: "$ ".to_string(),
    }
}

/// カスタムプロンプトのテンプレート。`MY_SHELL_PS1` を優先し、無ければ `PS1`。
/// どちらも未設定 (または空) なら `None` (既定のプロンプトを使う)。
fn custom_template() -> Option<String> {
    ["MY_SHELL_PS1", "PS1"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
}

/// テンプレートを bash の `${var@P}` (PS1 と同じ展開) で展開する。`$?` が直前の終了
/// ステータスになるよう `(exit N)` を先に実行する。bash が無い・古い (4.4 未満) などで
/// 失敗したら `None`。
fn expand_with_bash(template: &str, last_status: i32) -> Option<String> {
    let out = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "(exit {}); printf '%s' \"${{__MY_SHELL_PROMPT@P}}\"",
            last_status
        ))
        .env("__MY_SHELL_PROMPT", template)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 次に表示するプロンプトを作る。カスタムプロンプトの展開に失敗したら既定に戻す。
pub fn build(git_info: Option<&str>, last_status: i32) -> Prompt {
    custom_template()
        .and_then(|t| expand_with_bash(&t, last_status))
        .map(|s| Prompt::from_text(&s))
        .unwrap_or_else(|| default_prompt(git_info))
}

/// 端末上の表示幅。エスケープシーケンス (CSI `\e[...` / OSC `\e]...`) と制御文字は
/// 幅 0 として数える。
pub fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                // CSI: 終端文字 (0x40..=0x7e) まで
                Some('[') => {
                    for d in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&d) {
                            break;
                        }
                    }
                }
                // OSC (端末タイトル等): BEL か ST (`ESC \`) まで
                Some(']') => {
                    while let Some(d) = chars.next() {
                        if d == '\x07' {
                            break;
                        }
                        if d == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                // その他の 2 文字シーケンス
                _ => {}
            }
            continue;
        }
        if !c.is_control() {
            width += c.width().unwrap_or(0);
        }
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_skips_escapes() {
        assert_eq!(visible_width("\x1b[32mab\x1b[0m"), 2);
        assert_eq!(visible_width("\x1b[38;2;1;2;3mx"), 1);
        assert_eq!(visible_width("\x1b]0;title\x07$ "), 2); // OSC (BEL 終端)
        assert_eq!(visible_width("\x1b]0;title\x1b\\$ "), 2); // OSC (ST 終端)
        assert_eq!(visible_width("あい"), 4);
        assert_eq!(visible_width(""), 0);
    }

    #[test]
    fn from_text_splits_header_and_prefix() {
        let p = Prompt::from_text("a\nb\n$ ");
        assert_eq!(p.header, vec!["a", "b"]);
        assert_eq!(p.prefix, "$ ");
        // 1 行だけなら全体が入力行の前置き
        let p = Prompt::from_text("\x01\x1b[1m\x02user$ ");
        assert!(p.header.is_empty());
        assert_eq!(p.prefix, "\x1b[1muser$ ");
    }

    #[test]
    fn bash_expands_prompt_escapes_and_status() {
        // `$?` は直前の終了ステータス、`\n` は改行。`\[ \]` は取り除かれる (表示幅は
        // エスケープシーケンスから数えるので、付け忘れても崩れない)。
        assert_eq!(expand_with_bash("[$?]", 3).as_deref(), Some("[3]"));
        assert_eq!(expand_with_bash("a\\nb", 0).as_deref(), Some("a\nb"));
        assert_eq!(
            expand_with_bash("\\[\\e[1m\\]x", 0).as_deref(),
            Some("\x1b[1mx")
        );
    }
}
