//! 主機資產的收集（`inventory` 訊息的內容，METRICS.md 第 6 節）。
//!
//! 取不到的欄位省略（不是送空字串或 0）；字串一律以 `sanitize_display_text` 整理到 256 bytes。
//! 磁碟與網路介面的篩選、label 與 `system.filesystem.*`／`system.network.*` 相同，但不受每類上限限制
//! （各最多 64 個）。收集在 blocking 執行緒進行（呼叫端負責逾時）。
//!
//! 序號（METRICS.md 第 4.5 節、第 17 節 D4）：Linux 的 `/sys/class/dmi/id/product_serial` 只有 root
//! 能讀，發佈包的 unit 以 `ExecStartPre=+` 複製到 [`SERIAL_COPY_PATH`]；Pulse 依序讀該檔、再試
//! sysinfo（以 root 互動執行時讀得到）。都讀不到時不送，記一次 `PLS-I8009`。

use std::collections::HashSet;

use kairos_proto::{
    CpuInventory, DiskInventory, InterfaceInventory, Inventory, InventoryReport,
    MAX_INTERFACE_ADDRESSES, MAX_INVENTORY_DISKS, MAX_INVENTORY_INTERFACES,
    MAX_INVENTORY_TEXT_BYTES, MAX_LINE_LENGTH, MemoryInventory, OsInventory, SystemInventory,
    is_ip_cidr, is_mac_address, sanitize_display_text,
};

use super::DiskEntry;
use super::readings::{InterfaceReading, prioritized_filesystems, prioritized_interfaces};

/// Linux：unit 的 `ExecStartPre=+` 複製出的系統序號（D4）。0.12.0 之後的 unit 複製到 root 擁有的
/// `/run/pulse-hw/`（安全稽核 A2：root 不在服務帳號可寫的目錄中寫入）。
/// 改變位置時同時改 `packaging/pulse.service`（測試比對兩者）
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // Linux 專用（測試在各平台都跑）
pub const SERIAL_COPY_PATH: &str = "/run/pulse-hw/product_serial";

/// 0.8.0～0.12.0 的 unit 複製的位置（`RuntimeDirectory=pulse`，擁有者是服務帳號）：新版執行檔搭配
/// 還沒更新的 unit 時仍讀得到序號
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const SERIAL_COPY_PATH_LEGACY: &str = "/run/pulse/product_serial";

/// 資產收集的設定（`[inventory]` 與 `collect.interface_exclude`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryOptions {
    pub hardware_serials: bool,
    pub network_addresses: bool,
    pub interface_exclude: Vec<String>,
}

/// 字串欄位：整理禁止字元、截到 256 bytes；空的為 `None`
fn text(v: Option<String>) -> Option<String> {
    v.and_then(|v| sanitize_display_text(&v, MAX_INVENTORY_TEXT_BYTES))
}

/// 廠商預設的佔位字串（主機板未燒錄序號時常見），視為取不到
fn is_placeholder_serial(s: &str) -> bool {
    const PLACEHOLDERS: &[&str] = &[
        "to be filled by o.e.m.",
        "default string",
        "system serial number",
        "not specified",
        "not applicable",
        "none",
        "n/a",
        "0",
        "00000000",
        "0123456789",
        "123456789",
        "chassis serial number",
    ];
    let t = s.trim().to_ascii_lowercase();
    t.is_empty() || PLACEHOLDERS.contains(&t.as_str())
}

/// 系統序號：依序嘗試各來源，略過佔位字串
pub fn pick_serial(sources: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    sources
        .into_iter()
        .flatten()
        .find(|s| !is_placeholder_serial(s))
        .and_then(|s| text(Some(s)))
}

/// CPU 型號字串中的標稱時脈（`Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz` → 2000）
pub fn nominal_mhz_from_brand(brand: &str) -> Option<u64> {
    let (_, tail) = brand.rsplit_once('@')?;
    let tail = tail.trim();
    let (num, mult) = match tail.strip_suffix("GHz") {
        Some(n) => (n, 1000.0),
        None => (tail.strip_suffix("MHz")?, 1.0),
    };
    let v: f64 = num.trim().parse().ok()?;
    (v.is_finite() && v > 0.0).then(|| (v * mult).round() as u64)
}

