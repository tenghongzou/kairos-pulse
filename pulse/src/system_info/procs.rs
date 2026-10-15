//! 行程數（`system.process.count`）與行程排行（report 的 `processes`，METRICS.md 第 4.2、5 節）。
//!
//! 兩者各用一個 sysinfo `System`，與取樣 CPU 使用率的 `System` 分開：sysinfo 的行程 CPU 使用率是
//! 「同一個 `System` 兩次刷新行程之間」相對於全機 CPU 時間的比例，任何一次刷新（含只列舉行程的
//! 刷新、刷新 CPU）都會移動基準。因此：
//!
//! - 行程數：每次取樣以 `ProcessRefreshKind::nothing()` 只列舉行程（不讀詳細資料）；
//!   不列舉一般執行緒（`without_tasks`），與 `ps -e` 的列數相同。
//! - 行程排行：只在每 `processes.interval` 的那次取樣刷新 CPU 時間與記憶體（建立時先刷新一次作為基準），
//!   CPU 使用率因此是兩次排行之間的平均。
//!
//! `ProtectProc=invisible` 等限制下只看得到自己的行程（少於 2 個）時，兩者都不送，記一次 `PLS-I8009`。

use std::time::{Duration, Instant};

use kairos_proto::{
    MAX_PROCESS_NAME_BYTES, ProcessEntry, ProcessList, normalize_process_cpu,
    sanitize_display_text, sanitize_process_command,
};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, ThreadKind, UpdateKind};

use super::readings::{CommandMode, ProcessOptions};

/// 排行的間隔容許提早的量：取樣以 `report.interval` 為週期，兩次排行實際相隔可能略少於設定值
const DUE_SLACK: Duration = Duration::from_secs(1);

/// 判定「只看得到自己」的行程數門檻（METRICS.md 第 4.5 節：明顯少於 2 個）
const MIN_VISIBLE_PROCESSES: usize = 2;

/// 排行的一個候選行程（sysinfo 的原始值）
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub pid: u32,
    pub name: String,
    /// sysinfo 的 CPU 使用率（以單一核心為 100%）
    pub cpu_per_core: f64,
    pub memory: u64,
    /// 依 `processes.command` 整理過的命令列（`none` 時一律 `None`）
    pub command: Option<String>,
}

/// 依 CPU、依記憶體各取前 `top` 個（METRICS.md 第 5 節）。
///
/// - 名稱整理後為空的行程略過；CPU 換算成占整台主機的百分比（[`normalize_process_cpu`]）
/// - 依 CPU：使用率高者優先，相同時記憶體大者、再 pid 小者
/// - 依記憶體：記憶體為 0（Windows 取不到受保護行程的 Working Set）的不列入；大者優先，相同時 CPU、pid
pub fn top_processes(candidates: Vec<Candidate>, top: usize, logical_cpus: usize) -> ProcessList {
    let entries: Vec<ProcessEntry> = candidates
        .into_iter()
        .filter_map(|c| {
            Some(ProcessEntry {
                pid: c.pid,
                name: sanitize_display_text(&c.name, MAX_PROCESS_NAME_BYTES)?,
                cpu_usage: normalize_process_cpu(c.cpu_per_core, logical_cpus),
                memory: c.memory,
                user: None,
                command: c.command,
            })
        })
        .collect();
    let mut by_cpu = entries.clone();
    by_cpu.sort_by(|a, b| {
        b.cpu_usage
            .total_cmp(&a.cpu_usage)
            .then(b.memory.cmp(&a.memory))
            .then(a.pid.cmp(&b.pid))
    });
    by_cpu.dedup_by_key(|e| e.pid);
    by_cpu.truncate(top);
    let mut by_memory: Vec<ProcessEntry> = entries.into_iter().filter(|e| e.memory > 0).collect();
    by_memory.sort_by(|a, b| {
        b.memory
            .cmp(&a.memory)
            .then(b.cpu_usage.total_cmp(&a.cpu_usage))
            .then(a.pid.cmp(&b.pid))
    });
    by_memory.dedup_by_key(|e| e.pid);
    by_memory.truncate(top);
    ProcessList {
        top_cpu: by_cpu,
        top_memory: by_memory,
    }
}

