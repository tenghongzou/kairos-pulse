//! Pulse ↔ Kairos 協定訊息（共同規格 v1、v2）：Pulse 端的輔助函式。
//!
//! 訊息型別、序列化與欄位驗證在兩個產品共用的 `kairos-proto` crate（規格見
//! `crates/kairos-proto/PROTOCOL.md`），這裡重新匯出 Pulse 用到的部分，呼叫端仍以
//! `crate::protocol::…` 取用。傳輸為 NDJSON：一行一則 JSON 物件，欄位名稱與 `type` 值為定案；
//! 解析時忽略未知欄位。

use chrono::{DateTime, SecondsFormat, Utc};

pub use kairos_proto::{
    ClientMsg, DurableSeq, FEATURE_DURABLE, FEATURE_INVENTORY, FEATURE_METRICS, FEATURE_PROCESSES,
    HelloExt, HostInfo, MAX_LINE_LENGTH, METRIC_CPU_USAGE, METRIC_MEMORY_TOTAL, METRIC_MEMORY_USED,
    Metric, ProcessList, Proto, ProtoOffer, ReportBody, SUPPORTED_PROTO, ServerMsg, ServerMsgError,
    Session, is_forbidden_display_char, parse_server_msg,
};

use crate::system_info::Sample;

/// Pulse 支援的功能。`durable` 只在啟用本機緩衝時、`inventory`／`processes` 只在設定啟用時於 hello
/// 宣告（見 [`hello_features`]）
pub const CLIENT_FEATURES: &[&str] = &[
    FEATURE_METRICS,
    FEATURE_DURABLE,
    FEATURE_INVENTORY,
    FEATURE_PROCESSES,
];

/// hello 宣告哪些可選的功能（`metrics` 一律宣告）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionalFeatures {
    /// 啟用本機緩衝（`buffer.enabled` 且緩衝開啟成功）
    pub durable: bool,
    /// `inventory.enabled`
    pub inventory: bool,
    /// `processes.enabled`
    pub processes: bool,
}

impl OptionalFeatures {
    /// 全部宣告（預設設定、緩衝開啟成功）
    #[cfg(test)]
    pub const ALL: Self = Self {
        durable: true,
        inventory: true,
        processes: true,
    };

    /// 一次性的連線（`pulse enroll` 的確認、輪替後的確認）：不送 report 與資產，不宣告 inventory、processes
    pub fn one_shot(durable: bool) -> Self {
        Self {
            durable,
            inventory: false,
            processes: false,
        }
    }
}

/// hello 宣告的功能：停用本機緩衝（`buffer.enabled = false`）時不宣告 `durable`；
/// `inventory.enabled`／`processes.enabled = false` 時不宣告對應的 feature（METRICS.md 第 7.1 節）
pub fn hello_features(optional: OptionalFeatures) -> Vec<String> {
    CLIENT_FEATURES
        .iter()
        .filter(|f| match **f {
            FEATURE_DURABLE => optional.durable,
            FEATURE_INVENTORY => optional.inventory,
            FEATURE_PROCESSES => optional.processes,
            _ => true,
        })
        .map(|f| f.to_string())
        .collect()
}

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

/// 組出的 report 訊息
#[derive(Debug, Clone)]
pub struct BuiltReport {
    pub msg: ClientMsg,
    /// debug 日誌用的摘要
    pub summary: String,
    /// 超過單行上限而捨去的內容（METRICS.md 第 4.4 節；沒有捨去時 `None`）。呼叫端以節流的 WARN 記錄
    pub trimmed: Option<String>,
}

/// 截斷時指標所屬的「組」（METRICS.md 第 4.4 節：以整組捨去，不留下不完整的組）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrimClass {
    /// 沒有 label 的指標：一律保留
    Keep,
    Filesystem,
    Disk,
    Network,
    /// 溫度（以及其他帶 label 的指標）
    Temperature,
}

