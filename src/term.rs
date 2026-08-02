//! 端末モードの管理。

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::style::Print;
use crossterm::{cursor::SetCursorStyle, execute, terminal};
use std::io::{self, stdout};
use std::sync::OnceLock;

/// 端末の raw mode を確実に後始末する RAII ガード。
///
/// 正常終了・パニックいずれでも `Drop` で raw mode を戻す。
/// (`exit()` やシグナルで殺された場合は `Drop` が走らないため、
///  そのケースは [`setup_signal_handlers`] 側で別途救済する)
pub struct RawModeGuard;

impl RawModeGuard {
    pub fn new() -> io::Result<Self> {
        // 素の (cooked) termios を raw 化より前に確保する (自前 termios 管理の基準)。
        capture_cooked();
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

// ─── 端末モード (termios) の自前管理 ─────────────────────────────────────────
//
// crossterm の raw mode は「元 termios」をキャッシュして復元する方式なので、子が
// 異常終了して端末を変な状態で残すと、その壊れた状態を次の復元基準として取り込んで
// しまう。そこで cooked/raw を自前で持ち、tcsetattr で決め打ちの termios を直接張り直す。
// これにより子が端末をどう残しても、確実に狙った状態へ収束できる。

/// 起動時に確保した「素の (cooked) termios」。raw 化より前に 1 度だけ捕まえる。
static COOKED: OnceLock<libc::termios> = OnceLock::new();

/// 現在の termios を cooked の基準として保存する。raw mode にする前に呼ぶこと。
/// 端末でない (パイプ等) 場合は保存されず、以降の [`set_raw`]/[`set_cooked`] は無効になる。
pub fn capture_cooked() {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) } == 0 {
        let _ = COOKED.set(t);
    }
}

/// 保存済み cooked termios から raw termios を作る (crossterm と同じ `cfmakeraw`)。
fn raw_termios() -> Option<libc::termios> {
    let mut t = *COOKED.get()?;
    unsafe { libc::cfmakeraw(&mut t) };
    Some(t)
}

/// 端末を raw mode へ張り直す。crossterm のキャッシュを介さず tcsetattr で直接設定する
/// ので、直前の状態が何であっても確実に raw へ収束する。
pub fn set_raw() {
    if let Some(t) = raw_termios() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) };
    }
}

/// 端末を cooked mode (起動時の素の状態) へ戻す。子コマンドへ渡す前に呼ぶ。
/// 出力を吐き切ってから切り替えるよう `TCSADRAIN` を使う。
pub fn set_cooked() {
    if let Some(t) = COOKED.get() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, t) };
    }
}

/// プロンプト入力を読む直前に端末状態を強制的に整える。
///
/// - `tcsetpgrp`: 端末のフォアグラウンドをシェルへ奪い返す (異常終了した子の孤児
///   プロセスが端末を握ったままでも取り戻す)。SIGTTOU は無視済みなのでここで
///   シェルが止められることはない。
/// - [`set_raw`]: raw mode を張り直す。子が cooked のまま残しても、これで復帰し
///   Ctrl+C がキー入力として読めるようになる (cooked のままだと Ctrl+C の SIGINT が
///   本体の `SIG_IGN` で握り潰され、`^C` がエコーされるだけで効かなくなる)。
pub fn reassert_terminal() {
    unsafe {
        libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
    }
    set_raw();
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

    let _ = execute!(
        stdout(),
        Print(format!(
            "\x1b]7;file://{}{}\x1b\\",
            crate::editor::hostname(),
            percent_encode(&path)
        )),
    );
    set_title(&cwd_title());
}

/// OSC 2 (ウィンドウタイトル) だけを設定する。
///
/// BEL を含む文字列でタイトルが途中で終わらないよう制御文字を落とす。
pub fn set_title(title: &str) {
    let title: String = title.chars().filter(|c| !c.is_control()).collect();
    let _ = execute!(stdout(), Print(format!("\x1b]2;{}\x07", title)));
}

/// 現在の cwd に対応する既定タイトル (末尾ディレクトリ名)。
fn cwd_title() -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return String::new();
    };
    // "/" には file_name が無いのでパスそのものを使う。
    match cwd.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => cwd.to_string_lossy().into_owned(),
    }
}

/// 外部コマンド実行中のタイトルを `cmd名 @ dir` にする (子プロセス起動時に呼ぶ)。
/// コマンド名は先頭トークンだけ、ディレクトリはフルパスではなく末尾の名前だけを出す
/// (どちらもフルだとタイトルが長くなりすぎるため)。
pub fn set_running_title(cmd: &str) {
    let name = cmd.split_whitespace().next().unwrap_or(cmd);
    set_title(&format!("{} @ {}", name, cwd_title()));
}

/// タイトルを既定 (末尾ディレクトリ名) へ戻す (外部コマンド終了時に呼ぶ)。
pub fn reset_title() {
    set_title(&cwd_title());
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
