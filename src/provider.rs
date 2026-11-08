//! 補完候補プロバイダ。
//!
//! 「何を補完候補にするか」だけを担う。
//! UI (どう表示・選択するか) は `selector` モジュールが担う。

use crate::builtin::builtin_names;
use crate::history::History;
use std::path::{Path, PathBuf};

// ─── コンテキスト ─────────────────────────────────────────────────────────────

pub struct CompletionContext<'a> {
    /// 補完対象の現在トークン (最後のトークン or 行全体)
    pub prefix: &'a str,
    pub cwd: &'a Path,
    pub history: &'a History,
    /// 前回の Tab 補完で挿入されたテキストがトークン内のどこで終わるか (バイトオフセット)。
    /// Some(n) のとき prefix[..n] が補完済み部分、prefix[n..] がユーザー手入力のフィルタ。
    pub tab_end_in_token: Option<usize>,
}

// ─── トレイト ─────────────────────────────────────────────────────────────────

pub trait CandidateProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String>;
}

// ─── HistoryProvider ─────────────────────────────────────────────────────────

/// 実行履歴から候補を返す (ghost text・Ctrl+R 用)。
pub struct HistoryProvider;

impl CandidateProvider for HistoryProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String> {
        ctx.history.search_completions(ctx.prefix)
    }
}

// ─── CommandProvider ─────────────────────────────────────────────────────────

/// $PATH 上の実行ファイルとビルトイン名を候補にする (第1トークン補完用)。
pub struct CommandProvider;

impl CandidateProvider for CommandProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String> {
        // 照合は大小無視 (PathProvider と揃える)。挿入するのは実体の綴り。
        let prefix = ctx.prefix.to_lowercase();
        let mut cmds: Vec<String> = builtin_names()
            .filter(|name| name.to_lowercase().starts_with(&prefix))
            .map(|name| name.to_string())
            .collect();

        if let Ok(path_var) = std::env::var("PATH") {
            for dir in path_var.split(':') {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.to_lowercase().starts_with(&prefix) && is_executable(&entry) {
                        cmds.push(name_str.into_owned());
                    }
                }
            }
        }

        cmds.sort_unstable();
        cmds.dedup();
        cmds
    }
}

#[cfg(unix)]
fn is_executable(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::PermissionsExt;
    entry
        .metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::DirEntry) -> bool {
    true
}

// ─── PathProvider ────────────────────────────────────────────────────────────

/// ファイル/ディレクトリを候補にする。`dirs_only = true` のとき `cd` 向けにディレクトリのみ。
///
/// `ctx.prefix` はパスつき現在トークン (例: `/tmp/fo`, `~/pro`, `fo`)。ファイル名部分は
/// 部分一致 (大小無視) で照合する。
///
/// ディレクトリ部にグロブ (`*` `?` `[...]`) があるとき (例: `./*/logs/`) は、一致する
/// ディレクトリをすべて展開し、その中身を名前で重複を除いて候補にする。候補の頭には
/// トークンのディレクトリ部をそのまま付けるので、`*` を保ったまま階層を進められる。
pub struct PathProvider {
    pub dirs_only: bool,
}

impl CandidateProvider for PathProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String> {
        let (search_dir, file_prefix, display_prefix) = parse_path_token(ctx.prefix, ctx.cwd);

        // 照合はすべて大小無視。`needle` はトークンのファイル名部分全体。
        let needle = file_prefix.to_lowercase();

        // tab_end フィルタ (前回 Tab で base まで補完済み、以降が手入力フィルタ) の分割。
        // ディレクトリ部分 (display_prefix) は tab_end の計算に含まれるので引く。
        // これは「前方一致が 0 件のとき」のフォールバック照合にのみ使う:
        //   base を前方一致しつつ filter を部分一致する候補を拾う。
        let dir_prefix_len = display_prefix.len();
        let (base_lower, filter_lower) = match ctx.tab_end_in_token {
            Some(te) if te > dir_prefix_len && te <= dir_prefix_len + file_prefix.len() => {
                let split = te - dir_prefix_len;
                (
                    file_prefix[..split].to_lowercase(),
                    file_prefix[split..].to_lowercase(),
                )
            }
            _ => (String::new(), String::new()),
        };
        let has_filter = !filter_lower.is_empty();

