//! 補完オーケストレーション。
//!
//! どの `CandidateProvider` と `selector` を組み合わせるかを決める層。
//! main.rs はこのモジュールの公開関数だけを呼ぶ。

use crate::history::History;
use crate::provider::{
    CandidateProvider, CompletionContext, FileProvider, HistoryProvider, RegPathProvider,
};
use crate::selector::{self, Selection};
use std::path::{Path, PathBuf};

// ─── Tab 補完 ─────────────────────────────────────────────────────────────────

pub struct TabContext<'a> {
    /// カーソルまでの入力全体
    pub prefix: &'a str,
    pub cwd: &'a Path,
    pub history: &'a History,
    pub reg_paths: &'a [PathBuf],
    pub lines_above_cursor: u16,
}

/// Tab 補完を実行する。
///
/// - カーソル直前のトークンが `#` 単独 → reg_path + fzf
/// - それ以外 → 履歴 + インラインメニュー
pub fn tab_complete(ctx: TabContext<'_>) -> std::io::Result<Selection> {
    if let Some(hash_start) = hash_token_start(ctx.prefix) {
        // # トークン → 登録パスを fzf で選択
        let pctx = make_pctx("", ctx.cwd, ctx.history, ctx.reg_paths);
        let cands = RegPathProvider.candidates(&pctx);
        return match selector::run_fzf(&cands, None)? {
            Selection::Chosen(s) => {
                // # の位置を選択結果で置換した行全体を返す
                let new_line = format!("{}{}", &ctx.prefix[..hash_start], s);
                Ok(Selection::Chosen(new_line))
            }
            other => Ok(other),
        };
    }

    // デフォルト: 履歴からインラインメニュー
    let pctx = make_pctx(ctx.prefix, ctx.cwd, ctx.history, ctx.reg_paths);
    let cands = HistoryProvider.candidates(&pctx);
    selector::run_menu(&cands, ctx.lines_above_cursor)
}

// ─── Ctrl+R  ─────────────────────────────────────────────────────────────────

/// 全履歴を fzf で検索する。`initial_query` は fzf の初期絞り込み文字列。
pub fn fzf_history(
    initial_query: &str,
    cwd: &Path,
    history: &History,
) -> std::io::Result<Option<String>> {
    let pctx = make_pctx("", cwd, history, &[]);
    let cands = HistoryProvider.candidates(&pctx); // prefix="" で全履歴
    match selector::run_fzf(&cands, Some(initial_query))? {
        Selection::Chosen(s) => Ok(Some(s)),
        _ => Ok(None),
    }
}

// ─── Ctrl+T ──────────────────────────────────────────────────────────────────

/// カレントディレクトリ以下のファイルを fzf で選択する。
pub fn fzf_files(cwd: &Path, history: &History) -> std::io::Result<Option<String>> {
    let pctx = make_pctx("", cwd, history, &[]);
    let cands = FileProvider::default().candidates(&pctx);
    match selector::run_fzf(&cands, None)? {
        Selection::Chosen(s) => Ok(Some(s)),
        _ => Ok(None),
    }
}

// ─── ゴーストテキスト (Ctrl+F / →) ──────────────────────────────────────────

/// カーソル以降に表示するインライン補完テキストを返す。
///
/// 履歴の最上位候補の、prefix より後ろの部分だけを返す。
/// カーソルが行末でない場合は None。
pub fn get_ghost(prefix: &str, cwd: &Path, history: &History) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    let pctx = make_pctx(prefix, cwd, history, &[]);
    HistoryProvider
        .candidates(&pctx)
        .into_iter()
        .next()
        .map(|full| full[prefix.len()..].to_string())
        .filter(|s| !s.is_empty())
}

// ─── ユーティリティ ───────────────────────────────────────────────────────────

fn make_pctx<'a>(
    prefix: &'a str,
    cwd: &'a Path,
    history: &'a History,
    reg_paths: &'a [PathBuf],
) -> CompletionContext<'a> {
    CompletionContext {
        prefix,
        cwd,
        history,
        reg_paths,
    }
}

/// カーソルまでの入力の末尾トークンが `#` 単独であれば、その開始バイト位置を返す。
fn hash_token_start(prefix: &str) -> Option<usize> {
    let token_start = prefix
        .rfind(|c: char| c.is_whitespace())
        .map(|i| i + 1)
        .unwrap_or(0);
    if prefix[token_start..] == *"#" {
        Some(token_start)
    } else {
        None
    }
}
