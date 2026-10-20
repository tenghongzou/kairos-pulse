//! 實際生效的設定：環境變數 ＞ 資料庫 `settings` 表（主控台、CLI 寫入）＞ `kairos.toml` ＞ 程式內建預設
//! （`kairos/PKI.md` 第 8 節，已決定（2026-10-14））。
//!
//! 只有主控台可修改的項目（[`CONSOLE_KEYS`]）有資料庫的覆寫層；其他設定只來自設定檔與環境變數。
//! 被環境變數覆寫的項目無法從主控台修改（`409 locked_by_environment`，CLI 結束代碼 3）。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use kairos_proto::enroll::EnrollMode;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::config::{AppConfig, CertPolicySettings, SettingSource};
use crate::store::db::StoreResult;

pub const KEY_MODE: &str = "enroll.mode";
pub const KEY_AUTO_PAUSE_ENABLED: &str = "enroll.auto_pause_enabled";
pub const KEY_AUTO_PAUSE_AFTER_HOURS: &str = "enroll.auto_pause_after_hours";
pub const KEY_CERT_LIFETIME_DAYS: &str = "pki.policy.cert_lifetime_days";
pub const KEY_RENEW_BEFORE_DAYS: &str = "pki.policy.renew_before_days";
pub const KEY_RENEW_REQUIRES_APPROVAL: &str = "pki.policy.renew_requires_approval";
pub const KEY_GRACE_DAYS: &str = "pki.policy.expired_renew_grace_days";
pub const KEY_REVOKE_ON_SELF_DECOMMISSION: &str = "pki.policy.revoke_on_self_decommission";
pub const KEY_REVOKE_SUPERSEDED: &str = "pki.policy.revoke_superseded";

/// 內部狀態：模式最近一次切換為開放的時間（自動暫停的計時起點；不是設定項目，不列在 `config show`）
pub const KEY_OPEN_SINCE: &str = "enroll.open_since";

/// 主控台（與 CLI）可修改、存在 `settings` 表的設定鍵
pub const CONSOLE_KEYS: [&str; 9] = [
    KEY_MODE,
    KEY_AUTO_PAUSE_ENABLED,
    KEY_AUTO_PAUSE_AFTER_HOURS,
    KEY_CERT_LIFETIME_DAYS,
    KEY_RENEW_BEFORE_DAYS,
    KEY_RENEW_REQUIRES_APPROVAL,
    KEY_GRACE_DAYS,
    KEY_REVOKE_ON_SELF_DECOMMISSION,
    KEY_REVOKE_SUPERSEDED,
];

/// 資料庫的覆寫值（鍵 → JSON 值）
pub type Overrides = BTreeMap<String, Value>;

/// 實際生效的模式與憑證政策，以及每一項的來源
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub mode: EnrollMode,
    pub auto_pause_enabled: bool,
    pub auto_pause_after_hours: u32,
    pub policy: CertPolicySettings,
    /// 鍵 → 來源
    pub sources: BTreeMap<&'static str, SettingSource>,
}

impl Effective {
    pub fn source(&self, key: &str) -> SettingSource {
        self.sources
            .get(key)
            .copied()
            .unwrap_or(SettingSource::Default)
    }

    /// 自動暫停的門檻（毫秒）。debug 建置可以 `KAIROS_TEST_AUTO_PAUSE_SECS` 縮短（e2e 測試掛鉤）
    pub fn auto_pause_after_ms(&self) -> i64 {
        if let Some(secs) = crate::testhooks::auto_pause_secs() {
            return secs.saturating_mul(1000);
        }
        i64::from(self.auto_pause_after_hours) * 3_600_000
    }

    /// 用戶端憑證的有效期間（毫秒）。debug 建置可以 `KAIROS_TEST_CERT_LIFETIME_SECS` 縮短
    pub fn cert_lifetime_ms(&self, days: Option<i64>) -> i64 {
        if let Some(secs) = crate::testhooks::cert_lifetime_secs() {
            return secs.saturating_mul(1000);
        }
        days.unwrap_or(i64::from(self.policy.cert_lifetime_days)) * 86_400_000
    }

    /// `issued` 回應與 `hello_ack` 的政策
    pub fn protocol_policy(&self) -> kairos_proto::enroll::CertPolicy {
        kairos_proto::enroll::CertPolicy {
            cert_lifetime_days: self.policy.cert_lifetime_days,
            renew_before_days: self.policy.renew_before_days,
            expired_renew_grace_days: self.policy.expired_renew_grace_days,
        }
    }

