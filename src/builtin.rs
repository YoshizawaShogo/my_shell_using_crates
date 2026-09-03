//! シェル組み込みコマンド。

use crate::history::expand_tilde;
use crossterm::execute;
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use std::collections::HashMap;
use std::io::{self, BufRead, Seek, SeekFrom, stdout};
use std::path::{Path, PathBuf};

// ─── 定数 ─────────────────────────────────────────────────────────────────────

/// 自動記録したパスの永続化先。
pub const PATHS_FILE: &str = "~/.my_shell_paths";

/// 記録するパスの上限件数 (これを超えた古い (MRU 末尾) エントリを捨てる)。
const MAX_RECENT_PATHS: usize = 10_000;

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
    /// exit ビルトインが要求した終了ステータス (None = 終了要求なし)。
    /// main.rs のイベントループが拾ってシェルを終了させる。
    pub exit_status: Option<i32>,
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
            exit_status: None,
        }
    }
}

/// 永続化ファイルを読み込む。末尾 (新しい方) を優先して dedup し、MRU 順
/// (先頭が直近) で返す。戻り値: (パス一覧, ファイルサイズ)
///
/// is_dir は各行の末尾 `/` の有無から判定するので **stat しない**。存在しない
/// パスの除去も `refresh` ビルトインの手動実行に一本化し、起動を軽く保つ
/// (WSL2 の /mnt などで stat が桁違いに遅い問題を回避する)。
fn load_recent_paths() -> (Vec<RecentPath>, u64) {
    let path = expand_tilde(PATHS_FILE);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    // 末尾 (新しい方) を優先して dedup → 結果は新しい順 (MRU)。
    let mut seen = std::collections::HashSet::new();
    let mut result: Vec<RecentPath> = content
        .lines()
        .rev()
        .filter_map(parse_recent_line)
        .filter(|rp| seen.insert(rp.path.clone()))
        .collect();
    result.truncate(MAX_RECENT_PATHS);
    (result, file_size)
}

/// ファイルの 1 行を `RecentPath` に変換する (stat しない)。
/// 末尾 `/` があればディレクトリ、無ければファイルとみなし、パスからは `/` を除く。
/// 空行や `/` だけの行は `None`。
///
/// (末尾 `/` を is_dir の永続フラグとして使う。記録時に 1 回 stat した結果を
///  ここへ持ち越すことで、起動・リロードで stat し直さずに済む。)
fn parse_recent_line(line: &str) -> Option<RecentPath> {
    let (path_str, is_dir) = match line.strip_suffix('/') {
        Some(stripped) => (stripped, true),
        None => (line, false),
    };
    if path_str.is_empty() {
        return None;
    }
    Some(RecentPath {
        path: PathBuf::from(path_str),
        is_dir,
    })
}

/// `RecentPath` をファイル 1 行の文字列にする。ディレクトリは末尾 `/` を付けて
/// is_dir を永続化する ([parse_recent_line] と対を成す)。
fn recent_line(rp: &RecentPath) -> String {
    if rp.is_dir {
        format!("{}/", rp.path.to_string_lossy())
    } else {
        rp.path.to_string_lossy().into_owned()
    }
}

/// パスを 1 回 `metadata` して `RecentPath` にする。存在しなければ `None`。
/// (`metadata` はシンボリックリンクを辿るので、実体が dir かで `is_dir` を決める。)
/// 実在チェックを伴う唯一の経路なので、`refresh` の掃除でのみ使う。
fn stat_recent_path(p: PathBuf) -> Option<RecentPath> {
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
    // ファイルが縮んでいたら refresh (compact) で書き直された合図。オフセットが
    // 行境界とズレて壊れた読み込みになるので、全体を読み直して再構築する。
    if meta.len() < ctx.paths_file_offset {
        let (recent_paths, offset) = load_recent_paths();
        ctx.recent_paths = recent_paths;
        ctx.paths_file_offset = offset;
        return;
    }
    if meta.len() == ctx.paths_file_offset {
        return;
    }
    if file.seek(SeekFrom::Start(ctx.paths_file_offset)).is_err() {
        return;
    }
    let new_paths: Vec<RecentPath> = io::BufReader::new(&file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| parse_recent_line(&l))
        .collect();
    ctx.paths_file_offset = meta.len();
    for rp in new_paths {
        ctx.recent_paths.retain(|x| x.path != rp.path);
        ctx.recent_paths.insert(0, rp);
    }
    ctx.recent_paths.truncate(MAX_RECENT_PATHS);
}

