//! 補完オーケストレーション。
//!
//! どの `CandidateProvider` と `selector` を組み合わせるかを決める層。
//! main.rs はこのモジュールの公開関数だけを呼ぶ。

use crate::history::History;
use crate::provider::{
    CandidateProvider, CommandProvider, CompletionContext, EnvVarProvider, FileProvider,
    HistoryProvider, PathProvider, walk_dirs, walk_files,
};
use crate::selector::{self, PickerKind, Selection};
use std::path::Path;
use unicode_width::UnicodeWidthStr;

// ─── Tab 補完 ─────────────────────────────────────────────────────────────────

pub struct TabContext<'a> {
    /// カーソルまでの入力全体
    pub prefix: &'a str,
    pub cwd: &'a Path,
    pub history: &'a History,
    /// fg/bg 補完用のジョブ名 (各ジョブのコマンド先頭トークン)
    pub jobs: &'a [String],
    /// 前回の Tab 補完が完了した位置 (prefix 内バイトオフセット)。
    /// Some(n) のとき prefix[..n] が補完済み、prefix[n..] がユーザーの追加フィルタ。
    pub tab_end: Option<usize>,
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
    let token_start = token_start_pos(ctx.prefix);

    // tab_end をトークン内オフセットに変換する。
    // tab_end がトークン範囲 [token_start, token_start+token.len()] 内にあれば有効。
    let tab_end_in_token = ctx.tab_end.and_then(|te| {
        if te >= token_start && te <= token_start + token.len() {
            Some(te - token_start)
        } else {
            None
        }
    });

    let pctx = CompletionContext {
        prefix: token,
        cwd: ctx.cwd,
        history: ctx.history,
        tab_end_in_token,
    };

    let cands = match &kind {
        CompletionKind::Command => CommandProvider.candidates(&pctx),
        CompletionKind::Path { dirs_only } => PathProvider {
            dirs_only: *dirs_only,
        }
        .candidates(&pctx),
        CompletionKind::EnvVar => EnvVarProvider.candidates(&pctx),
        CompletionKind::Job => {
            // ジョブ名を前方一致で絞り、重複を除く (fg vim / bg top など)。
            let mut v: Vec<String> = ctx
                .jobs
                .iter()
                .filter(|name| name.starts_with(token))
                .cloned()
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        }
    };

    if cands.is_empty() {
        return Ok(Selection::Dismissed);
    }

    // 候補が 1 つなら即確定
    if cands.len() == 1 {
        return Ok(Selection::Chosen(finalize_choice(ctx.prefix, &cands[0])));
    }

    // 共通プレフィックスが現在トークンより長く、かつ入力が共通プレフィックスの
    // 前方一致である場合のみ延長する (次の Tab でメニュー)。
    // 部分一致(contains)候補だと common_prefix が入力と無関係になるため除外する。
    let cp = common_prefix(&cands);
    if cp.len() > token.len() && cp.to_lowercase().starts_with(&token.to_lowercase()) {
        return Ok(Selection::Chosen(replace_token(ctx.prefix, cp)));
    }

    // グリッドメニュー
    // パス補完では候補のディレクトリ部分を除いて表示する。
    // 例: token="AAA/x" のとき "AAA/XXX" → "XXX" と表示し、選択後に "AAA/" を補って確定。
    let strip_len = display_strip_len(&kind, token);
    let display_cands: Vec<String> = cands
        .iter()
        .map(|c| {
            if strip_len > 0 && c.len() > strip_len {
                c[strip_len..].to_string()
            } else {
                c.clone()
            }
        })
        .collect();

    // ユーザーが実際に入力した部分 (ディレクトリ部を除く) をハイライト用に渡す。
    let highlight = &token[strip_len..];
    // 入力行のカーソル表示位置 = "$ " (幅2) + カーソルまでの入力表示幅。
    let input_display = 2 + ctx.prefix.width() as u16;
    match selector::run_grid_menu(&display_cands, input_display, highlight)? {
        Selection::Chosen(display) => {
            let full = restore_display_choice(token, strip_len, display);
            Ok(Selection::Chosen(finalize_choice(ctx.prefix, &full)))
        }
        other => Ok(other),
    }
}

// ─── Ctrl+R ──────────────────────────────────────────────────────────────────

/// 全履歴を fzf で検索する。`initial_query` は初期絞り込み文字列。
pub fn fzf_history(initial_query: &str, history: &History) -> std::io::Result<Selection> {
    let cands = history.search_completions("");
    selector::run_fzf(&cands, Some(initial_query), PickerKind::History)
}