    /// `GET /api/v1/enroll/mode` 與 `kairos enroll mode` 的 `sources`
    pub fn sources_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        for (k, v) in &self.sources {
            m.insert((*k).to_string(), json!(v.as_str()));
        }
        Value::Object(m)
    }
}

/// 資料庫的值是否合法；不合法時回傳說明（寫入前檢查，讀取時略過不合法的值）
pub fn validate_override(key: &str, value: &Value) -> Result<(), String> {
    let ok = match key {
        KEY_MODE => value.as_str().and_then(EnrollMode::parse).is_some(),
        KEY_AUTO_PAUSE_ENABLED
        | KEY_RENEW_REQUIRES_APPROVAL
        | KEY_REVOKE_ON_SELF_DECOMMISSION
        | KEY_REVOKE_SUPERSEDED => value.is_boolean(),
        KEY_AUTO_PAUSE_AFTER_HOURS => value.as_u64().is_some_and(|h| (1..=168).contains(&h)),
        KEY_CERT_LIFETIME_DAYS => value.as_u64().is_some_and(|d| {
            (u64::from(crate::config::MIN_CERT_LIFETIME_DAYS)
                ..=u64::from(crate::config::MAX_CERT_LIFETIME_DAYS))
                .contains(&d)
        }),
        KEY_RENEW_BEFORE_DAYS => value.as_u64().is_some_and(|d| (1..=180).contains(&d)),
        KEY_GRACE_DAYS => value
            .as_u64()
            .is_some_and(|d| d <= u64::from(crate::config::MAX_GRACE_DAYS)),
        _ => return Err(format!("{key} 不是可從主控台修改的設定")),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{key} 的值 {value} 不合法"))
    }
}

/// 計算實際生效的值。資料庫中不合法的值略過（以設定檔或預設為準）
pub fn effective(config: &AppConfig, overrides: &Overrides) -> Effective {
    let prov = &config.provenance;
    let mut sources = BTreeMap::new();
    // 環境變數 > 資料庫 > 檔案 > 預設：環境變數設定的鍵不採用資料庫的值
    let pick = |key: &'static str, sources: &mut BTreeMap<&'static str, SettingSource>| {
        let base = prov.source(key);
        if base != SettingSource::Environment
            && let Some(v) = overrides.get(key)
            && validate_override(key, v).is_ok()
        {
            sources.insert(key, SettingSource::Console);
            return Some(v.clone());
        }
        sources.insert(key, base);
        None
    };
    let mode = pick(KEY_MODE, &mut sources)
        .and_then(|v| v.as_str().and_then(EnrollMode::parse))
        .unwrap_or(config.enroll.effective_mode());
    let auto_pause_enabled = pick(KEY_AUTO_PAUSE_ENABLED, &mut sources)
        .and_then(|v| v.as_bool())
        .unwrap_or(config.enroll.auto_pause_enabled);
    let auto_pause_after_hours = pick(KEY_AUTO_PAUSE_AFTER_HOURS, &mut sources)
        .and_then(|v| v.as_u64())
        .map_or(config.enroll.auto_pause_after_hours, |h| h as u32);
    let p = &config.pki.policy;
    let u32_of = |v: Option<Value>, d: u32| v.and_then(|v| v.as_u64()).map_or(d, |x| x as u32);
    let bool_of = |v: Option<Value>, d: bool| v.and_then(|v| v.as_bool()).unwrap_or(d);
    let mut policy = CertPolicySettings {
        cert_lifetime_days: u32_of(
            pick(KEY_CERT_LIFETIME_DAYS, &mut sources),
            p.cert_lifetime_days,
        ),
        renew_before_days: u32_of(
            pick(KEY_RENEW_BEFORE_DAYS, &mut sources),
            p.renew_before_days,
        ),
        renew_requires_approval: bool_of(
            pick(KEY_RENEW_REQUIRES_APPROVAL, &mut sources),
            p.renew_requires_approval,
        ),
        expired_renew_grace_days: u32_of(
            pick(KEY_GRACE_DAYS, &mut sources),
            p.expired_renew_grace_days,
        ),
        revoke_on_self_decommission: bool_of(
            pick(KEY_REVOKE_ON_SELF_DECOMMISSION, &mut sources),
            p.revoke_on_self_decommission,
        ),
        revoke_superseded: bool_of(
            pick(KEY_REVOKE_SUPERSEDED, &mut sources),
            p.revoke_superseded,
        ),
    };
    // 兩個值各自合法但組合不合法（輪替天數不小於有效天數）時沿用設定檔的輪替天數
    if policy.renew_before_days >= policy.cert_lifetime_days {
        policy.renew_before_days = p.renew_before_days.min(policy.cert_lifetime_days - 1);
    }
    Effective {
        mode,
        auto_pause_enabled,
        auto_pause_after_hours,
        policy,
        sources,
    }
}

