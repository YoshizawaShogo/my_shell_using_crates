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
        // 複数コマンド行 (主にペーストで `; ` 連結されたもの) は記録しない。
        // ゴースト補完やピッカーで見づらく、頻度も低いため。ただしクォート内や
        // エスケープされた `;` (例 `find ... -exec rm {} \;`) は正当な単一コマンド
        // なので記録する。判定はコマンド区切りの `;` があるかどうかで行う。
        if has_command_separator(&cmd) {
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

/// コマンド区切りの `;` (トップレベル・クォート外・非エスケープ) を含むか判定する。
///
/// シングル/ダブルクォートとバックスラッシュエスケープを追跡するので、
/// `find ... -exec rm {} \;` の `\;` や `awk '{print;}'`・`echo "a;b"` の
/// クォート内 `;` は区切りとみなさない。ペースト連結の `; ` だけを弾くのに使う。
fn has_command_separator(cmd: &str) -> bool {
    let mut chars = cmd.chars();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        match c {
            // シングルクォート内以外ではバックスラッシュが次の 1 文字をエスケープする。
            '\\' if !in_single => {
                chars.next();
            }
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ';' if !in_single && !in_double => return true,
            _ => {}
        }
    }
    false
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
    fn command_separator_detection() {
        // トップレベルの裸 `;` → 区切りとして検出 (記録しない)
        assert!(has_command_separator("ls; cd src"));
        assert!(has_command_separator("ls -la; cargo build"));
        assert!(has_command_separator("a; b; c"));
        // エスケープ・クォート内の `;` → 区切りではない (記録する)
        assert!(!has_command_separator(
            "find . -name '*.tmp' -exec rm {} \\;"
        ));
        assert!(!has_command_separator("awk '{print; next}' file"));
        assert!(!has_command_separator("echo \"a; b\""));
        // `;` を含まない通常コマンド
        assert!(!has_command_separator("git status"));
        // エスケープしたバックスラッシュの後の `;` は区切り
        assert!(has_command_separator("printf 'x\\\\'; ls"));
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
