//! 補完オーケストレーション。
//!
//! どの `CandidateProvider` と `selector` を組み合わせるかを決める層。
//! main.rs はこのモジュールの公開関数だけを呼ぶ。

use crate::history::History;
use crate::provider::{
    CandidateProvider, CommandProvider, CompletionContext, EnvVarProvider, FileProvider,
    HistoryProvider, PathProvider, walk_dirs, walk_files,
};
use crate::selector::{self, Selection};
use std::path::Path;

// ─── Tab 補完 ─────────────────────────────────────────────────────────────────

pub struct TabContext<'a> {
    /// カーソルまでの入力全体
    pub prefix: &'a str,
    pub cwd: &'a Path,
    pub history: &'a History,
    pub lines_above_cursor: u16,
}

/// Tab 補完を実行する。
///
/// 1. `classify` でトークン位置・種別を判定
/// 2. 候補が 1 つ → 即確定
/// 3. 候補の共通プレフィックスが現在トークンより長い → その分だけ補完して返す
/// 4. 複数 → グリッドメニュー (RegPath のみ skim)
pub fn tab_complete(ctx: TabContext<'_>) -> std::io::Result<Selection> {
    let kind = classify(ctx.prefix);
    let token = current_token(ctx.prefix);

    let pctx = CompletionContext {
        prefix: token,
        cwd: ctx.cwd,
        history: ctx.history,
    };

    let cands = match &kind {
        CompletionKind::Command => CommandProvider.candidates(&pctx),
        CompletionKind::Path { dirs_only } => PathProvider {
            dirs_only: *dirs_only,
        }
        .candidates(&pctx),
        CompletionKind::EnvVar => EnvVarProvider.candidates(&pctx),
    };

    if cands.is_empty() {
        return Ok(Selection::Dismissed);
    }

    // 候補が 1 つなら即確定
    if cands.len() == 1 {
        return Ok(Selection::Chosen(replace_token(ctx.prefix, &cands[0])));
    }

    // 共通プレフィックスが現在トークンより長ければ延長して返す (次の Tab でメニュー)
    let cp = common_prefix(&cands);
    if cp.len() > token.len() {
        return Ok(Selection::Chosen(replace_token(ctx.prefix, cp)));
    }

    // グリッドメニュー
    match selector::run_grid_menu(&cands, ctx.lines_above_cursor)? {
        Selection::Chosen(s) => Ok(Selection::Chosen(replace_token(ctx.prefix, &s))),
        other => Ok(other),
    }
}

// ─── Ctrl+R ──────────────────────────────────────────────────────────────────

/// 全履歴を skim で検索する。`initial_query` は skim の初期絞り込み文字列。
pub fn fzf_history(
    initial_query: &str,
    cwd: &Path,
    history: &History,
) -> std::io::Result<Option<String>> {
    let pctx = CompletionContext {
        prefix: "",
        cwd,
        history,
    };
    let cands = HistoryProvider.candidates(&pctx);
    match selector::run_fzf(&cands, Some(initial_query))? {
        Selection::Chosen(s) => Ok(Some(s)),
        _ => Ok(None),
    }
}

// ─── Ctrl+T ──────────────────────────────────────────────────────────────────

/// `root` 以下のファイル/ディレクトリをストリーミングしながら fzf で選択する。
///
/// ツリーが巨大でも列挙の途中から検索・表示でき、Ctrl+C / Esc で中断できる。
pub fn fzf_files(
    root: &Path,
    initial_query: Option<&str>,
    dirs_only: bool,
) -> std::io::Result<Option<String>> {
    let root = root.to_path_buf();
    let max_depth = FileProvider::default().max_depth;
    let sel = selector::run_fzf_streaming(
        move |emit| {
            if dirs_only {
                walk_dirs(&root, max_depth, emit)
            } else {
                walk_files(&root, max_depth, emit)
            }
        },
        initial_query,
    )?;
    match sel {
        Selection::Chosen(s) => Ok(Some(s)),
        _ => Ok(None),
    }
}

// ─── Ctrl+G ──────────────────────────────────────────────────────────────────

