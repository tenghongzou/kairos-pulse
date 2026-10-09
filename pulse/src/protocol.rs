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
    /// 欄位缺漏時仍視為 error，不整則忽略：缺 `seq` → null、缺 `code` / `message` → 空字串、
    /// 缺 `fatal` → 視為 fatal（保守處理：結束連線並退避重連）。
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

/// error 訊息缺少 `fatal` 欄位時的預設值。
fn fatal_by_default() -> bool {
    true
}

/// Unicode 一般類別 Cf（格式字元，例如 U+200B 零寬空格、U+202E 由右至左覆寫、U+FEFF BOM）
/// 的所有碼位範圍（Unicode 15.1）。std 沒有提供一般類別查詢，這裡直接列表。
///
/// 與 Kairos `protocol.rs` 的同名表完全相同：Kairos 拒絕 hostname / agent_version 含這些字元，
/// 兩邊必須一致，否則 Pulse 會因 `bad_request` 無法註冊。
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

/// Cf 以外、但同樣不可見或可用來偽裝外觀的碼位（R2-K23，與 Kairos 同步）：
/// U+034F 組合字形連接符、Hangul filler（U+115F、U+1160、U+3164、U+FFA0）、
/// 變體選擇字元（U+FE00–U+FE0F、U+E0100–U+E01EF）與私用區
/// （U+E000–U+F8FF、U+F0000–U+FFFFD、U+100000–U+10FFFD）。
const INVISIBLE_OR_PRIVATE_RANGES: &[(u32, u32)] = &[
    (0x034F, 0x034F),
    (0x115F, 0x1160),
    (0x3164, 0x3164),
    (0xE000, 0xF8FF),
    (0xFE00, 0xFE0F),
    (0xFFA0, 0xFFA0),
    (0xE0100, 0xE01EF),
    (0xF0000, 0xFFFFD),
    (0x100000, 0x10FFFD),
];

/// 不得出現在顯示用字串（hello 的 hostname / agent_version、日誌）中的字元：
/// 控制字元（Cc）、格式字元（Cf）、行分隔（Zl, U+2028）、段落分隔（Zp, U+2029），
/// 以及 [`INVISIBLE_OR_PRIVATE_RANGES`]（Hangul filler、變體選擇字元、私用區等）。
/// 規則與 Kairos 的 `is_forbidden_display_char` 相同。
pub fn is_forbidden_display_char(c: char) -> bool {
    if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
        return true;
    }
    let cp = u32::from(c);
    FORMAT_CHAR_RANGES
        .iter()
        .chain(INVISIBLE_OR_PRIVATE_RANGES)
        .any(|&(lo, hi)| (lo..=hi).contains(&cp))
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

    // 下列 serialize_* / deserialize_hello_ack / deserialize_acks / deserialize_errors 使用的
    // JSON 字串是 PROTOCOL.md「範例」節的測試向量（與 Kairos 的 src/protocol.rs 相同），
    // 修改協定時兩邊一起更新。

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
        // PROTOCOL.md「範例」節的測試向量（P hello）
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
        // PROTOCOL.md「範例」節的測試向量（P heartbeat）
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
        // PROTOCOL.md「範例」節的測試向量（P report）
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
        // PROTOCOL.md「範例」節的測試向量（K hello_ack）
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
        // PROTOCOL.md「範例」節的測試向量（K ack seq=2 / seq=6）
        let msg: ServerMsg = serde_json::from_str(r#"{"type":"ack","seq":2}"#).unwrap();
        assert_eq!(msg, ServerMsg::Ack { seq: 2 });
        let msg: ServerMsg = serde_json::from_str(r#"{"type":"ack","seq":6}"#).unwrap();
        assert_eq!(msg, ServerMsg::Ack { seq: 6 });
    }

    #[test]
    fn deserialize_errors() {
        // PROTOCOL.md「範例」節「錯誤範例」的測試向量
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
    fn forbidden_display_chars_match_kairos_rules() {
        // Cc
        for c in ['\u{0}', '\n', '\u{1b}', '\u{7f}', '\u{85}', '\u{9f}'] {
            assert!(is_forbidden_display_char(c), "{c:?}");
        }
        // Cf（表的頭尾與中間各取樣）與 Zl / Zp
        for c in [
            '\u{ad}',
            '\u{600}',
            '\u{605}',
            '\u{61c}',
            '\u{200b}',
            '\u{200f}',
            '\u{202a}',
            '\u{202e}',
            '\u{2066}',
            '\u{206f}',
            '\u{feff}',
            '\u{fffb}',
            '\u{110bd}',
            '\u{1343f}',
            '\u{e0001}',
            '\u{e007f}',
            '\u{2028}',
            '\u{2029}',
        ] {
            assert!(is_forbidden_display_char(c), "{c:?}");
        }
        // 範圍邊界外與一般字元
        for c in [
            'a',
            ' ',
            '-',
            '主',
            'é',
            '\u{ac}',
            '\u{ae}',
            '\u{606}',
            '\u{2010}',
            '\u{2065}',
            '\u{2070}',
            '\u{fffc}',
            '\u{e0002}',
            '\u{e0080}',
        ] {
            assert!(!is_forbidden_display_char(c), "{c:?}");
        }
        // 表需由小到大排列且不重疊（與 Kairos 相同的整理方式，方便逐行比對）
        for table in [FORMAT_CHAR_RANGES, INVISIBLE_OR_PRIVATE_RANGES] {
            assert!(table.windows(2).all(|w| w[0].1 < w[1].0));
            assert!(table.iter().all(|&(lo, hi)| lo <= hi));
        }
    }

    #[test]
    fn invisible_fillers_selectors_and_private_use_are_forbidden() {
        // R2-K23：每個範圍的頭尾都禁止
        for cp in [
            0x034F, 0x115F, 0x1160, 0x3164, 0xE000, 0xF8FF, 0xFE00, 0xFE0F, 0xFFA0, 0xE0100,
            0xE01EF, 0xF0000, 0xFFFFD, 0x100000, 0x10FFFD,
        ] {
            let c = char::from_u32(cp).unwrap();
            assert!(is_forbidden_display_char(c), "U+{cp:04X}");
        }
        // 範圍外緊鄰的碼位不受影響
        for cp in [
            0x034E, 0x0350, 0x115E, 0x1161, 0x3163, 0x3165, 0xF900, 0xFDFF, 0xFE10, 0xFF9F, 0xFFA1,
            0xE00FF, 0xE01F0, 0xEFFFF, 0xFFFFE, 0x10FFFE,
        ] {
            let c = char::from_u32(cp).unwrap();
            assert!(!is_forbidden_display_char(c), "U+{cp:04X}");
        }
    }

    #[test]
    fn agent_version_has_no_forbidden_chars() {
        // agent_version 取自編譯時的 CARGO_PKG_VERSION（semver），不會含禁止字元
        let v = env!("CARGO_PKG_VERSION");
        assert!(!v.is_empty() && v.len() <= 64);
        assert!(!v.chars().any(is_forbidden_display_char), "{v:?}");
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
