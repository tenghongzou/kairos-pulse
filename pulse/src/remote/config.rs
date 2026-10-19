//! 集中設定（P1，`crates/kairos-proto/REMOTE.md` 第 3 節）。
//!
//! - **合併**（第 3.3 節）：內建預設值 < Kairos 下發 < `pulse.toml` < `PULSE__*` 環境變數。每個白名單鍵以
//!   `kairos_proto::mgmt::settings::merge_key` 合併（本機明確寫出的鍵優先並回報 `overridden`；
//!   `collect.interface_exclude` 取聯集；隱私敏感的鍵只接受更保守的值），合併後以 `AppConfig::validate`
//!   檢查一次（與啟動時相同的規則）。任何白名單鍵的值不合法時整份拒絕（`rejected`、`PLS-W0501`）。
//! - **快取**（第 3.7 節）：`<狀態目錄>/remote/config.json`（目錄 0750、檔案 0640，寫暫存檔再改名）保存
//!   目前套用的版本與 last-known-good；離線或重新啟動後以快取運作，沒有期限。無法寫入時不套用
//!   （`rejected`、`PLS-E0506`），損毀時忽略快取（`PLS-E0506`）。
//! - **確認期與回復**（第 3.6 節）：即時套用的版本在 `max(3 × heartbeat.interval, 60 秒)` 內沒有斷線、
//!   需要重新連線的版本在重連後收到 `hello_ack` 即標為 last-known-good；確認前連續 3 次無法完成
//!   `hello_ack` 時回復上一份 last-known-good（`rolled_back`、`PLS-W0504`），同一個 `rev` 不再自動套用。
//!   崩潰保護：`pending_apply` 在確認後清除，啟動時發現同一個 `rev` 已連續 2 次在確認前結束，改以
//!   last-known-good 啟動並回報 `rolled_back`。
//! - **不使用快取的情況**：本機停用（`remote_config.enabled = false`，停用後第一次連線送一次
//!   `state: disabled`），或連上不支援 `config` 的 Kairos（快取保留但只用本機設定）。
//!
//! 生效的設定以 `watch` 發布（[`CentralConfig::subscribe`]）：即時的鍵由 `main` 的套用工作就地更新，
//! 需要重新連線的鍵（`Apply::Reconnect`）由連線迴圈結束目前的連線後重建取樣與連線（[`ApplyOutcome::reconnect`]）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use kairos_proto::mgmt::settings::{self, LocalValue, SETTINGS};
use kairos_proto::mgmt::{
    self, CONFIG_STATE_APPLIED, CONFIG_STATE_DISABLED, CONFIG_STATE_REJECTED,
    CONFIG_STATE_ROLLED_BACK, ConfigStatus, IgnoredKey, KeyError, OverriddenKey, Pending,
    RemoteConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::codes;
use crate::config_manager::AppConfig;

/// 快取的 `schema`
pub const CACHE_SCHEMA: &str = "pulse.remote-config/v1";
/// 快取檔（相對於狀態目錄）
pub const CACHE_RELATIVE: &str = "remote/config.json";
/// 快取的大小上限（白名單全部填滿約 2 KB，兩份加上來源仍遠小於此）
const MAX_CACHE_BYTES: u64 = 512 * 1024;
/// 確認期的下限（第 3.6 節：`max(3 × heartbeat.interval, 60 秒)`）
const MIN_CONFIRM: Duration = Duration::from_secs(60);
/// 確認前連續這麼多次無法完成 `hello_ack` 時回復 last-known-good
const CONFIRM_FAILURES: u32 = 3;
/// 同一個 `rev` 連續這麼多次在確認前結束（崩潰、被 systemd 重啟）時，改以 last-known-good 啟動
const CRASH_STARTS: u32 = 2;

// ---------------------------------------------------------------------------
// 白名單鍵與 AppConfig 欄位的對應
// ---------------------------------------------------------------------------

/// `AppConfig` 中白名單鍵（`report.interval`）的值；不在白名單時 `None`
pub fn get_value(c: &AppConfig, key: &str) -> Option<Value> {
    Some(match key {
        "heartbeat.interval" => json!(c.heartbeat.interval),
        "report.interval" => json!(c.report.interval),
        "retry.max_retries" => json!(c.retry.max_retries),
        "retry.initial_delay" => json!(c.retry.initial_delay),
        "retry.max_delay" => json!(c.retry.max_delay),
        "collect.filesystems" => json!(c.collect.filesystems),
        "collect.disk_io" => json!(c.collect.disk_io),
        "collect.network" => json!(c.collect.network),
        "collect.temperature" => json!(c.collect.temperature),
        "collect.max_filesystems" => json!(c.collect.max_filesystems),
        "collect.max_disks" => json!(c.collect.max_disks),
        "collect.max_interfaces" => json!(c.collect.max_interfaces),
        "collect.interface_exclude" => json!(c.collect.interface_exclude),
        "processes.enabled" => json!(c.processes.enabled),
        "processes.interval" => json!(c.processes.interval),
        "processes.top" => json!(c.processes.top),
        "processes.command" => json!(c.processes.command.trim()),
        "inventory.enabled" => json!(c.inventory.enabled),
        "inventory.interval" => json!(c.inventory.interval),
        "inventory.hardware_serials" => json!(c.inventory.hardware_serials),
        "inventory.network_addresses" => json!(c.inventory.network_addresses),
        "buffer.enabled" => json!(c.buffer.enabled),
        "buffer.max_mb" => json!(c.buffer.max_mb),
        "buffer.max_records" => json!(c.buffer.max_records),
        "buffer.max_age_hours" => json!(c.buffer.max_age_hours),
        "buffer.replay_interval_ms" => json!(c.buffer.replay_interval_ms),
        "log.level" => json!(c.log.level),
        "log.retention_days" => json!(c.log.retention_days),
        _ => return None,
    })
}

/// 把白名單鍵的值寫進 `AppConfig`（值應已通過 `SettingSpec::check_value`）
pub fn set_value(c: &mut AppConfig, key: &str, v: &Value) -> Result<(), String> {
    let bad = || format!("{key} 的值型別不符：{v}");
    let u64v = || v.as_u64().ok_or_else(bad);
    let boolv = || v.as_bool().ok_or_else(bad);
    let u32v = || {
        v.as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(bad)
    };
    let strv = || v.as_str().map(str::to_string).ok_or_else(bad);
    match key {
        "heartbeat.interval" => c.heartbeat.interval = u64v()?,
        "report.interval" => c.report.interval = u64v()?,
        "retry.max_retries" => c.retry.max_retries = u32v()?,
        "retry.initial_delay" => c.retry.initial_delay = u64v()?,
        "retry.max_delay" => c.retry.max_delay = u64v()?,
        "collect.filesystems" => c.collect.filesystems = boolv()?,
        "collect.disk_io" => c.collect.disk_io = boolv()?,
        "collect.network" => c.collect.network = boolv()?,
        "collect.temperature" => c.collect.temperature = boolv()?,
        "collect.max_filesystems" => c.collect.max_filesystems = u64v()?,
        "collect.max_disks" => c.collect.max_disks = u64v()?,
        "collect.max_interfaces" => c.collect.max_interfaces = u64v()?,
        "collect.interface_exclude" => {
            c.collect.interface_exclude = v
                .as_array()
                .ok_or_else(bad)?
                .iter()
                .map(|x| x.as_str().map(str::to_string).ok_or_else(bad))
                .collect::<Result<_, _>>()?;
        }
        "processes.enabled" => c.processes.enabled = boolv()?,
        "processes.interval" => c.processes.interval = u64v()?,
        "processes.top" => c.processes.top = u64v()?,
        "processes.command" => c.processes.command = strv()?,
        "inventory.enabled" => c.inventory.enabled = boolv()?,
        "inventory.interval" => c.inventory.interval = u64v()?,
        "inventory.hardware_serials" => c.inventory.hardware_serials = boolv()?,
        "inventory.network_addresses" => c.inventory.network_addresses = boolv()?,
        "buffer.enabled" => c.buffer.enabled = boolv()?,
        "buffer.max_mb" => c.buffer.max_mb = u64v()?,
        "buffer.max_records" => c.buffer.max_records = u64v()?,
        "buffer.max_age_hours" => c.buffer.max_age_hours = u64v()?,
        "buffer.replay_interval_ms" => c.buffer.replay_interval_ms = u64v()?,
        "log.level" => c.log.level = strv()?.to_ascii_lowercase(),
        "log.retention_days" => c.log.retention_days = u32v()?,
        _ => return Err(format!("{key} 不在集中設定的白名單中")),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 本機明確寫出的鍵
// ---------------------------------------------------------------------------

/// 本機明確寫出的白名單鍵與來源（`pulse.toml` 或 `env`）。值取自本機載入的 `AppConfig`
/// （環境變數一律以字串讀入，型別轉換後的值才是本機實際的值）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalSources {
    keys: BTreeMap<String, &'static str>,
}

impl LocalSources {
    /// 讀 `pulse.toml`（可不存在）與 `PULSE__*` 環境變數（`env_override` 為 `Some` 時改用給定的鍵值），
    /// 記下白名單中有出現的鍵；環境變數優先。讀不到的來源視為沒有鍵（語法錯誤在啟動時已經擋下）
    pub fn load(file: Option<&Path>, env_override: Option<config::Map<String, String>>) -> Self {
        let file_cfg = file.and_then(|p| {
            config::Config::builder()
                .add_source(
                    config::File::from(p)
                        .format(config::FileFormat::Toml)
                        .required(false),
                )
                .build()
                .ok()
        });
        let env_cfg = config::Config::builder()
            .add_source(AppConfig::env_source(env_override))
            .build()
            .ok();
        let has = |c: &Option<config::Config>, key: &str| {
            c.as_ref()
                .is_some_and(|c| c.get::<config::Value>(key).is_ok())
        };
        let mut keys = BTreeMap::new();
        for spec in SETTINGS {
            if has(&env_cfg, spec.key) {
                keys.insert(spec.key.to_string(), mgmt::OVERRIDE_SOURCE_ENV);
            } else if has(&file_cfg, spec.key) {
                keys.insert(spec.key.to_string(), mgmt::OVERRIDE_SOURCE_FILE);
            }
        }
        Self { keys }
    }

    /// 測試用：直接指定（鍵、來源）
    #[cfg(test)]
    pub fn from_pairs(pairs: &[(&str, &'static str)]) -> Self {
        Self {
            keys: pairs.iter().map(|(k, s)| (k.to_string(), *s)).collect(),
        }
    }

    /// 這個鍵的本機來源；沒有明確寫出時 `None`
    pub fn source(&self, key: &str) -> Option<&'static str> {
        self.keys.get(key).copied()
    }
}

// ---------------------------------------------------------------------------
// 合併
// ---------------------------------------------------------------------------

/// [`merge`] 的結果
#[derive(Debug, Clone)]
pub struct MergeOutcome {
    /// 合併後的完整設定（已通過 `AppConfig::validate`；`errors` 不是空的時為本機設定）
    pub config: AppConfig,
    /// 白名單中每個鍵實際生效的值（兩層物件，`config_status.effective`）
    pub effective: Map<String, Value>,
    pub overridden: Vec<OverriddenKey>,
    pub ignored: Vec<IgnoredKey>,
    /// 不是空的時整份拒絕
    pub errors: Vec<KeyError>,
    /// 每個鍵的來源（`pulse config show --sources`）：`default`、`remote:<設定檔>`、`pulse.toml`、`env`
    pub sources: BTreeMap<String, String>,
}

/// 合併本機設定與下發的 `settings`（`None`＝沒有下發，只用本機與內建預設）。REMOTE.md 第 3.3、3.5 節
pub fn merge(
    local: &AppConfig,
    defaults: &AppConfig,
    sources: &LocalSources,
    remote: Option<&Map<String, Value>>,
    remote_sources: Option<&BTreeMap<String, String>>,
) -> MergeOutcome {
    let check = remote.map(settings::check_remote).unwrap_or_default();
    let mut out = MergeOutcome {
        config: local.clone(),
        effective: Map::new(),
        overridden: Vec::new(),
        ignored: check.ignored.clone(),
        errors: check.errors.clone(),
        sources: BTreeMap::new(),
    };
    let accepted: BTreeMap<&str, &Value> = check
        .accepted
        .iter()
        .map(|(k, v)| (k.as_str(), v))
        .collect();
    let mut config = local.clone();
    let mut entries: Vec<(String, Value)> = Vec::new();
    for spec in SETTINGS {
        let default = get_value(defaults, spec.key).unwrap_or(Value::Null);
        let local_value = get_value(local, spec.key).unwrap_or(Value::Null);
        let local_src = sources.source(spec.key);
        let remote_value = accepted.get(spec.key).copied();
        let m = settings::merge_key(
            spec,
            &default,
            remote_value,
            local_src.map(|source| LocalValue {
                value: &local_value,
                source,
            }),
        );
        let remote_used = remote_value.is_some() && m.ignored.is_none();
        let union = spec.merge == settings::Merge::Union;
        let source = match local_src {
            // 本機明確寫出：本機優先（聯集的鍵有下發時標為下發）
            Some(src) if !(union && remote_used) => src.to_string(),
            _ if remote_used => format!(
                "remote:{}",
                remote_sources
                    .and_then(|s| s.get(spec.key))
                    .map_or("", String::as_str)
            ),
            Some(src) => src.to_string(),
            None => "default".to_string(),
        };
        out.sources.insert(spec.key.to_string(), source);
        if remote_value.is_some()
            && let Err(e) = set_value(&mut config, spec.key, &m.effective)
        {
            out.errors
                .push(key_error(spec.key, settings::ERR_TYPE_MISMATCH, e));
        }
        out.overridden.extend(m.overridden);
        out.ignored.extend(m.ignored);
        entries.push((spec.key.to_string(), m.effective));
    }
    out.effective = settings::nest(entries);
    if out.errors.is_empty()
        && let Err(e) = config.validate()
    {
        let message = e.to_string();
        let key = SETTINGS
            .iter()
            .map(|s| s.key)
            .filter(|k| message.contains(k))
            .max_by_key(|k| k.len())
            .unwrap_or("*");
        out.errors
            .push(key_error(key, settings::ERR_CONFLICT, message));
    }
    if out.errors.is_empty() {
        out.config = config;
    } else {
        // 整份拒絕：生效的仍是本機設定（呼叫端改用上一版）
        out.effective = merge(local, defaults, sources, None, None).effective;
    }
    out
}

fn key_error(key: &str, reason: &str, message: String) -> KeyError {
    let mut message: String = message
        .chars()
        .map(|c| {
            if crate::protocol::is_forbidden_display_char(c) {
                '?'
            } else {
                c
            }
        })
        .collect();
    if message.len() > mgmt::MAX_STATUS_MESSAGE_BYTES {
        let mut end = mgmt::MAX_STATUS_MESSAGE_BYTES;
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

/// 兩份 `effective` 中值不同的鍵（`report.interval`），依字典序
pub fn changed_keys(old: &Map<String, Value>, new: &Map<String, Value>) -> Vec<String> {
    SETTINGS
        .iter()
        .filter(|s| settings::get(old, s.key) != settings::get(new, s.key))
        .map(|s| s.key.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// 快取檔
// ---------------------------------------------------------------------------

/// 一份下發的設定（目前套用的版本或 last-known-good）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedConfig {
    pub rev: u64,
    pub hash: String,
    pub settings: Map<String, Value>,
    #[serde(default)]
    pub sources: BTreeMap<String, String>,
    /// 套用的時間（RFC 3339 UTC 毫秒）
    #[serde(default)]
    pub applied_at: String,
}

/// 套用後尚未確認（崩潰保護）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApply {
    pub rev: u64,
    /// 這一版套用後重新啟動的次數（確認前）
    pub starts: u32,
}

/// 已回復、尚待回報的版本
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolledBack {
    pub rev: u64,
    /// 失敗的版本的雜湊（`config_status.hash`）
    #[serde(default)]
    pub hash: String,
    /// 回復到的版本（沒有 last-known-good 時 0：只用本機設定）
    pub restored_rev: u64,
    #[serde(default)]
    pub reported: bool,
}

/// `<狀態目錄>/remote/config.json`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheFile {
    pub schema: String,
    /// 目前套用的版本（沒有任何下發時 `None`）
    pub applied: Option<CachedConfig>,
    /// 上一份確認可用的版本
    pub last_known_good: Option<CachedConfig>,
    pub pending_apply: Option<PendingApply>,
    /// 已回復的版本：Kairos 下發新的 `rev` 前不再自動套用
    pub skip_rev: Option<u64>,
    pub rolled_back: Option<RolledBack>,
    /// 本機停用後已通知過 Kairos（`state: disabled`）
    pub disabled_reported: bool,
}

impl CacheFile {
    fn load(path: &Path) -> Result<Option<Self>, String> {
        match crate::remote::read_small(path, MAX_CACHE_BYTES) {
            Ok(bytes) => {
                let c: CacheFile = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                if c.schema != CACHE_SCHEMA {
                    return Err(format!("schema 不是 {CACHE_SCHEMA}"));
                }
                for cached in [&c.applied, &c.last_known_good].into_iter().flatten() {
                    if mgmt::settings_hash(&cached.settings) != cached.hash {
                        return Err(format!("第 {} 版的雜湊與內容不符", cached.rev));
                    }
                }
                Ok(Some(c))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut c = self.clone();
        c.schema = CACHE_SCHEMA.to_string();
        let bytes = serde_json::to_vec_pretty(&c).map_err(std::io::Error::other)?;
        crate::remote::write_atomic(
            path,
            &bytes,
            crate::remote::FILE_MODE,
            crate::remote::DIR_MODE,
        )
    }
}

// ---------------------------------------------------------------------------
// 執行期狀態
// ---------------------------------------------------------------------------

/// 最近一次處理的結果（`config_status` 的 `rev`、`hash`、`state`、`errors`、`code`）
#[derive(Debug, Clone, PartialEq)]
struct LastStatus {
    rev: u64,
    hash: String,
    state: &'static str,
    errors: Vec<KeyError>,
    ignored_extra: Vec<IgnoredKey>,
    code: Option<&'static str>,
}

/// 確認期（第 3.6 節）
#[derive(Debug, Clone, Copy)]
struct Confirm {
    rev: u64,
    /// 即時套用：這個時間前沒有斷線即確認
    until: Instant,
    /// 需要一次成功的 `hello_ack`（需要重新連線的鍵，或即時套用後斷線了）
    needs_hello_ack: bool,
    failures: u32,
    /// 套用時是連線中的（在連線上收到 `config`）：確認前連續無法完成 `hello_ack` 才回復。重新啟動後
    /// 接續的確認期（`pending_apply` 還在）不計：Kairos 可能剛好停機，崩潰由 `starts` 判斷
    count_failures: bool,
}

/// [`CentralConfig::apply`] 的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApplyOutcome {
    /// 收到的 `config` 合法（`hash` 相符）；`false` 時當作沒有收到，不送 `config_status`
    pub accepted_message: bool,
    /// 改變了需要重新連線的鍵：送出 `config_status` 後結束目前的連線（統一重連一次）
    pub reconnect: bool,
}

/// [`CentralConfig::on_registered`] 的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegisteredOutcome {
    /// 送一次 `config_status`（協商了 `config` 的每條新連線）
    pub send_status: bool,
    /// 送 `config_get`（`pending.config_rev` 與已套用的版本不同）
    pub fetch: bool,
    /// 生效的設定改變了需要重新連線的鍵（例如連上不支援 `config` 的 Kairos，改回本機設定）
    pub reconnect: bool,
}

/// `pulse status`／`pulse config show` 用的摘要
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigView {
    /// `enabled`、`disabled`
    pub local: String,
    /// `unused`（沒有下發或 Kairos 不支援）、`in_use`
    pub usage: String,
    pub rev: u64,
    pub state: String,
    pub code: Option<String>,
    pub overridden: Vec<String>,
    pub cache_error: Option<String>,
}

struct Inner {
    enabled: bool,
    cache_path: Option<PathBuf>,
    cache: CacheFile,
    cache_error: Option<String>,
    local: AppConfig,
    defaults: AppConfig,
    sources: LocalSources,
    /// 使用下發的設定（啟動時有快取；連上不支援 `config` 的 Kairos 時暫停）
    using_remote: bool,
    merged: MergeOutcome,
    last: LastStatus,
    confirm: Option<Confirm>,
    /// 上次記 `config.overridden` 時被覆蓋的鍵
    logged_overridden: BTreeSet<String>,
}

/// 集中設定的狀態（跨連線保留；`Arc` 共用）
pub struct CentralConfig {
    inner: Mutex<Inner>,
    live: watch::Sender<Arc<AppConfig>>,
}

impl std::fmt::Debug for CentralConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CentralConfig").finish_non_exhaustive()
    }
}

fn now_rfc3339() -> String {
    crate::protocol::now_ts()
}

impl CentralConfig {
    /// 啟動時建立：讀快取（損毀時忽略、記 `PLS-E0506`）、崩潰保護（第 3.6 節），以快取的版本與本機設定
    /// 合併出生效的設定。`cache_path` 為 `None` 時不使用快取（測試或找不到狀態目錄）
    pub fn new(local: AppConfig, sources: LocalSources, cache_path: Option<PathBuf>) -> Arc<Self> {
        let defaults = AppConfig::embedded_defaults();
        let enabled = local.remote_config.enabled;
        let mut cache_error = None;
        let mut cache = match cache_path.as_deref().map(CacheFile::load) {
            Some(Ok(Some(c))) => c,
            Some(Ok(None)) | None => CacheFile::default(),
            Some(Err(e)) => {
                let path = cache_path.as_deref().unwrap_or(Path::new(""));
                log::error!(
                    "{}遠端設定的快取 {} 無法使用（{e}），忽略快取、改用本機設定；重新連上 Kairos 後會重新取得  path={} error={}",
                    codes::tag(
                        codes::CONFIG_CACHE_UNAVAILABLE,
                        Some(codes::E_REMOTE_CONFIG_CACHE)
                    ),
                    path.display(),
                    path.display(),
                    sanitize(&e)
                );
                cache_error = Some(e);
                CacheFile::default()
            }
        };
        let mut rolled_back_now = false;
        if enabled && let Some(p) = cache.pending_apply {
            let starts = p.starts.saturating_add(1);
            if starts >= CRASH_STARTS {
                // 崩潰保護：同一個 rev 已連續 2 次在確認期內結束
                rolled_back_now = true;
                Self::rollback_cache(&mut cache, p.rev);
            } else {
                cache.pending_apply = Some(PendingApply { rev: p.rev, starts });
            }
        }
        if !enabled {
            log::info!(
                "{}集中設定已在本機停用（remote_config.enabled = false）：只用本機設定，遠端快取保留不用  source=pulse.toml",
                codes::tag(
                    codes::CONFIG_DISABLED,
                    Some(codes::I_REMOTE_CONFIG_DISABLED)
                )
            );
        }
        let using_remote = enabled && cache.applied.is_some();
        let merged = Self::merge_for(&local, &defaults, &sources, &cache, using_remote);
        let last = Self::initial_status(&cache, enabled, &merged);
        let effective = Arc::new(merged.config.clone());
        let this = Arc::new(Self {
            inner: Mutex::new(Inner {
                enabled,
                cache_path,
                cache,
                cache_error,
                local,
                defaults,
                sources,
                using_remote,
                merged,
                last,
                confirm: None,
                logged_overridden: BTreeSet::new(),
            }),
            live: watch::channel(effective).0,
        });
        {
            let mut inner = this.lock();
            if rolled_back_now {
                let rb = inner.cache.rolled_back.clone().unwrap_or(RolledBack {
                    rev: 0,
                    hash: String::new(),
                    restored_rev: 0,
                    reported: false,
                });
                log::warn!(
                    "{}第 {} 版的集中設定套用後連續 {CRASH_STARTS} 次在確認前結束（崩潰或被重新啟動），改以第 {} 版（上一份可用的設定）啟動  rev={} restored_rev={} reason=crash",
                    codes::tag(
                        codes::CONFIG_ROLLED_BACK,
                        Some(codes::W_REMOTE_CONFIG_ROLLED_BACK)
                    ),
                    rb.rev,
                    rb.restored_rev,
                    rb.rev,
                    rb.restored_rev
                );
            }
            // 重新啟用後，下次停用時要再通知一次
            let reenabled = inner.enabled && inner.cache.disabled_reported;
            if reenabled {
                inner.cache.disabled_reported = false;
            }
            if rolled_back_now || reenabled || inner.cache.pending_apply.is_some() {
                Self::persist(&mut inner);
            }
            // 套用後尚未確認就重新啟動：第一次成功的 hello_ack 確認這一版（之後清除 pending_apply）
            if inner.enabled
                && let Some(p) = inner.cache.pending_apply
                && inner.cache.applied.as_ref().is_some_and(|a| a.rev == p.rev)
            {
                inner.confirm = Some(Confirm {
                    rev: p.rev,
                    until: Instant::now(),
                    needs_hello_ack: true,
                    failures: 0,
                    count_failures: false,
                });
            }
            if let Some(a) = &inner.cache.applied
                && inner.using_remote
            {
                log::info!(
                    "以快取的集中設定第 {} 版啟動（{}，{} 個鍵）",
                    a.rev,
                    inner
                        .cache_path
                        .as_deref()
                        .map_or_else(String::new, |p| p.display().to_string()),
                    settings::flatten(&a.settings).map_or(0, |f| f.len())
                );
            }
        }
        this
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn merge_for(
        local: &AppConfig,
        defaults: &AppConfig,
        sources: &LocalSources,
        cache: &CacheFile,
        using_remote: bool,
    ) -> MergeOutcome {
        let applied = cache.applied.as_ref().filter(|_| using_remote);
        let m = merge(
            local,
            defaults,
            sources,
            applied.map(|a| &a.settings),
            applied.map(|a| &a.sources),
        );
        if m.errors.is_empty() {
            m
        } else {
            // 快取的版本與目前的本機設定合併後不合法（例如本機設定改了）：只用本機設定
            log::warn!(
                "{}快取的集中設定第 {} 版與目前的本機設定合併後不合法（{}），暫時只用本機設定",
                codes::tag(
                    codes::CONFIG_REJECTED,
                    Some(codes::W_REMOTE_CONFIG_REJECTED)
                ),
                applied.map_or(0, |a| a.rev),
                m.errors
                    .iter()
                    .map(|e| format!("{}：{}", e.key, e.message))
                    .collect::<Vec<_>>()
                    .join("；")
            );
            merge(local, defaults, sources, None, None)
        }
    }

    fn initial_status(cache: &CacheFile, enabled: bool, merged: &MergeOutcome) -> LastStatus {
        let (rev, hash) = cache
            .applied
            .as_ref()
            .map_or((0, String::new()), |a| (a.rev, a.hash.clone()));
        if !enabled {
            return LastStatus {
                rev,
                hash,
                state: CONFIG_STATE_DISABLED,
                errors: Vec::new(),
                ignored_extra: Vec::new(),
                code: Some(codes::I_REMOTE_CONFIG_DISABLED),
            };
        }
        if let Some(rb) = cache.rolled_back.as_ref().filter(|r| !r.reported) {
            return LastStatus {
                rev: rb.rev,
                hash: rb.hash.clone(),
                state: CONFIG_STATE_ROLLED_BACK,
                errors: Vec::new(),
                ignored_extra: Vec::new(),
                code: Some(codes::W_REMOTE_CONFIG_ROLLED_BACK),
            };
        }
        LastStatus {
            rev,
            hash,
            state: CONFIG_STATE_APPLIED,
            errors: Vec::new(),
            ignored_extra: Vec::new(),
            code: (!merged.overridden.is_empty()).then_some(codes::I_REMOTE_CONFIG_OVERRIDDEN),
        }
    }

    /// 把快取改為回復到 last-known-good（`rev` 為失敗的版本）
    fn rollback_cache(cache: &mut CacheFile, rev: u64) {
        let restored = cache.last_known_good.clone();
        let hash = cache
            .applied
            .as_ref()
            .filter(|a| a.rev == rev)
            .map(|a| a.hash.clone())
            .unwrap_or_default();
        cache.rolled_back = Some(RolledBack {
            rev,
            hash,
            restored_rev: restored.as_ref().map_or(0, |c| c.rev),
            reported: false,
        });
        cache.applied = restored;
        cache.skip_rev = Some(rev);
        cache.pending_apply = None;
    }

    /// 寫快取；失敗時記 `PLS-E0506`（ERROR）並回傳 `false`
    fn persist(inner: &mut Inner) -> bool {
        let Some(path) = inner.cache_path.clone() else {
            return false;
        };
        match inner.cache.save(&path) {
            Ok(()) => {
                inner.cache_error = None;
                true
            }
            Err(e) => {
                let e = e.to_string();
                if inner.cache_error.as_deref() != Some(e.as_str()) {
                    log::error!(
                        "{}無法寫入遠端設定的快取 {}（{e}），改用本機設定；請確認狀態目錄可由服務帳號寫入  path={} error={}",
                        codes::tag(
                            codes::CONFIG_CACHE_UNAVAILABLE,
                            Some(codes::E_REMOTE_CONFIG_CACHE)
                        ),
                        path.display(),
                        path.display(),
                        sanitize(&e)
                    );
                }
                inner.cache_error = Some(e);
                false
            }
        }
    }

    /// 本機設定重新載入（`pulse enroll` 改寫了 `pulse.toml`）：以新的本機設定重新合併，回傳生效的設定
    pub fn replace_local(&self, local: AppConfig, sources: LocalSources) -> Arc<AppConfig> {
        let mut inner = self.lock();
        inner.local = local;
        inner.sources = sources;
        let merged = Self::merge_for(
            &inner.local,
            &inner.defaults,
            &inner.sources,
            &inner.cache,
            inner.using_remote,
        );
        let _ = self.switch_to(&mut inner, merged);
        // 本機設定改變時（含與快取無關的區段）一律發布
        self.publish(&inner);
        drop(inner);
        self.effective()
    }

    /// 目前生效的設定
    pub fn effective(&self) -> Arc<AppConfig> {
        self.live.borrow().clone()
    }

    /// 生效的設定改變時通知（即時的鍵由訂閱者就地套用）
    pub fn subscribe(&self) -> watch::Receiver<Arc<AppConfig>> {
        self.live.subscribe()
    }

    /// 本機開關（`remote_config.enabled`）
    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    /// hello 是否宣告 `config`：啟用時，或停用後尚未通知過 Kairos（送一次 `state: disabled`）
    pub fn declare(&self) -> bool {
        let inner = self.lock();
        inner.enabled || !inner.cache.disabled_reported
    }

    /// hello 的 `install.config`
    pub fn install_state(&self) -> &'static str {
        if self.lock().enabled {
            mgmt::INSTALL_CONFIG_ENABLED
        } else {
            mgmt::INSTALL_CONFIG_DISABLED
        }
    }

    /// 已套用（或回復後）的版本
    pub fn applied_rev(&self) -> u64 {
        self.lock().cache.applied.as_ref().map_or(0, |a| a.rev)
    }

    /// 確認期內（升級的交付等到確認期結束，REMOTE.md 第 4.6 節）
    pub fn confirming(&self) -> bool {
        self.lock().confirm.is_some()
    }

    fn publish(&self, inner: &Inner) {
        let new = Arc::new(inner.merged.config.clone());
        self.live.send_replace(new);
    }

    /// 生效的設定改為 `merged`，回傳是否改變了需要重新連線的鍵（並記錄改變的鍵）
    fn switch_to(&self, inner: &mut Inner, merged: MergeOutcome) -> (Vec<String>, bool) {
        let changed = changed_keys(&inner.merged.effective, &merged.effective);
        inner.merged = merged;
        if !changed.is_empty() {
            self.publish(inner);
        }
        let reconnect = settings::needs_reconnect(&changed);
        (changed, reconnect)
    }

    fn log_overridden(inner: &mut Inner, rev: u64) {
        let keys: BTreeSet<String> = inner
            .merged
            .overridden
            .iter()
            .map(|o| o.key.clone())
            .collect();
        if keys == inner.logged_overridden {
            return;
        }
        if !keys.is_empty() {
            let source = if inner
                .merged
                .overridden
                .iter()
                .all(|o| o.source == mgmt::OVERRIDE_SOURCE_ENV)
            {
                "env"
            } else {
                "file"
            };
            log::info!(
                "{}集中設定第 {rev} 版有 {} 個鍵由本機設定覆蓋（{}）；要以集中設定為準時，從 pulse.toml 或環境變數移除這些鍵  rev={rev} keys={} source={source}",
                codes::tag(
                    codes::CONFIG_OVERRIDDEN,
                    Some(codes::I_REMOTE_CONFIG_OVERRIDDEN)
                ),
                keys.len(),
                keys.iter().cloned().collect::<Vec<_>>().join("、"),
                keys.iter().cloned().collect::<Vec<_>>().join(",")
            );
        }
        inner.logged_overridden = keys;
    }

    /// 新連線收到 `hello_ack` 之後。`negotiated`：這條連線協商了 `config`
    pub fn on_registered(&self, negotiated: bool, pending: Option<&Pending>) -> RegisteredOutcome {
        let mut inner = self.lock();
        let mut out = RegisteredOutcome::default();
        // 確認期：重新連線後收到 hello_ack 即確認
        if let Some(c) = inner.confirm
            && c.needs_hello_ack
        {
            self.confirm_now(&mut inner, c.rev);
        }
        if !inner.enabled {
            out.send_status = negotiated && !inner.cache.disabled_reported;
            return out;
        }
        // 連上不支援 config 的 Kairos：快取保留但不使用；連回支援的 Kairos 時恢復
        let want_remote = negotiated && inner.cache.applied.is_some();
        if want_remote != inner.using_remote {
            inner.using_remote = want_remote;
            let merged = Self::merge_for(
                &inner.local,
                &inner.defaults,
                &inner.sources,
                &inner.cache,
                want_remote,
            );
            let (changed, reconnect) = self.switch_to(&mut inner, merged);
            if !changed.is_empty() {
                log::info!(
                    "{}",
                    if want_remote {
                        format!(
                            "Kairos 支援集中設定：恢復使用快取的第 {} 版（改變的鍵：{}）",
                            inner.cache.applied.as_ref().map_or(0, |a| a.rev),
                            changed.join("、")
                        )
                    } else {
                        format!(
                            "這個 Kairos 不支援集中設定（舊版或 JSONL 儲存）：快取保留但不使用，改用本機設定（改變的鍵：{}）",
                            changed.join("、")
                        )
                    }
                );
            }
            out.reconnect = reconnect;
        }
        if !negotiated {
            return out;
        }
        out.send_status = true;
        out.fetch = pending.is_some_and(|p| self.wants(&inner, p));
        if let Some(p) = pending
            && p.config_rev == Some(0)
            && inner.cache.applied.is_some()
        {
            // 沒有任何指派：回到本機與內建預設（不需要 config_get）
            drop(inner);
            let applied = self.apply_settings(0, Map::new(), None);
            out.fetch = false;
            out.reconnect |= applied.reconnect;
        }
        out
    }

    fn wants(&self, inner: &Inner, p: &Pending) -> bool {
        let applied = inner.cache.applied.as_ref().map_or(0, |a| a.rev);
        match p.config_rev {
            Some(0) => false,
            Some(rev) => rev != applied && inner.cache.skip_rev != Some(rev),
            None => false,
        }
    }

    /// 回應 `heartbeat` 的 `ack` 帶了 `pending`：是否要送 `config_get`（或在 `config_rev = 0` 時就地回到本機設定）。
    /// 回傳 (送 `config_get`, 送 `config_status`, 需要重新連線)
    pub fn on_pending(&self, p: &Pending) -> (bool, bool, bool) {
        let inner = self.lock();
        if !inner.enabled {
            return (false, false, false);
        }
        if p.config_rev == Some(0) && inner.cache.applied.is_some() {
            drop(inner);
            let o = self.apply_settings(0, Map::new(), None);
            return (false, true, o.reconnect);
        }
        (self.wants(&inner, p), false, false)
    }

    /// 處理 `config`（REMOTE.md 第 3.5 節）
    pub fn apply(&self, msg: &RemoteConfig) -> ApplyOutcome {
        if let Err(e) = mgmt::validate_remote_config(msg) {
            log::warn!(
                "{}Kairos 送來的 config（rev={}）不合規定，當作沒有收到：{}",
                codes::tag(codes::SERVER_UNEXPECTED, Some(codes::W_SERVER_UNEXPECTED)),
                msg.rev,
                sanitize(&e)
            );
            return ApplyOutcome::default();
        }
        if !self.enabled() {
            return ApplyOutcome::default();
        }
        let mut o = self.apply_settings(msg.rev, msg.settings.clone(), msg.sources.clone());
        o.accepted_message = true;
        o
    }

    fn apply_settings(
        &self,
        rev: u64,
        settings_map: Map<String, Value>,
        remote_sources: Option<BTreeMap<String, String>>,
    ) -> ApplyOutcome {
        let mut inner = self.lock();
        let hash = if rev == 0 && settings_map.is_empty() {
            String::new()
        } else {
            mgmt::settings_hash(&settings_map)
        };
        let remote_sources = remote_sources.unwrap_or_default();
        let kept = inner.cache.applied.as_ref().map_or(0, |a| a.rev);
        let merged = merge(
            &inner.local,
            &inner.defaults,
            &inner.sources,
            Some(&settings_map),
            Some(&remote_sources),
        );
        if !merged.errors.is_empty() {
            let first = &merged.errors[0];
            log::warn!(
                "{}第 {rev} 版的集中設定驗證失敗，整份拒絕、沿用第 {kept} 版：{}  rev={rev} key={} reason={} kept_rev={kept}",
                codes::tag(
                    codes::CONFIG_REJECTED,
                    Some(codes::W_REMOTE_CONFIG_REJECTED)
                ),
                merged
                    .errors
                    .iter()
                    .map(|e| format!("{} {}（{}）", e.key, e.reason, e.message))
                    .collect::<Vec<_>>()
                    .join("；"),
                first.key,
                first.reason
            );
            inner.last = LastStatus {
                rev,
                hash,
                state: CONFIG_STATE_REJECTED,
                errors: merged.errors.clone(),
                ignored_extra: merged.ignored.clone(),
                code: Some(codes::W_REMOTE_CONFIG_REJECTED),
            };
            return ApplyOutcome::default();
        }
        // 先寫快取（寫暫存檔再改名）並記下 pending_apply，才套用
        let previous_cache = inner.cache.clone();
        let entry = CachedConfig {
            rev,
            hash: hash.clone(),
            settings: settings_map,
            sources: remote_sources,
            applied_at: now_rfc3339(),
        };
        let empty = rev == 0 && entry.settings.is_empty();
        inner.cache.applied = (!empty).then_some(entry);
        inner.cache.skip_rev = None;
        inner.cache.rolled_back = None;
        inner.cache.pending_apply = (!empty).then_some(PendingApply { rev, starts: 0 });
        if inner.cache_path.is_some() && !Self::persist(&mut inner) {
            inner.cache = previous_cache;
            inner.last = LastStatus {
                rev,
                hash,
                state: CONFIG_STATE_REJECTED,
                errors: Vec::new(),
                ignored_extra: Vec::new(),
                code: Some(codes::E_REMOTE_CONFIG_CACHE),
            };
            return ApplyOutcome::default();
        }
        if inner.cache_path.is_none() {
            // 沒有快取位置（測試）：只在記憶體中
        }
        let old = inner.merged.effective.clone();
        inner.using_remote = !empty;
        let (changed, reconnect) = self.switch_to(&mut inner, merged);
        let changes: Vec<String> = changed
            .iter()
            .map(|k| {
                format!(
                    "{k}：{} → {}",
                    settings::get(&old, k).map_or_else(|| "—".into(), Value::to_string),
                    settings::get(&inner.merged.effective, k)
                        .map_or_else(|| "—".into(), Value::to_string)
                )
            })
            .collect();
        log::info!(
            "{}已套用集中設定第 {rev} 版{}{}  rev={rev} changed_keys={} restart_required=false",
            codes::tag(codes::CONFIG_APPLIED, None),
            if changes.is_empty() {
                "（生效的值沒有改變）".to_string()
            } else {
                format!("：{}", changes.join("；"))
            },
            if reconnect {
                "；重新連線以套用"
            } else {
                ""
            },
            changed.join(",")
        );
        Self::log_overridden(&mut inner, rev);
        let hb = Duration::from_secs(inner.merged.config.heartbeat.interval);
        inner.confirm = if empty {
            None
        } else {
            Some(Confirm {
                rev,
                until: Instant::now() + MIN_CONFIRM.max(hb.saturating_mul(3)),
                needs_hello_ack: reconnect,
                failures: 0,
                count_failures: true,
            })
        };
        let ignored = inner.merged.ignored.clone();
        inner.last = LastStatus {
            rev,
            hash,
            state: CONFIG_STATE_APPLIED,
            errors: Vec::new(),
            ignored_extra: ignored,
            code: (!inner.merged.overridden.is_empty())
                .then_some(codes::I_REMOTE_CONFIG_OVERRIDDEN),
        };
        ApplyOutcome {
            accepted_message: true,
            reconnect,
        }
    }

    fn confirm_now(&self, inner: &mut Inner, rev: u64) {
        inner.confirm = None;
        if inner.cache.applied.as_ref().is_some_and(|a| a.rev == rev) {
            inner.cache.last_known_good = inner.cache.applied.clone();
            inner.cache.pending_apply = None;
            Self::persist(inner);
            log::debug!("集中設定第 {rev} 版已確認可用（last-known-good）");
        }
    }

    /// 定時呼叫（連線中）：即時套用的版本在確認期內沒有斷線即確認
    pub fn on_tick(&self, now: Instant) {
        let mut inner = self.lock();
        if let Some(c) = inner.confirm
            && !c.needs_hello_ack
            && now >= c.until
        {
            self.confirm_now(&mut inner, c.rev);
        }
    }

    /// 下一次需要 [`CentralConfig::on_tick`] 的時間
    pub fn next_deadline(&self) -> Option<Instant> {
        let inner = self.lock();
        inner
            .confirm
            .filter(|c| !c.needs_hello_ack)
            .map(|c| c.until)
    }

    /// 已註冊的連線結束（斷線）：即時套用的確認期改為需要一次成功的 `hello_ack`
    pub fn on_disconnected(&self) {
        let mut inner = self.lock();
        if let Some(c) = inner.confirm.as_mut() {
            c.needs_hello_ack = true;
        }
    }

    /// 收到 `hello_ack` 之前連線失敗：確認期內連續 3 次時回復 last-known-good。回傳是否回復了
    /// （生效的設定改變，呼叫端重建取樣與連線）
    pub fn on_connect_failed(&self) -> bool {
        let mut inner = self.lock();
        let Some(c) = inner.confirm.as_mut().filter(|c| c.count_failures) else {
            return false;
        };
        c.needs_hello_ack = true;
        c.failures += 1;
        if c.failures < CONFIRM_FAILURES {
            return false;
        }
        let rev = c.rev;
        inner.confirm = None;
        Self::rollback_cache(&mut inner.cache, rev);
        Self::persist(&mut inner);
        let (restored, failed_hash) = inner
            .cache
            .rolled_back
            .as_ref()
            .map_or((0, String::new()), |r| (r.restored_rev, r.hash.clone()));
        let using = inner.cache.applied.is_some();
        inner.using_remote = using;
        let merged = Self::merge_for(
            &inner.local,
            &inner.defaults,
            &inner.sources,
            &inner.cache,
            using,
        );
        let (changed, _) = self.switch_to(&mut inner, merged);
        log::warn!(
            "{}套用集中設定第 {rev} 版後連續 {CONFIRM_FAILURES} 次無法完成註冊，已回復第 {restored} 版（上一份可用的設定），同一版不再自動套用（改變的鍵：{}）  rev={rev} restored_rev={restored} reason=connect_failed",
            codes::tag(
                codes::CONFIG_ROLLED_BACK,
                Some(codes::W_REMOTE_CONFIG_ROLLED_BACK)
            ),
            if changed.is_empty() {
                "無".to_string()
            } else {
                changed.join("、")
            }
        );
        inner.last = LastStatus {
            rev,
            hash: failed_hash,
            state: CONFIG_STATE_ROLLED_BACK,
            errors: Vec::new(),
            ignored_extra: Vec::new(),
            code: Some(codes::W_REMOTE_CONFIG_ROLLED_BACK),
        };
        true
    }

    /// 要送的 `config_status`（狀態為最近一次處理的結果；`effective` 等為目前的值）
    pub fn status_message(&self, seq: u64) -> ConfigStatus {
        let inner = self.lock();
        let mut status = ConfigStatus {
            seq,
            ts: now_rfc3339(),
            rev: inner.last.rev,
            hash: inner.last.hash.clone(),
            state: inner.last.state.to_string(),
            effective: inner.merged.effective.clone(),
            overridden: if inner.last.state == CONFIG_STATE_APPLIED {
                inner.merged.overridden.clone()
            } else {
                Vec::new()
            },
            ignored: if inner.last.state == CONFIG_STATE_APPLIED {
                inner.merged.ignored.clone()
            } else {
                inner.last.ignored_extra.clone()
            },
            errors: inner.last.errors.clone(),
            code: inner.last.code.map(str::to_string),
        };
        if status.hash.is_empty() && status.rev != 0 {
            // 規格：hash 只在 rev 為 0 時可以是空字串（舊版快取沒有記下失敗版本的雜湊時）
            status.hash = mgmt::settings_hash(&Map::new());
        }
        status.truncate_lists();
        status
    }

    /// `config_status` 收到 `ack`：停用的通知、回復的回報只送一次
    pub fn on_status_acked(&self, state: &str) {
        let mut inner = self.lock();
        let mut dirty = false;
        if state == CONFIG_STATE_DISABLED && !inner.cache.disabled_reported {
            inner.cache.disabled_reported = true;
            dirty = true;
        }
        if state == CONFIG_STATE_ROLLED_BACK
            && let Some(rb) = inner.cache.rolled_back.as_mut()
            && !rb.reported
        {
            rb.reported = true;
            dirty = true;
            // 回報過後，之後的連線回報目前套用的版本
            let (rev, hash) = inner
                .cache
                .applied
                .as_ref()
                .map_or((0, String::new()), |a| (a.rev, a.hash.clone()));
            inner.last = LastStatus {
                rev,
                hash,
                state: CONFIG_STATE_APPLIED,
                errors: Vec::new(),
                ignored_extra: Vec::new(),
                code: (!inner.merged.overridden.is_empty())
                    .then_some(codes::I_REMOTE_CONFIG_OVERRIDDEN),
            };
        }
        if dirty {
            Self::persist(&mut inner);
        }
    }

    /// `pulse status`、`pulse config show` 用的摘要
    pub fn view(&self) -> ConfigView {
        let inner = self.lock();
        ConfigView {
            local: if inner.enabled {
                mgmt::INSTALL_CONFIG_ENABLED
            } else {
                mgmt::INSTALL_CONFIG_DISABLED
            }
            .to_string(),
            usage: if inner.using_remote {
                "in_use"
            } else {
                "unused"
            }
            .to_string(),
            rev: inner.last.rev,
            state: inner.last.state.to_string(),
            code: inner.last.code.map(str::to_string),
            overridden: inner
                .merged
                .overridden
                .iter()
                .map(|o| o.key.clone())
                .collect(),
            cache_error: inner.cache_error.clone(),
        }
    }
}

/// 日誌用：移除控制字元並截斷
fn sanitize(s: &str) -> String {
    crate::network::sanitize_for_log(s, 300)
}

/// 讀快取並與本機設定合併出生效的設定（`pulse check-config --with-remote`、`pulse config show` 等
/// 不執行代理的命令；不寫快取、不做崩潰保護）
pub fn effective_from_cache(
    local: &AppConfig,
    sources: &LocalSources,
    cache_path: &Path,
) -> Result<(MergeOutcome, Option<CachedConfig>), String> {
    let defaults = AppConfig::embedded_defaults();
    let cache = CacheFile::load(cache_path)?.unwrap_or_default();
    let applied = cache.applied.filter(|_| local.remote_config.enabled);
    let m = merge(
        local,
        &defaults,
        sources,
        applied.as_ref().map(|a| &a.settings),
        applied.as_ref().map(|a| &a.sources),
    );
    Ok((m, applied))
}

#[cfg(test)]
mod tests;
