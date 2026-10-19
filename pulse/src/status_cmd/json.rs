//! `pulse status --json`（`cli-spec.md` §4.8；外層依 `cli-style.md` §7.2）。
//!
//! 輸出純 ASCII：非 ASCII 字元一律以 `\uXXXX` 跳脫（`cli-style.md` §7.1），在任何字碼頁下都不會變亂碼。

use serde::Serialize;
use serde_json::{Map, Value};

use super::eval::{FileState, Report};
use super::issues::{Issue, Severity};
use crate::status_file::{self as sf, StatusFile, ts};

pub const SCHEMA: &str = "pulse.status/v1";

#[derive(Debug, Serialize)]
struct Doc<'a> {
    schema: &'static str,
    ok: bool,
    generated_at: String,
    exit_code: u8,
    data: Option<Data<'a>>,
    warnings: Vec<Value>,
    error: Option<IssueJson>,
}

#[derive(Debug, Serialize)]
struct Data<'a> {
    agent_id: Option<&'a str>,
    hostname: &'a str,
    display_name: &'a str,
    instance: Option<&'a str>,
    version: &'a str,
    commit: &'a str,
    status: &'static str,
    config_file: Option<String>,
    service: Service<'a>,
    connection: Option<Connection<'a>>,
    reporting: Option<sf::Reporting>,
    identity: Option<Identity<'a>>,
    clock: Clock<'a>,
    status_file: StatusFileInfo<'a>,
    checks: Vec<Check<'a>>,
    issues: Vec<IssueJson>,
    /// 遠端管理（集中設定、遠端升級；REMOTE.md 第 10.3 節）：0.9.0 以前的狀態檔沒有時不輸出
    #[serde(skip_serializing_if = "Option::is_none")]
    management: Option<&'a sf::Management>,
}

#[derive(Debug, Serialize)]
struct Service<'a> {
    manager: Option<&'a str>,
    unit: Option<&'a str>,
    state: &'static str,
    pid: Option<u32>,
    started_at: Option<String>,
    restarts_1h: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Connection<'a> {
    state: &'a str,
    server: &'a str,
    tls_version: Option<&'a str>,
    mtls: bool,
    since: Option<&'a str>,
    consecutive_failures: u64,
    next_retry_at: Option<&'a str>,
    last_error_code: Option<&'a str>,
    last_registered_at: Option<&'a str>,
    last_rejection: Option<&'a sf::Rejection>,
    // 以下為工程補充的欄位（只新增，不影響 §4.8 的欄位）
    server_verified: bool,
    proto: Option<u32>,
    features: &'a [String],
    last_error: Option<&'a str>,
    /// 多個伺服器位址（`kairos/HA.md` 第 8.7 節）：連上的節點、候選位址的數量與索引、
    /// 所有位址都失敗時每個位址最後的錯誤
    server_node: Option<&'a str>,
    candidates: Option<u32>,
    candidate_index: Option<u32>,
    candidate_errors: &'a [sf::CandidateError],
}

#[derive(Debug, Serialize)]
struct Identity<'a> {
    state: String,
    enrollment: Option<Value>,
    /// 過期續簽（`state = "cert_expired"` 時）
    #[serde(skip_serializing_if = "Option::is_none")]
    grace: Option<Value>,
    cert_serial: Option<&'a str>,
    not_after: Option<&'a str>,
    remaining_ms: Option<i64>,
    renew_after: Option<&'a str>,
    /// `agent`：身分由 `pulse enroll` 管理，Kairos 支援時經 8443 自動更新；`none`：手動憑證
    rotation: &'static str,
    client_cert: Option<&'a str>,
    not_before: Option<&'a str>,
    agent_id_file: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct Clock<'a> {
    offset_ms: Option<i64>,
    /// 本機是否與時間伺服器同步：尚未偵測（`PLS-W8303` 待辦），一律為 `null`
    synced: Option<bool>,
    measured_at: Option<&'a str>,
    source: Option<&'a str>,
    valid: bool,
}

