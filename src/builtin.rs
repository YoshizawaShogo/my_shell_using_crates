//! シェル組み込みコマンド。

use crate::history::expand_tilde;
use std::collections::HashMap;
use std::io::{self, BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};

// ─── 定数 ─────────────────────────────────────────────────────────────────────

/// 自動記録したパスの永続化先。
pub const PATHS_FILE: &str = "~/.my_shell_paths";

/// 記録するパスの上限件数 (これを超えた古い (MRU 末尾) エントリを捨てる)。
const MAX_RECENT_PATHS: usize = 500;

// ─── シェル状態 ───────────────────────────────────────────────────────────────

/// 記録済みパス 1 件。
///
/// `is_dir` は記録/読込時の 1 回の `metadata` で確定させて保持する。これにより
/// Ctrl+G の cd 先フィルタ (ディレクトリのみ抽出) を、ピッカーを開くたびに全件
/// `is_dir()` (= stat) し直さずに済ませる。stat は I/O で、特に WSL2 の /mnt などは
/// 桁違いに遅いため、開くたびの stat がピッカーの体感遅延の主因だった。
pub struct RecentPath {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// ビルトインコマンドが読み書きするシェル状態
pub struct ShellContext {
    /// cd が積み上げるディレクトリスタック (pushd 相当)
    pub dir_stack: Vec<PathBuf>,
    /// コマンド引数から自動記録したパス一覧。MRU 順 (先頭が直近)。
    /// 永続化: ~/.my_shell_paths。Ctrl+G / Ctrl+T のピッカーが参照する。
    pub recent_paths: Vec<RecentPath>,
    /// ~/.my_shell_paths の既読バイト数 (差分読み込み用)
    pub paths_file_offset: u64,
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
        let (recent_paths, paths_file_offset) = load_recent_paths();
        Self {
            dir_stack: Vec::new(),
            recent_paths,
            paths_file_offset,
            abbrs: HashMap::new(),
            aliases: HashMap::new(),
            last_status: 0,
            jobs: Vec::new(),
        }
    }
}

/// 永続化ファイルを読み込む。末尾 (新しい方) を優先して dedup し、
/// 存在しないパスを除去したうえで MRU 順 (先頭が直近) で返す。
/// 戻り値: (パス一覧, ファイルサイズ)
fn load_recent_paths() -> (Vec<RecentPath>, u64) {
    let path = expand_tilde(PATHS_FILE);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let lines: Vec<PathBuf> = content
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();

    // 末尾 (新しい方) を優先して dedup → 結果は新しい順 (MRU)
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<PathBuf> = lines
        .into_iter()
        .rev()
        .filter(|p| seen.insert(p.clone()))
        .collect();

    // 実在チェックと is_dir 判定を 1 回の metadata で兼ねる (存在しなければ除外)。
    let mut result: Vec<RecentPath> = deduped.into_iter().filter_map(to_recent_path).collect();
    result.truncate(MAX_RECENT_PATHS);
    (result, file_size)
}

/// パスを 1 回 `metadata` して `RecentPath` にする。存在しなければ `None`。
/// (`metadata` はシンボリックリンクを辿るので、実体が dir かで `is_dir` を決める。)
fn to_recent_path(p: PathBuf) -> Option<RecentPath> {
    let is_dir = std::fs::metadata(&p).ok()?.is_dir();
    Some(RecentPath { path: p, is_dir })
}