fn trim_class(m: &Metric) -> (TrimClass, String) {
    if m.labels.is_empty() {
        return (TrimClass::Keep, String::new());
    }
    let label = |k: &str| m.labels.get(k).cloned().unwrap_or_default();
    if m.name.starts_with("system.filesystem.") {
        (TrimClass::Filesystem, label("mountpoint"))
    } else if m.name.starts_with("system.disk.") {
        (TrimClass::Disk, label("device"))
    } else if m.name.starts_with("system.network.") {
        (TrimClass::Network, label("device"))
    } else {
        (
            TrimClass::Temperature,
            serde_json::to_string(&m.labels).unwrap_or_default(),
        )
    }
}

/// 依協商結果組出 report 訊息。
///
/// - 沒有協商 `metrics`（v1 Kairos、或 v2 但未宣告）：`cpu_usage`／`total_memory`／`used_memory`
/// - 協商了 `metrics`：CPU、記憶體總量／已使用加上 `sample.extra`（F4 的標準指標）；協商了 `processes`
///   時附上行程排行（這筆取樣有的話）
/// - 整行超過單行上限（或指標數超過 256）時依 METRICS.md 第 4.4 節的順序捨去，直到不超過：
///   行程的命令列 → 行程清單縮成各 3 項 → 整個行程排行 → 溫度 → 優先順序最低的介面、磁碟 I/O 裝置、
///   掛載點（以整組捨去）。沒有 label 的指標一律保留
/// - `durable`：協商了 durable 時附上緩衝記錄的 `boot_id`／`report_seq`（A5）。同一筆取樣在同一種
///   格式下組出的內容固定，重送時與第一次送出的相同
pub fn report_message(
    seq: u64,
    sample: Sample,
    session: Session,
    durable: Option<DurableSeq>,
) -> BuiltReport {
    let with_durable = |body: ReportBody| match &durable {
        Some(d) => body.with_durable(d.clone()),
        None => body,
    };
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
            body: with_durable(ReportBody::legacy(
                cpu_usage,
                sample.total_memory,
                sample.used_memory,
            )),
        };
        return BuiltReport {
            msg,
            summary,
            trimmed: None,
        };
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
    let extra = sample.extra;
    // 行程排行：只在協商了 processes 時附上；送出前以與 Kairos 相同的規則檢查（正常不會不符）
    let mut processes = sample.processes.filter(|_| session.processes).filter(|p| {
        let ok = kairos_proto::validate_process_list(p);
        if let Err(e) = &ok {
            log::debug!("行程排行不符合協定限制，這筆不附上：{e}");
        }
        ok.is_ok()
    });
    let classes: Vec<(TrimClass, String)> = extra.iter().map(trim_class).collect();
    let mut kept = vec![true; extra.len()];
    let build = |kept: &[bool], processes: &Option<ProcessList>| {
        let metrics: Vec<Metric> = core
            .iter()
            .cloned()
            .chain(
                extra
                    .iter()
                    .zip(kept)
                    .filter(|(_, k)| **k)
                    .map(|(m, _)| m.clone()),
            )
            .collect();
        let count = metrics.len();
        let mut body = ReportBody::metrics(metrics);
        if let Some(p) = processes {
            body = body.with_processes(p.clone());
        }
        let msg = ClientMsg::Report {
            seq,
            ts: sample.ts.clone(),
            body: with_durable(body),
        };
        let fits = count <= kairos_proto::MAX_METRICS && serialized_len(&msg) <= MAX_LINE_LENGTH;
        (msg, fits)
    };
    let mut dropped: Vec<String> = Vec::new();
    let (mut msg, mut fits) = build(&kept, &processes);

    // 1. 行程的命令列
    if !fits
        && let Some(p) = &mut processes
        && p.top_cpu
            .iter()
            .chain(&p.top_memory)
            .any(|e| e.command.is_some())
    {
        for e in p.top_cpu.iter_mut().chain(p.top_memory.iter_mut()) {
            e.command = None;
        }
        dropped.push("行程命令列".to_string());
        (msg, fits) = build(&kept, &processes);
    }
    // 2. 行程清單縮成各 3 項，再捨去整個行程排行
    if !fits && let Some(p) = &mut processes {
        p.top_cpu.truncate(3);
        p.top_memory.truncate(3);
        (msg, fits) = build(&kept, &processes);
        if fits {
            dropped.push("行程排行縮成各 3 項".to_string());
        } else {
            processes = None;
            dropped.push("行程排行".to_string());
            (msg, fits) = build(&kept, &processes);
        }
    }
    // 3. 溫度 → 4. 介面、磁碟 I/O 裝置、掛載點：從優先順序最低（最後面）的組開始整組捨去
    for (class, what) in [
        (TrimClass::Temperature, "溫度感測器"),
        (TrimClass::Network, "網路介面"),
        (TrimClass::Disk, "磁碟 I/O 裝置"),
        (TrimClass::Filesystem, "掛載點"),
    ] {
        let mut groups = 0usize;
        while !fits {
            let Some(last) = (0..extra.len())
                .rev()
                .find(|&i| kept[i] && classes[i].0 == class)
            else {
                break;
            };
            let key = classes[last].1.clone();
            for (i, (c, k)) in classes.iter().enumerate() {
                if *c == class && *k == key {
                    kept[i] = false;
                }
            }
            groups += 1;
            (msg, fits) = build(&kept, &processes);
        }
        if groups > 0 {
            dropped.push(format!("{groups} 個{what}"));
        }
    }

    let sent_metrics = core.len() + kept.iter().filter(|k| **k).count();
    let mut summary = format!(
        "CPU {cpu_usage:.2}%，記憶體 {}/{} MB，共 {sent_metrics} 個指標",
        sample.used_memory, sample.total_memory
    );
    if let Some(p) = &processes {
        summary.push_str(&format!(
            "，行程排行 {}＋{} 項",
            p.top_cpu.len(),
            p.top_memory.len()
        ));
    }
    let trimmed = (!dropped.is_empty()).then(|| dropped.join("、"));
    if let Some(t) = &trimmed {
        summary.push_str(&format!("；超過單行上限，捨去{t}"));
    }
    BuiltReport {
        msg,
        summary,
        trimmed,
    }
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
            processes: None,
        }
    }

    fn to_value(msg: &ClientMsg) -> serde_json::Value {
        serde_json::to_value(msg).unwrap()
    }

    #[test]
    fn v1_session_sends_the_three_fields() {
        let fs = Metric::new("system.filesystem.total", 1, "By").with_label("mountpoint", "/");
        let BuiltReport { msg, summary, .. } =
            report_message(3, sample(vec![fs]), Session::V1, None);
        assert_eq!(
            to_value(&msg),
            serde_json::json!({"type":"report","seq":3,"ts":"2026-10-09T00:00:00.000Z",
                "cpu_usage":7.5,"total_memory":49152,"used_memory":37034})
        );
        assert_eq!(summary, "CPU 7.50%，記憶體 37034/49152 MB");
        // v2 但沒有協商 metrics：同 v1
        let BuiltReport { msg, .. } = report_message(
            3,
            sample(vec![]),
            Session {
                proto: 2,
                metrics: false,
                durable: false,
                inventory: false,
                processes: false,
            },
            None,
        );
        assert!(to_value(&msg).get("metrics").is_none());
    }

    #[test]
    fn durable_report_carries_boot_id_and_report_seq_and_is_reproducible() {
        // A5：重送時 boot_id、report_seq、ts 與內容都不變（只有 seq 不同）
        let durable = DurableSeq {
            boot_id: "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e".into(),
            report_seq: 42,
        };
        let session = Session {
            proto: 2,
            metrics: true,
            durable: true,
            inventory: false,
            processes: false,
        };
        let fs = Metric::new("system.temperature", 40, "Cel");
        let first = report_message(3, sample(vec![fs.clone()]), session, Some(durable.clone()));
        let again = report_message(9, sample(vec![fs]), session, Some(durable.clone()));
        let mut a = to_value(&first.msg);
        let mut b = to_value(&again.msg);
        assert_eq!(
            (&a["boot_id"], &a["report_seq"]),
            (&serde_json::json!(durable.boot_id), &serde_json::json!(42))
        );
        a.as_object_mut().unwrap().remove("seq");
        b.as_object_mut().unwrap().remove("seq");
        assert_eq!(a, b);
        // v1 格式也可以帶（durable 與 metrics 各自協商）
        let BuiltReport { msg, .. } = report_message(1, sample(vec![]), Session::V1, Some(durable));
        let v = to_value(&msg);
        assert!(v.get("cpu_usage").is_some() && v["report_seq"] == 42, "{v}");
        // 停用本機緩衝時不宣告 durable
        assert_eq!(
            hello_features(OptionalFeatures::ALL),
            vec!["metrics", "durable", "inventory", "processes"]
        );
        assert_eq!(
            hello_features(OptionalFeatures {
                durable: false,
                ..OptionalFeatures::ALL
            }),
            vec!["metrics", "inventory", "processes"]
        );
        // METRICS.md 第 7.1 節：inventory.enabled／processes.enabled = false 時不宣告
        assert_eq!(
            hello_features(OptionalFeatures {
                durable: true,
                inventory: false,
                processes: false,
            }),
            vec!["metrics", "durable"]
        );
    }

    #[test]
    fn metrics_session_sends_standard_and_extra_metrics() {
        let fs = Metric::new("system.filesystem.total", 1000, "By").with_label("mountpoint", "/");
        let session = Session {
            proto: 2,
            metrics: true,
            durable: false,
            inventory: false,
            processes: false,
        };
        let BuiltReport { msg, summary, .. } = report_message(4, sample(vec![fs]), session, None);
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
            durable: false,
            inventory: false,
            processes: false,
        };
        let BuiltReport { msg, summary, .. } = report_message(5, sample(extra), session, None);
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
            durable: false,
            inventory: false,
            processes: false,
        };
        let BuiltReport { msg, .. } = report_message(1, s, session, None);
        let ClientMsg::Report { body, .. } = &msg else {
            unreachable!()
        };
        kairos_proto::validate_metrics(body.metric_list().unwrap()).unwrap();
    }

    const FULL: Session = Session {
        proto: 2,
        metrics: true,
        durable: true,
        inventory: true,
        processes: true,
    };

    /// METRICS.md 第 16.1 節驗收的最壞情況：16 個掛載點、8 個裝置、16 個介面、32 個感測器，
    /// label 值都是 256 bytes，`processes.command = "full"`（每個命令列 1024 bytes）
    fn worst_case_sample() -> Sample {
        let long = |p: &str, i: usize| format!("{p}{i:02}{}", "x".repeat(300))[..256].to_string();
        let mut extra = vec![
            Metric::new("system.cpu.load_1m", 1, "1"),
            Metric::new("system.cpu.load_5m", 1, "1"),
            Metric::new("system.cpu.load_15m", 1, "1"),
            Metric::new("system.memory.available", 1, "By"),
            Metric::new("system.swap.total", 1, "By"),
            Metric::new("system.swap.used", 1, "By"),
            Metric::new("system.process.count", 1, "{processes}"),
            Metric::new("system.uptime", 1, "s"),
        ];
        for i in 0..16 {
            for name in ["total", "available", "used", "usage"] {
                extra.push(
                    Metric::new(
                        &format!("system.filesystem.{name}"),
                        1,
                        if name == "usage" { "%" } else { "By" },
                    )
                    .with_label("mountpoint", long("/m", i))
                    .with_label("fstype", long("fs", i)),
                );
            }
        }
        for i in 0..8 {
            for (name, unit) in [
                ("read", "By"),
                ("written", "By"),
                ("read_ops", "{operations}"),
                ("write_ops", "{operations}"),
                ("io_time", "ms"),
            ] {
                extra.push(
                    Metric::new(&format!("system.disk.{name}"), 1, unit)
                        .with_label("device", long("d", i)),
                );
            }
        }
        for i in 0..16 {
            for (name, unit) in [
                ("received", "By"),
                ("transmitted", "By"),
                ("packets_received", "{packets}"),
                ("packets_transmitted", "{packets}"),
                ("errors_received", "{errors}"),
                ("errors_transmitted", "{errors}"),
            ] {
                extra.push(
                    Metric::new(&format!("system.network.{name}"), 1, unit)
                        .with_label("device", long("n", i)),
                );
            }
        }
        for i in 0..32 {
            extra.push(
                Metric::new("system.temperature", 40, "Cel").with_label("sensor", long("t", i)),
            );
        }
        let entry = |pid: u32| kairos_proto::ProcessEntry {
            pid,
            name: "n".repeat(256),
            cpu_usage: 1.0,
            memory: 1,
            user: None,
            command: Some("c".repeat(kairos_proto::MAX_PROCESS_COMMAND_BYTES)),
        };
        Sample {
            extra,
            processes: Some(ProcessList {
                top_cpu: (1..=10).map(entry).collect(),
                top_memory: (11..=20).map(entry).collect(),
            }),
            ..sample(vec![])
        }
    }

    /// report 中各類「組」的 label（依出現順序、不重複）
    fn groups(v: &serde_json::Value, prefix: &str, label: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for m in v["metrics"].as_array().unwrap() {
            if m["name"].as_str().unwrap().starts_with(prefix) {
                let l = m["labels"][label].as_str().unwrap().to_string();
                if !out.contains(&l) {
                    out.push(l);
                }
            }
        }
        out
    }

    #[test]
    fn worst_case_report_is_trimmed_by_whole_groups_in_the_spec_order() {
        let s = worst_case_sample();
        assert_eq!(s.extra.len() + 3, 243);
        let durable = DurableSeq {
            boot_id: "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e".into(),
            report_seq: u64::MAX,
        };
        let built = report_message(u64::MAX, s, FULL, Some(durable));
        let line = serde_json::to_string(&built.msg).unwrap();
        assert!(line.len() <= MAX_LINE_LENGTH, "{}", line.len());
        let ClientMsg::Report { body, .. } = &built.msg else {
            unreachable!()
        };
        let metrics = body.metric_list().unwrap();
        assert!(metrics.len() <= kairos_proto::MAX_METRICS);
        kairos_proto::validate_metrics(metrics).unwrap();
        // 沒有 label 的 11 個一律保留
        assert_eq!(metrics.iter().filter(|m| m.labels.is_empty()).count(), 11);
        // 依序捨去：命令列 → 行程排行 → 溫度 → 介面 → …（這個大小下掛載點與磁碟都保留）
        let v = to_value(&built.msg);
        assert!(v.get("processes").is_none(), "{}", built.summary);
        assert!(groups(&v, "system.temperature", "sensor").is_empty());
        assert_eq!(groups(&v, "system.filesystem.", "mountpoint").len(), 16);
        assert_eq!(groups(&v, "system.disk.", "device").len(), 8);
        let nics = groups(&v, "system.network.", "device");
        assert!(nics.len() < 16, "{}", nics.len());
        // 留下的是優先順序高（前面）的介面，而且每組都完整
        for (i, n) in nics.iter().enumerate() {
            assert!(n.starts_with(&format!("n{i:02}")), "{n}");
            let count = metrics
                .iter()
                .filter(|m| m.labels.get("device") == Some(n))
                .count();
            assert_eq!(count, 6, "{n}");
        }
        let trimmed = built.trimmed.unwrap();
        assert!(
            trimmed.starts_with("行程命令列、行程排行、32 個溫度感測器、"),
            "{trimmed}"
        );
        assert!(trimmed.contains("個網路介面"), "{trimmed}");
        assert!(built.summary.contains("超過單行上限"), "{}", built.summary);
    }

    #[test]
    fn process_commands_are_dropped_first() {
        // 只有行程的命令列讓整行超過上限：只捨去命令列，指標與排行都保留
        let mut s = worst_case_sample();
        s.extra.retain(|m| m.labels.is_empty());
        let mut big = s.processes.clone().unwrap();
        for e in big.top_cpu.iter_mut().chain(big.top_memory.iter_mut()) {
            e.command = Some("c".repeat(kairos_proto::MAX_PROCESS_COMMAND_BYTES));
        }
        // 20 個 1024 bytes 的命令列約 20 KB，不會超過；把指標加大到接近上限
        s.extra.extend((0..150).map(|i| {
            Metric::new("app.queue.depth", 1, "1")
                .with_label("queue", format!("{i:03}{}", "q".repeat(253)))
        }));
        s.processes = Some(big);
        let built = report_message(1, s.clone(), FULL, None);
        let v = to_value(&built.msg);
        assert!(serde_json::to_string(&built.msg).unwrap().len() <= MAX_LINE_LENGTH);
        assert_eq!(
            built.trimmed.as_deref(),
            Some("行程命令列"),
            "{}",
            built.summary
        );
        let p = &v["processes"];
        assert_eq!(p["top_cpu"].as_array().unwrap().len(), 10);
        assert!(p["top_cpu"][0].get("command").is_none());
        assert_eq!(v["metrics"].as_array().unwrap().len(), 3 + 8 + 150);

        // 沒有協商 processes（舊版 Kairos）時不附上行程排行，也不需要截斷
        let built = report_message(
            1,
            Sample {
                extra: vec![],
                ..s.clone()
            },
            Session {
                processes: false,
                ..FULL
            },
            None,
        );
        assert!(to_value(&built.msg).get("processes").is_none());
        assert_eq!(built.trimmed, None);
        // v1 一律不帶
        let built = report_message(1, s, Session::V1, None);
        assert!(to_value(&built.msg).get("processes").is_none());
    }

    #[test]
    fn processes_ride_on_the_report_and_survive_a_replay() {
        // durable：含 processes 的緩衝記錄，補送時與第一次送出的內容相同（只有 seq 不同）
        let mut s = sample(vec![Metric::new("system.uptime", 10, "s")]);
        s.processes = Some(ProcessList {
            top_cpu: vec![kairos_proto::ProcessEntry {
                pid: 2211,
                name: "postgres".into(),
                cpu_usage: 9.41,
                memory: 4_294_967_296,
                user: None,
                command: None,
            }],
            top_memory: vec![],
        });
        let durable = DurableSeq {
            boot_id: "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e".into(),
            report_seq: 7,
        };
        // 緩衝的 JSON 往返
        let stored: Sample = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(stored, s);
        let live = report_message(3, s, FULL, Some(durable.clone()));
        let replay = report_message(40, stored, FULL, Some(durable));
        let mut a = to_value(&live.msg);
        let mut b = to_value(&replay.msg);
        a.as_object_mut().unwrap().remove("seq");
        b.as_object_mut().unwrap().remove("seq");
        assert_eq!(a, b);
        assert_eq!(a["processes"]["top_cpu"][0]["name"], "postgres");
        assert!(
            live.summary.contains("行程排行 1＋0 項"),
            "{}",
            live.summary
        );
        // 送出的內容經 kairos-proto 解析後通過伺服器的檢查
        let line = serde_json::to_string(&live.msg).unwrap();
        let parsed = kairos_proto::parse_client_msg(&line).unwrap();
        kairos_proto::validate_client_msg(&parsed).unwrap();
        let ClientMsg::Report { body, .. } = parsed else {
            unreachable!()
        };
        assert_eq!(body.process_list().unwrap().unwrap().top_cpu[0].pid, 2211);
    }

    #[test]
    fn samples_written_by_0_7_are_still_readable() {
        // 0.7 的緩衝記錄沒有 processes 欄位
        let old = r#"{"ts":"2026-10-09T00:00:00.000Z","cpu_usage":7.5,"total_memory":49152,
            "used_memory":37034,"total_memory_bytes":51539607552,"used_memory_bytes":38833356800,
            "extra":[{"name":"system.filesystem.total","value":1000,"unit":"By","labels":{"mountpoint":"/"}}]}"#;
        let s: Sample = serde_json::from_str(old).unwrap();
        assert_eq!(s.processes, None);
        assert_eq!(s.extra.len(), 1);
        // 沒有行程時序列化不輸出 processes（與 0.7 寫出的格式相同）
        assert!(!serde_json::to_string(&s).unwrap().contains("processes"));
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
