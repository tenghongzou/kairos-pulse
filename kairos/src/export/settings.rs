//! 指標轉出的實際生效設定：環境變數 ＞ 資料庫 `settings` 表（主控台）＞ `kairos.toml` ＞ 內建預設，
//! 與註冊、憑證政策相同的規則（`kairos/PKI.md` 第 8 節、[`crate::settings`]）。
//!
//! 只有 [`CONSOLE_KEYS`] 有資料庫的覆寫層；其他鍵（逾時、佇列、TLS 檔案…）只來自設定檔與環境變數。
//! 鍵存在同一張 `settings` 表，但不列入 [`crate::settings::CONSOLE_KEYS`]（`GET /api/v1/settings`、
//! `POST /api/v1/settings/reset` 不處理它們），由 `GET`／`PUT /api/v1/export/settings` 管理。
//!
//! OTLP 的 HTTP 標頭是機密：資料庫中為 `{"sealed": <base64url>, "names": [...]}`（以
//! `alerts-secrets.key` 加密，[`SecretBox`]），API 與日誌只顯示名稱與遮蔽值。設定檔的對應鍵是
//! `export.otlp.headers_file`（以環境變數設定時主控台不能修改標頭）。

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

use crate::alerts::secrets::{MASK, SecretBox};
use crate::config::export::{
    NAMING_KAIROS, NAMING_SEMCONV, OTLP_INTERVAL_RANGE, STALE_AFTER_RANGE, in_range, parse_headers,
    validate_endpoint, validate_header,
};
use crate::config::{AppConfig, OtlpSettings, PrometheusExportSettings, SettingSource};

pub const KEY_PROM_ENABLED: &str = "export.prometheus.enabled";
pub const KEY_PROM_STALE: &str = "export.prometheus.stale_after_secs";
pub const KEY_PROM_PROCESSES: &str = "export.prometheus.include_processes";
pub const KEY_OTLP_ENABLED: &str = "export.otlp.enabled";
pub const KEY_OTLP_ENDPOINT: &str = "export.otlp.endpoint";
pub const KEY_OTLP_INTERVAL: &str = "export.otlp.interval_secs";
pub const KEY_OTLP_NAMING: &str = "export.otlp.naming";
/// 主控台輸入的 OTLP 標頭（加密）；對應的設定檔鍵為 [`FILE_KEY_HEADERS`]
pub const KEY_OTLP_HEADERS: &str = "export.otlp.headers";
pub const FILE_KEY_HEADERS: &str = "export.otlp.headers_file";

/// 主控台可修改、存在 `settings` 表的鍵
pub const CONSOLE_KEYS: [&str; 8] = [
    KEY_PROM_ENABLED,
    KEY_PROM_STALE,
    KEY_PROM_PROCESSES,
    KEY_OTLP_ENABLED,
    KEY_OTLP_ENDPOINT,
    KEY_OTLP_INTERVAL,
    KEY_OTLP_NAMING,
    KEY_OTLP_HEADERS,
];

/// 主控台輸入的標頭最多幾個
pub const MAX_CONSOLE_HEADERS: usize = 16;

/// 資料庫的一列：(鍵, 值, 修改時間, 修改者)
pub type Row = (String, Value, i64, String);

/// 標頭的來源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadersFrom {
    None,
    File,
    Console,
}

impl HeadersFrom {
    pub fn as_str(self) -> &'static str {
        match self {
            HeadersFrom::None => "none",
            HeadersFrom::File => "file",
            HeadersFrom::Console => "console",
        }
    }
}

/// 實際生效的設定與來源
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub prometheus: PrometheusExportSettings,
    pub otlp: OtlpSettings,
    /// OTLP 請求另外帶的標頭（已解密或已讀取標頭檔）
    pub headers: Vec<(String, String)>,
    pub headers_from: HeadersFrom,
    /// 鍵 → 來源（[`CONSOLE_KEYS`]）
    pub sources: BTreeMap<&'static str, SettingSource>,
    /// 鍵 → (修改時間, 修改者)（來源為主控台的鍵）
    pub updated: BTreeMap<&'static str, (i64, String)>,
    /// 設定的問題（標頭檔無法讀取、機密無法解開、啟用卻沒有端點…）；啟動時記 WARN、主控台顯示
    pub problems: Vec<String>,
}

