//! 標準指標的組成（METRICS.md 第 3、4 節）：把一次取樣讀到的原始數值（[`Readings`]）依設定
//! （[`CollectOptions`]）篩選、排序、套上每類上限，轉成 report 的 `metrics`。
//!
//! 這裡只有純函式（不讀系統），讀取在 `system_info.rs` 的 `Extras` 與各平台的輔助函式，
//! 因此每個指標的換算、單位、上限與優先順序都能以假的讀數測試。
//!
//! 輸出的順序就是截斷時的優先順序（`protocol::report_message` 依此「以整組」捨去）：
//! 沒有 label 的指標 → 掛載點（每組 4 個）→ 磁碟 I/O 裝置（每組 2 或 5 個）→ 網路介面
//! （每組 6 個）→ 溫度感測器；各類之內依優先順序由高到低。

use std::collections::HashSet;
use std::time::Duration;

use kairos_proto::{
    METRIC_CPU_LOAD_1M, METRIC_CPU_LOAD_5M, METRIC_CPU_LOAD_15M, METRIC_DISK_IO_TIME,
    METRIC_DISK_READ, METRIC_DISK_READ_OPS, METRIC_DISK_WRITE_OPS, METRIC_DISK_WRITTEN,
    METRIC_FILESYSTEM_AVAILABLE, METRIC_FILESYSTEM_TOTAL, METRIC_FILESYSTEM_USAGE,
    METRIC_FILESYSTEM_USED, METRIC_MEMORY_AVAILABLE, METRIC_NETWORK_ERRORS_RECEIVED,
    METRIC_NETWORK_ERRORS_TRANSMITTED, METRIC_NETWORK_PACKETS_RECEIVED,
    METRIC_NETWORK_PACKETS_TRANSMITTED, METRIC_NETWORK_RECEIVED, METRIC_NETWORK_TRANSMITTED,
    METRIC_PROCESS_COUNT, METRIC_SWAP_TOTAL, METRIC_SWAP_USED, METRIC_TEMPERATURE, METRIC_UPTIME,
    Metric, filesystem_usage,
};

use super::{DiskEntry, dedupe_disks, is_meaningful_disk, is_meaningful_interface, label_value};
use crate::config_manager::AppConfig;

/// 溫度感測器的上限（METRICS.md 第 4.3 節；不開放設定）
pub const MAX_SENSORS: usize = 32;

/// `processes.command`（METRICS.md 第 4.6、8 節）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMode {
    /// 不送命令列（預設）
    None,
    /// 只送執行檔的完整路徑（不含參數）
    Executable,
    /// 送完整命令列（含參數，截斷到 1024 bytes）
    Full,
}

impl CommandMode {
    /// 設定值（已在載入時驗證）；不認得時視為 `none`（最保守）
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "executable" => Self::Executable,
            "full" => Self::Full,
            _ => Self::None,
        }
    }
}

/// 行程數與行程排行的設定（`[processes]`，`enabled = false` 時整個是 `None`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOptions {
    /// 行程排行的間隔（已不小於 `report.interval`）
    pub interval: Duration,
    /// 依 CPU、依記憶體各取幾個
    pub top: usize,
    pub command: CommandMode,
}

/// 採集範圍與上限（`[collect]`、`[processes]`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectOptions {
    pub filesystems: bool,
    pub disk_io: bool,
    pub network: bool,
    pub temperature: bool,
    pub max_filesystems: usize,
    pub max_disks: usize,
    pub max_interfaces: usize,
    /// 額外排除的介面名稱（glob）
    pub interface_exclude: Vec<String>,
    /// `None`：`processes.enabled = false`（不送行程數、不收集行程排行）
    pub processes: Option<ProcessOptions>,
}

impl CollectOptions {
    pub fn from_config(config: &AppConfig) -> Self {
        let c = &config.collect;
        let to_usize = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
        Self {
            filesystems: c.filesystems,
            disk_io: c.disk_io,
            network: c.network,
            temperature: c.temperature,
            max_filesystems: to_usize(c.max_filesystems),
            max_disks: to_usize(c.max_disks),
            max_interfaces: to_usize(c.max_interfaces),
            interface_exclude: c
                .interface_exclude
                .iter()
                .map(|g| g.trim().to_string())
                .collect(),
            processes: config.processes.enabled.then(|| ProcessOptions {
                interval: Duration::from_secs(config.effective_processes_interval()),
                top: to_usize(config.processes.top),
                command: CommandMode::parse(&config.processes.command),
            }),
        }
    }

