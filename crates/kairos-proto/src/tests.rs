//! 從 PROTOCOL.md 解析測試向量與錯誤碼表（審計 Q1）。
//!
//! 「範例」節的程式碼區塊中，`P ` 開頭的行是 Pulse → Kairos 的訊息，`K ` 開頭的是 Kairos → Pulse；
//! v1、v2 與錯誤範例分別在 `### v1`、`### v2`、`### 錯誤` 小節。
//! 其他模組的測試以 [`client_vector`] 等函式取用，規格與測試只有一份來源。

use crate::{ClientMsg, ErrorCode, ServerMsg, parse_client_msg, parse_server_msg};
use serde_json::Value;

/// 協定規格全文（唯一的一份）
pub(crate) const SPEC: &str = include_str!("../PROTOCOL.md");

/// 取出 `## <title>` 這一節的各行（到下一個 `## ` 標題為止）。`str::lines` 會去除行尾的 CR，
/// Windows 上以 CRLF 簽出的文件也能解析。
pub(crate) fn section(title: &str) -> Vec<&'static str> {
    let heading = format!("## {title}");
    let mut lines = SPEC.lines();
    assert!(
        lines.any(|l| l == heading),
        "PROTOCOL.md 找不到「{title}」節"
    );
    lines.take_while(|l| !l.starts_with("## ")).collect()
}

/// 「範例」節中 `### <sub>` 小節的各行（到下一個 `### ` 為止）
fn example_subsection(sub: &str) -> Vec<&'static str> {
    let heading = format!("### {sub}");
    let lines = section("範例");
    let start = lines
        .iter()
        .position(|l| *l == heading)
        .unwrap_or_else(|| panic!("PROTOCOL.md 的「範例」節找不到「{sub}」小節"));
    lines[start + 1..]
        .iter()
        .take_while(|l| !l.starts_with("### "))
        .copied()
        .collect()
}

/// 測試向量所在的小節
#[derive(Debug, Clone, Copy)]
pub(crate) enum Ver {
    V1,
    V2,
    Errors,
}

impl Ver {
    fn title(self) -> &'static str {
        match self {
            Ver::V1 => "v1",
            Ver::V2 => "v2",
            Ver::Errors => "錯誤",
        }
    }
}

/// 指定小節中指定方向（`P` 或 `K`）的所有訊息，依出現順序
pub(crate) fn vectors(ver: Ver, direction: char) -> Vec<&'static str> {
    let prefix = format!("{direction} ");
    example_subsection(ver.title())
        .into_iter()
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .filter(|l| l.starts_with('{'))
        .collect()
}

/// 依條件找出恰好一則測試向量
fn find(ver: Ver, direction: char, what: &str, pred: impl Fn(&Value) -> bool) -> &'static str {
    let found: Vec<_> = vectors(ver, direction)
        .into_iter()
        .filter(|l| pred(&serde_json::from_str(l).expect("測試向量必須是合法 JSON")))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "PROTOCOL.md {ver:?} 中 {what} 的測試向量應恰好一則: {found:?}"
    );
    found[0]
}

/// v1 小節中 `type` 為 `kind` 的 client 訊息測試向量
pub(crate) fn client_vector(kind: &str) -> &'static str {
    client_vector_in(Ver::V1, kind)
}

/// 指定小節中 `type` 為 `kind` 的 client 訊息測試向量
pub(crate) fn client_vector_in(ver: Ver, kind: &str) -> &'static str {
    find(ver, 'P', kind, |v| v["type"] == kind)
}

/// v1 小節中 `type` 為 `kind` 的 server 訊息測試向量（小節中只有一則的類型，例如 `hello_ack`）
pub(crate) fn server_vector(kind: &str) -> &'static str {
    server_vector_in(Ver::V1, kind)
}

/// 指定小節中 `type` 為 `kind` 的 server 訊息測試向量
pub(crate) fn server_vector_in(ver: Ver, kind: &str) -> &'static str {
    find(ver, 'K', kind, |v| v["type"] == kind)
}

/// v1 小節中 `seq` 為 `seq` 的 ack 測試向量
pub(crate) fn ack_vector(seq: u64) -> &'static str {
    find(Ver::V1, 'K', &format!("ack seq={seq}"), |v| {
        v["type"] == "ack" && v["seq"] == seq
    })
}

/// 錯誤小節中 `code` 為 `code` 的 error 測試向量
pub(crate) fn server_error_vector(code: &str) -> &'static str {
    find(Ver::Errors, 'K', code, |v| {
        v["type"] == "error" && v["code"] == code
    })
}

#[test]
fn spec_vectors_are_found() {
    // 解析規則改壞時（例如區塊格式變了）不會默默變成零個向量
    assert_eq!(vectors(Ver::V1, 'P').len(), 3);
    assert_eq!(vectors(Ver::V1, 'K').len(), 3);
    assert_eq!(vectors(Ver::V2, 'P').len(), 3);
    assert_eq!(vectors(Ver::V2, 'K').len(), 3);
    assert_eq!(vectors(Ver::Errors, 'P').len(), 0);
    assert_eq!(vectors(Ver::Errors, 'K').len(), 2);
}

#[test]
fn every_client_vector_round_trips() {
    for ver in [Ver::V1, Ver::V2] {
        for line in vectors(ver, 'P') {
            let msg = parse_client_msg(line).unwrap();
            assert_ne!(msg, ClientMsg::Unknown, "{line}");
            let back: Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
            assert_eq!(back, serde_json::from_str::<Value>(line).unwrap(), "{line}");
        }
    }
}

