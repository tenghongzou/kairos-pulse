//! 訊息型別與 JSON 解析。
//!
//! 兩個方向的訊息都同時實作序列化與反序列化：Kairos 解析 [`ClientMsg`]、送出 [`ServerMsg`]，
//! Pulse 相反。解析時忽略未知欄位（不使用 `deny_unknown_fields`），以便日後擴充。
//!
//! v2 新增的欄位（hello 的版本範圍、features 與主機資訊；report 的 `metrics`）以
//! [`HelloExt`]、[`ReportBody`] 收集，**解析時不會讓整則訊息失敗**：這些欄位型別錯誤時只記下
//! 錯誤原因，由伺服器依協商結果決定是否使用。協商結果為 v1 時，v2 欄位與 v1 Kairos 一樣被忽略，
//! v1 的行為（哪些訊息被接受、錯誤碼與 seq）因此不變。

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::fmt;
use std::io::{Error, ErrorKind};

use crate::metrics::Metric;

/// 日誌中顯示無效 `proto` 原始值的長度上限（字元）
pub(crate) const MAX_PROTO_DISPLAY_CHARS: usize = 32;

/// hello 的 `proto` 欄位。不是合法 u32 的值（字串、負數、小數、超出 u32）不視為格式錯誤，
/// 而是「不受支援的版本」，回 `unsupported_version`。
///
/// 序列化時 [`Proto::Version`] 輸出為 JSON 數字；[`Proto::Invalid`] 只出現在解析結果中，
/// 不能序列化（回傳錯誤）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proto {
    Version(u32),
    /// 不是合法 u32；內含截短後的原始 JSON 文字（只供日誌）
    Invalid(String),
}

impl fmt::Display for Proto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Proto::Version(v) => write!(f, "{}", v),
            Proto::Invalid(raw) => write!(f, "{}", raw.escape_debug()),
        }
    }
}

impl Serialize for Proto {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Proto::Version(v) => s.serialize_u32(*v),
            Proto::Invalid(_) => Err(serde::ser::Error::custom(
                "an invalid proto value cannot be serialized",
            )),
        }
    }
}

fn de_proto<'de, D: Deserializer<'de>>(d: D) -> Result<Proto, D::Error> {
    let value = Value::deserialize(d)?;
    Ok(match value.as_u64().and_then(|n| u32::try_from(n).ok()) {
        Some(n) => Proto::Version(n),
        None => Proto::Invalid(
            value
                .to_string()
                .chars()
                .take(MAX_PROTO_DISPLAY_CHARS)
                .collect(),
        ),
    })
}

/// `heartbeat_interval_ms`：任何 JSON 數字都接受（規格規定超出範圍不視為錯誤），
/// 負數視為 0、小數捨去、超出 u64 視為 `u64::MAX`，之後再由
/// [`clamp_heartbeat_interval_ms`](crate::clamp_heartbeat_interval_ms) 夾值。
/// 不是數字（字串、null 等）仍是格式錯誤。
fn de_heartbeat_interval<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    match Value::deserialize(d)? {
        Value::Number(n) => Ok(n
            .as_u64()
            // `as` 轉換會飽和：負數 → 0、過大 → u64::MAX
            .unwrap_or_else(|| n.as_f64().map_or(0, |f| f as u64))),
        _ => Err(serde::de::Error::custom(
            "heartbeat_interval_ms must be a number",
        )),
    }
}

/// hello 的版本協商欄位（v2，A3）。三者都可省略：省略 `proto_min`／`proto_max` 時，
/// client 支援的範圍就是 `proto` 單一版本（v1 client 的行為）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtoOffer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto_max: Option<u32>,
    /// client 支援的選用功能（例如 [`FEATURE_METRICS`](crate::FEATURE_METRICS)）；不認得的名稱忽略
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
}

/// hello 的作業系統資訊（v2）。各欄位都可省略，物件中的其他欄位忽略。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsInfo {
    /// 作業系統名稱，例如 `Ubuntu`、`Windows`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 作業系統版本，例如 `24.04`、`11 (26200)`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 核心版本，例如 `6.8.0-45-generic`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// CPU 架構，例如 `x86_64`、`aarch64`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
}

impl OsInfo {
    /// 所有欄位都沒有值
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.version.is_none()
            && self.kernel.is_none()
            && self.arch.is_none()
    }
}

/// hello 的主機資訊（v2；設計包 `enrollment-ux.md` §12 #5、`screens.md` §24 #15）。
/// 全部可省略；協商結果為 v1 時伺服器不讀取。限制見 [`validate_host_info`](crate::validate_host_info)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// 主機指紋（`<演算法>:<小寫十六進位>`，Pulse 為 `sha256:` 加 64 個字元），不含原始的機器 ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_fingerprint: Option<String>,
    /// 主控台顯示名稱（未設定時以 `hostname` 顯示）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 標籤，例如 `env:prod`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// client 宣告的 report 間隔（毫秒），供伺服器判定回報延遲
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_interval_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<OsInfo>,
}

/// 把 v2 欄位的各部分合併序列化到外層物件（搭配 `#[serde(flatten)]`）
fn serialize_merged<S: Serializer>(
    parts: &[Result<Value, String>],
    s: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::{Error as _, SerializeMap};
    let mut map = s.serialize_map(None)?;
    for part in parts {
        match part {
            Ok(Value::Object(fields)) => {
                for (k, v) in fields {
                    map.serialize_entry(k, v)?;
                }
            }
            Ok(_) => return Err(S::Error::custom("v2 fields must serialize to an object")),
            Err(e) => return Err(S::Error::custom(e)),
        }
    }
    map.end()
}

/// 以 `T` 解析外層物件中其餘的欄位；失敗時保留錯誤說明，不讓整則訊息失敗
fn parse_part<T: for<'a> Deserialize<'a>>(fields: &Value) -> Result<T, String> {
    T::deserialize(fields).map_err(|e| e.to_string())
}

/// 序列化 v2 欄位的一部分；`Err`（解析失敗的內容）不能序列化
fn to_part<T: Serialize>(part: &Result<T, String>) -> Result<Value, String> {
    match part {
        Ok(v) => serde_json::to_value(v).map_err(|e| e.to_string()),
        Err(e) => Err(format!("an invalid field cannot be serialized: {e}")),
    }
}

/// hello 中 v1 以外的欄位（v2 的版本協商與主機資訊）。
///
/// 兩部分各自解析：型別錯誤只讓該部分變成 `Err`（內含錯誤說明），hello 本身仍可解析，
/// 由伺服器依協商結果決定是否回 `bad_request`。`Err` 的部分不能序列化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloExt {
    /// `proto_min`、`proto_max`、`features`
    pub offer: Result<ProtoOffer, String>,
    /// `host_fingerprint`、`display_name`、`tags`、`report_interval_ms`、`os`
    pub host: Result<HostInfo, String>,
}

impl HelloExt {
    /// v2 client 的 hello 欄位
    pub fn new(offer: ProtoOffer, host: HostInfo) -> Self {
        Self {
            offer: Ok(offer),
            host: Ok(host),
        }
    }
}

impl Default for HelloExt {
    /// v1 hello：沒有任何 v2 欄位
    fn default() -> Self {
        Self::new(ProtoOffer::default(), HostInfo::default())
    }
}

impl Serialize for HelloExt {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_merged(&[to_part(&self.offer), to_part(&self.host)], s)
    }
}

impl<'de> Deserialize<'de> for HelloExt {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let fields = Value::Object(Map::deserialize(d)?);
        Ok(Self {
            offer: parse_part(&fields),
            host: parse_part(&fields),
        })
    }
}

