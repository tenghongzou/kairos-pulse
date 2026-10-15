//! 行程排行（feature `processes`，METRICS.md「行程」；商業審計 F4）。
//!
//! 協商了 [`FEATURE_PROCESSES`]（且協商了 `metrics`）時，report 另帶 `processes`：依 CPU 與依
//! 記憶體各取前 N 個行程（[`ProcessList`]）。行程總數是一般指標 `system.process.count`，不在這裡。
//!
//! 隱私：預設只送行程名稱（執行檔名稱，不含路徑與參數）。`command`（命令列）與 `user` 只在 Pulse
//! 明確設定時才送，見 METRICS.md「隱私」。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::validate::{sanitize_display_text, validate_display_text};

/// report 另帶行程排行 `processes`。雙方都宣告、且協商了 `metrics` 時才使用
pub const FEATURE_PROCESSES: &str = "processes";

/// `top_cpu`、`top_memory` 各自的項目數上限（Pulse 預設各 5）
pub const MAX_TOP_PROCESSES: usize = 10;
/// 行程名稱與 `user` 的長度上限（bytes；字元限制同 `hostname`）
pub const MAX_PROCESS_NAME_BYTES: usize = 256;
/// `command` 的長度上限（bytes；字元限制同 `hostname`，Pulse 送出前把禁止字元換成 `?` 並截斷）
pub const MAX_PROCESS_COMMAND_BYTES: usize = 1024;

/// report 的 `processes`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessList {
    /// CPU 使用率最高的行程，由高到低
    #[serde(default)]
    pub top_cpu: Vec<ProcessEntry>,
    /// 常駐記憶體（RSS／Working Set）最高的行程，由高到低
    #[serde(default)]
    pub top_memory: Vec<ProcessEntry>,
}

/// 一個行程
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessEntry {
    pub pid: u32,
    /// 執行檔名稱（不含路徑與參數），例 `postgres`、`sqlservr.exe`
    pub name: String,
    /// 占**整台主機** CPU 的百分比（0～100，與 `system.cpu.usage` 同一尺度；見 [`normalize_process_cpu`]）
    pub cpu_usage: f64,
    /// 常駐記憶體（bytes；Linux RSS、Windows Working Set）
    pub memory: u64,
    /// 執行身分（使用者名稱）。Pulse 的 `processes.include_user = true` 時才送
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 命令列。Pulse 的 `processes.command` 不是 `none` 時才送（見 METRICS.md「隱私」）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// 把 sysinfo 的行程 CPU 使用率（以單一核心為 100%，可超過 100）換算成占整台主機的百分比：
/// 除以邏輯處理器數、夾在 0～100、四捨五入到小數 2 位；不是有限值或處理器數為 0 時為 0
pub fn normalize_process_cpu(per_core_percent: f64, logical_cpus: usize) -> f64 {
    if !per_core_percent.is_finite() || logical_cpus == 0 {
        return 0.0;
    }
    let v = (per_core_percent / logical_cpus as f64).clamp(0.0, 100.0);
    (v * 100.0).round() / 100.0
}

/// Pulse 送出前整理命令列：以空白串接參數、禁止字元換成 `?`、截斷到
/// [`MAX_PROCESS_COMMAND_BYTES`]（不切斷 UTF-8 字元）；結果為空時 `None`
pub fn sanitize_process_command<S: AsRef<str>>(args: &[S]) -> Option<String> {
    let joined = args.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" ");
    sanitize_display_text(&joined, MAX_PROCESS_COMMAND_BYTES)
}

fn validate_entries(list: &str, entries: &[ProcessEntry]) -> Result<(), String> {
    if entries.len() > MAX_TOP_PROCESSES {
        return Err(format!(
            "processes.{list}: at most {MAX_TOP_PROCESSES} entries (got {})",
            entries.len()
        ));
    }
    let mut pids = HashSet::with_capacity(entries.len());
    for e in entries {
        if !pids.insert(e.pid) {
            return Err(format!(
                "processes.{list}: pid {} appears more than once",
                e.pid
            ));
        }
        validate_display_text(
            &format!("processes.{list}[pid {}].name", e.pid),
            &e.name,
            MAX_PROCESS_NAME_BYTES,
        )?;
        if !e.cpu_usage.is_finite() || !(0.0..=100.0).contains(&e.cpu_usage) {
            return Err(format!(
                "processes.{list}[pid {}].cpu_usage out of range 0..=100: {}",
                e.pid, e.cpu_usage
            ));
        }
        if let Some(user) = &e.user {
            validate_display_text(
                &format!("processes.{list}[pid {}].user", e.pid),
                user,
                MAX_PROCESS_NAME_BYTES,
            )?;
        }
        if let Some(cmd) = &e.command {
            validate_display_text(
                &format!("processes.{list}[pid {}].command", e.pid),
                cmd,
                MAX_PROCESS_COMMAND_BYTES,
            )?;
        }
    }
    Ok(())
}