// ─── Ctrl+T ──────────────────────────────────────────────────────────────────

/// `root` 以下のファイル/ディレクトリをストリーミングしながら fzf で選択する。
///
/// ツリーが巨大でも列挙の途中から検索・表示でき、Ctrl+C / Esc で中断できる。
pub fn fzf_files(
    root: &Path,
    initial_query: Option<&str>,
    dirs_only: bool,
) -> std::io::Result<Selection> {
    let root = root.to_path_buf();
    let max_depth = FileProvider::default().max_depth;
    selector::run_fzf_streaming(
        move |emit| {
            if dirs_only {
                walk_dirs(&root, max_depth, emit)
            } else {
                walk_files(&root, max_depth, emit)
            }
        },
        initial_query,
        PickerKind::Files,
    )
}

// ─── Ctrl+G / Ctrl+P ─────────────────────────────────────────────────────────

/// パス一覧 (MRU 順の記録パス / ディレクトリスタック) をピッカーで選択する。
pub fn fzf_paths(
    paths: &[std::path::PathBuf],
    initial_query: Option<&str>,
    kind: PickerKind,
) -> std::io::Result<Selection> {
    let cands: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    selector::run_fzf(&cands, initial_query, kind)
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
        tab_end_in_token: None,
    };
    // 直前の 1 コマンドはパスが消えていても常に候補に出す。
    let last = history.last_cmd();
    HistoryProvider
        .candidates(&pctx)
        .into_iter()
        .find(|full| Some(full.as_str()) == last || cmd_paths_exist(full, cwd))
        .map(|full| full[prefix.len()..].to_string())
        .filter(|s| !s.is_empty())
}

/// 「対象が実在しないと意味がないコマンド」について、引数パスが実在するか確認する。
/// 実在しないものがあれば false を返し、消えたパスを指す古い履歴を候補から外す。
///
/// 検査するのは次のいずれかのときだけ。それ以外は一切検査せず true を返す
/// (`cp src dst` の dst、`mkdir new`、`vim newfile` のように実在しなくてよい引数が
/// あるコマンドを誤って外さないため)。
///
/// 1. 先頭トークンがパス指定のコマンド実行 (`./x`, `../x`, `/abs/x`, `sub/x`, `~/x`)
///    → 実行ファイル自体 (先頭トークン) の実在のみ検査する。
/// 2. 先頭トークンが読み取り専用・対象必須のコマンド ([`PATH_INPUT_CMDS`])
///    → 非フラグ引数がすべて実在するか検査する。
/// 3. 先頭トークンが `source` / `.` → 読み込むスクリプト (第 2 トークン) のみ検査する。
///
/// 共通の除外規則:
/// - リダイレクト/パイプ (`> < | &`) を含む行は、出力先を作るケースがあるので検査しない。
/// - フラグ (`-` 始まり) と、フラグの値になりうる数値のみのトークンは無視する。
/// - glob (`*?[`) や変数 (`$`) を含むトークンは展開できないのでスキップする。
pub(crate) fn cmd_paths_exist(cmd: &str, cwd: &Path) -> bool {
    let Ok(tokens) = shell_words::split(cmd) else {
        return true;
    };
    // リダイレクト/パイプ/バックグラウンドを含む複合行は、出力先ファイルを新規に作る
    // ケース (`cat > out.txt`) があるので一切検査しない。
    if tokens.iter().any(|t| t.contains(['>', '<', '|', '&'])) {
        return true;
    }
    let Some(first) = tokens.first().map(String::as_str) else {
        return true;
    };

    // ① 先頭トークンがパス指定のコマンド実行: 実行ファイル自体の実在だけを見る。
    if first.contains('/') || first.starts_with('~') {
        return match resolve_path_token(first, cwd) {
            Some(p) => p.exists(),
            None => true, // glob/変数は展開できないので検査しない
        };
    }

    // ③ source / . : 読み込むスクリプト (第 2 トークン) のみ検査する。
    if matches!(first, "source" | ".") {
        return match tokens.get(1).and_then(|t| resolve_path_token(t, cwd)) {
            Some(p) => p.exists(),
            None => true,
        };
    }

    // ② 読み取り専用・対象必須のコマンド: 非フラグ引数をすべて検査する。
    if !PATH_INPUT_CMDS.contains(&first) {
        return true;
    }
    for t in tokens.iter().skip(1) {
        if t.starts_with('-') {
            continue;
        }
        // `head -n 5 file` の `5` のようなフラグの値 (数値のみ) はパスではない。
        if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        match resolve_path_token(t, cwd) {
            Some(p) if !p.exists() => return false,
            _ => {} // glob/変数はスキップ、実在すれば継続
        }
    }
    true
}

