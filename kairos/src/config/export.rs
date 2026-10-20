//! `[export]`：把收到的主機指標轉出到外部系統（商業審計 F7；`kairos/EXPORT.md`）。
//!
//! - `[export.prometheus]`：主控台的 `GET /api/v1/export/prometheus`（Prometheus text／OpenMetrics），
//!   以 API token（`hosts:read`，依主機群組篩選）驗證。
//! - `[export.otlp]`：以 OTLP/HTTP（protobuf）把每一筆 report 推送到 OpenTelemetry collector。
//!
//! 部分鍵可從主控台修改（存在資料庫 `settings` 表、優先於本檔；見 [`crate::export::settings`]）。
//! 機密（OTLP 的 HTTP 標頭，例如 `Authorization`）不寫在本檔：`headers_file` 指向另一個檔案，
//! 或由主控台輸入（以 `alerts-secrets.key` 加密後存入資料庫）。

use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind};

/// `export.prometheus.stale_after_secs` 的範圍
pub const STALE_AFTER_RANGE: (u64, u64) = (30, 86_400);
/// `export.prometheus.processes_top_n` 的範圍（report 的 `top_cpu`／`top_memory` 各最多 10 個）
pub const PROCESSES_TOP_N_RANGE: (u32, u32) = (1, 10);
/// `export.otlp.interval_secs` 的範圍
pub const OTLP_INTERVAL_RANGE: (u64, u64) = (1, 3600);
/// `export.otlp.timeout_secs` 的範圍
pub const OTLP_TIMEOUT_RANGE: (u64, u64) = (1, 60);
/// `export.otlp.max_batch_points` 的範圍
pub const OTLP_BATCH_RANGE: (usize, usize) = (100, 100_000);
/// `export.otlp.queue_max_points` 的範圍
pub const OTLP_QUEUE_RANGE: (usize, usize) = (1_000, 10_000_000);
/// `export.otlp.max_retries` 的上限
pub const OTLP_MAX_RETRIES: u32 = 20;
/// `export.otlp.resource_attributes` 的項目數上限
pub const MAX_RESOURCE_ATTRIBUTES: usize = 32;
/// OTLP 端點網址的長度上限
pub const MAX_ENDPOINT_CHARS: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct ExportSettings {
    pub prometheus: PrometheusExportSettings,
    pub otlp: OtlpSettings,
}

/// `[export.prometheus]`
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct PrometheusExportSettings {
    /// `GET /api/v1/export/prometheus` 是否提供（未啟用時回 404 `export_disabled`）
    pub enabled: bool,
    /// 最後一筆 report 的取樣時間超過這麼久（秒）就不輸出該主機的指標（只輸出 `kairos_host_up`），30～86400
    pub stale_after_secs: u64,
    /// 另外輸出行程排行（`kairos_process_*`，標籤含 pid 與行程名稱；高基數，預設不輸出）
    pub include_processes: bool,
    /// 行程排行每台主機輸出前幾名（CPU、記憶體各自），1～10
    pub processes_top_n: u32,
}

impl Default for PrometheusExportSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            stale_after_secs: 300,
            include_processes: false,
            processes_top_n: 5,
        }
    }
}

/// OTLP 指標名稱的對應方式
pub const NAMING_SEMCONV: &str = "semconv";
pub const NAMING_KAIROS: &str = "kairos";
/// 佇列已滿時的丟棄策略
pub const DROP_OLDEST: &str = "oldest";
pub const DROP_NEWEST: &str = "newest";

/// `[export.otlp]`
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct OtlpSettings {
    pub enabled: bool,
    /// OTLP/HTTP 的指標端點，例如 `https://otel-collector.example.com:4318/v1/metrics`
    /// （collector 的 `otlphttp` receiver；`http://` 也接受，標頭會以明文傳送）
    pub endpoint: String,
    /// 額外的 HTTP 標頭所在的檔案（每行 `名稱: 值`，`#` 開頭為註解；例如 `Authorization: Bearer …`）。
    /// 主控台輸入的標頭優先於本檔
    pub headers_file: String,
    /// 驗證 collector 伺服器憑證的 CA（PEM，另外加入作業系統的信任清單）；不提供「略過驗證」
    pub ca_file: String,
    /// collector 要求用戶端憑證（mTLS）時的憑證與私鑰（PEM），兩者同時設定或同時留空
    pub client_cert: String,
    pub client_key: String,
    /// 推送的間隔（秒），1～3600；佇列累積到 `max_batch_points` 時提早送出
    pub interval_secs: u64,
    /// 每個請求（連線、TLS、送出與回應）的逾時（秒），1～60
    pub timeout_secs: u64,
    /// 每個請求最多幾個資料點（以 report 為單位切批，一筆 report 不拆開），100～100000
    pub max_batch_points: usize,
    /// 記憶體佇列的上限（資料點），1000～10000000；一個資料點約 100～200 bytes
    pub queue_max_points: usize,
    /// 佇列已滿時：`oldest` 丟掉最舊的（預設，保留最新的狀態）、`newest` 丟掉新收到的
    pub drop_policy: String,
    /// 可重試的失敗（連線失敗、逾時、HTTP 429／502／503／504）最多重試幾次（指數退避 1、2、4… 秒，
    /// 最多 30 秒；429／503 的 `Retry-After` 優先），0～20；用完時丟掉這一批
    pub max_retries: u32,
    /// 指標名稱：`semconv`（對應 OpenTelemetry semantic conventions 的 `system.*`，預設）或
    /// `kairos`（原樣使用 Kairos 的名稱與單位，見 `crates/kairos-proto/METRICS.md`）
    pub naming: String,
    /// 另外加到每台主機 resource 的屬性（`名稱=值`，例如 `deployment.environment=production`），最多 32 個
    #[serde(deserialize_with = "crate::config::string_or_list")]
    pub resource_attributes: Vec<String>,
}

