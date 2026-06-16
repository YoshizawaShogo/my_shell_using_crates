//! 端末モードの管理。

use crossterm::terminal;
use std::io;

/// 端末の raw mode を確実に後始末する RAII ガード。
///
/// 正常終了・パニックいずれでも `Drop` で raw mode を戻す。
/// (`exit()` やシグナルで殺された場合は `Drop` が走らないため、
///  そのケースは [`setup_sigint_handler`] 側で別途救済する)
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

/// 対話シェル本体は SIGINT を無視する。
///
/// raw mode 中の Ctrl+C はキー入力バイトとして読まれ `CancelInput` になるため、
/// シグナルとしての SIGINT は本体には不要。これにより外部コマンド実行中の
/// Ctrl+C でシェル自身が死ぬのを防ぐ (子プロセス側は exec 前に既定動作へ戻す)。
pub fn setup_sigint_handler() -> io::Result<()> {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
    Ok(())
}
