//! 遠端管理（商業審計 F8，規格 [`REMOTE.md`](../../REMOTE.md) 第 2 節，已搬入 PROTOCOL.md「遠端管理」）的
//! 8443 協定部分：feature [`FEATURE_CONFIG`]（集中設定）與 [`FEATURE_UPGRADE`]（遠端升級）。
//!
//! - 兩個 feature 都只在協商結果為 v2、雙方都宣告時使用（[`MgmtSession`]）；Kairos 另外要求 SQLite 儲存、
//!   mTLS 與本機開關（[`ServerMgmtPolicy`]），且只在 client 也宣告時才列出（沿用 `renew` 的先例），
//!   因此 0.8 以前、或沒有宣告的一端收到的訊息與 0.8 完全相同。
//! - hello 另帶選用的 `install`（[`InstallInfo`]，放在 [`HostInfo`](crate::HostInfo)），不論是否宣告都送；
//!   `hello_ack` 與回應 `heartbeat` 的 `ack` 另帶 `pending`（[`Pending`]）。
//! - 新訊息：client 的 [`ConfigGet`]、[`ConfigStatus`]、[`UpgradeGet`]、[`UpgradeChunkGet`]、[`UpgradeStatus`]
//!   （[`ClientMsg`](crate::ClientMsg) 的對應變體），server 的 [`RemoteConfig`]（`type: "config"`）、
//!   [`UpgradeOffer`]、[`UpgradeChunk`]（[`ServerMsg`](crate::ServerMsg) 的對應變體）。
//! - 設定白名單（兩端共用一份鍵、型別、範圍、套用方式、隱私規則）：[`settings`] 模組。
//! - 發佈包的簽章、雜湊、檔名、清單與版本政策：[`release`](crate::release) 模組。
//!
//! 本模組只有型別與純函式；兩端的行為（何時送、狀態機、儲存）由 Kairos／Pulse 實作。

pub mod settings;
pub mod url;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

use crate::validate::validate_display_text;
use crate::{PROTO_V2, is_uuid_text};

// ---- feature 與協商 ----

/// 集中設定：`pending.config_rev`、`config_get`／`config`／`config_status`
pub const FEATURE_CONFIG: &str = "config";
/// 遠端升級：`pending.upgrade_id`、`upgrade_get`／`upgrade_offer`、`upgrade_chunk_get`／`upgrade_chunk`、
/// `upgrade_status`
pub const FEATURE_UPGRADE: &str = "upgrade";
/// 遠端管理的 feature。Kairos 只在 client 也宣告時才在 `hello_ack` 列出
pub const MGMT_FEATURES: [&str; 2] = [FEATURE_CONFIG, FEATURE_UPGRADE];

/// 遠端管理新增的 client 訊息 `type`。沒有協商對應的 feature 時，Kairos 一律回 `unknown_type`
/// （欄位不合法時也是，與 0.8 相同；比照 `renew_request` 以 [`message_type`](crate::message_type) 判斷）
pub const MGMT_CLIENT_MSG_TYPES: [&str; 5] = [
    "config_get",
    "config_status",
    "upgrade_get",
    "upgrade_chunk_get",
    "upgrade_status",
];

/// 這條連線上遠端管理 feature 的協商結果（兩端各自依 hello／hello_ack 算出，結果相同）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MgmtSession {
    /// 協商了 [`FEATURE_CONFIG`]
    pub config: bool,
    /// 協商了 [`FEATURE_UPGRADE`]
    pub upgrade: bool,
}

impl MgmtSession {
    /// 兩個都沒有協商（v1、舊版的任一端、或沒有宣告）
    pub const NONE: MgmtSession = MgmtSession {
        config: false,
        upgrade: false,
    };

    /// 依協商出的版本與雙方宣告的 features 建立：v2 且雙方都宣告才使用。
    /// Kairos 端的 `ours` 是它實際列在 `hello_ack` 的 features（[`ServerMgmtPolicy::features_to_list`]）
    pub fn new<A: AsRef<str>, B: AsRef<str>>(proto: u32, ours: &[A], theirs: &[B]) -> Self {
        let both = |feature: &str| {
            proto >= PROTO_V2
                && ours.iter().any(|f| f.as_ref() == feature)
                && theirs.iter().any(|f| f.as_ref() == feature)
        };
        MgmtSession {
            config: both(FEATURE_CONFIG),
            upgrade: both(FEATURE_UPGRADE),
        }
    }

    /// 至少協商了一個（`hello_ack` 與回應 `heartbeat` 的 `ack` 因此帶 `pending`）
    pub fn any(&self) -> bool {
        self.config || self.upgrade
    }

    /// 這個 client 訊息 `type` 在這條連線上是否可用；`false` 時 Kairos 回 `unknown_type`
    pub fn accepts_client_type(&self, kind: &str) -> bool {
        match kind {
            "config_get" | "config_status" => self.config,
            "upgrade_get" | "upgrade_chunk_get" | "upgrade_status" => self.upgrade,
            _ => false,
        }
    }
}

/// Kairos 端列出遠端管理 feature 的條件（REMOTE.md 第 2 節的表）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerMgmtPolicy {
    /// `[remote] config_enabled`（預設 true）
    pub config_enabled: bool,
    /// `[remote] upgrade_enabled`（預設 true）
    pub upgrade_enabled: bool,
    /// 以 SQLite 儲存（JSONL 時不列出）
    pub sqlite: bool,
    /// 這條連線以用戶端憑證（mTLS）證明了 `agent_id`
    pub mtls: bool,
}

