//! 主機資產（feature `inventory`，METRICS.md「資產（inventory 訊息）」；商業審計 F4）。
//!
//! Pulse 在協商了 [`FEATURE_INVENTORY`] 的連線上送出 `inventory` 訊息（[`ClientMsg::Inventory`]
//! (crate::ClientMsg::Inventory)），內容是作業系統、CPU、記憶體、系統（廠牌、型號、序號、BIOS）、
//! 磁碟與網路介面的快照。每個欄位都可省略（取不到的就不送），伺服器只檢查格式與上限
//! （[`validate_inventory`]），以 [`inventory_digest`] 判斷內容是否改變。
//!
//! 沒有協商時 Pulse 不送；舊版 Kairos 收到 `inventory` 時與其他不認得的 `type` 相同，回 `unknown_type`。

use serde::{Deserialize, Serialize};

use crate::enroll::{parse_rfc3339_ms, sha256_hex};
use crate::validate::validate_display_text;

/// 主機資產（`inventory` 訊息）。雙方都宣告時才使用
pub const FEATURE_INVENTORY: &str = "inventory";

/// 資產中一般字串欄位的長度上限（bytes；字元限制同 `hostname`）
pub const MAX_INVENTORY_TEXT_BYTES: usize = 256;
/// `disks` 的項目數上限
pub const MAX_INVENTORY_DISKS: usize = 64;
/// `network_interfaces` 的項目數上限
pub const MAX_INVENTORY_INTERFACES: usize = 64;
/// 每個網路介面的 `addresses` 項目數上限
pub const MAX_INTERFACE_ADDRESSES: usize = 32;
/// 同一條連線上兩則 `inventory` 的最短間隔（毫秒）；更密的由伺服器回 `storage_unavailable`
/// （`inventory rate limit exceeded`，未保存，client 稍後重送）
pub const INVENTORY_MIN_INTERVAL_MS: u64 = 60_000;

/// `disks[].kind` 的值
pub const DISK_KINDS: [&str; 3] = ["ssd", "hdd", "unknown"];

/// `inventory` 訊息（Pulse → Kairos）。`type` 由 [`ClientMsg`](crate::ClientMsg) 加上
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InventoryReport {
    pub seq: u64,
    /// 收集資產的時間（RFC 3339 UTC 毫秒，同其他訊息的 `ts`）
    pub ts: String,
    pub inventory: Inventory,
}

/// 資產快照。全部欄位可省略；省略代表「這次沒有取得」，**不代表**該項目已不存在
/// （例如沒有權限讀序號時不送 `system.serial`，伺服器保留上一次的值，見 METRICS.md）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<OsInventory>,
    /// 開機時間（RFC 3339 UTC，精確到秒即可）。不列入 [`inventory_digest`]（重新開機不算資產變更）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuInventory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryInventory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInventory>,
    /// 實體磁碟區（與 `system.filesystem.*` 相同的篩選：不含虛擬檔案系統）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disks: Option<Vec<DiskInventory>>,
    /// 網路介面（含 loopback 以外的所有實體與虛擬介面中，Pulse 篩選後的部分）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_interfaces: Option<Vec<InterfaceInventory>>,
}

/// 作業系統（比 hello 的 `os` 多了 `long_version` 與 `family`）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsInventory {
    /// `linux`、`windows`、`macos` 等（小寫）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// 例 `Ubuntu`、`Windows`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 例 `24.04`、`11 (26200)`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 例 `Ubuntu 24.04.1 LTS`、`Windows 11 Pro 24H2`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_version: Option<String>,
    /// 例 `6.8.0-45-generic`、`26200`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// 例 `x86_64`、`aarch64`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
}

/// CPU
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuInventory {
    /// 型號，例 `Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 廠商，例 `GenuineIntel`、`AuthenticAMD`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// 實體核心數
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_cores: Option<u32>,
    /// 邏輯處理器數（含超執行緒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_cores: Option<u32>,
    /// 標稱時脈（MHz）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_mhz: Option<u64>,
}

/// 記憶體（bytes）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryInventory {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_total: Option<u64>,
}