    /// 預設設定（METRICS.md 第 4.6 節）
    #[cfg(test)]
    pub fn defaults() -> Self {
        Self::from_config(&AppConfig::default_for_tests())
    }
}

/// 一個網路介面的讀數（sysinfo `NetworkData`）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterfaceReading {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    /// 是否有 IPv4 位址（介面的優先順序）
    pub has_ipv4: bool,
}

/// 一個磁碟 I/O 裝置的累計讀數。Windows 沒有次數與忙碌時間（`None`，不送）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskIoReading {
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub read_ops: Option<u64>,
    pub write_ops: Option<u64>,
    pub io_time_ms: Option<u64>,
}

/// 沒有 label 的新指標（CPU 與記憶體總量、已使用在 `Sample` 的固定欄位）
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UnlabeledReadings {
    /// 1、5、15 分鐘平均負載（只有 Linux；其他平台 `None`）
    pub load: Option<[f64; 3]>,
    pub memory_available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// 行程數；`processes.enabled = false` 或受限（`ProtectProc=`）時 `None`
    pub process_count: Option<u64>,
    pub uptime_secs: u64,
}

/// 一次取樣的原始讀數
#[derive(Debug, Clone, Default)]
pub struct Readings {
    pub unlabeled: UnlabeledReadings,
    /// 所有掛載點（尚未篩選）
    pub disks: Vec<DiskEntry>,
    /// 系統磁碟機的掛載點（Linux `/`；Windows 例如 `C:\`），排在第一位
    pub root_mount: String,
    /// 掛載點的磁碟 I/O：`(裝置 label, 讀數)`，順序不拘；`None` 表示這次取不到（不送 `system.disk.*`）
    pub disk_io: Option<Vec<(String, DiskIoReading)>>,
    pub interfaces: Vec<InterfaceReading>,
    pub temperatures: Vec<(String, Option<f32>)>,
}

/// 超過上限而略過的項目（`sysinfo.snapshot` 日誌）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Skipped {
    pub filesystems: Vec<String>,
    pub disks: Vec<String>,
    pub interfaces: Vec<String>,
}

impl Skipped {
    pub fn is_empty(&self) -> bool {
        self.filesystems.is_empty() && self.disks.is_empty() && self.interfaces.is_empty()
    }
}

/// 數值四捨五入到小數 2 位（避免 f32 轉 f64 的精度雜訊）；不是有限值時 `None`
pub(super) fn round2_f64(v: f64) -> Option<serde_json::Number> {
    if !v.is_finite() {
        return None;
    }
    serde_json::Number::from_f64((v * 100.0).round() / 100.0)
}

/// glob 比對（`*` 任意長度、`?` 任一字元，區分大小寫）：`collect.interface_exclude`
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // 經典的回溯比對：記住最後一個 * 的位置
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// 介面是否送出：略過 loopback 與虛擬介面（[`is_meaningful_interface`]）與 `collect.interface_exclude`
pub fn interface_included(name: &str, exclude: &[String]) -> bool {
    is_meaningful_interface(name) && !exclude.iter().any(|g| glob_match(g, name))
}