/// パスをファイル末尾に 1 行追記する (ディレクトリは末尾 `/` 付き)。
fn append_recent_path(rp: &RecentPath) -> io::Result<()> {
    use std::io::Write;
    let file_path = expand_tilde(PATHS_FILE);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_path)?;
    writeln!(file, "{}", recent_line(rp))
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
        let rp = RecentPath {
            path: canon.clone(),
            is_dir,
        };
        // MRU: 既存を取り除いて先頭へ。追記してから insert (insert が rp を消費する)。
        ctx.recent_paths.retain(|x| x.path != canon);
        let _ = append_recent_path(&rp);
        ctx.recent_paths.insert(0, rp);
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
    ("groot", groot),
    ("source-env", source_env),
    ("abbr", abbr),
    ("alias", alias),
    ("type", type_info),
    ("set", set),
    ("setenv", set), // setenv は set のエイリアス
    ("fg", fg),
    ("bg", bg),
    ("jobs", jobs),
    ("refresh", refresh),
    ("exit", exit),
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

    change_dir(&target, ctx)
}

/// カレントディレクトリを `target` へ移し、OLDPWD・dir_stack・端末通知を更新する。
/// cd / groot など「移動するビルトイン」が共通で使う。
fn change_dir(target: &Path, ctx: &mut ShellContext) -> io::Result<()> {
    // 移動前の cwd を控えておき、移動が成功したときだけ OLDPWD とスタックを更新する。
    // (存在しないディレクトリへの移動で OLDPWD やスタックが壊れるのを防ぐ)
    let prev = std::env::current_dir();
    std::env::set_current_dir(target)?;

    // SAFETY: ビルトインはメインスレッドからのみ実行される。Ctrl+T 列挙で一時的に
    // spawn されるワーカースレッドは getenv/setenv を一切呼ばないため、environ への
    // 並行アクセスは発生しない。
    //
    // PWD を実際のカレントへ更新する。これで `$PWD` (ビルトインは env::var、外部
    // コマンドは sh 経由で参照) が移動後も正しい値になる。target は相対の場合が
    // あるので current_dir() で絶対パスを取り直す。
    if let Ok(now) = std::env::current_dir() {
        unsafe { std::env::set_var("PWD", &now) };
    }

    if let Ok(cwd) = prev {
        unsafe { std::env::set_var("OLDPWD", &cwd) };
        ctx.dir_stack.retain(|x| x != &cwd);
        ctx.dir_stack.push(cwd);
    }

    // 移動先を端末へ知らせる (OSC 7 / タイトル)。cwd を変えるのは移動系ビルトインだけ
    // なので、ここと起動時の 1 回で足りる (毎回の再描画で送るとキー入力ごとに出る)。
    crate::term::notify_cwd();
    Ok(())
}

// ─── groot ──────────────────────────────────────────────────────────────────────

/// 現在の git リポジトリ (worktree) のルートへ移動する。リポジトリ外ならエラー。
/// カスタマイズ無しで使えるよう組み込みで提供する。
fn groot(_args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    let root = git_toplevel()?;
    change_dir(&root, ctx)
}

/// `git rev-parse --show-toplevel` で現在の worktree ルートを求める。
fn git_toplevel() -> io::Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| io::Error::other(format!("failed to run git: {}", e)))?;
    if !output.status.success() {
        return Err(io::Error::other("not a git repository"));
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(io::Error::other("not a git repository"));
    }
    Ok(PathBuf::from(path))
}

// ─── source-env ───────────────────────────────────────────────────────────────

