//! コマンド実行。

use crate::builtin::{ShellContext, find_builtin};
use crossterm::{
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
};
use std::collections::HashMap;
use std::io::{self, stdout};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// `interactive`: true のとき実行前に `\r\n` を出力してプロンプトと出力を分離する。
/// load_rc からは false で呼ぶ (起動時に空行が大量発生するのを防ぐ)。
///
/// 外部コマンドはトークン分割せず、先頭トークンの abbr 展開だけ行って残りは
/// そのまま `bash -c` に渡す。alias はすべて `bash` の `alias` 定義として前置し、
/// パイプ後 (`ls | grep`) を含むあらゆるコマンド位置で展開させる。
/// これによりパイプ・リダイレクト・グロブ・`$VAR`・クォートも `bash` が解釈する。
/// 単一行・複数行とも bash に統一している (`[[ ]]`・配列などが 1 行でも貼り付けでも
/// 同じように動くようにするため。`sh`=dash では bash 拡張が動かず挙動が食い違う)。
pub fn execute_command(cmd: &str, ctx: &mut ShellContext, interactive: bool) -> io::Result<()> {
    if interactive {
        execute!(stdout(), Print("\r\n"))?;
    }

    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    // 複数行 (ペーストされたスクリプト) は abbr/ドット展開/ビルトイン判定をせず、改行を
    // 保ったまま bash へそのまま渡す ("そのまま入力→そのまま実行")。
    if trimmed.contains('\n') {
        return run_script(trimmed, ctx);
    }

    // ドット列を親ディレクトリへの遡上パスに展開する (独自仕様: ドット 1 個 = 1 段上)。
    // コマンド位置 (行全体がドット、末尾 `/` は許容) → `cd <path>`
    //   例: `.` → `cd ..`、`..` → `cd ../..`、`...` `.../` → `cd ../../..`
    // 引数位置 (ls ... / cd .../src 等) → 3 個以上のドット列だけを展開して渡す。
    //   `.` `..` は引数では標準の意味のまま (`git add .` / `cd ..` を壊さない)。
    let dot_expanded: String;
    // 行全体がドット列か判定するとき、末尾の `/` (`.../` の見た目) は無視する。
    let dot_core = trimmed.strip_suffix('/').unwrap_or(trimmed);
    let trimmed = if !dot_core.is_empty() && dot_core.bytes().all(|b| b == b'.') {
        let levels = dot_core.len();
        dot_expanded = format!("cd {}", vec![".."; levels].join("/"));
        dot_expanded.as_str()
    } else if let Some(s) = expand_dot_args(trimmed) {
        dot_expanded = s;
        dot_expanded.as_str()
    } else {
        trimmed
    };

    // 先頭トークンの abbr を 1 段だけ展開する (abbr は先頭のみ。alias は sh に委ねる)。
    let (first, rest) = split_first_word(trimmed);
    let abbr_line: String = match ctx.abbrs.get(first) {
        Some(expansion) if rest.is_empty() => expansion.clone(),
        Some(expansion) => format!("{} {}", expansion, rest),
        None => trimmed.to_string(),
    };

    // 引数で指定された実在パスを自動記録する (Ctrl+G / Ctrl+T のピッカー用)。
    // インタラクティブ実行のみを対象にし (RC 実行のノイズを避ける)、相対パスは
    // 実行前の cwd を基準に解決するので `cd foo` の foo も正しく記録できる。
    if interactive && let Ok(tokens) = shell_words::split(&abbr_line) {
        let cwd = std::env::current_dir().unwrap_or_default();
        let paths: Vec<String> = tokens
            .iter()
            .map(|t| expand_arg(t, ctx.last_status))
            .collect();
        crate::builtin::record_arg_paths(ctx, &paths, &cwd);
    }

    // ビルトイン判定用に alias も 1 段展開してコマンド名を求める
    // (alias → ビルトインのケースを sh へ流さず本体で実行するため)。
    let (name0, rest0) = split_first_word(&abbr_line);
    let detect_line: String = match ctx.aliases.get(name0) {
        Some(expansion) if rest0.is_empty() => expansion.clone(),
        Some(expansion) => format!("{} {}", expansion, rest0),
        None => abbr_line.clone(),
    };
    let (name, args_str) = split_first_word(&detect_line);

    // rep は後続コマンドを分割・展開せず生の文字列のまま繰り返す (クォート・パイプ・
    // `$VAR` を毎回 bash に解釈させる)。終了ステータスは最後の回のものを残す。
    if name == "rep" {
        if let Err(e) = crate::builtin::rep_line(args_str, ctx) {
            ctx.last_status = 1;
            execute!(stdout(), Print(format!("rep: {}\r\n", e)))?;
        }
        return Ok(());
    }

    // `cd x && make` のように制御演算子でつながった行をビルトインに渡すと、2 つ目以降が
    // 黙って捨てられる。cd は行ごと bash の cd で実行し、終了後に最終カレントを持ち帰る
    // (下の外部コマンド経路)。他の独自ビルトインは bash が知らないので明示的にエラーにする。
    let chained = has_control_operator(args_str);
    if chained && name != "cd" && find_builtin(name).is_some() {
        ctx.last_status = 1;
        execute!(
            stdout(),
            Print(format!(
                "{}: cannot be combined with other commands (&&, ;, |, ...); run it on its own line\r\n",
                name
            ))
        )?;
        return Ok(());
    }

    // ビルトインは sh を経由しないので、引数の ~ / $VAR はここで自前展開する。
    if let Some(builtin) = find_builtin(name).filter(|_| !chained) {
        let args = match shell_words::split(args_str) {
            Ok(parts) => parts,
            Err(e) => {
                execute!(stdout(), Print(format!("parse error: {}\r\n", e)))?;
                return Ok(());
            }
        };
        let expanded: Vec<String> = args
            .iter()
            .map(|a| expand_arg(a, ctx.last_status))
            .collect();
        let str_args: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();
        let status = builtin(&str_args, ctx);
        ctx.last_status = if status.is_ok() { 0 } else { 1 };
        if let Err(e) = status {
            execute!(stdout(), Print(format!("{}: {}\r\n", name, e)))?;
        }
        return Ok(());
    }

    // 外部コマンド: alias 定義を前置し、行 (abbr 展開済み) はそのまま bash に渡す。
    // alias 展開はパイプ後を含めて bash に任せる。先頭の `(exit N)` で直前コマンドの
    // 終了ステータスを $? に引き継ぐ (bash -c は毎回新しいシェルなので明示が必要)。
    let prelude = build_alias_prelude(&ctx.aliases);
    let sh_cmd = format!("{}(exit {}); {}", prelude, ctx.last_status, abbr_line);
    let cwd_file = new_cwd_file();
    let mut command = bash_command(&sh_cmd, &cwd_file);
    // フォアグラウンド実行＋ Ctrl+Z 停止の検知と、終了後のカレントの持ち帰りは job
    // モジュールが担う。表示用のコマンド名には abbr 展開済みの行を渡す。
    crate::job::run_foreground(&mut command, abbr_line.trim(), cwd_file, ctx)?;
    note_if_non_bash(&abbr_line, ctx)
}

