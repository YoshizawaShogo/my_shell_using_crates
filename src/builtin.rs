//! シェル組み込みコマンド。

use crate::history::expand_tilde;
use std::io;
use std::path::PathBuf;

// ─── シェル状態 ───────────────────────────────────────────────────────────────

/// ビルトインコマンドが読み書きするシェル状態
#[derive(Default)]
pub struct ShellContext {
    /// cd が積み上げるディレクトリスタック (pushd 相当)
    pub dir_stack: Vec<PathBuf>,
}

// ─── トレイト ─────────────────────────────────────────────────────────────────

pub trait Builtin {
    fn name(&self) -> &'static str;
    fn run(&self, args: &[&str], ctx: &mut ShellContext) -> io::Result<()>;
}

// ─── cd ───────────────────────────────────────────────────────────────────────

pub struct Cd;

impl Builtin for Cd {
    fn name(&self) -> &'static str {
        "cd"
    }

    /// デフォルトで pushd 挙動: 移動前のディレクトリをスタックに積む。
    /// `cd -` は OLDPWD へ移動 (スタックとは独立)。
    fn run(&self, args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
        let target = match args.first().copied() {
            Some("-") => std::env::var("OLDPWD")
                .map(PathBuf::from)
                .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "OLDPWD が未設定です"))?,
            Some(path) => expand_tilde(path),
            None => std::env::var("HOME")
                .map(PathBuf::from)
                .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?,
        };

        if let Ok(cwd) = std::env::current_dir() {
            // SAFETY: メインスレッドからのみ呼ばれる。
            //         SIGINT ハンドラスレッドは環境変数を参照しない。
            unsafe { std::env::set_var("OLDPWD", &cwd) };
            ctx.dir_stack.push(cwd);
        }

        std::env::set_current_dir(&target)
    }
}

// ─── popd ─────────────────────────────────────────────────────────────────────

pub struct Popd;

impl Builtin for Popd {
    fn name(&self) -> &'static str {
        "popd"
    }

    fn run(&self, _args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
        match ctx.dir_stack.pop() {
            Some(prev) => std::env::set_current_dir(&prev),
            None => Err(io::Error::other("ディレクトリスタックが空です")),
        }
    }
}

// ─── レジストリ ───────────────────────────────────────────────────────────────

pub fn find_builtin(name: &str) -> Option<Box<dyn Builtin>> {
    let candidates: Vec<Box<dyn Builtin>> = vec![Box::new(Cd), Box::new(Popd)];
    candidates.into_iter().find(|b| b.name() == name)
}