/// 登録済みパスを skim で選択する。
pub fn fzf_reg_paths(reg_paths: &[std::path::PathBuf]) -> std::io::Result<Option<String>> {
    let cands: Vec<String> = reg_paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    match selector::run_fzf(&cands, None)? {
        Selection::Chosen(s) => Ok(Some(s)),
        _ => Ok(None),
    }
}

// ─── ゴーストテキスト ─────────────────────────────────────────────────────────

/// カーソル以降に表示するインライン補完テキストを返す (行末のときのみ)。
pub fn get_ghost(prefix: &str, cwd: &Path, history: &History) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    let pctx = CompletionContext {
        prefix,
        cwd,
        history,
    };
    HistoryProvider
        .candidates(&pctx)
        .into_iter()
        .next()
        .map(|full| full[prefix.len()..].to_string())
        .filter(|s| !s.is_empty())
}

// ─── 補完種別の分類 ───────────────────────────────────────────────────────────

#[derive(Debug)]
enum CompletionKind {
    /// 第1トークン (コマンド名補完)
    Command,
    /// ファイル/ディレクトリパス補完
    Path { dirs_only: bool },
    /// 環境変数 ($VAR)
    EnvVar,
}

/// カーソル直前の入力状態から補完種別を判定する。
fn classify(prefix: &str) -> CompletionKind {
    let token_start = token_start_pos(prefix);
    let token = &prefix[token_start..];

    // $VAR: どの位置でも環境変数補完
    if token.starts_with('$') {
        return CompletionKind::EnvVar;
    }

    let has_prev = token_start > 0
        && prefix[..token_start]
            .chars()
            .any(|c: char| !c.is_whitespace());

    if !has_prev {
        // 第1トークン: パス風でなければコマンド補完
        if token.starts_with('/') || token.starts_with("./") || token.starts_with("~/") {
            return CompletionKind::Path { dirs_only: false };
        }
        return CompletionKind::Command;
    }

    // 第2トークン以降: コマンド名によってファイル/ディレクトリを切り替え
    let cmd = prefix[..token_start]
        .split_whitespace()
        .next()
        .unwrap_or("");
    CompletionKind::Path {
        dirs_only: cmd == "cd",
    }
}

/// `prefix` の最後のホワイトスペースの次のバイト位置 (= 現在トークンの開始位置)。
fn token_start_pos(prefix: &str) -> usize {
    prefix
        .rfind(|c: char| c.is_whitespace())
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// カーソル直前の現在トークン文字列。
fn current_token(prefix: &str) -> &str {
    &prefix[token_start_pos(prefix)..]
}

// ─── ユーティリティ ───────────────────────────────────────────────────────────

/// `prefix` の末尾トークンを `new_token` で置き換えた行全体を返す。
fn replace_token(prefix: &str, new_token: &str) -> String {
    let start = token_start_pos(prefix);
    format!("{}{}", &prefix[..start], new_token)
}

/// 候補列の最長共通プレフィックスを返す。
fn common_prefix(candidates: &[String]) -> &str {
    let Some(first) = candidates.first() else {
        return "";
    };
    let mut end = first.len();
    for cand in candidates.iter().skip(1) {
        end = first[..end]
            .chars()
            .zip(cand.chars())
            .take_while(|(a, b)| a == b)
            .fold(0, |acc, (c, _)| acc + c.len_utf8());
    }
    &first[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_start() {
        assert_eq!(token_start_pos("git comm"), 4);
        assert_eq!(token_start_pos("git"), 0);
        assert_eq!(token_start_pos("a b c"), 4);
    }

    #[test]
    fn current_token_is_last() {
        assert_eq!(current_token("git comm"), "comm");
        assert_eq!(current_token("ls"), "ls");
    }

    #[test]
    fn replace_last_token() {
        assert_eq!(replace_token("git comm", "commit"), "git commit");
        assert_eq!(replace_token("ls", "ls"), "ls");
    }

    #[test]
    fn common_prefix_basic() {
        let two = vec!["commit".to_string(), "config".to_string()];
        assert_eq!(common_prefix(&two), "co");
        let one = vec!["abc".to_string()];
        assert_eq!(common_prefix(&one), "abc");
        let none = vec!["x".to_string(), "y".to_string()];
        assert_eq!(common_prefix(&none), "");
    }
}