#[derive(Debug, Serialize)]
struct StatusFileInfo<'a> {
    path: &'a str,
    updated_at: Option<&'a str>,
    stale: bool,
    /// 距今多久沒有更新（單調時鐘；沒有狀態檔時為 `null`）
    age_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Check<'a> {
    id: &'static str,
    result: &'static str,
    code: Option<&'static str>,
    summary: &'a str,
}

#[derive(Debug, Serialize)]
pub struct IssueJson {
    code: String,
    severity: &'static str,
    title: String,
    cause: String,
    fix: Vec<String>,
    details: Map<String, Value>,
    explain: String,
}

impl From<&Issue> for IssueJson {
    fn from(i: &Issue) -> Self {
        Self {
            code: i.code.clone(),
            severity: i.severity.token(),
            title: i.title.clone(),
            cause: i.cause_text(),
            fix: i.fix_texts(),
            details: i.details.clone(),
            explain: format!("pulse explain {}", i.code),
        }
    }
}

/// 序列化並把非 ASCII 字元跳脫為 `\uXXXX`（JSON 中只有字串會含非 ASCII 字元，逐字元替換即可）
pub fn to_ascii_json<T: Serialize>(value: &T) -> String {
    let text = serde_json::to_string_pretty(value).expect("狀態 JSON 一定可以序列化");
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out.push('\n');
    out
}

/// `pulse status --json` 的完整文件
pub struct JsonInput<'a> {
    pub report: &'a Report,
    pub file: &'a FileState,
    pub status_path: &'a str,
    pub service_manager: Option<&'a str>,
    pub service_unit: Option<&'a str>,
    pub config_file: Option<String>,
    pub exit_code: u8,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