impl Effective {
    /// OTLP 是否實際推送（啟用且有端點）
    pub fn otlp_active(&self) -> bool {
        self.otlp.enabled && !self.otlp.endpoint.trim().is_empty()
    }

    pub fn source(&self, key: &str) -> SettingSource {
        self.sources
            .get(key)
            .copied()
            .unwrap_or(SettingSource::Default)
    }

    /// 鍵的生效值（API 顯示；標頭只有名稱與遮蔽值）
    pub fn value_of(&self, key: &str) -> Value {
        match key {
            KEY_PROM_ENABLED => json!(self.prometheus.enabled),
            KEY_PROM_STALE => json!(self.prometheus.stale_after_secs),
            KEY_PROM_PROCESSES => json!(self.prometheus.include_processes),
            KEY_OTLP_ENABLED => json!(self.otlp.enabled),
            KEY_OTLP_ENDPOINT => json!(self.otlp.endpoint),
            KEY_OTLP_INTERVAL => json!(self.otlp.interval_secs),
            KEY_OTLP_NAMING => json!(self.otlp.naming),
            KEY_OTLP_HEADERS => masked_headers(&self.headers),
            _ => Value::Null,
        }
    }
}

/// 標頭的遮蔽表示：`[{"name": "Authorization", "value": "••••"}]`
pub fn masked_headers(h: &[(String, String)]) -> Value {
    Value::Array(
        h.iter()
            .map(|(k, _)| json!({"name": k, "value": MASK}))
            .collect(),
    )
}

/// 這個鍵是否由環境變數設定（主控台不能修改；標頭看 `headers_file`）
pub fn locked(config: &AppConfig, key: &str) -> bool {
    let k = if key == KEY_OTLP_HEADERS {
        FILE_KEY_HEADERS
    } else {
        key
    };
    config.provenance.source(k) == SettingSource::Environment
}

/// 資料庫的值是否合法（寫入前檢查；讀取時略過不合法的值）。標頭另外以 [`validate_headers`] 檢查
pub fn validate_value(key: &str, v: &Value) -> Result<(), String> {
    match key {
        KEY_PROM_ENABLED | KEY_PROM_PROCESSES | KEY_OTLP_ENABLED => {
            if v.is_boolean() {
                Ok(())
            } else {
                Err(format!("{key} 必須是 true 或 false"))
            }
        }
        KEY_PROM_STALE => match v.as_u64() {
            Some(n) => in_range(key, n, STALE_AFTER_RANGE),
            None => Err(format!("{key} 必須是整數")),
        },
        KEY_OTLP_INTERVAL => match v.as_u64() {
            Some(n) => in_range(key, n, OTLP_INTERVAL_RANGE),
            None => Err(format!("{key} 必須是整數")),
        },
        KEY_OTLP_ENDPOINT => match v.as_str() {
            Some(s) => validate_endpoint(s),
            None => Err(format!("{key} 必須是字串")),
        },
        KEY_OTLP_NAMING => match v.as_str() {
            Some(NAMING_SEMCONV | NAMING_KAIROS) => Ok(()),
            _ => Err(format!("{key} 必須是 semconv 或 kairos")),
        },
        KEY_OTLP_HEADERS => Ok(()),
        _ => Err(format!("{key} 不是可從主控台修改的指標轉出設定")),
    }
}

/// 主控台送出的標頭清單（`[{"name", "value"}]`）
pub fn validate_headers(v: &Value) -> Result<Vec<(String, String)>, String> {
    let list = v
        .as_array()
        .ok_or("otlp.headers 必須是 [{\"name\": …, \"value\": …}] 的陣列")?;
    if list.len() > MAX_CONSOLE_HEADERS {
        return Err(format!("otlp.headers 最多 {MAX_CONSOLE_HEADERS} 個"));
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for item in list {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .ok_or("每個標頭都要有 name 與 value")?
            .trim();
        let value = item
            .get("value")
            .and_then(Value::as_str)
            .ok_or("每個標頭都要有 name 與 value")?
            .trim();
        validate_header(name, value)?;
        if out.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
            return Err(format!("標頭 {name} 重複"));
        }
        out.push((name.to_string(), value.to_string()));
    }
    Ok(out)
}

