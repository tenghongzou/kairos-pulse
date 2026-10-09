use log::{debug, info};
use std::time::Duration;
use sysinfo::{Components, Disks, MINIMUM_CPU_UPDATE_INTERVAL, Networks, System};
use tokio::sync::mpsc;
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::protocol::{is_forbidden_display_char, now_ts};

/// 一個 MB 的位元組數，用來把 sysinfo 回傳的 bytes 換算成 MB。
const BYTES_PER_MB: u64 = 1024 * 1024;

/// 一次系統資訊取樣，由收集 task 產生、經 mpsc 交給連線 task 組成 report 訊息。
#[derive(Debug, Clone)]
pub struct Sample {
    /// 取樣時間（RFC 3339 UTC 毫秒），作為 report 的 `ts`
    pub ts: String,
    /// 全機 CPU 平均使用率（百分比）
    pub cpu_usage: f32,
    /// 總記憶體（MB）
    pub total_memory: u64,
    /// 已使用記憶體（MB）
    pub used_memory: u64,
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

/// hello 中 `hostname` 的長度上限（bytes），見 PROTOCOL.md。
const MAX_HOSTNAME_BYTES: usize = 255;

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

/// 在啟動時印出一次系統資訊（同時寫入日誌）。
///
/// 磁碟與網路介面在 INFO 只列有意義的項目（略過虛擬檔案系統、容量為 0 的掛載點、迴路與虛擬介面），
/// 略過的項目逐一記在 debug 等級，因此 `log.level = "debug"` 時可看到完整清單。
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

    // 系統詳細資訊
    info!("系統名稱: {:?}", System::name());
    info!("系統核心版本: {:?}", System::kernel_version());
    info!("系統版本: {:?}", System::os_version());
    info!("主機名稱: {:?}", System::host_name());

    // CPU：INFO 只記一行摘要（核心數、平均使用率），各核心使用率記 debug
    info!(
        "{}",
        cpu_summary_line(sys.cpus().len(), average_cpu_usage(&sys))
    );
    for cpu in sys.cpus() {
        debug!("{}", cpu_core_line(cpu.name(), cpu.cpu_usage()));
    }

    // 磁碟資訊（虛擬檔案系統與容量為 0 的掛載點只記 debug）
    info!("=> 磁碟資訊:");
    let disks = Disks::new_with_refreshed_list();
    let mut hidden_disks = 0usize;
    for disk in &disks {
        let fs_type = disk.file_system().to_string_lossy();
        let line = format!(
            "掛載位置: {:?}, 檔案系統: {fs_type}, 硬碟種類: {:?}, 磁碟名稱: {:?}, 總容量: {} GB, 可用空間: {} GB",
            disk.mount_point(),
            disk.kind(),
            disk.name(),
            disk.total_space() / 1_000_000_000,
            disk.available_space() / 1_000_000_000,
        );
        if is_meaningful_disk(&fs_type, disk.total_space()) {
            info!("{line}");
        } else {
            hidden_disks += 1;
            debug!("（虛擬或容量為 0，INFO 不列出）{line}");
        }
    }
    if hidden_disks > 0 {
        info!(
            "另有 {hidden_disks} 個虛擬檔案系統或容量為 0 的掛載點未列出（log.level = \"debug\" 可看完整清單）"
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

    // 元件溫度
    info!("=> 元件溫度:");
    let components = Components::new_with_refreshed_list();
    for component in &components {
        // sysinfo 0.33+ 在讀不到溫度時回傳 None
        match component.temperature() {
            Some(t) => info!("元件: {:?}, 溫度: {t}°C", component.label()),
            None => info!("元件: {:?}, 溫度: 無法讀取", component.label()),
        }
    }
}

/// 啟動時 CPU 的 INFO 摘要行（核心數、平均使用率）。
fn cpu_summary_line(cores: usize, average: f32) -> String {
    format!("CPU: {cores} 個核心，平均使用率 {average:.2}%（各核心使用率見 debug 等級）")
}

/// 啟動時單一核心使用率的 debug 行。
fn cpu_core_line(name: &str, usage: f32) -> String {
    format!("各核心 CPU 使用率：{name}：{usage:.2}%")
}

/// 在 blocking 執行緒執行 `f`。
///
/// `f` panic 時在呼叫端重新拋出（讓收集 task 以 panic 結束，由 main 監看並記錄）；
/// runtime 正在關閉而被取消時回傳 `None`。
async fn blocking<T, F>(f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => Some(v),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => None,
    }
}

/// 刷新 CPU 與記憶體讀數（阻塞的系統呼叫）。
fn refresh_cpu_and_memory(sys: &mut System) {
    sys.refresh_cpu_usage();
    sys.refresh_memory();
}

/// 定期取樣器：沿用同一個 `System`，每次取樣只刷新一次 CPU。
///
/// sysinfo 的 CPU 使用率是「兩次刷新之間」的平均，因此每次取樣得到的是
/// 「上次取樣到這次取樣之間」（約等於 `report.interval`）的平均，而不是只量約 200 ms 的瞬間值。
/// 第一次取樣前先刷新一次建立基準，並補足 sysinfo 要求的最短間隔
/// （`MINIMUM_CPU_UPDATE_INTERVAL`，用 tokio 的 sleep，不佔用任何執行緒）。
///
/// sysinfo 的 refresh 是阻塞的系統呼叫（首次可達數百 ms），一律放到 blocking 執行緒；
/// `System` 移入 / 移出 blocking 閉包，避免每次重新掃描。
struct Sampler {
    /// 取樣用的 `System`；只在 blocking 閉包執行期間為 `None`
    sys: Option<System>,
    /// 上次刷新 CPU 的時間；`None` 表示尚未建立基準
    last_refresh: Option<Instant>,
    /// 實際的刷新函式（測試時可替換，以驗證在哪個執行緒、呼叫幾次）
    refresh: fn(&mut System),
}

impl Sampler {
    fn new() -> Self {
        Self::with_refresh(refresh_cpu_and_memory)
    }

