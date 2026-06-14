//! コマンド履歴。
//!
//! セッション中はメモリ上に保持し、シェル終了時に HISTORY_FILE へ書き出す。
//!
//! # ファイル形式
//! タブ区切り 1 行 1 エントリ: `<unix_sec>\t<cwd>\t<cmd>`
//! `\`, `\t`, `\n` はそれぞれ `\\`, `\t`, `\n` にエスケープする。

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const HISTORY_FILE: &str = "~/.my_shell_history";

const MAX_ENTRIES: usize = 10_000;

// ─── エントリ ─────────────────────────────────────────────────────────────────

pub struct HistoryEntry {
    pub timestamp: u64,
    pub cwd: PathBuf,
    pub cmd: String,
}

// ─── History ──────────────────────────────────────────────────────────────────

pub struct History {
    entries: Vec<HistoryEntry>,
}

impl History {
    /// 起動時に呼ぶ。ファイルが存在しなければ空の履歴を返す。
    pub fn load() -> Self {
        let path = expand_tilde(HISTORY_FILE);
        let entries = load_entries(&path).unwrap_or_default();
        Self { entries }
    }

    /// コマンドをメモリ上の履歴に追加する。空行・空白のみは無視する。
    pub fn add(&mut self, cmd: &str, cwd: &Path) {
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.entries.push(HistoryEntry {
            timestamp,
            cwd: cwd.to_path_buf(),
            cmd,
        });
        // 上限を超えたら古いエントリを削除
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
    }

    /// 全エントリを HISTORY_FILE に書き出す (シェル終了時に呼ぶ)。
    pub fn save(&self) -> io::Result<()> {
        let path = expand_tilde(HISTORY_FILE);
        let mut file = std::fs::File::create(&path)?;
        for e in &self.entries {
            writeln!(
                file,
                "{}\t{}\t{}",
                e.timestamp,
                escape(e.cwd.to_string_lossy().as_ref()),
                escape(&e.cmd),
            )?;
        }
        Ok(())
    }

    /// `prefix` で始まる候補を返す。
    ///
    /// 優先順位:
    ///   1. `current_cwd` で実行されたエントリ (新しい順)
    ///   2. 他ディレクトリで実行されたエントリ (新しい順)
    ///
    /// 同じコマンド文字列は最新のもののみ残す (重複排除)。
    pub fn search_completions(&self, prefix: &str, current_cwd: &Path) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut same_cwd: Vec<String> = Vec::new();
        let mut other: Vec<String> = Vec::new();

        for entry in self.entries.iter().rev() {
            if !entry.cmd.starts_with(prefix) {
                continue;
            }
            if !seen.insert(entry.cmd.clone()) {
                continue;
            }
            if entry.cwd == current_cwd {
                same_cwd.push(entry.cmd.clone());
            } else {
                other.push(entry.cmd.clone());
            }
        }

        same_cwd.extend(other);
        same_cwd
    }
}

// ─── Drop で自動保存 ──────────────────────────────────────────────────────────

impl Drop for History {
    fn drop(&mut self) {
        let _ = self.save();
    }
}

// ─── ファイル読み込み ─────────────────────────────────────────────────────────

fn load_entries(path: &Path) -> io::Result<Vec<HistoryEntry>> {
    let file = std::fs::File::open(path)?;
    let lines: Vec<String> = io::BufReader::new(file)
        .lines()
        .collect::<io::Result<_>>()?;

    // MAX_ENTRIES 件だけメモリに乗せる (ファイル末尾 = 最新)
    let start = lines.len().saturating_sub(MAX_ENTRIES);
    Ok(lines[start..]
        .iter()
        .filter_map(|l| parse_line(l))
        .collect())
}

fn parse_line(line: &str) -> Option<HistoryEntry> {
    let mut parts = line.splitn(3, '\t');
    let timestamp: u64 = parts.next()?.parse().ok()?;
    let cwd = PathBuf::from(unescape(parts.next()?));
    let cmd = unescape(parts.next()?);
    Some(HistoryEntry {
        timestamp,
        cwd,
        cmd,
    })
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
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
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
            Some('t') => out.push('\t'),
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
