//! コマンド履歴。
//!
//! セッション中はメモリ上に保持し、コマンド実行のたびに HISTORY_FILE へ即時追記する。
//! 複数端末が同時に動いていても互いの履歴を上書きしない。
//! プロンプト再描画前に差分だけ再読み込みすることで端末間リアルタイム同期する。
//!
//! # ファイル形式
//! 1 行 1 コマンド: `<cmd>`（追記ログ、重複あり）
//! メモリ上は dedup・MRU 順。
//! `\`, `\n` はそれぞれ `\\`, `\n` にエスケープする。
//! 旧形式 (`<unix_sec>\t<cmd>`) も起動時に読み込める。

use std::collections::HashSet;
use std::io::{self, BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const HISTORY_FILE: &str = "~/.my_shell_history";

const MAX_ENTRIES: usize = 10_000;

// ─── History ──────────────────────────────────────────────────────────────────

pub struct History {
    /// 全履歴 (重複なし・MRU 順: 末尾が最新)
    entries: Vec<String>,
    /// ファイルの既読バイト数 (差分読み込み用)
    file_offset: u64,
}

impl History {
    /// 起動時に呼ぶ。ファイルが存在しなければ空の履歴を返す。
    pub fn load() -> Self {
        let path = expand_tilde(HISTORY_FILE);
        let (entries, file_offset) = load_entries(&path).unwrap_or_default();
        Self {
            entries,
            file_offset,
        }
    }

    /// コマンドをメモリに追加し、ファイルへ即時追記する。
    /// 同じコマンドが既にある場合は古い方を削除して末尾 (最新) へ移動する。
    pub fn add(&mut self, cmd: &str) {
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        // 2 トークン以下かつ 20 文字未満の短い自明なコマンド (ls, cd .. 等) は
        // 履歴の価値が薄いので記録しない (メモリにもファイルにも残さない)。
        if is_trivial(&cmd) {
            return;
        }
        self.entries.retain(|e| e != &cmd);
        self.entries.push(cmd.clone());
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
        let path = expand_tilde(HISTORY_FILE);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let line = format!("{}\n", escape(&cmd));
            if file.write_all(line.as_bytes()).is_ok() {
                self.file_offset += line.len() as u64;
            }
        }
    }

    /// ファイルの差分を読み込み、他端末のコマンドをメモリへ取り込む。
    /// プロンプト再描画前に呼ぶ。ファイルが増えていなければ即時リターン。
    pub fn reload(&mut self) {
        let path = expand_tilde(HISTORY_FILE);
        let Ok(mut file) = std::fs::File::open(&path) else {
            return;
        };
        let Ok(meta) = file.metadata() else { return };
        if meta.len() <= self.file_offset {
            return;
        }
        if file.seek(SeekFrom::Start(self.file_offset)).is_err() {
            return;
        }
        let new_cmds: Vec<String> = io::BufReader::new(&file)
            .lines()
            .filter_map(|l| l.ok().and_then(|s| parse_line(&s)))
            .collect();
        self.file_offset = meta.len();
        for cmd in new_cmds {
            self.entries.retain(|e| e != &cmd);
            self.entries.push(cmd);
        }
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// インデックス指定でコマンド文字列を取得する (履歴ナビゲーション用)。
    pub fn get_cmd(&self, idx: usize) -> Option<&str> {
        self.entries.get(idx).map(String::as_str)
    }

    /// 最新の履歴コマンドを返す (Alt+. の最終引数挿入用)。
    pub fn last_cmd(&self) -> Option<&str> {
        self.entries.last().map(String::as_str)
    }

    /// `prefix` で始まる候補を新しい順で返す。
    pub fn search_completions(&self, prefix: &str) -> Vec<String> {
        self.entries
            .iter()
            .rev()
            .filter(|cmd| cmd.starts_with(prefix))
            .cloned()
            .collect()
    }
}

// ─── ファイル読み込み ─────────────────────────────────────────────────────────

fn load_entries(path: &Path) -> io::Result<(Vec<String>, u64)> {
    let file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len();
    let cmds: Vec<String> = io::BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok().and_then(|s| parse_line(&s)))
        .collect();

    // 重複を除去: 末尾 (新しい方) を優先して残す → MRU 順 (末尾が最新)
    let mut seen = HashSet::new();
    let mut deduped: Vec<String> = cmds
        .into_iter()
        .rev()
        .filter(|cmd| seen.insert(cmd.clone()))
        .collect();
    deduped.reverse();

    if deduped.len() > MAX_ENTRIES {
        let excess = deduped.len() - MAX_ENTRIES;
        deduped.drain(..excess);
    }

    Ok((deduped, file_size))
}