/// 別シェルで設定ファイルを source し、環境変数と alias をこのシェルへ取り込む。
///
/// 環境変数だけでは不足するケース (例: PATH を alias で通している設定) があるため、
/// alias も継承する。
///
/// 使い方:
///   `source-env <file>`           ファイル名・拡張子・shebang からシェルを自動判別
///   `source-env <file> <shell>`   シェルを明示
fn source_env(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
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
    let fmt = AliasFmt::for_shell(&shell);
    // csh/tcsh には POSIX の `.` (dot) が無く source は `source`。fish も `source`。
    // これを `.` の分岐へ落とすと `.` が外部コマンド扱いになり必ず失敗する。
    let src = if fmt == AliasFmt::Posix {
        "."
    } else {
        "source"
    };

    // env と alias を 2 回に分けて取り込む (1 回で混ぜるより設計が単純)。どちらも
    // `source が成功したときだけ出力する` (`&&`) 形にして source 失敗を検出する。
    // env は複数行値も NUL で確実に区切れるよう `env -0` を使う。
    let env_out = run_shell_capture(&shell, &format!("{} '{}' && env -0", src, file_str))?;
    for record in env_out.split(|&b| b == 0) {
        if record.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(record);
        if let Some((k, v)) = s.split_once('=') {
            // SAFETY: ビルトインはメインスレッドからのみ実行される。
            unsafe { std::env::set_var(k, v) };
        }
    }

    // alias は 2 回目の起動で取得する。source は env 取得時に検証済み。空 alias でも
    // `alias` は 0 を返す (bash/fish/csh とも確認済み) ので追加のガードは要らない。
    let alias_out = run_shell_capture(&shell, &format!("{} '{}' && alias", src, file_str))?;
    for (name, value) in parse_aliases(&String::from_utf8_lossy(&alias_out), fmt) {
        ctx.aliases.insert(name, value);
    }

    Ok(())
}

/// `shell -c <script>` を実行し、成功時のみ stdout を返す。失敗時は stderr を載せた
/// エラーにする。
fn run_shell_capture(shell: &str, script: &str) -> io::Result<Vec<u8>> {
    let output = std::process::Command::new(shell)
        .args(["-c", script])
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
    Ok(output.stdout)
}

/// `alias` 出力の書式。シェル系統ごとに区切り方が異なる。
#[derive(Clone, Copy, PartialEq)]
enum AliasFmt {
    /// bash/zsh/sh/…: `alias ll='ls -la'` / `ll='ls -la'` (`=` 区切り・引用符)
    Posix,
    /// fish: `alias ll 'ls -la'` (先頭に `alias `・スペース区切り・引用符)
    Fish,
    /// csh/tcsh: `ll<TAB>ls -la` (タブ区切り・引用符なし)
    Csh,
}

impl AliasFmt {
    fn for_shell(shell: &str) -> Self {
        match shell {
            "fish" => AliasFmt::Fish,
            "csh" | "tcsh" => AliasFmt::Csh,
            _ => AliasFmt::Posix,
        }
    }
}

/// `alias` の全出力を (名前, 展開先) の並びへ分解する。書式は [`AliasFmt`] で切り替える。
fn parse_aliases(text: &str, fmt: AliasFmt) -> Vec<(String, String)> {
    match fmt {
        AliasFmt::Posix => parse_aliases_posix(text),
        AliasFmt::Fish => parse_aliases_fish(text),
        AliasFmt::Csh => parse_aliases_csh(text),
    }
}

