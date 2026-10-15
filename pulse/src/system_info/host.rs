//! hello 的 v2 主機資訊（A3；設計包 `enrollment-ux.md` §12 #5、`screens.md` §24 #15）：
//! 主機指紋、作業系統資訊、顯示名稱、標籤與 report 間隔。
//!
//! 主機指紋的計算方式由 PROTOCOL.md「主機指紋」規定：機器 ID（Linux `/etc/machine-id`、
//! Windows `MachineGuid`）去除前後空白、轉成 ASCII 小寫後，計算
//! `SHA-256("kairos-pulse host-fingerprint v1\n" + 機器 ID)`，只送出雜湊值。
//! SHA-256 使用 rustls 已內建的 ring 實作（不新增依賴）。

use std::time::Duration;
use sysinfo::System;

use kairos_proto::{HostInfo, MAX_OS_FIELD_BYTES, OsInfo, sanitize_display_text};

/// 主機指紋的用途字串（domain separation），讓雜湊值不能與其他軟體對機器 ID 的雜湊比對
const FINGERPRINT_DOMAIN: &str = "kairos-pulse host-fingerprint v1\n";

/// 收集 hello 的主機資訊。內含讀檔／登錄機碼與 sysinfo 的系統呼叫（都很快、只在啟動時呼叫一次）。
///
/// `display_name`、`tags` 已在載入設定時驗證（空字串的 `display_name` 表示不送）。
pub fn collect(display_name: &str, tags: &[String], report_interval: Duration) -> HostInfo {
    let os = os_info();
    HostInfo {
        host_fingerprint: read_machine_id().as_deref().and_then(fingerprint_of),
        display_name: Some(display_name.trim().to_string()).filter(|n| !n.is_empty()),
        tags: (!tags.is_empty()).then(|| tags.to_vec()),
        report_interval_ms: u64::try_from(report_interval.as_millis()).ok(),
        os: (!os.is_empty()).then_some(os),
    }
}

/// 由機器 ID 計算主機指紋；機器 ID 為空時回傳 `None`
pub fn fingerprint_of(machine_id: &str) -> Option<String> {
    let id = machine_id.trim().to_ascii_lowercase();
    if id.is_empty() {
        return None;
    }
    Some(format!(
        "sha256:{}",
        sha256_hex(format!("{FINGERPRINT_DOMAIN}{id}").as_bytes())
    ))
}

