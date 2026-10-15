//! 磁碟 I/O 的來源與裝置名稱（METRICS.md 第 3.2 節）。
//!
//! - Linux：直接讀 `/proc/diskstats`（讀寫的磁區數 × 512、完成的讀寫次數、忙碌時間），
//!   sysinfo 只有讀寫量。裝置名稱為核心的區塊裝置名稱（`nvme0n1p2`、`sdb1`、`dm-0`）。
//! - Windows：sysinfo 的 `Disk::usage`（`IOCTL_DISK_PERFORMANCE`），裝置名稱為磁碟機代號（`C:`）。
//!
//! 解析是純函式，以測試資料驗證。

use std::collections::HashMap;

use super::readings::DiskIoReading;

/// `/proc/diskstats` 的磁區大小固定為 512 bytes（與硬體的磁區大小無關，見核心文件 iostats）
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // Linux 專用（測試在各平台都跑）
const SECTOR_BYTES: u64 = 512;

/// 解析 `/proc/diskstats`：每行為 `major minor name` 加上至少 11 個欄位
/// （第 4 欄讀取完成次數、第 6 欄讀取磁區、第 8 欄寫入完成次數、第 10 欄寫入磁區、第 13 欄忙碌毫秒）。
/// 欄位不足或不是數字的行略過。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // Linux 專用（測試在各平台都跑）
pub fn parse_diskstats(content: &str) -> HashMap<String, DiskIoReading> {
    let mut out = HashMap::new();
    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 14 {
            continue;
        }
        let num = |i: usize| fields[i].parse::<u64>().ok();
        let (Some(reads), Some(sectors_read), Some(writes), Some(sectors_written), Some(io_ms)) =
            (num(3), num(5), num(7), num(9), num(12))
        else {
            continue;
        };
        out.insert(
            fields[2].to_string(),
            DiskIoReading {
                read_bytes: sectors_read.saturating_mul(SECTOR_BYTES),
                written_bytes: sectors_written.saturating_mul(SECTOR_BYTES),
                read_ops: Some(reads),
                write_ops: Some(writes),
                io_time_ms: Some(io_ms),
            },
        );
    }
    out
}

/// 掛載點的裝置（`/proc/mounts` 的第一欄，例如 `/dev/nvme0n1p2`、`/dev/mapper/vg-root`）對應到
/// `/proc/diskstats` 的名稱。`dm_names` 為 `(dm-N, /sys/block/dm-N/dm/name)`。
///
/// systemd 的 `PrivateDevices=yes` 下 `/dev` 只有虛擬裝置，不能以 symlink 解析，因此：
/// `/dev/mapper/<名稱>` 以 `dm_names` 查；其他 `/dev/<名稱>` 取最後一段（`/dev/disk/by-*/` 等 symlink
/// 讀得到時先解析）。不是 `/dev/` 開頭（NFS、ZFS 資料集、btrfs subvolume 以外的虛擬來源）時 `None`
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // Linux 專用（測試在各平台都跑）
pub fn linux_device_label(
    source: &str,
    resolved: Option<&str>,
    dm_names: &[(String, String)],
) -> Option<String> {
    let source = source.trim();
    if let Some(name) = source.strip_prefix("/dev/mapper/")
        && let Some((dm, _)) = dm_names.iter().find(|(_, n)| n == name)
    {
        return Some(dm.clone());
    }
    let path = resolved.unwrap_or(source);
    let name = path.strip_prefix("/dev/")?;
    let last = name.rsplit('/').next().unwrap_or(name);
    (!last.is_empty()).then(|| last.to_string())
}

/// Linux：實際讀取 `/proc/diskstats`；讀不到時 `None`
#[cfg(target_os = "linux")]
pub fn read_linux_diskstats() -> Option<HashMap<String, DiskIoReading>> {
    std::fs::read_to_string("/proc/diskstats")
        .ok()
        .map(|c| parse_diskstats(&c))
}

/// Linux：`/sys/block/dm-*/dm/name`（device mapper 的名稱）
#[cfg(target_os = "linux")]
pub fn read_dm_names() -> Vec<(String, String)> {
    let Ok(dir) = std::fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    dir.filter_map(Result::ok)
        .filter_map(|e| {
            let dm = e.file_name().to_string_lossy().into_owned();
            if !dm.starts_with("dm-") {
                return None;
            }
            let name = std::fs::read_to_string(e.path().join("dm/name")).ok()?;
            Some((dm, name.trim().to_string()))
        })
        .collect()
}