/// 複数行コマンド (ペーストされたスクリプト) を改行込みでそのまま bash へ渡して実行する。
///
/// 単一行のような加工 (abbr / ドット展開 / alias→ビルトイン判定) はしない。alias 定義
/// だけは前置し、`(exit N)` で直前の終了ステータスを `$?` へ引き継ぐ。実行シェルは
/// 単一行パスと同じく bash (`[[ ]]`・配列などの bash 拡張をそのまま使えるようにするため)。
fn run_script(script: &str, ctx: &mut ShellContext) -> io::Result<()> {
    let prelude = build_alias_prelude(&ctx.aliases);
    let sh_cmd = format!("{}(exit {}); {}", prelude, ctx.last_status, script);
    let cwd_file = new_cwd_file();
    let mut command = bash_command(&sh_cmd, &cwd_file);
    // ジョブ表示は先頭行だけを見せ、複数行であることを `…` で示す。
    let label = format!("{} …", script.lines().next().unwrap_or(script));
    crate::job::run_foreground(&mut command, &label, cwd_file, ctx)?;
    note_if_non_bash(script, ctx)
}

/// 外部コマンドの最終カレントを受け取る一時ファイルのパスを、実行 1 回ごとに払い出す。
/// 置き場所はユーザー専用の `$XDG_RUNTIME_DIR` (無ければ一時ディレクトリ)。
/// 実行ごとに別名にするのは、バックグラウンドへ回したジョブが後から書き込んでも、
/// 別のコマンドの結果と取り違えないため。
fn new_cwd_file() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!(
        "my_shell_cwd.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// 実行行の先頭に「bash の終了時 (EXIT trap) に `$PWD` を `cwd_file` へ書く」処理を足す。
/// bash の `cd` (チェーン・貼り付けたスクリプト・`pushd` 等) の結果をシェル本体へ持ち帰るため。
/// EXIT trap なので、正常終了・`exit`・Ctrl+C のいずれでも書かれ、終了ステータスも変えない。
/// 関数を alias 定義より前に置くのは、ユーザーの alias (`printf` 等) に化けないようにするため。
/// `exec` で置き換わった場合や、スクリプトが自前の EXIT trap で上書きした場合は書かれず、
/// カレントは持ち帰らない。
fn with_cwd_report(sh_cmd: &str, cwd_file: &Path) -> String {
    format!(
        "__my_shell_report() {{ printf '%s' \"$PWD\" > {} 2>/dev/null; }}\ntrap __my_shell_report EXIT\n{}",
        sh_single_quote(&cwd_file.to_string_lossy()),
        sh_cmd
    )
}

/// 引用符の外に bash の制御演算子・グループ化 (`;` `&` `|` `(` `)` 改行 `` ` ``) があるか。
/// 二重引用符の中でも有効なコマンド置換 (`$(` / `` ` ``) も対象にする
/// (`cd "$(git rev-parse --show-toplevel)"` を bash で評価させるため)。
/// リダイレクトの一部の `&` `|` (`2>&1` `>&2` `&>` `&>>` `>|`) は演算子とみなさない。
fn has_control_operator(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate() {
        let prev = if i > 0 { chars[i - 1] } else { '\0' };
        let next = chars.get(i + 1).copied().unwrap_or('\0');
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                }
            }
            Some(_) => match c {
                '\\' => escaped = true,
                '"' => quote = None,
                '`' => return true,
                '(' if prev == '$' => return true,
                _ => {}
            },
            None => match c {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(c),
                '&' if prev == '>' || next == '>' => {}
                '|' if prev == '>' => {}
                ';' | '&' | '|' | '(' | ')' | '\n' | '`' => return true,
                _ => {}
            },
        }
    }
    false
}