/// 系統（SMBIOS／DMI）。序號類欄位受 Pulse 的 `inventory.hardware_serials` 控制
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemInventory {
    /// 系統廠牌，例 `Dell Inc.`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// 型號，例 `PowerEdge R650`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    /// 系統序號（服務標籤）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// 主機板廠牌
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_vendor: Option<String>,
    /// 主機板型號
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_product: Option<String>,
    /// BIOS／UEFI 廠商
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bios_vendor: Option<String>,
    /// BIOS／UEFI 版本
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bios_version: Option<String>,
}

/// 磁碟區（每個掛載點一項）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskInventory {
    /// 掛載點，與 `system.filesystem.*` 的 `mountpoint` label 相同（Linux `/`、Windows `C:\`）。必填
    pub mountpoint: String,
    /// 裝置，與 `system.disk.*` 的 `device` label 相同（Linux `nvme0n1p2`、Windows `C:`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// 檔案系統，例 `ext4`、`NTFS`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fstype: Option<String>,
    /// 容量（bytes）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// [`DISK_KINDS`] 之一
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

/// 網路介面
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceInventory {
    /// 介面名稱，與 `system.network.*` 的 `device` label 相同。必填
    pub name: String,
    /// MAC 位址，小寫、以 `:` 分隔的 6 組十六進位（[`is_mac_address`]）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// IP 位址與前綴長度（CIDR），例 `192.168.10.21/24`、`fe80::1/64`（[`is_ip_cidr`]）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u64>,
}

/// MAC 位址：`xx:xx:xx:xx:xx:xx`，小寫十六進位。全為 0 的位址不送（視為取不到）
pub fn is_mac_address(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        && s != "00:00:00:00:00:00"
}

/// IP 位址與前綴長度：`<IPv4>/<0..=32>` 或 `<IPv6>/<0..=128>`（IPv6 以 std 的寫法，不含 zone）
pub fn is_ip_cidr(s: &str) -> bool {
    let Some((ip, prefix)) = s.split_once('/') else {
        return false;
    };
    if prefix.is_empty() || prefix.len() > 3 || !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => prefix <= 32,
        Ok(std::net::IpAddr::V6(_)) => prefix <= 128,
        Err(_) => false,
    }
}

fn opt_text(field: &str, value: &Option<String>) -> Result<(), String> {
    match value {
        Some(v) => validate_display_text(field, v, MAX_INVENTORY_TEXT_BYTES),
        None => Ok(()),
    }
}

