use serde::Serialize;
use sysinfo::{
    Components, Disks, Networks, System,
};

#[derive(Serialize)]
pub(crate) struct SystemInfo {
    cpu_usage: f32,
    total_memory: u64,
    used_memory: u64,
    hostname: String,
}

pub fn get_system_info() {
    // 初始化系統資訊
    let mut sys = System::new_all();

    // 刷新所有系統資訊
    sys.refresh_all();

    println!("=> 系統資訊:");
    // 記憶體資訊
    println!("總記憶體: {} MB", sys.total_memory() / 1024);
    println!("已使用記憶體: {} MB", sys.used_memory() / 1024);
    println!("總交換空間: {} MB", sys.total_swap() / 1024);
    println!("已使用交換空間: {} MB", sys.used_swap() / 1024);

    // 系統詳細資訊
    println!("系統名稱: {:?}", System::name());
    println!("系統核心版本: {:?}", System::kernel_version());
    println!("系統版本: {:?}", System::os_version());
    println!("主機名稱: {:?}", System::host_name());

    // CPU 數量
    println!("CPU 數量: {}", sys.cpus().len());
    // CPU 使用率
    println!("=> CPU 使用率:");
    for cpu in sys.cpus() {
        println!("CPU {}: {:.2}% 使用率", cpu.name(), cpu.cpu_usage());
    }

    // 磁碟資訊
    println!("=> 磁碟資訊:");
    let disks = Disks::new_with_refreshed_list();
    for disk in &disks {
        println!(
            "掛載位置: {:?}, 硬碟種類: {:?}, 磁碟名稱: {:?}, 總容量: {} GB, 可用空間: {} GB",
            disk.mount_point(),
            disk.kind(),
            disk.name(),
            disk.total_space() / 1_000_000_000,
            disk.available_space() / 1_000_000_000,
        );
    }

    // 網路介面資訊
    println!("=> 網路資訊:");
    let networks = Networks::new_with_refreshed_list();
    for (interface_name, data) in &networks {
        println!(
            "介面: {}, 已下載: {} B, 已上傳: {} B",
            interface_name,
            data.total_received(),
            data.total_transmitted(),
        );
    }

    // 元件溫度
    println!("=> 元件溫度:");
    let components = Components::new_with_refreshed_list();
    for component in &components {
        println!(
            "元件: {:?}, 溫度: {}°C",
            component.label(),
            component.temperature()
        );
    }
}

pub fn get_real_system_info() -> SystemInfo {
    let mut sys = System::new_all();
    sys.refresh_all();

    // 計算 CPU 使用率
    let cpu_usage: f32 = sys.cpus().iter().map(|cpu| cpu.cpu_usage()).sum();

    // 獲取記憶體資訊
    let total_memory = sys.total_memory() / 1024; // MB
    let used_memory = sys.used_memory() / 1024;   // MB

    // 獲取主機名稱
    let hostname = System::host_name().unwrap_or_else(|| "Unknown".to_string());

    SystemInfo {
        cpu_usage,
        total_memory,
        used_memory,
        hostname,
    }
}