/// 引用符の中身とエスケープされた文字を `x` に置き換える (語の区切りは保つ)。
/// 他シェルの書き方の判定で、引用符の中の記号や単語を拾わないため。
fn mask_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            escaped = false;
            out.push('x');
            continue;
        }
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if q == '"' && c == '\\' {
                    escaped = true;
                }
                out.push('x');
            }
            None => {
                match c {
                    '\\' => escaped = true,
                    '\'' | '"' => quote = Some(c),
                    _ => {
                        out.push(c);
                        continue;
                    }
                }
                out.push('x');
            }
        }
    }
    out
}

/// コマンドの区切りになる文字 (この直後の語はコマンド位置)。
fn is_command_separator(c: char) -> bool {
    matches!(c, ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}')
}

/// PowerShell のコマンドレット形式 (`Get-ChildItem` のような `Verb-Noun`) か。
fn is_cmdlet_like(w: &str) -> bool {
    let Some((verb, noun)) = w.split_once('-') else {
        return false;
    };
    let capitalized = |s: &str| {
        let mut cs = s.chars();
        s.len() >= 2
            && cs.next().is_some_and(|c| c.is_ascii_uppercase())
            && cs.all(|c| c.is_ascii_alphanumeric())
    };
    capitalized(verb) && capitalized(noun) && verb.chars().skip(1).any(|c| c.is_ascii_lowercase())
}

