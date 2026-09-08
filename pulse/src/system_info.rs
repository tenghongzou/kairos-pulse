use log::info;
use serde::Serialize;
use sysinfo::{Components, Disks, Networks, System, MINIMUM_CPU_UPDATE_INTERVAL};

/// 一個 MB 的位元組數，用來把 sysinfo 回傳的 bytes 換算成 MB。
const BYTES_PER_MB: u64 = 1024 * 1024;

#[derive(Serialize)]
pub(crate) struct SystemInfo {
    /// 全機 CPU 平均使用率（百分比）
    cpu_usage: f32,
    /// 總記憶體（MB）
    total_memory: u64,
    /// 已使用記憶體（MB）
    used_memory: u64,
    hostname: String,
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

/// 在啟動時印出一次完整的系統資訊（同時寫入日誌）。
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

    // CPU 數量與平均使用率
    info!("CPU 數量: {}", sys.cpus().len());
    info!("CPU 平均使用率: {:.2}%", average_cpu_usage(&sys));
    info!("=> 各核心 CPU 使用率:");
    for cpu in sys.cpus() {
        info!("CPU {}: {:.2}% 使用率", cpu.name(), cpu.cpu_usage());
    }

    // 磁碟資訊
    info!("=> 磁碟資訊:");
    let disks = Disks::new_with_refreshed_list();
    for disk in &disks {
        info!(
            "掛載位置: {:?}, 硬碟種類: {:?}, 磁碟名稱: {:?}, 總容量: {} GB, 可用空間: {} GB",
            disk.mount_point(),
            disk.kind(),
            disk.name(),
            disk.total_space() / 1_000_000_000,
            disk.available_space() / 1_000_000_000,
        );
    }

    // 網路介面資訊
    info!("=> 網路資訊:");
    let networks = Networks::new_with_refreshed_list();
    for (interface_name, data) in &networks {
        info!(
            "介面: {}, 已下載: {} B, 已上傳: {} B",
            interface_name,
            data.total_received(),
            data.total_transmitted(),
        );
    }

    // 元件溫度
    info!("=> 元件溫度:");
    let components = Components::new_with_refreshed_list();
    for component in &components {
        info!(
            "元件: {:?}, 溫度: {}°C",
            component.label(),
            component.temperature()
        );
    }
}

/// 取得要回報給伺服器的系統資訊。
///
/// 非同步版本：在兩次 CPU 刷新之間使用 tokio 的 sleep，避免阻塞 tokio 執行緒。
pub async fn get_real_system_info() -> SystemInfo {
    let mut sys = System::new_all();

    // 取得準確的 CPU 使用率：刷新 → 等待 → 再刷新
    sys.refresh_cpu_all();
    tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;
    sys.refresh_cpu_all();

    let cpu_usage = average_cpu_usage(&sys);

    // 記憶體資訊（bytes → MB）
    let total_memory = sys.total_memory() / BYTES_PER_MB;
    let used_memory = sys.used_memory() / BYTES_PER_MB;

    // 主機名稱
    let hostname = System::host_name().unwrap_or_else(|| "Unknown".to_string());

    SystemInfo {
        cpu_usage,
        total_memory,
        used_memory,
        hostname,
    }
}
