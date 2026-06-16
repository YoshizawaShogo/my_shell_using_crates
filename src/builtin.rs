//! シェル組み込みコマンド。

use crate::history::expand_tilde;
use crossterm::{execute, style::Print};
use std::collections::HashMap;
use std::io::{self, stdout};
use std::path::PathBuf;

// ─── 定数 ─────────────────────────────────────────────────────────────────────

pub const REG_PATHS_FILE: &str = "~/.my_shell_paths";

// ─── シェル状態 ───────────────────────────────────────────────────────────────

/// ビルトインコマンドが読み書きするシェル状態
pub struct ShellContext {
    /// cd が積み上げるディレクトリスタック (pushd 相当)
    pub dir_stack: Vec<PathBuf>,
    /// reg_path add で登録したパス一覧 (永続化: ~/.my_shell_paths)
    pub reg_paths: Vec<PathBuf>,
    /// abbr で定義した略語 (FROM → TO)
    pub abbrs: HashMap<String, String>,
    /// alias で定義したエイリアス (FROM → TO)
    pub aliases: HashMap<String, String>,
    /// 直前のコマンドの終了ステータス ($? に対応)
    pub last_status: i32,
}

impl Default for ShellContext {
    fn default() -> Self {
        Self {
            dir_stack: Vec::new(),
            reg_paths: load_reg_paths(),
            abbrs: HashMap::new(),
            aliases: HashMap::new(),
            last_status: 0,
        }
    }
}

fn load_reg_paths() -> Vec<PathBuf> {
    let path = expand_tilde(REG_PATHS_FILE);
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn save_reg_paths(paths: &[PathBuf]) -> io::Result<()> {
    let path = expand_tilde(REG_PATHS_FILE);
    let content = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{}\n", content))
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

// ─── reg_path ─────────────────────────────────────────────────────────────────

pub struct RegPath;

impl Builtin for RegPath {
    fn name(&self) -> &'static str {
        "reg_path"
    }

    fn run(&self, args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
        match args.first().copied() {
            Some("add") => {
                let path = match args.get(1) {
                    Some(&p) => expand_tilde(p),
                    None => std::env::current_dir()?,
                };
                let path = path.canonicalize().unwrap_or(path);
                if !ctx.reg_paths.contains(&path) {
                    ctx.reg_paths.push(path);
                    save_reg_paths(&ctx.reg_paths)?;
                }
                Ok(())
            }
            Some("list") => {
                for p in &ctx.reg_paths {
                    execute!(stdout(), Print(format!("{}\r\n", p.display())))?;
                }
                Ok(())
            }
            _ => Err(io::Error::other("使い方: reg_path add [path] | list")),
        }
    }
}

// ─── abbr ─────────────────────────────────────────────────────────────────────

pub struct Abbr;

impl Builtin for Abbr {
    fn name(&self) -> &'static str {
        "abbr"
    }

    fn run(&self, args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
        match (args.first(), args.get(1)) {
            (Some(&from), Some(&to)) => {
                ctx.abbrs.insert(from.to_string(), to.to_string());
                Ok(())
            }
            _ => Err(io::Error::other("使い方: abbr FROM TO")),
        }
    }
}

// ─── alias ────────────────────────────────────────────────────────────────────

pub struct Alias;

impl Builtin for Alias {
    fn name(&self) -> &'static str {
        "alias"
    }

    fn run(&self, args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
        match (args.first(), args.get(1)) {
            (Some(&from), Some(&to)) => {
                ctx.aliases.insert(from.to_string(), to.to_string());
                Ok(())
            }
            _ => Err(io::Error::other("使い方: alias FROM TO")),
        }
    }
}

// ─── set ──────────────────────────────────────────────────────────────────────

pub struct Set;

impl Builtin for Set {
    fn name(&self) -> &'static str {
        "set"
    }

    fn run(&self, args: &[&str], _ctx: &mut ShellContext) -> io::Result<()> {
        match (args.first(), args.get(1)) {
            (Some(&var), Some(&val)) => {
                // SAFETY: メインスレッドからのみ呼ばれる。
                //         SIGINT ハンドラスレッドは環境変数を参照しない。
                unsafe { std::env::set_var(var, val) };
                Ok(())
            }
            _ => Err(io::Error::other("使い方: set VAR VAL")),
        }
    }
}

// ─── setenv ───────────────────────────────────────────────────────────────────

pub struct Setenv;

impl Builtin for Setenv {
    fn name(&self) -> &'static str {
        "setenv"
    }

    fn run(&self, args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
        Set.run(args, ctx)
    }
}

// ─── レジストリ ───────────────────────────────────────────────────────────────

pub fn find_builtin(name: &str) -> Option<Box<dyn Builtin>> {
    let candidates: Vec<Box<dyn Builtin>> = vec![
        Box::new(Cd),
        Box::new(Popd),
        Box::new(RegPath),
        Box::new(Abbr),
        Box::new(Alias),
        Box::new(Set),
        Box::new(Setenv),
    ];
    candidates.into_iter().find(|b| b.name() == name)
}
