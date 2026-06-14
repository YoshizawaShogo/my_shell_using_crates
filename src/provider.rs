//! 補完候補プロバイダ。
//!
//! 「何を補完候補にするか」だけを担う。
//! UI (どう表示・選択するか) は `selector` モジュールが担う。

use crate::history::History;
use std::path::{Path, PathBuf};

// ─── コンテキスト ─────────────────────────────────────────────────────────────

pub struct CompletionContext<'a> {
    /// 補完対象の現在トークン (最後のトークン or 行全体)
    pub prefix: &'a str,
    pub cwd: &'a Path,
    pub history: &'a History,
    pub reg_paths: &'a [PathBuf],
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
        ctx.history.search_completions(ctx.prefix, ctx.cwd)
    }
}

// ─── CommandProvider ─────────────────────────────────────────────────────────

/// $PATH 上の実行ファイルとビルトイン名を候補にする (第1トークン補完用)。
pub struct CommandProvider;

const BUILTINS: &[&str] = &["cd", "popd", "reg_path", "abbr", "alias", "set", "setenv"];

impl CandidateProvider for CommandProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String> {
        let prefix = ctx.prefix;
        let mut cmds: Vec<String> = BUILTINS
            .iter()
            .filter(|&&b| b.starts_with(prefix))
            .map(|&b| b.to_string())
            .collect();

        if let Ok(path_var) = std::env::var("PATH") {
            for dir in path_var.split(':') {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with(prefix) && is_executable(&entry) {
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
/// `ctx.prefix` はパスつき現在トークン (例: `/tmp/fo`, `~/pro`, `fo`)。
pub struct PathProvider {
    pub dirs_only: bool,
}

impl CandidateProvider for PathProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String> {
        let (search_dir, file_prefix, display_prefix) = parse_path_token(ctx.prefix, ctx.cwd);

        let Ok(entries) = std::fs::read_dir(&search_dir) else {
            return vec![];
        };

        let mut results: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.starts_with(file_prefix) {
                    return None;
                }
                let is_dir = e.path().is_dir();
                if self.dirs_only && !is_dir {
                    return None;
                }
                let slash = if is_dir { "/" } else { "" };
                Some(format!("{}{}{}", display_prefix, name_str, slash))
            })
            .collect();

        results.sort_unstable();
        results
    }
}

/// トークンをディレクトリ部とファイルプレフィックスに分解する。
///
/// 返り値: `(探索ディレクトリ, ファイル名プレフィックス, 候補に付与する表示プレフィックス)`
///
/// 例: `"/tmp/fo"` → `(PathBuf("/tmp"), "fo", "/tmp/")`
/// 例: `"~/pro"` → `(PathBuf("~"), "pro", "~/")`
/// 例: `"fo"` → `(cwd, "fo", "")`
fn parse_path_token<'a>(token: &'a str, cwd: &Path) -> (PathBuf, &'a str, &'a str) {
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
        collect_files(ctx.cwd, self.max_depth)
    }
}

fn collect_files(root: &Path, max_depth: usize) -> Vec<String> {
    let mut out = Vec::new();
    visit(root, root, 0, max_depth, &mut out);
    out
}

fn visit(root: &Path, dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<String>) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let display = format!("./{}", rel.to_string_lossy());
        if path.is_file() {
            out.push(display);
        } else if path.is_dir() {
            visit(root, &path, depth + 1, max_depth, out);
        }
    }
}

// ─── RegPathProvider ─────────────────────────────────────────────────────────

/// `reg_path add` で登録済みのパスを候補にする (`#` トークン補完用)。
pub struct RegPathProvider;

impl CandidateProvider for RegPathProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String> {
        ctx.reg_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }
}