#[test]
fn every_server_vector_round_trips() {
    for ver in [Ver::V1, Ver::V2, Ver::Errors] {
        for line in vectors(ver, 'K') {
            let msg = parse_server_msg(line).unwrap();
            let back: Value = serde_json::from_str(&msg.to_line().unwrap()).unwrap();
            assert_eq!(back, serde_json::from_str::<Value>(line).unwrap(), "{line}");
        }
    }
}

#[test]
fn error_code_table_matches_spec() {
    // 「錯誤碼」節的表格：| `code` | 情境 | fatal（是／否…） |
    let mut rows = Vec::new();
    for line in section("錯誤碼") {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 4 {
            continue;
        }
        let Some(code) = cells[1].strip_prefix('`').and_then(|c| c.strip_suffix('`')) else {
            continue;
        };
        let fatal_cell = cells[cells.len() - 2];
        let fatal = if fatal_cell.starts_with('是') {
            true
        } else if fatal_cell.starts_with('否') {
            false
        } else {
            panic!("無法判讀 {code} 的 fatal 欄：{fatal_cell}");
        };
        rows.push((code, fatal));
    }
    assert_eq!(rows.len(), ErrorCode::ALL.len(), "{rows:?}");
    for (code, fatal) in rows {
        let parsed = ErrorCode::from_code(code)
            .unwrap_or_else(|| panic!("PROTOCOL.md 的錯誤碼 {code} 沒有對應的 ErrorCode"));
        assert_eq!(parsed.default_fatal(), fatal, "{code}");
        assert_eq!(
            ServerMsg::error(None, parsed, "").is_fatal(),
            fatal,
            "{code}"
        );
    }
}

#[test]
fn spec_limits_match_constants() {
    use crate::*;
    // 規格文字中的數值與常數一致（只檢查最容易漏改的幾個）
    let has = |s: String| assert!(SPEC.contains(&s), "PROTOCOL.md 應包含：{s}");
    has(format!("單行上限 **{MAX_LINE_LENGTH} bytes**"));
    has(format!("| `hostname` | 1～{MAX_HOSTNAME_BYTES} bytes"));
    has(format!(
        "| `agent_version` | 1～{MAX_AGENT_VERSION_BYTES} bytes"
    ));
    has(format!("最長 {MAX_TS_BYTES} bytes"));
    has(format!(
        "夾在 {MIN_HEARTBEAT_INTERVAL_MS}～{MAX_HEARTBEAT_INTERVAL_MS} 之間"
    ));
    has(format!("連續 {MAX_CONSECUTIVE_BAD_REQUESTS} 則無效訊息"));
    has(format!(
        "支援的版本範圍為 `{}`～`{}`",
        SUPPORTED_PROTO.min, SUPPORTED_PROTO.max
    ));
    has(format!(
        "**server（Kairos）**的範圍為 `[{}, {}]`",
        SUPPORTED_PROTO.min, SUPPORTED_PROTO.max
    ));
    // v2 hello 欄位
    has(format!(
        "最多 {MAX_FEATURES} 項，每項 1～{MAX_FEATURE_BYTES} bytes"
    ));
    has(format!("最長 {MAX_HOST_FINGERPRINT_BYTES} bytes。Pulse"));
    has(format!(
        "主控台的顯示名稱，1～{MAX_DISPLAY_NAME_BYTES} bytes"
    ));
    has(format!("最多 {MAX_TAGS} 個，每個 1～{MAX_TAG_BYTES} bytes"));
    has(format!(
        "整數 {MIN_REPORT_INTERVAL_MS}～{MAX_REPORT_INTERVAL_MS}"
    ));
    has(format!("各 1～{MAX_OS_FIELD_BYTES} bytes"));
    // v2 指標
    has(format!(
        "| `name` | string | 1～{MAX_METRIC_NAME_BYTES} bytes"
    ));
    has(format!(
        "| `unit` | string | 1～{MAX_METRIC_UNIT_BYTES} bytes"
    ));
    has(format!(
        "最多 {MAX_METRIC_LABELS} 個。名稱 1～{MAX_LABEL_KEY_BYTES} bytes"
    ));
    has(format!("值 1～{MAX_LABEL_VALUE_BYTES} bytes"));
    has(format!("整個陣列 1～{MAX_METRICS} 個元素"));
    // features 表與 SERVER_FEATURES 一致
    for feature in SERVER_FEATURES {
        has(format!("| `{feature}` |"));
    }
}

#[test]
fn standard_metric_table_matches_spec() {
    // 「標準指標」表：| `name` | `unit` | 範圍 | ...
    let rows: Vec<(String, String)> = SPEC
        .lines()
        .filter(|l| l.starts_with("| `system."))
        .map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            (
                cells[1].trim_matches('`').to_string(),
                cells[2].trim_matches('`').to_string(),
            )
        })
        .collect();
    assert_eq!(rows.len(), crate::STANDARD_METRICS.len(), "{rows:?}");
    for (std, (name, unit)) in crate::STANDARD_METRICS.iter().zip(&rows) {
        assert_eq!((std.name, std.unit), (name.as_str(), unit.as_str()));
    }
}

#[test]
fn host_fingerprint_examples_are_well_formed() {
    // 「主機指紋」的計算範例（Pulse 的實作以同一張表測試計算結果）
    let rows = host_fingerprint_examples();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for (_, fp) in rows {
        assert!(crate::is_host_fingerprint(fp), "{fp}");
    }
}

/// 「主機指紋」節的計算範例：(機器 ID, host_fingerprint)
pub(crate) fn host_fingerprint_examples() -> Vec<(&'static str, &'static str)> {
    SPEC.lines()
        .filter_map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            (cells.len() == 4 && cells[2].starts_with("`sha256:"))
                .then(|| (cells[1].trim_matches('`'), cells[2].trim_matches('`')))
        })
        .collect()
}
