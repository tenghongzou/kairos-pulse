//! 主機資產（`inventory` 訊息）的合併與變更歷史（`crates/kairos-proto/METRICS.md` 第 9.3 節）。
//!
//! - **合併**：收到的 `inventory` 中有出現的頂層物件（`os`、`cpu`、`memory`、`system`）逐欄覆蓋，省略的欄位
//!   保留舊值；`disks`、`network_interfaces` 有出現時整個取代（陣列中消失的項目代表已移除）。`boot_time`
//!   不保存在資產中（放在 `hosts.boot_time`，重新開機不算資產變更）。
//! - **變更歷史**：合併後的摘要（[`inventory_digest`]）改變時，以 [`diff`] 算出逐欄的差異。路徑以 `.` 串接，
//!   陣列以鍵值標示：`disks[/data].total`、`network_interfaces[eth0].addresses`、`network_interfaces[eth1]`
//!   （整個項目新增或移除時 `before` 或 `after` 為 `null`）。
//! - **IPv6 暫時位址**：Linux 與 Windows 預設啟用 IPv6 隱私延伸，全域位址（含 ULA）每隔數小時輪換，
//!   Pulse 會因此重送資產。變更歷史比較介面位址時，IPv6 的非 link-local 位址只比較所在的 /64 前綴
//!   （[`address_key`]），因此同一個前綴內的位址輪換不寫入歷史；前綴改變（換網段）、IPv4、link-local
//!   照常記錄。目前的資產（`host_inventory.body`）仍保存實際的位址。

use kairos_proto::{
    CpuInventory, DiskInventory, InterfaceInventory, Inventory, MemoryInventory, OsInventory,
    SystemInventory,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

/// 一個欄位的變更
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    pub path: String,
    pub before: Value,
    pub after: Value,
}

/// 每台主機的變更歷史最多保留的筆數（超過時刪最舊的；METRICS.md 第 9.3 節）
pub const MAX_CHANGES_PER_HOST: i64 = 1000;

fn over<T: Clone>(dst: &mut Option<T>, src: &Option<T>) {
    if src.is_some() {
        dst.clone_from(src);
    }
}

fn merge_os(dst: &mut OsInventory, src: &OsInventory) {
    over(&mut dst.family, &src.family);
    over(&mut dst.name, &src.name);
    over(&mut dst.version, &src.version);
    over(&mut dst.long_version, &src.long_version);
    over(&mut dst.kernel, &src.kernel);
    over(&mut dst.arch, &src.arch);
}

fn merge_cpu(dst: &mut CpuInventory, src: &CpuInventory) {
    over(&mut dst.model, &src.model);
    over(&mut dst.vendor, &src.vendor);
    over(&mut dst.physical_cores, &src.physical_cores);
    over(&mut dst.logical_cores, &src.logical_cores);
    over(&mut dst.frequency_mhz, &src.frequency_mhz);
}

fn merge_memory(dst: &mut MemoryInventory, src: &MemoryInventory) {
    over(&mut dst.total, &src.total);
    over(&mut dst.swap_total, &src.swap_total);
}

fn merge_system(dst: &mut SystemInventory, src: &SystemInventory) {
    over(&mut dst.vendor, &src.vendor);
    over(&mut dst.product, &src.product);
    over(&mut dst.serial, &src.serial);
    over(&mut dst.board_vendor, &src.board_vendor);
    over(&mut dst.board_product, &src.board_product);
    over(&mut dst.bios_vendor, &src.bios_vendor);
    over(&mut dst.bios_version, &src.bios_version);
}

