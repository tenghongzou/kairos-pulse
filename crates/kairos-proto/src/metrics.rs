//! v2 的通用指標格式（商業審計 A4）：report 的 `metrics: [{name, value, unit, labels}]`。
//!
//! 伺服器只檢查格式與上限（名稱、單位、label 的字元與長度、數量、重複），以及
//! [`STANDARD_METRICS`] 列出的標準指標的單位與數值範圍；其他指標原樣保存，不解讀其意義。
//!
//! 標準指標的完整清單、語意、各平台來源與計數器的處理規則見 `METRICS.md`（商業審計 F4）。
//! F4 新增的指標沿用同一個 `metrics` 陣列，不需要新的 feature：0.2.0～0.7.0 的 Kairos 只檢查
//! 名稱與單位的格式，原樣保存它們不認得的 `system.*` 指標。

use serde::{Deserialize, Serialize};
use serde_json::Number;
use std::collections::{BTreeMap, HashSet};

use crate::validate::validate_display_text;
use crate::{
    MAX_LABEL_KEY_BYTES, MAX_LABEL_VALUE_BYTES, MAX_METRIC_LABELS, MAX_METRIC_NAME_BYTES,
    MAX_METRIC_UNIT_BYTES, MAX_METRICS,
};

/// 一個指標。`value` 保留收到的 JSON 數字表示（整數不會變成 `1.0`），伺服器原樣寫入 JSONL。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// 小寫、以 `.` 分段的名稱，例如 `system.cpu.usage`（見 [`is_valid_metric_name`]）
    pub name: String,
    /// JSON 數字（JSON 沒有 NaN／∞，因此一定是有限值）
    pub value: Number,
    /// 單位，例如 `%`、`By`、`Cel`（見 [`is_valid_unit`]）
    pub unit: String,
    /// 維度，例如 `{"mountpoint": "/"}`；可省略（省略時與 `{}` 相同，序列化時不輸出）
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl Metric {
    /// 沒有 label 的指標
    pub fn new(name: &str, value: impl Into<Number>, unit: &str) -> Self {
        Self {
            name: name.to_string(),
            value: value.into(),
            unit: unit.to_string(),
            labels: BTreeMap::new(),
        }
    }

    /// 加上一個 label
    pub fn with_label(mut self, key: &str, value: impl Into<String>) -> Self {
        self.labels.insert(key.to_string(), value.into());
        self
    }

    /// 以 f64 表示的值（超過 2^53 的整數會失去精度，只用於範圍檢查與顯示）
    pub fn value_f64(&self) -> f64 {
        // serde_json 的 Number（未啟用 arbitrary_precision）一定能轉成 f64
        self.value.as_f64().unwrap_or(f64::NAN)
    }
}