impl ServerMgmtPolicy {
    /// 要列在 `hello_ack.features` 的遠端管理 feature：協商結果為 v2、條件成立且 client 也宣告的才列出
    pub fn features_to_list<B: AsRef<str>>(
        &self,
        proto: u32,
        client_features: &[B],
    ) -> Vec<&'static str> {
        if proto < PROTO_V2 || !self.sqlite || !self.mtls {
            return Vec::new();
        }
        let declared = |f: &str| client_features.iter().any(|c| c.as_ref() == f);
        let mut out = Vec::new();
        if self.config_enabled && declared(FEATURE_CONFIG) {
            out.push(FEATURE_CONFIG);
        }
        if self.upgrade_enabled && declared(FEATURE_UPGRADE) {
            out.push(FEATURE_UPGRADE);
        }
        out
    }
}

/// Pulse 端要宣告的遠端管理 feature（REMOTE.md 第 2 節的表）：
/// - `config`：`remote_config.enabled`（預設 true）；停用後的第一次連線也要宣告一次，以送出
///   `state: "disabled"` 的 `config_status`（由呼叫端決定 `declare_config`）
/// - `upgrade`：`install.upgrade` 為 [`INSTALL_UPGRADE_READY`] 時（`upgrade.enabled` 為 false 時
///   `install.upgrade` 是 `disabled`，因此不宣告）
pub fn client_mgmt_features(declare_config: bool, install: &InstallInfo) -> Vec<&'static str> {
    let mut out = Vec::new();
    if declare_config {
        out.push(FEATURE_CONFIG);
    }
    if install.upgrade.as_deref() == Some(INSTALL_UPGRADE_READY) {
        out.push(FEATURE_UPGRADE);
    }
    out
}

// ---- hello.install ----

/// `install.method`：安裝方式（偵測方式見 REMOTE.md 第 4.3 節）
pub const INSTALL_METHOD_TAR: &str = "tar";
pub const INSTALL_METHOD_DEB: &str = "deb";
pub const INSTALL_METHOD_RPM: &str = "rpm";
pub const INSTALL_METHOD_MSI: &str = "msi";
pub const INSTALL_METHOD_ZIP: &str = "zip";
pub const INSTALL_METHOD_UNKNOWN: &str = "unknown";
/// 規格定義的 `install.method`；其他值 Kairos 原樣保存、顯示為「其他」
pub const INSTALL_METHODS: [&str; 6] = [
    INSTALL_METHOD_TAR,
    INSTALL_METHOD_DEB,
    INSTALL_METHOD_RPM,
    INSTALL_METHOD_MSI,
    INSTALL_METHOD_ZIP,
    INSTALL_METHOD_UNKNOWN,
];

/// `install.upgrade`：可以接受遠端升級（只有這個值時才宣告 `upgrade`）
pub const INSTALL_UPGRADE_READY: &str = "ready";
/// `install.upgrade`：本機 `upgrade.enabled = false`
pub const INSTALL_UPGRADE_DISABLED: &str = "disabled";
/// `install.upgrade`：找不到或無法使用信任錨 `allowed_signers`（`PLS-E8403`）
pub const INSTALL_UPGRADE_NO_TRUST_ANCHOR: &str = "no_trust_anchor";
/// `install.upgrade`：Linux 沒有啟用 `pulse-upgrade.path`
pub const INSTALL_UPGRADE_NO_HELPER: &str = "no_helper";
/// `install.upgrade`：多實例、`unknown` 安裝方式或不支援的平台
pub const INSTALL_UPGRADE_UNSUPPORTED: &str = "unsupported";
/// 規格定義的 `install.upgrade`
pub const INSTALL_UPGRADE_STATES: [&str; 5] = [
    INSTALL_UPGRADE_READY,
    INSTALL_UPGRADE_DISABLED,
    INSTALL_UPGRADE_NO_TRUST_ANCHOR,
    INSTALL_UPGRADE_NO_HELPER,
    INSTALL_UPGRADE_UNSUPPORTED,
];

/// `install.config`：集中設定在本機啟用
pub const INSTALL_CONFIG_ENABLED: &str = "enabled";
/// `install.config`：本機 `remote_config.enabled = false`
pub const INSTALL_CONFIG_DISABLED: &str = "disabled";

/// `install` 每個欄位的長度上限（bytes）
pub const MAX_INSTALL_FIELD_BYTES: usize = 32;

/// hello 的 `install`（0.9.0 起的 Pulse，不論是否宣告 `config`／`upgrade` 都送；v2 選用欄位）。
/// 讓主控台區分「Pulse 太舊」「本機停用」「缺少輔助程式」等原因。物件中的其他欄位忽略
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallInfo {
    /// [`INSTALL_METHODS`] 之一
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// [`INSTALL_UPGRADE_STATES`] 之一
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<String>,
    /// [`INSTALL_CONFIG_ENABLED`] 或 [`INSTALL_CONFIG_DISABLED`]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
}

