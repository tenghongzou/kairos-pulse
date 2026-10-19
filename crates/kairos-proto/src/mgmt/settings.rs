//! 集中設定的白名單（REMOTE.md 第 3.1～3.3 節）：Kairos（K1：儲存設定檔時檢查、依 Pulse 版本過濾、
//! 合併指派）與 Pulse（P1：驗證、隱私規則、合併、決定是否重新連線）共用同一份資料。
//!
//! - [`SETTINGS`]：每個可下發的鍵（`<區段>.<鍵>`，與 `pulse.toml` 相同）的型別與範圍、套用方式
//!   （[`Apply`]）、最低 Pulse 版本、隱私規則（[`Privacy`]）與合併方式（[`Merge`]）。
//! - 線上格式是兩層物件 `{"<區段>": {"<鍵>": <值>}}`（[`check_shape`]、[`flatten`]、[`nest`]）。
//! - Kairos：[`validate_profile`]（`KRS-E0502`）、[`overlay`]（依優先序合併指派）、[`filter_for_version`]。
//! - Pulse：[`check_remote`]（不合法的值整份拒絕、不認得的鍵只忽略）、[`merge_key`]（內建預設 < 下發 <
//!   `pulse.toml` < 環境變數；隱私敏感的鍵只取較保守者；`collect.interface_exclude` 取聯集）、
//!   [`needs_reconnect`]。
//!
//! 內建預設值由 Pulse 自己提供（`pulse/config.toml`），本模組不複製一份，避免兩邊不一致。

use serde_json::{Map, Value};

use super::{
    IGNORED_NOT_MORE_RESTRICTIVE, IGNORED_UNKNOWN_KEY, IgnoredKey, KeyError, OverriddenKey,
};
use crate::release::Version;

/// 鍵（`<區段>.<鍵>`）的長度上限（bytes）
pub const MAX_SETTING_KEY_BYTES: usize = 128;

/// `errors[].reason`：型別不符（例如數字欄位給了字串）
pub const ERR_TYPE_MISMATCH: &str = "type_mismatch";
/// `errors[].reason`：超出範圍
pub const ERR_OUT_OF_RANGE: &str = "out_of_range";
/// `errors[].reason`：不在允許的值之中（列舉、字串格式）
pub const ERR_INVALID_VALUE: &str = "invalid_value";
/// `errors[].reason`：與另一個鍵矛盾（例如 `retry.max_delay` 小於 `retry.initial_delay`）
pub const ERR_CONFLICT: &str = "conflict";
/// `errors[].reason`（只在 Kairos 儲存設定檔時）：不在白名單
pub const ERR_UNKNOWN_KEY: &str = "unknown_key";

/// 值的型別與範圍（與 `pulse.toml` 相同）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    /// 整數，含兩端
    Int {
        min: u64,
        max: u64,
    },
    /// 字串，只能是其中之一（區分大小寫）
    Enum(&'static [&'static str]),
    /// 字串陣列：最多 `max_items` 個，每個 1～`max_item_bytes` bytes 且不是只有空白
    StringList {
        max_items: usize,
        max_item_bytes: usize,
    },
}

/// 套用方式（REMOTE.md 第 3.1 節「套用」欄）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// 不中斷連線、下一個週期生效
    Live,
    /// Pulse 主動結束目前的連線並重連（hello 的內容或協商的 feature 會改變）
    Reconnect,
}

/// 隱私規則（REMOTE.md 第 3.1 節、第 14 節 D10）：下發的值只有在比本機（含內建預設）
/// **更保守或相同**時才套用，否則放進 `ignored`（`not_more_restrictive`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privacy {
    /// 一般的鍵
    None,
    /// 字串，依保守程度由高到低排列（例如 `none` < `executable` < `full`）
    Ordered(&'static [&'static str]),
    /// 布林，`false` 是保守的值（只能下發 `false`；下發 `true` 只在本機已是 `true` 時視為相同）
    FalseIsConservative,
}