/// 全機 CPU 平均使用率（%，0～100，沒有 label）
pub const METRIC_CPU_USAGE: &str = "system.cpu.usage";
/// 1 分鐘平均負載（沒有單位，≥ 0，沒有 label；只有 Linux）
pub const METRIC_CPU_LOAD_1M: &str = "system.cpu.load_1m";
/// 5 分鐘平均負載
pub const METRIC_CPU_LOAD_5M: &str = "system.cpu.load_5m";
/// 15 分鐘平均負載
pub const METRIC_CPU_LOAD_15M: &str = "system.cpu.load_15m";
/// 總記憶體（bytes，沒有 label）
pub const METRIC_MEMORY_TOTAL: &str = "system.memory.total";
/// 已使用記憶體（bytes，沒有 label；＝ total − available）
pub const METRIC_MEMORY_USED: &str = "system.memory.used";
/// 可用記憶體（bytes，沒有 label；Linux `MemAvailable`、Windows `ullAvailPhys`）
pub const METRIC_MEMORY_AVAILABLE: &str = "system.memory.available";
/// 置換空間（swap／分頁檔）總量（bytes，沒有 label）
pub const METRIC_SWAP_TOTAL: &str = "system.swap.total";
/// 置換空間已使用量（bytes，沒有 label）
pub const METRIC_SWAP_USED: &str = "system.swap.used";
/// 檔案系統總容量（bytes；label `mountpoint`、`fstype`）
pub const METRIC_FILESYSTEM_TOTAL: &str = "system.filesystem.total";
/// 檔案系統可用空間（bytes，一般使用者可寫入的部分；label `mountpoint`、`fstype`）
pub const METRIC_FILESYSTEM_AVAILABLE: &str = "system.filesystem.available";
/// 檔案系統已使用量（bytes，＝ total − free；label `mountpoint`、`fstype`）
pub const METRIC_FILESYSTEM_USED: &str = "system.filesystem.used";
/// 檔案系統使用率（%，0～100，與 `df` 相同：used ÷ (used + available)；見 [`filesystem_usage`]）
pub const METRIC_FILESYSTEM_USAGE: &str = "system.filesystem.usage";
/// 磁碟開機以來累計讀取量（bytes，計數器；label `device`）
pub const METRIC_DISK_READ: &str = "system.disk.read";
/// 磁碟開機以來累計寫入量（bytes，計數器；label `device`）
pub const METRIC_DISK_WRITTEN: &str = "system.disk.written";
/// 磁碟累計完成的讀取次數（計數器；label `device`；只有 Linux）
pub const METRIC_DISK_READ_OPS: &str = "system.disk.read_ops";
/// 磁碟累計完成的寫入次數（計數器；label `device`；只有 Linux）
pub const METRIC_DISK_WRITE_OPS: &str = "system.disk.write_ops";
/// 磁碟累計忙碌時間（毫秒，計數器；label `device`；只有 Linux）。速率 ÷ 10 即忙碌百分比
pub const METRIC_DISK_IO_TIME: &str = "system.disk.io_time";
/// 網路介面開機以來累計接收量（bytes，計數器；label `device`）
pub const METRIC_NETWORK_RECEIVED: &str = "system.network.received";
/// 網路介面開機以來累計傳送量（bytes，計數器；label `device`）
pub const METRIC_NETWORK_TRANSMITTED: &str = "system.network.transmitted";
/// 網路介面累計接收封包數（計數器；label `device`）
pub const METRIC_NETWORK_PACKETS_RECEIVED: &str = "system.network.packets_received";
/// 網路介面累計傳送封包數（計數器；label `device`）
pub const METRIC_NETWORK_PACKETS_TRANSMITTED: &str = "system.network.packets_transmitted";
/// 網路介面累計接收錯誤數（計數器；label `device`）
pub const METRIC_NETWORK_ERRORS_RECEIVED: &str = "system.network.errors_received";
/// 網路介面累計傳送錯誤數（計數器；label `device`）
pub const METRIC_NETWORK_ERRORS_TRANSMITTED: &str = "system.network.errors_transmitted";
/// 行程數（沒有 label）
pub const METRIC_PROCESS_COUNT: &str = "system.process.count";
/// 開機後經過的時間（秒，沒有 label）
pub const METRIC_UPTIME: &str = "system.uptime";
/// 元件溫度（攝氏；label `sensor`）
pub const METRIC_TEMPERATURE: &str = "system.temperature";

/// 指標的種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// 量測值：每次取樣當下的狀態（使用率、容量、溫度），可直接畫圖、平均
    Gauge,
    /// 計數器：自某個起點（通常是開機或介面啟用）以來單調遞增的累計值。使用端以兩次取樣相減
    /// 求速率（[`counter_rate`]），變小代表計數器重置
    Counter,
}

/// 標準指標：名稱、單位與數值範圍由規格固定，伺服器會檢查
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StandardMetric {
    pub name: &'static str,
    pub unit: &'static str,
    /// 下限（含）；`None` 為不限
    pub min: Option<f64>,
    /// 上限（含）；`None` 為不限
    pub max: Option<f64>,
    pub kind: MetricKind,
    /// label 名稱（固定的一組；空的表示沒有 label）。伺服器不檢查，只供文件與使用端參考
    pub labels: &'static [&'static str],
}

const FS_LABELS: &[&str] = &["mountpoint", "fstype"];
const DEVICE_LABEL: &[&str] = &["device"];