/// 介面的位址清單：CIDR 字串、去重、排序（IPv4 在前）、最多 32 個
pub fn interface_addresses(addrs: impl IntoIterator<Item = (std::net::IpAddr, u8)>) -> Vec<String> {
    let mut v: Vec<(bool, String)> = addrs
        .into_iter()
        .map(|(ip, prefix)| (ip.is_ipv6(), format!("{ip}/{prefix}")))
        .filter(|(_, s)| is_ip_cidr(s))
        .collect();
    v.sort();
    v.dedup();
    v.into_iter()
        .map(|(_, s)| s)
        .take(MAX_INTERFACE_ADDRESSES)
        .collect()
}

/// MAC 位址：小寫、`:` 分隔；全為 0 或格式不符時 `None`
pub fn mac_text(bytes: [u8; 6]) -> Option<String> {
    let s = bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    is_mac_address(&s).then_some(s)
}

/// 介面的完整讀數（資產用）
#[derive(Debug, Clone, Default)]
pub struct InterfaceDetail {
    pub reading: InterfaceReading,
    pub mac: Option<String>,
    pub addresses: Vec<String>,
    pub mtu: Option<u64>,
}

/// 磁碟與介面清單轉成資產的欄位（篩選、優先順序與 label 同指標，最多各 64 個）
pub(super) fn disks_and_interfaces(
    disks: Vec<DiskEntry>,
    root_mount: &str,
    interfaces: Vec<InterfaceDetail>,
    opts: &InventoryOptions,
) -> (Vec<DiskInventory>, Vec<InterfaceInventory>) {
    let disks = prioritized_filesystems(disks, root_mount)
        .into_iter()
        .take(MAX_INVENTORY_DISKS)
        .map(|d| DiskInventory {
            device: d.device.as_deref().and_then(|s| text(Some(s.to_string()))),
            fstype: text(Some(d.file_system.clone())),
            total: Some(d.total_space),
            kind: Some(
                match d.kind.to_ascii_lowercase().as_str() {
                    "ssd" => "ssd",
                    "hdd" => "hdd",
                    _ => "unknown",
                }
                .to_string(),
            ),
            removable: Some(d.removable),
            read_only: Some(d.read_only),
            mountpoint: d.mount_point,
        })
        .collect();
    let mut details: std::collections::HashMap<String, InterfaceDetail> = interfaces
        .into_iter()
        .map(|i| (i.reading.name.clone(), i))
        .collect();
    let readings: Vec<InterfaceReading> = details.values().map(|d| d.reading.clone()).collect();
    let mut seen = HashSet::new();
    let ifaces = prioritized_interfaces(readings, &opts.interface_exclude)
        .into_iter()
        .filter_map(|r| {
            // prioritized_interfaces 已把名稱整理成 label；以原名查回細節（名稱整理後不同的介面極少見）
            let detail = details
                .remove(&r.name)
                .or_else(|| {
                    let key = details
                        .keys()
                        .find(|k| super::label_value(k).as_deref() == Some(r.name.as_str()))?
                        .clone();
                    details.remove(&key)
                })
                .unwrap_or_default();
            seen.insert(r.name.clone()).then(|| InterfaceInventory {
                mac: detail.mac.filter(|_| opts.network_addresses),
                addresses: if opts.network_addresses {
                    detail.addresses
                } else {
                    Vec::new()
                },
                mtu: detail.mtu.filter(|m| *m > 0),
                name: r.name,
            })
        })
        .take(MAX_INVENTORY_INTERFACES)
        .collect();
    (disks, ifaces)
}