/// 從資料庫讀出的 `settings` 轉成覆寫表
pub fn overrides_from(rows: &[(String, Value, i64, String)]) -> Overrides {
    rows.iter()
        .filter(|(k, ..)| CONSOLE_KEYS.contains(&k.as_str()))
        .map(|(k, v, ..)| (k.clone(), v.clone()))
        .collect()
}

/// `kairos config show --effective`：所有設定鍵的實際值與來源（JSON 物件：鍵 → `{value, source}`）
pub fn show_effective(
    config: &AppConfig,
    overrides: &Overrides,
) -> BTreeMap<String, (Value, SettingSource)> {
    let mut out = BTreeMap::new();
    let mut flat = BTreeMap::new();
    flatten_values(
        "",
        &serde_json::to_value(config).unwrap_or(Value::Null),
        &mut flat,
    );
    for (k, v) in flat {
        let src = config.provenance.source(&k);
        out.insert(k, (v, src));
    }
    // 雙層預設：沒有設定時顯示實際的模式
    out.insert(
        KEY_MODE.to_string(),
        (
            json!(config.enroll.effective_mode().as_str()),
            config.provenance.source(KEY_MODE),
        ),
    );
    out.insert(
        "pki.mode".to_string(),
        (
            json!(config.pki.effective_mode().as_str()),
            config.provenance.source("pki.mode"),
        ),
    );
    let eff = effective(config, overrides);
    for key in CONSOLE_KEYS {
        let value = match key {
            KEY_MODE => json!(eff.mode.as_str()),
            KEY_AUTO_PAUSE_ENABLED => json!(eff.auto_pause_enabled),
            KEY_AUTO_PAUSE_AFTER_HOURS => json!(eff.auto_pause_after_hours),
            KEY_CERT_LIFETIME_DAYS => json!(eff.policy.cert_lifetime_days),
            KEY_RENEW_BEFORE_DAYS => json!(eff.policy.renew_before_days),
            KEY_RENEW_REQUIRES_APPROVAL => json!(eff.policy.renew_requires_approval),
            KEY_GRACE_DAYS => json!(eff.policy.expired_renew_grace_days),
            KEY_REVOKE_ON_SELF_DECOMMISSION => json!(eff.policy.revoke_on_self_decommission),
            _ => json!(eff.policy.revoke_superseded),
        };
        out.insert(key.to_string(), (value, eff.source(key)));
    }
    out
}

/// 一個設定鍵的實際生效值與來源（`cli-style.md` §11.7、API `GET /api/v1/settings`）
#[derive(Debug, Clone, PartialEq)]
pub struct SettingView {
    pub key: String,
    pub value: Value,
    pub source: SettingSource,
    /// 對應的環境變數名稱
    pub env_var: String,
    /// 主控台（資料庫）的修改者與時間
    pub updated_by: Option<String>,
    pub updated_at: Option<i64>,
    /// 較低優先層中與生效值不同的值：(來源, 值)
    pub overridden: Vec<(SettingSource, Value)>,
}

impl SettingView {
    /// `--json` 與 API 的寫法（來源為 `env`／`console`／`file`／`default`）
    pub fn json(&self) -> Value {
        json!({
            "key": self.key,
            "value": self.value,
            "source": short_source(self.source),
            "env_var": self.env_var,
            "updated_by": self.updated_by,
            "updated_at": self.updated_at.map(crate::api::http::ts_ms),
            "overridden": self.overridden.iter().map(|(s, v)| json!({"source": short_source(*s), "value": v})).collect::<Vec<_>>(),
            "pending_restart": null,
        })
    }
}

