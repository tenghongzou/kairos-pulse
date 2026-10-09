//! Pulse ↔ Kairos 協定 v1：訊息型別、NDJSON 框架與驗證邏輯。
//!
//! 本模組只包含純函式與資料型別，不做任何 I/O，方便單元測試。

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::fmt;
use std::io::{Error, ErrorKind};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder, LinesCodec, LinesCodecError};

/// 目前支援的協定版本
pub const PROTO_VERSION: u32 = 1;

/// 單行上限（bytes，不含換行字元）
pub const MAX_LINE_LENGTH: usize = 65536;

/// 同一條連線連續幾則無效訊息（`bad_request` 與 `unknown_type` 合計，中間沒有合法訊息）後，
/// 該則錯誤改為 fatal 並斷線
pub const MAX_CONSECUTIVE_BAD_REQUESTS: u32 = 5;

/// 離線判定的下限（毫秒）
pub const MIN_OFFLINE_AFTER_MS: u64 = 5000;

/// 心跳間隔的倍數，超過此時間沒收到訊息即視為離線
pub const OFFLINE_HEARTBEAT_MULTIPLIER: u64 = 5;

/// 伺服器接受的心跳間隔範圍（毫秒），超出範圍會被夾值
pub const MIN_HEARTBEAT_INTERVAL_MS: u64 = 1000;
pub const MAX_HEARTBEAT_INTERVAL_MS: u64 = 60000;

/// hello 欄位長度上限（bytes）
pub const MAX_HOSTNAME_BYTES: usize = 255;
pub const MAX_AGENT_VERSION_BYTES: usize = 64;

/// `ts` 欄位長度上限（bytes）
pub const MAX_TS_BYTES: usize = 64;

/// 日誌中顯示無效 `proto` 原始值的長度上限（字元）
const MAX_PROTO_DISPLAY_CHARS: usize = 32;

/// hello 的 `proto` 欄位。不是合法 u32 的值（字串、負數、小數、超出 u32）不視為格式錯誤，
/// 而是「不受支援的版本」，回 `unsupported_version`。
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
/// 負數視為 0、小數捨去、超出 u64 視為 `u64::MAX`，之後再由 [`clamp_heartbeat_interval_ms`] 夾值。
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

/// Client → Server 訊息
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        seq: u64,
        #[serde(deserialize_with = "de_proto")]
        proto: Proto,
        agent_id: String,
        hostname: String,
        agent_version: String,
        #[serde(deserialize_with = "de_heartbeat_interval")]
        heartbeat_interval_ms: u64,
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
        /// 單位 MB
        total_memory: u64,
        /// 單位 MB
        used_memory: u64,
    },
    /// 不認得的 `type`；`serde(other)` 只能用於 unit 變體，因此拿不到 seq
    #[serde(other)]
    Unknown,
}

/// Server → Client 訊息
#[derive(Debug, Clone, PartialEq, Serialize)]
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
        seq: Option<u64>,
        code: String,
        message: String,
        fatal: bool,
    },
}

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

/// 解析一行 NDJSON。失敗時回傳給 client 的錯誤說明。
pub fn parse_client_msg(line: &str) -> Result<ClientMsg, String> {
    serde_json::from_str(line).map_err(|e| format!("invalid message: {}", e))
}

/// 由 client 宣告的心跳間隔計算離線門檻：`max(5 × heartbeat_interval_ms, 5000)`
/// 將 client 宣告的心跳間隔夾在 [`MIN_HEARTBEAT_INTERVAL_MS`]..=[`MAX_HEARTBEAT_INTERVAL_MS`]
pub fn clamp_heartbeat_interval_ms(heartbeat_interval_ms: u64) -> u64 {
    heartbeat_interval_ms.clamp(MIN_HEARTBEAT_INTERVAL_MS, MAX_HEARTBEAT_INTERVAL_MS)
}

/// `max(5 × clamp(heartbeat_interval_ms, 1000, 60000), 5000)`，結果介於 5000..=300000
pub fn offline_after_ms(heartbeat_interval_ms: u64) -> u64 {
    clamp_heartbeat_interval_ms(heartbeat_interval_ms)
        .saturating_mul(OFFLINE_HEARTBEAT_MULTIPLIER)
        .max(MIN_OFFLINE_AFTER_MS)
}

