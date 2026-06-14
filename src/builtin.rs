//! シェル組み込みコマンド。
//!
//! 外部プロセスに委譲できないコマンド (cd など) はここに実装する。
//! `Builtin` トレイトを実装して `find_builtin` に登録すれば認識される。

use std::io;

// ─── トレイト ─────────────────────────────────────────────────────────────────

pub trait Builtin {
    fn name(&self) -> &'static str;
    fn run(&self, args: &[&str]) -> io::Result<()>;
}

// ─── cd ───────────────────────────────────────────────────────────────────────

pub struct Cd;

impl Builtin for Cd {
    fn name(&self) -> &'static str {
        "cd"
    }

    fn run(&self, args: &[&str]) -> io::Result<()> {
        let target = match args.first().copied() {
            Some(path) => expand_tilde(path),
            None => {
                let home = std::env::var("HOME")
                    .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?;
                std::path::PathBuf::from(home)
            }
        };
        std::env::set_current_dir(&target)
    }
}

// ─── レジストリ ───────────────────────────────────────────────────────────────

/// 名前に対応する組み込みコマンドを返す
pub fn find_builtin(name: &str) -> Option<Box<dyn Builtin>> {
    let candidates: Vec<Box<dyn Builtin>> = vec![Box::new(Cd)];
    candidates.into_iter().find(|b| b.name() == name)
}

// ─── ユーティリティ ───────────────────────────────────────────────────────────

fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~" {
        std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("~"))
    } else if let Some(rest) = path.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(rest))
            .unwrap_or_else(|_| std::path::PathBuf::from(path))
    } else {
        std::path::PathBuf::from(path)
    }
}