/// 檢查 `processes`（違反時回 `invalid_report`，整筆 report 不保存；Pulse 送出前以同一個函式檢查）：
/// 每個清單最多 [`MAX_TOP_PROCESSES`] 項、同一清單內 `pid` 不重複、`name`／`user`／`command` 的
/// 長度與字元、`cpu_usage` 為 0～100 的有限值。兩個清單可以都是空的（例如取不到行程資訊）。
pub fn validate_process_list(list: &ProcessList) -> Result<(), String> {
    validate_entries("top_cpu", &list.top_cpu)?;
    validate_entries("top_memory", &list.top_memory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, cpu: f64) -> ProcessEntry {
        ProcessEntry {
            pid,
            name: format!("proc{pid}"),
            cpu_usage: cpu,
            memory: 1024 * u64::from(pid),
            user: None,
            command: None,
        }
    }

    #[test]
    fn cpu_is_normalized_to_the_whole_machine() {
        assert_eq!(normalize_process_cpu(350.0, 8), 43.75);
        assert_eq!(normalize_process_cpu(100.0, 3), 33.33);
        assert_eq!(normalize_process_cpu(900.0, 8), 100.0);
        assert_eq!(normalize_process_cpu(-1.0, 8), 0.0);
        assert_eq!(normalize_process_cpu(f64::NAN, 8), 0.0);
        assert_eq!(normalize_process_cpu(50.0, 0), 0.0);
    }

    #[test]
    fn command_is_joined_sanitized_and_truncated() {
        assert_eq!(
            sanitize_process_command(&["/usr/bin/python3", "-m", "http.server"]).as_deref(),
            Some("/usr/bin/python3 -m http.server")
        );
        assert_eq!(
            sanitize_process_command(&["a\tb", "c\u{202e}"]).as_deref(),
            Some("a?b c?")
        );
        let long = sanitize_process_command(&["主".repeat(400)]).unwrap();
        assert!(
            long.len() <= MAX_PROCESS_COMMAND_BYTES && long.len() >= MAX_PROCESS_COMMAND_BYTES - 3
        );
        assert_eq!(sanitize_process_command::<&str>(&[]), None);
        assert_eq!(sanitize_process_command(&["  "]), None);
    }

    #[test]
    fn list_limits() {
        assert_eq!(validate_process_list(&ProcessList::default()), Ok(()));
        let full = ProcessList {
            top_cpu: (1..=MAX_TOP_PROCESSES as u32)
                .map(|p| entry(p, 1.0))
                .collect(),
            top_memory: (1..=MAX_TOP_PROCESSES as u32)
                .map(|p| entry(p, 1.0))
                .collect(),
        };
        assert_eq!(
            validate_process_list(&full),
            Ok(()),
            "兩個清單可以有相同的 pid"
        );

        let mut bad = full.clone();
        bad.top_cpu.push(entry(99, 1.0));
        assert!(validate_process_list(&bad).is_err());

        let mut bad = full.clone();
        bad.top_memory[1].pid = bad.top_memory[0].pid;
        assert!(validate_process_list(&bad).is_err());

        for cpu in [-0.01, 100.01, f64::INFINITY] {
            let bad = ProcessList {
                top_cpu: vec![entry(1, cpu)],
                ..Default::default()
            };
            assert!(validate_process_list(&bad).is_err(), "{cpu}");
        }

        let mut e = entry(1, 0.0);
        e.name = String::new();
        assert!(
            validate_process_list(&ProcessList {
                top_cpu: vec![e.clone()],
                ..Default::default()
            })
            .is_err()
        );
        e.name = "ok".into();
        e.command = Some("x".repeat(MAX_PROCESS_COMMAND_BYTES + 1));
        assert!(
            validate_process_list(&ProcessList {
                top_cpu: vec![e.clone()],
                ..Default::default()
            })
            .is_err()
        );
        e.command = Some("x".repeat(MAX_PROCESS_COMMAND_BYTES));
        e.user = Some("root\n".into());
        assert!(
            validate_process_list(&ProcessList {
                top_cpu: vec![e.clone()],
                ..Default::default()
            })
            .is_err()
        );
        e.user = Some("NT AUTHORITY\\SYSTEM".into());
        assert_eq!(
            validate_process_list(&ProcessList {
                top_cpu: vec![e],
                ..Default::default()
            }),
            Ok(())
        );
    }

    #[test]
    fn json_shape() {
        let list = ProcessList {
            top_cpu: vec![entry(7, 12.5)],
            top_memory: vec![],
        };
        assert_eq!(
            serde_json::to_string(&list).unwrap(),
            r#"{"top_cpu":[{"pid":7,"name":"proc7","cpu_usage":12.5,"memory":7168}],"top_memory":[]}"#
        );
        // 清單可省略；不認得的欄位忽略
        let l: ProcessList = serde_json::from_str(r#"{"top_cpu":[],"future":1}"#).unwrap();
        assert!(l.top_memory.is_empty());
        for bad in [
            r#"{"top_cpu":[{"pid":-1,"name":"a","cpu_usage":1,"memory":1}]}"#,
            r#"{"top_cpu":[{"pid":1,"name":"a","cpu_usage":"1","memory":1}]}"#,
            r#"{"top_cpu":[{"pid":1,"cpu_usage":1,"memory":1}]}"#,
            r#"{"top_cpu":{}}"#,
        ] {
            assert!(serde_json::from_str::<ProcessList>(bad).is_err(), "{bad}");
        }
    }
}