/// bash/zsh/sh の `alias` 出力を解析する。
///
/// 各定義は `alias NAME='VALUE'` (zsh は先頭 `alias ` 無し)。VALUE は単一引用符・
/// 二重引用符・引用符なしのいずれかで、引用符付きの値は物理改行をまたぐことがある
/// (複数行 alias)。bash は値中のリテラル `'` を `'\''` で表す。
fn parse_aliases_posix(text: &str) -> Vec<(String, String)> {
    let b = text.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        // レコード先頭: 改行を読み飛ばす。
        if b[i] == b'\n' || b[i] == b'\r' {
            i += 1;
            continue;
        }
        // 先頭の "alias " を任意に外す (bash は前置、zsh は無し)。
        if text[i..].starts_with("alias ") {
            i += "alias ".len();
        }
        // NAME を '=' まで読む。'=' の前に改行/EOF なら不正行として捨てる。
        let name_start = i;
        while i < b.len() && b[i] != b'=' && b[i] != b'\n' {
            i += 1;
        }
        if i >= b.len() || b[i] == b'\n' {
            continue; // '=' 無し → この行はスキップ (改行は次ループで処理)
        }
        let name = text[name_start..i].trim().to_string();
        i += 1; // '=' を飛ばす
        let (value, next) = read_posix_value(text, i);
        i = next;
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

/// `=` の直後 (`start`) から値を読み、(値, 次の走査位置) を返す。
/// 引用符付きの値は行をまたいで閉じ引用符まで、引用符なしは行末まで読む。
fn read_posix_value(text: &str, start: usize) -> (String, usize) {
    let b = text.as_bytes();
    match b.get(start) {
        Some(b'\'') => read_quoted(text, start + 1, Quote::Single),
        Some(b'"') => read_quoted(text, start + 1, Quote::Double),
        _ => {
            let mut i = start;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            (text[start..i].to_string(), i)
        }
    }
}

enum Quote {
    Single,
    Double,
}

/// 引用符の内側 (`start`) から閉じ引用符までを読む。閉じたあとは行末まで読み飛ばす。
///
/// - Single: bash/zsh の `'\''` (閉じ→エスケープした `'`→開き) をリテラル `'` に戻す。
/// - Double: `\"` `\\` のバックスラッシュエスケープを外す。
fn read_quoted(text: &str, start: usize, quote: Quote) -> (String, usize) {
    let b = text.as_bytes();
    let (q, esc): (u8, u8) = match quote {
        Quote::Single => (b'\'', b'\''),
        Quote::Double => (b'"', b'\\'),
    };
    let mut v = String::new();
    let mut seg = start;
    let mut i = start;
    while i < b.len() {
        // Double: `\"` / `\\` を外す。
        if esc == b'\\'
            && b[i] == b'\\'
            && matches!(b.get(i + 1), Some(&c) if c == b'"' || c == b'\\')
        {
            v.push_str(&text[seg..i]);
            v.push(b[i + 1] as char);
            i += 2;
            seg = i;
            continue;
        }
        // Single: `'\''` をリテラル `'` に戻す。
        if esc == b'\'' && b[i] == b'\'' && text[i..].starts_with("'\\''") {
            v.push_str(&text[seg..i]);
            v.push('\'');
            i += 4;
            seg = i;
            continue;
        }
        if b[i] == q {
            v.push_str(&text[seg..i]);
            i += 1;
            while i < b.len() && b[i] != b'\n' {
                i += 1; // 閉じ引用符のあとの残り (通常は空) を読み飛ばす
            }
            return (v, i);
        }
        i += 1;
    }
    // 未終端: 残り全部を値とする。
    v.push_str(&text[seg..]);
    (v, b.len())
}

/// fish の `alias` 出力を解析する。各行は `alias NAME 'BODY'`。
///
/// fish は出力を必ず 1 行に収める (値中の改行は `\n` へエスケープ) ので行単位で解析
/// できる。ただし fish は alias を関数として保持し、複数行 alias は `alias` 表示時に
/// 値が崩れる (先頭に関数名が混じる等)。このため単一行 alias のみ忠実に復元でき、
/// 複数行 alias はベストエフォート (fish のエスケープ済み文字列のまま取り込む)。
fn parse_aliases_fish(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(body) = line.trim().strip_prefix("alias ") else {
            continue;
        };
        let Some((name, raw)) = body.split_once(' ') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        out.push((name.to_string(), unquote_fish_value(raw.trim())));
    }
    out
}

/// fish が値を囲む単一/二重引用符を外す。
fn unquote_fish_value(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'\'' && b[b.len() - 1] == b'\'') || (b[0] == b'"' && b[b.len() - 1] == b'"'))
    {
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

/// csh/tcsh の `alias` 出力を解析する。各定義は `NAME<TAB>VALUE` (引用符なし)。
///
/// 複数行 alias の 2 行目以降はタブを含まない継続行になるので、直前の値へ改行付きで
/// 連結する。
fn parse_aliases_csh(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if let Some((name, val)) = line.split_once('\t') {
            let name = name.trim();
            if !name.is_empty() {
                out.push((name.to_string(), val.to_string()));
            }
        } else if let Some(last) = out.last_mut() {
            // タブ無し = 継続行
            last.1.push('\n');
            last.1.push_str(line);
        }
    }
    out
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
        // 引数 1 個: 登録済みなら現在の展開先を表示する。
        (Some(&from), None) => match ctx.abbrs.get(from) {
            Some(to) => {
                execute!(stdout(), Print(format!("{} = {}\r\n", from, to)))?;
                Ok(())
            }
            None => Err(io::Error::other(format!("no such abbr: {}", from))),
        },
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
        // 引数 1 個: 登録済みなら現在の展開先を表示する。
        (Some(&from), None) => match ctx.aliases.get(from) {
            Some(to) => {
                execute!(stdout(), Print(format!("{} = {}\r\n", from, to)))?;
                Ok(())
            }
            None => Err(io::Error::other(format!("no such alias: {}", from))),
        },
        _ => Err(io::Error::other("usage: alias FROM TO")),
    }
}