impl Default for OtlpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            headers_file: String::new(),
            ca_file: String::new(),
            client_cert: String::new(),
            client_key: String::new(),
            interval_secs: 10,
            timeout_secs: 10,
            max_batch_points: 8_192,
            queue_max_points: 500_000,
            drop_policy: DROP_OLDEST.to_string(),
            max_retries: 5,
            naming: NAMING_SEMCONV.to_string(),
            resource_attributes: Vec::new(),
        }
    }
}

/// 檢查 OTLP 端點：空字串（未設定）或 `http(s)://host[:port]/path`，不可含帳號密碼
pub fn validate_endpoint(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(());
    }
    if url.chars().count() > MAX_ENDPOINT_CHARS || url.chars().any(char::is_control) {
        return Err(format!(
            "OTLP 端點必須是 {MAX_ENDPOINT_CHARS} 個字元以內、不含控制字元的網址"
        ));
    }
    crate::alerts::notify::transport::parse_url(url, true)
        .map(|_| ())
        .map_err(|e| format!("OTLP 端點無效：{}", e.replace("Webhook ", "")))
}

/// 解析 `名稱=值`（resource 屬性）
pub fn parse_attribute(text: &str) -> Result<(String, String), String> {
    let (k, v) = text
        .split_once('=')
        .ok_or_else(|| format!("resource 屬性「{text}」必須是 名稱=值"))?;
    let k = k.trim();
    if k.is_empty()
        || k.len() > 255
        || !k
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "resource 屬性名稱「{k}」必須是 1～255 個英數字、「.」「_」「-」"
        ));
    }
    if v.len() > 1024 || v.chars().any(char::is_control) {
        return Err(format!(
            "resource 屬性 {k} 的值必須是 1024 bytes 以內、不含控制字元"
        ));
    }
    Ok((k.to_string(), v.trim().to_string()))
}

/// 數值範圍
pub fn in_range<T: PartialOrd + Copy + std::fmt::Display>(
    key: &str,
    v: T,
    (lo, hi): (T, T),
) -> Result<(), String> {
    if v < lo || v > hi {
        Err(format!("{key} 必須介於 {lo}..={hi}（目前為 {v}）"))
    } else {
        Ok(())
    }
}

impl ExportSettings {
    pub fn validate(&self) -> Result<(), Error> {
        let bad = |m: String| Error::new(ErrorKind::InvalidInput, m);
        let p = &self.prometheus;
        in_range(
            "export.prometheus.stale_after_secs",
            p.stale_after_secs,
            STALE_AFTER_RANGE,
        )
        .map_err(bad)?;
        in_range(
            "export.prometheus.processes_top_n",
            p.processes_top_n,
            PROCESSES_TOP_N_RANGE,
        )
        .map_err(bad)?;
        let o = &self.otlp;
        validate_endpoint(&o.endpoint).map_err(|e| bad(format!("export.otlp.endpoint：{e}")))?;
        if o.enabled && o.endpoint.trim().is_empty() {
            return Err(bad(
                "export.otlp.enabled = true 時必須設定 export.otlp.endpoint（例如 https://otel-collector:4318/v1/metrics）".into(),
            ));
        }
        in_range(
            "export.otlp.interval_secs",
            o.interval_secs,
            OTLP_INTERVAL_RANGE,
        )
        .map_err(bad)?;
        in_range(
            "export.otlp.timeout_secs",
            o.timeout_secs,
            OTLP_TIMEOUT_RANGE,
        )
        .map_err(bad)?;
        in_range(
            "export.otlp.max_batch_points",
            o.max_batch_points,
            OTLP_BATCH_RANGE,
        )
        .map_err(bad)?;
        in_range(
            "export.otlp.queue_max_points",
            o.queue_max_points,
            OTLP_QUEUE_RANGE,
        )
        .map_err(bad)?;
        if o.queue_max_points < o.max_batch_points {
            return Err(bad(format!(
                "export.otlp.queue_max_points（{}）不可小於 export.otlp.max_batch_points（{}）",
                o.queue_max_points, o.max_batch_points
            )));
        }
        in_range(
            "export.otlp.max_retries",
            o.max_retries,
            (0, OTLP_MAX_RETRIES),
        )
        .map_err(bad)?;
        if !matches!(o.drop_policy.as_str(), DROP_OLDEST | DROP_NEWEST) {
            return Err(bad(format!(
                "export.otlp.drop_policy 的值 \"{}\" 無效，可用值：oldest、newest",
                o.drop_policy
            )));
        }
        if !matches!(o.naming.as_str(), NAMING_SEMCONV | NAMING_KAIROS) {
            return Err(bad(format!(
                "export.otlp.naming 的值 \"{}\" 無效，可用值：semconv、kairos",
                o.naming
            )));
        }
        if o.client_cert.trim().is_empty() != o.client_key.trim().is_empty() {
            return Err(bad(
                "export.otlp.client_cert 與 export.otlp.client_key 必須同時設定或同時留空".into(),
            ));
        }
        let attrs: Vec<&String> = o
            .resource_attributes
            .iter()
            .filter(|a| !a.trim().is_empty())
            .collect();
        if attrs.len() > MAX_RESOURCE_ATTRIBUTES {
            return Err(bad(format!(
                "export.otlp.resource_attributes 最多 {MAX_RESOURCE_ATTRIBUTES} 個"
            )));
        }
        for a in attrs {
            parse_attribute(a).map_err(|e| bad(format!("export.otlp.resource_attributes：{e}")))?;
        }
        Ok(())
    }
}

