use log::info;
use std::time::Duration;
use sysinfo::{Components, Disks, MINIMUM_CPU_UPDATE_INTERVAL, Networks, System};
use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};

use crate::protocol::now_ts;

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
/// - 其餘控制字元（C0、DEL、C1）替換為 `?`
/// - 取不到或結果為空時為 `"unknown"`
/// - 超過 255 bytes 時在字元邊界截斷
fn normalize_hostname(name: Option<String>) -> String {
    let name: String = name
        .unwrap_or_default()
        .trim()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
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
        // sysinfo 0.33+ 在讀不到溫度時回傳 None
        match component.temperature() {
            Some(t) => info!("元件: {:?}, 溫度: {t}°C", component.label()),
            None => info!("元件: {:?}, 溫度: 無法讀取", component.label()),
        }
    }
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

/// 取樣一次 CPU 與記憶體。
///
/// sysinfo 的 refresh 是阻塞的系統呼叫（首次可達數百 ms），放到 blocking 執行緒；
/// 兩次 CPU 刷新之間的等待（約 200 ms）則用 tokio 的 sleep，不佔用任何執行緒。
/// 重複使用同一個 `System`（移入 / 移出 blocking 閉包），避免每次重新掃描。
/// runtime 關閉中時回傳 `None`。
async fn sample(sys: System) -> Option<(System, Sample)> {
    let sys = blocking(move || {
        let mut sys = sys;
        sys.refresh_cpu_usage();
        sys
    })
    .await?;
    time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;
    blocking(move || {
        let mut sys = sys;
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let s = Sample {
            ts: now_ts(),
            cpu_usage: average_cpu_usage(&sys),
            total_memory: sys.total_memory() / BYTES_PER_MB,
            used_memory: sys.used_memory() / BYTES_PER_MB,
        };
        (sys, s)
    })
    .await
}

/// 收集 task：每 `interval` 取樣一次，以 `try_send` 交給連線 task。
///
/// 通道已滿（連線中斷或寫出卡住，上一筆還沒被取走）時**丟棄這次的新取樣**，
/// 因此不會堆積；連線 task 在每次註冊成功後會清掉通道內殘留的舊取樣。
/// 接收端關閉或 runtime 關閉時結束。
pub async fn collect_periodically(interval: Duration, tx: mpsc::Sender<Sample>) {
    let mut sys = System::new();
    let mut ticker = time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let Some((next, s)) = sample(sys).await else {
            return;
        };
        sys = next;
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

    #[tokio::test]
    async fn sample_runs_off_the_async_worker() {
        let (_, s) = sample(System::new()).await.unwrap();
        assert!(s.total_memory > 0);
        assert!(s.used_memory <= s.total_memory);
        assert!(s.cpu_usage.is_finite());
    }
}