/// 單行上限（METRICS.md 第 6.3 節第 6 點）：整則 `inventory` 訊息超過 65536 bytes 時，依序捨去介面的
/// `addresses`（每個介面只留第一個 IPv4 與第一個 IPv6）、再捨去優先順序最低的介面與磁碟，直到不超過。
/// 回傳是否有捨去
pub fn fit_to_line(inv: &mut Inventory) -> bool {
    let too_long = |inv: &Inventory| {
        let msg = kairos_proto::ClientMsg::Inventory(Box::new(InventoryReport {
            seq: u64::MAX,
            ts: "2026-01-01T00:00:00.000Z".into(),
            inventory: inv.clone(),
        }));
        !serde_json::to_string(&msg).is_ok_and(|l| l.len() <= MAX_LINE_LENGTH)
    };
    if !too_long(inv) {
        return false;
    }
    if let Some(ifaces) = &mut inv.network_interfaces {
        for i in ifaces.iter_mut() {
            let v4 = i.addresses.iter().find(|a| !a.contains(':')).cloned();
            let v6 = i.addresses.iter().find(|a| a.contains(':')).cloned();
            i.addresses = v4.into_iter().chain(v6).collect();
        }
    }
    loop {
        if !too_long(inv) {
            return true;
        }
        let ifaces = inv.network_interfaces.as_mut().filter(|v| !v.is_empty());
        if let Some(ifaces) = ifaces {
            ifaces.pop();
            continue;
        }
        let disks = inv.disks.as_mut().filter(|v| !v.is_empty());
        if let Some(disks) = disks {
            disks.pop();
            continue;
        }
        // 剩下的欄位都很短（不會到這裡）；保險起見不再處理
        return true;
    }
}

/// 實際收集一次（blocking）。回傳資產與這次取不到的項目（`PLS-I8009` 用）
pub fn collect(opts: &InventoryOptions) -> (Inventory, Vec<&'static str>) {
    use sysinfo::{CpuRefreshKind, Disks, Motherboard, Networks, Product, System};

    let mut limited = Vec::new();
    let mut sys = System::new();
    sys.refresh_cpu_list(CpuRefreshKind::nothing().with_frequency());
    sys.refresh_memory();

    let os = OsInventory {
        family: text(Some(std::env::consts::OS.to_string())),
        name: text(System::name()),
        version: text(System::os_version()),
        long_version: text(System::long_os_version()),
        kernel: text(System::kernel_version()),
        arch: text(Some(System::cpu_arch())),
    };

    let boot_time = i64::try_from(System::boot_time())
        .ok()
        .filter(|t| *t > 0)
        .and_then(|t| chrono::DateTime::<chrono::Utc>::from_timestamp(t, 0))
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

    let first = sys.cpus().first();
    let brand = first.map(|c| c.brand().to_string()).unwrap_or_default();
    let cpu = CpuInventory {
        model: text(Some(brand.clone())),
        vendor: text(first.map(|c| c.vendor_id().to_string())),
        physical_cores: System::physical_core_count().and_then(|n| u32::try_from(n).ok()),
        logical_cores: u32::try_from(sys.cpus().len()).ok().filter(|n| *n > 0),
        frequency_mhz: nominal_mhz_from_brand(&brand).or_else(platform::nominal_mhz),
    };

    let memory = MemoryInventory {
        total: Some(sys.total_memory()).filter(|v| *v > 0),
        swap_total: Some(sys.total_swap()),
    };

    let board = Motherboard::new();
    let serial = if opts.hardware_serials {
        let serial = pick_serial(platform::serial_sources());
        if serial.is_none() {
            limited.push("system.serial");
        }
        serial
    } else {
        None
    };
    let (bios_vendor, bios_version) = platform::bios();
    let system = SystemInventory {
        vendor: text(Product::vendor_name()),
        product: text(Product::name()),
        serial,
        board_vendor: text(board.as_ref().and_then(Motherboard::vendor_name)),
        board_product: text(board.as_ref().and_then(Motherboard::name)),
        bios_vendor: text(bios_vendor),
        bios_version: text(bios_version),
    };

    let disk_list = Disks::new_with_refreshed_list();
    let resolver = super::DeviceResolver::new();
    let disks: Vec<DiskEntry> = disk_list
        .iter()
        .map(|d| DiskEntry::from_disk(d, &resolver))
        .collect();
    let networks = Networks::new_with_refreshed_list();
    let interfaces: Vec<InterfaceDetail> = networks
        .iter()
        .map(|(name, data)| {
            let addrs: Vec<(std::net::IpAddr, u8)> = data
                .ip_networks()
                .iter()
                .map(|n| (n.addr, n.prefix))
                .collect();
            InterfaceDetail {
                reading: super::interface_reading(name, data),
                mac: mac_text(data.mac_address().0),
                addresses: interface_addresses(addrs),
                mtu: Some(data.mtu()),
            }
        })
        .collect();
    let (disks, network_interfaces) =
        disks_and_interfaces(disks, &super::root_mount(), interfaces, opts);

    let mut inv = Inventory {
        os: Some(os),
        boot_time,
        cpu: Some(cpu),
        memory: Some(memory),
        system: Some(system),
        disks: Some(disks),
        network_interfaces: Some(network_interfaces),
    };
    if fit_to_line(&mut inv) {
        log::debug!("資產資訊超過單行上限，已捨去部分介面位址或優先順序低的介面與磁碟");
    }
    (inv, limited)
}

