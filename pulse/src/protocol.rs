//! Pulse ↔ Kairos 協定訊息定義（共同規格 v1）。
//!
//! 傳輸為 NDJSON：一行一則 JSON 物件。欄位名稱與 `type` 值為定案，請勿更改。
//! 解析時忽略未知欄位（不使用 `deny_unknown_fields`）。

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// 目前的協定版本。
pub const PROTO_VERSION: u32 = 1;

/// 單行 NDJSON 的最大長度（bytes）。
pub const MAX_LINE_LENGTH: usize = 65536;

/// Client（Pulse）→ Server（Kairos）的訊息。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        seq: u64,
        proto: u32,
        agent_id: String,
        hostname: String,
        agent_version: String,
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
        /// 總記憶體（MB）
        total_memory: u64,
        /// 已使用記憶體（MB）
        used_memory: u64,
    },
}

/// Server（Kairos）→ Client（Pulse）的訊息。
#[derive(Debug, Clone, PartialEq, Deserialize)]
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

/// 目前時間，RFC 3339 UTC 毫秒格式（例如 `2026-10-08T03:15:40.001Z`）。
pub fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 送出前整理 CPU 使用率：非有限值（NaN / ±∞）視為 0，再夾在 0～100，
/// 避免伺服器回 `invalid_report`。
pub fn sanitize_cpu_usage(cpu: f32) -> f32 {
    if cpu.is_finite() {
        cpu.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// 序列化後轉成 `Value` 與測試向量比對（不比字串，避免欄位順序 / 浮點表示差異）。
    fn assert_serializes_to(msg: &ClientMsg, expected: &str) {
        let actual: Value = serde_json::from_str(&serde_json::to_string(msg).unwrap()).unwrap();
        let mut expected: Value = serde_json::from_str(expected).unwrap();
        // cpu_usage 為 f32，轉成 f64 後會有微小誤差，改以容差比對後移除
        if let (Some(a), Some(e)) = (actual.get("cpu_usage"), expected.get("cpu_usage")) {
            let (a, e) = (a.as_f64().unwrap(), e.as_f64().unwrap());
            assert!((a - e).abs() < 1e-4, "cpu_usage {a} != {e}");
            let mut actual = actual.clone();
            actual.as_object_mut().unwrap().remove("cpu_usage");
            expected.as_object_mut().unwrap().remove("cpu_usage");
            assert_eq!(actual, expected);
            return;
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn serialize_hello() {
        let msg = ClientMsg::Hello {
            seq: 1,
            proto: 1,
            agent_id: "6f1c0e4a-1b2c-4d3e-9f00-112233445566".into(),
            hostname: "web-01".into(),
            agent_version: "0.1.0".into(),
            heartbeat_interval_ms: 1000,
            ts: "2026-10-08T03:15:40.001Z".into(),
        };
        assert_serializes_to(
            &msg,
            r#"{"type":"hello","seq":1,"proto":1,"agent_id":"6f1c0e4a-1b2c-4d3e-9f00-112233445566","hostname":"web-01","agent_version":"0.1.0","heartbeat_interval_ms":1000,"ts":"2026-10-08T03:15:40.001Z"}"#,
        );
    }

    #[test]
    fn serialize_heartbeat() {
        let msg = ClientMsg::Heartbeat {
            seq: 2,
            ts: "2026-10-08T03:15:41.002Z".into(),
        };
        assert_serializes_to(
            &msg,
            r#"{"type":"heartbeat","seq":2,"ts":"2026-10-08T03:15:41.002Z"}"#,
        );
    }

    #[test]
    fn serialize_report() {
        let msg = ClientMsg::Report {
            seq: 6,
            ts: "2026-10-08T03:15:45.210Z".into(),
            cpu_usage: 17.84,
            total_memory: 49152,
            used_memory: 37034,
        };
        assert_serializes_to(
            &msg,
            r#"{"type":"report","seq":6,"ts":"2026-10-08T03:15:45.210Z","cpu_usage":17.84,"total_memory":49152,"used_memory":37034}"#,
        );
    }

    #[test]
    fn serialized_line_has_no_newline() {
        // LinesCodec 會補 `\n`，序列化結果本身不得含換行
        let line = serde_json::to_string(&ClientMsg::Heartbeat {
            seq: 1,
            ts: now_ts(),
        })
        .unwrap();
        assert!(!line.contains('\n'));
    }

    #[test]
    fn deserialize_hello_ack() {
        let msg: ServerMsg = serde_json::from_str(
            r#"{"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000}"#,
        )
        .unwrap();
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
        let msg: ServerMsg = serde_json::from_str(r#"{"type":"ack","seq":2}"#).unwrap();
        assert_eq!(msg, ServerMsg::Ack { seq: 2 });
        let msg: ServerMsg = serde_json::from_str(r#"{"type":"ack","seq":6}"#).unwrap();
        assert_eq!(msg, ServerMsg::Ack { seq: 6 });
    }

    #[test]
    fn deserialize_errors() {
        let msg: ServerMsg = serde_json::from_str(
            r#"{"type":"error","seq":1,"code":"unsupported_version","message":"supported proto: 1","fatal":true}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ServerMsg::Error {
                seq: Some(1),
                code: "unsupported_version".into(),
                message: "supported proto: 1".into(),
                fatal: true
            }
        );

        let msg: ServerMsg = serde_json::from_str(
            r#"{"type":"error","seq":null,"code":"unknown_type","message":"unknown message type","fatal":false}"#,
        )
        .unwrap();
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
    fn unknown_fields_are_ignored() {
        let msg: ServerMsg =
            serde_json::from_str(r#"{"type":"ack","seq":3,"future_field":"x"}"#).unwrap();
        assert_eq!(msg, ServerMsg::Ack { seq: 3 });
    }

    #[test]
    fn cpu_usage_is_sanitized() {
        assert_eq!(sanitize_cpu_usage(17.84), 17.84);
        assert_eq!(sanitize_cpu_usage(0.0), 0.0);
        assert_eq!(sanitize_cpu_usage(100.0), 100.0);
        assert_eq!(sanitize_cpu_usage(100.5), 100.0);
        assert_eq!(sanitize_cpu_usage(-3.0), 0.0);
        assert_eq!(sanitize_cpu_usage(f32::NAN), 0.0);
        assert_eq!(sanitize_cpu_usage(f32::INFINITY), 0.0);
        assert_eq!(sanitize_cpu_usage(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn ts_format() {
        let ts = now_ts();
        // 例如 2026-10-08T03:15:40.001Z：毫秒三位、以 Z 結尾
        assert_eq!(ts.len(), 24, "{ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[19..20], ".");
    }
}