/// `install` 欄位值的格式：1～32 bytes 的小寫英文字母、數字或 `_`
pub fn is_install_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_INSTALL_FIELD_BYTES
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// `install` 的限制（違反時 `bad_request`，與 `os` 相同；協商結果為 v1 時不檢查）。
/// 不認得的值不是錯誤（Kairos 原樣保存）
pub fn validate_install(install: &InstallInfo) -> Result<(), String> {
    for (field, value) in [
        ("install.method", &install.method),
        ("install.upgrade", &install.upgrade),
        ("install.config", &install.config),
    ] {
        if let Some(v) = value
            && !is_install_token(v)
        {
            return Err(format!(
                "{field} must be 1..={MAX_INSTALL_FIELD_BYTES} bytes of lowercase letters, digits or _"
            ));
        }
    }
    Ok(())
}

// ---- pending ----

/// `hello_ack` 與回應 `heartbeat` 的 `ack` 中的 `pending`（協商了 `config` 或 `upgrade` 時才帶）。
/// 欄位型別不符時 Pulse 視為沒有該欄位（與 `renew_now` 相同的容錯）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// 協商了 `config` 時才有：這台主機目前應套用的設定版本；`0` 表示沒有任何下發設定
    #[serde(
        default,
        deserialize_with = "de_lenient_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub config_rev: Option<u64>,
    /// 協商了 `upgrade` 且有提議給這台主機時才有：升級 ID（UUID，解析時轉成小寫；不是 UUID 時視為沒有）
    #[serde(
        default,
        deserialize_with = "de_lenient_uuid",
        skip_serializing_if = "Option::is_none"
    )]
    pub upgrade_id: Option<String>,
}

impl Pending {
    /// Kairos 端：依協商結果組出 `pending`。兩個 feature 都沒有協商時為 `None`（不帶 `pending`）；
    /// `config_rev` 只在協商了 `config` 時帶，`upgrade_id` 只在協商了 `upgrade` 且有提議時帶
    pub fn for_session(
        session: MgmtSession,
        config_rev: u64,
        upgrade_id: Option<&str>,
    ) -> Option<Pending> {
        session.any().then(|| Pending {
            config_rev: session.config.then_some(config_rev),
            upgrade_id: if session.upgrade {
                upgrade_id.map(str::to_string)
            } else {
                None
            },
        })
    }

    /// Pulse 端：`config_rev` 與本機已套用的版本不同時要送 `config_get`（沒有 `config_rev` 時不送）
    pub fn needs_config_get(&self, applied_rev: u64) -> bool {
        self.config_rev.is_some_and(|rev| rev != applied_rev)
    }

    /// Pulse 端：不認得的 `upgrade_id`（不是正在處理、也不是已完成的）時回傳它，要送 `upgrade_get`
    pub fn new_upgrade_id<S: AsRef<str>>(&self, known: &[S]) -> Option<&str> {
        self.upgrade_id
            .as_deref()
            .filter(|id| !known.iter().any(|k| k.as_ref().eq_ignore_ascii_case(id)))
    }
}

fn de_lenient_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(Value::deserialize(d)?.as_u64())
}

fn de_lenient_uuid<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) if is_uuid_text(&s) => Some(s.to_ascii_lowercase()),
        _ => None,
    })
}

/// 寬鬆的 `pending`：不是物件時視為沒有（整則 `hello_ack`／`ack` 仍可解析）
pub(crate) fn de_lenient_pending<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Pending>, D::Error> {
    Ok(match Value::deserialize(d)? {
        v @ Value::Object(_) => Pending::deserialize(v).ok(),
        _ => None,
    })
}

// ---- 共用的格式檢查 ----

/// 產品代碼（`PLS-W0501`、`KRS-E8413`）：`PLS` 或 `KRS`、`-`、`E`／`W`／`I`、4 位數字
pub fn is_product_code(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 9
        && (s.starts_with("PLS-") || s.starts_with("KRS-"))
        && matches!(b[4], b'E' | b'W' | b'I')
        && b[5..].iter().all(u8::is_ascii_digit)
}

/// 64 個小寫十六進位字元（SHA-256）
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 設定的雜湊：`sha256:` 加 64 個小寫十六進位字元
pub fn is_config_hash(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(is_sha256_hex)
}

fn check_code(code: &Option<String>) -> Result<(), String> {
    match code {
        Some(c) if !is_product_code(c) => {
            Err("code must be null or a product code such as PLS-W0501".to_string())
        }
        _ => Ok(()),
    }
}

fn check_uuid(field: &str, value: &str) -> Result<(), String> {
    if is_uuid_text(value) {
        Ok(())
    } else {
        Err(format!("{field} must be a UUID (8-4-4-4-12 hex digits)"))
    }
}

/// 可以是空字串的顯示用字串（`detail` 等）
fn check_optional_text(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.is_empty() {
        Ok(())
    } else {
        validate_display_text(field, value, max)
    }
}

// ---- 集中設定的訊息 ----

/// `config_status.state`：已套用（含部分鍵被本機覆蓋或忽略）
pub const CONFIG_STATE_APPLIED: &str = "applied";
/// `config_status.state`：驗證失敗，整份不套用，沿用上一版（`PLS-W0501`）
pub const CONFIG_STATE_REJECTED: &str = "rejected";
/// `config_status.state`：套用後健康確認失敗，已回復上一版（`PLS-W0504`）
pub const CONFIG_STATE_ROLLED_BACK: &str = "rolled_back";
/// `config_status.state`：本機停用集中設定（只在停用後的第一次連線送一次）
pub const CONFIG_STATE_DISABLED: &str = "disabled";
/// 規格定義的 `config_status.state`
pub const CONFIG_STATES: [&str; 4] = [
    CONFIG_STATE_APPLIED,
    CONFIG_STATE_REJECTED,
    CONFIG_STATE_ROLLED_BACK,
    CONFIG_STATE_DISABLED,
];