#[cfg(target_os = "linux")]
mod platform {
    fn read_trimmed(path: &str) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// 序號的來源（D4）：unit 複製的檔案 → sysinfo（root 才讀得到 /sys/class/dmi/id/product_serial）
    pub fn serial_sources() -> Vec<Option<String>> {
        vec![
            read_trimmed(super::SERIAL_COPY_PATH),
            read_trimmed(super::SERIAL_COPY_PATH_LEGACY),
            sysinfo::Product::serial_number(),
        ]
    }

    /// BIOS 廠商與版本（`/sys/class/dmi/id/bios_*`，所有人可讀）
    pub fn bios() -> (Option<String>, Option<String>) {
        (
            read_trimmed("/sys/class/dmi/id/bios_vendor"),
            read_trimmed("/sys/class/dmi/id/bios_version"),
        )
    }

    /// 標稱時脈（kHz → MHz）：`base_frequency`（intel_pstate）→ `cpuinfo_max_freq`。
    /// 不用 `/proc/cpuinfo` 的目前時脈（隨負載變動，會讓資產摘要一直改變）
    pub fn nominal_mhz() -> Option<u64> {
        [
            "/sys/devices/system/cpu/cpu0/cpufreq/base_frequency",
            "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq",
        ]
        .iter()
        .find_map(|p| read_trimmed(p)?.parse::<u64>().ok())
        .map(|khz| khz / 1000)
        .filter(|m| *m > 0)
    }
}

#[cfg(windows)]
mod platform {
    use crate::system_info::host::registry;

    pub fn serial_sources() -> Vec<Option<String>> {
        // SMBIOS（GetSystemFirmwareTable('RSMB')）不需要系統管理員權限
        vec![sysinfo::Product::serial_number()]
    }

    /// BIOS 廠商與版本（`HKLM\HARDWARE\DESCRIPTION\System\BIOS`）
    pub fn bios() -> (Option<String>, Option<String>) {
        const KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
        (
            registry::read_sz(KEY, "BIOSVendor"),
            registry::read_sz(KEY, "BIOSVersion"),
        )
    }