// ─── type (名前について分かることを種別を問わず全部出す) ───────────────────────

/// raw mode 用に 1 行出力する (改行は `\r\n`)。
fn println_raw(s: &str) -> io::Result<()> {
    execute!(stdout(), Print(format!("{}\r\n", s)))
}

/// パスが実行可能ファイル (通常ファイルかつ実行ビットあり) か。
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// PATH を走査して `name` に一致する **最初の** 実行可能ファイル (実際に走る 1 件) を返す。
/// `name` に `/` を含むならパス指定とみなし PATH 検索しない。
fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var("PATH").ok()?;
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|cand| is_executable(cand))
}

/// 実体パスの表示文字列。symlink なら最終ターゲットまで辿って `(→ target)` を付ける
/// (`vi → vim`、`python → python3.x`、`/etc/alternatives/...` を一目で分かるように)。
fn format_binary(p: &Path) -> String {
    let base = p.display().to_string();
    match std::fs::symlink_metadata(p) {
        // symlink のときだけ canonicalize で全リンクを解決した最終実体を添える。
        Ok(m) if m.file_type().is_symlink() => match std::fs::canonicalize(p) {
            Ok(real) if real != *p => format!("{} (→ {})", base, real.display()),
            _ => base,
        },
        _ => base,
    }
}

/// sh 側で解釈される語ならその種別ラベルを返す。このシェルは外部コマンドを `sh -c` に
/// 流すので、これらは PATH の実体ではなく sh が解釈する (echo/test/if など)。
fn sh_word_kind(name: &str) -> Option<&'static str> {
    const KEYWORDS: &[&str] = &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
        "in", "function", "select", "time", "{", "}", "!", "[[", "]]", "coproc",
    ];
    const BUILTINS: &[&str] = &[
        "echo", "printf", "test", "[", "read", "eval", "exec", "export", "readonly", "unset",
        "shift", "return", "break", "continue", "trap", "wait", "umask", "ulimit", "times",
        "command", "builtin", "true", "false", ":", ".", "source", "local", "declare", "typeset",
        "let", "getopts", "hash", "help", "dirs", "pushd", "popd",
    ];
    if KEYWORDS.contains(&name) {
        Some("sh keyword")
    } else if BUILTINS.contains(&name) {
        Some("sh builtin")
    } else {
        None
    }
}

/// `name: <label>[rest]` を label だけ色付きで 1 行出す (カテゴリを一目で識別できるように)。
fn print_kind(name: &str, label: &str, color: Color, rest: &str) -> io::Result<()> {
    execute!(
        stdout(),
        Print(format!("{}: ", name)),
        SetForegroundColor(color),
        Print(label.to_string()),
        ResetColor,
        Print(format!("{}\r\n", rest)),
    )
}

/// 色付きの見出し (末尾コロン込み) だけを 1 行出す (下に項目が続くセクション用)。
fn print_header(label: &str, color: Color) -> io::Result<()> {
    execute!(
        stdout(),
        SetForegroundColor(color),
        Print(label.to_string()),
        ResetColor,
        Print("\r\n"),
    )
}

/// 色付きラベル + 値を 1 行で出す (`label value`)。
fn print_field(label: &str, color: Color, value: &str) -> io::Result<()> {
    execute!(
        stdout(),
        SetForegroundColor(color),
        Print(label.to_string()),
        ResetColor,
        Print(format!(" {}\r\n", value)),
    )
}

/// 展開先 (alias/abbr の値) の先頭コマンドを PATH 解決し、実体パスを字下げして示す。
/// 出力した実体パスを返す (呼び出し側で直接 PATH 解決との重複判定に使う)。
fn print_expansion_target(to: &str) -> io::Result<Option<PathBuf>> {
    if let Some(first) = to.split_whitespace().next()
        && let Some(p) = which(first)
    {
        let disp = format_binary(&p);
        // 左右が同じ (値の先頭が既にフルパス) なら冗長なので出さない。
        // dedup 用に実体パスは返す (名前自体の PATH 解決と同一なら下の行も省く)。
        if first != disp {
            println_raw(&format!("    → {} = {}", first, disp))?;
        }
        return Ok(Some(p));
    }
    Ok(None)
}