/// `ignored[].reason`：不在白名單（含永遠只能在本機改的鍵與值，REMOTE.md 第 3.2 節）
pub const IGNORED_UNKNOWN_KEY: &str = "unknown_key";
/// `ignored[].reason`：這個平台不支援
pub const IGNORED_UNSUPPORTED_PLATFORM: &str = "unsupported_platform";
/// `ignored[].reason`：隱私敏感的鍵下發了比本機（含內建預設）不保守的值
pub const IGNORED_NOT_MORE_RESTRICTIVE: &str = "not_more_restrictive";
/// 陣列超過 [`MAX_STATUS_ITEMS`] 時最後一項的 `reason`（`key` 為 `*`）
pub const REASON_TRUNCATED: &str = "truncated";

/// `overridden[].source`：本機 `pulse.toml`
pub const OVERRIDE_SOURCE_FILE: &str = "pulse.toml";
/// `overridden[].source`：`PULSE__*` 環境變數
pub const OVERRIDE_SOURCE_ENV: &str = "env";

/// `config_status` 的 `overridden`、`ignored`、`errors` 各自的項目上限（含截斷標記）
pub const MAX_STATUS_ITEMS: usize = 64;
/// `errors[].message` 的長度上限（bytes）
pub const MAX_STATUS_MESSAGE_BYTES: usize = 256;

/// `config_get`（Pulse → Kairos）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigGet {
    pub seq: u64,
    pub ts: String,
    /// 本機已套用的版本，沒有時 `0`
    pub have_rev: u64,
}

/// `config`（Kairos → Pulse），回應 `config_get`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// 回應的 `config_get` 的 `seq`
    pub seq: u64,
    pub rev: u64,
    /// [`settings_hash`]（`settings` 正規化 JSON 的 SHA-256）
    pub hash: String,
    /// 兩層物件 `{"<區段>": {"<鍵>": <值>}}`，只有白名單中的鍵（[`settings`]）
    pub settings: Map<String, Value>,
    /// 鍵（`report.interval`）→ 設定檔名稱，供本機顯示來源；選用
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<BTreeMap<String, String>>,
    /// RFC 3339 UTC 毫秒
    pub issued_at: String,
}

impl RemoteConfig {
    /// Kairos 端：以 `settings` 計算 `hash` 後組出訊息
    pub fn new(
        seq: u64,
        rev: u64,
        settings: Map<String, Value>,
        sources: Option<BTreeMap<String, String>>,
        issued_at: impl Into<String>,
    ) -> Self {
        Self {
            seq,
            rev,
            hash: settings_hash(&settings),
            settings,
            sources,
            issued_at: issued_at.into(),
        }
    }
}

/// `overridden` 的一項：被本機覆蓋的鍵
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverriddenKey {
    /// 例如 `report.interval`
    pub key: String,
    pub remote: Value,
    pub local: Value,
    /// [`OVERRIDE_SOURCE_FILE`] 或 [`OVERRIDE_SOURCE_ENV`]
    pub source: String,
}

/// `ignored` 的一項：沒有套用、但不影響其他鍵
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoredKey {
    pub key: String,
    /// [`IGNORED_UNKNOWN_KEY`]、[`IGNORED_UNSUPPORTED_PLATFORM`]、[`IGNORED_NOT_MORE_RESTRICTIVE`]、
    /// [`REASON_TRUNCATED`]
    pub reason: String,
}

/// `errors` 的一項（`state` 為 `rejected`／`rolled_back` 時的原因）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyError {
    pub key: String,
    /// [`settings::ERR_TYPE_MISMATCH`] 等
    pub reason: String,
    /// ≤ 256 bytes，字元限制同 `hostname`；沒有時空字串
    #[serde(default)]
    pub message: String,
}

/// `config_status`（Pulse → Kairos），回 `ack`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigStatus {
    pub seq: u64,
    pub ts: String,
    /// 這次處理的版本；`disabled` 時為本機最後一次套用的版本（沒有時 `0`）
    pub rev: u64,
    /// 同上（沒有時空字串）
    pub hash: String,
    /// [`CONFIG_STATES`] 之一
    pub state: String,
    /// 白名單中每個鍵實際生效的值（與 `settings` 同一種兩層格式）
    #[serde(default)]
    pub effective: Map<String, Value>,
    #[serde(default)]
    pub overridden: Vec<OverriddenKey>,
    #[serde(default)]
    pub ignored: Vec<IgnoredKey>,
    #[serde(default)]
    pub errors: Vec<KeyError>,
    /// 對應的 Pulse 代碼（例如 `PLS-W0501`），沒有時 `null`
    #[serde(default)]
    pub code: Option<String>,
}

impl ConfigStatus {
    /// 把超過 [`MAX_STATUS_ITEMS`] 的 `ignored`、`errors` 截斷，最後一項換成
    /// `{"key":"*","reason":"truncated"}`。`overridden` 的項目數不會超過白名單的鍵數，不需截斷
    pub fn truncate_lists(&mut self) {
        fn cut<T>(v: &mut Vec<T>, marker: impl FnOnce() -> T) {
            if v.len() > MAX_STATUS_ITEMS {
                v.truncate(MAX_STATUS_ITEMS - 1);
                v.push(marker());
            }
        }
        cut(&mut self.ignored, || IgnoredKey {
            key: "*".into(),
            reason: REASON_TRUNCATED.into(),
        });
        cut(&mut self.errors, || KeyError {
            key: "*".into(),
            reason: REASON_TRUNCATED.into(),
            message: String::new(),
        });
    }
}