        // 前方一致 (トークン全体を starts_with) を最優先し、無いときだけフォールバック
        // (tab_end フィルタ時は base 前方一致 AND filter 部分一致、それ以外は素の部分一致)
        // へ落とす。tab_end フィルタ経路でも真の前方一致候補が部分一致に埋もれないよう、
        // 両経路でこの優先規則を共通化している。
        let mut prefix_matches: Vec<String> = Vec::new();
        let mut fallback_matches: Vec<String> = Vec::new();
        // グロブで複数のディレクトリを読むとき、同じ名前の候補を 1 つにまとめる。
        let mut seen = std::collections::HashSet::new();

        let entries = expand_glob_dirs(&search_dir)
            .into_iter()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flat_map(|rd| rd.flatten());
        for e in entries {
            let name = e.file_name();
            let name_str = name.to_string_lossy();
            let name_lower = name_str.to_lowercase();

            let is_prefix = name_lower.starts_with(&needle);
            let is_fallback = if has_filter {
                name_lower.starts_with(&base_lower) && name_lower.contains(&filter_lower)
            } else {
                name_lower.contains(&needle)
            };
            if !is_prefix && !is_fallback {
                continue;
            }

            let is_dir = e.path().is_dir();
            if self.dirs_only && !is_dir {
                continue;
            }
            let slash = if is_dir { "/" } else { "" };
            let candidate = format!("{}{}{}", display_prefix, name_str, slash);
            if !seen.insert(candidate.clone()) {
                continue;
            }
            if is_prefix {
                prefix_matches.push(candidate);
            } else {
                fallback_matches.push(candidate);
            }
        }

        prefix_matches.sort_unstable();
        if prefix_matches.is_empty() {
            fallback_matches.sort_unstable();
            fallback_matches
        } else {
            prefix_matches
        }
    }
}

/// グロブで展開するディレクトリ数の上限 (`/*/*/` のような広すぎる指定で固まらないため)。
const MAX_GLOB_DIRS: usize = 512;

/// グロブの記号 (`*` `?` `[`) を含むか。
fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// パスの各要素にあるグロブを展開し、一致するディレクトリをすべて返す。グロブを
/// 含まなければ `dir` だけを返す (存在確認は呼び出し側の `read_dir` に任せる)。
/// bash と同じく、`.` で始まらないパターンは隠しファイル (`.` 始まり) に一致させない。
fn expand_glob_dirs(dir: &Path) -> Vec<PathBuf> {
    if !has_glob(&dir.to_string_lossy()) {
        return vec![dir.to_path_buf()];
    }
    let mut dirs = vec![PathBuf::new()];
    for comp in dir.components() {
        let comp = comp.as_os_str().to_string_lossy();
        if has_glob(&comp) {
            let mut next = Vec::new();
            for d in &dirs {
                let Ok(rd) = std::fs::read_dir(d) else {
                    continue;
                };
                for e in rd.flatten() {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if glob_match(&comp, &name) && e.path().is_dir() {
                        next.push(d.join(&*name));
                    }
                }
            }
            next.sort();
            next.truncate(MAX_GLOB_DIRS);
            dirs = next;
        } else {
            for d in &mut dirs {
                d.push(&*comp);
            }
        }
        if dirs.is_empty() {
            break;
        }
    }
    dirs
}

/// `name` がグロブのパターン `pat` に一致するか。`*` (任意の文字列)・`?` (任意の 1 文字)・
/// `[abc]` `[a-z]` `[!x]` `[^x]` (文字クラス) に対応する。`.` で始まる名前には、
/// パターンも `.` で始まるときだけ一致する。
fn glob_match(pat: &str, name: &str) -> bool {
    if name.starts_with('.') && !pat.starts_with('.') {
        return false;
    }
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    glob_match_chars(&p, &n)
}