/// 量測值：`percent` 為 0～100，否則 ≥ 0（溫度另外處理）
const fn gauge(
    name: &'static str,
    unit: &'static str,
    percent: bool,
    labels: &'static [&'static str],
) -> StandardMetric {
    StandardMetric {
        name,
        unit,
        min: Some(0.0),
        max: if percent { Some(100.0) } else { None },
        kind: MetricKind::Gauge,
        labels,
    }
}

/// 計數器：≥ 0，不限上限
const fn counter(
    name: &'static str,
    unit: &'static str,
    labels: &'static [&'static str],
) -> StandardMetric {
    StandardMetric {
        name,
        unit,
        min: Some(0.0),
        max: None,
        kind: MetricKind::Counter,
        labels,
    }
}

/// METRICS.md「標準指標」表（Pulse 送出的指標）。前 8 個是 0.2.0 起就有的指標，其餘為 F4 新增
pub const STANDARD_METRICS: &[StandardMetric] = &[
    gauge(METRIC_CPU_USAGE, "%", true, &[]),
    gauge(METRIC_MEMORY_TOTAL, "By", false, &[]),
    gauge(METRIC_MEMORY_USED, "By", false, &[]),
    gauge(METRIC_FILESYSTEM_TOTAL, "By", false, FS_LABELS),
    gauge(METRIC_FILESYSTEM_AVAILABLE, "By", false, FS_LABELS),
    counter(METRIC_NETWORK_RECEIVED, "By", DEVICE_LABEL),
    counter(METRIC_NETWORK_TRANSMITTED, "By", DEVICE_LABEL),
    StandardMetric {
        name: METRIC_TEMPERATURE,
        unit: "Cel",
        min: None,
        max: None,
        kind: MetricKind::Gauge,
        labels: &["sensor"],
    },
    // ---- F4（0.8.0）----
    gauge(METRIC_CPU_LOAD_1M, "1", false, &[]),
    gauge(METRIC_CPU_LOAD_5M, "1", false, &[]),
    gauge(METRIC_CPU_LOAD_15M, "1", false, &[]),
    gauge(METRIC_MEMORY_AVAILABLE, "By", false, &[]),
    gauge(METRIC_SWAP_TOTAL, "By", false, &[]),
    gauge(METRIC_SWAP_USED, "By", false, &[]),
    gauge(METRIC_FILESYSTEM_USED, "By", false, FS_LABELS),
    gauge(METRIC_FILESYSTEM_USAGE, "%", true, FS_LABELS),
    counter(METRIC_DISK_READ, "By", DEVICE_LABEL),
    counter(METRIC_DISK_WRITTEN, "By", DEVICE_LABEL),
    counter(METRIC_DISK_READ_OPS, "{operations}", DEVICE_LABEL),
    counter(METRIC_DISK_WRITE_OPS, "{operations}", DEVICE_LABEL),
    counter(METRIC_DISK_IO_TIME, "ms", DEVICE_LABEL),
    counter(METRIC_NETWORK_PACKETS_RECEIVED, "{packets}", DEVICE_LABEL),
    counter(
        METRIC_NETWORK_PACKETS_TRANSMITTED,
        "{packets}",
        DEVICE_LABEL,
    ),
    counter(METRIC_NETWORK_ERRORS_RECEIVED, "{errors}", DEVICE_LABEL),
    counter(METRIC_NETWORK_ERRORS_TRANSMITTED, "{errors}", DEVICE_LABEL),
    gauge(METRIC_PROCESS_COUNT, "{processes}", false, &[]),
    gauge(METRIC_UPTIME, "s", false, &[]),
];

/// 0.2.0～0.7.0 的標準指標數（[`STANDARD_METRICS`] 的前幾項；舊版 Kairos 只認得這些，
/// 其他 `system.*` 名稱只檢查格式後原樣保存）
pub const LEGACY_STANDARD_METRICS: usize = 8;

/// 依名稱查標準指標
pub fn standard_metric(name: &str) -> Option<&'static StandardMetric> {
    STANDARD_METRICS.iter().find(|m| m.name == name)
}

/// 計數器的一次取樣（[`counter_rate`] 的輸入）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterSample {
    /// 計數器的值
    pub value: u64,
    /// 取樣時間（Unix 毫秒；report 的 `ts`）
    pub ts_ms: i64,
    /// 同一筆 report 的 `system.uptime`（秒）；沒有時 `None`
    pub uptime_secs: Option<u64>,
}