/// 掛載點的篩選與優先順序（METRICS.md 第 4.3 節）：略過虛擬檔案系統、容量為 0 與同一磁碟的重複
/// 掛載點、label 整理後為空或重複者；`root_mount` 排第一，其餘依容量由大到小（相同時依掛載點）。
/// 回傳的 `mount_point` 已整理成 label 值
pub fn prioritized_filesystems(disks: Vec<DiskEntry>, root_mount: &str) -> Vec<DiskEntry> {
    let meaningful = disks
        .into_iter()
        .filter(|d| is_meaningful_disk(&d.file_system, d.total_space))
        .collect();
    let (kept, _) = dedupe_disks(meaningful);
    let mut seen = HashSet::new();
    let mut out: Vec<DiskEntry> = kept
        .into_iter()
        .filter_map(|mut d| {
            let mount = label_value(&d.mount_point)?;
            seen.insert(mount.clone()).then(|| {
                d.mount_point = mount;
                d
            })
        })
        .collect();
    out.sort_by(|a, b| {
        let root = |d: &DiskEntry| d.mount_point.eq_ignore_ascii_case(root_mount);
        root(b)
            .cmp(&root(a))
            .then(b.total_space.cmp(&a.total_space))
            .then_with(|| a.mount_point.cmp(&b.mount_point))
    });
    out
}