/// v1 report 的三個欄位（v2 未協商 [`FEATURE_METRICS`](crate::FEATURE_METRICS) 時也使用）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyReport {
    /// 全機 CPU 平均使用率（%，有限值、0～100）。
    ///
    /// 線上格式為 JSON 數字，型別統一為 f64（A14）：Pulse 以 f64 送出已四捨五入到小數 2 位的值
    /// （f32 直接序列化會出現 `7.5000005` 這類精度雜訊）；Kairos 以
    /// [`server_cpu_usage`](crate::server_cpu_usage) 轉成 f32 後驗證與保存，
    /// 接受範圍與 JSONL 的輸出都與先前以 f32 反序列化時相同。
    pub cpu_usage: f64,
    /// 總記憶體（MB）
    pub total_memory: u64,
    /// 已使用記憶體（MB）
    pub used_memory: u64,
}

#[derive(Serialize, Deserialize)]
struct MetricsPart {
    metrics: Vec<Metric>,
}

/// durable report 的投遞識別（`boot_id`、`report_seq`；雙方都宣告
/// [`FEATURE_DURABLE`](crate::FEATURE_DURABLE) 時才送，見 PROTOCOL.md「durable」）。
///
/// 伺服器以 (agent_id, `boot_id`, `report_seq`) 去重：同一筆重送時兩個值與 `ts`、內容都不變，
/// 已保存過的那筆回 `ack` 但不再寫入。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DurableSeq {
    /// client 產生的 UUID（Pulse：每次程序啟動時產生，見 PROTOCOL.md）
    pub boot_id: String,
    /// 同一個 `boot_id` 內單調遞增，從 1 開始
    pub report_seq: u64,
}

/// report 中 durable 的兩個欄位；`null` 視同省略
#[derive(Deserialize)]
struct DurablePart {
    #[serde(default)]
    boot_id: Option<String>,
    #[serde(default)]
    report_seq: Option<u64>,
}

impl DurablePart {
    fn into_seq(self) -> Result<Option<DurableSeq>, String> {
        match (self.boot_id, self.report_seq) {
            (None, None) => Ok(None),
            (Some(boot_id), Some(report_seq)) => Ok(Some(DurableSeq {
                boot_id,
                report_seq,
            })),
            _ => Err("boot_id and report_seq must be given together".to_string()),
        }
    }
}

/// report 的內容：v1 的三個欄位與 v2 的 `metrics`，兩者各自解析。
///
/// 伺服器依連線協商的格式取用（[`ReportBody::legacy_fields`] 或 [`ReportBody::metric_list`]），
/// 另一種格式的欄位一律忽略（與 v1 Kairos 忽略未知欄位相同）。序列化時只輸出 `Ok` 的部分。
///
/// durable 的 `boot_id`／`report_seq` 也在這裡另外解析（[`ReportBody::durable_seq`]），
/// 只在協商了 durable 的連線上使用，其他連線忽略（型別錯誤也不檢查）。
#[derive(Debug, Clone, PartialEq)]
pub struct ReportBody {
    pub legacy: Result<LegacyReport, String>,
    pub metrics: Result<Vec<Metric>, String>,
    /// durable 的投遞識別；兩個欄位都省略時為 `Ok(None)`
    pub durable: Result<Option<DurableSeq>, String>,
}

impl ReportBody {
    /// v1 格式的 report 內容
    pub fn legacy(cpu_usage: f64, total_memory: u64, used_memory: u64) -> Self {
        Self {
            legacy: Ok(LegacyReport {
                cpu_usage,
                total_memory,
                used_memory,
            }),
            metrics: Err("missing field `metrics`".to_string()),
            durable: Ok(None),
        }
    }

    /// v2 通用指標格式的 report 內容
    pub fn metrics(metrics: Vec<Metric>) -> Self {
        Self {
            legacy: Err("missing field `cpu_usage`".to_string()),
            metrics: Ok(metrics),
            durable: Ok(None),
        }
    }

    /// 附上 durable 的投遞識別（durable 連線上的 report）
    pub fn with_durable(mut self, durable: DurableSeq) -> Self {
        self.durable = Ok(Some(durable));
        self
    }

    /// v1 格式的欄位；缺漏或型別錯誤時回傳 `bad_request` 的說明
    pub fn legacy_fields(&self) -> Result<&LegacyReport, String> {
        self.legacy
            .as_ref()
            .map_err(|e| format!("invalid message: {e}"))
    }

    /// v2 的 `metrics`；缺漏或型別錯誤時回傳 `bad_request` 的說明
    pub fn metric_list(&self) -> Result<&[Metric], String> {
        self.metrics
            .as_ref()
            .map(Vec::as_slice)
            .map_err(|e| format!("invalid message: {e}"))
    }

    /// durable 連線上的 `boot_id`／`report_seq`：缺漏、只有其中一個、型別錯誤或不符限制
    /// （[`validate_durable_seq`](crate::validate_durable_seq)）時回傳 `bad_request` 的說明
    pub fn durable_seq(&self) -> Result<&DurableSeq, String> {
        match &self.durable {
            Ok(Some(seq)) => {
                crate::validate_durable_seq(seq).map_err(|e| format!("invalid message: {e}"))?;
                Ok(seq)
            }
            Ok(None) => Err(
                "invalid message: missing fields `boot_id` and `report_seq` (required when durable is negotiated)"
                    .to_string(),
            ),
            Err(e) => Err(format!("invalid message: {e}")),
        }
    }
}

impl Serialize for ReportBody {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut parts = Vec::new();
        if let Ok(legacy) = &self.legacy {
            parts.push(serde_json::to_value(legacy).map_err(|e| e.to_string()));
        }
        if let Ok(metrics) = &self.metrics {
            parts.push(
                serde_json::to_value(MetricsPart {
                    metrics: metrics.clone(),
                })
                .map_err(|e| e.to_string()),
            );
        }
        if parts.is_empty() {
            parts.push(Err(
                "a report needs either the v1 fields or metrics".to_string()
            ));
        }
        match &self.durable {
            Ok(Some(durable)) => {
                parts.push(serde_json::to_value(durable).map_err(|e| e.to_string()))
            }
            Ok(None) => {}
            Err(e) => parts.push(Err(format!("an invalid field cannot be serialized: {e}"))),
        }
        serialize_merged(&parts, s)
    }
}

impl<'de> Deserialize<'de> for ReportBody {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let fields = Value::Object(Map::deserialize(d)?);
        Ok(Self {
            legacy: parse_part(&fields),
            metrics: parse_part::<MetricsPart>(&fields).map(|p| p.metrics),
            durable: parse_part::<DurablePart>(&fields).and_then(DurablePart::into_seq),
        })
    }
}

/// Client（Pulse）→ Server（Kairos）訊息
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        seq: u64,
        /// v1 的版本欄位；v2 client 仍必須送出（v1 Kairos 只看這個欄位，見 PROTOCOL.md「版本協商」）
        #[serde(deserialize_with = "de_proto")]
        proto: Proto,
        agent_id: String,
        hostname: String,
        agent_version: String,
        #[serde(deserialize_with = "de_heartbeat_interval")]
        heartbeat_interval_ms: u64,
        ts: String,
        /// v2 欄位（v1 hello 為 [`HelloExt::default`]）。hello 每條連線只有一則，裝箱避免
        /// 所有 [`ClientMsg`] 都佔用這部分的空間
        #[serde(flatten)]
        ext: Box<HelloExt>,
    },
    Heartbeat {
        seq: u64,
        ts: String,
    },
    Report {
        seq: u64,
        ts: String,
        #[serde(flatten)]
        body: ReportBody,
    },
    /// 不認得的 `type`；`serde(other)` 只能用於 unit 變體，因此拿不到 seq。
    /// 只出現在解析結果中，不能序列化。
    #[serde(other, skip_serializing)]
    Unknown,
}

