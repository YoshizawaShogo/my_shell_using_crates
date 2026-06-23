//! 端末モードの管理。

use crossterm::terminal;
use std::io;

/// 端末の raw mode を確実に後始末する RAII ガード。
///
/// 正常終了・パニックいずれでも `Drop` で raw mode を戻す。
/// (`exit()` やシグナルで殺された場合は `Drop` が走らないため、
///  そのケースは [`setup_signal_handlers`] 側で別途救済する)
pub struct RawModeGuard;

impl RawModeGuard {
    pub fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
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