/// `env`／`console`／`file`／`default`
pub fn short_source(s: SettingSource) -> &'static str {
    match s {
        SettingSource::Environment => "env",
        SettingSource::Console => "console",
        SettingSource::File => "file",
        SettingSource::Default => "default",
    }
}

/// 所有設定鍵的生效值、來源、修改者與被覆寫的值。`rows` 為資料庫 `settings` 表的內容
pub fn effective_view(
    config: &AppConfig,
    rows: &[(String, Value, i64, String)],
) -> Vec<SettingView> {
    let overrides = overrides_from(rows);
    let all = show_effective(config, &overrides);
    let prov = &config.provenance;
    all.into_iter()
        .map(|(key, (value, source))| {
            let row = rows
                .iter()
                .find(|(k, ..)| *k == key && CONSOLE_KEYS.contains(&k.as_str()));
            let mut overridden = Vec::new();
            if source == SettingSource::Environment
                && let Some((_, v, ..)) = row
                && *v != value
            {
                overridden.push((SettingSource::Console, v.clone()));
            }
            if matches!(source, SettingSource::Environment | SettingSource::Console)
                && let Some(v) = prov.file_values.get(&key)
                && !same_value(v, &value)
            {
                overridden.push((SettingSource::File, v.clone()));
            }
            SettingView {
                env_var: crate::config::Provenance::env_var(&key),
                updated_by: (source == SettingSource::Console)
                    .then(|| row.map(|r| r.3.clone()))
                    .flatten(),
                updated_at: (source == SettingSource::Console)
                    .then(|| row.map(|r| r.2))
                    .flatten(),
                key,
                value,
                source,
                overridden,
            }
        })
        .collect()
}

/// 設定檔的值（toml 的整數、字串）與生效值相同
fn same_value(file: &Value, effective: &Value) -> bool {
    file == effective || file.as_str().is_some_and(|s| Some(s) == effective.as_str())
}

/// 啟動時記一行 `INFO`：被資料庫（主控台）覆寫的設定檔設定（設計包 INTEGRATION §27.4 #2）
pub fn overridden_file_settings(
    config: &AppConfig,
    rows: &[(String, Value, i64, String)],
) -> Vec<String> {
    effective_view(config, rows)
        .into_iter()
        .filter(|v| v.source == SettingSource::Console)
        .filter_map(|v| {
            v.overridden
                .iter()
                .find(|(s, _)| *s == SettingSource::File)
                .map(|(_, f)| {
                    format!(
                        "{}（設定檔 {} → 生效 {}，{}）",
                        v.key,
                        f,
                        v.value,
                        v.updated_by.clone().unwrap_or_default()
                    )
                })
        })
        .collect()
}

fn flatten_values(prefix: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_values(&key, v, out);
            }
        }
        v => {
            out.insert(prefix.to_string(), v.clone());
        }
    }
}

/// 主控台覆寫的設定（`settings` 表）與 `meta` 表的鍵值。SQLite 的實作在 `store/sqlite/enroll.rs`（`settings`）
/// 與 `store/sqlite/alerts.rs`（`meta`）
pub trait SettingsStore {
    /// `settings` 表的所有值：(鍵, JSON 值, 更新時間, 更新者)
    fn load_settings(&self) -> StoreResult<Vec<(String, Value, i64, String)>>;

    /// 寫入（覆寫）一個鍵
    fn set_setting(&self, key: &str, value: &Value, by: &str, now_ms: i64) -> StoreResult<()>;

    /// 刪除 `settings` 的鍵（改用設定檔的值）；回傳是否存在
    fn delete_setting(&self, key: &str) -> StoreResult<bool>;

    /// `meta` 表的一個鍵（沒有時為 `None`）
    fn get_meta(&self, key: &str) -> StoreResult<Option<String>>;

    /// 寫入 `meta` 表的一個鍵
    fn put_meta(&self, key: &str, value: &str) -> StoreResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Provenance;

