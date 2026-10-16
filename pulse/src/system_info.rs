use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sysinfo::{Components, Disks, MINIMUM_CPU_UPDATE_INTERVAL, NetworkData, Networks, System};
use tokio::sync::mpsc;
use tokio::time::{self, Instant, MissedTickBehavior};

use kairos_proto::{MAX_LABEL_VALUE_BYTES, Metric, ProcessList, sanitize_display_text};

use crate::log_throttle::LogThrottle;
use crate::protocol::{is_forbidden_display_char, now_ts};

mod diskstats;
pub mod host;
pub mod inventory;
mod procs;
mod readings;

pub use readings::CollectOptions;

/// 一個 MB 的位元組數，用來把 sysinfo 回傳的 bytes 換算成 MB。
const BYTES_PER_MB: u64 = 1024 * 1024;

/// 一次系統資訊取樣，由收集 task 產生、經 mpsc 交給連線 task 組成 report 訊息。
///
/// 啟用本機緩衝（A5）時以 JSON 原樣存進緩衝（`spool`），送出時才依協商的格式組成 report。
/// 0.8.0 新增的欄位一律可省略（`serde(default)`），0.7 寫入的緩衝記錄仍可讀取。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// 取樣時間（RFC 3339 UTC 毫秒），作為 report 的 `ts`
    pub ts: String,
    /// 全機 CPU 平均使用率（百分比）
    pub cpu_usage: f32,
    /// 總記憶體（MB）
    pub total_memory: u64,
    /// 已使用記憶體（MB）
    pub used_memory: u64,
    /// 總記憶體（bytes；協定 v2 的 `system.memory.total`）
    pub total_memory_bytes: u64,
    /// 已使用記憶體（bytes；協定 v2 的 `system.memory.used`）
    pub used_memory_bytes: u64,
    /// CPU 與記憶體總量／已使用以外的標準指標（A4、F4；只在協定 v2 協商了 `metrics` 時送出，v1 不送）。
    /// 順序即截斷時的優先順序（見 `readings` 模組）
    pub extra: Vec<Metric>,
    /// 行程排行（F4；每 `processes.interval` 的那次取樣才有，只在協商了 `processes` 時送出）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<ProcessList>,
}

/// label 值：整理成符合協定限制的字串（禁止字元換成 `?`、截到 256 bytes），空的回傳 `None`
fn label_value(raw: &str) -> Option<String> {
    sanitize_display_text(raw, MAX_LABEL_VALUE_BYTES)
}

/// 系統磁碟機的掛載點：Linux `/`；Windows 為 `%SystemDrive%\`（通常 `C:\`）
fn root_mount() -> String {
    if cfg!(windows) {
        let drive = std::env::var("SystemDrive")
            .ok()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| "C:".to_string());
        format!("{}\\", drive.trim().trim_end_matches('\\'))
    } else {
        "/".to_string()
    }
}

/// 網路介面的讀數（累計值；介面優先順序用的 IPv4 判斷）
fn interface_reading(name: &str, data: &NetworkData) -> readings::InterfaceReading {
    readings::InterfaceReading {
        name: name.to_string(),
        rx_bytes: data.total_received(),
        tx_bytes: data.total_transmitted(),
        rx_packets: data.total_packets_received(),
        tx_packets: data.total_packets_transmitted(),
        rx_errors: data.total_errors_on_received(),
        tx_errors: data.total_errors_on_transmitted(),
        has_ipv4: data.ip_networks().iter().any(|n| n.addr.is_ipv4()),
    }
}

/// 部分系統資訊因權限或平台限制無法取得（`PLS-I8009`，METRICS.md 第 4.5 節）：
/// 每個項目在這個行程中只記一次 INFO
pub(crate) fn report_limited(item: &'static str, detail: &str) {
    static LOGGED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut logged = LOGGED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if logged.contains(&item) {
        return;
    }
    logged.push(item);
    info!(
        "{}部分系統資訊無法取得，不送 {item}：{detail}",
        crate::codes::tag(
            crate::codes::SYSINFO_LIMITED,
            Some(crate::codes::I_SYSINFO_LIMITED)
        )
    );
}

/// 掛載點的裝置對應到 `system.disk.*` 的 `device` label（Linux 為 diskstats 的名稱，Windows 為磁碟機代號）
struct DeviceResolver {
    #[cfg(target_os = "linux")]
    dm_names: Vec<(String, String)>,
}

impl DeviceResolver {
    fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            dm_names: diskstats::read_dm_names(),
        }
    }

    #[cfg(target_os = "linux")]
    fn resolve(&self, source: &str, _mount_point: &str) -> Option<String> {
        diskstats::resolve_linux_device(source, &self.dm_names)
    }

    #[cfg(not(target_os = "linux"))]
    fn resolve(&self, _source: &str, mount_point: &str) -> Option<String> {
        diskstats::windows_device_label(mount_point)
    }
}

/// 掛載點的可用空間（含保留給 root 的部分，`statvfs` 的 `f_bfree`；Windows 為
/// `GetDiskFreeSpaceExW` 的 `TotalNumberOfFreeBytes`）。`system.filesystem.used` = total − free
#[cfg(unix)]
fn filesystem_free_bytes(mount_point: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(mount_point.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs 為 POD，以 0 初始化合法；path 以 NUL 結尾且在呼叫期間有效
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut st) };
    if rc != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)] // f_bfree／f_frsize 的型別依平台而異
    let free = (st.f_bfree as u64).checked_mul(st.f_frsize as u64)?;
    Some(free)
}

#[cfg(windows)]
fn filesystem_free_bytes(mount_point: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = mount_point
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut caller, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: wide 以 NUL 結尾且在呼叫期間有效；三個輸出參數指向各自的 u64（ULARGE_INTEGER）
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut caller, &mut total, &mut free) };
    (ok != 0).then_some(free)
}

#[cfg(not(any(unix, windows)))]
fn filesystem_free_bytes(_mount_point: &std::path::Path) -> Option<u64> {
    None
}

/// 取樣磁碟、網路、溫度與行程用的 sysinfo 物件（第一次在 blocking 執行緒建立，之後沿用）
struct Extras {
    opts: Arc<CollectOptions>,
    disks: Disks,
    networks: Networks,
    components: Components,
    resolver: DeviceResolver,
    /// `processes.enabled = false` 時 `None`
    procs: Option<procs::ProcessCollector>,
    /// 超過上限而略過的項目：第一次取樣記一則 INFO，之後每 24 小時最多一則（項目改變時也記）
    skipped_log: LogThrottle,
    last_skipped: readings::Skipped,
}

/// `sysinfo.snapshot` 中略過項目的提醒間隔
const SKIPPED_LOG_WINDOW: Duration = Duration::from_secs(24 * 3600);

impl Extras {
    fn new(opts: Arc<CollectOptions>) -> Self {
        let wants_disks = opts.filesystems || opts.disk_io;
        Self {
            disks: if wants_disks {
                Disks::new_with_refreshed_list()
            } else {
                Disks::new()
            },
            networks: if opts.network {
                Networks::new_with_refreshed_list()
            } else {
                Networks::new()
            },
            components: if opts.temperature {
                Components::new_with_refreshed_list()
            } else {
                Components::new()
            },
            resolver: DeviceResolver::new(),
            procs: opts.processes.clone().map(procs::ProcessCollector::new),
            skipped_log: LogThrottle::new(SKIPPED_LOG_WINDOW),
            last_skipped: readings::Skipped::default(),
            opts,
        }
    }

    /// 磁碟 I/O 的讀數（取不到時 `None` 並記一次 `PLS-I8009`）
    fn disk_io(&self) -> Option<Vec<(String, readings::DiskIoReading)>> {
        if !self.opts.disk_io || self.opts.max_disks == 0 {
            return None;
        }
        #[cfg(target_os = "linux")]
        {
            let Some(stats) = diskstats::read_linux_diskstats() else {
                report_limited("system.disk.*", "無法讀取 /proc/diskstats");
                return None;
            };
            Some(stats.into_iter().collect())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let io: Vec<(String, readings::DiskIoReading)> = self
                .disks
                .iter()
                .filter_map(|d| {
                    let device =
                        diskstats::windows_device_label(&d.mount_point().to_string_lossy())?;
                    let u = d.usage();
                    Some((
                        device,
                        readings::DiskIoReading {
                            read_bytes: u.total_read_bytes,
                            written_bytes: u.total_written_bytes,
                            ..Default::default()
                        },
                    ))
                })
                .collect();
            // IOCTL_DISK_PERFORMANCE 打不開時 sysinfo 的讀數一律為 0（虛擬服務帳號下待實測）
            if io
                .iter()
                .all(|(_, r)| r.read_bytes == 0 && r.written_bytes == 0)
            {
                report_limited(
                    "system.disk.*",
                    "讀不到磁碟的效能計數器（IOCTL_DISK_PERFORMANCE），可能是服務帳號的權限不足",
                );
                return None;
            }
            Some(io)
        }
    }

