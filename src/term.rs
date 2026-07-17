//! 端末モードの管理。

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::style::Print;
use crossterm::{cursor::SetCursorStyle, execute, terminal};
use std::io::{self, stdout};

/// 端末の raw mode を確実に後始末する RAII ガード。
///
/// 正常終了・パニックいずれでも `Drop` で raw mode を戻す。
/// (`exit()` やシグナルで殺された場合は `Drop` が走らないため、
///  そのケースは [`setup_signal_handlers`] 側で別途救済する)
pub struct RawModeGuard;

impl RawModeGuard {
    pub fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        // ブラケットペーストを有効化する。これで貼り付けは 1 つの Event::Paste として
        // 届き、中の改行が Enter (= 即実行) にならず、レビュー前の誤爆を防げる。
        // 入力カーソルを I 型 (縦棒) にする。いずれも Drop で元へ戻す。
        let _ = execute!(stdout(), EnableBracketedPaste, SetCursorStyle::SteadyBar);
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            SetCursorStyle::DefaultUserShape,
            DisableBracketedPaste
        );
        let _ = terminal::disable_raw_mode();
    }
}

// ─── カレントディレクトリの通知 ───────────────────────────────────────────────

/// カレントディレクトリを端末へ通知する。cd の成功時と起動時に呼ぶ。
///
/// - OSC 7 (`file://host/path`): 端末に cwd を教える。新しいタブや分割を同じ
///   ディレクトリで開く、といった用途に使われる。パスは URI なので要エンコード。
/// - OSC 2 (ウィンドウタイトル): 末尾のディレクトリ名だけを出す。フルパスは長すぎ、
///   どこまで縮めるかの閾値は端末幅やタブ数に依存して決め打ちできないため。
pub fn notify_cwd() {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let path = cwd.to_string_lossy();

    // "/" には file_name が無いのでパスそのものを使う。
    let title = match cwd.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.clone().into_owned(),
    };
    // BEL を含む名前でタイトルが途中で終わらないよう制御文字を落とす。
    let title: String = title.chars().filter(|c| !c.is_control()).collect();

    let _ = execute!(
        stdout(),
        Print(format!(
            "\x1b]7;file://{}{}\x1b\\",
            crate::editor::hostname(),
            percent_encode(&path)
        )),
        Print(format!("\x1b]2;{}\x07", title)),
    );
}

/// OSC 7 の URI 用エンコード。非予約文字 (RFC 3986) と `/` 以外を `%XX` にする。
fn percent_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 対話シェル本体が無視すべきシグナルを設定する。
///
/// - `SIGINT`: raw mode 中の Ctrl+C はキー入力バイトとして読まれ `CancelInput` になるため、
///   シグナルとしては不要。外部コマンド実行中の Ctrl+C で本体が死ぬのも防ぐ。
/// - `SIGTSTP`: Ctrl+Z でシェル自身が止まらないように (停止すべきは子ジョブだけ)。
/// - `SIGTTOU` / `SIGTTIN`: `tcsetpgrp` など端末操作で本体が止められないように。
///
/// 子プロセスはこれらを exec 前に既定動作へ戻すので、停止・割り込みは子だけに効く。
pub fn setup_signal_handlers() -> io::Result<()> {
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTSTP, libc::SIGTTOU, libc::SIGTTIN] {
            libc::signal(sig, libc::SIG_IGN);
        }
    }
    Ok(())
}