/// SHA-256（小寫十六進位）。借用 rustls 的 ring 加密後端（TLS 1.3 cipher suite 的雜湊）
fn sha256_hex(data: &[u8]) -> String {
    use tokio_rustls::rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256;
    let suite = TLS13_AES_128_GCM_SHA256
        .tls13()
        .expect("TLS13_AES_128_GCM_SHA256 是 TLS 1.3 cipher suite");
    suite
        .common
        .hash_provider
        .hash(data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 作業系統資訊（取不到的欄位省略），已整理成符合協定限制的字串
fn os_info() -> OsInfo {
    let clean = |v: Option<String>| v.and_then(|v| sanitize_display_text(&v, MAX_OS_FIELD_BYTES));
    OsInfo {
        name: clean(System::name()),
        version: clean(System::os_version()),
        kernel: clean(System::kernel_version()),
        arch: clean(Some(System::cpu_arch())),
    }
}

/// 機器 ID：Linux 為 `/etc/machine-id`（沒有時 `/var/lib/dbus/machine-id`）
#[cfg(target_os = "linux")]
fn read_machine_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
}

/// 機器 ID：Windows 為 `HKLM\SOFTWARE\Microsoft\Cryptography` 的 `MachineGuid`
#[cfg(windows)]
fn read_machine_id() -> Option<String> {
    registry::machine_guid()
}

/// 其他平台：沒有定義機器 ID 的來源，不送主機指紋
#[cfg(not(any(target_os = "linux", windows)))]
fn read_machine_id() -> Option<String> {
    None
}

/// Windows 登錄（`HKEY_LOCAL_MACHINE`）的讀取：機器 ID、資產的 BIOS 與 CPU 標稱時脈
#[cfg(windows)]
pub(crate) mod registry {
    use std::ffi::c_void;

    /// `HKEY_LOCAL_MACHINE`：`(HKEY)(ULONG_PTR)(LONG)0x80000002`（符號延伸成指標寬度）
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
    /// `RRF_RT_REG_SZ`：只接受 REG_SZ
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    /// `RRF_RT_REG_DWORD`：只接受 REG_DWORD
    const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
    /// `RRF_SUBKEY_WOW6464KEY`：一律讀 64 位元檢視（32 位元行程也讀到同一個值）
    const RRF_SUBKEY_WOW6464KEY: u32 = 0x0001_0000;
    const ERROR_SUCCESS: i32 = 0;

    // 與 file_perms 相同，以 raw-dylib 連結 advapi32.dll，不引入新的 crate
    #[link(name = "advapi32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn RegGetValueW(
            hkey: isize,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            value_type: *mut u32,
            data: *mut c_void,
            data_len: *mut u32,
        ) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `HKLM\<sub_key>` 的 REG_SZ 值（最多 1023 個字元）；不存在、型別不符或為空時 `None`
    pub fn read_sz(sub_key: &str, value: &str) -> Option<String> {
        let sub_key = wide(sub_key);
        let value = wide(value);
        let mut buf = [0u16; 1024];
        let mut len = u32::try_from(std::mem::size_of_val(&buf)).ok()?;
        // SAFETY: 字串皆以 NUL 結尾且在呼叫期間有效；data 指向 buf，len 為其位元組數，
        // API 最多寫入 len 個位元組並回寫實際長度
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                sub_key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let units = (len as usize / 2).min(buf.len());
        let text = String::from_utf16_lossy(&buf[..units]);
        let text = text.trim_end_matches('\0').trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    /// `HKLM\<sub_key>` 的 REG_DWORD 值；不存在或型別不符時 `None`
    pub fn read_dword(sub_key: &str, value: &str) -> Option<u32> {
        let sub_key = wide(sub_key);
        let value = wide(value);
        let mut data: u32 = 0;
        let mut len = u32::try_from(std::mem::size_of::<u32>()).ok()?;
        // SAFETY: 同上；data 指向一個 u32，len 為 4
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                sub_key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY,
                std::ptr::null_mut(),
                (&raw mut data).cast(),
                &mut len,
            )
        };
        (status == ERROR_SUCCESS).then_some(data)
    }

    pub(super) fn machine_guid() -> Option<String> {
        read_sz(r"SOFTWARE\Microsoft\Cryptography", "MachineGuid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PROTOCOL.md「主機指紋」的計算範例：| `機器 ID` | `sha256:…` |
    fn spec_examples() -> Vec<(String, String)> {
        const SPEC: &str = include_str!("../../../crates/kairos-proto/PROTOCOL.md");
        SPEC.lines()
            .filter_map(|l| {
                let cells: Vec<&str> = l.split('|').map(str::trim).collect();
                (cells.len() == 4 && cells[2].starts_with("`sha256:")).then(|| {
                    (
                        cells[1].trim_matches('`').to_string(),
                        cells[2].trim_matches('`').to_string(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn fingerprint_matches_the_spec_examples() {
        let examples = spec_examples();
        assert_eq!(examples.len(), 2, "{examples:?}");
        for (machine_id, expected) in examples {
            assert_eq!(
                fingerprint_of(&machine_id).as_deref(),
                Some(expected.as_str())
            );
        }
    }

    #[test]
    fn fingerprint_normalizes_and_rejects_empty_ids() {
        let a = fingerprint_of("ABCDEF").unwrap();
        assert_eq!(fingerprint_of("  abcdef\n").as_deref(), Some(a.as_str()));
        assert!(kairos_proto::is_host_fingerprint(&a), "{a}");
        assert_eq!(fingerprint_of(""), None);
        assert_eq!(fingerprint_of(" \n"), None);
        // 不是原始 ID 的單純雜湊（有用途字串）
        assert_ne!(a, format!("sha256:{}", sha256_hex(b"abcdef")));
    }

    #[test]
    fn sha256_known_answer() {
        // FIPS 180-2 的 "abc" 測試向量
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn collected_host_info_passes_the_protocol_validation() {
        let tags = vec!["env:prod".to_string()];
        let host = collect(" Web 01 ", &tags, Duration::from_secs(5));
        assert_eq!(host.display_name.as_deref(), Some("Web 01"));
        assert_eq!(host.tags, Some(tags));
        assert_eq!(host.report_interval_ms, Some(5000));
        // Windows 一定有 MachineGuid；Linux 容器可能沒有 /etc/machine-id，不檢查
        #[cfg(windows)]
        assert!(host.host_fingerprint.is_some(), "{host:?}");
        assert!(
            host.os.as_ref().is_some_and(|os| os.arch.is_some()),
            "{host:?}"
        );
        kairos_proto::validate_host_info(&host).unwrap();
        // 沒有設定顯示名稱與標籤時省略
        let host = collect("", &[], Duration::from_secs(5));
        assert_eq!((host.display_name, host.tags), (None, None));
    }

    #[test]
    fn machine_id_is_stable() {
        assert_eq!(read_machine_id(), read_machine_id());
    }
}