/// 與本機值的合併方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// 本機明確寫出的值優先（第 3.3 節）
    LocalWins,
    /// 與本機（或內建預設）的值取聯集（`collect.interface_exclude`）
    Union,
}

/// 白名單中的一個鍵
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingSpec {
    /// `<區段>.<鍵>`，例如 `report.interval`
    pub key: &'static str,
    pub kind: Kind,
    pub apply: Apply,
    /// 認得這個鍵的最低 Pulse 版本；Kairos 只對 `agent_version` 不低於它的主機下發
    pub min_pulse: &'static str,
    pub privacy: Privacy,
    pub merge: Merge,
    /// 合法、但只能在本機設定的值（例如 `processes.command = "full"`）：Kairos 儲存時拒絕，
    /// Pulse 收到時放進 `ignored`（`unknown_key`，REMOTE.md 第 3.2 節）
    pub local_only_values: &'static [&'static str],
}

const fn int(key: &'static str, min: u64, max: u64, apply: Apply) -> SettingSpec {
    spec(key, Kind::Int { min, max }, apply)
}

const fn boolean(key: &'static str, apply: Apply) -> SettingSpec {
    spec(key, Kind::Bool, apply)
}

const fn spec(key: &'static str, kind: Kind, apply: Apply) -> SettingSpec {
    SettingSpec {
        key,
        kind,
        apply,
        min_pulse: "0.9.0",
        privacy: Privacy::None,
        merge: Merge::LocalWins,
        local_only_values: &[],
    }
}

/// `processes.command` 的保守程度（由高到低）
pub const PROCESS_COMMAND_ORDER: &[&str] = &["none", "executable", "full"];
/// `log.level` 可下發的值（小寫；`pulse.toml` 不分大小寫，下發一律小寫）
pub const LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// 可下發的設定（REMOTE.md 第 3.1 節）。範圍與 `pulse/src/config_manager.rs` 的 `AppConfig::validate` 相同
pub const SETTINGS: &[SettingSpec] = &[
    int("heartbeat.interval", 1, 60, Apply::Reconnect),
    int("report.interval", 1, 3600, Apply::Live),
    int("retry.max_retries", 0, u32::MAX as u64, Apply::Live),
    int("retry.initial_delay", 1, 3600, Apply::Live),
    int("retry.max_delay", 1, 3600, Apply::Live),
    boolean("collect.filesystems", Apply::Live),
    boolean("collect.disk_io", Apply::Live),
    boolean("collect.network", Apply::Live),
    boolean("collect.temperature", Apply::Live),
    int("collect.max_filesystems", 0, 16, Apply::Live),
    int("collect.max_disks", 0, 8, Apply::Live),
    int("collect.max_interfaces", 0, 16, Apply::Live),
    SettingSpec {
        merge: Merge::Union,
        ..spec(
            "collect.interface_exclude",
            Kind::StringList {
                max_items: 32,
                max_item_bytes: 256,
            },
            Apply::Live,
        )
    },
    boolean("processes.enabled", Apply::Reconnect),
    int("processes.interval", 1, 3600, Apply::Live),
    int(
        "processes.top",
        1,
        crate::MAX_TOP_PROCESSES as u64,
        Apply::Live,
    ),
    SettingSpec {
        privacy: Privacy::Ordered(PROCESS_COMMAND_ORDER),
        local_only_values: &["full"],
        ..spec(
            "processes.command",
            Kind::Enum(&["none", "executable"]),
            Apply::Live,
        )
    },
    boolean("inventory.enabled", Apply::Reconnect),
    int("inventory.interval", 300, 86_400, Apply::Live),
    SettingSpec {
        privacy: Privacy::FalseIsConservative,
        ..boolean("inventory.hardware_serials", Apply::Live)
    },
    SettingSpec {
        privacy: Privacy::FalseIsConservative,
        ..boolean("inventory.network_addresses", Apply::Live)
    },
    boolean("buffer.enabled", Apply::Reconnect),
    int("buffer.max_mb", 1, 102_400, Apply::Live),
    int("buffer.max_records", 100, 10_000_000, Apply::Live),
    int("buffer.max_age_hours", 1, 8760, Apply::Live),
    int("buffer.replay_interval_ms", 500, 60_000, Apply::Live),
    spec("log.level", Kind::Enum(LOG_LEVELS), Apply::Live),
    int("log.retention_days", 0, 3650, Apply::Live),
];