/// ファイルの差分を読み込み、他端末が記録したパスをメモリへ取り込む。
/// プロンプト再描画前に呼ぶ。ファイルが増えていなければ即時リターン。
pub fn reload_recent_paths(ctx: &mut ShellContext) {
    let path = expand_tilde(PATHS_FILE);
    let Ok(mut file) = std::fs::File::open(&path) else {
        return;
    };
    let Ok(meta) = file.metadata() else { return };
    if meta.len() <= ctx.paths_file_offset {
        return;
    }
    if file.seek(SeekFrom::Start(ctx.paths_file_offset)).is_err() {
        return;
    }
    let new_paths: Vec<RecentPath> = io::BufReader::new(&file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter_map(to_recent_path)
        .collect();
    ctx.paths_file_offset = meta.len();
    for rp in new_paths {
        ctx.recent_paths.retain(|x| x.path != rp.path);
        ctx.recent_paths.insert(0, rp);
    }
    ctx.recent_paths.truncate(MAX_RECENT_PATHS);
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
        let is_dir = canon.is_dir();
        // MRU: 既存を取り除いて先頭へ。
        ctx.recent_paths.retain(|x| x.path != canon);
        ctx.recent_paths.insert(
            0,
            RecentPath {
                path: canon.clone(),
                is_dir,
            },
        );
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
    ("source-env", source_env),
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

    // SAFETY: ビルトインはメインスレッドからのみ実行される。Ctrl+T 列挙で一時的に
    // spawn されるワーカースレッドは getenv/setenv を一切呼ばないため、environ への
    // 並行アクセスは発生しない。
    //
    // PWD を実際のカレントへ更新する。これで `$PWD` (ビルトインは env::var、外部
    // コマンドは sh 経由で参照) が cd 後も正しい値になる。target は相対の場合が
    // あるので current_dir() で絶対パスを取り直す。
    if let Ok(now) = std::env::current_dir() {
        unsafe { std::env::set_var("PWD", &now) };
    }

    if let Ok(cwd) = prev {
        unsafe { std::env::set_var("OLDPWD", &cwd) };
        ctx.dir_stack.retain(|x| x != &cwd);
        ctx.dir_stack.push(cwd);
    }
    Ok(())
}

// ─── source-env ───────────────────────────────────────────────────────────────

/// 別シェルで設定ファイルを source し、環境変数をこのシェルへ取り込む。
///
/// 使い方:
///   `source-env <file>`           ファイル名・拡張子・shebang からシェルを自動判別
///   `source-env <file> <shell>`   シェルを明示
fn source_env(args: &[&str], _ctx: &mut ShellContext) -> io::Result<()> {
    let (shell, file) = match args.len() {
        2 => {
            let shell = args[1].to_string();
            (shell, args[0])
        }
        1 => {
            let shell = detect_shell(args[0]).ok_or_else(|| {
                io::Error::other("cannot detect shell; use: source-env <file> <shell>")
            })?;
            (shell, args[0])
        }
        _ => return Err(io::Error::other("usage: source-env <file> [shell]")),
    };

    let file_path = expand_tilde(file);
    let file_str = file_path.to_string_lossy();
    let script = match shell.as_str() {
        "fish" => format!("source '{}' && env -0", file_str),
        _ => format!(". '{}' && env -0", file_str),
    };

    let output = std::process::Command::new(&shell)
        .args(["-c", &script])
        .output()
        .map_err(|e| io::Error::other(format!("failed to run {}: {}", shell, e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "{} exited with error: {}",
            shell,
            stderr.trim()
        )));
    }

    for record in output.stdout.split(|&b| b == 0) {
        if record.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(record);
        if let Some((k, v)) = s.split_once('=') {
            // SAFETY: ビルトインはメインスレッドからのみ実行される。
            unsafe { std::env::set_var(k, v) };
        }
    }

    Ok(())
}

/// ファイル名・shebang の順でシェルを推定する。
/// ファイル名にシェル名が含まれるかをサブストリングで判定する (bash → zsh → fish → csh → sh の順)。
/// sh は bash/zsh を誤検知しないよう最後に評価する。
fn detect_shell(file: &str) -> Option<String> {
    let basename = Path::new(file)
        .file_name()?
        .to_string_lossy()
        .to_lowercase();

    for shell in ["bash", "zsh", "fish", "csh", "sh"] {
        if basename.contains(shell) {
            return Some(shell.to_string());
        }
    }

    // サブストリングで拾えない既知ファイル名
    let by_name = match basename.as_str() {
        ".zlogin" | ".zprofile" | ".zlogout" => Some("zsh"),
        ".login" => Some("csh"),
        ".profile" => Some("sh"),
        _ => None,
    };
    if let Some(s) = by_name {
        return Some(s.to_string());
    }

    // shebang (#! /usr/bin/env bash や #!/bin/zsh など)
    let first_line = std::fs::read_to_string(file)
        .ok()
        .and_then(|s| s.lines().next().map(str::to_string))?;
    let shebang = first_line.strip_prefix("#!")?.trim().to_string();
    let shell_name = Path::new(shebang.split_whitespace().last()?)
        .file_name()?
        .to_string_lossy()
        .to_string();
    Some(shell_name)
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