/// 解析 HTTP 標頭檔（`名稱: 值`，一行一個；空行與 `#` 開頭略過）
pub fn parse_headers(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| format!("第 {} 行不是「名稱: 值」", i + 1))?;
        let (k, v) = (k.trim(), v.trim());
        validate_header(k, v).map_err(|e| format!("第 {} 行：{e}", i + 1))?;
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

/// 自訂標頭不可覆寫的標頭（由 Kairos 決定）
const RESERVED_HEADERS: [&str; 6] = [
    "host",
    "content-type",
    "content-length",
    "transfer-encoding",
    "connection",
    "user-agent",
];

/// 標頭名稱為 token 字元、值為可見 ASCII 與空白；不可覆寫 [`RESERVED_HEADERS`]
pub fn validate_header(name: &str, value: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    {
        return Err(format!("標頭名稱「{name}」不合法"));
    }
    if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!("標頭 {name} 由 Kairos 設定，不可自訂"));
    }
    if value.len() > 8192
        || !value
            .bytes()
            .all(|b| b == b' ' || b == b'\t' || b.is_ascii_graphic())
    {
        return Err(format!(
            "標頭 {name} 的值必須是 8192 bytes 以內的可見 ASCII 字元"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_ranges_are_checked() {
        let mut s = ExportSettings::default();
        assert!(s.validate().is_ok());
        s.otlp.enabled = true;
        assert!(s.validate().is_err(), "啟用時必須有端點");
        s.otlp.endpoint = "https://collector:4318/v1/metrics".into();
        assert!(s.validate().is_ok());
        s.otlp.endpoint = "ftp://x".into();
        assert!(s.validate().is_err());
        s.otlp.endpoint = "https://user:pw@collector/v1/metrics".into();
        assert!(s.validate().is_err(), "不可含帳號密碼");
        s.otlp.endpoint = "http://127.0.0.1:4318/v1/metrics".into();
        assert!(s.validate().is_ok());
        s.otlp.interval_secs = 0;
        assert!(s.validate().is_err());
        s.otlp.interval_secs = 10;
        s.otlp.queue_max_points = 1000;
        assert!(s.validate().is_err(), "佇列不可小於批次");
        s.otlp.queue_max_points = 500_000;
        s.otlp.naming = "prometheus".into();
        assert!(s.validate().is_err());
        s.otlp.naming = NAMING_KAIROS.into();
        s.otlp.client_cert = "c.pem".into();
        assert!(s.validate().is_err());
        s.otlp.client_key = "k.pem".into();
        assert!(s.validate().is_ok());
        s.otlp.resource_attributes = vec!["deployment.environment=prod".into(), "bad".into()];
        assert!(s.validate().is_err());
        s.otlp.resource_attributes.pop();
        assert!(s.validate().is_ok());
        s.prometheus.stale_after_secs = 10;
        assert!(s.validate().is_err());
    }

    #[test]
    fn headers_file_is_parsed_and_reserved_headers_are_rejected() {
        let h = parse_headers("# 註解\nAuthorization: Bearer abc\n\nX-Scope-OrgID:  tenant-a \n")
            .unwrap();
        assert_eq!(
            h,
            vec![
                ("Authorization".into(), "Bearer abc".into()),
                ("X-Scope-OrgID".into(), "tenant-a".into())
            ]
        );
        assert!(parse_headers("Content-Type: text/plain").is_err());
        assert!(parse_headers("no colon").is_err());
        assert!(parse_headers("Bad Name: x").is_err());
        assert!(parse_headers("X-A: \u{7f}").is_err());
    }
}