    /// 刷新（含新增與移除的磁碟、介面、感測器）並轉成指標；到了 `processes.interval` 時另附行程排行
    fn refresh_and_read(&mut self, sys: &System) -> (Vec<Metric>, Option<ProcessList>) {
        let opts = self.opts.clone();
        if opts.filesystems || opts.disk_io {
            self.disks.refresh(true);
        }
        if opts.network {
            self.networks.refresh(true);
        }
        if opts.temperature {
            self.components.refresh(true);
        }
        let (process_count, processes) = match &mut self.procs {
            Some(p) => {
                let s = p.sample();
                if s.restricted {
                    report_limited(
                        "system.process.count、processes",
                        "只看得到 Pulse 自己的行程（例如 systemd 的 ProtectProc=invisible）",
                    );
                }
                (s.count, s.top)
            }
            None => (None, None),
        };
        let resolver = &self.resolver;
        let disks = self
            .disks
            .iter()
            .map(|d| DiskEntry::from_disk(d, resolver))
            .collect();
        let mut interfaces: Vec<readings::InterfaceReading> = self
            .networks
            .iter()
            .map(|(name, data)| interface_reading(name, data))
            .collect();
        interfaces.sort_by(|a, b| a.name.cmp(&b.name));
        let temperatures = self
            .components
            .iter()
            .map(|c| (c.label().to_string(), c.temperature()))
            .collect();
        let load = cfg!(target_os = "linux").then(|| {
            let l = System::load_average();
            [l.one, l.five, l.fifteen]
        });
        let readings = readings::Readings {
            unlabeled: readings::UnlabeledReadings {
                load,
                memory_available: sys.available_memory(),
                swap_total: sys.total_swap(),
                swap_used: sys.used_swap(),
                process_count,
                uptime_secs: System::uptime(),
            },
            disks,
            root_mount: root_mount(),
            disk_io: self.disk_io(),
            interfaces,
            temperatures,
        };
        let (metrics, skipped) = readings::build_metrics(readings, &opts);
        self.log_skipped(skipped);
        (metrics, processes)
    }

    /// `sysinfo.snapshot`：超過每類上限而略過的項目（METRICS.md 第 4.3 節）
    fn log_skipped(&mut self, skipped: readings::Skipped) {
        if skipped.is_empty() {
            self.last_skipped = skipped;
            return;
        }
        let changed = skipped != self.last_skipped;
        if changed {
            self.skipped_log = LogThrottle::new(SKIPPED_LOG_WINDOW);
        }
        if self.skipped_log.check(Instant::now()).is_some() {
            let list = |v: &[String]| {
                if v.is_empty() {
                    "—".to_string()
                } else {
                    v.join("、")
                }
            };
            info!(
                "{}超過每類上限，下列項目不送（collect.max_filesystems={}、max_disks={}、max_interfaces={}）：\
                 掛載點 {}；磁碟 I/O 裝置 {}；網路介面 {}  skipped_filesystems={} skipped_disks={} skipped_interfaces={}",
                crate::codes::tag(crate::codes::SYSINFO_SNAPSHOT, None),
                self.opts.max_filesystems,
                self.opts.max_disks,
                self.opts.max_interfaces,
                list(&skipped.filesystems),
                list(&skipped.disks),
                list(&skipped.interfaces),
                skipped.filesystems.len(),
                skipped.disks.len(),
                skipped.interfaces.len()
            );
        }
        self.last_skipped = skipped;
    }
}

/// 計算全機 CPU 平均使用率。
///
/// sysinfo 需要在兩次刷新之間至少間隔 `MINIMUM_CPU_UPDATE_INTERVAL`
/// 才能取得有意義的 CPU 使用率，否則讀數會是 0 或不準確。
fn average_cpu_usage(sys: &System) -> f32 {
    let cpus = sys.cpus();
    if cpus.is_empty() {
        return 0.0;
    }
    let total: f32 = cpus.iter().map(|cpu| cpu.cpu_usage()).sum();
    total / cpus.len() as f32
}

// hello 中 `hostname` 的長度上限（bytes），見 PROTOCOL.md；與 Kairos 的驗證共用同一個常數。
use kairos_proto::MAX_HOSTNAME_BYTES;

/// 主機名稱（僅供顯示），已符合協定限制（見 `normalize_hostname`）。
///
/// 內部為阻塞的系統呼叫，在 async 環境中請用 `spawn_blocking` 呼叫。
pub fn hostname() -> String {
    normalize_hostname(System::host_name())
}

/// 讓主機名稱符合 hello 的限制：
/// - 去除前後空白（含結尾的 `\r\n`）
/// - 其餘控制字元（Cc：C0、DEL、C1）、格式字元（Cf，例如 U+200B、U+202E、U+FEFF）、
///   行／段落分隔（U+2028、U+2029）、Hangul filler、變體選擇字元與私用區等替換為 `?`
///   （與 Kairos 的 hostname 驗證規則相同，見 [`is_forbidden_display_char`]）
/// - 取不到或結果為空時為 `"unknown"`
/// - 超過 255 bytes 時在字元邊界截斷
fn normalize_hostname(name: Option<String>) -> String {
    let name: String = name
        .unwrap_or_default()
        .trim()
        .chars()
        .map(|c| if is_forbidden_display_char(c) { '?' } else { c })
        .collect();
    if name.is_empty() {
        return "unknown".to_string();
    }
    if name.len() <= MAX_HOSTNAME_BYTES {
        return name;
    }
    let mut end = MAX_HOSTNAME_BYTES;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

/// 虛擬或偽檔案系統（不對應實體儲存空間），啟動時的 INFO 磁碟清單中略過（完整清單見 debug）。
///
/// 例如 docker 主機上每個容器都有一個 overlay 掛載點，snap 套件是 squashfs。比對時不分大小寫；
/// `fuse.` 開頭的 FUSE 子類型只略過已知的虛擬類型（例如 `fuse.lxcfs`），sshfs 等仍會列出。
const VIRTUAL_FILESYSTEMS: &[&str] = &[
    "overlay",
    "overlayfs",
    "aufs",
    "tmpfs",
    "devtmpfs",
    "ramfs",
    "squashfs",
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "mqueue",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "fusectl",
    "hugetlbfs",
    "autofs",
    "nsfs",
    "efivarfs",
    "binfmt_misc",
    "rpc_pipefs",
    "fuse.lxcfs",
    "fuse.portal",
    "fuse.gvfsd-fuse",
    "nullfs",
    "devfs",
];

/// 檔案系統類型是否為虛擬檔案系統（見 [`VIRTUAL_FILESYSTEMS`]）。
fn is_virtual_filesystem(fs_type: &str) -> bool {
    let fs_type = fs_type.trim();
    VIRTUAL_FILESYSTEMS
        .iter()
        .any(|v| v.eq_ignore_ascii_case(fs_type))
}

/// 磁碟是否列在啟動時的 INFO 清單：略過虛擬檔案系統與總容量為 0 的掛載點。
fn is_meaningful_disk(fs_type: &str, total_space: u64) -> bool {
    total_space > 0 && !is_virtual_filesystem(fs_type)
}

/// 虛擬網路介面的名稱前綴（docker / bridge / veth pair / CNI 等），啟動時的 INFO 清單中略過。
const VIRTUAL_INTERFACE_PREFIXES: &[&str] = &[
    "veth", "docker", "br-", "virbr", "cni", "flannel", "cali", "vxlan", "tunl", "kube-", "lxcbr",
    "lxdbr",
];

/// 迴路介面的完整名稱（Linux `lo`、macOS / BSD `lo0`）。
const LOOPBACK_INTERFACES: &[&str] = &["lo", "lo0"];

/// 網路介面是否列在啟動時的 INFO 清單：略過迴路介面與虛擬介面（見 [`VIRTUAL_INTERFACE_PREFIXES`]）。
///
/// 前綴比對區分大小寫（Linux 介面名稱區分大小寫；例如 Hyper-V 的 `vEthernet (...)` 可能是實際對外的
/// 介面，不會被 `veth` 誤判）。
fn is_meaningful_interface(name: &str) -> bool {
    !LOOPBACK_INTERFACES.contains(&name)
        && !VIRTUAL_INTERFACE_PREFIXES
            .iter()
            .any(|p| name.starts_with(p))
}

/// 取不到的系統資訊（`None` 或空字串）顯示為「未知」。
fn display_or_unknown(value: Option<String>) -> String {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "未知".to_string())
}