/// 依 `processes.command` 整理命令列：`executable` 送執行檔路徑（讀不到時用命令列的第一段，
/// 例如 Linux 上其他使用者的行程沒有 `/proc/<pid>/exe` 的權限）；`full` 送整個命令列
pub fn process_command<S: AsRef<str>>(
    mode: CommandMode,
    exe: Option<&str>,
    args: &[S],
) -> Option<String> {
    match mode {
        CommandMode::None => None,
        CommandMode::Executable => {
            let exe = exe
                .map(str::to_string)
                .filter(|e| !e.trim().is_empty())
                .or_else(|| args.first().map(|a| a.as_ref().to_string()))?;
            sanitize_process_command(&[exe])
        }
        CommandMode::Full => sanitize_process_command(args),
    }
}

/// 一次取樣的行程資訊
#[derive(Debug, Default)]
pub struct ProcessSample {
    /// `system.process.count`；受限時 `None`
    pub count: Option<u64>,
    /// 這次取樣附上的排行（不是排行的那次、受限時 `None`）
    pub top: Option<ProcessList>,
    /// 這次判定為受限（看不到其他使用者的行程）
    pub restricted: bool,
}

/// 行程的取樣狀態（在 blocking 執行緒上使用）
pub struct ProcessCollector {
    opts: ProcessOptions,
    count_sys: System,
    top_sys: System,
    last_top: Instant,
}

impl ProcessCollector {
    /// 建立並刷新一次排行的基準（讀數不用：sysinfo 第一次刷新的行程 CPU 使用率為 0）
    pub fn new(opts: ProcessOptions) -> Self {
        // sysinfo 預設在 Linux 上保留每個行程的 /proc/<pid>/stat 開啟（最多到 fd 上限的一半）；
        // 兩個 System 各數百個 fd 沒有必要，取樣間隔為秒級，每次重新開啟即可
        sysinfo::set_open_files_limit(0);
        let mut c = Self {
            opts,
            count_sys: System::new(),
            top_sys: System::new(),
            last_top: Instant::now(),
        };
        c.refresh_top();
        c
    }

    fn top_refresh_kind(&self) -> ProcessRefreshKind {
        let kind = ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .without_tasks();
        match self.opts.command {
            CommandMode::None => kind,
            // 命令列可能被行程自己改寫（例如 postgres 的狀態），每次都重新讀取
            CommandMode::Executable => kind
                .with_exe(UpdateKind::OnlyIfNotSet)
                .with_cmd(UpdateKind::Always),
            CommandMode::Full => kind.with_cmd(UpdateKind::Always),
        }
    }

    fn refresh_top(&mut self) {
        let kind = self.top_refresh_kind();
        self.top_sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        self.last_top = Instant::now();
    }