/// 白名單中的鍵（`report.interval`）
pub fn setting(key: &str) -> Option<&'static SettingSpec> {
    SETTINGS.iter().find(|s| s.key == key)
}

/// 值的檢查失敗：`reason` 為 `errors[].reason`，`message` 為說明
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError {
    pub reason: &'static str,
    pub message: String,
}

impl SettingSpec {
    /// 值的型別與範圍是否合法（不含 [`SettingSpec::local_only_values`] 的判斷）
    pub fn check_value(&self, value: &Value) -> Result<(), ValueError> {
        let mismatch = |want: &str| ValueError {
            reason: ERR_TYPE_MISMATCH,
            message: format!("{} must be {want}", self.key),
        };
        match self.kind {
            Kind::Bool => value
                .as_bool()
                .map(|_| ())
                .ok_or_else(|| mismatch("a boolean")),
            Kind::Int { min, max } => {
                let n = value.as_u64().ok_or_else(|| {
                    if value.as_i64().is_some() {
                        ValueError {
                            reason: ERR_OUT_OF_RANGE,
                            message: format!("{} must be {min}..={max}", self.key),
                        }
                    } else {
                        mismatch("an integer")
                    }
                })?;
                if (min..=max).contains(&n) {
                    Ok(())
                } else {
                    Err(ValueError {
                        reason: ERR_OUT_OF_RANGE,
                        message: format!("{} must be {min}..={max} (got {n})", self.key),
                    })
                }
            }
            Kind::Enum(values) => {
                let s = value.as_str().ok_or_else(|| mismatch("a string"))?;
                if values.contains(&s) || self.local_only_values.contains(&s) {
                    Ok(())
                } else {
                    Err(ValueError {
                        reason: ERR_INVALID_VALUE,
                        message: format!("{} must be one of {}", self.key, values.join(", ")),
                    })
                }
            }
            Kind::StringList {
                max_items,
                max_item_bytes,
            } => {
                let items = value
                    .as_array()
                    .ok_or_else(|| mismatch("an array of strings"))?;
                if items.len() > max_items {
                    return Err(ValueError {
                        reason: ERR_OUT_OF_RANGE,
                        message: format!("{} must have at most {max_items} entries", self.key),
                    });
                }
                for item in items {
                    let s = item
                        .as_str()
                        .ok_or_else(|| mismatch("an array of strings"))?;
                    if s.trim().is_empty()
                        || s.len() > max_item_bytes
                        || s.chars().any(crate::is_forbidden_display_char)
                    {
                        return Err(ValueError {
                            reason: ERR_INVALID_VALUE,
                            message: format!(
                                "each entry of {} must be 1..={max_item_bytes} bytes of visible characters",
                                self.key
                            ),
                        });
                    }
                }
                Ok(())
            }
        }
    }

    /// 這個值只能在本機設定（[`SettingSpec::local_only_values`]）
    pub fn is_local_only(&self, value: &Value) -> bool {
        value
            .as_str()
            .is_some_and(|s| self.local_only_values.contains(&s))
    }

