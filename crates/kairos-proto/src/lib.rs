//! Pulse ↔ Kairos 協定（v1、v2）的共用實作（商業審計 A3、A4、A8、A14）。
//!
//! 規格只有一份：本 crate 目錄下的 [`PROTOCOL.md`](../PROTOCOL.md)。本 crate 的單元測試以
//! `include_str!` 讀入該文件，直接從「範例」節解析測試向量、從「錯誤碼」表比對 fatal 預設值，
//! 修改規格時不需要再到兩個產品裡各改一份複本。
//!
//! 內容：
//! - 協定常數（版本範圍、單行上限、欄位長度上限、心跳與離線門檻、指標上限）
//! - 訊息型別 [`ClientMsg`]、[`ServerMsg`] 與錯誤碼 [`ErrorCode`]，兩個方向都能序列化與反序列化
//!   （Kairos 解析 client 訊息、送出 server 訊息；Pulse 相反）
//! - 版本協商（A3）：[`client_proto_range`]、[`negotiate`]、[`Session`]
//! - at-least-once（A5）：durable 的 [`DurableSeq`]（`boot_id`／`report_seq`）與其限制
//! - 欄位驗證：UUID 格式的 `agent_id`、顯示用字串的禁止字元表、hello（含 v2 的主機資訊）、
//!   `ts`、report（v1 三欄位與 v2 的通用指標，A4）的限制
//!
//! 本 crate 只有純函式與資料型別，不做 I/O、不記日誌，也不依賴 tokio；NDJSON 的分行 codec
//! 與日誌的措辭留在各產品。

#![forbid(unsafe_code)]

mod messages;
mod metrics;
mod negotiate;
mod validate;

#[cfg(test)]
mod tests;

pub use messages::{
    ClientMsg, DurableSeq, ErrorCode, HelloExt, HostInfo, LegacyReport, OsInfo, Proto, ProtoOffer,
    ReportBody, SERVER_MSG_TYPES, ServerMsg, ServerMsgError, hello_seq_of_invalid,
    parse_client_msg, parse_server_msg,
};
pub use metrics::{
    METRIC_CPU_USAGE, METRIC_FILESYSTEM_AVAILABLE, METRIC_FILESYSTEM_TOTAL, METRIC_MEMORY_TOTAL,
    METRIC_MEMORY_USED, METRIC_NETWORK_RECEIVED, METRIC_NETWORK_TRANSMITTED, METRIC_TEMPERATURE,
    Metric, STANDARD_METRICS, StandardMetric, is_valid_label_key, is_valid_metric_name,
    is_valid_unit, validate_metric, validate_metrics,
};
pub use negotiate::{
    FEATURE_DURABLE, FEATURE_METRICS, ProtoRange, ProtoRangeError, SERVER_FEATURES,
    SUPPORTED_PROTO, Session, client_proto_range, negotiate, unsupported_version_error,
};
pub use validate::{
    clamp_heartbeat_interval_ms, is_forbidden_display_char, is_host_fingerprint, is_uuid_text,
    normalize_agent_id, offline_after_ms, sanitize_display_text, server_cpu_usage,
    validate_client_msg, validate_durable_seq, validate_features, validate_hello,
    validate_host_info, validate_report,
};

/// 協定 v1：hello 只有 `proto`，report 為 `cpu_usage`／`total_memory`／`used_memory` 三欄位
pub const PROTO_V1: u32 = 1;

/// 協定 v2：hello 帶版本範圍、features 與主機資訊，hello_ack 帶 features 與 `server_time`，
/// report 可改用通用指標 `metrics`（雙方都宣告 [`FEATURE_METRICS`] 時）
pub const PROTO_V2: u32 = 2;

/// 單行上限（bytes，不含換行字元 `\n`，但含 `\r`）
pub const MAX_LINE_LENGTH: usize = 65536;

/// 同一條連線連續幾則無效訊息（`bad_request` 與 `unknown_type` 合計，中間沒有合法訊息）後，
/// 該則錯誤改為 fatal 並斷線
pub const MAX_CONSECUTIVE_BAD_REQUESTS: u32 = 5;

/// 離線判定的下限（毫秒）
pub const MIN_OFFLINE_AFTER_MS: u64 = 5000;

/// 心跳間隔的倍數，超過此時間沒收到訊息即視為離線
pub const OFFLINE_HEARTBEAT_MULTIPLIER: u64 = 5;

/// 伺服器接受的心跳間隔範圍（毫秒），超出範圍會被夾值。
/// Pulse 的 `heartbeat.interval` 設定上限（60 秒）與 [`MAX_HEARTBEAT_INTERVAL_MS`] 一致。
pub const MIN_HEARTBEAT_INTERVAL_MS: u64 = 1000;
pub const MAX_HEARTBEAT_INTERVAL_MS: u64 = 60000;

/// hello 欄位長度上限（bytes）
pub const MAX_HOSTNAME_BYTES: usize = 255;
pub const MAX_AGENT_VERSION_BYTES: usize = 64;

/// `ts` 欄位長度上限（bytes）
pub const MAX_TS_BYTES: usize = 64;

/// v2 hello：`features` 的項目數與每項長度上限（client 與 server 相同）
pub const MAX_FEATURES: usize = 32;
pub const MAX_FEATURE_BYTES: usize = 64;

/// v2 hello：`display_name` 長度上限（bytes，同 `hostname`）
pub const MAX_DISPLAY_NAME_BYTES: usize = 255;

/// v2 hello：`tags` 的項目數與每項長度上限（bytes）
pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_BYTES: usize = 64;

/// v2 hello：`os` 物件中每個字串欄位的長度上限（bytes）
pub const MAX_OS_FIELD_BYTES: usize = 128;

/// v2 hello：`host_fingerprint` 長度上限（bytes）；格式見 [`is_host_fingerprint`]
pub const MAX_HOST_FINGERPRINT_BYTES: usize = 145;

/// v2 hello：`report_interval_ms` 的接受範圍（毫秒，1 秒～24 小時；超出時 `bad_request`）
pub const MIN_REPORT_INTERVAL_MS: u64 = 1000;
pub const MAX_REPORT_INTERVAL_MS: u64 = 86_400_000;

/// v2 report：每筆 report 的指標數上限
pub const MAX_METRICS: usize = 256;
/// 指標名稱長度上限（bytes）
pub const MAX_METRIC_NAME_BYTES: usize = 128;
/// 指標單位長度上限（bytes）
pub const MAX_METRIC_UNIT_BYTES: usize = 32;
/// 每個指標的 label 數上限
pub const MAX_METRIC_LABELS: usize = 16;
/// label 名稱長度上限（bytes）
pub const MAX_LABEL_KEY_BYTES: usize = 64;
/// label 值長度上限（bytes）
pub const MAX_LABEL_VALUE_BYTES: usize = 256;

/// durable：伺服器保證去重的重送範圍。同一個 (agent_id, `boot_id`) 中，`report_seq` 不小於
/// 「已保存的最大 `report_seq` − 此值 + 1」的重送都能正確判斷是否已保存；更舊的重送一律視為已保存
/// （回 `ack`、不寫入）。client 依序送出、在途筆數不超過 [`MAX_DURABLE_IN_FLIGHT`] 時，
/// 需要重送的資料一定落在這個範圍內
pub const DURABLE_DEDUP_WINDOW: u64 = 256;

/// durable：client 同一時間最多在途（已送出、尚未收到回應）的 report 筆數
pub const MAX_DURABLE_IN_FLIGHT: usize = 8;

const _: () = assert!(MAX_DURABLE_IN_FLIGHT as u64 <= DURABLE_DEDUP_WINDOW);
