//! ジョブ制御。
//!
//! 各外部コマンドを独立したプロセスグループで起動し、端末の所有権 (`tcsetpgrp`) を
//! 渡すことで Ctrl+Z による停止・`fg`/`bg` による再開を可能にする。
//! ジョブ表は [`ShellContext`] が保持し、`fg`/`bg`/`jobs` ビルトインがここを操作する。
//!
//! `&` でのバックグラウンド実行は従来どおり `sh` 任せ (本モジュールでは追跡しない)。
//! 追跡するのは「Ctrl+Z で停止したジョブ」と「それを `bg` で再開したもの」だけ。

use crate::builtin::ShellContext;
use crate::selector::{self, Selection};
use crossterm::{execute, style::Print, terminal};
use std::io::{self, stdout};
use std::process::Command;

const STDIN: i32 = libc::STDIN_FILENO;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Stopped,
}

impl JobState {
    fn label(self) -> &'static str {
        match self {
            JobState::Running => "Running",
            JobState::Stopped => "Stopped",
        }
    }
}

pub struct Job {
    /// ジョブ番号 ([1], [2], ...)。最小の空き番号を割り当てる。
    pub id: u32,
    /// プロセスグループ ID (= リーダーの pid)。シグナルは `kill(-pgid, ...)` で送る。
    pub pgid: i32,
    /// 表示用のコマンド行。
    pub cmd: String,
    pub state: JobState,
}

// ─── 端末・待機のヘルパ ───────────────────────────────────────────────────────

fn shell_pgid() -> i32 {
    unsafe { libc::getpgrp() }
}

/// 端末のフォアグラウンドプロセスグループを `pgid` にする。
/// 本体は SIGTTOU を無視しているので、ここで停止させられることはない。
fn give_terminal(pgid: i32) {
    unsafe {
        libc::tcsetpgrp(STDIN, pgid);
    }
}

enum Wait {
    Exited(i32),
    Signaled(i32),
    Stopped,
}

/// `pid` を停止/終了まで待つ (`WUNTRACED` で停止も拾う)。
fn wait_job(pid: i32) -> Wait {
    let mut status: libc::c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if r == -1 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Wait::Exited(1);
        }
        break;
    }
    if libc::WIFSTOPPED(status) {
        Wait::Stopped
    } else if libc::WIFSIGNALED(status) {
        Wait::Signaled(libc::WTERMSIG(status))
    } else {
        Wait::Exited(libc::WEXITSTATUS(status))
    }
}

/// 待機結果を `last_status` とジョブ表へ反映する。停止なら指定 id (なければ新規) で登録。
fn settle(outcome: Wait, pgid: i32, cmd: &str, reuse_id: Option<u32>, ctx: &mut ShellContext) {
    match outcome {
        Wait::Stopped => {
            let id = reuse_id.unwrap_or_else(|| next_id(&ctx.jobs));
            ctx.jobs.push(Job {
                id,
                pgid,
                cmd: cmd.to_string(),
                state: JobState::Stopped,
            });
            let _ = print_line(id, "Stopped", cmd);
            ctx.last_status = 148; // 128 + SIGTSTP
        }
        Wait::Exited(code) => ctx.last_status = code,
        Wait::Signaled(sig) => ctx.last_status = 128 + sig,
    }
}

fn next_id(jobs: &[Job]) -> u32 {
    let mut id = 1;
    while jobs.iter().any(|j| j.id == id) {
        id += 1;
    }
    id
}

fn print_line(id: u32, state: &str, cmd: &str) -> io::Result<()> {
    execute!(stdout(), Print(format!("[{}] {}  {}\r\n", id, state, cmd)))
}

// ─── フォアグラウンド実行 (exec.rs から) ──────────────────────────────────────

/// `command` を独立プロセスグループのフォアグラウンドジョブとして実行する。
/// Ctrl+Z で停止したらジョブ表へ登録してプロンプトに戻る。
///
/// `command` には呼び出し側で `pre_exec` (setpgid＋シグナル既定化) を設定済みであること。
pub fn run_foreground(command: &mut Command, cmd: &str, ctx: &mut ShellContext) -> io::Result<()> {
    let shell = shell_pgid();
    terminal::disable_raw_mode()?;
    let child = command.spawn()?;
    let pid = child.id() as i32;
    // 親側でも setpgid して競合を避ける (子が既に exec 済みでもエラーは無害)。
    unsafe {
        libc::setpgid(pid, pid);
    }
    give_terminal(pid);
    let outcome = wait_job(pid);
    give_terminal(shell);
    terminal::enable_raw_mode()?;
    // child は waitpid で回収済み。std Child は Drop で wait しないのでそのまま落とす。
    settle(outcome, pid, cmd, None, ctx);
    Ok(())
}

// ─── fg / bg / jobs ───────────────────────────────────────────────────────────

/// `jobs` ビルトイン: ジョブ一覧を表示する。
pub fn list(ctx: &ShellContext) -> io::Result<()> {
    for j in &ctx.jobs {
        print_line(j.id, j.state.label(), &j.cmd)?;
    }
    Ok(())
}

