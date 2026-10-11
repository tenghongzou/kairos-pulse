//! 從 PROTOCOL.md 解析測試向量與錯誤碼表（審計 Q1）。
//!
//! 「範例」節的程式碼區塊中，`P ` 開頭的行是 Pulse → Kairos 的訊息，`K ` 開頭的是 Kairos → Pulse。
//! 其他模組的測試以 [`client_vector`] 等函式取用，規格與測試只有一份來源。

use crate::{ClientMsg, ErrorCode, ServerMsg, parse_client_msg, parse_server_msg};
use serde_json::Value;

/// 協定規格全文（唯一的一份）
pub(crate) const SPEC: &str = include_str!("../PROTOCOL.md");

/// 取出 `## <title>` 這一節的各行（到下一個 `## ` 標題為止）。`str::lines` 會去除行尾的 CR，
/// Windows 上以 CRLF 簽出的文件也能解析。
fn section(title: &str) -> Vec<&'static str> {
    let heading = format!("## {title}");
    let mut lines = SPEC.lines();
    assert!(
        lines.any(|l| l == heading),
        "PROTOCOL.md 找不到「{title}」節"
    );
    lines.take_while(|l| !l.starts_with("## ")).collect()
}

/// 「範例」節中指定方向（`P` 或 `K`）的所有訊息，依出現順序
fn vectors(direction: char) -> Vec<&'static str> {
    let prefix = format!("{direction} ");
    section("範例")
        .into_iter()
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .filter(|l| l.starts_with('{'))
        .collect()
}

/// 依條件找出恰好一則測試向量
fn find(direction: char, what: &str, pred: impl Fn(&Value) -> bool) -> &'static str {
    let found: Vec<_> = vectors(direction)
        .into_iter()
        .filter(|l| pred(&serde_json::from_str(l).expect("測試向量必須是合法 JSON")))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "PROTOCOL.md 中 {what} 的測試向量應恰好一則: {found:?}"
    );
    found[0]
}

/// `type` 為 `kind` 的 client 訊息測試向量
pub(crate) fn client_vector(kind: &str) -> &'static str {
    find('P', kind, |v| v["type"] == kind)
}

/// `type` 為 `kind` 的 server 訊息測試向量（範例中只有一則的類型，例如 `hello_ack`）
pub(crate) fn server_vector(kind: &str) -> &'static str {
    find('K', kind, |v| v["type"] == kind)
}

/// `seq` 為 `seq` 的 ack 測試向量
pub(crate) fn ack_vector(seq: u64) -> &'static str {
    find('K', &format!("ack seq={seq}"), |v| {
        v["type"] == "ack" && v["seq"] == seq
    })
}

/// `code` 為 `code` 的 error 測試向量
pub(crate) fn server_error_vector(code: &str) -> &'static str {
    find('K', code, |v| v["type"] == "error" && v["code"] == code)
}

#[test]
fn spec_vectors_are_found() {
    // 解析規則改壞時（例如區塊格式變了）不會默默變成零個向量
    assert_eq!(vectors('P').len(), 3, "{:?}", vectors('P'));
    assert_eq!(vectors('K').len(), 5, "{:?}", vectors('K'));
}

#[test]
fn every_client_vector_round_trips() {
    for line in vectors('P') {
        let msg = parse_client_msg(line).unwrap();
        assert_ne!(msg, ClientMsg::Unknown, "{line}");
        let back: Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back, serde_json::from_str::<Value>(line).unwrap(), "{line}");
    }
}

#[test]
fn every_server_vector_round_trips() {
    for line in vectors('K') {
        let msg = parse_server_msg(line).unwrap();
        let back: Value = serde_json::from_str(&msg.to_line().unwrap()).unwrap();
        assert_eq!(back, serde_json::from_str::<Value>(line).unwrap(), "{line}");
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
    // 規格文字中的數值與常數一致（只檢查最容易漏改的幾個）
    assert!(SPEC.contains(&format!("單行上限 **{} bytes**", crate::MAX_LINE_LENGTH)));
    assert!(SPEC.contains(&format!(
        "| `hostname` | 1～{} bytes",
        crate::MAX_HOSTNAME_BYTES
    )));
    assert!(SPEC.contains(&format!(
        "| `agent_version` | 1～{} bytes",
        crate::MAX_AGENT_VERSION_BYTES
    )));
    assert!(SPEC.contains(&format!("最長 {} bytes", crate::MAX_TS_BYTES)));
    assert!(SPEC.contains(&format!(
        "夾在 {}～{} 之間",
        crate::MIN_HEARTBEAT_INTERVAL_MS,
        crate::MAX_HEARTBEAT_INTERVAL_MS
    )));
    assert!(SPEC.contains(&format!(
        "連續 {} 則無效訊息",
        crate::MAX_CONSECUTIVE_BAD_REQUESTS
    )));
    assert!(SPEC.contains(&format!("`proto`（目前為 `{}`）", crate::PROTO_VERSION)));
}