/// 1 行をコマンド文字列にパースする。
/// 新形式: `<cmd>`
/// 旧形式: `<unix_sec>\t<cmd>` または `<unix_sec>\t<cwd>\t<cmd>`
fn parse_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if let Some(tab) = line.find('\t')
        && line[..tab].parse::<u64>().is_ok()
    {
        let rest = &line[tab + 1..];
        let cmd = match rest.find('\t') {
            Some(i) => &rest[i + 1..],
            None => rest,
        };
        let cmd = unescape(cmd);
        return if cmd.is_empty() { None } else { Some(cmd) };
    }
    let cmd = unescape(line);
    if cmd.is_empty() { None } else { Some(cmd) }
}

/// 履歴に残さない「自明なコマンド」判定。
/// - 先頭トークンが `ls` / `cd` なら長さに関わらず true (無条件で除外)。
/// - それ以外は 2 トークン以下 (空白区切り) かつ 20 文字未満なら true。
pub fn is_trivial(cmd: &str) -> bool {
    let cmd = cmd.trim();
    let first = cmd.split_whitespace().next().unwrap_or("");
    if first == "ls" || first == "cd" {
        return true;
    }
    cmd.split_whitespace().count() <= 2 && cmd.chars().count() < 20
}

// ─── パス展開 ─────────────────────────────────────────────────────────────────

pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest);
    }
    PathBuf::from(path)
}

// ─── エスケープ ───────────────────────────────────────────────────────────────

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some(c) => {
                out.push('\\');
                out.push(c);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_roundtrip() {
        for s in ["plain", "a\nb", "a\\b", "mix\n\\"] {
            assert_eq!(unescape(&escape(s)), s, "roundtrip failed for {:?}", s);
        }
    }

    #[test]
    fn parse_line_ok() {
        assert_eq!(parse_line("ls -la").unwrap(), "ls -la");
    }

    #[test]
    fn parse_line_unescapes() {
        assert_eq!(parse_line("cmd\\nnext").unwrap(), "cmd\nnext");
    }

    #[test]
    fn parse_line_old_format() {
        assert_eq!(parse_line("123\tls -la").unwrap(), "ls -la");
    }

    #[test]
    fn parse_line_old_format_with_cwd() {
        assert_eq!(
            parse_line("123\t/home/user\techo hello").unwrap(),
            "echo hello"
        );
    }

    #[test]
    fn is_trivial_filters_short_commands() {
        // 2 token 以下かつ 20 文字未満 → 記録しない
        assert!(is_trivial("ls"));
        assert!(is_trivial("cd .."));
        assert!(is_trivial("git status")); // 2 token, 10 文字
        // 3 token 以上、または 20 文字以上 → 記録する
        assert!(!is_trivial("git commit -m")); // 3 token
        // ただし ls / cd は先頭トークンなら長さ・token 数に関わらず無条件で除外
        assert!(is_trivial("cd /very/long/directory/path")); // 20 文字以上でも cd は除外
        assert!(is_trivial("ls -la /some/longer/path")); // 3 token でも ls は除外
        // ls/cd で始まらない別コマンドは影響しない
        assert!(!is_trivial("cdr /very/long/directory/path"));
        assert!(!is_trivial("lsof -i :8080"));
    }

    #[test]
    fn parse_line_rejects_empty() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn dedup_keeps_latest() {
        let mut h = History {
            entries: Vec::new(),
            file_offset: 0,
        };
        h.entries.push("ls".to_string());
        h.entries.push("cd /tmp".to_string());
        // simulate add("ls") dedup
        h.entries.retain(|e| e != "ls");
        h.entries.push("ls".to_string());
        assert_eq!(h.entries, vec!["cd /tmp", "ls"]);
    }
}