/// 驗證 report 內容：cpu_usage 必須是 0..=100 的有限值，used_memory 不得大於 total_memory
pub fn validate_report(cpu_usage: f32, total_memory: u64, used_memory: u64) -> Result<(), String> {
    if !cpu_usage.is_finite() || !(0.0..=100.0).contains(&cpu_usage) {
        return Err(format!("cpu_usage out of range 0..=100: {}", cpu_usage));
    }
    if used_memory > total_memory {
        return Err(format!(
            "used_memory ({}) greater than total_memory ({})",
            used_memory, total_memory
        ));
    }
    Ok(())
}

/// 是否為 UUID 文字格式：8-4-4-4-12 個十六進位字元（共 36 字元，大小寫皆可）。
/// 只檢查格式，不檢查版本與 variant 位元。
pub fn is_uuid_text(s: &str) -> bool {
    const HYPHENS: [usize; 4] = [8, 13, 18, 23];
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, &b)| {
            if HYPHENS.contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// Unicode 一般類別 Cf（格式字元，例如 U+200B 零寬空格、U+202E 由右至左覆寫、U+FEFF BOM）
/// 的所有碼位範圍（Unicode 15.1）。std 沒有提供一般類別查詢，這裡直接列表。
const FORMAT_CHAR_RANGES: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

/// 雖不屬於 Cc／Cf，但顯示為空白或不可見、可用來偽裝或隱藏內容的碼位（R2-K23；Pulse 端使用相同的表）：
/// - U+034F 組合用字形連接抑制符（CGJ）
/// - Hangul filler：U+115F、U+1160、U+3164、U+FFA0（顯示為空白）
/// - 變體選擇字元：U+FE00–U+FE0F、U+E0100–U+E01EF
/// - 私用區：U+E000–U+F8FF、U+F0000–U+FFFFD、U+100000–U+10FFFD（顯示結果取決於字型，無法預期）
const INVISIBLE_CHAR_RANGES: &[(u32, u32)] = &[
    (0x034F, 0x034F),
    (0x115F, 0x1160),
    (0x3164, 0x3164),
    (0xFFA0, 0xFFA0),
    (0xFE00, 0xFE0F),
    (0xE0100, 0xE01EF),
    (0xE000, 0xF8FF),
    (0xF0000, 0xFFFFD),
    (0x100000, 0x10FFFD),
];

/// 不得出現在顯示用字串中的字元：控制字元（Cc）、格式字元（Cf）、行分隔（Zl, U+2028）、
/// 段落分隔（Zp, U+2029），以及 [`INVISIBLE_CHAR_RANGES`] 列出的不可見／偽裝用碼位。
/// 這些字元可能偽造日誌行、在 agents.json 或終端中改變文字方向、隱藏或偽裝內容。
pub fn is_forbidden_display_char(c: char) -> bool {
    if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
        return true;
    }
    let cp = u32::from(c);
    FORMAT_CHAR_RANGES
        .iter()
        .chain(INVISIBLE_CHAR_RANGES)
        .any(|&(lo, hi)| (lo..=hi).contains(&cp))
}

/// 檢查顯示用字串：長度 1..=max bytes，且不得含控制、格式、行／段落分隔字元（避免日誌注入）
fn validate_display_text(field: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(format!(
            "{} must be 1..={} bytes (got {})",
            field,
            max_bytes,
            value.len()
        ));
    }
    if value.chars().any(is_forbidden_display_char) {
        return Err(format!(
            "{} must not contain control, format or line/paragraph separator characters",
            field
        ));
    }
    Ok(())
}

/// agent_id 的正規化形式：UUID 不分大小寫，伺服器一律以小寫識別、註冊、比對與寫檔。
/// 應在 [`validate_hello`] 通過後呼叫。
pub fn normalize_agent_id(agent_id: &str) -> String {
    agent_id.to_ascii_lowercase()
}

/// 驗證 hello 的欄位限制（違反時回 `bad_request`）
pub fn validate_hello(agent_id: &str, hostname: &str, agent_version: &str) -> Result<(), String> {
    if !is_uuid_text(agent_id) {
        return Err("agent_id must be a UUID (8-4-4-4-12 hex digits)".to_string());
    }
    validate_display_text("hostname", hostname, MAX_HOSTNAME_BYTES)?;
    validate_display_text("agent_version", agent_version, MAX_AGENT_VERSION_BYTES)?;
    Ok(())
}