    fn with_refresh(refresh: fn(&mut System)) -> Self {
        Self {
            sys: Some(System::new()),
            last_refresh: None,
            refresh,
        }
    }

    /// 取樣一次 CPU 與記憶體。runtime 關閉中時回傳 `None`。
    async fn sample(&mut self) -> Option<Sample> {
        if self.last_refresh.is_none() {
            // 第一次：先刷新一次作為 CPU 使用率的基準，讀數本身不用
            self.refresh_and_read().await?;
        }
        if let Some(last) = self.last_refresh {
            let since = last.elapsed();
            if since < MINIMUM_CPU_UPDATE_INTERVAL {
                time::sleep(MINIMUM_CPU_UPDATE_INTERVAL - since).await;
            }
        }
        self.refresh_and_read().await
    }

    /// 在 blocking 執行緒刷新一次並讀出取樣。
    async fn refresh_and_read(&mut self) -> Option<Sample> {
        let mut sys = self.sys.take()?;
        let refresh = self.refresh;
        let (sys, s) = blocking(move || {
            refresh(&mut sys);
            let s = Sample {
                ts: now_ts(),
                cpu_usage: average_cpu_usage(&sys),
                total_memory: sys.total_memory() / BYTES_PER_MB,
                used_memory: sys.used_memory() / BYTES_PER_MB,
            };
            (sys, s)
        })
        .await?;
        self.sys = Some(sys);
        self.last_refresh = Some(Instant::now());
        Some(s)
    }
}

/// 收集 task：每 `interval` 取樣一次，以 `try_send` 交給連線 task。
///
/// 通道已滿（連線中斷或寫出卡住，上一筆還沒被取走）時**丟棄這次的新取樣**，
/// 因此不會堆積；連線 task 在每次註冊成功後會清掉通道內殘留的舊取樣，
/// 上一筆 report 尚未收到伺服器回應時也會略過新取樣。
/// 接收端關閉或 runtime 關閉時結束。
pub async fn collect_periodically(interval: Duration, tx: mpsc::Sender<Sample>) {
    let mut sampler = Sampler::new();
    let mut ticker = time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let Some(s) = sampler.sample().await else {
            return;
        };
        match tx.try_send(s) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(s)) => {
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
        // 總容量為 0 的掛載點一律略過；小於 1 GB（顯示為 0 GB）但非 0 的仍列出（例如 /boot/efi）
        assert!(!is_meaningful_disk("ext4", 0));
        assert!(is_meaningful_disk("vfat", 512 * 1024 * 1024));
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
        let mut sampler = Sampler::new();
        let s = sampler.sample().await.unwrap();
        assert!(s.total_memory > 0);
        assert!(s.used_memory <= s.total_memory);
        assert!(s.cpu_usage.is_finite());
        let s = sampler.sample().await.unwrap();
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
            let mut sampler = Sampler::with_refresh(record_refresh_thread);
            sampler.sample().await.unwrap();
            sampler.sample().await.unwrap();
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

    #[tokio::test(start_paused = true)]
    async fn cpu_is_averaged_over_the_report_interval() {
        // R2-P3：之後每次取樣只刷新一次 CPU、不再額外等待 200 ms，
        // 因此 CPU 使用率涵蓋上次取樣到這次取樣之間的整段時間
        use std::sync::atomic::Ordering::SeqCst;
        let mut sampler = Sampler::with_refresh(count_refresh);
        let t0 = Instant::now();
        sampler.sample().await.unwrap();
        assert_eq!(REFRESH_CALLS.load(SeqCst), 2, "第一次：基準 + 讀數");
        assert!(t0.elapsed() >= MINIMUM_CPU_UPDATE_INTERVAL);
        for i in 1..=3 {
            time::sleep(Duration::from_secs(5)).await;
            let t = Instant::now();
            sampler.sample().await.unwrap();
            assert_eq!(REFRESH_CALLS.load(SeqCst), 2 + i);
            assert!(t.elapsed() < MINIMUM_CPU_UPDATE_INTERVAL, "不應再額外等待");
        }
        // 兩次取樣間隔太短時才補足 sysinfo 要求的最短間隔
        let t = Instant::now();
        sampler.sample().await.unwrap();
        assert!(t.elapsed() >= MINIMUM_CPU_UPDATE_INTERVAL);
        assert_eq!(REFRESH_CALLS.load(SeqCst), 6);
    }
}