/// `config_status` 的限制（Kairos 端；違反時 `bad_request`）
pub fn validate_config_status(s: &ConfigStatus) -> Result<(), String> {
    if !CONFIG_STATES.contains(&s.state.as_str()) {
        return Err(format!("state must be one of {}", CONFIG_STATES.join(", ")));
    }
    if !(is_config_hash(&s.hash) || (s.hash.is_empty() && s.rev == 0)) {
        return Err(
            "hash must be sha256:<64 lowercase hex digits> (empty only when rev is 0)".to_string(),
        );
    }
    settings::check_shape(&s.effective).map_err(|e| format!("effective: {e}"))?;
    for (field, len) in [
        ("overridden", s.overridden.len()),
        ("ignored", s.ignored.len()),
        ("errors", s.errors.len()),
    ] {
        if len > MAX_STATUS_ITEMS {
            return Err(format!(
                "{field} must have at most {MAX_STATUS_ITEMS} entries (got {len})"
            ));
        }
    }
    let keys = s
        .overridden
        .iter()
        .map(|o| (o.key.as_str(), o.source.as_str()))
        .chain(
            s.ignored
                .iter()
                .map(|i| (i.key.as_str(), i.reason.as_str())),
        )
        .chain(s.errors.iter().map(|e| (e.key.as_str(), e.reason.as_str())));
    for (key, tag) in keys {
        validate_display_text("key", key, settings::MAX_SETTING_KEY_BYTES)?;
        validate_display_text("reason/source", tag, MAX_INSTALL_FIELD_BYTES)?;
    }
    for e in &s.errors {
        check_optional_text("errors[].message", &e.message, MAX_STATUS_MESSAGE_BYTES)?;
    }
    check_code(&s.code)
}

/// `config` 的限制（Pulse 端；違反時當作沒有收到，記 WARN）：`hash` 必須等於 `settings` 的
/// [`settings_hash`]，`settings` 必須是兩層物件。鍵與值是否合法由 [`settings::check_remote`] 判斷
/// （不合法的值造成整份拒絕，不是協定錯誤）
pub fn validate_remote_config(c: &RemoteConfig) -> Result<(), String> {
    settings::check_shape(&c.settings).map_err(|e| format!("settings: {e}"))?;
    if c.hash != settings_hash(&c.settings) {
        return Err("hash does not match the canonical JSON of settings".to_string());
    }
    crate::enroll::parse_rfc3339_ms(&c.issued_at).map_err(|e| format!("issued_at {e}"))?;
    Ok(())
}