/// 檢查資產內容（違反時回 `invalid_report`，這則不保存）：字串欄位 1～256 bytes 且字元限制同
/// `hostname`、`boot_time` 為 RFC 3339、項目數上限、`mountpoint`／`name` 不得重複、MAC 與 CIDR 格式、
/// `disks[].kind` 為 [`DISK_KINDS`] 之一。數值欄位不檢查範圍（u64 本身已排除負數與小數）。
pub fn validate_inventory(inv: &Inventory) -> Result<(), String> {
    if let Some(os) = &inv.os {
        opt_text("os.family", &os.family)?;
        opt_text("os.name", &os.name)?;
        opt_text("os.version", &os.version)?;
        opt_text("os.long_version", &os.long_version)?;
        opt_text("os.kernel", &os.kernel)?;
        opt_text("os.arch", &os.arch)?;
    }
    if let Some(t) = &inv.boot_time {
        parse_rfc3339_ms(t).map_err(|e| format!("boot_time {e}"))?;
    }
    if let Some(cpu) = &inv.cpu {
        opt_text("cpu.model", &cpu.model)?;
        opt_text("cpu.vendor", &cpu.vendor)?;
    }
    if let Some(sys) = &inv.system {
        opt_text("system.vendor", &sys.vendor)?;
        opt_text("system.product", &sys.product)?;
        opt_text("system.serial", &sys.serial)?;
        opt_text("system.board_vendor", &sys.board_vendor)?;
        opt_text("system.board_product", &sys.board_product)?;
        opt_text("system.bios_vendor", &sys.bios_vendor)?;
        opt_text("system.bios_version", &sys.bios_version)?;
    }
    if let Some(disks) = &inv.disks {
        if disks.len() > MAX_INVENTORY_DISKS {
            return Err(format!(
                "disks: at most {MAX_INVENTORY_DISKS} entries (got {})",
                disks.len()
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for d in disks {
            validate_display_text("disks.mountpoint", &d.mountpoint, MAX_INVENTORY_TEXT_BYTES)?;
            if !seen.insert(d.mountpoint.as_str()) {
                return Err(format!(
                    "disks: mountpoint {:?} appears more than once",
                    d.mountpoint
                ));
            }
            opt_text("disks.device", &d.device)?;
            opt_text("disks.fstype", &d.fstype)?;
            if let Some(kind) = &d.kind
                && !DISK_KINDS.contains(&kind.as_str())
            {
                return Err(format!("disks.kind must be one of {DISK_KINDS:?}"));
            }
        }
    }
    if let Some(ifaces) = &inv.network_interfaces {
        if ifaces.len() > MAX_INVENTORY_INTERFACES {
            return Err(format!(
                "network_interfaces: at most {MAX_INVENTORY_INTERFACES} entries (got {})",
                ifaces.len()
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for i in ifaces {
            validate_display_text("network_interfaces.name", &i.name, MAX_INVENTORY_TEXT_BYTES)?;
            if !seen.insert(i.name.as_str()) {
                return Err(format!(
                    "network_interfaces: name {:?} appears more than once",
                    i.name
                ));
            }
            if let Some(mac) = &i.mac
                && !is_mac_address(mac)
            {
                return Err(format!(
                    "network_interfaces {}: mac must be 6 lowercase hex pairs separated by ':' and not all zero",
                    i.name
                ));
            }
            if i.addresses.len() > MAX_INTERFACE_ADDRESSES {
                return Err(format!(
                    "network_interfaces {}: at most {MAX_INTERFACE_ADDRESSES} addresses (got {})",
                    i.name,
                    i.addresses.len()
                ));
            }
            if let Some(bad) = i.addresses.iter().find(|a| !is_ip_cidr(a)) {
                return Err(format!(
                    "network_interfaces {}: address {:?} must be <ip>/<prefix>",
                    i.name,
                    bad.chars().take(64).collect::<String>()
                ));
            }
        }
    }
    Ok(())
}

/// 資產內容的摘要（`sha256:` 加 64 個小寫十六進位字元），用來判斷「內容是否改變」：
/// Pulse 決定是否需要再送、Kairos 決定是否寫入變更歷史。
///
/// 正規化：去除 `boot_time`；`disks` 依 `mountpoint`、`network_interfaces` 依 `name`、
/// 每個介面的 `addresses` 依字串排序；再以 serde_json 序列化（欄位順序固定、省略的欄位不輸出）。
/// 因此項目順序不同、只有開機時間不同的兩份資產摘要相同。
pub fn inventory_digest(inv: &Inventory) -> String {
    let mut canon = inv.clone();
    canon.boot_time = None;
    if let Some(disks) = &mut canon.disks {
        disks.sort_by(|a, b| a.mountpoint.cmp(&b.mountpoint));
    }
    if let Some(ifaces) = &mut canon.network_interfaces {
        ifaces.sort_by(|a, b| a.name.cmp(&b.name));
        for i in ifaces.iter_mut() {
            i.addresses.sort();
        }
    }
    // 這些型別只有字串、整數、布林與 Option／Vec，序列化不會失敗
    let bytes = serde_json::to_vec(&canon).unwrap_or_default();
    format!("sha256:{}", sha256_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Inventory {
        Inventory {
            os: Some(OsInventory {
                family: Some("linux".into()),
                name: Some("Ubuntu".into()),
                version: Some("24.04".into()),
                long_version: Some("Ubuntu 24.04.1 LTS".into()),
                kernel: Some("6.8.0-45-generic".into()),
                arch: Some("x86_64".into()),
            }),
            boot_time: Some("2026-10-02T08:11:04Z".into()),
            cpu: Some(CpuInventory {
                model: Some("Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz".into()),
                vendor: Some("GenuineIntel".into()),
                physical_cores: Some(32),
                logical_cores: Some(64),
                frequency_mhz: Some(2000),
            }),
            memory: Some(MemoryInventory {
                total: Some(51_539_607_552),
                swap_total: Some(0),
            }),
            system: None,
            disks: Some(vec![
                DiskInventory {
                    mountpoint: "/".into(),
                    device: Some("nvme0n1p2".into()),
                    fstype: Some("ext4".into()),
                    total: Some(502_468_108_288),
                    kind: Some("ssd".into()),
                    removable: Some(false),
                    read_only: Some(false),
                },
                DiskInventory {
                    mountpoint: "/data".into(),
                    ..Default::default()
                },
            ]),
            network_interfaces: Some(vec![
                InterfaceInventory {
                    name: "eth0".into(),
                    mac: Some("52:54:00:12:34:56".into()),
                    addresses: vec![
                        "192.168.10.21/24".into(),
                        "fe80::5054:ff:fe12:3456/64".into(),
                    ],
                    mtu: Some(1500),
                },
                InterfaceInventory {
                    name: "eth1".into(),
                    ..Default::default()
                },
            ]),
        }
    }

    #[test]
    fn mac_and_cidr_formats() {
        assert!(is_mac_address("52:54:00:12:34:56"));
        assert!(is_mac_address("ff:ff:ff:ff:ff:ff"));
        for bad in [
            "",
            "52:54:00:12:34",
            "52:54:00:12:34:56:78",
            "52-54-00-12-34-56",
            "52:54:00:12:34:5G",
            "52:54:00:12:34:5",
            "52:54:00:AB:34:56",
            "00:00:00:00:00:00",
        ] {
            assert!(!is_mac_address(bad), "{bad}");
        }
        for ok in [
            "10.0.0.1/8",
            "0.0.0.0/0",
            "192.168.1.5/32",
            "::1/128",
            "fe80::1/64",
        ] {
            assert!(is_ip_cidr(ok), "{ok}");
        }
        for bad in [
            "10.0.0.1",
            "10.0.0.1/33",
            "10.0.0.1/",
            "10.0.0.1/-1",
            "10.0.0.1/+8",
            "::1/129",
            "fe80::1%eth0/64",
            "host/24",
            "10.0.0.1/0008",
        ] {
            assert!(!is_ip_cidr(bad), "{bad}");
        }
    }

    #[test]
    fn valid_inventory_and_limits() {
        let inv = sample();
        assert_eq!(validate_inventory(&inv), Ok(()));
        assert_eq!(validate_inventory(&Inventory::default()), Ok(()));

        let mut bad = inv.clone();
        bad.boot_time = Some("2026-10-02 08:11:04".into());
        assert!(validate_inventory(&bad).is_err());

        let mut bad = inv.clone();
        bad.os.as_mut().unwrap().name = Some("a\nb".into());
        assert!(validate_inventory(&bad).is_err());

        let mut bad = inv.clone();
        bad.cpu.as_mut().unwrap().model = Some("x".repeat(MAX_INVENTORY_TEXT_BYTES + 1));
        assert!(validate_inventory(&bad).is_err());

        let mut bad = inv.clone();
        bad.disks.as_mut().unwrap()[1].mountpoint = "/".into();
        assert!(validate_inventory(&bad).is_err(), "重複的 mountpoint");

        let mut bad = inv.clone();
        bad.disks.as_mut().unwrap()[0].kind = Some("nvme".into());
        assert!(validate_inventory(&bad).is_err());

        let mut bad = inv.clone();
        bad.network_interfaces.as_mut().unwrap()[1].name = "eth0".into();
        assert!(validate_inventory(&bad).is_err(), "重複的介面名稱");

        let mut bad = inv.clone();
        bad.network_interfaces.as_mut().unwrap()[0].mac = Some("52:54:00:12:34:5G".into());
        assert!(validate_inventory(&bad).is_err());

        let mut bad = inv.clone();
        bad.network_interfaces.as_mut().unwrap()[0]
            .addresses
            .push("10.0.0.1".into());
        assert!(validate_inventory(&bad).is_err());

        let mut many = inv.clone();
        many.disks = Some(
            (0..MAX_INVENTORY_DISKS)
                .map(|i| DiskInventory {
                    mountpoint: format!("/m{i}"),
                    ..Default::default()
                })
                .collect(),
        );
        many.network_interfaces = Some(
            (0..MAX_INVENTORY_INTERFACES)
                .map(|i| InterfaceInventory {
                    name: format!("if{i}"),
                    addresses: (0..MAX_INTERFACE_ADDRESSES)
                        .map(|a| format!("10.{i}.{a}.1/24"))
                        .collect(),
                    ..Default::default()
                })
                .collect(),
        );
        assert_eq!(validate_inventory(&many), Ok(()));
        let mut too_many = many.clone();
        too_many.disks.as_mut().unwrap().push(DiskInventory {
            mountpoint: "/x".into(),
            ..Default::default()
        });
        assert!(validate_inventory(&too_many).is_err());
        let mut too_many = many.clone();
        too_many.network_interfaces.as_mut().unwrap()[0]
            .addresses
            .push("10.255.255.1/24".into());
        assert!(validate_inventory(&too_many).is_err());
        let mut too_many = many;
        too_many
            .network_interfaces
            .as_mut()
            .unwrap()
            .push(InterfaceInventory {
                name: "extra".into(),
                ..Default::default()
            });
        assert!(validate_inventory(&too_many).is_err());
    }

    #[test]
    fn digest_ignores_order_and_boot_time() {
        let a = sample();
        let d = inventory_digest(&a);
        assert!(d.starts_with("sha256:") && d.len() == 7 + 64, "{d}");

        let mut b = a.clone();
        b.boot_time = Some("2026-10-14T00:00:00Z".into());
        b.disks.as_mut().unwrap().reverse();
        b.network_interfaces.as_mut().unwrap().reverse();
        b.network_interfaces.as_mut().unwrap()[1]
            .addresses
            .reverse();
        assert_eq!(inventory_digest(&b), d);
        let mut c = a.clone();
        c.boot_time = None;
        assert_eq!(inventory_digest(&c), d);

        // 實際的變更會改變摘要
        let mut m = a.clone();
        m.memory.as_mut().unwrap().total = Some(68_719_476_736);
        assert_ne!(inventory_digest(&m), d);
        let mut m = a.clone();
        m.network_interfaces.as_mut().unwrap()[0].addresses[0] = "192.168.10.22/24".into();
        assert_ne!(inventory_digest(&m), d);
        // 「取不到」（省略）與「有值」不同
        let mut m = a;
        m.cpu = None;
        assert_ne!(inventory_digest(&m), d);
    }

    #[test]
    fn json_shape() {
        // 省略的欄位不輸出；不認得的欄位忽略；必填欄位缺漏是格式錯誤
        let v = serde_json::to_value(Inventory {
            disks: Some(vec![DiskInventory {
                mountpoint: "C:\\".into(),
                ..Default::default()
            }]),
            network_interfaces: Some(vec![InterfaceInventory {
                name: "Ethernet".into(),
                ..Default::default()
            }]),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"disks":[{"mountpoint":"C:\\"}],"network_interfaces":[{"name":"Ethernet"}]})
        );
        let inv: Inventory =
            serde_json::from_str(r#"{"future":1,"os":{"name":"Windows","x":true}}"#).unwrap();
        assert_eq!(inv.os.unwrap().name.as_deref(), Some("Windows"));
        for bad in [
            r#"{"disks":[{"device":"sda"}]}"#,
            r#"{"network_interfaces":[{"mac":"52:54:00:12:34:56"}]}"#,
            r#"{"cpu":{"logical_cores":-1}}"#,
            r#"{"memory":{"total":1.5}}"#,
            r#"{"disks":{}}"#,
            r#"{"network_interfaces":[{"name":"eth0","addresses":"10.0.0.1/8"}]}"#,
        ] {
            assert!(serde_json::from_str::<Inventory>(bad).is_err(), "{bad}");
        }
    }
}
