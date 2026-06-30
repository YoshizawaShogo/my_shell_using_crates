//! コマンド履歴。
//!
//! セッション中はメモリ上に保持し、シェル終了時にセッション追加分を HISTORY_FILE へ追記する。
//! 複数端末が同時に動いていても互いの履歴を上書きしない。
//!
//! # ファイル形式
//! 1 行 1 コマンド: `<cmd>`
//! `\`, `\n` はそれぞれ `\\`, `\n` にエスケープする。
//! 旧形式 (`<unix_sec>\t<cmd>`) も起動時に読み込める。

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

pub const HISTORY_FILE: &str = "~/.my_shell_history";

const MAX_ENTRIES: usize = 10_000;

// ─── History ──────────────────────────────────────────────────────────────────

pub struct History {
    /// 全履歴 (重複なし・古い順)
    entries: Vec<String>,
    /// このセッションで追加したコマンド (追記用)
    session_cmds: Vec<String>,
}

impl History {
    /// 起動時に呼ぶ。ファイルが存在しなければ空の履歴を返す。
    pub fn load() -> Self {
        let path = expand_tilde(HISTORY_FILE);
        let entries = load_entries(&path).unwrap_or_default();
        Self {
            entries,
            session_cmds: Vec::new(),
        }
    }

    /// コマンドをメモリ上の履歴に追加する。空行・空白のみは無視する。
    /// 同じコマンドが既にある場合は古い方を削除して末尾に移動する。
    pub fn add(&mut self, cmd: &str) {
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        self.entries.retain(|e| e != &cmd);
        self.entries.push(cmd.clone());
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
        self.session_cmds.retain(|e| e != &cmd);
        self.session_cmds.push(cmd);
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

    /// セッション追加分だけを HISTORY_FILE へ追記する。
    pub fn save(&self) -> io::Result<()> {
        if self.session_cmds.is_empty() {
            return Ok(());
        }
        let path = expand_tilde(HISTORY_FILE);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        for cmd in &self.session_cmds {
            writeln!(file, "{}", escape(cmd))?;
        }
        Ok(())
    }
}

// ─── Drop で自動保存 ──────────────────────────────────────────────────────────

impl Drop for History {
    fn drop(&mut self) {
        let _ = self.save();
    }
}

// ─── ファイル読み込み ─────────────────────────────────────────────────────────

fn load_entries(path: &Path) -> io::Result<Vec<String>> {
    let file = std::fs::File::open(path)?;
    let cmds: Vec<String> = io::BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok().and_then(|s| parse_line(&s)))
        .collect();

    // 重複を除去: 末尾 (新しい方) を優先して残す
    let mut seen = HashSet::new();
    let mut deduped: Vec<String> = cmds
        .into_iter()
        .rev()
        .filter(|cmd| seen.insert(cmd.clone()))
        .collect();
    deduped.reverse();

    // MAX_ENTRIES 件だけメモリに乗せる (末尾 = 最新)
    if deduped.len() > MAX_ENTRIES {
        let excess = deduped.len() - MAX_ENTRIES;
        deduped.drain(..excess);
    }

    Ok(deduped)
}

/// 1 行をコマンド文字列にパースする。
/// 新形式: `<cmd>`
/// 旧形式: `<unix_sec>\t<cmd>` または `<unix_sec>\t<cwd>\t<cmd>`
fn parse_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // 旧形式: 先頭フィールドが数値のときはタイムスタンプとみなす
    if let Some(tab) = line.find('\t')
        && line[..tab].parse::<u64>().is_ok()
    {
        let rest = &line[tab + 1..];
        // <cwd>\t<cmd> がさらに続く場合は最後のフィールドがコマンド
        let cmd = match rest.find('\t') {
            Some(i) => &rest[i + 1..],
            None => rest,
        };
        let cmd = unescape(cmd);
        return if cmd.is_empty() { None } else { Some(cmd) };
    }
    // 新形式
    let cmd = unescape(line);
    if cmd.is_empty() { None } else { Some(cmd) }
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
        let cmd = unescape("cmd\\nnext");
        assert_eq!(cmd, "cmd\nnext");
        assert_eq!(parse_line("cmd\\nnext").unwrap(), "cmd\nnext");
    }

    #[test]
    fn parse_line_old_format() {
        // 旧形式: <unix_sec>\t<cmd>
        assert_eq!(parse_line("123\tls -la").unwrap(), "ls -la");
    }

    #[test]
    fn parse_line_old_format_with_cwd() {
        // 旧形式: <unix_sec>\t<cwd>\t<cmd>
        assert_eq!(
            parse_line("123\t/home/user\techo hello").unwrap(),
            "echo hello"
        );
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
            session_cmds: Vec::new(),
        };
        h.add("ls");
        h.add("cd /tmp");
        h.add("ls");
        assert_eq!(h.entries, vec!["cd /tmp", "ls"]);
    }
}