/// 正規化 JSON（REMOTE.md 第 3.4.4 節）：物件的鍵依字典序（UTF-8 位元組順序）排列、沒有任何空白、
/// 數字照 serde_json 的輸出、字串依 serde_json 的跳脫規則（`"`、`\`、控制字元，其餘原樣）。
/// 不依賴 serde_json 的 `preserve_order` 特性是否被其他套件啟用
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&map[k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(v, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// 設定的雜湊：`sha256:` 加 `settings` 正規化 JSON（[`canonical_json`]）的 SHA-256 小寫十六進位
pub fn settings_hash(settings: &Map<String, Value>) -> String {
    format!(
        "sha256:{}",
        crate::enroll::sha256_hex(canonical_json(&Value::Object(settings.clone())).as_bytes())
    )
}

// ---- 遠端升級的訊息 ----

/// `upgrade_offer.action`
pub const ACTION_UPGRADE: &str = "upgrade";
pub const ACTION_DOWNGRADE: &str = "downgrade";

/// `artifact.source`：以 `upgrade_chunk_get` 從這條連線下載（0.9.0 的預設，也是唯一實作的方式）
pub const SOURCE_KAIROS: &str = "kairos";
/// `artifact.source`：從 `url` 以 HTTPS 下載（外部 URL，0.10.0；REMOTE.md 第 14 節 D15）
pub const SOURCE_URL: &str = "url";

/// `upgrade_status.state`
pub const UPGRADE_STATE_ACCEPTED: &str = "accepted";
pub const UPGRADE_STATE_DECLINED: &str = "declined";
pub const UPGRADE_STATE_STAGED: &str = "staged";
pub const UPGRADE_STATE_EXPIRED: &str = "expired";
pub const UPGRADE_STATE_APPLYING: &str = "applying";
pub const UPGRADE_STATE_SUCCEEDED: &str = "succeeded";
pub const UPGRADE_STATE_FAILED: &str = "failed";
pub const UPGRADE_STATE_ROLLED_BACK: &str = "rolled_back";
pub const UPGRADE_STATE_ROLLBACK_FAILED: &str = "rollback_failed";
/// 規格定義的 `upgrade_status.state`
pub const UPGRADE_STATES: [&str; 9] = [
    UPGRADE_STATE_ACCEPTED,
    UPGRADE_STATE_DECLINED,
    UPGRADE_STATE_STAGED,
    UPGRADE_STATE_EXPIRED,
    UPGRADE_STATE_APPLYING,
    UPGRADE_STATE_SUCCEEDED,
    UPGRADE_STATE_FAILED,
    UPGRADE_STATE_ROLLED_BACK,
    UPGRADE_STATE_ROLLBACK_FAILED,
];

/// 這次升級在主機上已經結束（之後不會再有同一個 `upgrade_id` 的狀態）
pub fn is_terminal_upgrade_state(state: &str) -> bool {
    matches!(
        state,
        UPGRADE_STATE_DECLINED
            | UPGRADE_STATE_EXPIRED
            | UPGRADE_STATE_SUCCEEDED
            | UPGRADE_STATE_FAILED
            | UPGRADE_STATE_ROLLED_BACK
            | UPGRADE_STATE_ROLLBACK_FAILED
    )
}

/// 發佈檔的大小上限（bytes，128 MiB）
pub const MAX_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
/// `manifest.content` 的上限（bytes）
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024;
/// `sha256sums` 的上限（bytes）
pub const MAX_SHA256SUMS_BYTES: usize = 16 * 1024;
/// `signature` 的上限（bytes）
pub const MAX_SIGNATURE_BYTES: usize = 4 * 1024;
/// `artifact.url` 的上限（bytes）
pub const MAX_ARTIFACT_URL_BYTES: usize = 2048;
/// `rollout_id` 的上限（bytes）
pub const MAX_ROLLOUT_ID_BYTES: usize = 64;
/// 每段的上限（bytes；base64 後約 43.7 KB，加上 JSON 不超過單行上限）
pub const MAX_CHUNK_BYTES: u32 = 32768;
/// Pulse 同一時間最多在途的段落請求（另計，不占 durable report 的在途筆數）
pub const MAX_CHUNKS_IN_FLIGHT: usize = 2;
/// 下載完成後 SHA-256 連續不符幾次就放棄（`PLS-E8402`）
pub const MAX_DOWNLOAD_HASH_MISMATCHES: u32 = 3;
/// `[remote] max_concurrent_downloads` 的預設值（台）
pub const DEFAULT_MAX_CONCURRENT_DOWNLOADS: u32 = 100;
/// 一台主機超過這麼久沒有請求下一段時釋出下載名額（秒）
pub const DOWNLOAD_IDLE_RELEASE_SECS: u64 = 600;
/// `health_timeout_secs` 的範圍與 Kairos 的預設值（秒）
pub const MIN_HEALTH_TIMEOUT_SECS: u32 = 120;
pub const MAX_HEALTH_TIMEOUT_SECS: u32 = 3600;
pub const DEFAULT_HEALTH_TIMEOUT_SECS: u32 = 600;
/// `upgrade_status.detail` 的上限（bytes）
pub const MAX_UPGRADE_DETAIL_BYTES: usize = 512;
/// 暫存需要的額外空間（bytes）：`3 × artifact.size + 64 MiB`（[`staging_space_needed`]）
pub const STAGING_EXTRA_BYTES: u64 = 64 * 1024 * 1024;

/// 暫存發佈檔、回滾檔與解開的執行檔需要的空間：`3 × artifact.size + 64 MiB`；不足時 `declined`
/// （`PLS-W8412`）
pub fn staging_space_needed(artifact_size: u64) -> u64 {
    artifact_size
        .saturating_mul(3)
        .saturating_add(STAGING_EXTRA_BYTES)
}

/// 實際使用的健康確認期限（秒）：本機 `upgrade.health_timeout_secs` 為 0 時用 Kairos 的值，
/// 非 0 時取兩者較大；結果夾在 120～3600
pub fn effective_health_timeout_secs(offered: u32, local: u32) -> u32 {
    let secs = if local == 0 {
        offered
    } else {
        offered.max(local)
    };
    secs.clamp(MIN_HEALTH_TIMEOUT_SECS, MAX_HEALTH_TIMEOUT_SECS)
}

/// `upgrade_get`（Pulse → Kairos）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeGet {
    pub seq: u64,
    pub ts: String,
    pub upgrade_id: String,
}

/// `upgrade_offer.target`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeTarget {
    pub version: String,
    pub commit: String,
}

/// `upgrade_offer` 的 `artifact`／`rollback_artifact`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// 發佈檔名 `pulse-<版本>-<commit>-<os>-<arch>.<格式>`（[`ReleaseFile`](crate::release::ReleaseFile)）
    pub filename: String,
    /// bytes，上限 [`MAX_ARTIFACT_BYTES`]
    pub size: u64,
    /// 64 個小寫十六進位
    pub sha256: String,
    /// [`SOURCE_KAIROS`] 或 [`SOURCE_URL`]
    pub source: String,
    /// `source` 為 `url` 時：`https://` 開頭、≤ 2048 bytes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// `upgrade_offer.manifest`：發佈清單的檔名與內容（JSON 字串）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRef {
    pub filename: String,
    pub content: String,
}

/// `upgrade_offer.apply_window`：Pulse 只在這段期間開始替換（下載與驗證可以提早）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyWindow {
    pub not_before: String,
    pub not_after: String,
}

impl ApplyWindow {
    /// `now_ms` 是否在時段內（兩端都含）；時間無法解析時 `false`
    pub fn contains(&self, now_ms: i64) -> bool {
        match self.bounds() {
            Some((a, b)) => (a..=b).contains(&now_ms),
            None => false,
        }
    }

