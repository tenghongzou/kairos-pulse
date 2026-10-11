//! 欄位限制與驗證：agent_id、顯示用字串的禁止字元、hello／`ts`／report、心跳與離線門檻。
//!
//! Kairos 用這些函式判定是否回 `bad_request` / `invalid_report`；Pulse 用同一份規則在送出前
//! 整理 hostname、檢查 agent_id，兩邊不會再因各自維護一份表而不一致（R2-K23）。

use crate::{
    ClientMsg, MAX_AGENT_VERSION_BYTES, MAX_HEARTBEAT_INTERVAL_MS, MAX_HOSTNAME_BYTES,
    MAX_TS_BYTES, MIN_HEARTBEAT_INTERVAL_MS, MIN_OFFLINE_AFTER_MS, OFFLINE_HEARTBEAT_MULTIPLIER,
};

/// 將 client 宣告的心跳間隔夾在 [`MIN_HEARTBEAT_INTERVAL_MS`]..=[`MAX_HEARTBEAT_INTERVAL_MS`]
pub fn clamp_heartbeat_interval_ms(heartbeat_interval_ms: u64) -> u64 {
    heartbeat_interval_ms.clamp(MIN_HEARTBEAT_INTERVAL_MS, MAX_HEARTBEAT_INTERVAL_MS)
}

/// 由 client 宣告的心跳間隔計算離線門檻：
/// `max(5 × clamp(heartbeat_interval_ms, 1000, 60000), 5000)`，結果介於 5000..=300000
pub fn offline_after_ms(heartbeat_interval_ms: u64) -> u64 {
    clamp_heartbeat_interval_ms(heartbeat_interval_ms)
        .saturating_mul(OFFLINE_HEARTBEAT_MULTIPLIER)
        .max(MIN_OFFLINE_AFTER_MS)
}

/// 伺服器端（Kairos）驗證與保存 `cpu_usage` 時使用的 f32 值（A14）。
///
/// 線上型別統一為 f64（見 [`ClientMsg::Report`]），但 Kairos 一向以 f32 驗證、寫入 JSONL 與
/// agents.json：例如 `100.000001` 在 f32 下等於 `100.0` 而被接受，`1e39` 轉成 f32 為 `inf` 而被拒絕。
/// serde_json（未啟用 `float_roundtrip`）反序列化 f32 時也是先解析成 f64 再以 `as` 轉型，
/// 因此對所有小數輸入，這裡的結果與先前直接反序列化成 f32 完全相同。
/// 唯一的差異是超過 2^53 的整數輸入（先轉 f64 再轉 f32 可能多一次捨入），這些值都遠大於 100、
/// 一律被拒絕，只可能影響錯誤說明中顯示的數值。
pub fn server_cpu_usage(cpu_usage: f64) -> f32 {
    cpu_usage as f32
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

/// agent_id 的正規化形式：UUID 不分大小寫，一律以小寫識別、註冊、比對與寫檔
/// （伺服器在 [`validate_hello`] 通過後呼叫；Pulse 讀入 agent.id 後也轉成小寫再送出）。
pub fn normalize_agent_id(agent_id: &str) -> String {
    agent_id.to_ascii_lowercase()
}

/// Unicode 一般類別 Cf（格式字元，例如 U+200B 零寬空格、U+202E 由右至左覆寫、U+FEFF BOM）
/// 的所有碼位範圍（Unicode 15.1）。std 沒有提供一般類別查詢，這裡直接列表。
/// 由小到大排列且不重疊。
pub(crate) const FORMAT_CHAR_RANGES: &[(u32, u32)] = &[
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

/// 雖不屬於 Cc／Cf，但顯示為空白或不可見、可用來偽裝或隱藏內容的碼位（R2-K23）。
/// 由小到大排列且不重疊：
/// - U+034F 組合用字形連接抑制符（CGJ）
/// - Hangul filler：U+115F、U+1160、U+3164、U+FFA0（顯示為空白）
/// - 變體選擇字元：U+FE00–U+FE0F、U+E0100–U+E01EF
/// - 私用區：U+E000–U+F8FF、U+F0000–U+FFFFD、U+100000–U+10FFFD（顯示結果取決於字型，無法預期）
pub(crate) const INVISIBLE_CHAR_RANGES: &[(u32, u32)] = &[
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

/// 不得出現在顯示用字串（hello 的 hostname / agent_version、`ts`、日誌）中的字元：
/// 控制字元（Cc）、格式字元（Cf）、行分隔（Zl, U+2028）、段落分隔（Zp, U+2029），
/// 以及 [`INVISIBLE_CHAR_RANGES`] 列出的不可見／偽裝用碼位。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_client_msg;
    use crate::tests::client_vector;

    // ---- 原 Kairos src/protocol.rs 的測試 ----

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
            "６f1c0e4a-1b2c-4d3e-9f00-11223344556",   // 全形數字（原 Pulse agent_id.rs 的案例）
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
        for kind in ["hello", "heartbeat", "report"] {
            assert!(validate_client_msg(&parse_client_msg(client_vector(kind)).unwrap()).is_ok());
        }
    }

    // ---- 原 Pulse src/protocol.rs 的測試 ----

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
        // 表需由小到大排列且不重疊
        for table in [FORMAT_CHAR_RANGES, INVISIBLE_CHAR_RANGES] {
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

    // ---- 合併後新增的測試 ----

    #[test]
    fn server_cpu_usage_matches_direct_f32_deserialization() {
        // A14：線上型別改為 f64 後，Kairos 轉成 f32 的結果必須與先前直接反序列化成 f32 相同
        #[derive(serde::Deserialize)]
        struct OldReport {
            cpu_usage: f32,
        }
        for text in [
            "0",
            "0.0",
            "100",
            "100.0",
            "17.84",
            "7.5000005",
            "33.333336",
            "99.995",
            "-0.0",
            "-0.01",
            "100.000001",
            "100.00001",
            "1e-50",
            "1e39",
            "-1e39",
            "1e308",
            "1e-7",
            "4294967296",
            "-5",
            "0.1",
            "0.2",
            "0.30000000000000004",
            "16777217",
            "9007199254740993",
        ] {
            let line = format!(r#"{{"cpu_usage":{text}}}"#);
            let old: OldReport = serde_json::from_str(&line).unwrap();
            let wire: serde_json::Value = serde_json::from_str(&line).unwrap();
            let new = server_cpu_usage(wire["cpu_usage"].as_f64().unwrap());
            assert_eq!(
                old.cpu_usage.to_bits(),
                new.to_bits(),
                "{text}: {} vs {}",
                old.cpu_usage,
                new
            );
            // 接受與拒絕的結果相同
            assert_eq!(
                validate_report(old.cpu_usage, 1, 1).is_ok(),
                validate_report(new, 1, 1).is_ok(),
                "{text}"
            );
        }
        // f32 精度下 100.000001 等於 100.0，仍被接受（與合併前的 Kairos 相同）
        assert!(validate_report(server_cpu_usage(100.000001), 1, 1).is_ok());
    }
}