/// 網路介面的篩選與優先順序（METRICS.md 第 4.3 節）：有 IPv4 位址者 → 累計流量大者 → 名稱。
/// 回傳的 `name` 已整理成 label 值（整理後為空或重複者略過）
pub fn prioritized_interfaces(
    interfaces: Vec<InterfaceReading>,
    exclude: &[String],
) -> Vec<InterfaceReading> {
    let mut seen = HashSet::new();
    let mut out: Vec<InterfaceReading> = interfaces
        .into_iter()
        .filter(|i| interface_included(&i.name, exclude))
        .filter_map(|mut i| {
            let name = label_value(&i.name)?;
            seen.insert(name.clone()).then(|| {
                i.name = name;
                i
            })
        })
        .collect();
    out.sort_by(|a, b| {
        let traffic = |i: &InterfaceReading| u128::from(i.rx_bytes) + u128::from(i.tx_bytes);
        b.has_ipv4
            .cmp(&a.has_ipv4)
            .then(traffic(b).cmp(&traffic(a)))
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// 把讀數轉成指標（順序即截斷的優先順序，見模組說明），並回傳超過上限而略過的項目
pub fn build_metrics(readings: Readings, opts: &CollectOptions) -> (Vec<Metric>, Skipped) {
    let mut out = Vec::new();
    let mut skipped = Skipped::default();
    let u = readings.unlabeled;

    // ---- 沒有 label ----
    if let Some(load) = u.load {
        for (name, v) in [
            (METRIC_CPU_LOAD_1M, load[0]),
            (METRIC_CPU_LOAD_5M, load[1]),
            (METRIC_CPU_LOAD_15M, load[2]),
        ] {
            if let Some(v) = round2_f64(v.max(0.0)) {
                out.push(Metric::new(name, v, "1"));
            }
        }
    }
    out.push(Metric::new(
        METRIC_MEMORY_AVAILABLE,
        u.memory_available,
        "By",
    ));
    out.push(Metric::new(METRIC_SWAP_TOTAL, u.swap_total, "By"));
    out.push(Metric::new(
        METRIC_SWAP_USED,
        u.swap_used.min(u.swap_total),
        "By",
    ));
    if let Some(count) = u.process_count {
        out.push(Metric::new(METRIC_PROCESS_COUNT, count, "{processes}"));
    }
    out.push(Metric::new(METRIC_UPTIME, u.uptime_secs, "s"));

    // ---- 掛載點 ----
    let filesystems = prioritized_filesystems(readings.disks, &readings.root_mount);
    if opts.filesystems {
        for (i, fs) in filesystems.iter().enumerate() {
            if i >= opts.max_filesystems {
                skipped.filesystems.push(fs.mount_point.clone());
                continue;
            }
            let labeled = |m: Metric| {
                let m = m.with_label("mountpoint", fs.mount_point.clone());
                match label_value(&fs.file_system) {
                    Some(t) => m.with_label("fstype", t),
                    None => m,
                }
            };
            // used = total − free（free 含保留給 root 的部分）；取不到 free 時以 available 代替
            let free = fs.free_space.unwrap_or(fs.available_space);
            let used = fs.total_space.saturating_sub(free);
            out.push(labeled(Metric::new(
                METRIC_FILESYSTEM_TOTAL,
                fs.total_space,
                "By",
            )));
            out.push(labeled(Metric::new(
                METRIC_FILESYSTEM_AVAILABLE,
                fs.available_space,
                "By",
            )));
            out.push(labeled(Metric::new(METRIC_FILESYSTEM_USED, used, "By")));
            if let Some(usage) =
                filesystem_usage(used, fs.available_space).and_then(serde_json::Number::from_f64)
            {
                out.push(labeled(Metric::new(METRIC_FILESYSTEM_USAGE, usage, "%")));
            }
        }
    }

    // ---- 磁碟 I/O：依掛載點的順序，同一裝置只送一次 ----
    if opts.disk_io
        && let Some(io) = &readings.disk_io
    {
        let mut seen = HashSet::new();
        let mut kept = 0usize;
        for fs in &filesystems {
            let Some(device) = &fs.device else { continue };
            let Some((_, r)) = io.iter().find(|(d, _)| d == device) else {
                continue;
            };
            if !seen.insert(device.clone()) {
                continue;
            }
            if kept >= opts.max_disks {
                skipped.disks.push(device.clone());
                continue;
            }
            kept += 1;
            let labeled = |m: Metric| m.with_label("device", device.clone());
            out.push(labeled(Metric::new(METRIC_DISK_READ, r.read_bytes, "By")));
            out.push(labeled(Metric::new(
                METRIC_DISK_WRITTEN,
                r.written_bytes,
                "By",
            )));
            for (name, v, unit) in [
                (METRIC_DISK_READ_OPS, r.read_ops, "{operations}"),
                (METRIC_DISK_WRITE_OPS, r.write_ops, "{operations}"),
                (METRIC_DISK_IO_TIME, r.io_time_ms, "ms"),
            ] {
                if let Some(v) = v {
                    out.push(labeled(Metric::new(name, v, unit)));
                }
            }
        }
    }

    // ---- 網路介面 ----
    if opts.network {
        let interfaces = prioritized_interfaces(readings.interfaces, &opts.interface_exclude);
        for (i, nic) in interfaces.iter().enumerate() {
            if i >= opts.max_interfaces {
                skipped.interfaces.push(nic.name.clone());
                continue;
            }
            for (name, v, unit) in [
                (METRIC_NETWORK_RECEIVED, nic.rx_bytes, "By"),
                (METRIC_NETWORK_TRANSMITTED, nic.tx_bytes, "By"),
                (METRIC_NETWORK_PACKETS_RECEIVED, nic.rx_packets, "{packets}"),
                (
                    METRIC_NETWORK_PACKETS_TRANSMITTED,
                    nic.tx_packets,
                    "{packets}",
                ),
                (METRIC_NETWORK_ERRORS_RECEIVED, nic.rx_errors, "{errors}"),
                (METRIC_NETWORK_ERRORS_TRANSMITTED, nic.tx_errors, "{errors}"),
            ] {
                out.push(Metric::new(name, v, unit).with_label("device", nic.name.clone()));
            }
        }
    }

    // ---- 溫度：讀不到的感測器略過，同一個 label 只送第一個 ----
    if opts.temperature {
        let mut seen = HashSet::new();
        for (label, temperature) in &readings.temperatures {
            let (Some(sensor), Some(value)) = (
                label_value(label),
                temperature.and_then(|t| round2_f64(f64::from(t))),
            ) else {
                continue;
            };
            if !seen.insert(sensor.clone()) {
                continue;
            }
            if seen.len() > MAX_SENSORS {
                break;
            }
            out.push(Metric::new(METRIC_TEMPERATURE, value, "Cel").with_label("sensor", sensor));
        }
    }
    (out, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(mount: &str, fs: &str, name: &str, total: u64) -> DiskEntry {
        DiskEntry {
            mount_point: mount.to_string(),
            file_system: fs.to_string(),
            kind: "SSD".to_string(),
            name: name.to_string(),
            total_space: total,
            available_space: total / 2,
            free_space: Some(total / 2 + total / 20),
            removable: false,
            read_only: false,
            device: name.strip_prefix("/dev/").map(str::to_string),
        }
    }

    fn nic(name: &str, rx: u64, ipv4: bool) -> InterfaceReading {
        InterfaceReading {
            name: name.to_string(),
            rx_bytes: rx,
            tx_bytes: rx / 10,
            rx_packets: rx / 1000,
            tx_packets: rx / 10000,
            rx_errors: 1,
            tx_errors: 0,
            has_ipv4: ipv4,
        }
    }

    fn io(read: u64) -> DiskIoReading {
        DiskIoReading {
            read_bytes: read,
            written_bytes: read * 2,
            read_ops: Some(read / 512),
            write_ops: Some(read / 256),
            io_time_ms: Some(42),
        }
    }

    fn unlabeled() -> UnlabeledReadings {
        UnlabeledReadings {
            load: Some([2.153, 1.98, 1.754_999]),
            memory_available: 12_706_250_752,
            swap_total: 4_294_967_296,
            swap_used: 0,
            process_count: Some(312),
            uptime_secs: 1_532_941,
        }
    }

    fn names(metrics: &[Metric]) -> Vec<(String, Vec<String>)> {
        metrics
            .iter()
            .map(|m| (m.name.clone(), m.labels.values().cloned().collect()))
            .collect()
    }

    #[test]
    fn every_new_standard_metric_has_the_spec_unit_and_value() {
        // METRICS.md 範例的主機：2 個掛載點、2 個磁碟 I/O 裝置、1 個介面、1 個溫度感測器
        let readings = Readings {
            unlabeled: unlabeled(),
            disks: vec![
                disk("/data", "xfs", "/dev/sdb1", 3_998_639_460_352),
                disk("/", "ext4", "/dev/nvme0n1p2", 502_468_108_288),
                disk("/proc", "proc", "proc", 0),
            ],
            root_mount: "/".into(),
            disk_io: Some(vec![
                ("sdb1".into(), io(4_567_890_123)),
                ("nvme0n1p2".into(), io(912_345_678_848)),
                ("loop0".into(), io(1)),
            ]),
            interfaces: vec![nic("eth0", 98_765_432_101, true), nic("lo", 5, true)],
            temperatures: vec![("coretemp Package id 0".into(), Some(48.5))],
        };
        let (metrics, skipped) = build_metrics(readings, &CollectOptions::defaults());
        assert!(skipped.is_empty(), "{skipped:?}");
        kairos_proto::validate_metrics(&metrics).unwrap();
        let got = names(&metrics);
        let expected: Vec<(&str, Vec<&str>)> = vec![
            ("system.cpu.load_1m", vec![]),
            ("system.cpu.load_5m", vec![]),
            ("system.cpu.load_15m", vec![]),
            ("system.memory.available", vec![]),
            ("system.swap.total", vec![]),
            ("system.swap.used", vec![]),
            ("system.process.count", vec![]),
            ("system.uptime", vec![]),
            ("system.filesystem.total", vec!["ext4", "/"]),
            ("system.filesystem.available", vec!["ext4", "/"]),
            ("system.filesystem.used", vec!["ext4", "/"]),
            ("system.filesystem.usage", vec!["ext4", "/"]),
            ("system.filesystem.total", vec!["xfs", "/data"]),
            ("system.filesystem.available", vec!["xfs", "/data"]),
            ("system.filesystem.used", vec!["xfs", "/data"]),
            ("system.filesystem.usage", vec!["xfs", "/data"]),
            ("system.disk.read", vec!["nvme0n1p2"]),
            ("system.disk.written", vec!["nvme0n1p2"]),
            ("system.disk.read_ops", vec!["nvme0n1p2"]),
            ("system.disk.write_ops", vec!["nvme0n1p2"]),
            ("system.disk.io_time", vec!["nvme0n1p2"]),
            ("system.disk.read", vec!["sdb1"]),
            ("system.disk.written", vec!["sdb1"]),
            ("system.disk.read_ops", vec!["sdb1"]),
            ("system.disk.write_ops", vec!["sdb1"]),
            ("system.disk.io_time", vec!["sdb1"]),
            ("system.network.received", vec!["eth0"]),
            ("system.network.transmitted", vec!["eth0"]),
            ("system.network.packets_received", vec!["eth0"]),
            ("system.network.packets_transmitted", vec!["eth0"]),
            ("system.network.errors_received", vec!["eth0"]),
            ("system.network.errors_transmitted", vec!["eth0"]),
            ("system.temperature", vec!["coretemp Package id 0"]),
        ];
        let expected: Vec<(String, Vec<String>)> = expected
            .into_iter()
            .map(|(n, l)| (n.to_string(), l.into_iter().map(String::from).collect()))
            .collect();
        assert_eq!(got, expected);
        // 單位與種類都與標準指標表相同（validate_metrics 已檢查單位與範圍）
        for m in &metrics {
            let std = kairos_proto::standard_metric(&m.name).expect(&m.name);
            assert_eq!(m.unit, std.unit, "{}", m.name);
        }
        let value = |name: &str, label: &str| {
            metrics
                .iter()
                .find(|m| {
                    m.name == name && (label.is_empty() || m.labels.values().any(|v| v == label))
                })
                .unwrap()
                .value
                .to_string()
        };
        // 負載四捨五入到小數 2 位
        assert_eq!(value("system.cpu.load_1m", ""), "2.15");
        assert_eq!(value("system.cpu.load_15m", ""), "1.75");
        // used = total − free；usage = used ÷ (used + available)，與 df 相同
        let total = 502_468_108_288_u64;
        let free = total / 2 + total / 20;
        let used = total - free;
        assert_eq!(value("system.filesystem.used", "/"), used.to_string());
        assert_eq!(
            value("system.filesystem.usage", "/"),
            kairos_proto::filesystem_usage(used, total / 2)
                .unwrap()
                .to_string()
        );
        // 計數器送累計值（不換算速率）
        assert_eq!(value("system.disk.read", "sdb1"), "4567890123");
        assert_eq!(value("system.disk.written", "sdb1"), "9135780246");
        assert_eq!(value("system.disk.io_time", "sdb1"), "42");
        assert_eq!(value("system.network.received", "eth0"), "98765432101");
        assert_eq!(value("system.uptime", ""), "1532941");
    }

    #[test]
    fn platform_gaps_are_omitted_not_zero() {
        // Windows：沒有負載、沒有讀寫次數與忙碌時間；行程受限時不送行程數；取不到 free 時以 available 算
        let mut d = disk("C:\\", "NTFS", "", 1000);
        d.free_space = None;
        d.device = Some("C:".into());
        let readings = Readings {
            unlabeled: UnlabeledReadings {
                load: None,
                process_count: None,
                swap_used: 9,
                swap_total: 5,
                ..unlabeled()
            },
            disks: vec![d],
            root_mount: "C:\\".into(),
            disk_io: Some(vec![(
                "C:".into(),
                DiskIoReading {
                    read_bytes: 7,
                    written_bytes: 8,
                    ..Default::default()
                },
            )]),
            interfaces: vec![],
            temperatures: vec![],
        };
        let (metrics, _) = build_metrics(readings, &CollectOptions::defaults());
        kairos_proto::validate_metrics(&metrics).unwrap();
        let all: Vec<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
        assert!(
            !all.iter().any(|n| n.starts_with("system.cpu.load")),
            "{all:?}"
        );
        assert!(!all.contains(&"system.process.count"));
        assert!(!all.contains(&"system.disk.read_ops") && !all.contains(&"system.disk.io_time"));
        assert!(all.contains(&"system.disk.read") && all.contains(&"system.disk.written"));
        // 置換空間已使用不大於總量
        let swap_used = metrics
            .iter()
            .find(|m| m.name == "system.swap.used")
            .unwrap();
        assert_eq!(swap_used.value.as_u64(), Some(5));
        let used = metrics
            .iter()
            .find(|m| m.name == "system.filesystem.used")
            .unwrap();
        assert_eq!(used.value.as_u64(), Some(500));

        // 取不到磁碟 I/O（disk_io = None）時不送 system.disk.*
        let readings = Readings {
            unlabeled: unlabeled(),
            disks: vec![disk("/", "ext4", "/dev/sda1", 10)],
            root_mount: "/".into(),
            disk_io: None,
            ..Default::default()
        };
        let (metrics, _) = build_metrics(readings, &CollectOptions::defaults());
        assert!(!metrics.iter().any(|m| m.name.starts_with("system.disk.")));
    }

    #[test]
    fn caps_and_priorities_follow_the_spec() {
        // 40 個掛載點、40 個裝置、40 個介面、40 個感測器：各取 16／8／16／32，依優先順序
        let mut disks: Vec<DiskEntry> = (0..40)
            .map(|i| {
                disk(
                    &format!("/m{i:02}"),
                    "ext4",
                    &format!("/dev/sd{i:02}"),
                    1000 + i,
                )
            })
            .collect();
        disks.push(disk("/", "ext4", "/dev/root", 1)); // 最小，但 / 一定排第一
        let readings = Readings {
            unlabeled: unlabeled(),
            disk_io: Some(
                (0..40)
                    .map(|i| (format!("sd{i:02}"), io(i)))
                    .chain([("root".to_string(), io(1))])
                    .collect(),
            ),
            disks,
            root_mount: "/".into(),
            interfaces: (0..40)
                .map(|i| nic(&format!("en{i:02}"), 1000 + i, i % 2 == 0))
                .collect(),
            temperatures: (0..40).map(|i| (format!("s{i:02}"), Some(40.0))).collect(),
        };
        let (metrics, skipped) = build_metrics(readings, &CollectOptions::defaults());
        kairos_proto::validate_metrics(&metrics).unwrap();
        // 最壞情況的合計：11 個無 label（含 CPU 與記憶體總量／已使用 3 個，在 Sample 的固定欄位）
        assert_eq!(metrics.len() + 3, 11 + 16 * 4 + 8 * 5 + 16 * 6 + 32);
        assert!(metrics.len() + 3 <= kairos_proto::MAX_METRICS);
        let labels = |name: &str| -> Vec<String> {
            metrics
                .iter()
                .filter(|m| m.name == name)
                .map(|m| {
                    m.labels
                        .get("mountpoint")
                        .or_else(|| m.labels.get("device"))
                        .or_else(|| m.labels.get("sensor"))
                        .unwrap()
                        .clone()
                })
                .collect()
        };
        let mounts = labels("system.filesystem.total");
        assert_eq!(mounts.len(), 16);
        assert_eq!(mounts[0], "/");
        assert_eq!(mounts[1], "/m39", "其餘依容量由大到小");
        assert_eq!(mounts[15], "/m25");
        // 磁碟 I/O 依掛載點的順序
        assert_eq!(
            labels("system.disk.read"),
            [
                "root", "sd39", "sd38", "sd37", "sd36", "sd35", "sd34", "sd33"
            ]
        );
        // 介面：有 IPv4 者優先，其次累計流量大者
        let nics = labels("system.network.received");
        assert_eq!(nics.len(), 16);
        assert_eq!(nics[0], "en38");
        assert!(
            nics.iter().all(|n| n[2..].parse::<u32>().unwrap() % 2 == 0),
            "{nics:?}"
        );
        assert_eq!(labels("system.temperature").len(), MAX_SENSORS);
        assert_eq!(skipped.filesystems.len(), 41 - 16);
        assert_eq!(skipped.disks.len(), 41 - 8);
        assert_eq!(skipped.interfaces.len(), 40 - 16);

        // 設定可以關閉每一類或把上限設為 0
        let readings = Readings {
            unlabeled: unlabeled(),
            disks: vec![disk("/", "ext4", "/dev/sda1", 10)],
            root_mount: "/".into(),
            disk_io: Some(vec![("sda1".into(), io(1))]),
            interfaces: vec![nic("eth0", 1, true)],
            temperatures: vec![("t".into(), Some(1.0))],
        };
        let opts = CollectOptions {
            filesystems: false,
            max_disks: 0,
            network: false,
            temperature: false,
            ..CollectOptions::defaults()
        };
        let (metrics, _) = build_metrics(readings, &opts);
        assert!(metrics.iter().all(|m| m.labels.is_empty()), "{metrics:?}");
        assert_eq!(metrics.len(), 8);
    }

    #[test]
    fn filesystem_filters_and_labels() {
        let readings = Readings {
            unlabeled: unlabeled(),
            disks: vec![
                disk("/", "ext4", "/dev/sda1", 1000),
                disk("/opt/pulse/logs", "ext4", "/dev/sda1", 1000), // 重複掛載點
                disk("/run", "tmpfs", "tmpfs", 1000),               // 虛擬
                disk("/mnt/a\nb", "ext4", "/dev/sdc1", 5),          // 禁止字元換成 ?
                disk(" ", "ext4", "/dev/sdd1", 5),                  // label 為空：略過
            ],
            root_mount: "/".into(),
            ..Default::default()
        };
        let (metrics, _) = build_metrics(readings, &CollectOptions::defaults());
        kairos_proto::validate_metrics(&metrics).unwrap();
        let mounts: Vec<&str> = metrics
            .iter()
            .filter(|m| m.name == "system.filesystem.total")
            .map(|m| m.labels["mountpoint"].as_str())
            .collect();
        assert_eq!(mounts, ["/", "/mnt/a?b"]);
    }

    #[test]
    fn temperatures_skip_unreadable_sensors_and_round() {
        let readings = Readings {
            unlabeled: unlabeled(),
            temperatures: vec![
                ("coretemp Package id 0".into(), Some(47.83)),
                ("acpitz".into(), None),
                ("nvme".into(), Some(f32::NAN)),
                (" ".into(), Some(1.0)),
                ("coretemp Package id 0".into(), Some(1.0)), // 重複的 label 只取第一個
            ],
            ..Default::default()
        };
        let (metrics, _) = build_metrics(readings, &CollectOptions::defaults());
        let temps: Vec<String> = metrics
            .iter()
            .filter(|m| m.name == "system.temperature")
            .map(|m| m.value.to_string())
            .collect();
        // f32 的精度雜訊被去掉
        assert_eq!(temps, ["47.83"]);
    }

    #[test]
    fn interface_exclude_globs() {
        assert!(glob_match("tap*", "tap0"));
        assert!(glob_match("tap*", "tap"));
        assert!(!glob_match("tap*", "xtap0"));
        assert!(glob_match("wg?", "wg0"));
        assert!(!glob_match("wg?", "wg10"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*vpn*", "corp-vpn-1"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
        assert!(glob_match("乙太網路*", "乙太網路 2"));
        assert!(!glob_match("Eth*", "eth0"), "區分大小寫");
        let exclude = vec!["tap*".to_string(), "wg?".to_string()];
        assert!(interface_included("eth0", &exclude));
        assert!(!interface_included("tap3", &exclude));
        assert!(!interface_included("wg0", &exclude));
        assert!(
            !interface_included("docker0", &exclude),
            "內建的虛擬介面規則仍適用"
        );
        let kept = prioritized_interfaces(
            vec![
                nic("tap3", 9, true),
                nic("eth0", 1, false),
                nic("wg0", 9, true),
            ],
            &exclude,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "eth0");
    }

    #[test]
    fn worst_case_label_values_stay_within_the_metric_count() {
        // 每個 label 256 bytes 也不影響數量（單行長度由 protocol 的截斷處理）
        let long = |p: &str, i: usize| format!("{p}{i:02}{}", "x".repeat(300));
        let readings = Readings {
            unlabeled: unlabeled(),
            disks: (0..20)
                .map(|i| disk(&long("/m", i), &long("fs", i), &format!("/dev/d{i}"), 10))
                .collect(),
            root_mount: "/".into(),
            disk_io: Some((0..20).map(|i| (format!("d{i}"), io(1))).collect()),
            interfaces: (0..20).map(|i| nic(&long("n", i), 1, true)).collect(),
            temperatures: (0..40).map(|i| (long("t", i), Some(1.0))).collect(),
        };
        let (metrics, _) = build_metrics(readings, &CollectOptions::defaults());
        kairos_proto::validate_metrics(&metrics).unwrap();
        assert!(metrics.iter().all(|m| {
            m.labels
                .values()
                .all(|v| v.len() <= kairos_proto::MAX_LABEL_VALUE_BYTES)
        }));
        assert_eq!(metrics.len() + 3, 243);
    }
}