    /// 是否已超過 `not_after`（Pulse 放棄並回報 `expired`）；時間無法解析時 `true`
    pub fn has_expired(&self, now_ms: i64) -> bool {
        self.bounds().is_none_or(|(_, b)| now_ms > b)
    }

    fn bounds(&self) -> Option<(i64, i64)> {
        let a = crate::enroll::parse_rfc3339_ms(&self.not_before).ok()?;
        let b = crate::enroll::parse_rfc3339_ms(&self.not_after).ok()?;
        Some((a, b))
    }
}

/// `upgrade_offer`（Kairos → Pulse），回應 `upgrade_get`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeOffer {
    /// 回應的 `upgrade_get` 的 `seq`
    pub seq: u64,
    pub upgrade_id: String,
    pub rollout_id: String,
    /// [`ACTION_UPGRADE`] 或 [`ACTION_DOWNGRADE`]
    pub action: String,
    pub target: UpgradeTarget,
    pub artifact: ArtifactRef,
    /// deb／rpm／msi 的主機才有：目前版本、同格式同平台的發佈檔（自動回滾用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_artifact: Option<ArtifactRef>,
    /// 0.9.0 以前的發佈沒有清單：`null`（只能作為降版或回滾目標）
    pub manifest: Option<ManifestRef>,
    /// 該次發佈的 `SHA256SUMS` 全文
    pub sha256sums: String,
    /// `SHA256SUMS.sig` 全文（`-----BEGIN SSH SIGNATURE-----`）
    pub signature: String,
    pub apply_window: ApplyWindow,
    /// 120～3600
    pub health_timeout_secs: u32,
    pub allow_downgrade: bool,
}

impl UpgradeOffer {
    /// `upgrade_chunk_get` 的 `filename` 只能是這次提議的 `artifact` 或 `rollback_artifact`
    /// （否則 Kairos 回 `upgrade_unavailable`）
    pub fn file(&self, filename: &str) -> Option<&ArtifactRef> {
        std::iter::once(&self.artifact)
            .chain(self.rollback_artifact.as_ref())
            .find(|a| a.filename == filename)
    }
}

fn check_artifact(field: &str, a: &ArtifactRef) -> Result<(), String> {
    crate::release::ReleaseFile::parse(&a.filename)
        .map_err(|e| format!("{field}.filename: {e}"))?;
    if a.size == 0 || a.size > MAX_ARTIFACT_BYTES {
        return Err(format!(
            "{field}.size must be 1..={MAX_ARTIFACT_BYTES} (got {})",
            a.size
        ));
    }
    if !is_sha256_hex(&a.sha256) {
        return Err(format!("{field}.sha256 must be 64 lowercase hex digits"));
    }
    match (a.source.as_str(), &a.url) {
        (SOURCE_KAIROS, None) => Ok(()),
        (SOURCE_URL, Some(u))
            if u.starts_with("https://")
                && u.len() <= MAX_ARTIFACT_URL_BYTES
                && !u.chars().any(|c| c.is_whitespace() || is_forbidden(c)) =>
        {
            Ok(())
        }
        (SOURCE_URL, _) => Err(format!(
            "{field}.url must start with https:// and be at most {MAX_ARTIFACT_URL_BYTES} bytes"
        )),
        (SOURCE_KAIROS, Some(_)) => Err(format!("{field}.url is only allowed with source url")),
        _ => Err(format!("{field}.source must be kairos or url")),
    }
}

fn is_forbidden(c: char) -> bool {
    crate::is_forbidden_display_char(c)
}

/// `upgrade_offer` 的格式限制（Pulse 端；違反時這個提議 `declined`，`PLS-E8402`）。
/// 簽章、雜湊、檔名與目標的一致性、平台、版本政策另由 [`release`](crate::release) 檢查
pub fn validate_upgrade_offer(o: &UpgradeOffer) -> Result<(), String> {
    check_uuid("upgrade_id", &o.upgrade_id)?;
    validate_display_text("rollout_id", &o.rollout_id, MAX_ROLLOUT_ID_BYTES)?;
    if o.action != ACTION_UPGRADE && o.action != ACTION_DOWNGRADE {
        return Err("action must be upgrade or downgrade".to_string());
    }
    crate::release::Version::parse(&o.target.version)
        .map_err(|e| format!("target.version: {e}"))?;
    if !crate::release::is_commit(&o.target.commit) {
        return Err("target.commit must be 7..=40 lowercase hex digits".to_string());
    }
    check_artifact("artifact", &o.artifact)?;
    if let Some(r) = &o.rollback_artifact {
        check_artifact("rollback_artifact", r)?;
    }
    if let Some(m) = &o.manifest {
        if m.content.len() > MAX_MANIFEST_BYTES {
            return Err(format!(
                "manifest.content must be at most {MAX_MANIFEST_BYTES} bytes"
            ));
        }
        crate::release::ReleaseFile::parse_manifest_name(&m.filename)
            .map_err(|e| format!("manifest.filename: {e}"))?;
    }
    if o.sha256sums.is_empty() || o.sha256sums.len() > MAX_SHA256SUMS_BYTES {
        return Err(format!(
            "sha256sums must be 1..={MAX_SHA256SUMS_BYTES} bytes"
        ));
    }
    if o.signature.is_empty() || o.signature.len() > MAX_SIGNATURE_BYTES {
        return Err(format!("signature must be 1..={MAX_SIGNATURE_BYTES} bytes"));
    }
    let (a, b) = o
        .apply_window
        .bounds()
        .ok_or("apply_window.not_before and not_after must be RFC 3339 timestamps")?;
    if a > b {
        return Err("apply_window.not_before must not be later than not_after".to_string());
    }
    if !(MIN_HEALTH_TIMEOUT_SECS..=MAX_HEALTH_TIMEOUT_SECS).contains(&o.health_timeout_secs) {
        return Err(format!(
            "health_timeout_secs must be {MIN_HEALTH_TIMEOUT_SECS}..={MAX_HEALTH_TIMEOUT_SECS}"
        ));
    }
    Ok(())
}