/// 引数実在を検査する読み取り専用・対象必須コマンドの一覧 (先頭トークンで一致)。
const PATH_INPUT_CMDS: &[&str] = &[
    "ls", "cd", "less", "more", "bat", "cat", "head", "tail", "diff", "file", "stat", "wc",
];

/// トークンを実在検査用のパスに解決する。glob/変数を含み展開できない場合は `None`。
fn resolve_path_token(t: &str, cwd: &Path) -> Option<std::path::PathBuf> {
    if t.contains(['*', '?', '[', '$']) {
        return None;
    }
    let home = || std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let p = if t == "~" {
        home()
    } else if let Some(rest) = t.strip_prefix("~/") {
        home().join(rest)
    } else if t.starts_with('/') {
        std::path::PathBuf::from(t)
    } else {
        cwd.join(t)
    };
    Some(p)
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
    /// ジョブ名 (fg/bg の引数)
    Job,
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
        // 第1トークン: '/' を含む語はパス (実行ファイルをパス指定で起動)、それ以外は
        // コマンド名補完。シェルと同じく、スラッシュを含む語は PATH 検索ではなくパスとして
        // 扱う (`./x`, `../x`, `/abs/x`, `sub/x`, `~/x`)。
        if token.contains('/') {
            return CompletionKind::Path { dirs_only: false };
        }
        return CompletionKind::Command;
    }

    // 第2トークン以降: コマンド名によって補完種別を切り替える
    let cmd = prefix[..token_start]
        .split_whitespace()
        .next()
        .unwrap_or("");
    match cmd {
        "fg" | "bg" => CompletionKind::Job,
        "cd" => CompletionKind::Path { dirs_only: true },
        _ => CompletionKind::Path { dirs_only: false },
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

/// 候補を確定して入力行を作る。ディレクトリ (末尾 `/`) はさらに潜れるよう空白を
/// 付けず、ファイル/コマンドは次のトークンへ進めるよう末尾に空白を足す。
fn finalize_choice(prefix: &str, choice: &str) -> String {
    let base = replace_token(prefix, choice);
    if choice.ends_with('/') {
        base
    } else {
        base + " "
    }
}

/// グリッド表示で候補から隠す現在トークン内のディレクトリ prefix 長。
fn display_strip_len(kind: &CompletionKind, token: &str) -> usize {
    if matches!(kind, CompletionKind::Path { .. }) {
        token.rfind('/').map(|i| i + 1).unwrap_or(0)
    } else {
        0
    }
}

/// グリッドで短縮表示された候補を、入力行へ挿入するフル候補に戻す。
fn restore_display_choice(token: &str, strip_len: usize, display: String) -> String {
    if strip_len > 0 {
        format!("{}{}", &token[..strip_len], display)
    } else {
        display
    }
}

/// 候補列の最長共通プレフィックスを返す (大小無視で比較し、綴りは先頭候補のものを使う)。
///
/// 大小無視にするのは `PathProvider` / `CommandProvider` が大小無視で候補を集めるため。
/// 区別すると `Makefile` と `makeshift` のような候補で延長が効かなくなる。
fn common_prefix(candidates: &[String]) -> &str {
    let Some(first) = candidates.first() else {
        return "";
    };
    let mut end = first.len();
    for cand in candidates.iter().skip(1) {
        end = first[..end]
            .chars()
            .zip(cand.chars())
            .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
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
    fn display_strip_len_path_uses_last_slash() {
        let kind = CompletionKind::Path { dirs_only: false };
        assert_eq!(display_strip_len(&kind, "src/"), 4);
        assert_eq!(display_strip_len(&kind, "src/ma"), 4);
        assert_eq!(display_strip_len(&kind, "src/nested/ma"), 11);
        assert_eq!(display_strip_len(&kind, "main"), 0);
    }

    #[test]
    fn display_strip_len_non_path_is_zero() {
        assert_eq!(display_strip_len(&CompletionKind::EnvVar, "$FOO/ba"), 0);
        assert_eq!(display_strip_len(&CompletionKind::Command, "bin/ba"), 0);
        assert_eq!(display_strip_len(&CompletionKind::Job, "job/name"), 0);
    }

    #[test]
    fn restore_display_choice_uses_directory_prefix_once() {
        assert_eq!(
            restore_display_choice("src/ma", 4, "main.rs".to_string()),
            "src/main.rs"
        );
        assert_eq!(
            restore_display_choice("src/", 4, "main.rs".to_string()),
            "src/main.rs"
        );
        assert_eq!(
            restore_display_choice("ma", 0, "main.rs".to_string()),
            "main.rs"
        );
    }

    #[test]
    fn classify_first_token() {
        // '/' を含む第1トークンはパス補完 (../ や sub/ も含む)
        assert!(matches!(classify("../"), CompletionKind::Path { .. }));
        assert!(matches!(classify("../sr"), CompletionKind::Path { .. }));
        assert!(matches!(classify("./b"), CompletionKind::Path { .. }));
        assert!(matches!(classify("sub/x"), CompletionKind::Path { .. }));
        assert!(matches!(classify("/abs/x"), CompletionKind::Path { .. }));
        // '/' を含まない語はコマンド名補完
        assert!(matches!(classify("ls"), CompletionKind::Command));
        assert!(matches!(classify(".."), CompletionKind::Command));
        // $ は環境変数
        assert!(matches!(classify("$HO"), CompletionKind::EnvVar));
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

    #[test]
    fn common_prefix_case_insensitive() {
        // 大小が違っても共通接頭辞を検出し、綴りは先頭候補のものを使う。
        let v = vec!["Makefile".to_string(), "makeshift".to_string()];
        assert_eq!(common_prefix(&v), "Make");
        let v2 = vec!["README".to_string(), "readme.md".to_string()];
        assert_eq!(common_prefix(&v2), "README");
    }

    #[test]
    fn finalize_dir_vs_file() {
        // ディレクトリ候補 (末尾 /) は空白を付けず、ファイルは空白を足す。
        assert_eq!(finalize_choice("cd sr", "src/"), "cd src/");
        assert_eq!(finalize_choice("cat ma", "main.rs"), "cat main.rs ");
    }

    #[test]
    fn cmd_paths_exist_lscd_bare_arg() {
        let base = std::env::temp_dir();
        let sub = base.join(format!("cmd_paths_exist_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&sub);
        let name = sub.file_name().unwrap().to_string_lossy().to_string();

        // ls/cd の裸の相対引数を検査する
        assert!(cmd_paths_exist("ls", &base)); // 引数なし
        assert!(cmd_paths_exist("ls -la", &base)); // フラグのみ
        assert!(cmd_paths_exist(&format!("cd {}", name), &base));
        assert!(!cmd_paths_exist("cd no_such_dir_xyz", &base));
        assert!(!cmd_paths_exist("ls -la no_such_dir_xyz", &base));

        // glob / 変数はスキップ (検査せず true 扱い)
        assert!(cmd_paths_exist("ls *.rs", &base));
        assert!(cmd_paths_exist("ls $HOME", &base));

        // 出力を作るコマンドは一切検査しない (cp の dst、mkdir、エディタの新規ファイル)
        assert!(cmd_paths_exist("cp a.txt /no/such/dir/b.txt", &base));
        assert!(cmd_paths_exist("mkdir ./new_dir_xyz", &base));
        assert!(cmd_paths_exist("vim ~/no_such_file_xyz", &base));

        // ② 読み取り専用コマンドは非フラグ引数の実在を検査する
        assert!(!cmd_paths_exist("cat no_such_file_xyz", &base));
        assert!(!cmd_paths_exist("less no_such_file_xyz", &base));
        assert!(cmd_paths_exist(&format!("cat {}", name), &base));
        // フラグの値 (数値) はパスとして扱わない
        assert!(!cmd_paths_exist("head -n 5 no_such_file_xyz", &base));
        assert!(cmd_paths_exist(&format!("head -n 5 {}", name), &base));
        // リダイレクトを含む行は出力先を作りうるので検査しない
        assert!(cmd_paths_exist("cat foo > no_such_file_xyz", &base));

        // ① 先頭トークンがパス指定のコマンド実行: 実行ファイル自体を検査する
        assert!(!cmd_paths_exist("./no_such_script_xyz.sh", &base));
        assert!(!cmd_paths_exist("no_such_dir_xyz/run.sh --flag", &base));
        assert!(!cmd_paths_exist("/no/such/abs/script_xyz.sh", &base));

        // ③ source / . : 読み込むスクリプトのみ検査する
        assert!(!cmd_paths_exist("source no_such_env_xyz.sh", &base));
        assert!(!cmd_paths_exist(". no_such_env_xyz.sh", &base));

        let _ = std::fs::remove_dir_all(&sub);
    }
}