/// Linux：掛載點裝置的 diskstats 名稱（讀得到 symlink 時先解析）
#[cfg(target_os = "linux")]
pub fn resolve_linux_device(source: &str, dm_names: &[(String, String)]) -> Option<String> {
    let resolved = std::fs::canonicalize(source.trim())
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    linux_device_label(source, resolved.as_deref(), dm_names)
}

#[cfg(any(not(target_os = "linux"), test))]
/// Windows：掛載點 `C:\` 的裝置名稱 `C:`（磁碟機代號以外的掛載點，例如掛載到資料夾的磁碟區，為 `None`）
pub fn windows_device_label(mount_point: &str) -> Option<String> {
    let m = mount_point.trim().trim_end_matches('\\');
    let mut chars = m.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic() => {
            Some(format!("{}:", letter.to_ascii_uppercase()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 實際主機的 `/proc/diskstats`（核心 5.x 起有 18 或 20 個欄位；舊核心 14 個）
    const SAMPLE: &str = "\
 259       0 nvme0n1 512345 1234 45823456 123456 912345 4567 3069135780 654321 0 8123456 777777 0 0 0 0 0 0
 259       2 nvme0n1p2 23456789 1000 1781925154 99999 45678901 2000 2997202911 88888 3 8123456 188887 0 0 0 0
   8      17 sdb1 345678 12 8921660 5555 9876543 21 1929012346 6666 0 2345678 12221
 253       0 dm-0 1 0 8 0 2 0 16 0 0 4 0 0 0 0 0
   7       0 loop0 x y z
 bad line
";

    #[test]
    fn diskstats_fields_are_parsed() {
        let stats = parse_diskstats(SAMPLE);
        assert_eq!(stats.len(), 4, "{stats:?}");
        let p2 = stats["nvme0n1p2"];
        assert_eq!(p2.read_ops, Some(23_456_789));
        assert_eq!(p2.read_bytes, 1_781_925_154 * 512);
        assert_eq!(p2.write_ops, Some(45_678_901));
        assert_eq!(p2.written_bytes, 2_997_202_911 * 512);
        assert_eq!(p2.io_time_ms, Some(8_123_456));
        // 舊核心的 14 欄格式
        let sdb1 = stats["sdb1"];
        assert_eq!(sdb1.read_bytes, 8_921_660 * 512);
        assert_eq!(sdb1.io_time_ms, Some(2_345_678));
        assert_eq!(stats["dm-0"].written_bytes, 16 * 512);
        assert!(!stats.contains_key("loop0"));
        // 每個值都是累計值，經過 validate_metrics（≥ 0、單位）
        assert!(parse_diskstats("").is_empty());
    }

    #[test]
    fn mount_sources_map_to_diskstats_names() {
        let dm = vec![
            ("dm-0".to_string(), "ubuntu--vg-ubuntu--lv".to_string()),
            ("dm-1".to_string(), "data".to_string()),
        ];
        assert_eq!(
            linux_device_label("/dev/nvme0n1p2", None, &dm).as_deref(),
            Some("nvme0n1p2")
        );
        // PrivateDevices=yes 下 /dev/mapper 的 symlink 不存在：以 /sys/block/dm-*/dm/name 對應
        assert_eq!(
            linux_device_label("/dev/mapper/ubuntu--vg-ubuntu--lv", None, &dm).as_deref(),
            Some("dm-0")
        );
        // 讀得到 symlink 時以解析結果為準
        assert_eq!(
            linux_device_label("/dev/disk/by-uuid/1234", Some("/dev/sdb1"), &dm).as_deref(),
            Some("sdb1")
        );
        assert_eq!(
            linux_device_label("/dev/mapper/unknown", Some("/dev/dm-7"), &dm).as_deref(),
            Some("dm-7")
        );
        // 不是區塊裝置的來源
        assert_eq!(linux_device_label("rpool/ROOT/ubuntu", None, &dm), None);
        assert_eq!(linux_device_label("nas:/export", None, &dm), None);
        assert_eq!(linux_device_label("/dev/", None, &dm), None);
    }

    #[test]
    fn windows_drive_letters() {
        assert_eq!(windows_device_label("C:\\").as_deref(), Some("C:"));
        assert_eq!(windows_device_label("d:").as_deref(), Some("D:"));
        assert_eq!(windows_device_label("C:\\Mount\\Data"), None);
        assert_eq!(windows_device_label(""), None);
    }
}