/// 解析後、依狀態處理前的共通驗證（違反時回 `bad_request`）：`ts` 最長 64 bytes，
/// 且不得含 [`is_forbidden_display_char`] 的字元（`ts` 會原樣寫入 JSONL 與 agents.json；R2-K8）。
/// 不要求 RFC 3339：規格只把 `ts` 定義為參考用字串，伺服器不解析它。
pub fn validate_client_msg(msg: &ClientMsg) -> Result<(), String> {
    let ts = match msg {
        ClientMsg::Hello { ts, .. }
        | ClientMsg::Heartbeat { ts, .. }
        | ClientMsg::Report { ts, .. } => ts,
        ClientMsg::Unknown => return Ok(()),
    };
    if ts.len() > MAX_TS_BYTES {
        return Err(format!(
            "ts must be at most {} bytes (got {})",
            MAX_TS_BYTES,
            ts.len()
        ));
    }
    if ts.chars().any(is_forbidden_display_char) {
        return Err(
            "ts must not contain control, format, invisible or line/paragraph separator characters"
                .to_string(),
        );
    }
    Ok(())
}

/// 從連線讀到的一個框架
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// 一行合法 UTF-8 文字（已去除 `\n` / `\r\n`）
    Line(String),
    /// 單行超過 [`MAX_LINE_LENGTH`]
    TooLong,
    /// 該行不是合法 UTF-8（該行已被丟棄，可繼續讀下一行）
    InvalidUtf8,
}

/// 包裝 `LinesCodec::new_with_max_length(65536)` 的 NDJSON codec。
///
/// `LinesCodec` 遇到超長行或非 UTF-8 時會回傳錯誤，而 `Framed` 在 decoder 回錯誤後
/// 會進入 errored 狀態：下一次 poll 回傳 `None`，且緩衝區內已收到的後續行要等到
/// 下一批資料進來才會被解碼。為了讓非 UTF-8 能以非 fatal 的 `bad_request` 處理並
/// 繼續讀取，這裡把這兩種 decoder 錯誤轉成一般的 [`Frame`]，不讓 `Framed` 進入錯誤狀態。
/// `LinesCodec` 在 UTF-8 檢查前已把該行從緩衝區切除，因此狀態保持一致。
#[derive(Debug)]
pub struct NdjsonCodec {
    inner: LinesCodec,
}

impl NdjsonCodec {
    pub fn new() -> Self {
        Self {
            inner: LinesCodec::new_with_max_length(MAX_LINE_LENGTH),
        }
    }
}

impl Default for NdjsonCodec {
    fn default() -> Self {
        Self::new()
    }
}

fn map_decoded(result: Result<Option<String>, LinesCodecError>) -> Result<Option<Frame>, Error> {
    match result {
        Ok(Some(line)) => Ok(Some(Frame::Line(line))),
        Ok(None) => Ok(None),
        Err(LinesCodecError::MaxLineLengthExceeded) => Ok(Some(Frame::TooLong)),
        // LinesCodec 的 decode 只會因 UTF-8 解碼失敗回傳 InvalidData
        Err(LinesCodecError::Io(e)) if e.kind() == ErrorKind::InvalidData => {
            Ok(Some(Frame::InvalidUtf8))
        }
        Err(LinesCodecError::Io(e)) => Err(e),
    }
}

fn map_encode_error(e: LinesCodecError) -> Error {
    match e {
        LinesCodecError::Io(e) => e,
        LinesCodecError::MaxLineLengthExceeded => {
            Error::new(ErrorKind::InvalidData, "送出的訊息超過單行上限")
        }
    }
}

impl Decoder for NdjsonCodec {
    type Item = Frame;
    type Error = Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, Error> {
        map_decoded(self.inner.decode(buf))
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, Error> {
        map_decoded(self.inner.decode_eof(buf))
    }
}

impl Encoder<String> for NdjsonCodec {
    type Error = Error;