/// 上一次的資產（沒有時 `None`）與這次收到的合併。結果不含 `boot_time`
pub fn merge(prev: Option<&Inventory>, new: &Inventory) -> Inventory {
    let mut out = prev.cloned().unwrap_or_default();
    out.boot_time = None;
    if let Some(os) = &new.os {
        merge_os(out.os.get_or_insert_with(Default::default), os);
    }
    if let Some(cpu) = &new.cpu {
        merge_cpu(out.cpu.get_or_insert_with(Default::default), cpu);
    }
    if let Some(mem) = &new.memory {
        merge_memory(out.memory.get_or_insert_with(Default::default), mem);
    }
    if let Some(sys) = &new.system {
        merge_system(out.system.get_or_insert_with(Default::default), sys);
    }
    if new.disks.is_some() {
        out.disks.clone_from(&new.disks);
    }
    if new.network_interfaces.is_some() {
        out.network_interfaces.clone_from(&new.network_interfaces);
    }
    out
}

/// 比較變更歷史時代表一個介面位址的鍵：IPv6 的非 link-local 位址以 /64 前綴代表（暫時位址的輪換
/// 不算變更），其他位址為原字串
pub fn address_key(cidr: &str) -> String {
    let ip = cidr.split('/').next().unwrap_or(cidr);
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => {
            let seg = v6.segments();
            let link_local = seg[0] & 0xffc0 == 0xfe80;
            if link_local || v6.is_loopback() || v6.is_unspecified() {
                cidr.to_string()
            } else {
                let net = std::net::Ipv6Addr::new(seg[0], seg[1], seg[2], seg[3], 0, 0, 0, 0);
                format!("{net}/64 (IPv6 前綴)")
            }
        }
        _ => cidr.to_string(),
    }
}

fn address_keys(addresses: &[String]) -> BTreeSet<String> {
    addresses.iter().map(|a| address_key(a)).collect()
}