/// 引数位置に `$` の付かない `(…)` があるか (他シェルのコマンド置換。bash なら `$(…)`)。
/// `foo()` / `foo ()` (関数定義)、`$(` `<(` `>(` `=(` (置換・配列)、コマンド位置や
/// `if (` などのサブシェルは除く。
fn has_bare_paren_substitution(u: &str) -> bool {
    for (i, c) in u.char_indices() {
        if c != '(' || u[i + 1..].trim_start().starts_with(')') {
            continue;
        }
        let before = &u[..i];
        let trimmed = before.trim_end();
        if trimmed.len() == before.len() {
            continue; // 直前に空白が無い: `$(` `<(` `=(` `foo(` など
        }
        match trimmed.chars().last() {
            None => continue,
            Some(last) if is_command_separator(last) => continue, // コマンド位置のサブシェル
            Some(_) => {}
        }
        let word = trimmed
            .rsplit(|ch: char| ch.is_whitespace() || is_command_separator(ch))
            .next()
            .unwrap_or("");
        if matches!(
            word,
            "if" | "elif" | "while" | "until" | "then" | "else" | "do" | "time" | "!"
        ) {
            continue;
        }
        return true;
    }
    false
}

/// bash では書かない、他のシェル特有の書き方を含むか。bash が失敗したときに注意書きを
/// 添えるかどうかの判定にだけ使う (成功したコマンドには出さないので、多少の取りこぼしは許容)。
///
/// - 演算子・変数の並び: `&|`、`>>&`、`$env:NAME`
/// - コマンド位置の語: `end` `endif` `endsw` `foreach` `begin` `and` `or` `@`、`Verb-Noun` 形式
/// - 引数位置の `$` の付かない `(…)` (例: `for i in (seq 3)`)
fn looks_non_bash(line: &str) -> bool {
    let u = mask_quoted(line);
    if u.contains("&|") || u.contains(">>&") || u.contains("$env:") {
        return true;
    }
    let foreign_command = u.split(is_command_separator).any(|seg| {
        seg.split_whitespace().next().is_some_and(|w| {
            matches!(
                w,
                "end" | "endif" | "endsw" | "foreach" | "begin" | "and" | "or" | "@"
            ) || is_cmdlet_like(w)
        })
    });
    foreign_command || has_bare_paren_substitution(&u)
}

/// 外部コマンドが構文エラー (2) / コマンド未検出 (127) で失敗し、かつ他のシェルの書き方に
/// 見えるとき、このシェルは bash の書き方で実行していると注意書きする。どのシェルの書き方かは
/// 断定しない (誤判定を避けるため)。
fn note_if_non_bash(line: &str, ctx: &ShellContext) -> io::Result<()> {
    if matches!(ctx.last_status, 2 | 127) && looks_non_bash(line) {
        execute!(
            stdout(),
            SetForegroundColor(Color::Rgb {
                r: 0xe2,
                g: 0xd9,
                b: 0x80
            }),
            Print(
                "↳ note: this shell runs commands with bash syntax; \
                 the command looks like it is written for a different shell\r\n"
            ),
            ResetColor,
        )?;
    }
    Ok(())
}

