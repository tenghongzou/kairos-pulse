//! Pulse ↔ Kairos 協定訊息（共同規格 v1）：Pulse 端的輔助函式。
//!
//! 訊息型別、序列化與欄位驗證在兩個產品共用的 `kairos-proto` crate（規格見
//! `crates/kairos-proto/PROTOCOL.md`），這裡重新匯出 Pulse 用到的部分，呼叫端仍以
//! `crate::protocol::…` 取用。傳輸為 NDJSON：一行一則 JSON 物件，欄位名稱與 `type` 值為定案；
//! 解析時忽略未知欄位。

use chrono::{SecondsFormat, Utc};

pub use kairos_proto::{
    ClientMsg, MAX_LINE_LENGTH, Proto, ReportBody, ServerMsg, ServerMsgError,
    is_forbidden_display_char, parse_server_msg,
};

/// 本程式送出與接受的協定版本（只支援 v1；版本協商見 kairos-proto）
pub const PROTO_VERSION: u32 = kairos_proto::PROTO_V1;

// `heartbeat.interval` 的設定上限（秒）必須與伺服器的夾值上限一致（PROTOCOL.md「hello 欄位限制」）
const _: () = assert!(
    crate::config_manager::MAX_HEARTBEAT_INTERVAL * 1000 == kairos_proto::MAX_HEARTBEAT_INTERVAL_MS
);

/// 目前時間，RFC 3339 UTC 毫秒格式（例如 `2026-10-08T03:15:40.001Z`）。
pub fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 送出前整理 CPU 使用率：非有限值（NaN / ±∞）視為 0，再夾在 0～100，
/// 避免伺服器回 `invalid_report`。
pub fn sanitize_cpu_usage(cpu: f32) -> f32 {
    if cpu.is_finite() {
        cpu.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// report 實際送出的 `cpu_usage`：先以 [`sanitize_cpu_usage`] 整理，再轉成 f64 四捨五入到小數 2 位。
///
/// 在 f64 上以「乘 100 → round → 除以 100」計算，結果是最接近 `n / 100` 的 f64，
/// serde_json 以最短表示法輸出，因此 JSON 中最多只有 2 位小數（例如 f32 的 `7.5000005` → `7.5`）。
/// 結果仍是 0～100 的有限值，符合 PROTOCOL.md 的範圍（Kairos 端的驗證見 `kairos_proto::validate_report`）。
pub fn report_cpu_usage(cpu: f32) -> f64 {
    let cpu = f64::from(sanitize_cpu_usage(cpu));
    ((cpu * 100.0).round() / 100.0).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 協定測試向量（PROTOCOL.md「範例」節）的序列化與解析測試在 kairos-proto，
    // 這裡只測 Pulse 端的輔助函式。

    /// report 序列化後的 `"cpu_usage":...` 數值字串。
    fn serialized_cpu(cpu: f32) -> String {
        let line = serde_json::to_string(&ClientMsg::Report {
            seq: 1,
            ts: "2026-10-09T00:00:00.000Z".into(),
            body: ReportBody::legacy(report_cpu_usage(cpu), 2, 1),
        })
        .unwrap();
        let start = line.find("\"cpu_usage\":").unwrap() + "\"cpu_usage\":".len();
        let rest = &line[start..];
        rest[..rest.find(',').unwrap()].to_string()
    }

    #[test]
    fn serialized_line_has_no_newline() {
        // LinesCodec 會補 `\n`，序列化結果本身不得含換行
        let line = serde_json::to_string(&ClientMsg::Heartbeat {
            seq: 1,
            ts: now_ts(),
        })
        .unwrap();
        assert!(!line.contains('\n'));
    }

    #[test]
    fn cpu_usage_is_sanitized() {
        assert_eq!(sanitize_cpu_usage(17.84), 17.84);
        assert_eq!(sanitize_cpu_usage(0.0), 0.0);
        assert_eq!(sanitize_cpu_usage(100.0), 100.0);
        assert_eq!(sanitize_cpu_usage(100.5), 100.0);
        assert_eq!(sanitize_cpu_usage(-3.0), 0.0);
        assert_eq!(sanitize_cpu_usage(f32::NAN), 0.0);
        assert_eq!(sanitize_cpu_usage(f32::INFINITY), 0.0);
        assert_eq!(sanitize_cpu_usage(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn cpu_usage_is_rounded_to_two_decimals_without_f32_noise() {
        // F7：部署測試看到的 f32 雜訊
        assert_eq!(serialized_cpu(7.500_000_5), "7.5");
        assert_eq!(serialized_cpu(2.304_609_3), "2.3");
        assert_eq!(serialized_cpu(17.84), "17.84");
        assert_eq!(serialized_cpu(33.333_336), "33.33");
        assert_eq!(serialized_cpu(66.666_67), "66.67");
        assert_eq!(serialized_cpu(0.004), "0.0");
        assert_eq!(serialized_cpu(0.005_1), "0.01");
        assert_eq!(serialized_cpu(99.996), "100.0");
        // 範圍與非有限值的處理不變（PROTOCOL.md：有限值、0～100）
        assert_eq!(serialized_cpu(100.5), "100.0");
        assert_eq!(serialized_cpu(-3.0), "0.0");
        assert_eq!(serialized_cpu(f32::NAN), "0.0");
        assert_eq!(serialized_cpu(f32::INFINITY), "0.0");
        // 全範圍掃描：JSON 數值最多 2 位小數、仍在 0～100、與原值相差不超過 0.005（加上 f32 誤差）
        let mut cpu = 0.0_f32;
        while cpu <= 100.0 {
            let text = serialized_cpu(cpu);
            let decimals = text.split('.').nth(1).map_or(0, str::len);
            assert!(decimals <= 2, "{cpu} → {text}");
            let v: f64 = text.parse().unwrap();
            assert!((0.0..=100.0).contains(&v), "{cpu} → {text}");
            assert!((v - f64::from(cpu)).abs() <= 0.005 + 1e-6, "{cpu} → {text}");
            assert_eq!(v, report_cpu_usage(cpu), "反序列化後與送出的值相同");
            cpu += 0.013_7;
        }
    }

    #[test]
    fn agent_version_has_no_forbidden_chars() {
        // agent_version 取自編譯時的 CARGO_PKG_VERSION（semver），不會含禁止字元
        let v = env!("CARGO_PKG_VERSION");
        assert!(!v.is_empty() && v.len() <= 64);
        assert!(!v.chars().any(is_forbidden_display_char), "{v:?}");
    }

    #[test]
    fn ts_format() {
        let ts = now_ts();
        // 例如 2026-10-08T03:15:40.001Z：毫秒三位、以 Z 結尾
        assert_eq!(ts.len(), 24, "{ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[19..20], ".");
    }
}