    /// 隱私規則：`remote` 是否比 `baseline`（本機明確的值，沒有時為內建預設）更保守或相同。
    /// 無法比較的值（型別不符、不在順序表中）一律視為不更保守（fail closed）
    pub fn is_at_least_as_restrictive(&self, remote: &Value, baseline: &Value) -> bool {
        match self.privacy {
            Privacy::None => true,
            Privacy::FalseIsConservative => matches!(
                (remote.as_bool(), baseline.as_bool()),
                (Some(false), _) | (Some(true), Some(true))
            ),
            Privacy::Ordered(order) => {
                let rank = |v: &Value| v.as_str().and_then(|s| order.iter().position(|o| *o == s));
                matches!((rank(remote), rank(baseline)), (Some(r), Some(b)) if r <= b)
            }
        }
    }
}

/// 兩層物件的格式：`{"<區段>": {"<鍵>": <值>}}`，區段為小寫英文字母、數字、`_` 與 `.`（巢狀區段），
/// 鍵為小寫英文字母、數字與 `_`，`<區段>.<鍵>` 最長 [`MAX_SETTING_KEY_BYTES`] bytes。
/// 值的型別不在這裡檢查
pub fn check_shape(settings: &Map<String, Value>) -> Result<(), String> {
    let ident = |s: &str, dots: bool| {
        let mut b = s.bytes();
        matches!(b.next(), Some(b'a'..=b'z'))
            && b.all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || (dots && c == b'.')
            })
            && !(dots && (s.ends_with('.') || s.contains("..")))
    };
    for (section, keys) in settings {
        if !ident(section, true) {
            return Err(format!("invalid section name {section:?}"));
        }
        let keys = keys
            .as_object()
            .ok_or_else(|| format!("section {section:?} must be an object"))?;
        for key in keys.keys() {
            if !ident(key, false) || section.len() + 1 + key.len() > MAX_SETTING_KEY_BYTES {
                return Err(format!("invalid key name {section:?}.{key:?}"));
            }
        }
    }
    Ok(())
}