    fn encode(&mut self, line: String, buf: &mut BytesMut) -> Result<(), Error> {
        self.inner.encode(line, buf).map_err(map_encode_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    // PROTOCOL.md「範例」節的測試向量
    const V_HELLO: &str = r#"{"type":"hello","seq":1,"proto":1,"agent_id":"6f1c0e4a-1b2c-4d3e-9f00-112233445566","hostname":"web-01","agent_version":"0.1.0","heartbeat_interval_ms":1000,"ts":"2026-10-08T03:15:40.001Z"}"#;
    const V_HELLO_ACK: &str = r#"{"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000}"#;
    const V_HEARTBEAT: &str = r#"{"type":"heartbeat","seq":2,"ts":"2026-10-08T03:15:41.002Z"}"#;
    const V_ACK_2: &str = r#"{"type":"ack","seq":2}"#;
    const V_REPORT: &str = r#"{"type":"report","seq":6,"ts":"2026-10-08T03:15:45.210Z","cpu_usage":17.84,"total_memory":49152,"used_memory":37034}"#;
    const V_ACK_6: &str = r#"{"type":"ack","seq":6}"#;
    const V_ERR_VERSION: &str = r#"{"type":"error","seq":1,"code":"unsupported_version","message":"supported proto: 1","fatal":true}"#;
    const V_ERR_UNKNOWN: &str = r#"{"type":"error","seq":null,"code":"unknown_type","message":"unknown message type","fatal":false}"#;

    fn assert_server_msg(msg: &ServerMsg, vector: &str) {
        let actual: Value = serde_json::from_str(&msg.to_line().unwrap()).unwrap();
        let expected: Value = serde_json::from_str(vector).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn vector_hello_deserializes() {
        assert_eq!(
            parse_client_msg(V_HELLO).unwrap(),
            ClientMsg::Hello {
                seq: 1,
                proto: Proto::Version(1),
                agent_id: "6f1c0e4a-1b2c-4d3e-9f00-112233445566".into(),
                hostname: "web-01".into(),
                agent_version: "0.1.0".into(),
                heartbeat_interval_ms: 1000,
                ts: "2026-10-08T03:15:40.001Z".into(),
            }
        );
    }

    #[test]
    fn vector_heartbeat_deserializes() {
        assert_eq!(
            parse_client_msg(V_HEARTBEAT).unwrap(),
            ClientMsg::Heartbeat {
                seq: 2,
                ts: "2026-10-08T03:15:41.002Z".into()
            }
        );
    }

    #[test]
    fn vector_report_deserializes() {
        assert_eq!(
            parse_client_msg(V_REPORT).unwrap(),
            ClientMsg::Report {
                seq: 6,
                ts: "2026-10-08T03:15:45.210Z".into(),
                cpu_usage: 17.84,
                total_memory: 49152,
                used_memory: 37034,
            }
        );
    }

    #[test]
    fn vector_server_messages_serialize() {
        assert_server_msg(
            &ServerMsg::HelloAck {
                seq: 1,
                proto: PROTO_VERSION,
                offline_after_ms: offline_after_ms(1000),
            },
            V_HELLO_ACK,
        );
        assert_server_msg(&ServerMsg::Ack { seq: 2 }, V_ACK_2);
        assert_server_msg(&ServerMsg::Ack { seq: 6 }, V_ACK_6);
        assert_server_msg(
            &ServerMsg::error(Some(1), ErrorCode::UnsupportedVersion, "supported proto: 1"),
            V_ERR_VERSION,
        );
        assert_server_msg(
            &ServerMsg::error(None, ErrorCode::UnknownType, "unknown message type"),
            V_ERR_UNKNOWN,
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
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
            r#"{"type":"report","seq":1,"ts":"x","cpu_usage":"high","total_memory":1,"used_memory":1}"#,
            r#"{"type":"hello","seq":1,"proto":1}"#, // 欄位缺漏
        ] {
            assert!(parse_client_msg(line).is_err(), "應解析失敗: {line}");
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
    fn agent_id_is_normalized_to_lowercase() {
        assert_eq!(
            normalize_agent_id("6F1C0E4A-1B2C-4D3E-9F00-112233445566"),
            "6f1c0e4a-1b2c-4d3e-9f00-112233445566"
        );
        assert_eq!(
            normalize_agent_id("6f1c0e4a-1b2c-4d3e-9f00-112233445566"),
            "6f1c0e4a-1b2c-4d3e-9f00-112233445566"
        );
    }

    #[test]
    fn heartbeat_interval_is_clamped() {
        assert_eq!(clamp_heartbeat_interval_ms(0), 1000);
        assert_eq!(clamp_heartbeat_interval_ms(999), 1000);
        assert_eq!(clamp_heartbeat_interval_ms(1000), 1000);
        assert_eq!(clamp_heartbeat_interval_ms(30000), 30000);
        assert_eq!(clamp_heartbeat_interval_ms(60000), 60000);
        assert_eq!(clamp_heartbeat_interval_ms(60001), 60000);
        assert_eq!(clamp_heartbeat_interval_ms(u64::MAX), 60000);
    }

    #[test]
    fn offline_after_is_within_5000_and_300000() {
        assert_eq!(offline_after_ms(0), 5000);
        assert_eq!(offline_after_ms(1000), 5000);
        assert_eq!(offline_after_ms(1001), 5005);
        assert_eq!(offline_after_ms(3000), 15000);
        assert_eq!(offline_after_ms(60000), 300000);
        assert_eq!(offline_after_ms(60001), 300000);
        assert_eq!(offline_after_ms(u64::MAX), 300000);
    }

    #[test]
    fn report_validation_boundaries() {
        assert!(validate_report(0.0, 100, 0).is_ok());
        assert!(validate_report(100.0, 100, 100).is_ok());
        assert!(validate_report(17.84, 49152, 37034).is_ok());
        assert!(validate_report(0.0, 0, 0).is_ok());

        assert!(validate_report(-0.01, 100, 0).is_err());
        assert!(validate_report(100.01, 100, 0).is_err());
        assert!(validate_report(f32::NAN, 100, 0).is_err());
        assert!(validate_report(f32::INFINITY, 100, 0).is_err());
        assert!(validate_report(f32::NEG_INFINITY, 100, 0).is_err());
        assert!(validate_report(50.0, 100, 101).is_err());
    }

    #[test]
    fn huge_cpu_value_becomes_infinite_and_is_rejected() {
        // 超出 f32 範圍的數字會被轉成 inf，必須被驗證擋下
        let line = r#"{"type":"report","seq":1,"ts":"x","cpu_usage":1e39,"total_memory":1,"used_memory":1}"#;
        let Ok(ClientMsg::Report {
            cpu_usage,
            total_memory,
            used_memory,
            ..
        }) = parse_client_msg(line)
        else {
            panic!("應解析為 report");
        };
        assert!(validate_report(cpu_usage, total_memory, used_memory).is_err());
    }

    #[test]
    fn uuid_text_validation() {
        // 測試向量中的 agent_id 必須通過
        assert!(is_uuid_text("6f1c0e4a-1b2c-4d3e-9f00-112233445566"));
        assert!(is_uuid_text("6F1C0E4A-1B2C-4D3E-9F00-112233445566"));
        assert!(is_uuid_text("00000000-0000-0000-0000-000000000000"));

        for bad in [
            "",
            "abc",
            "6f1c0e4a1b2c4d3e9f00112233445566",       // 沒有連字號
            "6f1c0e4a-1b2c-4d3e-9f00-11223344556",    // 少一個字元
            "6f1c0e4a-1b2c-4d3e-9f00-1122334455667",  // 多一個字元
            "6f1c0e4a-1b2c-4d3e-9f00-11223344556g",   // 非十六進位
            "6f1c0e4a-1b2c-4d3e-9f0-0112233445566",   // 連字號位置錯誤
            "{6f1c0e4a-1b2c-4d3e-9f00-112233445566}", // 大括號
            " 6f1c0e4a-1b2c-4d3e-9f00-11223344556",   // 前導空白
            "6f1c0e4a-1b2c-4d3e-9f00-11223344556é",   // 非 ASCII（37 bytes）
        ] {
            assert!(!is_uuid_text(bad), "不應通過: {bad:?}");
        }
    }

    #[test]
    fn hello_field_limits() {
        let id = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
        assert!(validate_hello(id, "web-01", "0.1.0").is_ok());
        assert!(validate_hello("not-a-uuid", "web-01", "0.1.0").is_err());

        // hostname 1..=255 bytes
        assert!(validate_hello(id, "", "0.1.0").is_err());
        assert!(validate_hello(id, "h", "0.1.0").is_ok());
        assert!(validate_hello(id, &"h".repeat(255), "0.1.0").is_ok());
        assert!(validate_hello(id, &"h".repeat(256), "0.1.0").is_err());
        // 以 bytes 計算：85 個三位元組字元 = 255 bytes，86 個 = 258 bytes
        assert!(validate_hello(id, &"主".repeat(85), "0.1.0").is_ok());
        assert!(validate_hello(id, &"主".repeat(86), "0.1.0").is_err());

        // agent_version 1..=64 bytes
        assert!(validate_hello(id, "web-01", "").is_err());
        assert!(validate_hello(id, "web-01", &"1".repeat(64)).is_ok());
        assert!(validate_hello(id, "web-01", &"1".repeat(65)).is_err());
    }

    #[test]
    fn hello_rejects_control_characters() {
        let id = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
        // 一般文字（含空白、中文、符號）可接受
        assert!(validate_hello(id, "web 01 主機-(prod)", "0.1.0-beta+1").is_ok());

        for bad in [
            "web\n2026-10-09 00:00:00 [ERROR] FAKE",
            "web\r",
            "\u{1b}[31mred",
            "tab\there",
            "nul\0",
            "del\u{7f}",
            "c1\u{85}",
        ] {
            let err = validate_hello(id, bad, "0.1.0").unwrap_err();
            assert!(err.contains("control"), "{bad:?}: {err}");
            let err = validate_hello(id, "web-01", bad).unwrap_err();
            assert!(err.contains("control"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn hello_rejects_format_and_separator_characters() {
        let id = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
        for bad in [
            "web\u{202e}10-bew", // 由右至左覆寫
            "web\u{2028}fake",   // 行分隔（Zl）
            "web\u{2029}fake",   // 段落分隔（Zp）
            "web\u{200b}01",     // 零寬空格
            "\u{feff}web",       // BOM
            "web\u{2066}01",     // 方向隔離
            "soft\u{ad}hyphen",  // 軟連字號
            "tag\u{e0041}",      // 標籤字元
            "web\u{061c}01",     // 阿拉伯字母標記
        ] {
            let err = validate_hello(id, bad, "0.1.0").unwrap_err();
            assert!(err.contains("format"), "{bad:?}: {err}");
            assert!(validate_hello(id, "web-01", bad).is_err(), "{bad:?}");
        }
        // 一般的非 ASCII 文字不受影響
        for ok in ["主機-01", "café", "サーバー", "web 01", "\u{2027}"] {
            assert!(validate_hello(id, ok, "0.1.0").is_ok(), "{ok:?}");
        }
        // 表格中每個範圍的兩端都會被拒絕，範圍外的鄰近碼位則否
        for &(lo, hi) in FORMAT_CHAR_RANGES {
            for cp in [lo, hi] {
                assert!(is_forbidden_display_char(char::from_u32(cp).unwrap()));
            }
        }
        assert!(!is_forbidden_display_char('\u{200a}'));
        assert!(!is_forbidden_display_char('\u{2030}'));
    }

    #[test]
    fn hello_rejects_invisible_and_private_use_characters() {
        // R2-K23：Hangul filler、CGJ、變體選擇字元、私用區
        let id = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
        for bad in [
            "web\u{3164}01",
            "\u{115f}",
            "web\u{1160}",
            "web\u{ffa0}01",
            "web\u{34f}01",
            "web\u{fe0f}",
            "web\u{e0100}",
            "web\u{e000}",
            "web\u{f8ff}",
            "web\u{f0000}",
            "web\u{10fffd}",
        ] {
            assert!(validate_hello(id, bad, "0.1.0").is_err(), "{bad:?}");
            assert!(validate_hello(id, "web-01", bad).is_err(), "{bad:?}");
        }
        for &(lo, hi) in INVISIBLE_CHAR_RANGES {
            for cp in [lo, hi] {
                assert!(is_forbidden_display_char(char::from_u32(cp).unwrap()));
            }
        }
        // 範圍外的鄰近碼位不受影響
        for ok in [
            '\u{034e}',
            '\u{0350}',
            '\u{115e}',
            '\u{1161}',
            '\u{3163}',
            '\u{3165}',
            '\u{ff9f}',
            '\u{ffa1}',
            '\u{fdff}',
            '\u{fe10}',
            '\u{f900}',
            '\u{effff}',
            '\u{e01f0}',
        ] {
            assert!(!is_forbidden_display_char(ok), "{:04X}", u32::from(ok));
        }
    }

    #[test]
    fn ts_rejects_forbidden_characters() {
        // R2-K8：ts 會原樣寫入 JSONL 與 agents.json
        let hb = |ts: &str| ClientMsg::Heartbeat {
            seq: 1,
            ts: ts.to_string(),
        };
        for bad in [
            "2026\u{2028}",
            "\u{202e}2026",
            "2026\u{85}",
            "2026\n",
            "2026\u{3164}",
        ] {
            let err = validate_client_msg(&hb(bad)).unwrap_err();
            assert!(err.starts_with("ts"), "{bad:?}: {err}");
        }
        assert!(validate_client_msg(&hb("2026-10-08T03:15:41.002+08:00")).is_ok());
    }

    #[test]
    fn ts_length_limit() {
        let ts_ok = "2".repeat(MAX_TS_BYTES);
        let ts_bad = "2".repeat(MAX_TS_BYTES + 1);
        let hb = |ts: &str| ClientMsg::Heartbeat {
            seq: 1,
            ts: ts.to_string(),
        };
        assert!(validate_client_msg(&hb("2026-10-08T03:15:41.002Z")).is_ok());
        assert!(validate_client_msg(&hb("")).is_ok());
        assert!(validate_client_msg(&hb(&ts_ok)).is_ok());
        assert!(validate_client_msg(&hb(&ts_bad)).is_err());

        let report = ClientMsg::Report {
            seq: 1,
            ts: ts_bad.clone(),
            cpu_usage: 1.0,
            total_memory: 1,
            used_memory: 1,
        };
        assert!(validate_client_msg(&report).is_err());
        let hello = parse_client_msg(&format!(
            r#"{{"type":"hello","seq":1,"proto":1,"agent_id":"a","hostname":"h","agent_version":"v","heartbeat_interval_ms":1,"ts":"{ts_bad}"}}"#
        ))
        .unwrap();
        assert!(validate_client_msg(&hello).is_err());
        assert!(validate_client_msg(&ClientMsg::Unknown).is_ok());
        // 規格測試向量必須通過
        for v in [V_HELLO, V_HEARTBEAT, V_REPORT] {
            assert!(validate_client_msg(&parse_client_msg(v).unwrap()).is_ok());
        }
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
    fn codec_splits_lines_and_accepts_crlf() {
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::from(&b"{\"a\":1}\r\n{\"b\":2}\npartial"[..]);
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Line("{\"a\":1}".into()))
        );
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Line("{\"b\":2}".into()))
        );
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn codec_invalid_utf8_is_skipped_and_reading_continues() {
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::from(&b"\xff\xfe\n{\"ok\":true}\n"[..]);
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::InvalidUtf8));
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Line("{\"ok\":true}".into()))
        );
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn codec_line_length_counts_carriage_return() {
        // T2：上限 65536 bytes 不含 `\n` 但含 `\r`
        // 65535 bytes + `\r\n` → 接受，去掉 `\r` 後為 65535 bytes
        let mut codec = NdjsonCodec::new();
        let mut ok = vec![b'a'; MAX_LINE_LENGTH - 1];
        ok.extend_from_slice(b"\r\n");
        let mut buf = BytesMut::from(&ok[..]);
        assert!(
            matches!(codec.decode(&mut buf).unwrap(), Some(Frame::Line(l)) if l.len() == MAX_LINE_LENGTH - 1)
        );

        // 65536 bytes + `\r\n` → 含 `\r` 共 65537 bytes，回報 TooLong
        let mut codec = NdjsonCodec::new();
        let mut bad = vec![b'a'; MAX_LINE_LENGTH];
        bad.extend_from_slice(b"\r\n");
        let mut buf = BytesMut::from(&bad[..]);
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::TooLong));

        // 分批到達時邊界相同
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&vec![b'a'; MAX_LINE_LENGTH]);
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
        buf.extend_from_slice(b"\r");
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::TooLong));
    }

    #[test]
    fn codec_line_length_limit() {
        let mut codec = NdjsonCodec::new();
        // 剛好 65536 bytes 可以接受
        let mut ok = vec![b'a'; MAX_LINE_LENGTH];
        ok.push(b'\n');
        let mut buf = BytesMut::from(&ok[..]);
        assert!(
            matches!(codec.decode(&mut buf).unwrap(), Some(Frame::Line(l)) if l.len() == MAX_LINE_LENGTH)
        );

        // 超過即回報 TooLong
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::from(&vec![b'a'; MAX_LINE_LENGTH + 1][..]);
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::TooLong));
    }

    #[test]
    fn codec_encode_appends_newline() {
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::new();
        codec
            .encode("{\"type\":\"ack\",\"seq\":1}".to_string(), &mut buf)
            .unwrap();
        assert_eq!(&buf[..], b"{\"type\":\"ack\",\"seq\":1}\n");
    }
}