/// 兩次取樣之間的每秒速率。下列情況回 `None`（這一段沒有速率，圖上為缺口，**不補 0**）：
/// - `cur` 不晚於 `prev`（時間相同或倒退，例如補送的舊資料晚到）
/// - 值變小（計數器重置：重新開機、驅動程式重新載入、介面重建、32 位元計數器溢位）
/// - 兩筆都有 uptime 且 `cur` 的 uptime 較小（中間重新開機過；即使值碰巧沒有變小也不計算）
pub fn counter_rate(prev: CounterSample, cur: CounterSample) -> Option<f64> {
    if cur.ts_ms <= prev.ts_ms || cur.value < prev.value {
        return None;
    }
    if let (Some(p), Some(c)) = (prev.uptime_secs, cur.uptime_secs)
        && c < p
    {
        return None;
    }
    let secs = (cur.ts_ms - prev.ts_ms) as f64 / 1000.0;
    Some((cur.value - prev.value) as f64 / secs)
}

/// 檔案系統使用率（%）：`used ÷ (used + available) × 100`，與 `df` 的 Use% 相同（保留給 root 的
/// 空間不算可用，因此 used + available 可能小於 total），四捨五入到小數 2 位、夾在 0～100。
/// 兩者都是 0 時 `None`（不送 `system.filesystem.usage`）
pub fn filesystem_usage(used: u64, available: u64) -> Option<f64> {
    let denom = used as f64 + available as f64;
    if denom <= 0.0 {
        return None;
    }
    let v = (used as f64 / denom * 100.0).clamp(0.0, 100.0);
    Some((v * 100.0).round() / 100.0)
}

/// 指標名稱：1～[`MAX_METRIC_NAME_BYTES`] bytes，至少兩段、以 `.` 分隔，
/// 每段以小寫英文字母開頭，其後為小寫英文字母、數字或 `_`（例如 `system.cpu.usage`）。
pub fn is_valid_metric_name(name: &str) -> bool {
    name.len() <= MAX_METRIC_NAME_BYTES
        && name.split('.').count() >= 2
        && name.split('.').all(is_lower_ident)
}

/// 小寫英文字母開頭，其後為小寫英文字母、數字或 `_`
fn is_lower_ident(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// 單位：1～[`MAX_METRIC_UNIT_BYTES`] bytes，只能是英文字母、數字與 `% / . _ - { } ^ *`
/// （UCUM 風格，例如 `%`、`By`、`By/s`、`Cel`、`s`、`1`、`{packets}`）。
pub fn is_valid_unit(unit: &str) -> bool {
    !unit.is_empty()
        && unit.len() <= MAX_METRIC_UNIT_BYTES
        && unit.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'%' | b'/' | b'.' | b'_' | b'-' | b'{' | b'}' | b'^' | b'*'
                )
        })
}