/// 1 つの名前について、シェルが知っていることを該当する分だけ全部出す。
/// builtin / alias / abbr / PATH 上の実行ファイルは排他ではないので順に調べて出す。
fn describe_name(name: &str, ctx: &ShellContext) -> io::Result<()> {
    let mut found = false;
    // 展開先解決で既に出した実体パス。名前自体の PATH 解決と重複したら出さない。
    let mut shown_paths: Vec<PathBuf> = Vec::new();

    if find_builtin(name).is_some() {
        print_kind(name, "shell builtin", Color::Cyan, "")?;
        found = true;
    }
    if let Some(to) = ctx.aliases.get(name) {
        print_kind(name, "alias", Color::Green, &format!(" = {}", to))?;
        shown_paths.extend(print_expansion_target(to)?);
        found = true;
    }
    if let Some(to) = ctx.abbrs.get(name) {
        print_kind(name, "abbr", Color::Yellow, &format!(" = {}", to))?;
        shown_paths.extend(print_expansion_target(to)?);
        found = true;
    }
    if let Some(label) = sh_word_kind(name) {
        print_kind(name, label, Color::Cyan, " (run by sh)")?;
        found = true;
    }
    if let Some(p) = which(name) {
        // alias/abbr が同じ実体を指していた場合 (`cp='cp -i'` 等) は重複なので省く。
        if !shown_paths.contains(&p) {
            println_raw(&format!("{}: {}", name, format_binary(&p)))?;
        }
        found = true;
    }
    // 同名の環境変数があれば併せて出す (set/setenv で管理しているものも含む)。
    if let Ok(val) = std::env::var(name) {
        print_kind(name, "env", Color::Magenta, &format!(" = {}", val))?;
        found = true;
    }

    if !found {
        print_kind(name, "not found", Color::Red, "")?;
    }
    Ok(())
}

/// 引数なし: シェルが保持する情報を一括で出す。ソートして安定した並びにする。
/// セクション見出しは describe_name と同じ配色 (builtins=cyan, alias=green, abbr=yellow,
/// 構造的な項目=blue) にして走査しやすくする。
fn dump_all(ctx: &ShellContext) -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    print_field("cwd:", Color::Blue, &cwd.display().to_string())?;
    print_field("last status:", Color::Blue, &ctx.last_status.to_string())?;

    let builtins: Vec<&str> = builtin_names().collect();
    print_field(
        &format!("builtins ({}):", builtins.len()),
        Color::Cyan,
        &builtins.join(" "),
    )?;

    let mut aliases: Vec<(&String, &String)> = ctx.aliases.iter().collect();
    aliases.sort_by(|a, b| a.0.cmp(b.0));
    print_header(&format!("aliases ({}):", aliases.len()), Color::Green)?;
    for (k, v) in aliases {
        println_raw(&format!("  {} = {}", k, v))?;
    }

    let mut abbrs: Vec<(&String, &String)> = ctx.abbrs.iter().collect();
    abbrs.sort_by(|a, b| a.0.cmp(b.0));
    print_header(&format!("abbrs ({}):", abbrs.len()), Color::Yellow)?;
    for (k, v) in abbrs {
        println_raw(&format!("  {} = {}", k, v))?;
    }

    print_header(
        &format!("dir stack ({}):", ctx.dir_stack.len()),
        Color::Blue,
    )?;
    for d in &ctx.dir_stack {
        println_raw(&format!("  {}", d.display()))?;
    }

    print_header(&format!("jobs ({}):", ctx.jobs.len()), Color::Blue)?;
    for j in &ctx.jobs {
        println_raw(&format!("  [{}] {}", j.id, j.cmd))?;
    }

    print_field(
        "recent paths:",
        Color::Blue,
        &ctx.recent_paths.len().to_string(),
    )?;
    Ok(())
}

