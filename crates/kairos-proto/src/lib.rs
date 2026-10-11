//! Pulse ↔ Kairos 協定 v1 的共用實作（商業審計 A8、A14）。
//!
//! 規格只有一份：本 crate 目錄下的 [`PROTOCOL.md`](../PROTOCOL.md)。本 crate 的單元測試以
//! `include_str!` 讀入該文件，直接從「範例」節解析測試向量、從「錯誤碼」表比對 fatal 預設值，
//! 修改規格時不需要再到兩個產品裡各改一份複本。
//!
//! 內容：
//! - 協定常數（版本、單行上限、欄位長度上限、心跳與離線門檻）
//! - 訊息型別 [`ClientMsg`]、[`ServerMsg`] 與錯誤碼 [`ErrorCode`]，兩個方向都能序列化與反序列化
//!   （Kairos 解析 client 訊息、送出 server 訊息；Pulse 相反）
//! - 欄位驗證：UUID 格式的 `agent_id`、顯示用字串的禁止字元表、hello／`ts`／report 的限制
//!
//! 本 crate 只有純函式與資料型別，不做 I/O、不記日誌，也不依賴 tokio；NDJSON 的分行 codec
//! 與日誌的措辭留在各產品。

#![forbid(unsafe_code)]

mod messages;
mod validate;

#[cfg(test)]
mod tests;

pub use messages::{
    ClientMsg, ErrorCode, Proto, SERVER_MSG_TYPES, ServerMsg, ServerMsgError, hello_seq_of_invalid,
    parse_client_msg, parse_server_msg,
};
pub use validate::{
    clamp_heartbeat_interval_ms, is_forbidden_display_char, is_uuid_text, normalize_agent_id,
    offline_after_ms, server_cpu_usage, validate_client_msg, validate_hello, validate_report,
};

/// 目前支援的協定版本
pub const PROTO_VERSION: u32 = 1;

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