/// `upgrade_chunk_get`（Pulse → Kairos）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeChunkGet {
    pub seq: u64,
    pub ts: String,
    pub upgrade_id: String,
    pub filename: String,
    pub offset: u64,
    /// 1～32768
    pub length: u32,
}

/// `upgrade_chunk_get` 的限制（Kairos 端；違反時 `bad_request`。`filename` 不在提議中時
/// `upgrade_unavailable`，由 [`UpgradeOffer::file`] 判斷）
pub fn validate_upgrade_chunk_get(g: &UpgradeChunkGet) -> Result<(), String> {
    check_uuid("upgrade_id", &g.upgrade_id)?;
    crate::release::ReleaseFile::parse(&g.filename).map_err(|e| format!("filename: {e}"))?;
    if g.length == 0 || g.length > MAX_CHUNK_BYTES {
        return Err(format!("length must be 1..={MAX_CHUNK_BYTES}"));
    }
    if g.offset >= MAX_ARTIFACT_BYTES {
        return Err(format!("offset must be less than {MAX_ARTIFACT_BYTES}"));
    }
    Ok(())
}

/// `upgrade_chunk`（Kairos → Pulse），回應 `upgrade_chunk_get`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeChunk {
    pub seq: u64,
    pub upgrade_id: String,
    pub filename: String,
    pub offset: u64,
    /// 標準 base64（含補位）
    pub data: String,
    /// 這段之後沒有資料
    pub eof: bool,
}

impl UpgradeChunk {
    /// Kairos 端：以原始位元組組出訊息（`data` 為標準 base64）
    pub fn new(
        seq: u64,
        upgrade_id: impl Into<String>,
        filename: impl Into<String>,
        offset: u64,
        bytes: &[u8],
        eof: bool,
    ) -> Self {
        Self {
            seq,
            upgrade_id: upgrade_id.into(),
            filename: filename.into(),
            offset,
            data: crate::b64::encode(bytes),
            eof,
        }
    }

    /// Pulse 端：解出這段的位元組（嚴格的標準 base64，最多 [`MAX_CHUNK_BYTES`]）
    pub fn decode_data(&self) -> Result<Vec<u8>, String> {
        if self.data.len() > (MAX_CHUNK_BYTES as usize).div_ceil(3) * 4 {
            return Err(format!(
                "data must decode to at most {MAX_CHUNK_BYTES} bytes"
            ));
        }
        let bytes = crate::b64::decode(&self.data).map_err(|e| format!("data: {e}"))?;
        if bytes.len() > MAX_CHUNK_BYTES as usize {
            return Err(format!(
                "data must decode to at most {MAX_CHUNK_BYTES} bytes"
            ));
        }
        Ok(bytes)
    }
}

/// `upgrade_status.progress`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeProgress {
    pub bytes: u64,
    pub total: u64,
}

/// `upgrade_status`（Pulse → Kairos），回 `ack`；Kairos 以 (`upgrade_id`, `state`) 去重
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeStatus {
    pub seq: u64,
    pub ts: String,
    pub upgrade_id: String,
    /// [`UPGRADE_STATES`] 之一
    pub state: String,
    pub from_version: String,
    pub to_version: String,
    /// Pulse 代碼或 `null`
    #[serde(default)]
    pub code: Option<String>,
    /// ≤ 512 bytes，字元限制同 `hostname`；可為空字串
    #[serde(default)]
    pub detail: String,
    /// 選用：`accepted` 之後每完成 25% 送一次
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<UpgradeProgress>,
}

/// `upgrade_get` 的限制（Kairos 端；違反時 `bad_request`）
pub fn validate_upgrade_get(g: &UpgradeGet) -> Result<(), String> {
    check_uuid("upgrade_id", &g.upgrade_id)
}

/// `upgrade_status` 的限制（Kairos 端；違反時 `bad_request`）
pub fn validate_upgrade_status(s: &UpgradeStatus) -> Result<(), String> {
    check_uuid("upgrade_id", &s.upgrade_id)?;
    if !UPGRADE_STATES.contains(&s.state.as_str()) {
        return Err(format!(
            "state must be one of {}",
            UPGRADE_STATES.join(", ")
        ));
    }
    check_optional_text(
        "from_version",
        &s.from_version,
        crate::MAX_AGENT_VERSION_BYTES,
    )?;
    check_optional_text("to_version", &s.to_version, crate::MAX_AGENT_VERSION_BYTES)?;
    check_optional_text("detail", &s.detail, MAX_UPGRADE_DETAIL_BYTES)?;
    if let Some(p) = s.progress
        && (p.bytes > p.total || p.total > MAX_ARTIFACT_BYTES)
    {
        return Err(format!(
            "progress.bytes must not exceed progress.total (at most {MAX_ARTIFACT_BYTES})"
        ));
    }
    check_code(&s.code)
}

#[cfg(test)]
mod tests;