    /// 標稱時脈：`HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0` 的 `~MHz`
    /// （sysinfo 的時脈是目前時脈，會變動）
    pub fn nominal_mhz() -> Option<u64> {
        registry::read_dword(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "~MHz")
            .map(u64::from)
            .filter(|m| *m > 0)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    pub fn serial_sources() -> Vec<Option<String>> {
        vec![sysinfo::Product::serial_number()]
    }
    pub fn bios() -> (Option<String>, Option<String>) {
        (None, None)
    }
    pub fn nominal_mhz() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn opts() -> InventoryOptions {
        InventoryOptions {
            hardware_serials: true,
            network_addresses: true,
            interface_exclude: Vec::new(),
        }
    }

    fn disk(mount: &str, dev: &str, total: u64) -> DiskEntry {
        DiskEntry {
            mount_point: mount.into(),
            file_system: "ext4".into(),
            kind: "SSD".into(),
            name: format!("/dev/{dev}"),
            total_space: total,
            available_space: 1,
            free_space: None,
            removable: false,
            read_only: false,
            device: Some(dev.into()),
        }
    }

    fn iface(name: &str, ipv4: bool, addrs: &[&str]) -> InterfaceDetail {
        InterfaceDetail {
            reading: InterfaceReading {
                name: name.into(),
                has_ipv4: ipv4,
                rx_bytes: 10,
                ..Default::default()
            },
            mac: mac_text([0x52, 0x54, 0, 0x12, 0x34, 0x56]),
            addresses: addrs.iter().map(|s| s.to_string()).collect(),
            mtu: Some(1500),
        }
    }

    #[test]
    fn serial_placeholders_are_treated_as_missing() {
        assert_eq!(
            pick_serial([None, Some("7XK2LM3".into())]).as_deref(),
            Some("7XK2LM3")
        );
        assert_eq!(
            pick_serial([Some("To Be Filled By O.E.M.".into()), Some(" ".into())]),
            None
        );
        assert_eq!(
            pick_serial([Some("Default string".into()), Some("AB\tC".into())]).as_deref(),
            Some("AB?C"),
            "禁止字元換成 ?"
        );
        assert_eq!(pick_serial(Vec::<Option<String>>::new()), None);
    }

    #[test]
    fn nominal_frequency_comes_from_the_brand() {
        assert_eq!(
            nominal_mhz_from_brand("Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz"),
            Some(2000)
        );
        assert_eq!(
            nominal_mhz_from_brand("Intel(R) Core(TM) i7-8700 CPU @ 3.20GHz"),
            Some(3200)
        );
        assert_eq!(nominal_mhz_from_brand("Some CPU @ 800MHz"), Some(800));
        assert_eq!(
            nominal_mhz_from_brand("AMD EPYC 7763 64-Core Processor"),
            None
        );
        assert_eq!(nominal_mhz_from_brand("x @ fastGHz"), None);
    }

    #[test]
    fn addresses_and_mac_are_formatted_for_the_protocol() {
        let addrs = interface_addresses([
            ("fe80::5054:ff:fe12:3456".parse::<IpAddr>().unwrap(), 64),
            ("192.168.10.21".parse().unwrap(), 24),
            ("192.168.10.21".parse().unwrap(), 24),
        ]);
        assert_eq!(addrs, ["192.168.10.21/24", "fe80::5054:ff:fe12:3456/64"]);
        assert_eq!(
            mac_text([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]).as_deref(),
            Some("52:54:00:12:34:56")
        );
        assert_eq!(mac_text([0; 6]), None);
        let many = interface_addresses((0..40u8).map(|i| (IpAddr::from([10, 0, 0, i]), 32)));
        assert_eq!(many.len(), MAX_INTERFACE_ADDRESSES);
    }

    #[test]
    fn disks_and_interfaces_follow_the_metric_filters() {
        let mut usb = disk("/media/usb", "sdc1", 64);
        usb.removable = true;
        usb.kind = "HDD".into();
        let (disks, ifaces) = disks_and_interfaces(
            vec![
                disk("/data", "sdb1", 4000),
                disk("/", "nvme0n1p2", 500),
                usb,
                DiskEntry {
                    file_system: "tmpfs".into(),
                    ..disk("/run", "tmpfs", 10)
                },
            ],
            "/",
            vec![
                iface("lo", true, &["127.0.0.1/8"]),
                iface("docker0", true, &["172.17.0.1/16"]),
                iface("eth1", false, &[]),
                iface("eth0", true, &["192.168.10.21/24", "fe80::1/64"]),
            ],
            &opts(),
        );
        let mounts: Vec<&str> = disks.iter().map(|d| d.mountpoint.as_str()).collect();
        assert_eq!(mounts, ["/", "/data", "/media/usb"]);
        assert_eq!(disks[0].device.as_deref(), Some("nvme0n1p2"));
        assert_eq!(disks[0].kind.as_deref(), Some("ssd"));
        assert_eq!(disks[2].kind.as_deref(), Some("hdd"));
        assert_eq!(disks[2].removable, Some(true));
        let names: Vec<&str> = ifaces.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["eth0", "eth1"]);
        assert_eq!(ifaces[0].mac.as_deref(), Some("52:54:00:12:34:56"));
        assert_eq!(ifaces[0].addresses.len(), 2);
        let inv = Inventory {
            disks: Some(disks),
            network_interfaces: Some(ifaces),
            ..Default::default()
        };
        kairos_proto::validate_inventory(&inv).unwrap();

        // inventory.network_addresses = false：不送 MAC 與 IP
        let (_, ifaces) = disks_and_interfaces(
            Vec::new(),
            "/",
            vec![iface("eth0", true, &["10.0.0.1/8"])],
            &InventoryOptions {
                network_addresses: false,
                ..opts()
            },
        );
        assert_eq!(ifaces[0].mac, None);
        assert!(ifaces[0].addresses.is_empty());
        assert_eq!(ifaces[0].mtu, Some(1500));
    }