fn glob_match_chars(p: &[char], n: &[char]) -> bool {
    match p.first() {
        None => n.is_empty(),
        Some('*') => (0..=n.len()).any(|i| glob_match_chars(&p[1..], &n[i..])),
        Some('?') => !n.is_empty() && glob_match_chars(&p[1..], &n[1..]),
        Some('[') => {
            let Some(&c) = n.first() else {
                return false;
            };
            match match_char_class(p, c) {
                Some((len, true)) => glob_match_chars(&p[len..], &n[1..]),
                Some((_, false)) => false,
                // 閉じ `]` が無ければ `[` はただの文字
                None => c == '[' && glob_match_chars(&p[1..], &n[1..]),
            }
        }
        Some(&c) => n.first() == Some(&c) && glob_match_chars(&p[1..], &n[1..]),
    }
}

/// `p` 先頭の文字クラス (`[...]`) を読み、(消費した長さ, `c` が一致したか) を返す。
/// 閉じ `]` が無ければ `None`。先頭 (否定の `!`/`^` の直後) の `]` は文字として扱う。
fn match_char_class(p: &[char], c: char) -> Option<(usize, bool)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let start = i;
    let mut matched = false;
    while i < p.len() {
        if p[i] == ']' && i > start {
            return Some((i + 1, matched != negate));
        }
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            matched |= p[i] <= c && c <= p[i + 2];
            i += 3;
        } else {
            matched |= p[i] == c;
            i += 1;
        }
    }
    None
}

/// トークンをディレクトリ部とファイルプレフィックスに分解する。
///
/// 返り値: `(探索ディレクトリ, ファイル名プレフィックス, 候補に付与する表示プレフィックス)`
///
/// 例: `"/tmp/fo"` → `(PathBuf("/tmp"), "fo", "/tmp/")`
/// 例: `"~/pro"` → `(PathBuf("~"), "pro", "~/")`
/// 例: `"fo"` → `(cwd, "fo", "")`
pub(crate) fn parse_path_token<'a>(token: &'a str, cwd: &Path) -> (PathBuf, &'a str, &'a str) {
    match token.rfind('/') {
        Some(slash) => {
            let dir_part = &token[..=slash]; // "/tmp/" や "~/"
            let file_prefix = &token[slash + 1..];

            let search_dir = if let Some(rest) = dir_part.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                PathBuf::from(format!("{}/{}", home, rest))
            } else if dir_part.starts_with('/') {
                PathBuf::from(dir_part)
            } else {
                cwd.join(dir_part)
            };

            (search_dir, file_prefix, dir_part)
        }
        None => (cwd.to_path_buf(), token, ""),
    }
}

// ─── EnvVarProvider ──────────────────────────────────────────────────────────

/// `$` で始まる現在トークンに対して環境変数名を候補にする。
pub struct EnvVarProvider;

impl CandidateProvider for EnvVarProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String> {
        let var_prefix = ctx.prefix.strip_prefix('$').unwrap_or(ctx.prefix);
        let mut vars: Vec<String> = std::env::vars()
            .filter(|(k, _)| k.starts_with(var_prefix))
            .map(|(k, _)| format!("${}", k))
            .collect();
        vars.sort_unstable();
        vars
    }
}

// ─── FileProvider (Ctrl+T 用) ─────────────────────────────────────────────────

/// カレントディレクトリ以下のファイル/ディレクトリを再帰列挙する (Ctrl+T 用)。
pub struct FileProvider {
    pub max_depth: usize,
}

impl Default for FileProvider {
    fn default() -> Self {
        Self { max_depth: 5 }
    }
}