    #[test]
    fn precedence_is_env_then_console_then_file_then_default() {
        let mut c = AppConfig::default();
        // 預設：關閉
        let e = effective(&c, &Overrides::new());
        assert_eq!(e.mode, EnrollMode::Closed);
        assert_eq!(e.source(KEY_MODE), SettingSource::Default);
        // 設定檔：開放
        c.enroll.mode = Some(EnrollMode::Open);
        c.provenance = Provenance {
            file_keys: [KEY_MODE.to_string()].into(),
            ..Default::default()
        };
        let e = effective(&c, &Overrides::new());
        assert_eq!(
            (e.mode, e.source(KEY_MODE)),
            (EnrollMode::Open, SettingSource::File)
        );
        // 主控台的值優先於設定檔
        let mut o = Overrides::new();
        o.insert(KEY_MODE.into(), json!("renew_only"));
        o.insert(KEY_GRACE_DAYS.into(), json!(30));
        o.insert(KEY_AUTO_PAUSE_AFTER_HOURS.into(), json!(500)); // 不合法：略過
        let e = effective(&c, &o);
        assert_eq!(
            (e.mode, e.source(KEY_MODE)),
            (EnrollMode::RenewOnly, SettingSource::Console)
        );
        assert_eq!(e.policy.expired_renew_grace_days, 30);
        assert_eq!(e.source(KEY_GRACE_DAYS), SettingSource::Console);
        assert_eq!(e.auto_pause_after_hours, 24);
        assert_eq!(e.source(KEY_AUTO_PAUSE_AFTER_HOURS), SettingSource::Default);
        // 環境變數優先於主控台
        c.provenance.env_keys.insert(KEY_MODE.to_string());
        let e = effective(&c, &o);
        assert_eq!(
            (e.mode, e.source(KEY_MODE)),
            (EnrollMode::Open, SettingSource::Environment)
        );
        assert_eq!(Provenance::env_var(KEY_MODE), "KAIROS__ENROLL__MODE");
    }

    #[test]
    fn overrides_are_validated() {
        assert!(validate_override(KEY_MODE, &json!("open")).is_ok());
        assert!(validate_override(KEY_MODE, &json!("off")).is_err());
        assert!(validate_override(KEY_CERT_LIFETIME_DAYS, &json!(30)).is_err());
        assert!(validate_override(KEY_CERT_LIFETIME_DAYS, &json!(825)).is_ok());
        assert!(validate_override("server.listen", &json!("x")).is_err());
        assert!(validate_override(KEY_RENEW_REQUIRES_APPROVAL, &json!(1)).is_err());
    }

    #[test]
    fn effective_view_reports_modifier_and_overridden_file_value() {
        let mut c = AppConfig::default();
        c.enroll.mode = Some(EnrollMode::Open);
        c.provenance.file_keys.insert(KEY_MODE.into());
        c.provenance
            .file_values
            .insert(KEY_MODE.into(), json!("open"));
        let rows = vec![(
            KEY_MODE.to_string(),
            json!("renew_only"),
            1_760_000_000_000,
            "admin.chen".to_string(),
        )];
        let view = effective_view(&c, &rows);
        let m = view.iter().find(|v| v.key == KEY_MODE).unwrap();
        assert_eq!(m.value, json!("renew_only"));
        assert_eq!(m.source, SettingSource::Console);
        assert_eq!(m.updated_by.as_deref(), Some("admin.chen"));
        assert_eq!(m.overridden, vec![(SettingSource::File, json!("open"))]);
        assert_eq!(m.json()["source"], "console");
        let o = overridden_file_settings(&c, &rows);
        assert_eq!(o.len(), 1);
        assert!(o[0].starts_with("enroll.mode"), "{o:?}");
        // 設定檔與生效值相同時沒有被覆寫的值
        let listen = view.iter().find(|v| v.key == "server.listen").unwrap();
        assert!(listen.overridden.is_empty());
    }

    #[test]
    fn show_effective_lists_every_key_with_its_source() {
        let mut c = AppConfig::default();
        c.server.listen = "0.0.0.0:8443".into();
        c.provenance.file_keys.insert("server.listen".into());
        let mut o = Overrides::new();
        o.insert(KEY_MODE.into(), json!("open"));
        let all = show_effective(&c, &o);
        assert_eq!(
            all["server.listen"],
            (json!("0.0.0.0:8443"), SettingSource::File)
        );
        assert_eq!(all["server.max_connections"].1, SettingSource::Default);
        assert_eq!(all[KEY_MODE], (json!("open"), SettingSource::Console));
        assert_eq!(all["pki.mode"].0, json!("offline"));
        assert!(all.contains_key("enroll.max_requests_per_ip_per_minute"));
        assert!(!all.contains_key(KEY_OPEN_SINCE));
    }
}
