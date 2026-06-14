//! 端末モードの管理。

use crossterm::terminal;
use signal_hook::{consts::SIGINT, iterator::Signals};
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

/// 別スレッドで SIGINT を監視し、外部シグナルでも raw mode を解除してから終了する
pub fn setup_sigint_handler() -> io::Result<()> {
    let mut signals = Signals::new([SIGINT])?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = terminal::disable_raw_mode();
            std::process::exit(130); // 慣例: 128 + SIGINT(2)
        }
    });
    Ok(())
}
