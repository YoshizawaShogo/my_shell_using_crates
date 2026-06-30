//! シェル組み込みコマンド。

use crate::history::expand_tilde;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

// ─── 定数 ─────────────────────────────────────────────────────────────────────

/// 自動記録したパスの永続化先。
pub const PATHS_FILE: &str = "~/.my_shell_paths";

/// 記録するパスの上限件数 (これを超えた古い (MRU 末尾) エントリを捨てる)。
const MAX_RECENT_PATHS: usize = 500;

// ─── シェル状態 ───────────────────────────────────────────────────────────────

/// ビルトインコマンドが読み書きするシェル状態
pub struct ShellContext {
    /// cd が積み上げるディレクトリスタック (pushd 相当)
    pub dir_stack: Vec<PathBuf>,
    /// コマンド引数から自動記録したパス一覧。MRU 順 (先頭が直近)。
    /// 永続化: ~/.my_shell_paths。Ctrl+G / Ctrl+T のピッカーが参照する。
    pub recent_paths: Vec<PathBuf>,
    /// abbr で定義した略語 (FROM → TO)
    pub abbrs: HashMap<String, String>,
    /// alias で定義したエイリアス (FROM → TO)
    pub aliases: HashMap<String, String>,
    /// 直前のコマンドの終了ステータス ($? に対応)
    pub last_status: i32,
    /// ジョブ制御の表 (Ctrl+Z で停止したジョブと bg で再開したもの)
    pub jobs: Vec<crate::job::Job>,
}

impl Default for ShellContext {
    fn default() -> Self {
        Self {
            dir_stack: Vec::new(),
            recent_paths: load_recent_paths(),
            abbrs: HashMap::new(),
            aliases: HashMap::new(),
            last_status: 0,
            jobs: Vec::new(),
        }
    }
}

/// 永続化ファイルを読み込む。末尾 (新しい方) を優先して dedup し、
/// 存在しないパスを除去したうえで MRU 順 (先頭が直近) で返す。
fn load_recent_paths() -> Vec<PathBuf> {
    let path = expand_tilde(PATHS_FILE);
    let lines: Vec<PathBuf> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();

    // 末尾 (新しい方) を優先して dedup → 結果は新しい順 (MRU)
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<PathBuf> = lines
        .into_iter()
        .rev()
        .filter(|p| seen.insert(p.clone()))
        .collect();

    deduped.retain(|p| p.exists());
    deduped.truncate(MAX_RECENT_PATHS);
    deduped
}

/// パスをファイル末尾に 1 行追記する。
fn append_recent_path(p: &Path) -> io::Result<()> {
    use std::io::Write;
    let file_path = expand_tilde(PATHS_FILE);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_path)?;
    writeln!(file, "{}", p.to_string_lossy())
}

/// コマンドの引数群から実在するパスを `recent_paths` に MRU 記録する。
///
/// `args` は abbr/エイリアス展開・`~`/`$VAR` 展開を済ませた引数文字列で、`cwd` は
/// コマンド実行**前**の作業ディレクトリ (相対パスの解決基準。`cd foo` でも正しく
/// 解決できる)。`-` で始まるフラグや実在しないパスは無視する。ファイルはファイルの
/// まま、ディレクトリはディレクトリのまま記録する。
pub fn record_arg_paths(ctx: &mut ShellContext, args: &[String], cwd: &Path) {
    let mut changed = false;
    for a in args {
        if a.is_empty() || a.starts_with('-') {
            continue;
        }
        let p = PathBuf::from(a);
        let abs = if p.is_absolute() { p } else { cwd.join(p) };
        // canonicalize は実在しなければ Err。これで存在チェックと正規化を兼ねる。
        let Ok(canon) = abs.canonicalize() else {
            continue;
        };
        // MRU: 既存を取り除いて先頭へ。
        ctx.recent_paths.retain(|x| x != &canon);
        ctx.recent_paths.insert(0, canon.clone());
        let _ = append_recent_path(&canon);
        changed = true;
    }
    if changed {
        ctx.recent_paths.truncate(MAX_RECENT_PATHS);
    }
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
    ("abbr", abbr),
    ("alias", alias),
    ("set", set),
    ("setenv", set), // setenv は set のエイリアス
    ("fg", fg),
    ("bg", bg),
    ("jobs", jobs),
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
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "OLDPWD not set"))?,
        Some(path) => {
            let p = expand_tilde(path);
            // ファイルが渡されたとき、その親ディレクトリへ移動する
            if p.is_file() {
                p.parent().map(PathBuf::from).unwrap_or(p)
            } else {
                p
            }
        }
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
    let dest = ctx
        .dir_stack
        .pop()
        .ok_or_else(|| io::Error::other("directory stack is empty"))?;
    let before = std::env::current_dir();
    std::env::set_current_dir(&dest)?;
    if let Ok(cwd) = before {
        unsafe { std::env::set_var("OLDPWD", &cwd) };
    }
    Ok(())
}

// ─── abbr ─────────────────────────────────────────────────────────────────────

fn abbr(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    match (args.first(), args.get(1)) {
        (Some(&from), Some(&to)) => {
            ctx.abbrs.insert(from.to_string(), to.to_string());
            Ok(())
        }
        _ => Err(io::Error::other("usage: abbr FROM TO")),
    }
}

// ─── alias ────────────────────────────────────────────────────────────────────

fn alias(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    match (args.first(), args.get(1)) {
        (Some(&from), Some(&to)) => {
            ctx.aliases.insert(from.to_string(), to.to_string());
            Ok(())
        }
        _ => Err(io::Error::other("usage: alias FROM TO")),
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
        _ => Err(io::Error::other("usage: set VAR VAL")),
    }
}

// ─── fg / bg / jobs (ジョブ制御は job モジュールへ委譲) ────────────────────────

fn fg(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    crate::job::fg(args.first().copied(), ctx)
}

fn bg(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    crate::job::bg(args.first().copied(), ctx)
}

fn jobs(_args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    crate::job::list(ctx)
}