    #[test]
    fn oversized_inventory_is_trimmed_in_the_spec_order() {
        // 64 個介面 × 32 個位址 ≈ 100 KB：先只留第一個 IPv4 與 IPv6，仍超過時再捨去介面與磁碟
        let addrs: Vec<String> = std::iter::once("10.0.0.1/24".to_string())
            .chain((0..31).map(|i| format!("2001:db8:aaaa:bbbb:cccc:dddd:eeee:{i:x}/128")))
            .collect();
        let ifaces: Vec<InterfaceInventory> = (0..64)
            .map(|i| InterfaceInventory {
                name: format!("eth{i}"),
                mac: None,
                addresses: addrs.clone(),
                mtu: Some(1500),
            })
            .collect();
        let mut inv = Inventory {
            network_interfaces: Some(ifaces),
            ..Default::default()
        };
        assert!(fit_to_line(&mut inv));
        let ifaces = inv.network_interfaces.as_ref().unwrap();
        assert_eq!(ifaces.len(), 64, "只捨去位址就夠了");
        assert_eq!(
            ifaces[0].addresses,
            ["10.0.0.1/24", "2001:db8:aaaa:bbbb:cccc:dddd:eeee:0/128"]
        );
        kairos_proto::validate_inventory(&inv).unwrap();

        // 名稱很長的介面與磁碟：要捨去整個項目
        let long = "x".repeat(250);
        let mut inv = Inventory {
            network_interfaces: Some(
                (0..64)
                    .map(|i| InterfaceInventory {
                        name: format!("{i:02}{long}"),
                        mac: None,
                        addresses: vec![],
                        mtu: None,
                    })
                    .collect(),
            ),
            disks: Some(
                (0..64)
                    .map(|i| DiskInventory {
                        mountpoint: format!("/{i:02}{long}"),
                        device: Some(format!("{i:02}{long}")),
                        fstype: Some(long.clone()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        assert!(fit_to_line(&mut inv));
        let line = serde_json::to_string(&inv).unwrap();
        assert!(line.len() <= MAX_LINE_LENGTH);
        // 先捨去介面（優先順序最低的在後面），磁碟都還在
        assert_eq!(inv.disks.as_ref().unwrap().len(), 64);
        let left = inv.network_interfaces.as_ref().unwrap();
        assert!(!left.is_empty() && left.len() < 64, "{}", left.len());
        assert!(left[0].name.starts_with("00"));
        // 不需要時不動
        let mut small = Inventory::default();
        assert!(!fit_to_line(&mut small));
    }

    #[test]
    fn real_inventory_passes_the_protocol_validation() {
        let (inv, _limited) = collect(&opts());
        kairos_proto::validate_inventory(&inv).unwrap();
        let os = inv.os.as_ref().unwrap();
        assert_eq!(os.family.as_deref(), Some(std::env::consts::OS));
        assert!(inv.cpu.as_ref().unwrap().logical_cores.is_some());
        assert!(inv.memory.as_ref().unwrap().total.is_some());
        assert!(inv.boot_time.is_some());
        // 同一台主機連續收集兩次，摘要相同（不含開機時間與會變動的時脈）
        let (again, _) = collect(&opts());
        assert_eq!(
            kairos_proto::inventory_digest(&inv),
            kairos_proto::inventory_digest(&again)
        );
        // 關閉序號
        let (inv, limited) = collect(&InventoryOptions {
            hardware_serials: false,
            ..opts()
        });
        assert_eq!(inv.system.unwrap().serial, None);
        assert!(!limited.contains(&"system.serial"));
    }
}