/// `type [NAME...]`: 名前について分かることを種別を問わずまとめて出す。
/// 引数なしのときはシェルが持つ情報を全部列挙する。
fn type_info(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    if args.is_empty() {
        return dump_all(ctx);
    }
    for &name in args {
        describe_name(name, ctx)?;
    }
    Ok(())
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

// ─── refresh ──────────────────────────────────────────────────────────────────

/// `~/.my_shell_paths` を compact する。追記専用ログなので実在しないパスや重複が
/// 溜まり続けるのを掃除するためのビルトイン。
///
/// dedup + 実在チェック済みの `recent_paths` (MRU 順) を一時ファイルに書き、
/// atomic rename で本体を差し替える。読み手は常に「旧の完全なファイル」か
/// 「新の完全なファイル」だけを見るので torn read が起きない。他端末は次の再描画で
/// [reload_recent_paths] がファイル縮小を検知し、全体を読み直して再同期する。
fn refresh(_args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    use std::io::Write;

    // 他端末の追記も取り込んだ最新状態にしてから書き出す。
    reload_recent_paths(ctx);

    // 実在チェックを全件やり直す。起動・リロードは stat しないので、削除された
    // パスや古い is_dir はメモリに残ったまま。掃除はこの refresh に一本化しており、
    // ここで stat し直して消えたエントリを落とし、is_dir も現在の実体に合わせて更新する。
    let before = ctx.recent_paths.len();
    ctx.recent_paths = std::mem::take(&mut ctx.recent_paths)
        .into_iter()
        .filter_map(|rp| stat_recent_path(rp.path))
        .collect();
    let removed = before - ctx.recent_paths.len();

    let path = expand_tilde(PATHS_FILE);
    let tmp = path.with_extension("tmp");

    // recent_paths は MRU 順 (先頭が直近)。ファイルは「古い順・末尾が直近」で読むため
    // (load_recent_paths が末尾優先で dedup する)、逆順に書いて直近を末尾へ置く。
    {
        let mut f = std::fs::File::create(&tmp)?;
        for rp in ctx.recent_paths.iter().rev() {
            writeln!(f, "{}", recent_line(rp))?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;

    // 自端末のオフセットを新サイズへ合わせる (書き直した分を再読み込みしない)。
    ctx.paths_file_offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    execute!(
        stdout(),
        Print(format!(
            "refreshed: {} paths ({} removed)\r\n",
            ctx.recent_paths.len(),
            removed
        ))
    )
}

// ─── exit ─────────────────────────────────────────────────────────────────────

/// シェルを終了する。`exit [status]`、引数なしは直前コマンドのステータスを引き継ぐ。
///
/// ここでは要求を [`ShellContext::exit_status`] に置くだけで、実際の終了は
/// main.rs のイベントループが行う。停止ジョブの警告や履歴保存・raw mode 解除を
/// Ctrl+D の終了経路と共通化するため (`std::process::exit` は Drop を飛ばすので使わない)。
fn exit(args: &[&str], ctx: &mut ShellContext) -> io::Result<()> {
    if args.len() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many arguments",
        ));
    }
    let status = match args.first() {
        // last_status はこの後 execute_command が上書きするが、ここで読む時点では
        // まだ直前コマンドの値。POSIX シェルと同じく引数なしはそれを引き継ぐ。
        None => ctx.last_status,
        Some(s) => s.parse::<i32>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("numeric argument required: {}", s),
            )
        })?,
    };
    ctx.exit_status = Some(status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_line_roundtrip() {
        // ディレクトリは末尾 / 付き、ファイルは無しで往復する。
        let dir = RecentPath {
            path: PathBuf::from("/home/x/proj"),
            is_dir: true,
        };
        let file = RecentPath {
            path: PathBuf::from("/home/x/proj/main.rs"),
            is_dir: false,
        };
        assert_eq!(recent_line(&dir), "/home/x/proj/");
        assert_eq!(recent_line(&file), "/home/x/proj/main.rs");

        let d = parse_recent_line(&recent_line(&dir)).unwrap();
        assert_eq!(d.path, dir.path);
        assert!(d.is_dir);
        let f = parse_recent_line(&recent_line(&file)).unwrap();
        assert_eq!(f.path, file.path);
        assert!(!f.is_dir);
    }

    #[test]
    fn parse_recent_line_edges() {
        // 空行と "/" だけの行は None。
        assert!(parse_recent_line("").is_none());
        assert!(parse_recent_line("/").is_none());
        // 末尾 / 無しはファイル扱い (旧フォーマットもここに落ちる)。
        let f = parse_recent_line("/tmp/a").unwrap();
        assert!(!f.is_dir);
        assert_eq!(f.path, PathBuf::from("/tmp/a"));
        // ルートディレクトリは "//" として記録され "/" に復元される。
        let root = RecentPath {
            path: PathBuf::from("/"),
            is_dir: true,
        };
        assert_eq!(recent_line(&root), "//");
        let r = parse_recent_line("//").unwrap();
        assert_eq!(r.path, PathBuf::from("/"));
        assert!(r.is_dir);
    }

    fn pairs(v: &[(String, String)]) -> Vec<(&str, &str)> {
        v.iter()
            .map(|(k, val)| (k.as_str(), val.as_str()))
            .collect()
    }

    #[test]
    fn parse_aliases_posix_single_line() {
        // bash 前置あり・zsh 前置なし・引用符なしが混在しても解析できる。
        let out = parse_aliases_posix("alias ll='ls -la'\ng='git'\npy=python3\n");
        assert_eq!(
            pairs(&out),
            vec![("ll", "ls -la"), ("g", "git"), ("py", "python3")]
        );
        // 値中のリテラル単一引用符 (`'\''`) と二重引用符。
        let out = parse_aliases_posix(concat!(
            r"alias say='echo '\''hi'\'''",
            "\n",
            r#"alias e="editor -w""#,
            "\n"
        ));
        assert_eq!(pairs(&out), vec![("say", "echo 'hi'"), ("e", "editor -w")]);
    }

    #[test]
    fn parse_aliases_posix_multiline() {
        // bash 実出力: 単一引用符の値が物理改行をまたぐ複数行 alias。
        let out = parse_aliases_posix("alias multi='echo one\necho two'\nalias single='ls -la'\n");
        assert_eq!(
            pairs(&out),
            vec![("multi", "echo one\necho two"), ("single", "ls -la")]
        );
    }

    #[test]
    fn parse_aliases_fish_single_line() {
        // fish 実出力: `alias NAME 'BODY'`。
        let out = parse_aliases_fish("alias ll 'ls -la'\nalias tool '/opt/bin/tool --fast'\n");
        assert_eq!(
            pairs(&out),
            vec![("ll", "ls -la"), ("tool", "/opt/bin/tool --fast")]
        );
    }

    #[test]
    fn parse_aliases_fish_multiline_best_effort() {
        // fish は複数行 alias を関数として保持し `alias` 表示で値が崩れる (先頭に名前が
        // 混じる・空白/改行がエスケープされる)。名前は取れるが値は忠実に復元できない、
        // というベストエフォート挙動を固定する。
        let out = parse_aliases_fish("alias multi multi\\ echo\\ one\\necho\\ two\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "multi");
        assert!(out[0].1.contains("echo")); // 値は取り込むが fish のエスケープ済み文字列
    }

    #[test]
    fn parse_aliases_csh_single_line() {
        // csh/tcsh 実出力: `NAME<TAB>VALUE`。
        let out = parse_aliases_csh("ll\tls -la\ntool\t/opt/bin/tool --fast\n");
        assert_eq!(
            pairs(&out),
            vec![("ll", "ls -la"), ("tool", "/opt/bin/tool --fast")]
        );
    }

    #[test]
    fn git_toplevel_in_repo() {
        // このクレート自体が git リポジトリなので、ルートが取れて cwd を含むはず。
        // (cwd を変更すると並行テストに影響するので、解決だけを検証し移動はしない)
        let root = git_toplevel().expect("should resolve repo root");
        assert!(root.is_absolute());
        let cwd = std::env::current_dir().unwrap();
        assert!(
            cwd.starts_with(&root),
            "cwd {cwd:?} should be under root {root:?}"
        );
    }

    #[test]
    fn parse_aliases_csh_multiline() {
        // csh 実出力: 2 行目以降はタブ無しの継続行。直前の値へ改行付きで連結する。
        let out = parse_aliases_csh("multi\techo one\necho two\nsingle\tls -la\n");
        assert_eq!(
            pairs(&out),
            vec![("multi", "echo one\necho two"), ("single", "ls -la")]
        );
    }
}