/// 標頭加密後存入資料庫的值
pub fn seal_headers(secrets: &SecretBox, h: &[(String, String)]) -> Result<Value, String> {
    let plain = Value::Array(
        h.iter()
            .map(|(k, v)| json!({"name": k, "value": v}))
            .collect(),
    );
    let blob = secrets.seal(&plain)?;
    Ok(json!({
        "sealed": crate::api::http::b64url_encode(&blob),
        "names": h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
    }))
}

fn unseal_headers(secrets: &SecretBox, v: &Value) -> Result<Vec<(String, String)>, String> {
    let text = v
        .get("sealed")
        .and_then(Value::as_str)
        .ok_or("格式不正確")?;
    let blob = crate::api::http::b64url_decode(text).ok_or("格式不正確")?;
    validate_headers(&secrets.unseal(&blob)?)
}

/// 讀取資料庫 `settings` 表中指標轉出的列（失敗時當作沒有，啟動時記 WARN）
pub fn load_rows(db: &Path) -> Vec<Row> {
    let read = || -> Result<Vec<Row>, String> {
        let conn = crate::store::open_read(db).map_err(|e| e.to_string())?;
        conn.load_settings().map_err(|e| e.to_string())
    };
    match read() {
        Ok(rows) => rows
            .into_iter()
            .filter(|(k, ..)| CONSOLE_KEYS.contains(&k.as_str()))
            .collect(),
        Err(e) => {
            log::warn!("指標轉出：無法讀取資料庫中的設定（改用設定檔的值）：{e}");
            Vec::new()
        }
    }
}

/// 計算實際生效的設定。資料庫中不合法的值略過（以設定檔或預設為準）
pub fn effective(config: &AppConfig, rows: &[Row], secrets: Option<&SecretBox>) -> Effective {
    let prov = &config.provenance;
    let mut sources = BTreeMap::new();
    let mut updated = BTreeMap::new();
    let mut problems = Vec::new();
    let mut pick = |key: &'static str| -> Option<Value> {
        let base = prov.source(key);
        if base != SettingSource::Environment
            && let Some((_, v, at, by)) = rows.iter().find(|(k, ..)| k == key)
            && validate_value(key, v).is_ok()
        {
            sources.insert(key, SettingSource::Console);
            updated.insert(key, (*at, by.clone()));
            return Some(v.clone());
        }
        sources.insert(key, base);
        None
    };
    let mut prometheus = config.export.prometheus.clone();
    if let Some(v) = pick(KEY_PROM_ENABLED).and_then(|v| v.as_bool()) {
        prometheus.enabled = v;
    }
    if let Some(v) = pick(KEY_PROM_STALE).and_then(|v| v.as_u64()) {
        prometheus.stale_after_secs = v;
    }
    if let Some(v) = pick(KEY_PROM_PROCESSES).and_then(|v| v.as_bool()) {
        prometheus.include_processes = v;
    }
    let mut otlp = config.export.otlp.clone();
    if let Some(v) = pick(KEY_OTLP_ENABLED).and_then(|v| v.as_bool()) {
        otlp.enabled = v;
    }
    if let Some(v) = pick(KEY_OTLP_ENDPOINT) {
        otlp.endpoint = v.as_str().unwrap_or_default().trim().to_string();
    }
    if let Some(v) = pick(KEY_OTLP_INTERVAL).and_then(|v| v.as_u64()) {
        otlp.interval_secs = v;
    }
    if let Some(v) = pick(KEY_OTLP_NAMING) {
        otlp.naming = v.as_str().unwrap_or(NAMING_SEMCONV).to_string();
    }
    // 標頭：主控台（加密）＞ headers_file；headers_file 以環境變數設定時不採用主控台的值
    let header_row = rows.iter().find(|(k, ..)| k == KEY_OTLP_HEADERS);
    let header_locked = prov.source(FILE_KEY_HEADERS) == SettingSource::Environment;
    let mut headers = Vec::new();
    let mut headers_from = HeadersFrom::None;
    let mut from_console = false;
    if !header_locked && let Some((_, v, at, by)) = header_row {
        match secrets.map(|s| unseal_headers(s, v)) {
            Some(Ok(h)) => {
                headers = h;
                headers_from = HeadersFrom::Console;
                from_console = true;
                sources.insert(KEY_OTLP_HEADERS, SettingSource::Console);
                updated.insert(KEY_OTLP_HEADERS, (*at, by.clone()));
            }
            Some(Err(e)) => problems.push(format!(
                "主控台輸入的 OTLP 標頭無法解開（{e}）：資料目錄的 {} 遺失或與資料庫不成對；改用 export.otlp.headers_file。請從備份還原金鑰檔，或在主控台重新輸入標頭",
                crate::alerts::secrets::KEY_FILE
            )),
            None => problems.push(
                "主控台輸入的 OTLP 標頭無法解開（沒有可用的金鑰檔）；改用 export.otlp.headers_file"
                    .to_string(),
            ),
        }
    }
    if !from_console {
        sources.insert(KEY_OTLP_HEADERS, prov.source(FILE_KEY_HEADERS));
        let path = otlp.headers_file.trim();
        if !path.is_empty() {
            match std::fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|t| parse_headers(&t))
            {
                Ok(h) => {
                    headers_from = if h.is_empty() {
                        HeadersFrom::None
                    } else {
                        HeadersFrom::File
                    };
                    headers = h;
                }
                Err(e) => problems.push(format!(
                    "無法使用 export.otlp.headers_file {path}（{e}）：OTLP 請求不帶自訂標頭"
                )),
            }
        }
    }
    if otlp.enabled && otlp.endpoint.trim().is_empty() {
        problems.push("export.otlp.enabled = true 但沒有設定端點，OTLP 不推送".to_string());
    }
    Effective {
        prometheus,
        otlp,
        headers,
        headers_from,
        sources,
        updated,
        problems,
    }
}

