//! 訊息型別與 JSON 解析。
//!
//! 兩個方向的訊息都同時實作序列化與反序列化：Kairos 解析 [`ClientMsg`]、送出 [`ServerMsg`]，
//! Pulse 相反。解析時忽略未知欄位（不使用 `deny_unknown_fields`），以便日後擴充。

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::fmt;
use std::io::{Error, ErrorKind};

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

/// Client（Pulse）→ Server（Kairos）訊息
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        /// 全機 CPU 平均使用率（%，有限值、0～100）。
        ///
        /// 線上格式為 JSON 數字，型別統一為 f64（A14）：Pulse 以 f64 送出已四捨五入到小數 2 位的值
        /// （f32 直接序列化會出現 `7.5000005` 這類精度雜訊）；Kairos 以
        /// [`server_cpu_usage`](crate::server_cpu_usage) 轉成 f32 後驗證與保存，
        /// 接受範圍與 JSONL 的輸出都與先前以 f32 反序列化時相同。
        cpu_usage: f64,
        /// 總記憶體（MB）
        total_memory: u64,
        /// 已使用記憶體（MB）
        used_memory: u64,
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
        proto: u32,
        offline_after_ms: u64,
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
    },
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
    use crate::tests::{ack_vector, client_vector, server_error_vector, server_vector};
    use crate::{PROTO_VERSION, offline_after_ms, server_cpu_usage, validate_report};
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
            cpu_usage: 17.84,
            total_memory: 49152,
            used_memory: 37034,
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
                proto: PROTO_VERSION,
                offline_after_ms: offline_after_ms(1000),
            },
            server_vector("hello_ack"),
        );
        assert_server_msg(&ServerMsg::Ack { seq: 2 }, ack_vector(2));
        assert_server_msg(&ServerMsg::Ack { seq: 6 }, ack_vector(6));
        assert_server_msg(
            &ServerMsg::error(Some(1), ErrorCode::UnsupportedVersion, "supported proto: 1"),
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
    fn huge_cpu_value_becomes_infinite_and_is_rejected() {
        // 超出 f32 範圍的數字轉成伺服器端的 f32 後是 inf，必須被驗證擋下
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
                offline_after_ms: 5000
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
                message: "supported proto: 1".into(),
                fatal: true
            }
        );

        let msg: ServerMsg = serde_json::from_str(server_error_vector("unknown_type")).unwrap();
        assert_eq!(
            msg,
            ServerMsg::Error {
                seq: None,
                code: "unknown_type".into(),
                message: "unknown message type".into(),
                fatal: false
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
                fatal: true
            }
        );
        let msg: ServerMsg = serde_json::from_str(r#"{"type":"error"}"#).unwrap();
        assert_eq!(
            msg,
            ServerMsg::Error {
                seq: None,
                code: String::new(),
                message: String::new(),
                fatal: true
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
}
