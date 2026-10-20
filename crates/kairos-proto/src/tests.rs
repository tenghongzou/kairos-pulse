//! 從 PROTOCOL.md 解析測試向量與錯誤碼表（審計 Q1）。
//!
//! 「範例」節的程式碼區塊中，`P ` 開頭的行是 Pulse → Kairos 的訊息，`K ` 開頭的是 Kairos → Pulse；
//! v1、v2、durable 與錯誤範例分別在 `### v1`、`### v2`、`### durable`、`### 錯誤` 小節。
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
    Durable,
    /// 正常輪替（feature `renew`）
    Renew,
    /// 除役通知（feature `deregister`）
    Deregister,
    /// 叢集連線與排空（feature `cluster`）
    Cluster,
    /// 遠端管理（feature `config`、`upgrade`）
    Mgmt,
    Errors,
}

impl Ver {
    fn title(self) -> &'static str {
        match self {
            Ver::V1 => "v1",
            Ver::V2 => "v2",
            Ver::Durable => "durable",
            Ver::Renew => "輪替",
            Ver::Deregister => "除役",
            Ver::Cluster => "叢集",
            Ver::Mgmt => "遠端管理",
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
    assert_eq!(vectors(Ver::Durable, 'P').len(), 2);
    assert_eq!(vectors(Ver::Durable, 'K').len(), 2);
    assert_eq!(vectors(Ver::Renew, 'P').len(), 2);
    assert_eq!(vectors(Ver::Renew, 'K').len(), 2);
    assert_eq!(vectors(Ver::Deregister, 'P').len(), 2);
    assert_eq!(vectors(Ver::Deregister, 'K').len(), 2);
    assert_eq!(vectors(Ver::Cluster, 'P').len(), 2);
    assert_eq!(vectors(Ver::Cluster, 'K').len(), 3);
    assert_eq!(vectors(Ver::Mgmt, 'P').len(), 8);
    assert_eq!(vectors(Ver::Mgmt, 'K').len(), 10);
    assert_eq!(vectors(Ver::Errors, 'P').len(), 0);
    assert_eq!(vectors(Ver::Errors, 'K').len(), 5);
}

#[test]
fn every_client_vector_round_trips() {
    for ver in [
        Ver::V1,
        Ver::V2,
        Ver::Durable,
        Ver::Renew,
        Ver::Deregister,
        Ver::Cluster,
        Ver::Mgmt,
    ] {
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
    for ver in [
        Ver::V1,
        Ver::V2,
        Ver::Durable,
        Ver::Renew,
        Ver::Deregister,
        Ver::Cluster,
        Ver::Mgmt,
        Ver::Errors,
    ] {
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
    // durable
    has(format!(
        "已保存的最大 `report_seq` − {}」",
        DURABLE_DEDUP_WINDOW - 1
    ));
    has(format!("同一時間最多 **{MAX_DURABLE_IN_FLIGHT} 筆**在途"));
    has(format!(
        "每條連線最多 {MAX_DURABLE_IN_FLIGHT} 筆 report 在途"
    ));
    // features 表與 SERVER_FEATURES、LIFECYCLE_FEATURES 一致
    for feature in SERVER_FEATURES
        .iter()
        .chain(LIFECYCLE_FEATURES.iter())
        .chain([FEATURE_CLUSTER].iter())
        .chain(MGMT_FEATURES.iter())
    {
        has(format!("| `{feature}` |"));
    }
    // feature cluster
    has(format!(
        "{MIN_DRAIN_RECONNECT_MS}～{MAX_DRAIN_RECONNECT_MS}，超出範圍時 client 夾在範圍內，缺漏或型別不符時為 {DEFAULT_DRAIN_RECONNECT_MS}"
    ));
    has(format!(
        "Kairos 送 `max({DEFAULT_DRAIN_RECONNECT_MS}, 連線數 × {DRAIN_MS_PER_CONNECTION})`"
    ));
    has(format!(
        "1～{MAX_SERVER_NODE_BYTES} bytes，只有小寫英文字母"
    ));
    has(format!(
        "最多 {} 秒；沒有回應的 report",
        DRAIN_IN_FLIGHT_WAIT_MS / 1000
    ));
    // 訊息表列出所有 server 訊息類型
    for kind in SERVER_MSG_TYPES {
        has(format!("| `{kind}` |"));
    }
    // feature config、upgrade
    use crate::mgmt::*;
    has(format!(
        "各 1～{MAX_INSTALL_FIELD_BYTES} bytes 的小寫英文字母、數字或 `_`"
    ));
    has(format!("各最多 {MAX_STATUS_ITEMS} 項"));
    has(format!("`message` ≤ {MAX_STATUS_MESSAGE_BYTES} bytes"));
    has(format!("每段最多 {MAX_CHUNK_BYTES} bytes"));
    has(format!("`length`（1～{MAX_CHUNK_BYTES}）"));
    has(format!("最多 **{MAX_CHUNKS_IN_FLIGHT} 段**在途"));
    has(format!("連續 {MAX_DOWNLOAD_HASH_MISMATCHES} 次不符時放棄"));
    has(format!("（預設 {DEFAULT_MAX_CONCURRENT_DOWNLOADS} 台）"));
    has(format!(
        "健康確認期限（{MIN_HEALTH_TIMEOUT_SECS}～{MAX_HEALTH_TIMEOUT_SECS}）"
    ));
    has(format!("（≤ {} KiB）", MAX_MANIFEST_BYTES / 1024));
    has(format!("全文（≤ {} KiB）", MAX_SHA256SUMS_BYTES / 1024));
    has(format!("全文（≤ {} KiB，", MAX_SIGNATURE_BYTES / 1024));
    has(format!("0～{MAX_UPGRADE_DETAIL_BYTES} bytes"));
    has(format!("≤ {MAX_ARTIFACT_URL_BYTES} bytes）"));
    has(format!("1～{} MiB", MAX_ARTIFACT_BYTES / 1024 / 1024));
    for state in CONFIG_STATES.iter().chain(UPGRADE_STATES.iter()) {
        has(format!("`{state}`"));
    }
    for t in MGMT_CLIENT_MSG_TYPES {
        has(format!("| `{t}` |"));
    }
    has(format!(
        "namespace 為 `{}`",
        crate::release::RELEASE_NAMESPACE
    ));
    has(format!(
        "principal 為 `{}`",
        crate::release::RELEASE_PRINCIPAL
    ));
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

// ---- 8443 的憑證輪替與除役（feature renew、deregister） ----

#[test]
fn lifecycle_vectors_parse_into_the_shared_types() {
    use crate::{FEATURE_DEREGISTER, FEATURE_RENEW, INTENT_DEREGISTER};
    // 輪替：hello 宣告 renew；hello_ack 帶寬限天數與 renew_now
    let ClientMsg::Hello { ext, .. } =
        parse_client_msg(client_vector_in(Ver::Renew, "hello")).unwrap()
    else {
        panic!("應解析為 hello");
    };
    assert!(
        ext.offer
            .as_ref()
            .unwrap()
            .features
            .as_ref()
            .unwrap()
            .iter()
            .any(|f| f == FEATURE_RENEW)
    );
    assert_eq!(ext.intent, None);
    match parse_server_msg(server_vector_in(Ver::Renew, "hello_ack")).unwrap() {
        ServerMsg::HelloAck {
            expired_renew_grace_days,
            renew_now,
            ..
        } => assert_eq!(
            (expired_renew_grace_days, renew_now),
            (Some(90), Some(true))
        ),
        other => panic!("{other:?}"),
    }
    let ClientMsg::RenewRequest(req) =
        parse_client_msg(client_vector_in(Ver::Renew, "renew_request")).unwrap()
    else {
        panic!("應解析為 renew_request");
    };
    assert_eq!(req.seq, 2);
    assert_eq!(req.request_id, "5b0e9c1a-8d27-4f63-a1e4-7c9d2b6f3e81");
    let ServerMsg::RenewResult(res) =
        parse_server_msg(server_vector_in(Ver::Renew, "renew_result")).unwrap()
    else {
        panic!("應解析為 renew_result");
    };
    assert_eq!(
        (res.seq, res.state.as_str()),
        (2, crate::lifecycle::RENEW_STATE_ISSUED)
    );
    assert_eq!(res.request_id, req.request_id);
    assert_eq!(res.policy.unwrap().expired_renew_grace_days, 90);

    // 除役：intent 與 deregister／deregister_ack
    let ClientMsg::Hello { ext, .. } =
        parse_client_msg(client_vector_in(Ver::Deregister, "hello")).unwrap()
    else {
        panic!("應解析為 hello");
    };
    assert_eq!(ext.intent.as_deref(), Some(INTENT_DEREGISTER));
    assert_eq!(
        ext.offer.as_ref().unwrap().features.as_deref(),
        Some(&[FEATURE_DEREGISTER.to_string()][..])
    );
    let ClientMsg::Deregister(d) =
        parse_client_msg(client_vector_in(Ver::Deregister, "deregister")).unwrap()
    else {
        panic!("應解析為 deregister");
    };
    assert_eq!(
        d.reason,
        crate::lifecycle::DEREGISTER_REASON_UNINSTALL_PURGE
    );
    assert!(crate::validate_client_msg(&ClientMsg::Deregister(d.clone())).is_ok());
    let ServerMsg::DeregisterAck(ack) =
        parse_server_msg(server_vector_in(Ver::Deregister, "deregister_ack")).unwrap()
    else {
        panic!("應解析為 deregister_ack");
    };
    assert_eq!(ack.seq, d.seq);
    assert!(ack.certificate_revoked && !ack.already_decommissioned);
    // 沒有 renew 時 hello_ack 不帶新欄位
    let line = server_vector_in(Ver::Deregister, "hello_ack");
    assert!(!line.contains("expired_renew_grace_days") && !line.contains("renew_now"));
}

#[test]
fn lifecycle_additions_do_not_change_the_older_wire_format() {
    // 舊版的向量（v1、v2、durable）照舊解析並序列化回相同的 JSON（every_*_round_trips），
    // 而且不會出現任何新欄位：
    for ver in [Ver::V1, Ver::V2, Ver::Durable] {
        for line in vectors(ver, 'K').into_iter().chain(vectors(ver, 'P')) {
            for new_field in ["intent", "expired_renew_grace_days", "renew_now"] {
                assert!(!line.contains(new_field), "{line}");
            }
        }
    }
    // HelloExt::new（沒有 intent）序列化出的 hello 沒有 intent 欄位
    let ClientMsg::Hello { ext, .. } =
        parse_client_msg(client_vector_in(Ver::V2, "hello")).unwrap()
    else {
        panic!("應解析為 hello");
    };
    assert_eq!(ext.intent, None);
    let v = serde_json::to_value(&*ext).unwrap();
    assert!(v.get("intent").is_none(), "{v}");
    // 加上 intent 時才輸出
    let with = serde_json::to_value(ext.as_ref().clone().with_intent("deregister")).unwrap();
    assert_eq!(with["intent"], "deregister");
}

#[test]
fn lenient_lifecycle_fields() {
    // hello 的 intent 不是字串：視為沒有（hello 仍可解析）
    let line = client_vector_in(Ver::V2, "hello").replacen('{', r#"{"intent":7,"#, 1);
    let ClientMsg::Hello { ext, .. } = parse_client_msg(&line).unwrap() else {
        panic!("應解析為 hello");
    };
    assert_eq!(ext.intent, None);
    assert!(ext.offer.is_ok() && ext.host.is_ok());
    // hello_ack 的新欄位型別不符：視為沒有，整則仍可解析
    let line = r#"{"type":"hello_ack","seq":1,"proto":2,"offline_after_ms":75000,"features":["renew"],"server_time":"2026-10-14T02:00:00.009Z","expired_renew_grace_days":"90","renew_now":"yes"}"#;
    match parse_server_msg(line).unwrap() {
        ServerMsg::HelloAck {
            expired_renew_grace_days: None,
            renew_now: None,
            features: Some(f),
            ..
        } => assert_eq!(f, ["renew"]),
        other => panic!("{other:?}"),
    }
    // deregister_ack 缺少布林欄位：預設 false
    let line = r#"{"type":"deregister_ack","seq":2,"request_id":"r","agent_id":"a","state":"decommissioned","decommissioned_at":"2026-10-14T03:00:00.015Z"}"#;
    let ServerMsg::DeregisterAck(ack) = parse_server_msg(line).unwrap() else {
        panic!("應解析為 deregister_ack");
    };
    assert!(!ack.certificate_revoked && !ack.already_decommissioned);
    // 必要欄位缺漏的 renew_request 是無法解析的訊息（伺服器依是否協商決定 bad_request 或 unknown_type）
    let line = r#"{"type":"renew_request","seq":2}"#;
    assert!(parse_client_msg(line).is_err());
    assert_eq!(crate::message_type(line).as_deref(), Some("renew_request"));
    assert_eq!(crate::message_type(r#"{"type":5}"#), None);
    assert_eq!(crate::message_type("not json"), None);
    // ts 的限制同其他訊息
    let long = client_vector_in(Ver::Renew, "renew_request").replace(
        "2026-10-14T02:00:01.000Z\",\"request_id",
        &format!("{}\",\"request_id", "x".repeat(65)),
    );
    let msg = parse_client_msg(&long).unwrap();
    assert!(crate::validate_client_msg(&msg).unwrap_err().contains("ts"));
}

#[test]
fn new_error_codes_are_not_fatal() {
    for (code, text) in [
        (ErrorCode::RenewRequiresApproval, "renew_requires_approval"),
        (ErrorCode::InvalidCsr, "invalid_csr"),
        (ErrorCode::RateLimited, "rate_limited"),
    ] {
        assert_eq!(code.as_str(), text);
        assert_eq!(ErrorCode::from_code(text), Some(code));
        assert!(!code.default_fatal());
        assert_eq!(
            serde_json::to_value(ServerMsg::error(Some(2), code, "m")).unwrap(),
            serde_json::json!({"type":"error","seq":2,"code":text,"message":"m","fatal":false})
        );
    }
    // 錯誤範例的向量與 ServerMsg::error 的輸出相同
    let v = server_error_vector("renew_requires_approval");
    let built = ServerMsg::error(
        Some(2),
        ErrorCode::RenewRequiresApproval,
        "renewal requires approval or offline issuance; resend the same CSR later",
    );
    assert_eq!(
        serde_json::from_str::<Value>(&built.to_line().unwrap()).unwrap(),
        serde_json::from_str::<Value>(v).unwrap()
    );
}

// ---- 叢集連線與排空（feature cluster） ----

#[test]
fn cluster_vectors_parse_into_the_shared_types() {
    use crate::{DRAIN_REASON_NODE_DRAINING, FEATURE_CLUSTER};
    let ClientMsg::Hello { ext, .. } =
        parse_client_msg(client_vector_in(Ver::Cluster, "hello")).unwrap()
    else {
        panic!("應解析為 hello");
    };
    assert!(
        ext.offer
            .as_ref()
            .unwrap()
            .features
            .as_ref()
            .unwrap()
            .iter()
            .any(|f| f == FEATURE_CLUSTER)
    );
    match parse_server_msg(server_vector_in(Ver::Cluster, "hello_ack")).unwrap() {
        ServerMsg::HelloAck {
            server_node,
            cluster_id,
            features: Some(f),
            ..
        } => {
            assert_eq!(server_node.as_deref(), Some("kairos-a"));
            assert_eq!(
                cluster_id.as_deref(),
                Some("3f6c2b1a-9d4e-4f8a-b7c6-1e2d3c4b5a69")
            );
            assert!(f.iter().any(|f| f == FEATURE_CLUSTER));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        parse_server_msg(server_vector_in(Ver::Cluster, "drain")).unwrap(),
        ServerMsg::drain(DRAIN_REASON_NODE_DRAINING, 10_000)
    );
    // 舊版的向量沒有新欄位，hello_ack 不帶 server_node
    for ver in [Ver::V1, Ver::V2, Ver::Durable, Ver::Renew, Ver::Deregister] {
        for line in vectors(ver, 'K') {
            for new_field in ["server_node", "cluster_id", "\"drain\""] {
                assert!(!line.contains(new_field), "{line}");
            }
        }
    }
}

#[test]
fn lenient_cluster_fields() {
    // hello_ack 的 server_node／cluster_id 型別或格式不符：視為沒有，整則仍可解析
    let long = format!("\"{}\"", "a".repeat(64));
    for (node, id) in [
        ("7", "\"x\""),
        ("\"Kairos A\"", "12"),
        ("\"-a\"", "null"),
        (long.as_str(), "\"3f6c2b1a\""),
    ] {
        let line = format!(
            r#"{{"type":"hello_ack","seq":1,"proto":2,"offline_after_ms":75000,"features":["cluster"],"server_node":{node},"cluster_id":{id}}}"#
        );
        match parse_server_msg(&line).unwrap() {
            ServerMsg::HelloAck {
                server_node: None,
                cluster_id: None,
                ..
            } => {}
            other => panic!("{line}: {other:?}"),
        }
    }
    // cluster_id 轉成小寫
    let line = r#"{"type":"hello_ack","seq":1,"proto":2,"offline_after_ms":75000,"cluster_id":"3F6C2B1A-9D4E-4F8A-B7C6-1E2D3C4B5A69"}"#;
    let ServerMsg::HelloAck { cluster_id, .. } = parse_server_msg(line).unwrap() else {
        panic!("應解析為 hello_ack");
    };
    assert_eq!(
        cluster_id.as_deref(),
        Some("3f6c2b1a-9d4e-4f8a-b7c6-1e2d3c4b5a69")
    );
    // drain 的欄位缺漏或型別不符：整則仍可解析，reconnect_within_ms 視為沒有
    for line in [
        r#"{"type":"drain"}"#,
        r#"{"type":"drain","seq":null,"reason":"node_draining","reconnect_within_ms":"10000"}"#,
        r#"{"type":"drain","reconnect_within_ms":-1}"#,
    ] {
        match parse_server_msg(line).unwrap() {
            ServerMsg::Drain {
                reconnect_within_ms: None,
                ..
            } => {}
            other => panic!("{line}: {other:?}"),
        }
    }
    // Kairos 送出的 drain：seq 一律為 null
    let v: Value =
        serde_json::from_str(&ServerMsg::drain("node_draining", 12_345).to_line().unwrap())
            .unwrap();
    assert_eq!(
        v,
        serde_json::json!({"type":"drain","seq":null,"reason":"node_draining","reconnect_within_ms":12345})
    );
}

#[test]
fn old_pulse_treats_drain_as_an_unknown_type() {
    // 0.8 的 Pulse 不認得 drain：parse_server_msg 依 0.8 的類型清單分類為「未知類型」（只記 DEBUG）
    const TYPES_0_8: [&str; 5] = [
        "hello_ack",
        "ack",
        "error",
        "renew_result",
        "deregister_ack",
    ];
    let drain = server_vector_in(Ver::Cluster, "drain");
    let kind = crate::message_type(drain).unwrap();
    assert!(!TYPES_0_8.contains(&kind.as_str()));
    assert!(crate::SERVER_MSG_TYPES.contains(&kind.as_str()));
}