impl CandidateProvider for FileProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String> {
        let mut out = Vec::new();
        walk_files(ctx.cwd, self.max_depth, &mut |s| {
            out.push(s);
            true
        });
        out
    }
}

/// `root` 以下のファイル/ディレクトリを並列列挙し、各エントリ
/// (例: `./src/`, `./src/main.rs`) を `emit` へ逐次渡す。
///
/// `emit` が false を返したら走査スレッドへ中断を伝える。
/// `ignore` クレートにより .gitignore / 隠しファイルを自動除外し、
/// 複数スレッドで同時走査するため素の readdir より大幅に速い。
pub fn walk_files(root: &Path, max_depth: usize, emit: &mut dyn FnMut(String) -> bool) {
    walk_parallel(root, max_depth, false, emit);
}

/// `walk_files` のディレクトリのみ版 (cd の Ctrl+T 補完用)。
pub fn walk_dirs(root: &Path, max_depth: usize, emit: &mut dyn FnMut(String) -> bool) {
    walk_parallel(root, max_depth, true, emit);
}

fn walk_parallel(
    root: &Path,
    max_depth: usize,
    dirs_only: bool,
    emit: &mut dyn FnMut(String) -> bool,
) {
    use ignore::{WalkBuilder, WalkState};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<String>();
    let root = root.to_path_buf();

    std::thread::spawn(move || {
        let sent = Arc::new(AtomicUsize::new(0));

        // walker を走らせる共通クロージャ。walker はスレッド内で同期的に完了する。
        let run_walker =
            |walker: ignore::WalkParallel, tx: &mpsc::Sender<String>, sent: &Arc<AtomicUsize>| {
                walker.run(|| {
                    let tx = tx.clone();
                    let root = root.clone();
                    let sent = sent.clone();
                    Box::new(move |result| {
                        let Ok(entry) = result else {
                            return WalkState::Continue;
                        };
                        if entry.depth() == 0 {
                            return WalkState::Continue;
                        }
                        let Some(ft) = entry.file_type() else {
                            return WalkState::Continue;
                        };
                        if dirs_only && !ft.is_dir() {
                            return WalkState::Continue;
                        }
                        if !ft.is_file() && !ft.is_dir() {
                            return WalkState::Continue;
                        }
                        let path = entry.path();
                        let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy();
                        let s = if ft.is_dir() {
                            format!("./{}/", rel)
                        } else {
                            format!("./{}", rel)
                        };
                        if tx.send(s).is_err() {
                            WalkState::Quit
                        } else {
                            sent.fetch_add(1, Ordering::Relaxed);
                            WalkState::Continue
                        }
                    })
                });
            };

        // Phase 1: 通常深さ (max_depth) で走査
        let w1 = WalkBuilder::new(&root)
            .max_depth(Some(max_depth))
            .build_parallel();
        run_walker(w1, &tx, &sent);

        // Phase 2: 常により深く (max_depth+1 〜 max_depth*2) を追加走査する。
        // スコアリング側で浅い結果が上位になるよう調整してあるので、
        // Phase 2 の結果が Phase 1 結果を押しのけることはない。
        let w2 = WalkBuilder::new(&root)
            .min_depth(Some(max_depth + 1))
            .max_depth(Some(max_depth * 2))
            .build_parallel();
        run_walker(w2, &tx, &sent);
        // tx がここで drop → rx 側のチャネルが閉じる
    });

    for s in rx {
        if !emit(s) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_token_relative() {
        let cwd = Path::new("/home/x");
        let (dir, file, disp) = parse_path_token("fo", cwd);
        assert_eq!(dir, PathBuf::from("/home/x"));
        assert_eq!(file, "fo");
        assert_eq!(disp, "");
    }

    #[test]
    fn path_token_absolute() {
        let cwd = Path::new("/home/x");
        let (dir, file, disp) = parse_path_token("/tmp/fo", cwd);
        assert_eq!(dir, PathBuf::from("/tmp/"));
        assert_eq!(file, "fo");
        assert_eq!(disp, "/tmp/");
    }

    #[test]
    fn path_token_subdir() {
        let cwd = Path::new("/home/x");
        let (dir, file, disp) = parse_path_token("sub/fo", cwd);
        assert_eq!(dir, PathBuf::from("/home/x/sub/"));
        assert_eq!(file, "fo");
        assert_eq!(disp, "sub/");
    }

    #[test]
    fn glob_match_patterns() {
        assert!(glob_match("*", "logs"));
        assert!(glob_match("a*z", "abcz") && !glob_match("a*z", "abc"));
        assert!(glob_match("?b", "ab") && !glob_match("?b", "b"));
        assert!(glob_match("[ab]x", "bx") && !glob_match("[ab]x", "cx"));
        assert!(glob_match("[a-c]1", "b1") && !glob_match("[a-c]1", "d1"));
        assert!(glob_match("[!a]1", "b1") && !glob_match("[!a]1", "a1"));
        assert!(glob_match("[]]", "]")); // 先頭の ] は文字
        assert!(glob_match("[x", "[x")); // 閉じ ] が無い [ は文字
        // 隠しファイルにはパターンも . で始まるときだけ一致 (bash と同じ)
        assert!(!glob_match("*", ".git") && glob_match(".*", ".git"));
    }

    /// `./*/logs/` のようにディレクトリ部にグロブがあるとき、一致する全ディレクトリの
    /// 中身を重複なく候補にし、`*` を保ったまま返す。
    #[test]
    fn path_candidates_expand_glob_directories() {
        let dir = std::env::temp_dir().join(format!("prov_glob_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (sub, files) in [
            ("a/logs", &["x.log", "common.log"][..]),
            ("b/logs", &["y.log", "common.log"][..]),
            (".hidden/logs", &["h.log"][..]),
        ] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            for f in files {
                std::fs::write(dir.join(sub).join(f), b"").unwrap();
            }
        }
        std::fs::create_dir_all(dir.join("c")).unwrap(); // logs が無いディレクトリ
        std::fs::create_dir_all(dir.join("a/logs/sub")).unwrap();
        let history = History::load();
        let cands = |prefix: &str, dirs_only: bool| {
            let ctx = CompletionContext {
                prefix,
                cwd: &dir,
                history: &history,
                tab_end_in_token: None,
            };
            PathProvider { dirs_only }.candidates(&ctx)
        };
        assert_eq!(
            cands("./*/logs/", false),
            vec![
                "./*/logs/common.log",
                "./*/logs/sub/",
                "./*/logs/x.log",
                "./*/logs/y.log"
            ]
        );
        assert_eq!(cands("./*/logs/co", false), vec!["./*/logs/common.log"]);
        assert_eq!(cands("./[ab]/logs/", true), vec!["./[ab]/logs/sub/"]);
        // 絶対パスでも同じ
        let abs = format!("{}/*/logs/y", dir.display());
        assert_eq!(
            cands(&abs, false),
            vec![format!("{}/*/logs/y.log", dir.display())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// tab_end フィルタ経路でも「トークン全体の前方一致」を最優先する。
    /// `datafeed` (前方一致) が `datafile` (`e` を含むだけの部分一致) に埋もれず
    /// 単独候補として返ることを確認する (前方一致優先バグの回帰防止)。
    #[test]
    fn path_filter_branch_prefers_prefix() {
        let dir = std::env::temp_dir().join(format!("prov_prefix_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["datafeed.json", "datafile.csv", "metadata.txt"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        let history = History::load();

        // base="dataf" まで補完済み (tab_end=5)、手入力フィルタ "e" → token "datafe"
        let ctx = CompletionContext {
            prefix: "datafe",
            cwd: &dir,
            history: &history,
            tab_end_in_token: Some(5),
        };
        let cands = PathProvider { dirs_only: false }.candidates(&ctx);
        assert_eq!(cands, vec!["datafeed.json".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