/// 套用變更後，原本的 OTLP 標頭會不會被沿用到另一個端點（安全稽核 E2）。標頭「只能更換、不能檢視」，
/// 但只改端點就沿用的話，等於可以把它送到任何地方看——包含設定檔 `headers_file` 的標頭（主控台連保存都
/// 不保存）。`headers_provided`：這次請求有沒有提供新的標頭（空的清單也算：新的端點不帶標頭）
pub fn carries_headers_to_new_endpoint(
    before: &Effective,
    after: &Effective,
    headers_provided: bool,
) -> bool {
    before.otlp.endpoint.trim() != after.otlp.endpoint.trim()
        && !after.headers.is_empty()
        && !headers_provided
}

#[cfg(test)]
mod tests {
    use super::*;

    // 安全稽核 E2：端點改變時，主控台保存的標頭與 headers_file 的標頭都不能沿用
    #[test]
    fn changing_the_endpoint_does_not_carry_stored_headers_along() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-export-e2-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("otlp-headers");
        std::fs::write(&file, "X-Api-Key: file-secret\n").unwrap();
        let mut c = AppConfig::default();
        c.export.otlp.endpoint = "https://collector.example/v1/metrics".into();
        c.export.otlp.headers_file = file.to_string_lossy().into_owned();
        let before = effective(&c, &[], None);
        assert_eq!(before.headers_from, HeadersFrom::File);
        // 主控台只改端點：設定檔的標頭會跟著走
        let moved = rows(&[(
            KEY_OTLP_ENDPOINT,
            json!("http://elsewhere.example/v1/metrics"),
        )]);
        let after = effective(&c, &moved, None);
        assert_eq!(after.headers_from, HeadersFrom::File);
        assert!(carries_headers_to_new_endpoint(&before, &after, false));
        // 同一次請求提供了新的標頭（主控台的值取代 headers_file）：可以
        assert!(!carries_headers_to_new_endpoint(&before, &after, true));
        // 端點沒有變（只改別的設定）：不受影響
        let same = rows(&[(KEY_OTLP_INTERVAL, json!(30))]);
        assert!(!carries_headers_to_new_endpoint(
            &before,
            &effective(&c, &same, None),
            false
        ));
        // 沒有任何標頭：端點可以自由修改
        c.export.otlp.headers_file = String::new();
        let before = effective(&c, &[], None);
        assert!(!carries_headers_to_new_endpoint(
            &before,
            &effective(&c, &moved, None),
            false
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rows(list: &[(&str, Value)]) -> Vec<Row> {
        list.iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    v.clone(),
                    1_760_000_000_000,
                    "admin".to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn precedence_env_console_file_default() {
        let mut c = AppConfig::default();
        let e = effective(&c, &[], None);
        assert!(!e.prometheus.enabled);
        assert_eq!(e.source(KEY_PROM_ENABLED), SettingSource::Default);
        c.export.prometheus.enabled = true;
        c.provenance.file_keys.insert(KEY_PROM_ENABLED.into());
        let e = effective(&c, &[], None);
        assert!(e.prometheus.enabled);
        assert_eq!(e.source(KEY_PROM_ENABLED), SettingSource::File);
        let r = rows(&[
            (KEY_PROM_ENABLED, json!(false)),
            (KEY_OTLP_INTERVAL, json!(99_999)), // 不合法：略過
            (KEY_OTLP_ENDPOINT, json!("http://127.0.0.1:4318/v1/metrics")),
            (KEY_OTLP_ENABLED, json!(true)),
        ]);
        let e = effective(&c, &r, None);
        assert!(!e.prometheus.enabled);
        assert_eq!(e.source(KEY_PROM_ENABLED), SettingSource::Console);
        assert_eq!(e.updated[KEY_PROM_ENABLED].1, "admin");
        assert_eq!(e.otlp.interval_secs, 10);
        assert_eq!(e.source(KEY_OTLP_INTERVAL), SettingSource::Default);
        assert!(e.otlp_active());
        c.provenance.env_keys.insert(KEY_PROM_ENABLED.into());
        let e = effective(&c, &r, None);
        assert!(e.prometheus.enabled, "環境變數優先於主控台");
        assert_eq!(e.source(KEY_PROM_ENABLED), SettingSource::Environment);
        assert!(locked(&c, KEY_PROM_ENABLED));
        assert!(!locked(&c, KEY_OTLP_ENDPOINT));
    }

    #[test]
    fn headers_are_sealed_and_fall_back_to_the_file() {
        let dir = crate::api::tests::temp_dir("export-headers");
        let file = dir.join("otlp-headers");
        std::fs::write(&file, "Authorization: Bearer from-file\n").unwrap();
        let mut c = AppConfig::default();
        c.export.otlp.headers_file = file.to_string_lossy().into_owned();
        let e = effective(&c, &[], None);
        assert_eq!(e.headers_from, HeadersFrom::File);
        assert_eq!(e.headers[0].1, "Bearer from-file");
        let sb = SecretBox::from_key(&[7u8; 32]).unwrap();
        let h = validate_headers(&json!([{"name": "X-Token", "value": "s3cret"}])).unwrap();
        let sealed = seal_headers(&sb, &h).unwrap();
        assert!(!sealed.to_string().contains("s3cret"), "資料庫中不可有明文");
        assert_eq!(sealed["names"], json!(["X-Token"]));
        let r = rows(&[(KEY_OTLP_HEADERS, sealed.clone())]);
        let e = effective(&c, &r, Some(&sb));
        assert_eq!(e.headers_from, HeadersFrom::Console);
        assert_eq!(e.headers, vec![("X-Token".into(), "s3cret".into())]);
        assert_eq!(
            e.value_of(KEY_OTLP_HEADERS),
            json!([{"name": "X-Token", "value": MASK}])
        );
        // 金鑰不成對：改用標頭檔並記錄問題
        let other = SecretBox::from_key(&[8u8; 32]).unwrap();
        let e = effective(&c, &r, Some(&other));
        assert_eq!(e.headers_from, HeadersFrom::File);
        assert_eq!(e.problems.len(), 1);
        // headers_file 由環境變數設定：主控台的標頭不採用
        c.provenance.env_keys.insert(FILE_KEY_HEADERS.into());
        let e = effective(&c, &r, Some(&sb));
        assert_eq!(e.headers_from, HeadersFrom::File);
        assert!(locked(&c, KEY_OTLP_HEADERS));
        assert!(validate_headers(&json!([{"name": "Host", "value": "x"}])).is_err());
        assert!(
            validate_headers(&json!([{"name": "A", "value": "1"}, {"name": "a", "value": "2"}]))
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn values_are_validated() {
        assert!(validate_value(KEY_PROM_ENABLED, &json!(true)).is_ok());
        assert!(validate_value(KEY_PROM_ENABLED, &json!("true")).is_err());
        assert!(validate_value(KEY_PROM_STALE, &json!(29)).is_err());
        assert!(validate_value(KEY_OTLP_ENDPOINT, &json!("")).is_ok());
        assert!(validate_value(KEY_OTLP_ENDPOINT, &json!("collector:4318")).is_err());
        assert!(validate_value(KEY_OTLP_NAMING, &json!("kairos")).is_ok());
        assert!(validate_value("export.otlp.queue_max_points", &json!(1)).is_err());
    }
}