/// 兩層物件攤平成 `(<區段>.<鍵>, 值)`，依區段、鍵的字典序。格式不符時錯誤（[`check_shape`]）
pub fn flatten(settings: &Map<String, Value>) -> Result<Vec<(String, &Value)>, String> {
    check_shape(settings)?;
    let mut out: Vec<(String, &Value)> = settings
        .iter()
        .flat_map(|(section, keys)| {
            keys.as_object()
                .into_iter()
                .flatten()
                .map(move |(k, v)| (format!("{section}.{k}"), v))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// `(<區段>.<鍵>, 值)` 組回兩層物件（以最後一個 `.` 分開區段與鍵；沒有 `.` 的鍵略過）
pub fn nest<K: AsRef<str>>(entries: impl IntoIterator<Item = (K, Value)>) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in entries {
        let Some((section, name)) = key.as_ref().rsplit_once('.') else {
            continue;
        };
        if let Value::Object(m) = out
            .entry(section.to_string())
            .or_insert_with(|| Value::Object(Map::new()))
        {
            m.insert(name.to_string(), value);
        }
    }
    out
}

/// 從兩層物件中取出某個鍵（`report.interval`）的值
pub fn get<'a>(settings: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    let (section, name) = key.rsplit_once('.')?;
    settings.get(section)?.as_object()?.get(name)
}

fn key_error(key: &str, reason: &str, message: String) -> KeyError {
    let mut message = message;
    if message.len() > super::MAX_STATUS_MESSAGE_BYTES {
        let mut end = super::MAX_STATUS_MESSAGE_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    KeyError {
        key: key.to_string(),
        reason: reason.to_string(),
        message,
    }
}

/// 鍵之間的限制（目前只有 `retry.max_delay ≥ retry.initial_delay`，兩者都出現時才檢查）
fn cross_check(flat: &[(String, &Value)]) -> Option<KeyError> {
    let find = |k: &str| {
        flat.iter()
            .find(|(key, _)| key == k)
            .and_then(|(_, v)| v.as_u64())
    };
    match (find("retry.initial_delay"), find("retry.max_delay")) {
        (Some(initial), Some(max)) if max < initial => Some(key_error(
            "retry.max_delay",
            ERR_CONFLICT,
            "retry.max_delay must not be less than retry.initial_delay".to_string(),
        )),
        _ => None,
    }
}

/// Kairos 儲存設定檔時的檢查（`KRS-E0502`，API 400 `config_invalid`）：不在白名單的鍵、
/// 只能在本機設定的值、型別或範圍不符、鍵之間矛盾都是錯誤
pub fn validate_profile(settings: &Map<String, Value>) -> Result<(), Vec<KeyError>> {
    let flat = flatten(settings).map_err(|e| vec![key_error("*", ERR_INVALID_VALUE, e)])?;
    let mut errors = Vec::new();
    for (key, value) in &flat {
        match setting(key) {
            None => errors.push(key_error(
                key,
                ERR_UNKNOWN_KEY,
                format!("{key} cannot be set remotely"),
            )),
            Some(spec) if spec.is_local_only(value) => errors.push(key_error(
                key,
                ERR_INVALID_VALUE,
                format!("this value of {key} can only be set locally in pulse.toml"),
            )),
            Some(spec) => {
                if let Err(e) = spec.check_value(value) {
                    errors.push(key_error(key, e.reason, e.message));
                }
            }
        }
    }
    errors.extend(cross_check(&flat));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// [`check_remote`] 的結果
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemoteCheck {
    /// 白名單中、值合法的鍵（之後交給 [`merge_key`]）
    pub accepted: Vec<(String, Value)>,
    /// 不認得的鍵與只能在本機設定的值（不影響其他鍵）
    pub ignored: Vec<IgnoredKey>,
    /// 型別、範圍或鍵之間的錯誤；不是空的時整份拒絕（`rejected`、`PLS-W0501`）
    pub errors: Vec<KeyError>,
}

/// Pulse 收到 `config` 時的第一步（REMOTE.md 第 3.5 節）：不認得的鍵與只能在本機設定的值放進
/// `ignored`；任何白名單鍵的值不合法時放進 `errors`（整份拒絕）。隱私規則與本機覆蓋由 [`merge_key`]
/// 處理；合併後仍要以 Pulse 既有的完整驗證（`AppConfig::validate`）再檢查一次
pub fn check_remote(settings: &Map<String, Value>) -> RemoteCheck {
    let mut out = RemoteCheck::default();
    let flat = match flatten(settings) {
        Ok(f) => f,
        Err(e) => {
            out.errors.push(key_error("*", ERR_INVALID_VALUE, e));
            return out;
        }
    };
    for (key, value) in &flat {
        match setting(key) {
            Some(spec) if !spec.is_local_only(value) => match spec.check_value(value) {
                Ok(()) => out.accepted.push((key.clone(), (*value).clone())),
                Err(e) => out.errors.push(key_error(key, e.reason, e.message)),
            },
            _ => out.ignored.push(IgnoredKey {
                key: key.clone(),
                reason: IGNORED_UNKNOWN_KEY.to_string(),
            }),
        }
    }
    out.errors.extend(cross_check(&flat));
    out
}

/// 本機明確寫出的值與其來源（[`super::OVERRIDE_SOURCE_FILE`] 或 [`super::OVERRIDE_SOURCE_ENV`]）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalValue<'a> {
    pub value: &'a Value,
    pub source: &'a str,
}

/// [`merge_key`] 的結果
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    /// 實際生效的值（`config_status.effective`）
    pub effective: Value,
    /// 被本機覆蓋（本機值與下發值不同時才有）
    pub overridden: Option<OverriddenKey>,
    /// 隱私敏感的鍵下發了不更保守的值（`not_more_restrictive`）
    pub ignored: Option<IgnoredKey>,
}

/// 一個鍵的合併（REMOTE.md 第 3.3 節：內建預設 < 下發 < `pulse.toml` < 環境變數）：
///
/// 1. 隱私敏感的鍵：下發值比「本機值（沒有時為內建預設）」不保守時忽略（`not_more_restrictive`），
///    生效值為本機值或內建預設。
/// 2. `collect.interface_exclude`（[`Merge::Union`]）：本機值（沒有時為內建預設）與下發值取聯集，
///    本機的項目在前、重複的只留一個；不算覆蓋。
/// 3. 其他：本機有值時以本機為準（與下發值不同時記為 `overridden`），否則用下發值。
///
/// `remote` 為 `None`（沒有下發這個鍵）時生效值為本機值或內建預設。`remote` 應已通過
/// [`check_remote`]
pub fn merge_key(
    spec: &SettingSpec,
    default: &Value,
    remote: Option<&Value>,
    local: Option<LocalValue<'_>>,
) -> Merged {
    let baseline = local.map_or(default, |l| l.value);
    let plain = |effective: &Value| Merged {
        effective: effective.clone(),
        overridden: None,
        ignored: None,
    };
    let Some(remote) = remote else {
        return plain(baseline);
    };
    if !spec.is_at_least_as_restrictive(remote, baseline) {
        return Merged {
            ignored: Some(IgnoredKey {
                key: spec.key.to_string(),
                reason: IGNORED_NOT_MORE_RESTRICTIVE.to_string(),
            }),
            ..plain(baseline)
        };
    }
    if spec.merge == Merge::Union {
        let mut items: Vec<Value> = baseline.as_array().cloned().unwrap_or_default();
        for item in remote.as_array().into_iter().flatten() {
            if !items.contains(item) {
                items.push(item.clone());
            }
        }
        return plain(&Value::Array(items));
    }
    match local {
        Some(l) => Merged {
            overridden: (l.value != remote).then(|| OverriddenKey {
                key: spec.key.to_string(),
                remote: remote.clone(),
                local: l.value.clone(),
                source: l.source.to_string(),
            }),
            ..plain(l.value)
        },
        None => plain(remote),
    }
}

/// 生效值改變的鍵中，是否有需要重新連線的（[`Apply::Reconnect`]）；需要時在全部即時鍵套用後
/// 統一重連一次
pub fn needs_reconnect<K: AsRef<str>>(changed_keys: &[K]) -> bool {
    changed_keys
        .iter()
        .any(|k| setting(k.as_ref()).is_some_and(|s| s.apply == Apply::Reconnect))
}

/// Kairos 依優先序合併指派：`layers` 由低到高，同一個鍵以後者為準（兩層物件逐鍵覆蓋）
pub fn overlay<'a>(layers: impl IntoIterator<Item = &'a Map<String, Value>>) -> Map<String, Value> {
    let mut out = Map::new();
    for layer in layers {
        for (section, keys) in layer {
            let Some(keys) = keys.as_object() else {
                continue;
            };
            if let Value::Object(m) = out
                .entry(section.clone())
                .or_insert_with(|| Value::Object(Map::new()))
            {
                for (k, v) in keys {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
    }
    out
}

/// Kairos 依主機回報的 `agent_version` 只送該版本認得的鍵（[`SettingSpec::min_pulse`]）。
/// 只比較 `MAJOR.MINOR.PATCH`（`0.9.0-test.1` 視為 0.9.0，測試建置也能收到 0.9.0 的鍵）。
/// 版本無法解析（例如開發版的奇怪字串）時不送任何鍵；不在白名單的鍵一律略過
pub fn filter_for_version(
    settings: &Map<String, Value>,
    agent_version: &str,
) -> Map<String, Value> {
    let Ok(version) = Version::parse(agent_version) else {
        return Map::new();
    };
    let Ok(flat) = flatten(settings) else {
        return Map::new();
    };
    nest(flat.into_iter().filter_map(|(key, value)| {
        let spec = setting(&key)?;
        let min = Version::parse(spec.min_pulse).ok()?;
        (version.core() >= min.core()).then(|| (key, value.clone()))
    }))
}