/// `fg [id]`: 停止/バックグラウンドのジョブをフォアグラウンドで再開する。
/// id 省略時はピッカーで選ぶ。
pub fn fg(arg: Option<&str>, ctx: &mut ShellContext) -> io::Result<()> {
    let Some(id) = resolve(arg, ctx)? else {
        return Ok(());
    };
    let Some(pos) = ctx.jobs.iter().position(|j| j.id == id) else {
        return Err(io::Error::other(format!("fg: no such job {}", id)));
    };
    let job = ctx.jobs.remove(pos);
    execute!(stdout(), Print(format!("{}\r\n", job.cmd)))?;

    let shell = shell_pgid();
    terminal::disable_raw_mode()?;
    give_terminal(job.pgid);
    unsafe {
        libc::kill(-job.pgid, libc::SIGCONT);
    }
    let outcome = wait_job(job.pgid);
    give_terminal(shell);
    terminal::enable_raw_mode()?;
    settle(outcome, job.pgid, &job.cmd, Some(job.id), ctx);
    Ok(())
}

/// `bg [id]`: 停止ジョブをバックグラウンドで再開する。id 省略時はピッカーで選ぶ。
pub fn bg(arg: Option<&str>, ctx: &mut ShellContext) -> io::Result<()> {
    let Some(id) = resolve(arg, ctx)? else {
        return Ok(());
    };
    let Some(job) = ctx.jobs.iter_mut().find(|j| j.id == id) else {
        return Err(io::Error::other(format!("bg: no such job {}", id)));
    };
    job.state = JobState::Running;
    let (pgid, cmd) = (job.pgid, job.cmd.clone());
    unsafe {
        libc::kill(-pgid, libc::SIGCONT);
    }
    execute!(stdout(), Print(format!("[{}] {} &\r\n", id, cmd)))
}

/// 引数からジョブ番号を解決する。タイプ数を減らすため次の順で判定する:
/// 1. ジョブが 1 つだけなら即確定 (引数があっても)。
/// 2. 引数が `1` / `%1` のような番号ならそれを直接使う。
/// 3. それ以外の引数はタグ (検索語) とみなし、ピッカーをその初期クエリで開く。
///    タグで 1 件に絞れるならピッカーを出さず即確定する。
/// 4. 引数なしならピッカーを開く。
fn resolve(arg: Option<&str>, ctx: &ShellContext) -> io::Result<Option<u32>> {
    if ctx.jobs.is_empty() {
        execute!(stdout(), Print("no jobs\r\n"))?;
        return Ok(None);
    }
    if ctx.jobs.len() == 1 {
        return Ok(Some(ctx.jobs[0].id));
    }
    if let Some(s) = arg {
        let num = s.strip_prefix('%').unwrap_or(s);
        if let Ok(id) = num.parse::<u32>() {
            return Ok(Some(id));
        }
    }
    pick(ctx, arg)
}

/// ジョブをピッカーで選ばせ、選んだジョブ番号を返す。
/// 候補は `[id] State  cmd` 形式でコマンド名から識別できるようにする。
/// `initial` はタグ (初期クエリ)。これで 1 件に絞れるならピッカーを出さず即確定する。
fn pick(ctx: &ShellContext, initial: Option<&str>) -> io::Result<Option<u32>> {
    let cands: Vec<String> = ctx
        .jobs
        .iter()
        .map(|j| format!("[{}] {}  {}", j.id, j.state.label(), j.cmd))
        .collect();
    // タグで 1 件に絞れるなら即確定 (無駄なタイプを省く)。
    if let Some(q) = initial.filter(|q| !q.is_empty()) {
        let matched = selector::filter(&cands, q);
        if matched.len() == 1 {
            let i = cands.iter().position(|c| *c == matched[0]).unwrap();
            return Ok(Some(ctx.jobs[i].id));
        }
    }
    match selector::run_fzf(&cands, initial)? {
        Selection::Chosen(s) => Ok(cands.iter().position(|c| *c == s).map(|i| ctx.jobs[i].id)),
        _ => Ok(None),
    }
}

// ─── バックグラウンドジョブの回収 / 終了処理 ──────────────────────────────────

/// Running なジョブを非ブロッキングで点検し、終了したら除去して通知、
/// 端末入力待ち等で停止したら Stopped に更新して通知する。プロンプト表示前に呼ぶ。
pub fn reap_finished(ctx: &mut ShellContext) -> io::Result<()> {
    let mut done: Vec<(u32, String)> = Vec::new();
    let mut stopped: Vec<(u32, String)> = Vec::new();
    for job in &mut ctx.jobs {
        if job.state != JobState::Running {
            continue;
        }
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(job.pgid, &mut status, libc::WNOHANG | libc::WUNTRACED) };
        if r <= 0 {
            continue; // 0 = まだ動作中 / -1 = 取得不可
        }
        if libc::WIFSTOPPED(status) {
            job.state = JobState::Stopped;
            stopped.push((job.id, job.cmd.clone()));
        } else {
            done.push((job.id, job.cmd.clone()));
        }
    }
    ctx.jobs.retain(|j| !done.iter().any(|(id, _)| *id == j.id));
    for (id, cmd) in done {
        print_line(id, "Done", &cmd)?;
    }
    for (id, cmd) in stopped {
        print_line(id, "Stopped", &cmd)?;
    }
    Ok(())
}

/// シェル終了時に残ったジョブへ SIGCONT＋SIGHUP を送る (宙ぶらりんを避ける)。
pub fn hangup_all(ctx: &ShellContext) {
    for j in &ctx.jobs {
        unsafe {
            libc::kill(-j.pgid, libc::SIGCONT);
            libc::kill(-j.pgid, libc::SIGHUP);
        }
    }
}