    /// 每次取樣呼叫：列舉行程數；到了 `processes.interval` 時另外刷新並回傳排行
    pub fn sample(&mut self) -> ProcessSample {
        self.count_sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        );
        let count = self
            .count_sys
            .processes()
            .values()
            .filter(|p| p.thread_kind() != Some(ThreadKind::Userland))
            .count();
        if count < MIN_VISIBLE_PROCESSES {
            return ProcessSample {
                count: None,
                top: None,
                restricted: true,
            };
        }
        let due = self.last_top.elapsed() + DUE_SLACK >= self.opts.interval;
        let top = due.then(|| {
            self.refresh_top();
            let logical = self.top_sys.cpus().len().max(
                std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1),
            );
            let mode = self.opts.command;
            let candidates = self
                .top_sys
                .processes()
                .values()
                .filter(|p| p.thread_kind() != Some(ThreadKind::Userland))
                .map(|p| {
                    let args: Vec<String> = if mode == CommandMode::None {
                        Vec::new()
                    } else {
                        p.cmd()
                            .iter()
                            .map(|a| a.to_string_lossy().into_owned())
                            .collect()
                    };
                    let exe = p.exe().map(|e| e.to_string_lossy().into_owned());
                    Candidate {
                        pid: p.pid().as_u32(),
                        name: p.name().to_string_lossy().into_owned(),
                        cpu_per_core: f64::from(p.cpu_usage()),
                        memory: p.memory(),
                        command: process_command(mode, exe.as_deref(), &args),
                    }
                })
                .collect();
            top_processes(candidates, self.opts.top, logical)
        });
        ProcessSample {
            count: Some(count as u64),
            top,
            restricted: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(pid: u32, name: &str, cpu: f64, memory: u64) -> Candidate {
        Candidate {
            pid,
            name: name.to_string(),
            cpu_per_core: cpu,
            memory,
            command: None,
        }
    }

    #[test]
    fn top_lists_are_sorted_capped_and_normalized() {
        let list = top_processes(
            vec![
                cand(1, "init", 0.0, 10),
                cand(2211, "postgres", 75.3, 4_294_967_296),
                cand(1873, "java", 25.6, 8_589_934_592),
                cand(9, "csrss.exe", 0.5, 0), // 記憶體取不到：不列入記憶體排行
                cand(10, "", 900.0, 1),       // 名稱為空：略過
                cand(11, "ba\tsh", 1.0, 5),   // 禁止字元換成 ?
            ],
            3,
            8,
        );
        let cpu: Vec<(u32, f64)> = list.top_cpu.iter().map(|e| (e.pid, e.cpu_usage)).collect();
        assert_eq!(cpu, [(2211, 9.41), (1873, 3.2), (11, 0.13)]);
        let mem: Vec<u32> = list.top_memory.iter().map(|e| e.pid).collect();
        assert_eq!(mem, [1873, 2211, 1]);
        assert_eq!(list.top_cpu[2].name, "ba?sh");
        assert!(
            list.top_cpu
                .iter()
                .all(|e| e.user.is_none() && e.command.is_none())
        );
        kairos_proto::validate_process_list(&list).unwrap();
    }

    #[test]
    fn top_respects_the_protocol_limits() {
        let many: Vec<Candidate> = (0..500)
            .map(|i| {
                cand(
                    i,
                    &format!("p{i}{}", "x".repeat(300)),
                    f64::from(i),
                    u64::from(i),
                )
            })
            .collect();
        let list = top_processes(many, kairos_proto::MAX_TOP_PROCESSES, 4);
        assert_eq!(list.top_cpu.len(), 10);
        assert_eq!(list.top_cpu[0].pid, 499);
        // 單核心 499% ÷ 4 核心 = 100%（夾在 0～100）
        assert_eq!(list.top_cpu[0].cpu_usage, 100.0);
        assert_eq!(list.top_memory.len(), 10);
        assert!(
            list.top_cpu
                .iter()
                .all(|e| e.name.len() <= MAX_PROCESS_NAME_BYTES)
        );
        kairos_proto::validate_process_list(&list).unwrap();
        assert_eq!(top_processes(Vec::new(), 5, 4), ProcessList::default());
    }

    #[test]
    fn command_modes_follow_the_privacy_rules() {
        let args = ["/usr/bin/mysql", "-uroot", "-pS3cret"];
        assert_eq!(
            process_command(CommandMode::None, Some("/usr/bin/mysql"), &args),
            None
        );
        // executable：只有執行檔路徑，參數（可能含密碼）不送
        assert_eq!(
            process_command(CommandMode::Executable, Some("/usr/bin/mysql"), &args).as_deref(),
            Some("/usr/bin/mysql")
        );
        // 讀不到 exe（其他使用者的行程）時用命令列的第一段，仍不含參數
        assert_eq!(
            process_command(CommandMode::Executable, None, &args).as_deref(),
            Some("/usr/bin/mysql")
        );
        assert_eq!(
            process_command::<&str>(CommandMode::Executable, None, &[]),
            None
        );
        // full：完整命令列（管理者自行承擔），整理禁止字元並截斷到 1024 bytes
        assert_eq!(
            process_command(CommandMode::Full, None, &args).as_deref(),
            Some("/usr/bin/mysql -uroot -pS3cret")
        );
        let long = vec!["a".repeat(2000)];
        let cmd = process_command(CommandMode::Full, None, &long).unwrap();
        assert_eq!(cmd.len(), kairos_proto::MAX_PROCESS_COMMAND_BYTES);
        assert_eq!(
            process_command(CommandMode::Full, None, &["a\tb"]).as_deref(),
            Some("a?b")
        );
        // 核心執行緒沒有命令列
        assert_eq!(process_command::<&str>(CommandMode::Full, None, &[]), None);
    }

    #[test]
    fn real_processes_are_counted_and_ranked() {
        // 實際的行程清單：建立時是基準，間隔到了才回傳排行
        let mut c = ProcessCollector::new(ProcessOptions {
            interval: Duration::from_secs(3600),
            top: 5,
            command: CommandMode::Full,
        });
        let s = c.sample();
        assert!(s.count.is_some_and(|n| n >= 2) || s.restricted, "{s:?}");
        assert!(s.top.is_none(), "間隔未到不附排行");
        c.opts.interval = Duration::ZERO;
        let s = c.sample();
        if let Some(top) = s.top {
            assert!(!top.top_cpu.is_empty() && top.top_cpu.len() <= 5);
            kairos_proto::validate_process_list(&top).unwrap();
            // 自己（cargo test 的行程）一定在清單中
            let me = std::process::id();
            let all = c.top_sys.processes();
            assert!(all.keys().any(|p| p.as_u32() == me));
        } else {
            assert!(s.restricted);
        }
    }
}
