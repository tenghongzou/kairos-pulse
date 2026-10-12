//! Pulse ↔ Kairos 協定訊息（共同規格 v1、v2）：Pulse 端的輔助函式。
//!
//! 訊息型別、序列化與欄位驗證在兩個產品共用的 `kairos-proto` crate（規格見
//! `crates/kairos-proto/PROTOCOL.md`），這裡重新匯出 Pulse 用到的部分，呼叫端仍以
//! `crate::protocol::…` 取用。傳輸為 NDJSON：一行一則 JSON 物件，欄位名稱與 `type` 值為定案；
//! 解析時忽略未知欄位。

use chrono::{DateTime, SecondsFormat, Utc};

pub use kairos_proto::{
    ClientMsg, FEATURE_METRICS, HelloExt, HostInfo, MAX_LINE_LENGTH, METRIC_CPU_USAGE,
    METRIC_MEMORY_TOTAL, METRIC_MEMORY_USED, Metric, Proto, ProtoOffer, ReportBody,
    SUPPORTED_PROTO, ServerMsg, ServerMsgError, Session, is_forbidden_display_char,
    parse_server_msg,
};

use crate::system_info::Sample;

/// Pulse 在 v2 hello 宣告的功能（與 Kairos 相同的一組）
pub const CLIENT_FEATURES: &[&str] = &[FEATURE_METRICS];

/// 本機時鐘與伺服器相差超過此值時記 WARN（設計包 PF03 的時鐘檢查門檻）
pub const MAX_CLOCK_SKEW: chrono::TimeDelta = chrono::TimeDelta::minutes(5);

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

/// hello_ack 的 `server_time` 與本機時間相差超過 [`MAX_CLOCK_SKEW`] 時的警告文字。
/// 無法解析時不警告（`server_time` 只供參考，不影響連線）。
pub fn clock_skew_warning(server_time: &str, now: DateTime<Utc>) -> Option<String> {
    let server = DateTime::parse_from_rfc3339(server_time)
        .ok()?
        .with_timezone(&Utc);
    let skew = now - server;
    if skew.abs() <= MAX_CLOCK_SKEW {
        return None;
    }
    let (direction, secs) = if skew > chrono::TimeDelta::zero() {
        ("快", skew.num_seconds())
    } else {
        ("慢", -skew.num_seconds())
    };
    Some(format!(
        "本機時鐘比伺服器{direction}約 {secs} 秒（伺服器時間 {}）：report 的 ts 與日誌時間會不準確，\
         日後的憑證自動註冊也會因時鐘偏差被拒，請檢查本機的時間同步（NTP）",
        server.to_rfc3339_opts(SecondsFormat::Millis, true)
    ))
}

/// 依協商結果組出 report 訊息，並回傳 debug 日誌用的摘要。
///
/// - 沒有協商 `metrics`（v1 Kairos、或 v2 但未宣告）：`cpu_usage`／`total_memory`／`used_memory`
/// - 協商了 `metrics`：CPU、記憶體三個標準指標加上磁碟、網路、溫度（A4）。整行超過單行上限時，
///   捨去後面的磁碟、網路、溫度指標（CPU 與記憶體一定保留）
pub fn report_message(seq: u64, sample: Sample, session: Session) -> (ClientMsg, String) {
    // 四捨五入到小數 2 位（f64），JSON 中不會出現 f32 的精度雜訊
    let cpu_usage = report_cpu_usage(sample.cpu_usage);
    if !session.metrics {
        let summary = format!(
            "CPU {cpu_usage:.2}%，記憶體 {}/{} MB",
            sample.used_memory, sample.total_memory
        );
        let msg = ClientMsg::Report {
            seq,
            ts: sample.ts,
            body: ReportBody::legacy(cpu_usage, sample.total_memory, sample.used_memory),
        };
        return (msg, summary);
    }
    let core = [
        Metric::new(
            METRIC_CPU_USAGE,
            serde_json::Number::from_f64(cpu_usage).unwrap_or_else(|| 0.into()),
            "%",
        ),
        Metric::new(METRIC_MEMORY_TOTAL, sample.total_memory_bytes, "By"),
        // used 不得大於 total（Kairos 會回 invalid_report）
        Metric::new(
            METRIC_MEMORY_USED,
            sample.used_memory_bytes.min(sample.total_memory_bytes),
            "By",
        ),
    ];
    let mut extra = sample.extra;
    let total_extra = extra.len();
    let build = |extra: &[Metric]| ClientMsg::Report {
        seq,
        ts: sample.ts.clone(),
        body: ReportBody::metrics(core.iter().chain(extra).cloned().collect()),
    };
    let mut msg = build(&extra);
    while !extra.is_empty() && serialized_len(&msg) > MAX_LINE_LENGTH {
        extra.truncate(extra.len() / 2);
        msg = build(&extra);
    }
    let mut summary = format!(
        "CPU {cpu_usage:.2}%，記憶體 {}/{} MB，共 {} 個指標",
        sample.used_memory,
        sample.total_memory,
        core.len() + extra.len()
    );
    if extra.len() < total_extra {
        summary.push_str(&format!(
            "；超過單行上限，捨去 {} 個磁碟／網路／溫度指標",
            total_extra - extra.len()
        ));
    }
    (msg, summary)
}