pub fn document(input: &JsonInput) -> String {
    let report = input.report;
    let file: Option<&StatusFile> = match input.file {
        FileState::Ok(f) => Some(f),
        _ => None,
    };
    let now = input.generated_at;
    let connection = file.map(|f| {
        let c = &f.connection;
        Connection {
            state: &c.state,
            server: &c.server,
            tls_version: c.tls_version.as_deref(),
            mtls: c.mtls,
            since: c.since.as_deref(),
            consecutive_failures: c.consecutive_failures,
            next_retry_at: c.next_retry_at.as_deref(),
            last_error_code: c.last_error_code.as_deref(),
            last_registered_at: c.last_registered_at.as_deref(),
            last_rejection: c.last_rejection.as_ref(),
            server_verified: c.server_verified,
            proto: c.proto,
            features: &c.features,
            last_error: c.last_error.as_deref(),
            server_node: c.server_node.as_deref(),
            candidates: c.candidates,
            candidate_index: c.candidate_index,
            candidate_errors: &c.candidate_errors,
        }
    });
    let reporting = file.map(|f| {
        let mut r = f.reporting.clone();
        if r.buffer.state == "ok" {
            r.buffer.replay_eta_ms = report.replay_eta.map(|d| d.as_millis() as u64);
            r.buffer.counters_since = Some(f.started_at.clone());
            r.buffer.replaying = Some(
                report.replay_eta.is_some()
                    || (f.connection.state == "registered" && r.buffer.pending.unwrap_or(0) > 0),
            );
        }
        r
    });
    let identity = file.map(|f| {
        let id = &f.identity;
        let revoked = f
            .connection
            .last_rejection
            .as_ref()
            .and_then(|r| r.code.as_deref())
            == Some(crate::codes::E_CLIENT_CERT_REJECTED);
        let not_after = id.not_after.as_deref().and_then(sf::parse_ts);
        let remaining = not_after.map(|t| (t - now).num_milliseconds().max(0));
        let managed = id.state.is_some();
        Identity {
            state: match &id.state {
                _ if revoked => "revoked".to_string(),
                Some(s) => s.clone(),
                None if remaining == Some(0) => "cert_expired".to_string(),
                None => "registered".to_string(),
            },
            enrollment: id
                .enrollment
                .as_ref()
                .and_then(|e| serde_json::to_value(e).ok()),
            grace: id.grace.as_ref().and_then(|g| serde_json::to_value(g).ok()),
            cert_serial: id.cert_serial.as_deref(),
            not_after: id.not_after.as_deref(),
            remaining_ms: remaining,
            renew_after: id.renew_after.as_deref(),
            rotation: if managed { "agent" } else { "none" },
            client_cert: id.client_cert.as_deref(),
            not_before: id.not_before.as_deref(),
            agent_id_file: id.agent_id_file.as_deref(),
        }
    });
    let clock = Clock {
        offset_ms: file.and_then(|f| f.clock.offset_ms),
        synced: None,
        measured_at: file.and_then(|f| f.clock.measured_at.as_deref()),
        source: file.and_then(|f| f.clock.source.as_deref()),
        valid: report.clock_valid && !file.is_some_and(|f| f.clock.unverified),
    };
    let checks = report
        .rows
        .iter()
        .map(|r| Check {
            id: r.id,
            result: r.result.token(),
            code: r.code,
            summary: &r.summary,
        })
        .collect();
    let data = Data {
        agent_id: report.agent_id.as_deref(),
        hostname: &report.hostname,
        display_name: file
            .and_then(|f| f.display_name.as_deref())
            .unwrap_or(&report.hostname),
        instance: None,
        version: &report.version,
        commit: &report.commit,
        status: report.status.token(),
        config_file: file
            .and_then(|f| f.config_file.clone())
            .or_else(|| input.config_file.clone()),
        service: Service {
            manager: input.service_manager,
            unit: input.service_unit,
            state: report.service_state,
            pid: report.service_pid,
            started_at: report.service_started_at.map(ts),
            restarts_1h: report.restarts_1h,
        },
        connection,
        reporting,
        identity,
        clock,
        status_file: StatusFileInfo {
            path: input.status_path,
            updated_at: file.map(|f| f.updated_at.as_str()),
            stale: report.stale || (!report.agent_running && file.is_some()),
            age_ms: report
                .file_age
                .filter(|a| *a != std::time::Duration::MAX)
                .map(|a| a.as_millis() as u64),
        },
        checks,
        issues: report.issues.iter().map(IssueJson::from).collect(),
        management: file.and_then(|f| f.management.as_ref()),
    };
    to_ascii_json(&Doc {
        schema: SCHEMA,
        ok: true,
        generated_at: ts(now),
        exit_code: input.exit_code,
        data: Some(data),
        warnings: Vec::new(),
        error: None,
    })
}

/// 命令本身無法完成（例如沒有權限讀取狀態檔）：`ok = false`、`data = null`
pub fn failure(issue: &Issue, exit_code: u8, now: chrono::DateTime<chrono::Utc>) -> String {
    debug_assert_eq!(issue.severity, Severity::Error);
    to_ascii_json(&Doc {
        schema: SCHEMA,
        ok: false,
        generated_at: ts(now),
        exit_code,
        data: None,
        warnings: Vec::new(),
        error: Some(IssueJson::from(issue)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_is_pure_ascii_and_round_trips() {
        #[derive(Serialize)]
        struct T {
            s: &'static str,
        }
        let out = to_ascii_json(&T {
            s: "線上 ✓ 😀 a\"b",
        });
        assert!(out.is_ascii(), "{out}");
        assert!(out.contains("\\u7dda\\u4e0a"), "{out}");
        // 補充平面的字元以 UTF-16 代理對跳脫
        assert!(out.contains("\\ud83d\\ude00"), "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["s"], "線上 ✓ 😀 a\"b");
        assert!(out.ends_with("}\n"));
    }
}