/// Server（Kairos）→ Client（Pulse）訊息
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    HelloAck {
        seq: u64,
        /// 協商出的版本（v1 Kairos 一律為 1）
        proto: u32,
        offline_after_ms: u64,
        /// 伺服器支援的選用功能（v2；協商結果為 v1 時不送）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        features: Option<Vec<String>>,
        /// 伺服器送出 hello_ack 時的時間，RFC 3339 UTC 毫秒（v2；協商結果為 v1 時不送）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_time: Option<String>,
    },
    Ack {
        seq: u64,
    },
    /// 伺服器一律送出完整的四個欄位。解析時容忍欄位缺漏、不整則忽略：缺 `seq` → null、
    /// 缺 `code` / `message` → 空字串、缺 `fatal` → 視為 fatal（保守處理：結束連線並退避重連）。
    Error {
        #[serde(default)]
        seq: Option<u64>,
        #[serde(default)]
        code: String,
        #[serde(default)]
        message: String,
        #[serde(default = "fatal_by_default")]
        fatal: bool,
        /// `unsupported_version` 時伺服器支援的版本範圍（v2 起的 Kairos；其他錯誤不送）。
        /// 不是合法 u32 時視為沒有（不讓整則 error 無法解析）
        #[serde(
            default,
            deserialize_with = "de_lenient_u32",
            skip_serializing_if = "Option::is_none"
        )]
        proto_min: Option<u32>,
        #[serde(
            default,
            deserialize_with = "de_lenient_u32",
            skip_serializing_if = "Option::is_none"
        )]
        proto_max: Option<u32>,
    },
}

/// 寬鬆的 u32：不是合法 u32 的值（字串、負數、小數、null 等）視為 `None`
fn de_lenient_u32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(Value::deserialize(d)?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok()))
}

/// error 訊息缺少 `fatal` 欄位時的預設值
fn fatal_by_default() -> bool {
    true
}

/// [`ServerMsg`] 的所有 `type` 值；不在其中的視為未知類型（見 [`parse_server_msg`]）
pub const SERVER_MSG_TYPES: [&str; 3] = ["hello_ack", "ack", "error"];

/// 規格定義的錯誤碼（只能用這些）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    UnsupportedVersion,
    HelloRequired,
    TooLong,
    Superseded,
    BadRequest,
    UnknownType,
    AlreadyRegistered,
    InvalidReport,
    /// 啟用 mTLS 時，hello 的 agent_id 與客戶端憑證不符
    AuthFailed,
    /// 連線期間重新驗證時，客戶端憑證已被撤銷、過期或不再受信任
    CertificateRejected,
    /// 伺服器無法寫入這筆 report；資料未保存
    StorageUnavailable,
}

impl ErrorCode {
    /// 所有錯誤碼（順序同 PROTOCOL.md 的錯誤碼表以外無特別意義）
    pub const ALL: [ErrorCode; 11] = [
        ErrorCode::UnsupportedVersion,
        ErrorCode::HelloRequired,
        ErrorCode::TooLong,
        ErrorCode::Superseded,
        ErrorCode::BadRequest,
        ErrorCode::UnknownType,
        ErrorCode::AlreadyRegistered,
        ErrorCode::InvalidReport,
        ErrorCode::AuthFailed,
        ErrorCode::CertificateRejected,
        ErrorCode::StorageUnavailable,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::UnsupportedVersion => "unsupported_version",
            ErrorCode::HelloRequired => "hello_required",
            ErrorCode::TooLong => "too_long",
            ErrorCode::Superseded => "superseded",
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::UnknownType => "unknown_type",
            ErrorCode::AlreadyRegistered => "already_registered",
            ErrorCode::InvalidReport => "invalid_report",
            ErrorCode::AuthFailed => "auth_failed",
            ErrorCode::CertificateRejected => "certificate_rejected",
            ErrorCode::StorageUnavailable => "storage_unavailable",
        }
    }

    /// 由 `code` 字串取得錯誤碼；不是規格定義的錯誤碼時回傳 `None`
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == code)
    }

    /// 錯誤碼預設是否為 fatal（`bad_request` 連續達上限時另行升級為 fatal）
    pub fn default_fatal(self) -> bool {
        matches!(
            self,
            ErrorCode::UnsupportedVersion
                | ErrorCode::HelloRequired
                | ErrorCode::TooLong
                | ErrorCode::Superseded
                | ErrorCode::AuthFailed
                | ErrorCode::CertificateRejected
        )
    }
}

impl ServerMsg {
    /// 建立錯誤訊息，fatal 依錯誤碼預設值
    pub fn error(seq: Option<u64>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::error_with_fatal(seq, code, message, code.default_fatal())
    }

    pub fn error_with_fatal(
        seq: Option<u64>,
        code: ErrorCode,
        message: impl Into<String>,
        fatal: bool,
    ) -> Self {
        ServerMsg::Error {
            seq,
            code: code.as_str().to_string(),
            message: message.into(),
            fatal,
            proto_min: None,
            proto_max: None,
        }
    }

    /// 此訊息送出後是否必須關閉連線
    pub fn is_fatal(&self) -> bool {
        matches!(self, ServerMsg::Error { fatal: true, .. })
    }

    /// 序列化為單行 JSON（不含換行字元，由 codec 補上）
    pub fn to_line(&self) -> Result<String, Error> {
        serde_json::to_string(self).map_err(|e| Error::new(ErrorKind::InvalidData, e))
    }
}

/// 解析一行 client 訊息（Kairos 端）。失敗時回傳給 client 的錯誤說明。
/// 不認得的 `type` 解析為 [`ClientMsg::Unknown`]（由呼叫端回 `unknown_type`）。
pub fn parse_client_msg(line: &str) -> Result<ClientMsg, String> {
    serde_json::from_str(line).map_err(|e| format!("invalid message: {}", e))
}

/// 無法解析為合法訊息的一行（欄位缺漏、型別錯誤等）：若仍是 JSON 物件、`type` 為 `"hello"`
/// 且 `seq` 為 u64，回傳該 seq，讓 `bad_request` 能帶回（F5）；否則回傳 `None`
pub fn hello_seq_of_invalid(line: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(rename = "type")]
        kind: Option<Value>,
        seq: Option<Value>,
    }
    let probe: Probe = serde_json::from_str(line).ok()?;
    if probe.kind?.as_str()? != "hello" {
        return None;
    }
    probe.seq?.as_u64()
}

/// [`parse_server_msg`] 的失敗原因
#[derive(Debug)]
pub enum ServerMsgError {
    /// 是 JSON 物件且 `type` 為字串，但不是 [`SERVER_MSG_TYPES`] 之一（例如較新版伺服器新增的訊息）。
    /// 內含原始的 `type` 值（未清理，寫進日誌前須由呼叫端處理）
    UnknownType(String),
    /// 其他無法解析的情況：不是 JSON、不是物件、缺少 `type`、已知類型的欄位缺漏或型別錯誤等
    Invalid(serde_json::Error),
}