/// label 名稱：1～[`MAX_LABEL_KEY_BYTES`] bytes，小寫英文字母或 `_` 開頭，其後為小寫英文字母、數字或 `_`。
pub fn is_valid_label_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    key.len() <= MAX_LABEL_KEY_BYTES
        && matches!(bytes.next(), Some(b'a'..=b'z' | b'_'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// 檢查單一指標（名稱、單位、label、標準指標的單位與範圍）
pub fn validate_metric(metric: &Metric) -> Result<(), String> {
    if !is_valid_metric_name(&metric.name) {
        return Err(format!(
            "metric name {:?} must be 1..={} bytes of dot-separated lowercase segments, e.g. system.cpu.usage",
            truncate_for_message(&metric.name),
            MAX_METRIC_NAME_BYTES
        ));
    }
    let name = &metric.name;
    if !is_valid_unit(&metric.unit) {
        return Err(format!(
            "metric {name}: unit must be 1..={MAX_METRIC_UNIT_BYTES} bytes of letters, digits or % / . _ - {{ }} ^ *"
        ));
    }
    if metric.labels.len() > MAX_METRIC_LABELS {
        return Err(format!(
            "metric {name}: at most {MAX_METRIC_LABELS} labels (got {})",
            metric.labels.len()
        ));
    }
    for (key, value) in &metric.labels {
        if !is_valid_label_key(key) {
            return Err(format!(
                "metric {name}: label name {:?} must be 1..={MAX_LABEL_KEY_BYTES} bytes of lowercase letters, digits or _",
                truncate_for_message(key)
            ));
        }
        validate_display_text(
            &format!("metric {name}: label {key}"),
            value,
            MAX_LABEL_VALUE_BYTES,
        )?;
    }
    if let Some(std) = standard_metric(name) {
        if metric.unit != std.unit {
            return Err(format!("metric {name}: unit must be {:?}", std.unit));
        }
        let v = metric.value_f64();
        if std.min.is_some_and(|min| v < min) || std.max.is_some_and(|max| v > max) {
            return Err(format!(
                "metric {name}: value {v} out of range {}..={}",
                std.min.map_or_else(String::new, |m| m.to_string()),
                std.max.map_or_else(String::new, |m| m.to_string())
            ));
        }
    }
    Ok(())
}

/// 檢查 v2 report 的 `metrics`（違反時回 `invalid_report`，這筆不保存）：
/// 1～[`MAX_METRICS`] 個、每個都通過 [`validate_metric`]、同一名稱與 label 組合不得重複，
/// 且同時有沒有 label 的 `system.memory.used` 與 `system.memory.total` 時前者不得大於後者。
pub fn validate_metrics(metrics: &[Metric]) -> Result<(), String> {
    if metrics.is_empty() || metrics.len() > MAX_METRICS {
        return Err(format!(
            "metrics must contain 1..={MAX_METRICS} entries (got {})",
            metrics.len()
        ));
    }
    let mut seen = HashSet::with_capacity(metrics.len());
    for metric in metrics {
        validate_metric(metric)?;
        if !seen.insert((metric.name.as_str(), &metric.labels)) {
            return Err(format!(
                "metric {} appears more than once with the same labels",
                metric.name
            ));
        }
    }
    let unlabeled = |name: &str| {
        metrics
            .iter()
            .find(|m| m.name == name && m.labels.is_empty())
            .map(|m| &m.value)
    };
    if let (Some(used), Some(total)) = (
        unlabeled(METRIC_MEMORY_USED),
        unlabeled(METRIC_MEMORY_TOTAL),
    ) && number_gt(used, total)
    {
        return Err(format!(
            "{METRIC_MEMORY_USED} ({used}) greater than {METRIC_MEMORY_TOTAL} ({total})"
        ));
    }
    Ok(())
}

/// 比較兩個 JSON 數字：都是非負整數時以 u64 比較（不失精度），否則以 f64 比較
fn number_gt(a: &Number, b: &Number) -> bool {
    match (a.as_u64(), b.as_u64()) {
        (Some(a), Some(b)) => a > b,
        _ => a.as_f64().unwrap_or(f64::NAN) > b.as_f64().unwrap_or(f64::NAN),
    }
}

/// 錯誤說明中引用對方送來的字串時，最多取 64 個字元（以 `{:?}` 輸出，控制字元會被跳脫）
fn truncate_for_message(s: &str) -> String {
    s.chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu(v: f64) -> Metric {
        Metric::new(METRIC_CPU_USAGE, Number::from_f64(v).unwrap(), "%")
    }

    #[test]
    fn metric_names() {
        for ok in [
            "system.cpu.usage",
            "a.b",
            "app.queue_depth2",
            "x1.y_",
            &format!("a.{}", "b".repeat(MAX_METRIC_NAME_BYTES - 2)),
        ] {
            assert!(is_valid_metric_name(ok), "{ok}");
        }
        for bad in [
            "",
            "cpu",
            "system..cpu",
            ".system.cpu",
            "system.cpu.",
            "System.cpu",
            "system.1cpu",
            "system._cpu",
            "system.cpu-usage",
            "system.cpu usage",
            "system.cpu.使用率",
            &format!("a.{}", "b".repeat(MAX_METRIC_NAME_BYTES - 1)),
        ] {
            assert!(!is_valid_metric_name(bad), "{bad}");
        }
    }

    #[test]
    fn units_and_label_keys() {
        for ok in [
            "%",
            "By",
            "By/s",
            "Cel",
            "s",
            "1",
            "{packets}",
            "m^2",
            "kW*h",
            "a-b_c.d",
        ] {
            assert!(is_valid_unit(ok), "{ok}");
        }
        for bad in [
            "",
            " ",
            "°C",
            "By s",
            "\"",
            "a\\b",
            &"a".repeat(MAX_METRIC_UNIT_BYTES + 1),
        ] {
            assert!(!is_valid_unit(bad), "{bad}");
        }
        assert!(is_valid_unit(&"a".repeat(MAX_METRIC_UNIT_BYTES)));
        for ok in [
            "device",
            "_x",
            "mount_point2",
            &"k".repeat(MAX_LABEL_KEY_BYTES),
        ] {
            assert!(is_valid_label_key(ok), "{ok}");
        }
        for bad in [
            "",
            "Device",
            "1x",
            "a.b",
            "a-b",
            "名稱",
            &"k".repeat(MAX_LABEL_KEY_BYTES + 1),
        ] {
            assert!(!is_valid_label_key(bad), "{bad}");
        }
    }

    #[test]
    fn label_values_follow_the_display_text_rules() {
        let m = |v: &str| cpu(1.0).with_label("sensor", v);
        assert!(validate_metric(&m("coretemp Package id 0")).is_ok());
        assert!(validate_metric(&m("C:\\")).is_ok());
        assert!(validate_metric(&m(&"主".repeat(85))).is_ok());
        assert!(validate_metric(&m(&"v".repeat(MAX_LABEL_VALUE_BYTES))).is_ok());
        for bad in [
            String::new(),
            "v".repeat(MAX_LABEL_VALUE_BYTES + 1),
            "a\nb".to_string(),
            "a\u{202e}b".to_string(),
            "a\u{3164}".to_string(),
        ] {
            assert!(validate_metric(&m(&bad)).is_err(), "{bad:?}");
        }
        let mut many = cpu(1.0);
        for i in 0..MAX_METRIC_LABELS {
            many = many.with_label(&format!("k{i}"), "v");
        }
        assert!(validate_metric(&many).is_ok());
        assert!(validate_metric(&many.with_label("one_more", "v")).is_err());
    }

    #[test]
    fn standard_metrics_check_unit_and_range() {
        assert!(validate_metric(&cpu(0.0)).is_ok());
        assert!(validate_metric(&cpu(100.0)).is_ok());
        assert!(validate_metric(&cpu(-0.01)).is_err());
        assert!(validate_metric(&cpu(100.01)).is_err());
        assert!(validate_metric(&Metric::new(METRIC_CPU_USAGE, 5, "percent")).is_err());
        assert!(validate_metric(&Metric::new(METRIC_MEMORY_TOTAL, 0, "By")).is_ok());
        assert!(validate_metric(&Metric::new(METRIC_MEMORY_TOTAL, -1, "By")).is_err());
        assert!(validate_metric(&Metric::new(METRIC_MEMORY_TOTAL, 1, "MB")).is_err());
        // 溫度可以是負數
        let t = Metric::new(METRIC_TEMPERATURE, Number::from_f64(-12.5).unwrap(), "Cel");
        assert!(validate_metric(&t).is_ok());
        // 非標準指標只檢查格式
        assert!(validate_metric(&Metric::new("app.queue.depth", -3, "{items}")).is_ok());
        // 表中的名稱本身都合法、不重複
        let mut names = HashSet::new();
        for m in STANDARD_METRICS {
            assert!(
                is_valid_metric_name(m.name) && is_valid_unit(m.unit),
                "{m:?}"
            );
            assert!(names.insert(m.name), "{m:?}");
        }
    }

    #[test]
    fn metric_list_limits() {
        assert!(validate_metrics(&[]).is_err());
        assert!(validate_metrics(&[cpu(1.0)]).is_ok());
        // 同名但 label 不同可以；完全相同不行
        let fs = |mount: &str| {
            Metric::new(METRIC_FILESYSTEM_TOTAL, 1, "By").with_label("mountpoint", mount)
        };
        assert!(validate_metrics(&[fs("/"), fs("/data")]).is_ok());
        assert!(validate_metrics(&[fs("/"), fs("/")]).is_err());
        assert!(validate_metrics(&[cpu(1.0), cpu(2.0)]).is_err());
        let many: Vec<_> = (0..MAX_METRICS).map(|i| fs(&format!("/m{i}"))).collect();
        assert!(validate_metrics(&many).is_ok());
        let mut too_many = many;
        too_many.push(cpu(1.0));
        assert!(validate_metrics(&too_many).is_err());
    }

    #[test]
    fn used_memory_must_not_exceed_total() {
        let total = Metric::new(METRIC_MEMORY_TOTAL, 1000, "By");
        let used = |v: u64| Metric::new(METRIC_MEMORY_USED, v, "By");
        assert!(validate_metrics(&[total.clone(), used(1000)]).is_ok());
        assert!(validate_metrics(&[used(1001), total.clone()]).is_err());
        // 只有其中一個時不比較
        assert!(validate_metrics(&[used(1001)]).is_ok());
        // 大整數以 u64 比較，不因 f64 精度而誤判
        let big = Metric::new(METRIC_MEMORY_TOTAL, u64::MAX - 1, "By");
        assert!(validate_metrics(&[big.clone(), used(u64::MAX - 1)]).is_ok());
        assert!(validate_metrics(&[big, used(u64::MAX)]).is_err());
    }

    #[test]
    fn value_keeps_its_json_representation() {
        let m: Metric =
            serde_json::from_str(r#"{"name":"a.b","value":37034,"unit":"By"}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"name":"a.b","value":37034,"unit":"By"}"#
        );
        let m: Metric =
            serde_json::from_str(r#"{"name":"a.b","value":17.84,"unit":"%","labels":{}}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"name":"a.b","value":17.84,"unit":"%"}"#
        );
        // 未知欄位忽略；缺欄位、型別錯誤是格式錯誤
        assert!(
            serde_json::from_str::<Metric>(r#"{"name":"a.b","value":1,"unit":"1","x":true}"#)
                .is_ok()
        );
        for bad in [
            r#"{"name":"a.b","value":1}"#,
            r#"{"name":"a.b","unit":"1"}"#,
            r#"{"value":1,"unit":"1"}"#,
            r#"{"name":"a.b","value":"1","unit":"1"}"#,
            r#"{"name":"a.b","value":1,"unit":"1","labels":{"k":1}}"#,
            r#"{"name":"a.b","value":1,"unit":"1","labels":[]}"#,
        ] {
            assert!(serde_json::from_str::<Metric>(bad).is_err(), "{bad}");
        }
    }

    /// 0.2.0～0.7.0 的標準指標（凍結）。這些指標的名稱、單位與範圍不能再改：舊版 Pulse 仍會送出，
    /// 舊版 Kairos 也以這些規則檢查新版 Pulse 送出的值
    const FROZEN_V2_STANDARD: [(&str, &str, Option<f64>, Option<f64>); 8] = [
        ("system.cpu.usage", "%", Some(0.0), Some(100.0)),
        ("system.memory.total", "By", Some(0.0), None),
        ("system.memory.used", "By", Some(0.0), None),
        ("system.filesystem.total", "By", Some(0.0), None),
        ("system.filesystem.available", "By", Some(0.0), None),
        ("system.network.received", "By", Some(0.0), None),
        ("system.network.transmitted", "By", Some(0.0), None),
        ("system.temperature", "Cel", None, None),
    ];

    #[test]
    fn legacy_standard_metrics_are_unchanged() {
        assert_eq!(LEGACY_STANDARD_METRICS, FROZEN_V2_STANDARD.len());
        for (m, (name, unit, min, max)) in STANDARD_METRICS.iter().zip(FROZEN_V2_STANDARD) {
            assert_eq!((m.name, m.unit, m.min, m.max), (name, unit, min, max));
        }
    }

    #[test]
    fn new_standard_metrics_pass_the_old_server_rules() {
        // 0.7.0 的 Kairos 對不認得的 system.* 指標只檢查格式：新指標的名稱、單位與 label 名稱都必須
        // 符合這些格式，新版 Pulse 連舊版 Kairos 時 report 才不會被整筆拒絕
        for m in &STANDARD_METRICS[LEGACY_STANDARD_METRICS..] {
            assert!(m.name.starts_with("system."), "{m:?}");
            assert!(
                is_valid_metric_name(m.name) && is_valid_unit(m.unit),
                "{m:?}"
            );
            assert!(m.labels.iter().all(|k| is_valid_label_key(k)), "{m:?}");
            assert!(m.labels.len() <= MAX_METRIC_LABELS, "{m:?}");
            assert!(
                !FROZEN_V2_STANDARD.iter().any(|(n, ..)| *n == m.name),
                "{m:?}"
            );
        }
        // 計數器一定是非負、不限上限
        for m in STANDARD_METRICS
            .iter()
            .filter(|m| m.kind == MetricKind::Counter)
        {
            assert_eq!((m.min, m.max), (Some(0.0), None), "{m:?}");
        }
        assert_eq!(standard_metric(METRIC_UPTIME).unwrap().unit, "s");
        assert!(standard_metric("system.cpu.unknown").is_none());
    }

    #[test]
    fn new_standard_metrics_are_checked_by_the_new_server() {
        let usage = |v: f64| {
            Metric::new(METRIC_FILESYSTEM_USAGE, Number::from_f64(v).unwrap(), "%")
                .with_label("mountpoint", "/")
                .with_label("fstype", "ext4")
        };
        assert!(validate_metric(&usage(90.5)).is_ok());
        assert!(validate_metric(&usage(100.5)).is_err());
        assert!(validate_metric(&Metric::new(METRIC_UPTIME, 3600, "s")).is_ok());
        assert!(validate_metric(&Metric::new(METRIC_UPTIME, -1, "s")).is_err());
        assert!(validate_metric(&Metric::new(METRIC_UPTIME, 3600, "By")).is_err());
        let load = Metric::new(METRIC_CPU_LOAD_1M, Number::from_f64(12.25).unwrap(), "1");
        assert!(validate_metric(&load).is_ok(), "負載沒有上限");
        let rx = Metric::new(METRIC_NETWORK_PACKETS_RECEIVED, u64::MAX, "{packets}")
            .with_label("device", "eth0");
        assert!(validate_metric(&rx).is_ok());
        assert!(validate_metric(&Metric::new(METRIC_DISK_IO_TIME, 5, "s")).is_err());
    }

    #[test]
    fn counter_rate_handles_resets_and_reordering() {
        let s = |value: u64, ts_ms: i64, uptime: Option<u64>| CounterSample {
            value,
            ts_ms,
            uptime_secs: uptime,
        };
        assert_eq!(
            counter_rate(s(1000, 0, None), s(6000, 5000, None)),
            Some(1000.0)
        );
        assert_eq!(counter_rate(s(5, 0, None), s(5, 1000, None)), Some(0.0));
        // 重置（值變小）
        assert_eq!(counter_rate(s(6000, 0, None), s(10, 5000, None)), None);
        // 時間相同或倒退
        assert_eq!(counter_rate(s(1, 5000, None), s(2, 5000, None)), None);
        assert_eq!(counter_rate(s(1, 5000, None), s(2, 4000, None)), None);
        // 中間重新開機：值碰巧變大也不計算
        assert_eq!(
            counter_rate(s(100, 0, Some(86_400)), s(200, 5000, Some(3))),
            None
        );
        // 只有一筆有 uptime 時依值判斷
        assert_eq!(
            counter_rate(s(100, 0, None), s(200, 1000, Some(3))),
            Some(100.0)
        );
        // 大數值不溢位
        assert_eq!(
            counter_rate(s(u64::MAX - 1000, 0, None), s(u64::MAX, 1000, None)),
            Some(1000.0)
        );
    }

    #[test]
    fn filesystem_usage_matches_df() {
        assert_eq!(filesystem_usage(0, 0), None);
        assert_eq!(filesystem_usage(0, 100), Some(0.0));
        assert_eq!(filesystem_usage(100, 0), Some(100.0));
        // df：Used 45G、Avail 5G → 90%
        assert_eq!(filesystem_usage(45, 5), Some(90.0));
        assert_eq!(filesystem_usage(1, 2), Some(33.33));
        assert_eq!(filesystem_usage(u64::MAX, u64::MAX), Some(50.0));
    }
}