fn to_object<T: Serialize>(v: &T) -> Map<String, Value> {
    match serde_json::to_value(v) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// 兩個物件逐欄比較（兩邊的鍵取聯集，缺少的欄位為 `null`）
fn diff_fields(
    prefix: &str,
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    out: &mut Vec<Change>,
) {
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for k in keys {
        let b = before.get(k).cloned().unwrap_or(Value::Null);
        let a = after.get(k).cloned().unwrap_or(Value::Null);
        if b != a {
            out.push(Change {
                path: format!("{prefix}.{k}"),
                before: b,
                after: a,
            });
        }
    }
}

fn diff_section<T: Serialize + Default>(
    name: &str,
    before: Option<&T>,
    after: Option<&T>,
    out: &mut Vec<Change>,
) {
    let b = before.map(to_object).unwrap_or_default();
    let a = after.map(to_object).unwrap_or_default();
    diff_fields(name, &b, &a, out);
}

fn disks_by_mount(d: Option<&Vec<DiskInventory>>) -> BTreeMap<String, Map<String, Value>> {
    d.map(|d| {
        d.iter()
            .map(|x| (x.mountpoint.clone(), to_object(x)))
            .collect()
    })
    .unwrap_or_default()
}

fn ifaces_by_name(d: Option<&Vec<InterfaceInventory>>) -> BTreeMap<String, &InterfaceInventory> {
    d.map(|d| d.iter().map(|x| (x.name.clone(), x)).collect())
        .unwrap_or_default()
}

fn sorted_addresses(i: &InterfaceInventory) -> Value {
    let mut a = i.addresses.clone();
    a.sort();
    json!(a)
}

/// 合併後的兩份資產之間的差異（`before` 為上一次合併後的結果）。`boot_time` 不比較；介面位址的
/// IPv6 暫時位址輪換不算變更（[`address_key`]）。沒有差異時為空
pub fn diff(before: &Inventory, after: &Inventory) -> Vec<Change> {
    let mut out = Vec::new();
    diff_section("os", before.os.as_ref(), after.os.as_ref(), &mut out);
    diff_section("cpu", before.cpu.as_ref(), after.cpu.as_ref(), &mut out);
    diff_section(
        "memory",
        before.memory.as_ref(),
        after.memory.as_ref(),
        &mut out,
    );
    diff_section(
        "system",
        before.system.as_ref(),
        after.system.as_ref(),
        &mut out,
    );
    // 磁碟：依掛載點
    let (bd, ad) = (
        disks_by_mount(before.disks.as_ref()),
        disks_by_mount(after.disks.as_ref()),
    );
    let keys: BTreeSet<&String> = bd.keys().chain(ad.keys()).collect();
    for k in keys {
        match (bd.get(k), ad.get(k)) {
            (Some(b), Some(a)) => {
                let mut fields = Map::new();
                for (key, v) in b {
                    if key != "mountpoint" {
                        fields.insert(key.clone(), v.clone());
                    }
                }
                let mut after_fields = a.clone();
                after_fields.remove("mountpoint");
                diff_fields(&format!("disks[{k}]"), &fields, &after_fields, &mut out);
            }
            (b, a) => out.push(Change {
                path: format!("disks[{k}]"),
                before: b.map_or(Value::Null, |m| Value::Object(m.clone())),
                after: a.map_or(Value::Null, |m| Value::Object(m.clone())),
            }),
        }
    }
    // 介面：依名稱；位址以 address_key 比較
    let (bi, ai) = (
        ifaces_by_name(before.network_interfaces.as_ref()),
        ifaces_by_name(after.network_interfaces.as_ref()),
    );
    let keys: BTreeSet<&String> = bi.keys().chain(ai.keys()).collect();
    for k in keys {
        match (bi.get(k), ai.get(k)) {
            (Some(b), Some(a)) => {
                let prefix = format!("network_interfaces[{k}]");
                if b.mac != a.mac {
                    out.push(Change {
                        path: format!("{prefix}.mac"),
                        before: json!(b.mac),
                        after: json!(a.mac),
                    });
                }
                if b.mtu != a.mtu {
                    out.push(Change {
                        path: format!("{prefix}.mtu"),
                        before: json!(b.mtu),
                        after: json!(a.mtu),
                    });
                }
                if address_keys(&b.addresses) != address_keys(&a.addresses) {
                    out.push(Change {
                        path: format!("{prefix}.addresses"),
                        before: sorted_addresses(b),
                        after: sorted_addresses(a),
                    });
                }
            }
            (b, a) => out.push(Change {
                path: format!("network_interfaces[{k}]"),
                before: b.map_or(Value::Null, |i| Value::Object(to_object(*i))),
                after: a.map_or(Value::Null, |i| Value::Object(to_object(*i))),
            }),
        }
    }
    out
}

/// `boot_time`（RFC 3339）→ Unix 秒
pub fn boot_time_secs(inv: &Inventory) -> Option<i64> {
    inv.boot_time
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Inventory {
        serde_json::from_value(json!({
            "os": {"family": "linux", "name": "Ubuntu", "version": "24.04", "long_version": "Linux (Ubuntu 24.04)", "kernel": "6.8.0-45-generic", "arch": "x86_64"},
            "boot_time": "2026-10-02T08:11:04Z",
            "cpu": {"model": "Xeon", "vendor": "GenuineIntel", "physical_cores": 4, "logical_cores": 8, "frequency_mhz": 2000},
            "memory": {"total": 8_589_934_592_u64, "swap_total": 0},
            "system": {"vendor": "QEMU", "product": "Standard PC", "serial": "S1"},
            "disks": [{"mountpoint": "/", "device": "vda1", "fstype": "ext4", "total": 100, "kind": "hdd", "removable": false, "read_only": false}],
            "network_interfaces": [{"name": "eth0", "mac": "52:54:00:12:34:56", "addresses": ["192.168.10.21/24", "2001:db8:1:2:1111:2222:3333:4444/64", "fe80::5054:ff:fe12:3456/64"], "mtu": 1500}]
        }))
        .unwrap()
    }

    #[test]
    fn omitted_fields_keep_the_old_values_and_arrays_are_replaced() {
        let first = merge(None, &sample());
        assert_eq!(first.boot_time, None);
        // 這次沒有序號（讀不到）、沒有磁碟清單；介面只剩 eth1
        let next: Inventory = serde_json::from_value(json!({
            "system": {"vendor": "QEMU", "product": "Standard PC"},
            "memory": {"total": 17_179_869_184_u64},
            "network_interfaces": [{"name": "eth1", "addresses": []}]
        }))
        .unwrap();
        let merged = merge(Some(&first), &next);
        let sys = merged.system.as_ref().unwrap();
        assert_eq!(sys.serial.as_deref(), Some("S1"));
        assert_eq!(merged.memory.as_ref().unwrap().swap_total, Some(0));
        assert_eq!(merged.memory.as_ref().unwrap().total, Some(17_179_869_184));
        assert_eq!(merged.disks, first.disks);
        let ifaces = merged.network_interfaces.as_ref().unwrap();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].name, "eth1");
        let changes = diff(&first, &merged);
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "memory.total",
                "network_interfaces[eth0]",
                "network_interfaces[eth1]"
            ]
        );
        assert_eq!(changes[0].before, json!(8_589_934_592_u64));
        assert_eq!(changes[0].after, json!(17_179_869_184_u64));
        assert_eq!(changes[1].after, Value::Null);
        assert_eq!(changes[2].before, Value::Null);
        assert_eq!(changes[2].after["name"], "eth1");
    }

    #[test]
    fn disk_changes_use_the_mountpoint_as_the_key() {
        let a = merge(None, &sample());
        let mut b = a.clone();
        let disks = b.disks.as_mut().unwrap();
        disks[0].total = Some(200);
        disks.push(DiskInventory {
            mountpoint: "/data".into(),
            total: Some(5),
            ..Default::default()
        });
        let changes = diff(&a, &b);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "disks[/].total");
        assert_eq!(changes[0].before, json!(100));
        assert_eq!(changes[1].path, "disks[/data]");
        assert_eq!(changes[1].before, Value::Null);
        assert_eq!(changes[1].after["total"], 5);
        // 只有順序不同：沒有差異
        let mut c = b.clone();
        c.disks.as_mut().unwrap().reverse();
        assert!(diff(&b, &c).is_empty());
    }

    #[test]
    fn ipv6_temporary_address_rotation_is_not_a_change() {
        let a = merge(None, &sample());
        let mut b = a.clone();
        b.network_interfaces.as_mut().unwrap()[0].addresses = vec![
            "192.168.10.21/24".into(),
            "2001:db8:1:2:aaaa:bbbb:cccc:dddd/64".into(),
            "fe80::5054:ff:fe12:3456/64".into(),
        ];
        assert!(diff(&a, &b).is_empty());
        // 摘要仍然不同（目前的資產保存實際的位址）
        assert_ne!(
            kairos_proto::inventory_digest(&a),
            kairos_proto::inventory_digest(&b)
        );
        // 換到另一個 /64：記錄
        b.network_interfaces.as_mut().unwrap()[0].addresses[1] =
            "2001:db8:1:3:aaaa:bbbb:cccc:dddd/64".into();
        let changes = diff(&a, &b);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "network_interfaces[eth0].addresses");
        // IPv4 改變：記錄
        let mut c = a.clone();
        c.network_interfaces.as_mut().unwrap()[0].addresses[0] = "192.168.10.22/24".into();
        assert_eq!(diff(&a, &c).len(), 1);
        assert_eq!(address_key("fe80::1/64"), "fe80::1/64");
        assert_eq!(
            address_key("2001:db8:1:2::5/64"),
            "2001:db8:1:2::/64 (IPv6 前綴)"
        );
        assert_eq!(address_key("10.0.0.1/8"), "10.0.0.1/8");
    }

    #[test]
    fn boot_time_is_not_part_of_the_asset() {
        let a = merge(None, &sample());
        let mut later = sample();
        later.boot_time = Some("2026-10-20T00:00:00Z".into());
        let b = merge(Some(&a), &later);
        assert!(diff(&a, &b).is_empty());
        assert_eq!(boot_time_secs(&later), Some(1_792_454_400));
    }
}