impl fmt::Display for ServerMsgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerMsgError::UnknownType(kind) => {
                write!(f, "unknown message type {:?}", kind)
            }
            ServerMsgError::Invalid(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ServerMsgError {}

/// 解析一行 server 訊息（Pulse 端），並把「未知的訊息類型」與其他解析失敗分開（A14）：
/// 協定規定雙方忽略不認得的內容以便日後擴充，未知類型是預期中的情況，呼叫端不必以 WARN 記錄；
/// 其他失敗代表伺服器送了壞掉的訊息。兩者都不是合法訊息，處置（是否重置閒置計時等）由呼叫端決定。
pub fn parse_server_msg(line: &str) -> Result<ServerMsg, ServerMsgError> {
    serde_json::from_str(line).map_err(|e| {
        #[derive(Deserialize)]
        struct Probe {
            #[serde(rename = "type")]
            kind: Option<Value>,
        }
        match serde_json::from_str::<Probe>(line) {
            Ok(Probe {
                kind: Some(Value::String(kind)),
            }) if !SERVER_MSG_TYPES.contains(&kind.as_str()) => ServerMsgError::UnknownType(kind),
            _ => ServerMsgError::Invalid(e),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{
        Ver, ack_vector, client_vector, client_vector_in, server_error_vector, server_vector,
        server_vector_in,
    };
    use crate::{
        PROTO_V1, SUPPORTED_PROTO, offline_after_ms, server_cpu_usage, unsupported_version_error,
        validate_report,
    };
    use serde_json::json;

    fn assert_server_msg(msg: &ServerMsg, vector: &str) {
        let actual: Value = serde_json::from_str(&msg.to_line().unwrap()).unwrap();
        let expected: Value = serde_json::from_str(vector).unwrap();
        assert_eq!(actual, expected);
    }

    /// 序列化後轉成 `Value` 與測試向量比對（不比字串，避免欄位順序差異）。
    /// cpu_usage 為 f64，與測試向量逐位元比對，不需容差。
    fn assert_client_serializes_to(msg: &ClientMsg, expected: &str) {
        let actual: Value = serde_json::from_str(&serde_json::to_string(msg).unwrap()).unwrap();
        let expected: Value = serde_json::from_str(expected).unwrap();
        assert_eq!(actual, expected);
    }

    fn vector_hello() -> ClientMsg {
        ClientMsg::Hello {
            seq: 1,
            proto: Proto::Version(1),
            agent_id: "6f1c0e4a-1b2c-4d3e-9f00-112233445566".into(),
            hostname: "web-01".into(),
            agent_version: "0.1.0".into(),
            heartbeat_interval_ms: 1000,
            ts: "2026-10-08T03:15:40.001Z".into(),
            ext: Box::default(),
        }
    }

    fn vector_heartbeat() -> ClientMsg {
        ClientMsg::Heartbeat {
            seq: 2,
            ts: "2026-10-08T03:15:41.002Z".into(),
        }
    }

    fn vector_report() -> ClientMsg {
        ClientMsg::Report {
            seq: 6,
            ts: "2026-10-08T03:15:45.210Z".into(),
            body: ReportBody::legacy(17.84, 49152, 37034),
        }
    }

    // ---- 原 Kairos src/protocol.rs 的測試（client 訊息的解析、server 訊息的序列化） ----

    #[test]
    fn vector_hello_deserializes() {
        assert_eq!(
            parse_client_msg(client_vector("hello")).unwrap(),
            vector_hello()
        );
    }

    #[test]
    fn vector_heartbeat_deserializes() {
        assert_eq!(
            parse_client_msg(client_vector("heartbeat")).unwrap(),
            vector_heartbeat()
        );
    }

    #[test]
    fn vector_report_deserializes() {
        assert_eq!(
            parse_client_msg(client_vector("report")).unwrap(),
            vector_report()
        );
    }

    #[test]
    fn vector_server_messages_serialize() {
        assert_server_msg(
            &ServerMsg::HelloAck {
                seq: 1,
                proto: PROTO_V1,
                offline_after_ms: offline_after_ms(1000),
                features: None,
                server_time: None,
            },
            server_vector("hello_ack"),
        );
        assert_server_msg(&ServerMsg::Ack { seq: 2 }, ack_vector(2));
        assert_server_msg(&ServerMsg::Ack { seq: 6 }, ack_vector(6));
        assert_server_msg(
            &unsupported_version_error(Some(1), SUPPORTED_PROTO),
            server_error_vector("unsupported_version"),
        );
        assert_server_msg(
            &ServerMsg::error(None, ErrorCode::UnknownType, "unknown message type"),
            server_error_vector("unknown_type"),
        );
    }

    #[test]
    fn client_msg_unknown_fields_are_ignored() {
        let line = r#"{"type":"heartbeat","seq":3,"ts":"x","extra":{"a":1}}"#;
        assert!(matches!(
            parse_client_msg(line),
            Ok(ClientMsg::Heartbeat { seq: 3, .. })
        ));
    }

    #[test]
    fn unknown_type_maps_to_unknown_variant() {
        assert_eq!(
            parse_client_msg(r#"{"type":"bogus","seq":9}"#),
            Ok(ClientMsg::Unknown)
        );
        assert_eq!(
            parse_client_msg(r#"{"type":"bogus"}"#),
            Ok(ClientMsg::Unknown)
        );
    }

    #[test]
    fn malformed_messages_are_bad_requests() {
        for line in [
            "",
            "not json",
            "{",
            "[1,2]",
            "42",
            r#"{"seq":1}"#,                              // 缺 type
            r#"{"type":"heartbeat","seq":1}"#,           // 缺 ts
            r#"{"type":"heartbeat","seq":-1,"ts":"x"}"#, // seq 為負
            r#"{"type":"report","seq":1,"cpu_usage":1,"total_memory":1,"used_memory":1}"#, // 缺 ts
            r#"{"type":"hello","seq":1,"proto":1}"#,     // 欄位缺漏
        ] {
            assert!(parse_client_msg(line).is_err(), "應解析失敗: {line}");
        }
        // report 的內容欄位錯誤：可以解析（v2 欄位與 v1 欄位各自解析），由 validate_client_msg 與
        // 協商後的格式檢查回 bad_request（見 report_body_is_parsed_per_format）
        for line in [
            r#"{"type":"report","seq":1,"ts":"x","cpu_usage":"high","total_memory":1,"used_memory":1}"#,
            r#"{"type":"report","seq":1,"ts":"x"}"#,
        ] {
            let msg = parse_client_msg(line).unwrap();
            assert!(crate::validate_client_msg(&msg).is_err(), "{line}");
        }
    }

    fn hello_line(proto: &str, heartbeat: &str) -> String {
        format!(
            r#"{{"type":"hello","seq":1,"proto":{proto},"agent_id":"6f1c0e4a-1b2c-4d3e-9f00-112233445566","hostname":"h","agent_version":"v","heartbeat_interval_ms":{heartbeat},"ts":"t"}}"#
        )
    }

    fn parsed_hello(proto: &str, heartbeat: &str) -> Result<(Proto, u64), String> {
        match parse_client_msg(&hello_line(proto, heartbeat))? {
            ClientMsg::Hello {
                proto,
                heartbeat_interval_ms,
                ..
            } => Ok((proto, heartbeat_interval_ms)),
            other => panic!("應解析為 hello: {other:?}"),
        }
    }

    #[test]
    fn invalid_proto_is_unsupported_version_not_bad_request() {
        assert_eq!(parsed_hello("1", "1000").unwrap().0, Proto::Version(1));
        assert_eq!(parsed_hello("2", "1000").unwrap().0, Proto::Version(2));
        assert_eq!(
            parsed_hello("4294967295", "1000").unwrap().0,
            Proto::Version(u32::MAX)
        );
        for bad in [
            "4294967296",
            "-1",
            "1.5",
            "1.0",
            r#""1""#,
            "null",
            "true",
            "[1]",
        ] {
            let (proto, _) = parsed_hello(bad, "1000").unwrap();
            assert!(matches!(proto, Proto::Invalid(_)), "{bad}: {proto:?}");
        }
        // 日誌用的原始值有長度上限
        let long = format!(r#""{}""#, "x".repeat(1000));
        match parsed_hello(&long, "1000").unwrap().0 {
            Proto::Invalid(raw) => assert_eq!(raw.chars().count(), MAX_PROTO_DISPLAY_CHARS),
            other => panic!("{other:?}"),
        }
        // 缺少 proto 仍是格式錯誤
        let missing = r#"{"type":"hello","seq":1,"agent_id":"a","hostname":"h","agent_version":"v","heartbeat_interval_ms":1000,"ts":"t"}"#;
        assert!(parse_client_msg(missing).is_err());
    }

    #[test]
    fn out_of_range_heartbeat_interval_is_not_an_error() {
        assert_eq!(parsed_hello("1", "1000").unwrap().1, 1000);
        assert_eq!(parsed_hello("1", "0").unwrap().1, 0);
        assert_eq!(parsed_hello("1", "-5").unwrap().1, 0);
        assert_eq!(parsed_hello("1", "1500.9").unwrap().1, 1500);
        assert_eq!(parsed_hello("1", "1e30").unwrap().1, u64::MAX);
        assert_eq!(
            parsed_hello("1", "18446744073709551616").unwrap().1,
            u64::MAX
        );
        // 夾值後都落在 1000..=60000
        assert_eq!(offline_after_ms(parsed_hello("1", "-5").unwrap().1), 5000);
        assert_eq!(
            offline_after_ms(parsed_hello("1", "1e30").unwrap().1),
            300000
        );
        // 不是數字、或缺少欄位仍是格式錯誤
        for bad in [r#""1000""#, "null", "{}"] {
            assert!(parsed_hello("1", bad).is_err(), "{bad}");
        }
        let missing = r#"{"type":"hello","seq":1,"proto":1,"agent_id":"a","hostname":"h","agent_version":"v","ts":"t"}"#;
        assert!(parse_client_msg(missing).is_err());
    }

    #[test]
    fn huge_cpu_value_becomes_infinite_and_is_rejected() {
        // 超出 f32 範圍的數字轉成伺服器端的 f32 後是 inf，必須被驗證擋下
        let line = r#"{"type":"report","seq":1,"ts":"x","cpu_usage":1e39,"total_memory":1,"used_memory":1}"#;
        let Ok(ClientMsg::Report { body, .. }) = parse_client_msg(line) else {
            panic!("應解析為 report");
        };
        let LegacyReport {
            cpu_usage,
            total_memory,
            used_memory,
        } = body.legacy_fields().unwrap().clone();
        let cpu_usage = server_cpu_usage(cpu_usage);
        assert!(cpu_usage.is_infinite());
        assert!(validate_report(cpu_usage, total_memory, used_memory).is_err());
    }

    #[test]
    fn error_code_fatality_matches_spec() {
        let fatal = [
            ErrorCode::UnsupportedVersion,
            ErrorCode::HelloRequired,
            ErrorCode::TooLong,
            ErrorCode::Superseded,
            ErrorCode::AuthFailed,
            ErrorCode::CertificateRejected,
        ];
        assert_eq!(ErrorCode::AuthFailed.as_str(), "auth_failed");
        assert_eq!(
            ErrorCode::CertificateRejected.as_str(),
            "certificate_rejected"
        );
        let non_fatal = [
            ErrorCode::BadRequest,
            ErrorCode::UnknownType,
            ErrorCode::AlreadyRegistered,
            ErrorCode::InvalidReport,
            ErrorCode::StorageUnavailable,
        ];
        assert_eq!(
            ErrorCode::StorageUnavailable.as_str(),
            "storage_unavailable"
        );
        for code in fatal {
            assert!(code.default_fatal(), "{}", code.as_str());
            assert!(ServerMsg::error(None, code, "").is_fatal());
        }
        for code in non_fatal {
            assert!(!code.default_fatal(), "{}", code.as_str());
            assert!(!ServerMsg::error(None, code, "").is_fatal());
        }
        assert!(ServerMsg::error_with_fatal(None, ErrorCode::BadRequest, "", true).is_fatal());
        assert!(!ServerMsg::Ack { seq: 1 }.is_fatal());
    }

    #[test]
    fn error_serializes_code_string() {
        let v: Value = serde_json::from_str(
            &ServerMsg::error(Some(3), ErrorCode::InvalidReport, "m")
                .to_line()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"type":"error","seq":3,"code":"invalid_report","message":"m","fatal":false})
        );
    }

    #[test]
    fn hello_seq_is_recovered_from_unparsable_hello() {
        // F5：hello 欄位缺漏或型別錯誤，仍能取回 seq
        let v_hello = client_vector("hello");
        let missing_host = v_hello.replace(r#""hostname":"web-01","#, "");
        assert!(parse_client_msg(&missing_host).is_err());
        assert_eq!(hello_seq_of_invalid(&missing_host), Some(1));
        let numeric_host = v_hello.replace(r#""web-01""#, "42");
        assert_eq!(hello_seq_of_invalid(&numeric_host), Some(1));
        // 不是 hello、seq 不是 u64、不是 JSON 物件：不帶 seq
        assert_eq!(
            hello_seq_of_invalid(r#"{"type":"heartbeat","seq":3}"#),
            None
        );
        assert_eq!(hello_seq_of_invalid(r#"{"type":"hello","seq":-1}"#), None);
        assert_eq!(hello_seq_of_invalid(r#"{"type":"hello","seq":"1"}"#), None);
        assert_eq!(hello_seq_of_invalid(r#"{"type":"hello"}"#), None);
        assert_eq!(hello_seq_of_invalid(r#"{"seq":1}"#), None);
        assert_eq!(hello_seq_of_invalid("[1,2]"), None);
        assert_eq!(hello_seq_of_invalid("not json"), None);
    }

    // ---- 原 Pulse src/protocol.rs 的測試（client 訊息的序列化、server 訊息的解析） ----

    #[test]
    fn serialize_hello() {
        assert_client_serializes_to(&vector_hello(), client_vector("hello"));
    }

    #[test]
    fn serialize_heartbeat() {
        assert_client_serializes_to(&vector_heartbeat(), client_vector("heartbeat"));
    }

    #[test]
    fn serialize_report() {
        assert_client_serializes_to(&vector_report(), client_vector("report"));
    }

    #[test]
    fn deserialize_hello_ack() {
        let msg: ServerMsg = serde_json::from_str(server_vector("hello_ack")).unwrap();
        assert_eq!(
            msg,
            ServerMsg::HelloAck {
                seq: 1,
                proto: 1,
                offline_after_ms: 5000,
                features: None,
                server_time: None,
            }
        );
    }

    #[test]
    fn deserialize_acks() {
        let msg: ServerMsg = serde_json::from_str(ack_vector(2)).unwrap();
        assert_eq!(msg, ServerMsg::Ack { seq: 2 });
        let msg: ServerMsg = serde_json::from_str(ack_vector(6)).unwrap();
        assert_eq!(msg, ServerMsg::Ack { seq: 6 });
    }

    #[test]
    fn deserialize_errors() {
        let msg: ServerMsg =
            serde_json::from_str(server_error_vector("unsupported_version")).unwrap();
        assert_eq!(
            msg,
            ServerMsg::Error {
                seq: Some(1),
                code: "unsupported_version".into(),
                message: "supported proto: 1-2".into(),
                fatal: true,
                proto_min: Some(1),
                proto_max: Some(2),
            }
        );

        let msg: ServerMsg = serde_json::from_str(server_error_vector("unknown_type")).unwrap();
        assert_eq!(
            msg,
            ServerMsg::Error {
                seq: None,
                code: "unknown_type".into(),
                message: "unknown message type".into(),
                fatal: false,
                proto_min: None,
                proto_max: None,
            }
        );
    }

    #[test]
    fn error_with_missing_fields_is_not_ignored() {
        // P4：缺 message / fatal 也要能解析；缺 fatal 視為 fatal
        let msg: ServerMsg =
            serde_json::from_str(r#"{"type":"error","seq":3,"code":"auth_failed"}"#).unwrap();
        assert_eq!(
            msg,
            ServerMsg::Error {
                seq: Some(3),
                code: "auth_failed".into(),
                message: String::new(),
                fatal: true,
                proto_min: None,
                proto_max: None,
            }
        );
        let msg: ServerMsg = serde_json::from_str(r#"{"type":"error"}"#).unwrap();
        assert_eq!(
            msg,
            ServerMsg::Error {
                seq: None,
                code: String::new(),
                message: String::new(),
                fatal: true,
                proto_min: None,
                proto_max: None,
            }
        );
        // 明確給 fatal:false 時照給的值
        let msg: ServerMsg =
            serde_json::from_str(r#"{"type":"error","code":"unknown_type","fatal":false}"#)
                .unwrap();
        assert!(matches!(msg, ServerMsg::Error { fatal: false, .. }));
        // 型別錯誤仍是解析失敗
        assert!(
            serde_json::from_str::<ServerMsg>(r#"{"type":"error","code":1,"fatal":true}"#).is_err()
        );
    }

    #[test]
    fn server_msg_unknown_fields_are_ignored() {
        let msg: ServerMsg =
            serde_json::from_str(r#"{"type":"ack","seq":3,"future_field":"x"}"#).unwrap();
        assert_eq!(msg, ServerMsg::Ack { seq: 3 });
    }

    // ---- 合併後新增的測試 ----

    #[test]
    fn parse_server_msg_separates_unknown_types_from_invalid_lines() {
        // A14：未知類型與壞掉的訊息分開
        assert_eq!(
            parse_server_msg(ack_vector(2)).unwrap(),
            ServerMsg::Ack { seq: 2 }
        );
        for (line, kind) in [
            (r#"{"type":"future_msg"}"#, "future_msg"),
            (r#"{"type":"command","seq":3,"args":[1]}"#, "command"),
            (r#"{"type":""}"#, ""),
            (r#"{"type":"ACK","seq":1}"#, "ACK"),
        ] {
            match parse_server_msg(line) {
                Err(ServerMsgError::UnknownType(k)) => assert_eq!(k, kind, "{line}"),
                other => panic!("{line}: {other:?}"),
            }
        }
        // 已知類型的欄位錯誤、type 不是字串、缺少 type、不是物件、不是 JSON：一律為 Invalid
        for line in [
            r#"{"type":"ack"}"#,
            r#"{"type":"ack","seq":"x"}"#,
            r#"{"type":"hello_ack","seq":1}"#,
            r#"{"type":"error","code":1}"#,
            r#"{"type":1}"#,
            r#"{"type":null}"#,
            r#"{"seq":1}"#,
            "[1,2]",
            "not json",
            "",
            r#"{"type":"future_msg"} trailing"#,
        ] {
            assert!(
                matches!(parse_server_msg(line), Err(ServerMsgError::Invalid(_))),
                "{line}"
            );
        }
        // 錯誤說明：Invalid 沿用 serde_json 的說明，未知類型以 escape 後的文字顯示
        let e = parse_server_msg(r#"{"type":"x\ny"}"#).unwrap_err();
        assert_eq!(e.to_string(), r#"unknown message type "x\ny""#);
        let e = parse_server_msg("not json").unwrap_err();
        let serde_err = serde_json::from_str::<ServerMsg>("not json").unwrap_err();
        assert_eq!(e.to_string(), serde_err.to_string());
    }

    #[test]
    fn server_msg_types_list_every_variant() {
        let samples = [
            ServerMsg::HelloAck {
                seq: 1,
                proto: 1,
                offline_after_ms: 5000,
                features: None,
                server_time: None,
            },
            ServerMsg::Ack { seq: 1 },
            ServerMsg::error(None, ErrorCode::BadRequest, "x"),
        ];
        let kinds: Vec<String> = samples
            .iter()
            .map(|m| {
                serde_json::to_value(m).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(kinds, SERVER_MSG_TYPES);
    }

    #[test]
    fn invalid_proto_and_unknown_variant_cannot_be_serialized() {
        let mut hello = vector_hello();
        if let ClientMsg::Hello { proto, .. } = &mut hello {
            *proto = Proto::Invalid("\"1\"".into());
        }
        assert!(serde_json::to_string(&hello).is_err());
        assert!(serde_json::to_string(&ClientMsg::Unknown).is_err());
    }

    #[test]
    fn error_codes_round_trip_through_strings() {
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_code(code.as_str()), Some(code));
        }
        assert_eq!(ErrorCode::from_code("Bad_Request"), None);
        assert_eq!(ErrorCode::from_code(""), None);
    }

    // ---- 協定 v2（A3、A4） ----

    use crate::metrics::Metric;

    /// 在 v1 hello 測試向量的結尾加上欄位
    fn v1_hello_with(extra: &str) -> String {
        let v1 = client_vector("hello");
        format!("{},{extra}}}", &v1[..v1.len() - 1])
    }

    fn hello_ext(line: &str) -> HelloExt {
        match parse_client_msg(line) {
            Ok(ClientMsg::Hello { ext, .. }) => *ext,
            other => panic!("應解析為 hello: {other:?}"),
        }
    }

    fn report_body(line: &str) -> ReportBody {
        match parse_client_msg(line) {
            Ok(ClientMsg::Report { body, .. }) => body,
            other => panic!("應解析為 report: {other:?}"),
        }
    }

    #[test]
    fn v2_hello_vector_parses_into_extension_fields() {
        let ClientMsg::Hello {
            proto, ext, seq, ..
        } = parse_client_msg(client_vector_in(Ver::V2, "hello")).unwrap()
        else {
            panic!("應解析為 hello");
        };
        assert_eq!((seq, proto), (1, Proto::Version(1)));
        assert_eq!(
            ext.offer.unwrap(),
            ProtoOffer {
                proto_min: Some(1),
                proto_max: Some(2),
                features: Some(vec!["metrics".into()]),
            }
        );
        let host = ext.host.unwrap();
        assert_eq!(
            host.host_fingerprint.as_deref(),
            Some("sha256:355cf5ed40956a057645145cccf74f594102726167e94e3511aae182acc4ec43")
        );
        assert_eq!(host.display_name.as_deref(), Some("Web 01（正式環境）"));
        assert_eq!(host.tags, Some(vec!["env:prod".into(), "team:web".into()]));
        assert_eq!(host.report_interval_ms, Some(5000));
        assert_eq!(
            host.os,
            Some(OsInfo {
                name: Some("Ubuntu".into()),
                version: Some("24.04".into()),
                kernel: Some("6.8.0-45-generic".into()),
                arch: Some("x86_64".into()),
            })
        );
        assert!(crate::validate_host_info(&host).is_ok());
    }

    #[test]
    fn v1_hello_has_empty_extension() {
        assert_eq!(hello_ext(client_vector("hello")), HelloExt::default());
        // v1 hello 序列化後不會多出任何欄位
        let v: Value = serde_json::to_value(vector_hello()).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 8, "{v}");
    }

    #[test]
    fn v2_hello_ack_and_report_vectors() {
        assert_eq!(
            parse_server_msg(server_vector_in(Ver::V2, "hello_ack")).unwrap(),
            ServerMsg::HelloAck {
                seq: 1,
                proto: 2,
                offline_after_ms: 75000,
                features: Some(vec!["metrics".into()]),
                server_time: Some("2026-10-08T03:15:40.012Z".into()),
            }
        );
        let body = report_body(client_vector_in(Ver::V2, "report"));
        // v2 report 沒有三欄位：v1 格式取不到，指標格式可以
        assert!(body.legacy_fields().unwrap_err().contains("cpu_usage"));
        let metrics = body.metric_list().unwrap();
        assert_eq!(metrics.len(), 8);
        assert_eq!(
            metrics[0],
            Metric::new(
                "system.cpu.usage",
                serde_json::Number::from_f64(17.84).unwrap(),
                "%"
            )
        );
        assert_eq!(metrics[1].value.as_u64(), Some(51_539_607_552));
        assert_eq!(metrics[3].labels["mountpoint"], "/");
        assert!(crate::validate_metrics(metrics).is_ok());
        // v1 report：指標格式取不到
        let body = report_body(client_vector("report"));
        assert!(body.metric_list().unwrap_err().contains("metrics"));
        assert_eq!(body.legacy_fields().unwrap().total_memory, 49152);
    }

    #[test]
    fn v2_fields_with_wrong_types_do_not_break_the_hello() {
        // 型別錯誤只讓該部分變成 Err，hello 本身仍可解析（由伺服器依協商結果決定是否拒絕）
        let ext = hello_ext(&v1_hello_with(r#""display_name":42"#));
        assert_eq!(ext.offer, Ok(ProtoOffer::default()));
        assert!(ext.host.unwrap_err().contains("invalid type"));

        let ext = hello_ext(&v1_hello_with(r#""proto_max":"2","tags":["a"]"#));
        assert!(ext.offer.is_err());
        assert_eq!(ext.host.unwrap().tags, Some(vec!["a".into()]));

        for extra in [
            r#""proto_min":-1,"proto_max":2"#,
            r#""proto_min":1.5,"proto_max":2"#,
            r#""features":"metrics""#,
            r#""features":[1]"#,
        ] {
            assert!(hello_ext(&v1_hello_with(extra)).offer.is_err(), "{extra}");
        }
        for extra in [
            r#""report_interval_ms":-1"#,
            r#""report_interval_ms":"5000""#,
            r#""tags":"prod""#,
            r#""os":"linux""#,
            r#""os":{"name":1}"#,
            r#""host_fingerprint":true"#,
        ] {
            assert!(hello_ext(&v1_hello_with(extra)).host.is_err(), "{extra}");
        }
        // null 視同省略；os 中的其他欄位忽略
        let ext = hello_ext(&v1_hello_with(
            r#""proto_min":null,"display_name":null,"os":{"name":"Debian","extra":[1]}"#,
        ));
        assert_eq!(ext.offer, Ok(ProtoOffer::default()));
        assert_eq!(
            ext.host.unwrap().os.unwrap().name.as_deref(),
            Some("Debian")
        );
        // 解析失敗的部分不能序列化
        let mut hello = vector_hello();
        if let ClientMsg::Hello { ext, .. } = &mut hello {
            ext.host = Err("x".into());
        }
        assert!(serde_json::to_string(&hello).is_err());
    }

    #[test]
    fn report_formats_are_parsed_independently() {
        // 三欄位正確、metrics 型別錯誤：v1 格式可用（v1 Kairos 也會忽略 metrics）
        let v1 = client_vector("report");
        let line = format!(r#"{},"metrics":5}}"#, &v1[..v1.len() - 1]);
        let body = report_body(&line);
        assert!(body.legacy_fields().is_ok());
        assert!(body.metric_list().is_err());
        assert!(crate::validate_client_msg(&parse_client_msg(&line).unwrap()).is_ok());
        // metrics 正確、三欄位型別錯誤：指標格式可用
        let line = r#"{"type":"report","seq":1,"ts":"x","cpu_usage":"high","metrics":[{"name":"a.b","value":1,"unit":"1"}]}"#;
        let body = report_body(line);
        assert!(body.legacy_fields().is_err());
        assert_eq!(body.metric_list().unwrap().len(), 1);
        assert!(crate::validate_client_msg(&parse_client_msg(line).unwrap()).is_ok());
        // 兩者都不行：validate_client_msg 回 v1 格式的錯誤說明（與 v1 Kairos 相同）
        let line = r#"{"type":"report","seq":1,"ts":"x","total_memory":1,"used_memory":1}"#;
        let err = crate::validate_client_msg(&parse_client_msg(line).unwrap()).unwrap_err();
        assert!(err.contains("missing field `cpu_usage`"), "{err}");
        // 只輸出 Ok 的部分；兩者都不是 Ok 時不能序列化
        let v = serde_json::to_value(ClientMsg::Report {
            seq: 1,
            ts: "t".into(),
            body: ReportBody::metrics(vec![Metric::new("a.b", 1, "1")]),
        })
        .unwrap();
        assert_eq!(
            v,
            json!({"type":"report","seq":1,"ts":"t","metrics":[{"name":"a.b","value":1,"unit":"1"}]})
        );
        let empty = ClientMsg::Report {
            seq: 1,
            ts: "t".into(),
            body: ReportBody {
                legacy: Err("x".into()),
                metrics: Err("y".into()),
                durable: Ok(None),
            },
        };
        assert!(serde_json::to_string(&empty).is_err());
    }

    #[test]
    fn durable_vector_carries_boot_id_and_report_seq() {
        let ClientMsg::Hello { ext, .. } =
            parse_client_msg(client_vector_in(Ver::Durable, "hello")).unwrap()
        else {
            panic!("應解析為 hello");
        };
        assert_eq!(
            ext.offer.unwrap().features,
            Some(vec!["metrics".into(), "durable".into()])
        );
        let ServerMsg::HelloAck { features, .. } =
            parse_server_msg(server_vector_in(Ver::Durable, "hello_ack")).unwrap()
        else {
            panic!("應解析為 hello_ack");
        };
        assert_eq!(features, Some(vec!["metrics".into(), "durable".into()]));
        let body = report_body(client_vector_in(Ver::Durable, "report"));
        let durable = body.durable_seq().unwrap();
        assert_eq!(
            durable,
            &DurableSeq {
                boot_id: "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e".into(),
                report_seq: 42,
            }
        );
        assert_eq!(body.metric_list().unwrap().len(), 3);
        // 重送：同一筆內容以 with_durable 組回，序列化結果與向量相同（seq 以外）
        let rebuilt = ClientMsg::Report {
            seq: 2,
            ts: "2026-10-08T03:15:45.210Z".into(),
            body: ReportBody::metrics(body.metric_list().unwrap().to_vec())
                .with_durable(durable.clone()),
        };
        assert_client_serializes_to(&rebuilt, client_vector_in(Ver::Durable, "report"));
    }

    #[test]
    fn durable_fields_are_parsed_independently() {
        let v1 = client_vector("report");
        let with = |extra: &str| report_body(&format!("{},{extra}}}", &v1[..v1.len() - 1]));
        const BOOT: &str = "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e";
        // 省略（或 null）：Ok(None)，非 durable 連線不受影響；durable 連線為 bad_request
        let body = report_body(v1);
        assert_eq!(body.durable, Ok(None));
        assert!(body.durable_seq().unwrap_err().contains("boot_id"));
        assert_eq!(
            with(r#""boot_id":null,"report_seq":null"#).durable,
            Ok(None)
        );
        // 型別錯誤或只有一個：只讓 durable 部分失敗，v1 欄位照常可用
        for extra in [
            format!(r#""boot_id":"{BOOT}""#),
            r#""report_seq":1"#.to_string(),
            format!(r#""boot_id":"{BOOT}","report_seq":"1""#),
            format!(r#""boot_id":"{BOOT}","report_seq":-1"#),
            format!(r#""boot_id":"{BOOT}","report_seq":1.5"#),
            r#""boot_id":7,"report_seq":1"#.to_string(),
        ] {
            let body = with(&extra);
            assert!(body.durable.is_err(), "{extra}");
            assert!(body.legacy_fields().is_ok(), "{extra}");
            let err = body.durable_seq().unwrap_err();
            assert!(err.starts_with("invalid message: "), "{err}");
            // validate_client_msg 不看 durable 欄位（只有 durable 連線才檢查）
            let line = format!("{},{extra}}}", &v1[..v1.len() - 1]);
            assert!(crate::validate_client_msg(&parse_client_msg(&line).unwrap()).is_ok());
        }
        // 格式正確但不符限制
        let body = with(r#""boot_id":"not-a-uuid","report_seq":1"#);
        assert!(body.durable_seq().unwrap_err().contains("UUID"));
        let body = with(&format!(r#""boot_id":"{BOOT}","report_seq":0"#));
        assert!(body.durable_seq().unwrap_err().contains("at least 1"));
        // 大寫的 UUID 可接受（伺服器轉小寫）
        let upper = BOOT.to_ascii_uppercase();
        let body = with(&format!(
            r#""boot_id":"{upper}","report_seq":18446744073709551615"#
        ));
        assert_eq!(body.durable_seq().unwrap().report_seq, u64::MAX);
        // 解析失敗的 durable 部分不能序列化
        let msg = ClientMsg::Report {
            seq: 1,
            ts: "t".into(),
            body: ReportBody {
                durable: Err("x".into()),
                ..ReportBody::legacy(1.0, 2, 1)
            },
        };
        assert!(serde_json::to_string(&msg).is_err());
    }

    #[test]
    fn error_proto_range_is_lenient() {
        // Pulse 解析 error 時容忍欄位問題：proto_min／proto_max 不是 u32 時視為沒有
        let msg = parse_server_msg(
            r#"{"type":"error","seq":1,"code":"unsupported_version","message":"m","fatal":true,"proto_min":"1","proto_max":-2}"#,
        )
        .unwrap();
        assert!(matches!(
            msg,
            ServerMsg::Error {
                proto_min: None,
                proto_max: None,
                ..
            }
        ));
        // 其他錯誤不輸出這兩個欄位
        let v = serde_json::to_value(ServerMsg::error(None, ErrorCode::BadRequest, "m")).unwrap();
        assert!(
            v.get("proto_min").is_none() && v.get("proto_max").is_none(),
            "{v}"
        );
    }

    /// 凍結的 v1 型別定義（v1 Kairos 與 v1 Pulse 實際使用的 serde 定義，沒有 `deny_unknown_fields`），
    /// 用來確認舊版看到 v2 欄位時的行為。不要隨協定修改。
    mod frozen_v1 {
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(dead_code)]
        pub enum ClientMsg {
            Hello {
                seq: u64,
                proto: serde_json::Value,
                agent_id: String,
                hostname: String,
                agent_version: String,
                heartbeat_interval_ms: serde_json::Value,
                ts: String,
            },
            Heartbeat {
                seq: u64,
                ts: String,
            },
            Report {
                seq: u64,
                ts: String,
                cpu_usage: f32,
                total_memory: u64,
                used_memory: u64,
            },
            #[serde(other)]
            Unknown,
        }

        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ServerMsg {
            HelloAck {
                seq: u64,
                proto: u32,
                offline_after_ms: u64,
            },
            Ack {
                seq: u64,
            },
            Error {
                #[serde(default)]
                seq: Option<u64>,
                #[serde(default)]
                code: String,
                #[serde(default)]
                message: String,
                #[serde(default = "yes")]
                fatal: bool,
            },
        }

        fn yes() -> bool {
            true
        }
    }

    #[test]
    fn v1_kairos_accepts_the_v2_hello_as_v1() {
        // 新 Pulse ↔ 舊 Kairos：v1 Kairos 只檢查 proto == 1，v2 欄位被忽略
        let v2_hello = client_vector_in(Ver::V2, "hello");
        match serde_json::from_str::<frozen_v1::ClientMsg>(v2_hello).unwrap() {
            frozen_v1::ClientMsg::Hello {
                proto, hostname, ..
            } => {
                assert_eq!(proto, json!(1));
                assert_eq!(hostname, "web-01");
            }
            other => panic!("{other:?}"),
        }
        // 舊 Kairos 回的 hello_ack 就是 v1 向量；新 Pulse 解析為 proto 1、沒有 v2 欄位
        assert_eq!(
            parse_server_msg(server_vector("hello_ack")).unwrap(),
            ServerMsg::HelloAck {
                seq: 1,
                proto: 1,
                offline_after_ms: 5000,
                features: None,
                server_time: None,
            }
        );
        // 新 Pulse 在 v1 連線送出的 report 就是 v1 向量，舊 Kairos 可解析
        assert!(matches!(
            serde_json::from_str::<frozen_v1::ClientMsg>(client_vector("report")).unwrap(),
            frozen_v1::ClientMsg::Report { seq: 6, .. }
        ));
        // 反過來，v2 的 metrics report 在舊 Kairos 是欄位缺漏（新 Pulse 不會在 v1 連線送出）
        assert!(
            serde_json::from_str::<frozen_v1::ClientMsg>(client_vector_in(Ver::V2, "report"))
                .is_err()
        );
    }

    #[test]
    fn v1_pulse_ignores_v2_fields_in_server_messages() {
        // 新 Kairos 只在協商為 v2 時送出這些欄位；即使送到 v1 Pulse，多出的欄位也會被忽略
        assert_eq!(
            serde_json::from_str::<frozen_v1::ServerMsg>(server_vector_in(Ver::V2, "hello_ack"))
                .unwrap(),
            frozen_v1::ServerMsg::HelloAck {
                seq: 1,
                proto: 2,
                offline_after_ms: 75000
            }
        );
        assert_eq!(
            serde_json::from_str::<frozen_v1::ServerMsg>(server_error_vector(
                "unsupported_version"
            ))
            .unwrap(),
            frozen_v1::ServerMsg::Error {
                seq: Some(1),
                code: "unsupported_version".into(),
                message: "supported proto: 1-2".into(),
                fatal: true
            }
        );
    }

    #[test]
    fn v1_pulse_hello_is_a_v1_hello_for_the_new_server() {
        // 舊 Pulse ↔ 新 Kairos：舊版 hello（v1 向量）沒有任何 v2 欄位，協商範圍為 [1, 1]
        let ClientMsg::Hello { proto, ext, .. } = parse_client_msg(client_vector("hello")).unwrap()
        else {
            panic!("應解析為 hello");
        };
        let range = crate::client_proto_range(&proto, &ext.offer).unwrap();
        assert_eq!(crate::negotiate(range, SUPPORTED_PROTO), Some(PROTO_V1));
        // 新 Pulse 的 hello：協商為 v2
        let ClientMsg::Hello { proto, ext, .. } =
            parse_client_msg(client_vector_in(Ver::V2, "hello")).unwrap()
        else {
            panic!("應解析為 hello");
        };
        let range = crate::client_proto_range(&proto, &ext.offer).unwrap();
        assert_eq!(
            crate::negotiate(range, SUPPORTED_PROTO),
            Some(crate::PROTO_V2)
        );
        // 新 Pulse 的 hello 遇到只支援 v1 的伺服器：協商為 v1
        assert_eq!(
            crate::negotiate(range, crate::ProtoRange { min: 1, max: 1 }),
            Some(PROTO_V1)
        );
    }
}
