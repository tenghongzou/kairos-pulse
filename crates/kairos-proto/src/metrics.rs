//! v2 的通用指標格式（商業審計 A4）：report 的 `metrics: [{name, value, unit, labels}]`。
//!
//! 伺服器只檢查格式與上限（名稱、單位、label 的字元與長度、數量、重複），以及
//! [`STANDARD_METRICS`] 列出的標準指標的單位與數值範圍；其他指標原樣保存，不解讀其意義。

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
/// 總記憶體（bytes，沒有 label）
pub const METRIC_MEMORY_TOTAL: &str = "system.memory.total";
/// 已使用記憶體（bytes，沒有 label）
pub const METRIC_MEMORY_USED: &str = "system.memory.used";
/// 檔案系統總容量（bytes；label `mountpoint`、`fstype`）
pub const METRIC_FILESYSTEM_TOTAL: &str = "system.filesystem.total";
/// 檔案系統可用空間（bytes；label `mountpoint`、`fstype`）
pub const METRIC_FILESYSTEM_AVAILABLE: &str = "system.filesystem.available";
/// 網路介面開機以來累計接收量（bytes；label `device`）
pub const METRIC_NETWORK_RECEIVED: &str = "system.network.received";
/// 網路介面開機以來累計傳送量（bytes；label `device`）
pub const METRIC_NETWORK_TRANSMITTED: &str = "system.network.transmitted";
/// 元件溫度（攝氏；label `sensor`）
pub const METRIC_TEMPERATURE: &str = "system.temperature";

/// 標準指標：名稱、單位與數值範圍由規格固定，伺服器會檢查
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StandardMetric {
    pub name: &'static str,
    pub unit: &'static str,
    /// 下限（含）；`None` 為不限
    pub min: Option<f64>,
    /// 上限（含）；`None` 為不限
    pub max: Option<f64>,
}

/// PROTOCOL.md「標準指標」表（Pulse 送出的指標）
pub const STANDARD_METRICS: &[StandardMetric] = &[
    StandardMetric {
        name: METRIC_CPU_USAGE,
        unit: "%",
        min: Some(0.0),
        max: Some(100.0),
    },
    StandardMetric {
        name: METRIC_MEMORY_TOTAL,
        unit: "By",
        min: Some(0.0),
        max: None,
    },
    StandardMetric {
        name: METRIC_MEMORY_USED,
        unit: "By",
        min: Some(0.0),
        max: None,
    },
    StandardMetric {
        name: METRIC_FILESYSTEM_TOTAL,
        unit: "By",
        min: Some(0.0),
        max: None,
    },
    StandardMetric {
        name: METRIC_FILESYSTEM_AVAILABLE,
        unit: "By",
        min: Some(0.0),
        max: None,
    },
    StandardMetric {
        name: METRIC_NETWORK_RECEIVED,
        unit: "By",
        min: Some(0.0),
        max: None,
    },
    StandardMetric {
        name: METRIC_NETWORK_TRANSMITTED,
        unit: "By",
        min: Some(0.0),
        max: None,
    },
    StandardMetric {
        name: METRIC_TEMPERATURE,
        unit: "Cel",
        min: None,
        max: None,
    },
];

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
    if let Some(std) = STANDARD_METRICS.iter().find(|m| m.name == name) {
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
}