/// 磁碟容量的顯示文字（十進位單位，與 `df -H` 相同）：
/// 1 GB 以上顯示到小數 1 位（`39.4 GB`），1 MB～1 GB 用整數 MB（`536 MB`），
/// 再小則用 KB / B，因此容量不為 0 的掛載點不會顯示成 `0 GB`。
fn format_disk_bytes(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    if bytes >= GB {
        // u64 → f64 在 2^53 以上會失去精度，顯示到小數 1 位不受影響
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 一個掛載點（啟動時的磁碟清單、`system.filesystem.*`、`system.disk.*` 與資產共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiskEntry {
    mount_point: String,
    file_system: String,
    kind: String,
    name: String,
    total_space: u64,
    available_space: u64,
    /// 含保留給 root 的可用空間（`statvfs` 的 `f_bfree`）；取不到時 `None`
    free_space: Option<u64>,
    removable: bool,
    read_only: bool,
    /// `system.disk.*` 的 `device` label（Linux 為 diskstats 的名稱、Windows 為磁碟機代號）
    device: Option<String>,
}

impl DiskEntry {
    fn from_disk(disk: &sysinfo::Disk, resolver: &DeviceResolver) -> Self {
        let mount_point = disk.mount_point().display().to_string();
        let name = disk.name().to_string_lossy().into_owned();
        Self {
            device: resolver.resolve(&name, &mount_point),
            free_space: filesystem_free_bytes(disk.mount_point()),
            mount_point,
            file_system: disk.file_system().to_string_lossy().into_owned(),
            kind: disk.kind().to_string(),
            name,
            total_space: disk.total_space(),
            available_space: disk.available_space(),
            removable: disk.is_removable(),
            read_only: disk.is_read_only(),
        }
    }

    /// 日誌中的一行（不使用 Rust Debug 格式，路徑與名稱不加引號）。
    fn line(&self) -> String {
        let name = self.name.trim();
        format!(
            "掛載位置: {}, 檔案系統: {}, 硬碟種類: {}, 磁碟名稱: {}, 總容量: {}, 可用空間: {}",
            self.mount_point,
            self.file_system,
            self.kind,
            if name.is_empty() { "未知" } else { name },
            format_disk_bytes(self.total_space),
            format_disk_bytes(self.available_space),
        )
    }

    /// 判斷「同一個磁碟」的鍵：磁碟名稱＋檔案系統＋總容量。
    /// 磁碟名稱為空時無法辨識是哪個裝置，回傳 `None`（不合併）。
    fn identity(&self) -> Option<(&str, &str, u64)> {
        let name = self.name.trim();
        (!name.is_empty()).then_some((name, self.file_system.trim(), self.total_space))
    }
}

/// 去除重複的掛載點（N1）：同一個「磁碟名稱＋檔案系統＋總容量」只保留掛載路徑最短的那個
/// （長度相同時取字典序較小者），其餘視為同一個磁碟的重複掛載點。
///
/// systemd 硬化下 `ReadWritePaths=`、`PrivateTmp=yes` 會在服務的 mount namespace 中建立
/// 與 `/` 同一個裝置的 bind mount（例如 `/opt/pulse/logs`、`/tmp`、`/var/tmp`）；btrfs 的多個
/// subvolume 也會以同一個裝置出現。磁碟名稱為空（無法辨識裝置）的項目一律保留。
///
/// 回傳 `(保留的項目, [(被合併的項目, 保留下來的掛載位置)])`，兩者都維持原本的順序。
fn dedupe_disks(disks: Vec<DiskEntry>) -> (Vec<DiskEntry>, Vec<(DiskEntry, String)>) {
    use std::collections::HashMap;

    let shorter = |a: &DiskEntry, b: &DiskEntry| {
        (a.mount_point.len(), &a.mount_point) < (b.mount_point.len(), &b.mount_point)
    };
    // 每個鍵目前保留的項目索引
    let mut winners: HashMap<(&str, &str, u64), usize> = HashMap::new();
    for (i, disk) in disks.iter().enumerate() {
        if let Some(key) = disk.identity() {
            winners
                .entry(key)
                .and_modify(|w| {
                    if shorter(disk, &disks[*w]) {
                        *w = i;
                    }
                })
                .or_insert(i);
        }
    }
    // 每個項目被合併到哪一個掛載位置（自己就是保留者時為 None）
    let merged_into: Vec<Option<String>> = disks
        .iter()
        .enumerate()
        .map(|(i, d)| {
            d.identity()
                .map(|key| winners[&key])
                .filter(|&w| w != i)
                .map(|w| disks[w].mount_point.clone())
        })
        .collect();

    let mut kept = Vec::new();
    let mut merged = Vec::new();
    for (disk, into) in disks.into_iter().zip(merged_into) {
        match into {
            Some(mount) => merged.push((disk, mount)),
            None => kept.push(disk),
        }
    }
    (kept, merged)
}

/// 啟動時元件溫度的 INFO 行（N2）：沒有任何溫度感測器時只有一行「未偵測到溫度感測器」，
/// 不印沒有內容的標題。
fn temperature_lines(components: &[(String, Option<f32>)]) -> Vec<String> {
    if components.is_empty() {
        return vec!["未偵測到溫度感測器".to_string()];
    }
    let mut lines = vec!["=> 元件溫度:".to_string()];
    for (label, temperature) in components {
        let label = label.trim();
        let label = if label.is_empty() { "未知" } else { label };
        // sysinfo 0.33+ 在讀不到溫度時回傳 None
        lines.push(match temperature {
            Some(t) => format!("元件: {label}, 溫度: {t:.1}°C"),
            None => format!("元件: {label}, 溫度: 無法讀取"),
        });
    }
    lines
}

/// 在啟動時印出一次系統資訊（同時寫入日誌）。
///
/// 磁碟與網路介面在 INFO 只列有意義的項目（略過虛擬檔案系統、容量為 0 的掛載點、
/// 同一磁碟的重複掛載點、迴路與虛擬介面），略過的項目逐一記在 debug 等級，
/// 因此 `log.level = "debug"` 時可看到完整清單。
pub fn get_system_info() {
    // 初始化系統資訊
    let mut sys = System::new_all();

    // CPU 使用率需要兩次刷新且中間有足夠間隔才會準確
    sys.refresh_cpu_all();
    std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_all();

    info!("=> 系統資訊:");
    // 記憶體資訊（sysinfo 0.30+ 回傳單位為 bytes）
    info!("總記憶體: {} MB", sys.total_memory() / BYTES_PER_MB);
    info!("已使用記憶體: {} MB", sys.used_memory() / BYTES_PER_MB);
    info!("總交換空間: {} MB", sys.total_swap() / BYTES_PER_MB);
    info!("已使用交換空間: {} MB", sys.used_swap() / BYTES_PER_MB);

    // 系統詳細資訊（取不到時顯示「未知」）
    info!("系統名稱: {}", display_or_unknown(System::name()));
    info!(
        "系統核心版本: {}",
        display_or_unknown(System::kernel_version())
    );
    info!("系統版本: {}", display_or_unknown(System::os_version()));
    info!("主機名稱: {}", display_or_unknown(System::host_name()));

    // CPU：INFO 只記一行摘要（核心數、平均使用率），各核心使用率記 debug
    info!(
        "{}",
        cpu_summary_line(sys.cpus().len(), average_cpu_usage(&sys))
    );
    for cpu in sys.cpus() {
        debug!("{}", cpu_core_line(cpu.name(), cpu.cpu_usage()));
    }

    // 磁碟資訊（虛擬檔案系統、容量為 0 的掛載點與同一磁碟的重複掛載點只記 debug）
    info!("=> 磁碟資訊:");
    let disks = Disks::new_with_refreshed_list();
    let resolver = DeviceResolver::new();
    let mut hidden_disks = 0usize;
    let mut meaningful = Vec::new();
    for disk in &disks {
        let entry = DiskEntry::from_disk(disk, &resolver);
        if is_meaningful_disk(&entry.file_system, entry.total_space) {
            meaningful.push(entry);
        } else {
            hidden_disks += 1;
            debug!("（虛擬或容量為 0，INFO 不列出）{}", entry.line());
        }
    }
    let (kept, merged) = dedupe_disks(meaningful);
    for entry in &kept {
        info!("{}", entry.line());
    }
    for (entry, kept_mount) in &merged {
        debug!(
            "（與 {kept_mount} 為同一個磁碟的重複掛載點，INFO 不列出）{}",
            entry.line()
        );
    }
    if hidden_disks > 0 {
        info!(
            "另有 {hidden_disks} 個虛擬檔案系統或容量為 0 的掛載點未列出（log.level = \"debug\" 可看完整清單）"
        );
    }
    if !merged.is_empty() {
        info!(
            "另有 {} 個與已列出磁碟相同的重複掛載點（例如 systemd 沙箱的 bind mount）未列出（log.level = \"debug\" 可看完整清單）",
            merged.len()
        );
    }

    // 網路介面資訊（迴路與虛擬介面只記 debug）
    info!("=> 網路資訊:");
    let networks = Networks::new_with_refreshed_list();
    let mut hidden_interfaces = 0usize;
    for (interface_name, data) in &networks {
        let line = format!(
            "介面: {}, 已下載: {} B, 已上傳: {} B",
            interface_name,
            data.total_received(),
            data.total_transmitted(),
        );
        if is_meaningful_interface(interface_name) {
            info!("{line}");
        } else {
            hidden_interfaces += 1;
            debug!("（迴路或虛擬介面，INFO 不列出）{line}");
        }
    }
    if hidden_interfaces > 0 {
        info!(
            "另有 {hidden_interfaces} 個迴路或虛擬網路介面（lo、veth*、docker*、br-* 等）未列出（log.level = \"debug\" 可看完整清單）"
        );
    }

    // 元件溫度（沒有感測器時只記一行「未偵測到溫度感測器」，不印空標題）
    let components = Components::new_with_refreshed_list();
    let readings: Vec<(String, Option<f32>)> = components
        .iter()
        .map(|c| (c.label().to_string(), c.temperature()))
        .collect();
    for line in temperature_lines(&readings) {
        info!("{line}");
    }
}

// 啟動時 CPU 的 INFO 摘要行（核心數、平均使用率）。
fn cpu_summary_line(cores: usize, average: f32) -> String {
    format!("CPU: {cores} 個核心，平均使用率 {average:.2}%（各核心使用率見 debug 等級）")
}

/// 啟動時單一核心使用率的 debug 行。
fn cpu_core_line(name: &str, usage: f32) -> String {
    format!("各核心 CPU 使用率：{name}：{usage:.2}%")
}

/// 刷新 CPU 與記憶體讀數（阻塞的系統呼叫）。
fn refresh_cpu_and_memory(sys: &mut System) {
    sys.refresh_cpu_usage();
    sys.refresh_memory();
}

/// 單次取樣（sysinfo 刷新）逾時的下限與上限；實際值見 [`sample_timeout`]。
const MIN_SAMPLE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SAMPLE_TIMEOUT: Duration = Duration::from_secs(10);

/// 取樣持續卡住時，提醒用 WARN 的節流視窗（第一次逾時一定記錄）。
const STUCK_LOG_WINDOW: Duration = Duration::from_secs(600);

/// 單次取樣的逾時：`clamp(report.interval, 2 秒, 10 秒)`。
///
/// 正常的刷新只要數毫秒（第一次掃描可達數百毫秒）。超過一個回報間隔仍未完成，這次 report
/// 已經趕不上；下限 2 秒避免很短的間隔下把偶發的慢刷新當成卡住，上限 10 秒讓長間隔時
/// 也能及早發現（例如 Windows 的 WMI 查詢卡住）。
fn sample_timeout(interval: Duration) -> Duration {
    interval.clamp(MIN_SAMPLE_TIMEOUT, MAX_SAMPLE_TIMEOUT)
}

/// 一次取樣的結果。
#[derive(Debug)]
enum SampleOutcome {
    /// 取樣完成
    Ready(Sample),
    /// 取樣逾時、仍卡住，或剛從卡住恢復（重新建立基準）：略過這次 report
    Skipped,
    /// runtime 正在關閉
    Stopped,
}

/// blocking 執行緒上的一次刷新：回傳 `System`、磁碟等的 sysinfo 物件與讀出的取樣。
type RefreshTask = tokio::task::JoinHandle<(System, Option<Extras>, Sample)>;

/// 定期取樣器：沿用同一個 `System`，每次取樣只刷新一次 CPU。
///
/// sysinfo 的 CPU 使用率是「兩次刷新之間」的平均，因此每次取樣得到的是
/// 「上次取樣到這次取樣之間」（約等於 `report.interval`）的平均，而不是只量約 200 ms 的瞬間值。
/// 收集 task 啟動時先以 [`Sampler::baseline`] 建立基準，第一次取樣在一個 `report.interval` 之後，
/// 因此第一筆也涵蓋約一個 `report.interval`。沒有基準就取樣時（測試），先刷新一次建立基準，
/// 並補足 sysinfo 要求的最短間隔（`MINIMUM_CPU_UPDATE_INTERVAL`，用 tokio 的 sleep，不佔用任何執行緒）。
///
/// sysinfo 的 refresh 是阻塞的系統呼叫（首次可達數百 ms），一律放到 blocking 執行緒；
/// `System` 移入 / 移出 blocking 閉包，避免每次重新掃描。
///
/// **逾時**（Q4）：刷新超過 `timeout` 仍未完成時記 WARN 並略過這次 report（heartbeat 照常送出，
/// 伺服器端看起來仍在線，因此必須在本機日誌留下紀錄）。卡住的刷新無法中斷，`System` 仍在
/// 該執行緒上：之後每次 tick 只檢查它是否完成，**不再發起新的刷新**（避免卡住的執行緒越積越多），
/// 期間持續略過 report，每 `STUCK_LOG_WINDOW` 提醒一次。完成後取回 `System`、以完成當下為新的
/// 基準（記 INFO），下一次 tick 恢復回報。
struct Sampler {
    /// 取樣用的 `System`；blocking 閉包執行期間（含卡住時）為 `None`
    sys: Option<System>,
    /// 磁碟、網路、溫度與行程的採集設定（A4、F4）；測試替換刷新函式時為 `None`（不取樣）
    collect: Option<Arc<CollectOptions>>,
    /// 磁碟、網路與溫度的 sysinfo 物件；第一次刷新時在 blocking 執行緒建立，與 `sys` 一起移動
    extras: Option<Extras>,
    /// 上次刷新 CPU 的時間；`None` 表示尚未建立基準
    last_refresh: Option<Instant>,
    /// 實際的刷新函式（測試時可替換，以驗證在哪個執行緒、呼叫幾次）
    refresh: fn(&mut System),
    /// 單次刷新的逾時
    timeout: Duration,
    /// 逾時後仍在執行的刷新與其開始時間
    stuck: Option<(RefreshTask, Instant)>,
    /// 卡住期間略過的 report 次數（含逾時的那一次）
    skipped: u64,
    /// 卡住期間提醒用 WARN 的節流
    stuck_log: LogThrottle,
}

impl Sampler {
    fn new(timeout: Duration, opts: CollectOptions) -> Self {
        Self {
            collect: Some(Arc::new(opts)),
            ..Self::with_refresh(refresh_cpu_and_memory, timeout)
        }
    }

    fn with_refresh(refresh: fn(&mut System), timeout: Duration) -> Self {
        Self {
            sys: Some(System::new()),
            collect: None,
            extras: None,
            last_refresh: None,
            refresh,
            timeout,
            stuck: None,
            skipped: 0,
            stuck_log: LogThrottle::new(STUCK_LOG_WINDOW),
        }
    }

    /// 建立 CPU 使用率的基準（刷新一次、讀數不用）。runtime 關閉中時回傳 `None`；
    /// 逾時時照常返回（之後由 [`Sampler::sample`] 處理卡住的刷新）。
    async fn baseline(&mut self) -> Option<()> {
        match self.refresh_and_read().await {
            SampleOutcome::Stopped => None,
            SampleOutcome::Ready(_) | SampleOutcome::Skipped => Some(()),
        }
    }

    /// 取樣一次 CPU 與記憶體。
    async fn sample(&mut self) -> SampleOutcome {
        if self.stuck.is_some() {
            return self.check_stuck().await;
        }
        if self.last_refresh.is_none() {
            // 第一次：先刷新一次作為 CPU 使用率的基準，讀數本身不用
            match self.refresh_and_read().await {
                SampleOutcome::Ready(_) => {}
                other => return other,
            }
        }
        if let Some(last) = self.last_refresh {
            let since = last.elapsed();
            if since < MINIMUM_CPU_UPDATE_INTERVAL {
                time::sleep(MINIMUM_CPU_UPDATE_INTERVAL - since).await;
            }
        }
        self.refresh_and_read().await
    }

    /// 在 blocking 執行緒刷新一次並讀出取樣（含逾時）。
    async fn refresh_and_read(&mut self) -> SampleOutcome {
        let Some(mut sys) = self.sys.take() else {
            // 只有卡住時 sys 才不在（由 sample 先處理），保守起見視為略過
            return SampleOutcome::Skipped;
        };
        let refresh = self.refresh;
        let mut extras = self.extras.take();
        let collect = self.collect.clone();
        let started = Instant::now();
        let mut task = tokio::task::spawn_blocking(move || {
            refresh(&mut sys);
            if let Some(opts) = collect
                && extras.is_none()
            {
                extras = Some(Extras::new(opts));
            }
            let (extra, processes) = extras
                .as_mut()
                .map(|e| e.refresh_and_read(&sys))
                .unwrap_or_default();
            let s = Sample {
                ts: now_ts(),
                cpu_usage: average_cpu_usage(&sys),
                total_memory: sys.total_memory() / BYTES_PER_MB,
                used_memory: sys.used_memory() / BYTES_PER_MB,
                total_memory_bytes: sys.total_memory(),
                used_memory_bytes: sys.used_memory(),
                extra,
                processes,
            };
            (sys, extras, s)
        });
        match time::timeout(self.timeout, &mut task).await {
            Ok(joined) => match finished(joined) {
                Some((sys, extras, s)) => {
                    crate::metrics::global()
                        .sample_duration
                        .observe(started.elapsed());
                    self.sys = Some(sys);
                    self.extras = extras;
                    self.last_refresh = Some(Instant::now());
                    SampleOutcome::Ready(s)
                }
                None => SampleOutcome::Stopped,
            },
            Err(_) => {
                crate::metrics::global().sample_timeouts.inc();
                self.skipped = 1;
                self.stuck_log = LogThrottle::new(STUCK_LOG_WINDOW);
                self.stuck_log.check(Instant::now());
                log::warn!(
                    "{}系統資訊取樣（sysinfo）超過 {} 秒仍未完成，略過這次 report；heartbeat 照常送出，\
                     伺服器端仍會顯示此主機在線，但不會收到新的 report。取樣完成前不再發起新的取樣\
                     （每 {} 分鐘提醒一次）",
                    crate::codes::tag(
                        crate::codes::SAMPLE_TIMEOUT,
                        Some(crate::codes::W_SAMPLE_TIMEOUT)
                    ),
                    self.timeout.as_secs_f64(),
                    STUCK_LOG_WINDOW.as_secs() / 60
                );
                self.stuck = Some((task, started));
                SampleOutcome::Skipped
            }
        }
    }

    /// 上一次刷新逾時後的處理：仍在執行 → 略過（節流提醒）；已完成 → 取回 `System`，
    /// 以現在為新的基準，這次仍略過（讀數涵蓋卡住期間，不可靠），下次恢復回報。
    async fn check_stuck(&mut self) -> SampleOutcome {
        let Some((task, started)) = self.stuck.take() else {
            return SampleOutcome::Skipped;
        };
        if !task.is_finished() {
            self.skipped = self.skipped.saturating_add(1);
            if self.stuck_log.check(Instant::now()).is_some() {
                log::warn!(
                    "{}系統資訊取樣仍未完成（已 {} 秒），持續略過 report（已略過 {} 次）",
                    crate::codes::tag(
                        crate::codes::SAMPLE_TIMEOUT,
                        Some(crate::codes::W_SAMPLE_TIMEOUT)
                    ),
                    started.elapsed().as_secs(),
                    self.skipped
                );
            }
            self.stuck = Some((task, started));
            return SampleOutcome::Skipped;
        }
        match finished(task.await) {
            Some((sys, extras, _stale)) => {
                self.sys = Some(sys);
                self.extras = extras;
                self.last_refresh = Some(Instant::now());
                log::info!(
                    "系統資訊取樣已恢復（卡住約 {} 秒，期間略過 {} 次 report）；以現在為新的 CPU 基準，\
                     下次取樣恢復回報",
                    started.elapsed().as_secs(),
                    self.skipped
                );
                self.skipped = 0;
                SampleOutcome::Skipped
            }
            None => SampleOutcome::Stopped,
        }
    }
}

/// blocking 任務的結果：panic 時在呼叫端重新拋出（讓收集 task 以 panic 結束，由 main 監看並記錄）；
/// runtime 正在關閉而被取消時回傳 `None`。
fn finished<T>(joined: Result<T, tokio::task::JoinError>) -> Option<T> {
    match joined {
        Ok(v) => Some(v),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => None,
    }
}

/// 收集 task：啟動時先建立 CPU 基準，之後每 `interval` 取樣一次（第一次在一個 `interval` 之後），
/// 以 `try_send` 交給連線 task。
///
/// 第一筆 CPU 使用率因此涵蓋約一個 `interval`，而不是啟動當下約 200 ms 的瞬間值
/// （剛好落在啟動負載上，與「約等於 `report.interval` 的平均」不符）。
///
/// 取樣超過 [`sample_timeout`] 仍未完成時略過該次 report 並記 WARN（見 [`Sampler`]）。
///
/// 通道已滿（連線中斷或寫出卡住，上一筆還沒被取走）時**丟棄這次的新取樣**，
/// 因此不會堆積；連線 task 在每次註冊成功後會清掉通道內殘留的舊取樣，
/// 上一筆 report 尚未收到伺服器回應時也會略過新取樣。
/// 接收端關閉或 runtime 關閉時結束。
pub async fn collect_periodically(
    interval: Duration,
    opts: CollectOptions,
    tx: mpsc::Sender<Sample>,
) {
    collect_with(Sampler::new(sample_timeout(interval), opts), interval, tx).await;
}

/// [`collect_periodically`] 的實作，取樣器由呼叫端提供（測試時可替換刷新函式）。
async fn collect_with(mut sampler: Sampler, interval: Duration, tx: mpsc::Sender<Sample>) {
    // 先建立基準，並跳過 interval 立即觸發的第一次 tick：第一次取樣在一個 interval 之後
    if sampler.baseline().await.is_none() {
        return;
    }
    let mut ticker = time::interval_at(Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let s = match sampler.sample().await {
            SampleOutcome::Ready(s) => s,
            SampleOutcome::Skipped => continue,
            SampleOutcome::Stopped => return,
        };
        crate::local_status::record(crate::local_status::State::sampled);
        match tx.try_send(s) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(s)) => {
                crate::local_status::record(crate::local_status::State::dropped_disconnected);
                log::debug!(
                    "回報佇列已滿（目前未連線或寫出中），丟棄此次取樣（ts={}）",
                    s.ts
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                log::debug!("回報接收端已關閉，停止收集系統資訊");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_normalization() {
        assert_eq!(normalize_hostname(Some("web-01".into())), "web-01");
        assert_eq!(normalize_hostname(None), "unknown");
        assert_eq!(normalize_hostname(Some("  ".into())), "unknown");
        assert_eq!(normalize_hostname(Some("a".repeat(300))).len(), 255);
        // 多位元組字元不可被切半：85 × 3 bytes = 255，86 個字會被截成 85 個
        let cut = normalize_hostname(Some("主".repeat(86)));
        assert_eq!(cut, "主".repeat(85));
        let cut = normalize_hostname(Some(format!("a{}", "主".repeat(90))));
        assert_eq!(cut.len(), 253); // 1 + 84 × 3
    }

    #[test]
    fn hostname_control_chars_replaced() {
        assert_eq!(normalize_hostname(Some("web\n01".into())), "web?01");
        assert_eq!(
            normalize_hostname(Some("a\tb\u{7f}c\u{0}".into())),
            "a?b?c?"
        );
        // C1 控制字元（2 bytes）也替換
        assert_eq!(normalize_hostname(Some("x\u{85}y".into())), "x?y");
        // 前後空白（含結尾換行）先去除，不會變成 ?
        let h = normalize_hostname(Some("host\r\n".into()));
        assert_eq!(h, "host");
        // 全是控制字元也不會變成空字串
        assert_eq!(normalize_hostname(Some("\u{1}\u{2}".into())), "??");
        // 替換後再截斷：結果不含控制字元且不超過 255 bytes
        let h = normalize_hostname(Some(format!("{}\u{85}", "主".repeat(85))));
        assert!(h.len() <= 255 && !h.chars().any(char::is_control));
    }

    #[test]
    fn hostname_format_and_separator_chars_replaced() {
        // Kairos 會以 bad_request 拒絕含 Cf / Zl / Zp 的 hostname，送出前必須替換
        assert_eq!(
            normalize_hostname(Some("web\u{200b}01".into())),
            "web?01",
            "零寬空格"
        );
        assert_eq!(
            normalize_hostname(Some("abc\u{202e}fed".into())),
            "abc?fed",
            "由右至左覆寫"
        );
        assert_eq!(
            normalize_hostname(Some("a\u{2028}b\u{2029}c".into())),
            "a?b?c"
        );
        assert_eq!(normalize_hostname(Some("\u{feff}host".into())), "?host");
        assert_eq!(normalize_hostname(Some("x\u{ad}y\u{e0041}".into())), "x?y?");
        // 一般非 ASCII 主機名稱不受影響
        assert_eq!(normalize_hostname(Some("伺服器-01".into())), "伺服器-01");
        // 替換為 ? 後再截斷：4 bytes 的字元換成 1 byte，結果不超過 255 bytes 且不含禁止字元
        let h = normalize_hostname(Some("\u{e0041}".repeat(100)));
        assert_eq!(h, "?".repeat(100));
        let h = normalize_hostname(Some(format!("{}\u{200b}", "主".repeat(85))));
        assert!(h.len() <= 255 && !h.chars().any(is_forbidden_display_char));
    }

    #[test]
    fn hostname_fillers_selectors_and_private_use_replaced() {
        // R2-K23：Kairos 會拒絕這些碼位，送出前必須替換
        assert_eq!(
            normalize_hostname(Some("web\u{3164}01".into())),
            "web?01",
            "Hangul filler"
        );
        assert_eq!(
            normalize_hostname(Some("\u{115f}\u{1160}a\u{ffa0}".into())),
            "??a?"
        );
        assert_eq!(
            normalize_hostname(Some("a\u{34f}b\u{fe0f}c\u{e0100}".into())),
            "a?b?c?"
        );
        assert_eq!(
            normalize_hostname(Some("\u{e000}x\u{f0000}\u{10fffd}".into())),
            "?x??"
        );
        // 只由這些字元組成也不會變成空字串，且結果不含禁止字元
        let h = normalize_hostname(Some("\u{3164}".repeat(100)));
        assert_eq!(h, "?".repeat(100));
        let h = normalize_hostname(Some(format!("{}\u{e000}", "主".repeat(85))));
        assert!(h.len() <= 255 && !h.chars().any(is_forbidden_display_char));
    }

    #[test]
    fn virtual_filesystems_and_empty_mounts_are_hidden() {
        // F6：docker 主機上的 overlay 掛載點、tmpfs 等不列在 INFO
        for fs in [
            "overlay",
            "tmpfs",
            "devtmpfs",
            "squashfs",
            "proc",
            "sysfs",
            "cgroup2",
            "TMPFS",
            "fuse.lxcfs",
            " overlay ",
        ] {
            assert!(!is_meaningful_disk(fs, 10_000_000_000), "{fs}");
        }
        // 實體或網路檔案系統照常列出
        for fs in [
            "ext4",
            "xfs",
            "btrfs",
            "zfs",
            "vfat",
            "NTFS",
            "ReFS",
            "apfs",
            "nfs4",
            "cifs",
            "fuse.sshfs",
            "",
        ] {
            assert!(is_meaningful_disk(fs, 10_000_000_000), "{fs}");
        }
        // 總容量為 0 的掛載點一律略過；小於 1 GB 但非 0 的仍列出（例如 /boot/efi，以 MB 顯示）
        assert!(!is_meaningful_disk("ext4", 0));
        assert!(is_meaningful_disk("vfat", 512 * 1024 * 1024));
    }

    fn disk(mount: &str, fs: &str, name: &str, total: u64) -> DiskEntry {
        DiskEntry {
            mount_point: mount.to_string(),
            file_system: fs.to_string(),
            kind: "Unknown".to_string(),
            name: name.to_string(),
            total_space: total,
            available_space: total / 2,
            free_space: None,
            removable: false,
            read_only: false,
            device: None,
        }
    }

    fn mounts(disks: &[DiskEntry]) -> Vec<&str> {
        disks.iter().map(|d| d.mount_point.as_str()).collect()
    }

    fn merged_pairs(merged: &[(DiskEntry, String)]) -> Vec<(&str, &str)> {
        merged
            .iter()
            .map(|(d, into)| (d.mount_point.as_str(), into.as_str()))
            .collect()
    }

    #[test]
    fn systemd_sandbox_bind_mounts_are_merged_into_root() {
        // N1：DEPLOY-TEST 第二輪的實際情況（ReadWritePaths、PrivateTmp 的 bind mount）
        const LV: &str = "/dev/mapper/ubuntu--vg-ubuntu--lv";
        const TOTAL: u64 = 39_350_000_000;
        let disks = vec![
            disk("/", "ext4", LV, TOTAL),
            disk("/opt/pulse/logs", "ext4", LV, TOTAL),
            disk("/tmp", "ext4", LV, TOTAL),
            disk("/var/tmp", "ext4", LV, TOTAL),
            disk("/boot", "ext4", "/dev/sda2", 2_000_000_000),
            disk("/boot/efi", "vfat", "/dev/sda1", 1_124_000_000),
        ];
        let (kept, merged) = dedupe_disks(disks);
        assert_eq!(mounts(&kept), ["/", "/boot", "/boot/efi"]);
        assert_eq!(
            merged_pairs(&merged),
            [("/opt/pulse/logs", "/"), ("/tmp", "/"), ("/var/tmp", "/")]
        );
    }

    #[test]
    fn dedupe_keeps_the_shortest_mount_regardless_of_order() {
        // 保留者不一定是第一個出現的項目；輸出維持原本的順序
        let disks = vec![
            disk("/srv/data", "xfs", "/dev/sdb1", 100),
            disk("/home", "btrfs", "/dev/sda2", 500),
            disk("/data", "xfs", "/dev/sdb1", 100),
            disk("/", "btrfs", "/dev/sda2", 500),
            disk("/mnt/x", "xfs", "/dev/sdb1", 100),
        ];
        let (kept, merged) = dedupe_disks(disks);
        assert_eq!(mounts(&kept), ["/data", "/"]);
        assert_eq!(
            merged_pairs(&merged),
            [("/srv/data", "/data"), ("/home", "/"), ("/mnt/x", "/data")]
        );

        // 長度相同時取字典序較小者
        let (kept, merged) = dedupe_disks(vec![
            disk("/b", "ext4", "/dev/sdc1", 7),
            disk("/a", "ext4", "/dev/sdc1", 7),
        ]);
        assert_eq!(mounts(&kept), ["/a"]);
        assert_eq!(merged_pairs(&merged), [("/b", "/a")]);
    }

    #[test]
    fn dedupe_requires_same_name_filesystem_and_total() {
        // 名稱、檔案系統或總容量任一不同都不合併
        let disks = vec![
            disk("/", "ext4", "/dev/sda1", 100),
            disk("/a", "ext4", "/dev/sdb1", 100),
            disk("/b", "xfs", "/dev/sda1", 100),
            disk("/c", "ext4", "/dev/sda1", 101),
        ];
        let (kept, merged) = dedupe_disks(disks);
        assert_eq!(mounts(&kept), ["/", "/a", "/b", "/c"]);
        assert!(merged.is_empty());

        // 磁碟名稱為空（例如 Windows 未命名的磁碟區）無法辨識裝置，不合併
        let (kept, merged) = dedupe_disks(vec![
            disk("C:\\", "NTFS", "", 1_000),
            disk("D:\\", "NTFS", "  ", 1_000),
        ]);
        assert_eq!(mounts(&kept), ["C:\\", "D:\\"]);
        assert!(merged.is_empty());

        assert_eq!(dedupe_disks(Vec::new()), (Vec::new(), Vec::new()));
    }

    #[test]
    fn disk_line_has_no_debug_quotes_and_readable_units() {
        // N3：不再出現 "/" 這種 Debug 格式；小分割區不再顯示 0 GB
        let d = DiskEntry {
            kind: "SSD".to_string(),
            available_space: 24_123_456_789,
            ..disk(
                "/",
                "ext4",
                "/dev/mapper/ubuntu--vg-ubuntu--lv",
                39_350_000_000,
            )
        };
        assert_eq!(
            d.line(),
            "掛載位置: /, 檔案系統: ext4, 硬碟種類: SSD, 磁碟名稱: /dev/mapper/ubuntu--vg-ubuntu--lv, \
             總容量: 39.4 GB, 可用空間: 24.1 GB"
        );
        // Ubuntu 預設的 1.05 GiB EFI 分割區與常見的 512 MiB EFI 分割區
        let efi = DiskEntry {
            available_space: 1_117_478_912,
            ..disk("/boot/efi", "vfat", "/dev/sda1", 1_124_999_168)
        };
        assert!(
            efi.line().contains("總容量: 1.1 GB, 可用空間: 1.1 GB"),
            "{}",
            efi.line()
        );
        let efi = DiskEntry {
            available_space: 529_000_000,
            ..disk("/boot/efi", "vfat", "/dev/sda1", 535_805_952)
        };
        let line = efi.line();
        assert!(line.contains("總容量: 535 MB, 可用空間: 529 MB"), "{line}");
        assert!(!line.contains('"') && !line.contains("0 GB"), "{line}");
        // 磁碟名稱為空時顯示「未知」
        assert!(disk("/x", "ext4", "", 1).line().contains("磁碟名稱: 未知"));
    }

    #[test]
    fn disk_capacity_units() {
        assert_eq!(format_disk_bytes(0), "0 B");
        assert_eq!(format_disk_bytes(999), "999 B");
        assert_eq!(format_disk_bytes(1_000), "1 KB");
        assert_eq!(format_disk_bytes(999_999), "999 KB");
        assert_eq!(format_disk_bytes(1_000_000), "1 MB");
        assert_eq!(format_disk_bytes(536_870_912), "536 MB");
        assert_eq!(format_disk_bytes(999_999_999), "999 MB");
        assert_eq!(format_disk_bytes(1_000_000_000), "1.0 GB");
        assert_eq!(format_disk_bytes(39_350_000_000), "39.4 GB");
        assert_eq!(format_disk_bytes(2_000_398_934_016), "2000.4 GB");
        assert_eq!(format_disk_bytes(u64::MAX), "18446744073.7 GB");
    }

    #[test]
    fn missing_system_info_is_shown_as_unknown() {
        // N3：不再出現 Some("Ubuntu")；取不到時為「未知」
        assert_eq!(display_or_unknown(Some("Ubuntu".into())), "Ubuntu");
        assert_eq!(
            display_or_unknown(Some("6.8.0-138-generic\n".into())),
            "6.8.0-138-generic"
        );
        assert_eq!(display_or_unknown(None), "未知");
        assert_eq!(display_or_unknown(Some("  ".into())), "未知");
    }

    #[test]
    fn temperature_section_without_sensors_is_a_single_line() {
        // N2：沒有感測器時不印空的「=> 元件溫度:」標題
        assert_eq!(temperature_lines(&[]), ["未偵測到溫度感測器"]);
        assert_eq!(
            temperature_lines(&[
                ("coretemp Package id 0".to_string(), Some(45.25)),
                ("acpitz".to_string(), None),
                (String::new(), Some(30.0)),
            ]),
            [
                "=> 元件溫度:",
                "元件: coretemp Package id 0, 溫度: 45.2°C",
                "元件: acpitz, 溫度: 無法讀取",
                "元件: 未知, 溫度: 30.0°C",
            ]
        );
    }

    #[test]
    fn startup_info_has_no_debug_formatting() {
        // N2、N3：實際輸出的 INFO 中沒有 Some(...)、帶引號的路徑，也沒有空的溫度標題
        let ((), logs) = crate::test_log::capture(get_system_info);
        let info: Vec<_> = logs
            .iter()
            .filter(|(l, _)| *l == log::Level::Info)
            .map(|(_, m)| m.as_str())
            .collect();
        for m in &info {
            assert!(!m.contains("Some(") && !m.contains("None"), "{m}");
            if m.starts_with("掛載位置") {
                assert!(!m.contains('"'), "{m}");
                assert!(!m.contains("總容量: 0 "), "{m}");
            }
        }
        for key in ["系統名稱: ", "系統核心版本: ", "系統版本: ", "主機名稱: "] {
            assert_eq!(
                info.iter().filter(|m| m.starts_with(key)).count(),
                1,
                "{key}: {info:?}"
            );
        }
        let title = info.iter().position(|m| *m == "=> 元件溫度:");
        let no_sensor = info.contains(&"未偵測到溫度感測器");
        assert!(title.is_some() != no_sensor, "{info:?}");
        if let Some(i) = title {
            assert!(
                info.get(i + 1).is_some_and(|m| m.starts_with("元件: ")),
                "{info:?}"
            );
        }
    }

    #[test]
    fn loopback_and_virtual_interfaces_are_hidden() {
        // F6：docker 主機上的 veth、docker0、br-xxxx 與 lo 不列在 INFO
        for name in [
            "lo",
            "lo0",
            "veth1a2b3c4",
            "docker0",
            "docker_gwbridge",
            "br-0123456789ab",
            "virbr0",
            "cni0",
            "flannel.1",
            "cali1234abcd",
            "vxlan.calico",
            "tunl0",
            "kube-ipvs0",
            "lxcbr0",
        ] {
            assert!(!is_meaningful_interface(name), "{name}");
        }
        for name in [
            "eth0",
            "ens18",
            "enp3s0",
            "eno1",
            "wlan0",
            "wlp2s0",
            "bond0",
            "en0",
            "wg0",
            "tailscale0",
            "local0",
            "乙太網路",
            "Wi-Fi",
            "vEthernet (Default Switch)",
            "bridge0",
        ] {
            assert!(is_meaningful_interface(name), "{name}");
        }
    }

    #[test]
    fn cpu_summary_and_core_lines() {
        assert_eq!(
            cpu_summary_line(24, 37.375),
            "CPU: 24 個核心，平均使用率 37.38%（各核心使用率見 debug 等級）"
        );
        assert_eq!(cpu_core_line("cpu3", 5.0), "各核心 CPU 使用率：cpu3：5.00%");
    }

    #[test]
    fn startup_info_lists_cpu_cores_only_at_debug() {
        // INFO 只有一行 CPU 摘要；每個核心一行的使用率只在 debug
        let ((), logs) = crate::test_log::capture(get_system_info);
        let cores = System::new_all().cpus().len();
        let info_cpu: Vec<_> = logs
            .iter()
            .filter(|(l, m)| *l == log::Level::Info && m.contains("CPU"))
            .collect();
        assert_eq!(info_cpu.len(), 1, "{info_cpu:?}");
        assert!(
            info_cpu[0].1.starts_with(&format!("CPU: {cores} 個核心")),
            "{info_cpu:?}"
        );
        let debug_cores = logs
            .iter()
            .filter(|(l, m)| *l == log::Level::Debug && m.starts_with("各核心 CPU 使用率"))
            .count();
        assert_eq!(debug_cores, cores, "{logs:?}");
    }

    #[tokio::test]
    async fn sample_reads_real_values() {
        let mut sampler = Sampler::new(
            sample_timeout(Duration::from_secs(5)),
            CollectOptions::defaults(),
        );
        let s = ready(sampler.sample().await);
        assert!(s.total_memory > 0);
        assert!(s.used_memory <= s.total_memory);
        assert!(s.cpu_usage.is_finite());
        let s = ready(sampler.sample().await);
        assert!(s.cpu_usage.is_finite() && (0.0..=100.0 + 1e-3).contains(&s.cpu_usage));
    }

    static REFRESH_THREADS: std::sync::Mutex<Vec<std::thread::ThreadId>> =
        std::sync::Mutex::new(Vec::new());

    fn record_refresh_thread(_: &mut System) {
        REFRESH_THREADS
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }

    #[test]
    fn sample_refresh_runs_off_the_async_worker() {
        // R2-T3：current_thread runtime 的 async worker 就是測試執行緒本身；
        // 阻塞的 refresh 必須在其他（blocking pool）執行緒上執行
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let worker = std::thread::current().id();
        rt.block_on(async {
            let mut sampler = Sampler::with_refresh(record_refresh_thread, Duration::from_secs(10));
            ready(sampler.sample().await);
            ready(sampler.sample().await);
        });
        let threads = REFRESH_THREADS.lock().unwrap().clone();
        // 第一次取樣：基準 + 讀數；第二次：只刷新一次
        assert_eq!(threads.len(), 3, "{threads:?}");
        assert!(threads.iter().all(|t| *t != worker), "{threads:?}");
    }

    static REFRESH_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn count_refresh(_: &mut System) {
        REFRESH_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    static FIRST_SAMPLE_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn count_first_sample_refresh(_: &mut System) {
        FIRST_SAMPLE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[tokio::test(start_paused = true)]
    async fn first_sample_covers_a_full_interval() {
        // Q3：啟動時先建立基準並跳過第一次 tick，第一筆取樣在一個 interval 之後，
        // 只刷新一次（不再是「基準 + 200 ms 後的讀數」）
        use std::sync::atomic::Ordering::SeqCst;
        const INTERVAL: Duration = Duration::from_secs(5);
        let (tx, mut rx) = mpsc::channel(4);
        let start = Instant::now();
        let task = tokio::spawn(collect_with(
            Sampler::with_refresh(count_first_sample_refresh, Duration::from_secs(10)),
            INTERVAL,
            tx,
        ));
        // 第一個 interval 內沒有任何取樣，只建立了基準
        time::sleep(INTERVAL - Duration::from_millis(10)).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(FIRST_SAMPLE_CALLS.load(SeqCst), 1, "只有基準");

        rx.recv().await.unwrap();
        let first_at = start.elapsed();
        assert!(
            first_at >= INTERVAL && first_at < INTERVAL + MINIMUM_CPU_UPDATE_INTERVAL,
            "{first_at:?}"
        );
        assert_eq!(FIRST_SAMPLE_CALLS.load(SeqCst), 2, "基準 + 第一筆");

        rx.recv().await.unwrap();
        let second_at = start.elapsed();
        assert!(
            second_at >= INTERVAL * 2 && second_at < INTERVAL * 2 + MINIMUM_CPU_UPDATE_INTERVAL
        );
        assert_eq!(FIRST_SAMPLE_CALLS.load(SeqCst), 3);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn cpu_is_averaged_over_the_report_interval() {
        // R2-P3：之後每次取樣只刷新一次 CPU、不再額外等待 200 ms，
        // 因此 CPU 使用率涵蓋上次取樣到這次取樣之間的整段時間
        use std::sync::atomic::Ordering::SeqCst;
        let mut sampler = Sampler::with_refresh(count_refresh, Duration::from_secs(10));
        let t0 = Instant::now();
        ready(sampler.sample().await);
        assert_eq!(REFRESH_CALLS.load(SeqCst), 2, "第一次：基準 + 讀數");
        assert!(t0.elapsed() >= MINIMUM_CPU_UPDATE_INTERVAL);
        for i in 1..=3 {
            time::sleep(Duration::from_secs(5)).await;
            let t = Instant::now();
            ready(sampler.sample().await);
            assert_eq!(REFRESH_CALLS.load(SeqCst), 2 + i);
            assert!(t.elapsed() < MINIMUM_CPU_UPDATE_INTERVAL, "不應再額外等待");
        }
        // 兩次取樣間隔太短時才補足 sysinfo 要求的最短間隔
        let t = Instant::now();
        ready(sampler.sample().await);
        assert!(t.elapsed() >= MINIMUM_CPU_UPDATE_INTERVAL);
        assert_eq!(REFRESH_CALLS.load(SeqCst), 6);
    }

    /// 測試用：取出完成的取樣。
    #[tokio::test]
    async fn real_extra_metrics_are_valid() {
        // 實際的磁碟、網路與溫度讀數也符合協定（各平台可能為空）
        let (metrics, processes) = tokio::task::spawn_blocking(|| {
            let mut sys = System::new();
            refresh_cpu_and_memory(&mut sys);
            let mut extras = Extras::new(Arc::new(CollectOptions::defaults()));
            extras.refresh_and_read(&sys)
        })
        .await
        .unwrap();
        // 沒有 label 的新指標一定有（負載只有 Linux）
        let names: Vec<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
        for name in [
            "system.memory.available",
            "system.swap.total",
            "system.uptime",
        ] {
            assert!(names.contains(&name), "{name}: {names:?}");
        }
        assert_eq!(
            names.contains(&"system.cpu.load_1m"),
            cfg!(target_os = "linux")
        );
        kairos_proto::validate_metrics(&metrics).unwrap();
        assert!(
            processes.is_none(),
            "建立時只是基準，排行在 processes.interval 之後"
        );
        // 每個送出的掛載點都有完整的一組（total、available、used、usage）
        let count = |n: &str| names.iter().filter(|x| **x == n).count();
        assert_eq!(
            count("system.filesystem.total"),
            count("system.filesystem.used")
        );
        assert_eq!(
            count("system.filesystem.total"),
            count("system.filesystem.usage")
        );
    }

    fn ready(outcome: SampleOutcome) -> Sample {
        match outcome {
            SampleOutcome::Ready(s) => s,
            other => panic!("預期取樣完成：{other:?}"),
        }
    }

    #[test]
    fn sample_timeout_follows_interval_within_bounds() {
        assert_eq!(
            sample_timeout(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            sample_timeout(Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            sample_timeout(Duration::from_secs(3600)),
            Duration::from_secs(10)
        );
    }

    /// 卡住測試用的刷新函式：`STUCK` 為 true 時一直等待（模擬 WMI 卡住）。
    static STUCK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static STUCK_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn maybe_stuck_refresh(_: &mut System) {
        use std::sync::atomic::Ordering::SeqCst;
        STUCK_CALLS.fetch_add(1, SeqCst);
        while STUCK.load(SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn stuck_sampling_is_reported_and_skipped() {
        // Q4：取樣卡住時記 WARN、略過 report，不再發起新的刷新；恢復後重新建立基準再回報
        use std::sync::atomic::Ordering::SeqCst;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ((), logs) = crate::test_log::capture(|| {
            rt.block_on(async {
                let mut sampler =
                    Sampler::with_refresh(maybe_stuck_refresh, Duration::from_millis(100));
                sampler.baseline().await.unwrap();
                assert_eq!(STUCK_CALLS.load(SeqCst), 1);

                STUCK.store(true, SeqCst);
                time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;
                let t = Instant::now();
                assert!(matches!(sampler.sample().await, SampleOutcome::Skipped));
                assert!(t.elapsed() >= Duration::from_millis(100));
                assert!(t.elapsed() < Duration::from_secs(5), "逾時後立即返回");
                // 仍卡住：不發起新的刷新、立即略過
                for _ in 0..3 {
                    let t = Instant::now();
                    assert!(matches!(sampler.sample().await, SampleOutcome::Skipped));
                    assert!(t.elapsed() < Duration::from_millis(50));
                }
                assert_eq!(STUCK_CALLS.load(SeqCst), 2, "卡住期間不得再發起刷新");

                // 恢復：取回 System，這次仍略過（重新建立基準），下次恢復回報
                STUCK.store(false, SeqCst);
                time::sleep(Duration::from_millis(200)).await;
                assert!(matches!(sampler.sample().await, SampleOutcome::Skipped));
                time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;
                ready(sampler.sample().await);
                assert_eq!(STUCK_CALLS.load(SeqCst), 3);
            });
        });
        let warns: Vec<_> = logs
            .iter()
            .filter(|(l, _)| *l == log::Level::Warn)
            .map(|(_, m)| m.as_str())
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "第一次逾時記一則，之後在節流視窗內：{logs:?}"
        );
        assert!(
            warns[0].contains("略過這次 report") && warns[0].contains("heartbeat 照常送出"),
            "{warns:?}"
        );
        assert!(
            logs.iter().any(|(l, m)| *l == log::Level::Info
                && m.contains("已恢復")
                && m.contains("略過 4 次")),
            "{logs:?}"
        );
    }

    static STUCK_COLLECT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn maybe_stuck_collect_refresh(_: &mut System) {
        while STUCK_COLLECT.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[tokio::test]
    async fn collector_skips_reports_while_sampling_is_stuck() {
        // Q4：收集 task 在取樣卡住期間不送出取樣，恢復後繼續
        use std::sync::atomic::Ordering::SeqCst;
        let (tx, mut rx) = mpsc::channel(8);
        let task = tokio::spawn(collect_with(
            Sampler::with_refresh(maybe_stuck_collect_refresh, Duration::from_millis(100)),
            Duration::from_millis(250),
            tx,
        ));
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        STUCK_COLLECT.store(true, SeqCst);
        // 正在進行的取樣可能剛好在卡住前完成：先清掉
        time::sleep(Duration::from_millis(300)).await;
        while rx.try_recv().is_ok() {}
        time::sleep(Duration::from_millis(1000)).await;
        assert!(rx.try_recv().is_err(), "卡住期間不應有取樣");
        STUCK_COLLECT.store(false, SeqCst);
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("恢復後應繼續送出取樣")
            .unwrap();
        task.abort();
    }
}