/// 序列化後的長度（無法序列化時視為超長）
fn serialized_len(msg: &ClientMsg) -> usize {
    serde_json::to_string(msg).map_or(usize::MAX, |l| l.len())
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

    fn sample(extra: Vec<Metric>) -> Sample {
        Sample {
            ts: "2026-10-09T00:00:00.000Z".into(),
            cpu_usage: 7.500_000_5,
            total_memory: 49152,
            used_memory: 37034,
            total_memory_bytes: 51_539_607_552,
            used_memory_bytes: 38_833_356_800,
            extra,
        }
    }

    fn to_value(msg: &ClientMsg) -> serde_json::Value {
        serde_json::to_value(msg).unwrap()
    }

    #[test]
    fn v1_session_sends_the_three_fields() {
        let fs = Metric::new("system.filesystem.total", 1, "By").with_label("mountpoint", "/");
        let (msg, summary) = report_message(3, sample(vec![fs]), Session::V1);
        assert_eq!(
            to_value(&msg),
            serde_json::json!({"type":"report","seq":3,"ts":"2026-10-09T00:00:00.000Z",
                "cpu_usage":7.5,"total_memory":49152,"used_memory":37034})
        );
        assert_eq!(summary, "CPU 7.50%，記憶體 37034/49152 MB");
        // v2 但沒有協商 metrics：同 v1
        let (msg, _) = report_message(
            3,
            sample(vec![]),
            Session {
                proto: 2,
                metrics: false,
            },
        );
        assert!(to_value(&msg).get("metrics").is_none());
    }

    #[test]
    fn metrics_session_sends_standard_and_extra_metrics() {
        let fs = Metric::new("system.filesystem.total", 1000, "By").with_label("mountpoint", "/");
        let session = Session {
            proto: 2,
            metrics: true,
        };
        let (msg, summary) = report_message(4, sample(vec![fs]), session);
        let v = to_value(&msg);
        assert!(v.get("cpu_usage").is_none(), "{v}");
        assert_eq!(
            v["metrics"],
            serde_json::json!([
                {"name":"system.cpu.usage","value":7.5,"unit":"%"},
                {"name":"system.memory.total","value":51_539_607_552_u64,"unit":"By"},
                {"name":"system.memory.used","value":38_833_356_800_u64,"unit":"By"},
                {"name":"system.filesystem.total","value":1000,"unit":"By","labels":{"mountpoint":"/"}}
            ])
        );
        assert!(summary.ends_with("共 4 個指標"), "{summary}");
        let ClientMsg::Report { body, .. } = &msg else {
            unreachable!()
        };
        kairos_proto::validate_metrics(body.metric_list().unwrap()).unwrap();
    }

    #[test]
    fn oversized_metrics_are_trimmed_to_the_line_limit() {
        // 256 bytes 的 label × 250 個 ≈ 80 KB > 65536：捨去後面的指標，CPU 與記憶體一定保留
        let extra: Vec<Metric> = (0..250)
            .map(|i| {
                Metric::new("system.temperature", i, "Cel")
                    .with_label("sensor", format!("{i:03}{}", "x".repeat(250)))
            })
            .collect();
        let session = Session {
            proto: 2,
            metrics: true,
        };
        let (msg, summary) = report_message(5, sample(extra), session);
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.len() <= MAX_LINE_LENGTH, "{}", line.len());
        let v = to_value(&msg);
        let metrics = v["metrics"].as_array().unwrap();
        assert!(
            metrics.len() > 3 && metrics.len() < 253,
            "{}",
            metrics.len()
        );
        assert_eq!(metrics[0]["name"], "system.cpu.usage");
        assert!(summary.contains("超過單行上限"), "{summary}");
    }

    #[test]
    fn used_memory_metric_never_exceeds_total() {
        let mut s = sample(vec![]);
        s.used_memory_bytes = s.total_memory_bytes + 1;
        let session = Session {
            proto: 2,
            metrics: true,
        };
        let (msg, _) = report_message(1, s, session);
        let ClientMsg::Report { body, .. } = &msg else {
            unreachable!()
        };
        kairos_proto::validate_metrics(body.metric_list().unwrap()).unwrap();
    }

    #[test]
    fn clock_skew_is_reported_beyond_five_minutes() {
        let now = DateTime::parse_from_rfc3339("2026-10-08T03:15:40.000Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(clock_skew_warning("2026-10-08T03:15:40.012Z", now), None);
        assert_eq!(clock_skew_warning("2026-10-08T03:20:40.000Z", now), None);
        assert_eq!(clock_skew_warning("2026-10-08T03:10:40.000Z", now), None);
        let slow = clock_skew_warning("2026-10-08T03:25:40.000Z", now).unwrap();
        assert!(slow.starts_with("本機時鐘比伺服器慢約 600 秒"), "{slow}");
        let fast = clock_skew_warning("2026-10-08T03:05:39.000Z", now).unwrap();
        assert!(fast.starts_with("本機時鐘比伺服器快約 601 秒"), "{fast}");
        // 其他時區的表示也接受；無法解析時不警告
        assert_eq!(
            clock_skew_warning("2026-10-08T11:15:40.000+08:00", now),
            None
        );
        assert_eq!(clock_skew_warning("not a time", now), None);
    }
}
