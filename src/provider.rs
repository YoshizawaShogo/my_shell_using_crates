//! 補完候補プロバイダ。
//!
//! 「何を補完候補にするか」だけを担う。
//! UI (どう表示・選択するか) は `selector` モジュールが担う。

use crate::history::History;
use std::path::{Path, PathBuf};

// ─── コンテキスト ─────────────────────────────────────────────────────────────

/// プロバイダが候補生成に使う情報
pub struct CompletionContext<'a> {
    /// カーソルまでの入力 (最後のトークン or 行全体)
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

/// 実行履歴から候補を返す。同一 cwd を優先 (詳細は `History::search_completions`)。
pub struct HistoryProvider;

impl CandidateProvider for HistoryProvider {
    fn candidates(&self, ctx: &CompletionContext<'_>) -> Vec<String> {
        ctx.history.search_completions(ctx.prefix, ctx.cwd)
    }
}

// ─── FileProvider ────────────────────────────────────────────────────────────

/// カレントディレクトリ以下のファイル/ディレクトリを列挙する (Ctrl+T 用)。
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
            continue; // 隠しファイルをスキップ
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