/// `bash -c <sh_cmd>` の Command を作る。終了時にカレントを `cwd_file` へ書き残すよう
/// [`with_cwd_report`] で包む。子を独立プロセスグループに入れて端末ジョブ制御の対象にし、
/// 親シェルが無視しているシグナルを既定動作へ戻す。これをしないと Ctrl+Z で停止できず、
/// Ctrl+C も効かない。
fn bash_command(sh_cmd: &str, cwd_file: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("bash");
    command.arg("-c").arg(with_cwd_report(sh_cmd, cwd_file));
    unsafe {
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            for sig in [
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTSTP,
                libc::SIGTTOU,
                libc::SIGTTIN,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    command
}

/// 行中の各トークンに含まれるドット列 (`...` 以上) を相対パスに展開する。
/// 変換が不要なら `None` を返す。
fn expand_dot_args(line: &str) -> Option<String> {
    let mut changed = false;
    let result: Vec<String> = line
        .split_whitespace()
        .map(|t| match expand_dot_token(t) {
            Some(exp) => {
                changed = true;
                exp
            }
            None => t.to_string(),
        })
        .collect();
    if changed {
        Some(result.join(" "))
    } else {
        None
    }
}

/// トークンを `/` 区切りのコンポーネントに分け、3 個以上のドットだけからなる
/// コンポーネントをドット数ぶんの `..` の連鎖に展開する (ドット 1 個 = 1 段上)。
///
/// 例: `...` → `../../..`、`.../` → `../../../`、`.../src` → `../../../src`。
/// `.` / `..` は引数では標準の意味を保つため変えない。ドット以外を含む
/// コンポーネント (`..foo` 等) も変えない。変換が無ければ `None`。
fn expand_dot_token(token: &str) -> Option<String> {
    if !token.contains('.') {
        return None;
    }
    let mut changed = false;
    let comps: Vec<String> = token
        .split('/')
        .map(|c| {
            if c.len() >= 3 && c.bytes().all(|b| b == b'.') {
                changed = true;
                vec![".."; c.len()].join("/")
            } else {
                c.to_string()
            }
        })
        .collect();
    if changed { Some(comps.join("/")) } else { None }
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

/// 全 alias を bash の `alias` 定義に変換した前文を作る。
///
/// この前文を外部コマンドの前に置くと、bash がコマンド位置 (行頭・`|`/`;`/`&&` の
/// 直後など) ごとに alias を展開する。各定義を別行に置くのは「alias は次の行から
/// 有効」という規則のため。`shopt -s expand_aliases` は非対話 (`bash -c`) では既定で
/// 無効な alias 展開を有効化するために必須。
fn build_alias_prelude(aliases: &HashMap<String, String>) -> String {
    let mut out = String::from("shopt -s expand_aliases 2>/dev/null\n");
    for (name, value) in aliases {
        if !is_valid_alias_name(name) {
            continue;
        }
        out.push_str("alias ");
        out.push_str(name);
        out.push('=');
        out.push_str(&sh_single_quote(value));
        out.push('\n');
    }
    out
}

/// sh の alias 名として安全か (英数字・`_`・`-` のみ)。それ以外は前文から除外する。
fn is_valid_alias_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 文字列を sh のシングルクォート文字列リテラルに変換する。
fn sh_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''"); // 閉じ → エスケープした ' → 開き
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// 単語先頭の `~` と `$VAR` / `${VAR}` / `$$` / `$?` を展開する。
fn expand_arg(s: &str, last_status: i32) -> String {
    // ~ 展開: 単語が `~` だけ、または `~/` で始まる場合のみ (規則は expand_tilde に集約)
    let s: String = if s == "~" || s.starts_with("~/") {
        crate::history::expand_tilde(s)
            .to_string_lossy()
            .into_owned()
    } else {
        s.to_string()
    };

    // $VAR / ${VAR} 展開
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next(); // '{'を消費
            let name: String = chars.by_ref().take_while(|&x| x != '}').collect();
            out.push_str(&std::env::var(&name).unwrap_or_default());
        } else {
            match chars.peek() {
                Some(&'$') => {
                    chars.next();
                    out.push_str(&std::process::id().to_string());
                }
                Some(&'?') => {
                    chars.next();
                    out.push_str(&last_status.to_string());
                }
                _ => {
                    // 変数名: [a-zA-Z_][a-zA-Z0-9_]*
                    let mut name = String::new();
                    while let Some(&x) = chars.peek() {
                        if x.is_alphanumeric() || x == '_' {
                            name.push(x);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if name.is_empty() {
                        out.push('$'); // 後続が英数字でない裸の $
                    } else {
                        out.push_str(&std::env::var(&name).unwrap_or_default());
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_operator_detection() {
        // 引用符の外の演算子・グループ化はチェーンとみなす。
        for s in [
            "/tmp && make",
            "/tmp; ls",
            "/tmp || echo ng",
            "/tmp | cat",
            "/tmp &",
            "$(git rev-parse --show-toplevel)",
            "\"$(git rev-parse --show-toplevel)\"", // 二重引用符内のコマンド置換
            "`pwd`",
            "a\nb",
            "/tmp |& cat", // |& はパイプ
        ] {
            assert!(has_control_operator(s), "{s:?} should be chained");
        }
        // 引用符・エスケープの中の記号や、普通の引数はチェーンではない。
        for s in [
            "/tmp",
            "'a && b'",
            "\"a; b\"",
            "a\\;b",
            "\"$HOME/x\"",
            "-",
            "",
            // リダイレクトの一部の & / | はチェーンではない
            "foo 2>&1",
            "foo >&2",
            "foo &>/dev/null",
            "foo &>>log",
            "foo >| out",
        ] {
            assert!(!has_control_operator(s), "{s:?} should not be chained");
        }
    }

    /// `with_cwd_report` で包んだ行を本物の bash で実行し、(終了ステータス, 書かれた cwd) を返す。
    /// シェル本体のカレントは変えない (子の bash の中だけで cd する)。
    fn run_reported(cmd: &str) -> (i32, Option<String>) {
        let file = new_cwd_file();
        let status = std::process::Command::new("bash")
            .arg("-c")
            .arg(with_cwd_report(cmd, &file))
            .status()
            .unwrap()
            .code()
            .unwrap();
        let cwd = std::fs::read_to_string(&file).ok();
        let _ = std::fs::remove_file(&file);
        (status, cwd)
    }

    #[test]
    fn cwd_report_keeps_status_and_reports_pwd() {
        // チェーン内の cd の結果が書かれ、最後のコマンドの終了ステータスは保たれる。
        assert_eq!(run_reported("cd /tmp && false"), (1, Some("/tmp".into())));
        assert_eq!(run_reported("cd /tmp; true"), (0, Some("/tmp".into())));
        // 行末コメントがあっても書かれる。
        assert_eq!(run_reported("cd /tmp # comment").1, Some("/tmp".into()));
        // サブシェル内の cd は bash どおり外へ漏れない。
        let here = std::env::current_dir().unwrap();
        let (_, cwd) = run_reported("(cd /tmp)");
        assert_eq!(cwd.map(PathBuf::from), Some(here));
        // exit で終わっても、その時点のカレントと終了ステータスが残る。
        assert_eq!(run_reported("cd /tmp && exit 3"), (3, Some("/tmp".into())));
        // exec で置き換わった場合は書かれない (= カレントを持ち帰らない)。
        assert_eq!(run_reported("cd /tmp && exec true"), (0, None));
    }

    #[test]
    fn non_bash_syntax_detection() {
        // 他のシェル特有の書き方 (bash では構文エラー / コマンド未検出になる)。
        for s in [
            "for i in (seq 3); echo $i; end",
            "if test -d /tmp; echo yes; end",
            "echo hi; and echo ok",
            "begin; echo a; end",
            "make &| less",
            "echo hi >>& log",
            "foreach i (1 2 3)",
            "endif",
            "@ n = 1 + 2",
            "Get-ChildItem -Path /tmp",
            "$env:FOO = \"x\"",
            "echo (date)",
        ] {
            assert!(looks_non_bash(s), "{s:?} should look non-bash");
        }
        // 正しい bash の書き方は対象外 (サブシェル・配列・関数定義・各種置換・リダイレクト)。
        for s in [
            "ls -la | grep foo",
            "if (true); then echo y; fi",
            "(cd /tmp; ls)",
            "arr=(1 2 3)",
            "foo() { echo hi; }",
            "foo () { echo hi; }",
            "echo $(date)",
            "diff <(ls a) <(ls b)",
            "make 2>&1 | tee log",
            "make |& tee log",
            "echo end",
            "echo \"; end\"",
            "endpoint --start",
            "git log --format='(%h) %s'",
            "x-ray scan",
        ] {
            assert!(!looks_non_bash(s), "{s:?} should look like bash");
        }
    }

    #[test]
    fn split_first() {
        assert_eq!(split_first_word("git status"), ("git", "status"));
        assert_eq!(split_first_word("  ls  -la  x"), ("ls", "-la  x"));
        assert_eq!(split_first_word("solo"), ("solo", ""));
        assert_eq!(split_first_word(""), ("", ""));
    }

    #[test]
    fn expand_dot_token_cases() {
        // 3 個以上のドット列 → ドット数ぶんの `..`。末尾スラッシュ・後続パスも対応。
        assert_eq!(expand_dot_token("..."), Some("../../..".to_string()));
        assert_eq!(expand_dot_token("...."), Some("../../../..".to_string()));
        assert_eq!(expand_dot_token(".../"), Some("../../../".to_string()));
        assert_eq!(
            expand_dot_token(".../src"),
            Some("../../../src".to_string())
        );
        assert_eq!(
            expand_dot_token("..../a/b"),
            Some("../../../../a/b".to_string())
        );
        // `.` / `..` / `../` と、ドット以外を含むコンポーネントは変えない。
        assert_eq!(expand_dot_token("."), None);
        assert_eq!(expand_dot_token(".."), None);
        assert_eq!(expand_dot_token("../"), None);
        assert_eq!(expand_dot_token("../foo"), None);
        assert_eq!(expand_dot_token("..foo"), None);
        assert_eq!(expand_dot_token("foo"), None);
    }

    #[test]
    fn expand_dot_args_in_line() {
        assert_eq!(expand_dot_args("ls .../"), Some("ls ../../../".to_string()));
        assert_eq!(
            expand_dot_args("cd .../src"),
            Some("cd ../../../src".to_string())
        );
        // ドット列が無ければ変換しない (元の行をそのまま使う)。
        assert_eq!(expand_dot_args("git status"), None);
        assert_eq!(expand_dot_args("cd ../foo"), None);
    }

    #[test]
    fn expand_status_and_literal() {
        assert_eq!(expand_arg("$?", 7), "7");
        assert_eq!(expand_arg("code=$?", 0), "code=0");
        assert_eq!(expand_arg("plain", 0), "plain");
        assert_eq!(expand_arg("$", 0), "$"); // 裸の $ はそのまま
    }

    #[test]
    fn single_quote_escapes() {
        assert_eq!(sh_single_quote("ls --color=auto"), "'ls --color=auto'");
        // 埋め込み ' は「閉じ → \' → 開き」になる
        assert_eq!(sh_single_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn valid_alias_names() {
        assert!(is_valid_alias_name("grep"));
        assert!(is_valid_alias_name("ll"));
        assert!(is_valid_alias_name("git-log"));
        assert!(!is_valid_alias_name("")); // 空は不可
        assert!(!is_valid_alias_name("..")); // abbr 的な名前は sh alias 不可
        assert!(!is_valid_alias_name("a b")); // 空白を含む
    }

    #[test]
    fn prelude_contains_defs() {
        let mut aliases = HashMap::new();
        aliases.insert("grep".to_string(), "grep --color=auto".to_string());
        let prelude = build_alias_prelude(&aliases);
        assert!(prelude.contains("alias grep='grep --color=auto'\n"));
        assert!(prelude.starts_with("shopt -s expand_aliases"));
    }
}
