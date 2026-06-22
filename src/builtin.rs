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

// ─── 登録テーブル ─────────────────────────────────────────────────────────────

/// ビルトイン実装の関数ポインタ。
type BuiltinFn = fn(&[&str], &mut ShellContext) -> io::Result<()>;

/// `(名前, 実装)` の唯一の真実の源。
///
/// 実行 (`find_builtin`) もコマンド名補完 (`builtin_names`) もこの表だけを参照する。
/// ビルトインを増やすときはここに 1 行足せばよい。
const BUILTINS: &[(&str, BuiltinFn)] = &[
    ("cd", cd),
    ("popd", popd),
    ("reg_path", reg_path),
    ("abbr", abbr),
    ("alias", alias),
    ("set", set),
    ("setenv", set), // setenv は set のエイリアス
];

/// 名前に対応するビルトイン実装を返す。
pub fn find_builtin(name: &str) -> Option<BuiltinFn> {
    BUILTINS.iter().find(|&&(n, _)| n == name).map(|&(_, f)| f)
}

/// 登録済みビルトイン名を列挙する (コマンド名補完用)。
pub fn builtin_names() -> impl Iterator<Item = &'static str> {
    BUILTINS.iter().map(|&(n, _)| n)
}

// ─── cd ───────────────────────────────────────────────────────────────────────

/// デフォルトで pushd 挙動: 移動前のディレクトリをスタックに積む。
/// `cd -` は OLDPWD へ移動 (スタックとは独立)。
fn cd(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    let target = match args.first().copied() {
        Some("-") => std::env::var("OLDPWD")
            .map(PathBuf::from)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "OLDPWD が未設定です"))?,
        Some(path) => expand_tilde(path),
        None => std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?,
    };

    // 移動前の cwd を控えておき、cd が成功したときだけ OLDPWD とスタックを更新する。
    // (存在しないディレクトリへの cd で OLDPWD やスタックが壊れるのを防ぐ)
    let prev = std::env::current_dir();
    std::env::set_current_dir(&target)?;

    if let Ok(cwd) = prev {
        // SAFETY: ビルトインはメインスレッドからのみ実行される。Ctrl+T 列挙で一時的に
        // spawn されるワーカースレッドは getenv/setenv を一切呼ばないため、environ への
        // 並行アクセスは発生しない。
        unsafe { std::env::set_var("OLDPWD", &cwd) };
        ctx.dir_stack.push(cwd);
    }
    Ok(())
}

// ─── popd ─────────────────────────────────────────────────────────────────────

fn popd(_args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    match ctx.dir_stack.pop() {
        Some(prev) => std::env::set_current_dir(&prev),
        None => Err(io::Error::other("ディレクトリスタックが空です")),
    }
}

// ─── reg_path ─────────────────────────────────────────────────────────────────

fn reg_path(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
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
        Some("rm") => {
            // add と同じ正規化で対象を求め、一致する登録を消す。
            let path = match args.get(1) {
                Some(&p) => expand_tilde(p),
                None => std::env::current_dir()?,
            };
            let path = path.canonicalize().unwrap_or(path);
            let before = ctx.reg_paths.len();
            ctx.reg_paths.retain(|p| p != &path);
            if ctx.reg_paths.len() == before {
                return Err(io::Error::other(format!(
                    "登録されていません: {}",
                    path.display()
                )));
            }
            save_reg_paths(&ctx.reg_paths)
        }
        Some("list") => {
            for p in &ctx.reg_paths {
                execute!(stdout(), Print(format!("{}\r\n", p.display())))?;
            }
            Ok(())
        }
        _ => Err(io::Error::other(
            "使い方: reg_path add [path] | rm [path] | list",
        )),
    }
}

// ─── abbr ─────────────────────────────────────────────────────────────────────

fn abbr(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    match (args.first(), args.get(1)) {
        (Some(&from), Some(&to)) => {
            ctx.abbrs.insert(from.to_string(), to.to_string());
            Ok(())
        }
        _ => Err(io::Error::other("使い方: abbr FROM TO")),
    }
}

// ─── alias ────────────────────────────────────────────────────────────────────

fn alias(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    match (args.first(), args.get(1)) {
        (Some(&from), Some(&to)) => {
            ctx.aliases.insert(from.to_string(), to.to_string());
            Ok(())
        }
        _ => Err(io::Error::other("使い方: alias FROM TO")),
    }
}

// ─── set / setenv ─────────────────────────────────────────────────────────────

fn set(args: &[&str], _ctx: &mut ShellContext) -> io::Result<()> {
    match (args.first(), args.get(1)) {
        (Some(&var), Some(&val)) => {
            // SAFETY: ビルトインはメインスレッドからのみ実行される。Ctrl+T 列挙で一時的に
            // spawn されるワーカースレッドは getenv/setenv を一切呼ばないため、environ への
            // 並行アクセスは発生しない。
            unsafe { std::env::set_var(var, val) };
            Ok(())
        }
        _ => Err(io::Error::other("使い方: set VAR VAL")),
    }
}
