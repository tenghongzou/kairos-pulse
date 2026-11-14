//! 依狀態檔、服務管理員與設定判定各檢查列與本機狀態（`pulse/cli-spec.md` §1.2、§4.2）。

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::fmt::{self, Zone};
use super::issues::{self, Ctx, Issue};
use super::service::{ServiceInfo, SvcState};
use crate::codes;
use crate::status_file::{self as sf, StatusFile, parse_ts};

/// 本機狀態（設計系統的狀態 token，`cli-spec.md` §1.2）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostStatus {
    // 宣告順序即嚴重度順序（critical → offline → warning → unknown → pending → online）
    Critical,
    Offline,
    Warning,
    Unknown,
    Pending,
    Online,
}

impl HostStatus {
    pub fn token(self) -> &'static str {
        match self {
            HostStatus::Critical => "critical",
            HostStatus::Offline => "offline",
            HostStatus::Warning => "warning",
            HostStatus::Unknown => "unknown",
            HostStatus::Pending => "pending",
            HostStatus::Online => "online",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HostStatus::Critical => "嚴重",
            HostStatus::Offline => "離線",
            HostStatus::Warning => "警告",
            HostStatus::Unknown => "未知",
            HostStatus::Pending => "待核准",
            HostStatus::Online => "線上",
        }
    }

    /// `pulse status` 的結束代碼（`cli-spec.md` §1.3）
    pub fn exit_code(self) -> u8 {
        match self {
            HostStatus::Online => 0,
            HostStatus::Pending => 7,
            HostStatus::Warning => 8,
            HostStatus::Critical | HostStatus::Offline | HostStatus::Unknown => 9,
        }
    }

    /// `--exit-codes=nagios`：0 OK、1 WARNING、2 CRITICAL、3 UNKNOWN（`cli-spec.md` §1.3）
    pub fn nagios_code(self) -> u8 {
        match self {
            HostStatus::Online => 0,
            HostStatus::Warning | HostStatus::Pending => 1,
            HostStatus::Critical | HostStatus::Offline => 2,
            HostStatus::Unknown => 3,
        }
    }
}

/// 單一檢查列的結果（`checks[].result`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Warning,
    Error,
    Offline,
    Pending,
    Skipped,
}

impl Outcome {
    pub fn token(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Warning => "warning",
            Outcome::Error => "error",
            Outcome::Offline => "offline",
            Outcome::Pending => "pending",
            Outcome::Skipped => "skipped",
        }
    }
}

/// 一個檢查列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// `service`、`connection`、`reporting`、`identity`、`config`、`clock`
    pub id: &'static str,
    /// 「服務」「連線」…
    pub name: &'static str,
    pub result: Outcome,
    pub code: Option<&'static str>,
    /// 第一行與之後的補充行
    pub lines: Vec<String>,
    /// `--json` 的 `checks[].summary`
    pub summary: String,
}

impl Row {
    fn new(id: &'static str, name: &'static str, result: Outcome, line: String) -> Self {
        Self {
            id,
            name,
            result,
            code: None,
            summary: line.clone(),
            lines: vec![line],
        }
    }

    fn code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    fn more(mut self, line: impl Into<String>) -> Self {
        self.lines.push(line.into());
        self
    }
}

/// 狀態檔的讀取結果
#[derive(Debug, Clone)]
pub enum FileState {
    Ok(Box<StatusFile>),
    Missing,
    /// 無法解析或無法讀取（不含權限不足，那是整個命令失敗）
    Invalid(String),
}

/// 設定（`pulse status` 自己讀到的）
#[derive(Debug, Clone, Default)]
pub struct ConfigView {
    /// 設定檔的絕對路徑（存在時）
    pub file: Option<String>,
    /// 設定檔不存在時，預期的位置
    pub expected: String,
    /// 設定檔有誤：代碼與訊息
    pub error: Option<(&'static str, String)>,
}

/// 判定所需的輸入
#[derive(Debug, Clone)]
pub struct Input {
    pub file: FileState,
    pub status_path: String,
    /// 服務管理員（沒有時為 `None`）
    pub service: Option<ServiceInfo>,
    /// 狀態檔記錄的 PID 是否還在（`None`：無法判斷）
    pub pid_alive: Option<bool>,
    pub config: ConfigView,
    pub now: DateTime<Utc>,
    pub now_mono: Option<u64>,
    pub zone: Zone,
    pub windows: bool,
    /// 狀態檔沒有時的主機名稱與 agent_id
    pub hostname: String,
    pub agent_id: Option<String>,
    pub version: String,
    pub commit: String,
}

/// 判定結果
#[derive(Debug, Clone)]
pub struct Report {
    pub status: HostStatus,
    pub version: String,
    pub commit: String,
    pub hostname: String,
    pub agent_id: Option<String>,
    /// 未啟用 mTLS，狀態檔刻意不含 agent_id（而這個帳號也讀不到 agent.id）
    pub agent_id_hidden: bool,
    pub rows: Vec<Row>,
    pub issues: Vec<Issue>,
    pub brief: String,
    /// 狀態檔距今多久沒有更新、是否過時
    pub file_age: Option<Duration>,
    pub stale: bool,
    pub agent_running: bool,
    /// `--json` 的 `service.state` 等
    pub service_state: &'static str,
    pub service_pid: Option<u32>,
    pub service_started_at: Option<DateTime<Utc>>,
    pub restarts_1h: Option<u64>,
    /// 連線中時的預計補完時間
    pub replay_eta: Option<Duration>,
    pub clock_valid: bool,
}

/// 補送積壓的預計補完時間（`cli-spec.md` §4.2）：待補送筆數 ÷ (1000 ÷ 補送間隔 − 1 ÷ report.interval)
pub fn replay_eta(
    pending: u64,
    replay_interval_ms: u64,
    report_interval_ms: u64,
) -> Option<Duration> {
    if pending == 0 || replay_interval_ms == 0 || report_interval_ms == 0 {
        return None;
    }
    let per_sec = 1000.0 / replay_interval_ms as f64 - 1000.0 / report_interval_ms as f64;
    if per_sec <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(pending as f64 / per_sec))
}

fn ts_opt(s: &Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().and_then(parse_ts)
}

fn since(now: DateTime<Utc>, t: DateTime<Utc>) -> Duration {
    (now - t).to_std().unwrap_or_default()
}

/// 時鐘偏差在 24 小時內量到、且量測後沒有重新開機時才用於到期判定（`enrollment-ux.md` 8.1）
fn clock_valid(file: &StatusFile, now: DateTime<Utc>, now_mono: Option<u64>) -> bool {
    let Some(measured) = ts_opt(&file.clock.measured_at) else {
        return false;
    };
    if file.clock.offset_ms.is_none() || since(now, measured) > Duration::from_secs(24 * 3600) {
        return false;
    }
    match (file.clock.measured_mono_ms, now_mono) {
        (Some(then), Some(now)) => then <= now,
        _ => true,
    }
}

/// 已註冊時的節點與候選位址：「（Kairos 節點 kairos-b；第 2 個位址，共 3 個）」（設計包
/// `decisions/INTEGRATION.md` §31.6）。只有節點名稱時「（Kairos 節點 kairos-b）」；沒有節點名稱且只有一個
/// 候選位址時為空字串（與 0.8 相同）
fn cluster_note(c: &crate::status_file::Connection) -> String {
    let mut parts = Vec::new();
    if let Some(node) = c.server_node.as_deref().filter(|n| !n.is_empty()) {
        parts.push(format!("Kairos 節點 {node}"));
    }
    if let (Some(total), Some(index)) = (c.candidates, c.candidate_index)
        && total > 1
    {
        parts.push(format!(
            "第 {} 個位址，共 {total} 個",
            index.saturating_add(1)
        ));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("（{}）", parts.join("；"))
    }
}

/// 連線列的簡短原因：「kairos…:8443 拒絕連線」
fn reason_phrase(code: Option<&str>, server: &str, host: &str, last_error: &str) -> String {
    match code {
        Some(codes::E_CONN_REFUSED) => format!("{server} 拒絕連線"),
        Some(codes::E_CONN_TIMEOUT) => format!("{server} 連線逾時"),
        Some(codes::E_DNS) => format!("找不到主機 {host}（DNS 解析失敗）"),
        Some(codes::E_SERVER_SILENT) => format!("{server} 沒有回應"),
        Some(codes::E_PER_IP_LIMIT) => format!("{server} 在加密連線建立前中斷"),
        Some(codes::W_SUPERSEDED) => "被同一 agent_id 的新連線取代".to_string(),
        Some(codes::E_ALL_CANDIDATES_FAILED) => "所有候選位址都無法連線".to_string(),
        Some(codes::I_DRAIN) => "Kairos 節點正在排空，改連其他節點".to_string(),
        _ if last_error.is_empty() => format!("無法連線到 {server}"),
        _ => last_error.to_string(),
    }
}

/// 需要人工處理的拒絕的簡短說明（連線列「無法註冊：…」）
fn rejection_phrase(code: Option<&str>, last_error: &str) -> (&'static str, String) {
    let p = |s: &str| ("無法註冊", s.to_string());
    let s = |s: &str| ("無法連線", s.to_string());
    match code {
        Some(codes::E_CLIENT_CERT_REJECTED) => p("這台主機的憑證已被撤銷"),
        Some(codes::E_SERVER_CRL) => p("Kairos 的撤銷清單已過期（伺服器端問題）"),
        Some(codes::E_CLIENT_CERT_MISMATCH) => p("用戶端憑證與 agent_id 不符"),
        Some(codes::E_CLIENT_CERT_MULTI_SAN) => p("用戶端憑證含多個 DNS SAN"),
        Some(codes::E_CLIENT_CERT_EXPIRED) => p("用戶端憑證已過期"),
        Some(codes::E_CLIENT_CERT_UNTRUSTED) => p("Kairos 不信任這台主機的憑證簽發者"),
        Some(codes::E_CLIENT_CERT_REQUIRED) => p("Kairos 要求用戶端憑證，但本機沒有設定"),
        Some(codes::E_PROTO_VERSION) => p("協定版本不相容"),
        Some(codes::E_AGENT_ID_CONFLICT) => p("多台主機使用同一個 agent_id"),
        Some(codes::E_REGISTRY_FULL) => p("Kairos 的主機狀態表已滿"),
        Some(codes::E_HELLO_REQUIRED | codes::E_LINE_TOO_LONG | codes::E_HELLO_BAD_REQUEST) => {
            p("Kairos 拒絕了註冊訊息")
        }
        Some(codes::E_SERVER_CERT_UNTRUSTED) => s("伺服器憑證不受信任"),
        Some(codes::E_SERVER_CERT_NAME) => s("伺服器憑證的名稱不符"),
        Some(codes::E_SERVER_CERT_VALIDITY) => s("伺服器憑證已過期或尚未生效"),
        Some(codes::E_SERVER_CERT_REVOKED) => s("伺服器憑證已被撤銷"),
        Some(codes::E_SERVER_CERT_PURPOSE) => s("伺服器憑證用途不符"),
        _ => ("無法註冊", last_error.to_string()),
    }
}

fn code_suffix(code: Option<&str>) -> String {
    code.map(|c| format!("（{c}）")).unwrap_or_default()
}

/// TLS 版本的顯示：`TLSv1.3` → `TLS 1.3`
fn tls_label(v: &str) -> String {
    v.strip_prefix("TLSv")
        .map_or_else(|| v.to_string(), |n| format!("TLS {n}"))
}

/// 主判定
pub fn evaluate(input: &Input) -> Report {
    let now = input.now;
    let file = match &input.file {
        FileState::Ok(f) => Some(f.as_ref()),
        _ => None,
    };
    let file_age = file.map(|f| sf::age(f, now, input.now_mono));
    let stale_by_age = file_age.is_some_and(|a| a > sf::STALE_AFTER);
    let hostname = file
        .and_then(|f| f.hostname.clone())
        .unwrap_or_else(|| input.hostname.clone());
    let agent_id = file
        .and_then(|f| f.agent_id.clone())
        .or_else(|| input.agent_id.clone());
    let (version, commit) = match file {
        Some(f) if !f.version.is_empty() => (f.version.clone(), f.commit.clone()),
        _ => (input.version.clone(), input.commit.clone()),
    };

    // ---- 服務：服務管理員優先；沒有服務管理員（或服務未安裝）時以狀態檔的 PID 判斷 ----
    let managed = input
        .service
        .as_ref()
        .filter(|s| s.state != SvcState::NotInstalled);
    let file_says_running = file.is_some_and(|f| f.running);
    let agent_running = match managed {
        Some(s) => s.state == SvcState::Running,
        None => file_says_running && input.pid_alive.unwrap_or(!stale_by_age),
    };
    let unit = input
        .service
        .as_ref()
        .map(|s| s.unit.clone())
        .or_else(|| file.and_then(|f| f.service.unit.clone()))
        .unwrap_or_else(|| super::service::default_unit().to_string());
    let ctx = Ctx::new(input, &unit, &hostname, agent_id.as_deref(), file);
    let svc_label = match (managed.map(|s| s.manager), input.windows) {
        (Some("scm"), _) if unit == "Pulse" => "Pulse 服務".to_string(),
        (Some("scm"), _) => format!("{unit} 服務"),
        (Some(_), _) => unit.clone(),
        (None, _) => "Pulse".to_string(),
    };
    // Windows 的服務標籤是中文（「Pulse 服務執行中」不加空格）；systemd unit 名稱後加空格
    let join = |label: &str, rest: &str| {
        if label.ends_with("服務") {
            format!("{label}{rest}")
        } else {
            format!("{label} {rest}")
        }
    };
    let started_at = file.and_then(|f| ts_opt(&Some(f.started_at.clone())));
    let restarts_1h = file.map(|f| {
        let hour_ago = now - chrono::Duration::hours(1);
        let starts = f
            .starts_1h
            .iter()
            .filter_map(|s| parse_ts(s))
            .filter(|t| *t > hour_ago)
            .count() as u64;
        starts.saturating_sub(1)
    });
    let pid = file
        .filter(|_| agent_running)
        .map(|f| f.pid)
        .or_else(|| managed.and_then(|s| s.main_pid));

    let mut rows = Vec::new();
    let mut issues: Vec<Issue> = Vec::new();
    let mut status_candidates: Vec<HostStatus> = Vec::new();
    let fresh = agent_running && file.is_some() && !stale_by_age;
    let stale = agent_running && !fresh;
    // 狀態檔過時（服務執行中）：依賴狀態檔的連線、回報、身分、時鐘列以最後已知內容照常判定，摘要加
    // 「（最後已知，N 分鐘前）」；本機狀態一律為「未知」（cli-spec.md §4.8、decisions/INTEGRATION.md §26.2 #8）
    let last_known_only = stale && file.is_some();
    let use_file = fresh || last_known_only;

    // 服務列
    let service_row = if agent_running {
        let mut detail = Vec::new();
        if let Some(pid) = pid {
            detail.push(format!("PID {pid}"));
        }
        if let (Some(t), true) = (started_at, fresh) {
            detail.push(format!("已執行 {}", fmt::duration(since(now, t))));
        }
        let unmanaged = if managed.is_none() {
            "；未由服務管理員管理"
        } else {
            ""
        };
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!("（{}{unmanaged}）", detail.join("，"))
        };
        let base = join(&svc_label, &format!("執行中{detail}"));
        if !fresh {
            let why = match (&input.file, file_age) {
                (FileState::Missing, _) => "但沒有狀態檔".to_string(),
                (FileState::Invalid(_), _) => "但狀態檔無法解析".to_string(),
                (_, Some(a)) if a == Duration::MAX => "但狀態檔是重新開機前寫的".to_string(),
                (_, Some(a)) => format!("但狀態檔已 {}未更新", fmt::duration(a)),
                _ => "但狀態檔過時".to_string(),
            };
            issues.push(issues::status_stale(
                &ctx,
                &svc_label,
                &input.file,
                file_age,
            ));
            status_candidates.push(HostStatus::Unknown);
            Row::new(
                "service",
                "服務",
                Outcome::Warning,
                format!("{base}，{why}（{}）", codes::W_STATUS_STALE),
            )
            .code(codes::W_STATUS_STALE)
            .summary(join(&svc_label, "執行中，狀態檔過時"))
        } else if restarts_1h.unwrap_or(0) >= 3 {
            let n = restarts_1h.unwrap_or(0);
            Row::new(
                "service",
                "服務",
                Outcome::Warning,
                format!("{base}，最近 1 小時重新啟動 {n} 次"),
            )
            .summary(join(
                &svc_label,
                &format!("執行中，最近 1 小時重新啟動 {n} 次"),
            ))
        } else {
            Row::new("service", "服務", Outcome::Ok, base).summary(join(&svc_label, "執行中"))
        }
    } else {
        status_candidates.push(HostStatus::Critical);
        let installed = managed.is_some() || file.is_some();
        if installed {
            let exit_code = managed
                .and_then(|s| s.exit_code)
                .or_else(|| file.and_then(|f| f.exit_code));
            let exit_at = managed
                .and_then(|s| s.exit_at)
                .or_else(|| file.and_then(|f| ts_opt(&f.stopped_at)));
            let mut last = Vec::new();
            if let Some(t) = exit_at {
                last.push(format!("上次結束：{}", fmt::date_time(t, input.zone)));
            }
            if let Some(c) = exit_code {
                last.push(format!("結束代碼 {c}"));
            }
            let last = if last.is_empty() {
                String::new()
            } else {
                format!("（{}）", last.join("，"))
            };
            let verb = match managed.map(|s| s.state) {
                Some(SvcState::Starting) => "正在重新啟動",
                _ if input.windows && managed.is_some() => "已停止",
                _ => "未執行",
            };
            issues.push(issues::service_not_running(
                &ctx,
                &svc_label,
                managed,
                file,
                input.config.error.as_ref(),
            ));
            Row::new(
                "service",
                "服務",
                Outcome::Error,
                join(&svc_label, &format!("{verb}{last}")),
            )
            .code(codes::E_SERVICE_NOT_RUNNING)
            .summary(join(&svc_label, verb))
        } else {
            issues.push(issues::service_not_installed(&ctx, input.service.as_ref()));
            Row::new(
                "service",
                "服務",
                Outcome::Error,
                format!("找不到 Pulse 服務與狀態檔（{}）", input.status_path),
            )
            .code(codes::E_SERVICE_NOT_INSTALLED)
            .summary("未安裝")
        }
    };
    rows.push(service_row);

    // ---- 連線、回報、身分、設定、時鐘 ----
    let last_known = file.map(|f| {
        fmt::time_only(
            ts_opt(&Some(f.updated_at.clone())).unwrap_or(now),
            input.zone,
        )
    });
    let skip_reason = if use_file {
        None
    } else if !agent_running && file.is_some() {
        Some(format!(
            "服務未執行，以下為最後已知狀態（{}）",
            last_known.clone().unwrap_or_default()
        ))
    } else if file.is_some() {
        Some(format!(
            "狀態檔過時，以下為最後已知狀態（{}）",
            last_known.clone().unwrap_or_default()
        ))
    } else {
        Some("沒有狀態檔，無法判斷".to_string())
    };

    let conn = file.map(|f| &f.connection);
    let connected = use_file && conn.is_some_and(|c| c.state == "registered");
    // 最後已知內容產生的問題說明不列出（可能已不是現況；過時本身由服務列的 PLS-W8004 說明）
    let issues_before = issues.len();
    let mut conn_error = false;
    let mut brief = BriefParts::default();
    match (&skip_reason, conn) {
        (Some(reason), _) => {
            rows.push(
                Row::new("connection", "連線", Outcome::Skipped, reason.clone()).summary(
                    if agent_running {
                        "狀態檔過時"
                    } else {
                        "服務未執行"
                    },
                ),
            );
        }
        (None, None) => unreachable!("fresh 時一定有狀態檔"),
        (None, Some(_)) if file.and_then(|f| enrollment_rows(f, input)).is_some() => {
            let (conn_row, _, state) = enrollment_rows(file.unwrap(), input).unwrap();
            if conn_row.result == Outcome::Error {
                conn_error = true;
            }
            brief.state = state;
            brief.conn_code = conn_row.code.map(str::to_string);
            rows.push(conn_row);
        }
        (None, Some(c)) => {
            let server = c.server.as_str();
            let host = server.rsplit_once(':').map_or(server, |(h, _)| h);
            let since_t = ts_opt(&c.since);
            let outage = since_t.map(|t| since(now, t)).unwrap_or_default();
            let last_error = c.last_error.clone().unwrap_or_default();
            match c.state.as_str() {
                "registered" => {
                    let mut tls = Vec::new();
                    if let Some(v) = &c.tls_version {
                        tls.push(tls_label(v));
                    }
                    if c.mtls {
                        tls.push("mTLS".to_string());
                    }
                    if !c.server_verified {
                        tls.push("未驗證伺服器憑證".to_string());
                    }
                    let tls = if tls.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", tls.join("，"))
                    };
                    let node = cluster_note(c);
                    rows.push(
                        Row::new(
                            "connection",
                            "連線",
                            Outcome::Ok,
                            format!(
                                "已註冊到 {server}{node}{tls}，已連線 {}",
                                fmt::duration(outage)
                            ),
                        )
                        .summary("已註冊"),
                    );
                    brief.state = "registered".into();
                }
                "rejected" => {
                    conn_error = true;
                    let code = c.last_error_code.as_deref();
                    let (verb, phrase) = rejection_phrase(code, &last_error);
                    let rejection = c.last_rejection.as_ref();
                    let from = rejection.and_then(|r| parse_ts(&r.since)).or(since_t);
                    let mut row = Row::new(
                        "connection",
                        "連線",
                        Outcome::Error,
                        format!("{verb}：{phrase}{}", code_suffix(code)),
                    )
                    .summary(format!("{verb}：{phrase}"));
                    row.code = code.and_then(static_code);
                    if let Some(t) = from {
                        row = row.more(format!(
                            "自 {} 起已重試 {} 次（服務會持續重試，但不會成功）",
                            fmt::date_time(t, input.zone),
                            c.consecutive_failures
                        ));
                    }
                    issues.extend(issues::connection(&ctx, c, true, outage));
                    rows.push(row);
                    brief.state = "rejected".into();
                    brief.conn_code = code.map(str::to_string);
                    brief.since = from.map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string());
                }
                "connecting" => {
                    rows.push(
                        Row::new(
                            "connection",
                            "連線",
                            Outcome::Offline,
                            format!("連線中：正在連線到 {server}（啟動後尚未註冊）"),
                        )
                        .summary("連線中"),
                    );
                    brief.state = "connecting".into();
                }
                _ => {
                    // reconnecting：一般網路錯誤。先前被拒（需要人工處理）的原因不被蓋掉（DEPLOY-TEST N7）
                    let code = c.last_error_code.as_deref();
                    let rejected_before = c.last_rejection.as_ref();
                    let long = outage >= crate::network::OUTAGE_ERROR_AFTER;
                    let result = if rejected_before.is_some() || long {
                        conn_error = true;
                        Outcome::Error
                    } else {
                        Outcome::Offline
                    };
                    let first = match rejected_before {
                        Some(r) => {
                            let (_, phrase) = rejection_phrase(r.code.as_deref(), &r.message);
                            format!(
                                "重新連線中（上次被拒：{} {phrase}）",
                                r.code.as_deref().unwrap_or("")
                            )
                            .replace("：  ", "：")
                        }
                        None => format!(
                            "重新連線中：{}{}",
                            reason_phrase(code, server, host, &last_error),
                            code_suffix(code)
                        ),
                    };
                    let next = match ts_opt(&c.next_retry_at) {
                        Some(t) if t > now => format!("下次重試 {}", fmt::later(since(t, now))),
                        _ => "即將重試".to_string(),
                    };
                    let mut row = Row::new("connection", "連線", result, first)
                        .summary("重新連線中")
                        .more(format!(
                            "已中斷 {}，連續失敗 {} 次，{next}",
                            fmt::duration(outage),
                            c.consecutive_failures
                        ));
                    row.code = rejected_before
                        .and_then(|r| r.code.as_deref())
                        .or(code)
                        .and_then(static_code);
                    if let Some(t) = ts_opt(&c.last_registered_at) {
                        row = row.more(format!("上次註冊成功：{}", fmt::date_time(t, input.zone)));
                    }
                    issues.extend(issues::connection(
                        &ctx,
                        c,
                        rejected_before.is_some() || long,
                        outage,
                    ));
                    rows.push(row);
                    brief.state = format!("reconnecting {}", fmt::compact(outage));
                    brief.conn_code = rejected_before
                        .and_then(|r| r.code.as_deref())
                        .or(code)
                        .map(str::to_string);
                }
            }
        }
    }
    let conn_row = rows.last().cloned().expect("剛加入連線列");
    if conn_row.result == Outcome::Offline {
        status_candidates.push(HostStatus::Offline);
    }

    // 回報
    let mut replay_eta_out = None;
    match (&skip_reason, file) {
        (Some(_), Some(f)) => {
            let last = ts_opt(&f.reporting.last_report_at)
                .map(|t| format!("最後一筆 {}", fmt::time_only(t, input.zone)))
                .unwrap_or_else(|| "沒有回報紀錄".to_string());
            rows.push(Row::new("reporting", "回報", Outcome::Skipped, last.clone()).summary(last));
        }
        (Some(reason), None) => {
            rows.push(Row::new(
                "reporting",
                "回報",
                Outcome::Skipped,
                reason.clone(),
            ));
        }
        (None, None) => unreachable!(),
        (None, Some(f)) if enrollment_rows(f, input).is_some() => {
            let why = if f.identity.grace.is_some() {
                "憑證過期期間不會連線，也不會回報"
            } else {
                "尚未註冊，不會回報"
            };
            rows.push(Row::new(
                "reporting",
                "回報",
                Outcome::Skipped,
                why.to_string(),
            ));
        }
        (None, Some(f)) => {
            let (row, eta) = reporting_row(f, input, connected, conn_error, &ctx, &mut issues);
            replay_eta_out = eta;
            brief.backlog = f.reporting.buffer.pending.filter(|&p| p > 0);
            brief.replay_eta = eta;
            let b = &f.reporting.buffer;
            let recent = |t: &Option<String>| {
                ts_opt(t).is_some_and(|t| since(now, t) < Duration::from_secs(24 * 3600))
            };
            if recent(&b.last_dropped_full_at) || recent(&b.last_dropped_expired_at) {
                brief.dropped = Some(b.dropped_full.unwrap_or(0) + b.dropped_expired.unwrap_or(0));
            }
            brief.at_most_once = f.reporting.delivery == "at_most_once";
            rows.push(row);
        }
    }

    // 身分
    let clock_ok = file.is_some_and(|f| clock_valid(f, now, input.now_mono));
    match file {
        None => rows.push(Row::new(
            "identity",
            "身分",
            Outcome::Skipped,
            "沒有狀態檔，無法判斷".to_string(),
        )),
        Some(f) => {
            let row = match enrollment_rows(f, input) {
                Some((_, id_row, _)) => id_row,
                None => identity_row(f, input, clock_ok, &ctx, &mut issues),
            };
            brief.cert_days = cert_days(f, now);
            rows.push(row);
        }
    }

    if last_known_only {
        issues.truncate(issues_before);
    }

    // 設定（照常：直接讀設定檔）
    rows.push(config_row(input, file, agent_running, &ctx, &mut issues));
    // 遠端管理（REMOTE.md 第 10.3 節）：「設定」列加上集中設定的狀態；有進行中的升級或最近 24 小時有結果時
    // 另有「升級」列（回滾失敗時為錯誤，結束代碼 9）
    if let Some(m) = file.and_then(|f| f.management.as_ref()) {
        if let (Some(c), Some(row)) = (&m.config, rows.last_mut()) {
            row.lines.push(management_config_line(c));
        }
        if let Some(u) = &m.upgrade
            && let Some(row) = upgrade_row(u, now)
        {
            rows.push(row);
        }
    }

    // 時鐘
    let issues_before = issues.len();
    rows.push(clock_row(file, use_file, connected, &ctx, &mut issues));
    if last_known_only {
        issues.truncate(issues_before);
        let ago = last_known_ago(file_age);
        for row in rows.iter_mut().filter(|r| {
            matches!(r.id, "connection" | "reporting" | "identity" | "clock")
                && r.result != Outcome::Skipped
        }) {
            row.summary.push_str(&ago);
            if let Some(first) = row.lines.first_mut() {
                first.push_str(&ago);
            }
        }
    }

    // ---- 本機狀態 ----
    for row in &rows {
        match row.result {
            Outcome::Error => status_candidates.push(HostStatus::Critical),
            Outcome::Warning if row.code != Some(codes::W_STATUS_STALE) => {
                status_candidates.push(HostStatus::Warning)
            }
            Outcome::Pending => status_candidates.push(HostStatus::Pending),
            _ => {}
        }
    }
    let status = if last_known_only {
        HostStatus::Unknown
    } else {
        status_candidates
            .into_iter()
            .min()
            .unwrap_or(HostStatus::Online)
    };

    // ---- --brief ----
    brief.codes = rows
        .iter()
        .filter(|r| r.result != Outcome::Ok)
        .filter_map(|r| r.code.map(str::to_string))
        .collect();
    // 憑證有效期限本身是問題時才在非線上狀態寫出 cert=（cli-spec §4.7 的範例）
    brief.cert_issue = rows.iter().any(|r| {
        r.id == "identity"
            && matches!(
                r.code,
                Some(
                    codes::W_CLIENT_CERT_EXPIRING
                        | codes::E_CLIENT_CERT_EXPIRED
                        | codes::E_CERT_NOT_YET_VALID
                )
            )
    });
    let brief = brief.line(
        status,
        &hostname,
        agent_running,
        stale,
        file_age,
        file.is_some() || managed.is_some(),
        conn.map(|c| c.server.as_str()),
    );

    Report {
        status,
        version,
        commit,
        hostname,
        agent_id_hidden: agent_id.is_none() && file.is_some_and(|f| f.agent_id_hidden),
        agent_id,
        rows,
        issues,
        brief,
        file_age,
        stale,
        agent_running,
        service_state: match managed {
            Some(s) => s.state.as_str(),
            None if agent_running => "running",
            None if file.is_some() => "stopped",
            None => "not_installed",
        },
        service_pid: pid,
        service_started_at: started_at.filter(|_| agent_running),
        restarts_1h,
        replay_eta: replay_eta_out,
        clock_valid: clock_ok,
    }
}

/// 註冊申請或過期續簽進行中時的「連線」「身分」列與 `--brief` 的狀態欄（設計包 `enrollment-ux.md` §4.2、
/// `cli-spec.md` §4.2、§4.6、§4.7）。不是由 `pulse enroll` 管理、或已註冊時為 `None`（照舊判定）
fn enrollment_rows(f: &StatusFile, input: &Input) -> Option<(Row, Row, String)> {
    let id = &f.identity;
    let now = input.now;
    let later = |t: &Option<String>| {
        ts_opt(t).filter(|t| *t > now).map(|t| {
            format!(
                "{}（約 {}）",
                fmt::later(since(t, now)),
                fmt::time_only(t, input.zone)
            )
        })
    };
    let code_of = |c: &Option<String>| c.as_deref().and_then(codes::known);
    if let Some(g) = &id.grace {
        let code = code_of(&g.last_code).unwrap_or(codes::E_CLIENT_CERT_EXPIRED);
        let expired = ts_opt(&id.not_after);
        let days = expired.map(|na| (now - na).num_days().max(0));
        let ends = ts_opt(&g.ends_at);
        let left = ends.map(|e| (e - now).num_days().max(0));
        let mut identity = match (expired, days) {
            (Some(na), Some(d)) => format!(
                "憑證已於 {} 過期（{d} 天前）",
                input.zone.local(na).format("%Y-%m-%d")
            ),
            _ => "憑證已過期".to_string(),
        };
        if g.renew_state == "grace_exceeded" {
            identity.push_str(&format!(
                "；已超過寬限期（{} 天）；需重新註冊（{}）",
                g.days.unwrap_or(90),
                codes::E_GRACE_EXCEEDED
            ));
        } else if let (Some(l), Some(e)) = (left, ends) {
            identity.push_str(&format!(
                "；過期寬限期剩 {l} 天（至 {}）",
                input.zone.local(e).format("%Y-%m-%d")
            ));
        }
        let next = later(&g.next_attempt_at)
            .map(|n| format!("下次嘗試 {n}"))
            .unwrap_or_default();
        let (conn_text, conn_code) = match g.renew_state.as_str() {
            "grace_blocked" => (
                "無法自動續簽：註冊端點無法連線".to_string(),
                codes::E_ENROLL_UNREACHABLE,
            ),
            "grace_pending" => (
                format!(
                    "憑證已過期，續簽申請等待核准（申請編號 {}）",
                    g.request_code.as_deref().unwrap_or("?")
                ),
                codes::I_PENDING_APPROVAL,
            ),
            "grace_exceeded" => (
                "無法自動續簽：已超過過期寬限期".to_string(),
                codes::E_GRACE_EXCEEDED,
            ),
            "grace_rejected" => (
                "Kairos 拒絕自動續簽：主機識別資訊與紀錄不符".to_string(),
                codes::E_GRACE_IDENTITY,
            ),
            "grace_clock_rejected" => (
                "無法自動續簽：Kairos 因時鐘偏差暫不受理".to_string(),
                codes::E_CLOCK_SKEW,
            ),
            _ => ("憑證已過期，正在自動續簽".to_string(), code),
        };
        let mut conn = Row::new(
            "connection",
            "連線",
            Outcome::Error,
            format!("{conn_text}（{conn_code}）"),
        )
        .code(conn_code)
        .summary(conn_text);
        let attempts = format!("已嘗試 {} 次", g.attempts);
        conn = conn.more(if next.is_empty() {
            attempts
        } else {
            format!("{attempts}，{next}")
        });
        let identity_row = Row::new("identity", "身分", Outcome::Error, identity.clone())
            .code(codes::E_CLIENT_CERT_EXPIRED)
            .summary(identity);
        let mut brief = format!(
            "cert_expired renew={}",
            g.renew_state.trim_start_matches("grace_")
        );
        if let Some(l) = left {
            brief = format!(
                "cert_expired grace={l}d renew={}",
                g.renew_state.trim_start_matches("grace_")
            );
        }
        if let Some(o) = g.offset_ms {
            brief.push_str(&format!(" offset={}", fmt_signed(o)));
        }
        return Some((conn, identity_row, brief));
    }
    let e = id.enrollment.as_ref()?;
    let rc = e.request_code.as_deref().unwrap_or("尚未建立");
    let code = code_of(&e.last_code);
    let (result, conn_text, id_text, brief) = match e.state.as_str() {
        "submitting" => {
            let busy = matches!(
                code,
                Some(codes::E_ENROLL_RATE_LIMITED | codes::E_PENDING_QUEUE_FULL)
            );
            let next = later(&e.next_submit_at)
                .map(|n| format!("，下次重試 {n}"))
                .unwrap_or_default();
            let t = if busy {
                format!("註冊端點忙碌{next}（{}）", code.unwrap_or(""))
            } else {
                format!("正在送出註冊申請{next}")
            };
            (
                Outcome::Pending,
                format!("尚未註冊：{t}"),
                t,
                "submitting".to_string(),
            )
        }
        "clock_rejected" => {
            let off = e.offset_ms.unwrap_or(0);
            let next = later(&e.next_submit_at)
                .map(|n| format!("（下次 {n}）"))
                .unwrap_or_default();
            let t = format!(
                "Kairos 因時鐘偏差暫不受理註冊：相差 {}；校正後自動重送{next}",
                fmt::offset_amount(off)
            );
            (
                Outcome::Error,
                "尚未註冊：Kairos 因時鐘偏差暫不受理註冊申請（PLS-E8302）".to_string(),
                t,
                format!("clock_rejected offset={}", fmt_signed(off)),
            )
        }
        "pending_approval" => {
            let until = ts_opt(&e.expires_at)
                .map(|t| format!("，有效至 {}", fmt::date_time(t, input.zone)))
                .unwrap_or_default();
            let t = format!("等待核准（申請編號 {rc}{until}）");
            (
                Outcome::Pending,
                format!("等待核准：申請編號 {rc}"),
                t,
                "pending_approval".to_string(),
            )
        }
        "issued" => (
            Outcome::Pending,
            "已核准，正在連線".to_string(),
            "已取得憑證，正在連線".to_string(),
            "issued".to_string(),
        ),
        "registered" => return None,
        other => {
            let c = code.unwrap_or(codes::E_NOT_ENROLLED);
            let title = codes::title(c).unwrap_or("尚未註冊");
            let t = match other {
                "rejected" => format!(
                    "申請被拒絕（原因：「{}」）",
                    e.rejected_reason.as_deref().unwrap_or("未填寫")
                ),
                _ => title.to_string(),
            };
            (
                Outcome::Error,
                format!("無法註冊：{title}"),
                t,
                other.to_string(),
            )
        }
    };
    let row_code = code.or(match e.state.as_str() {
        "pending_approval" => Some(codes::I_PENDING_APPROVAL),
        "clock_rejected" => Some(codes::E_CLOCK_SKEW),
        "issued" => Some(codes::I_ISSUED),
        _ => None,
    });
    let with_code = |t: String| match row_code {
        Some(c) if !t.contains(c) => format!("{t}（{c}）"),
        _ => t,
    };
    let mut conn =
        Row::new("connection", "連線", result, with_code(conn_text.clone())).summary(conn_text);
    let mut idr = Row::new("identity", "身分", result, with_code(id_text.clone())).summary(id_text);
    if let Some(c) = row_code {
        conn = conn.code(c);
        idr = idr.code(c);
    }
    if let Some(ip) = &e.observed_ip
        && result != Outcome::Ok
    {
        conn = conn.more(format!("Kairos 看到的來源位址：{ip}"));
    }
    Some((conn, idr, brief))
}

/// `--brief` 的帶正負號偏差（`-7m42s`）
fn fmt_signed(ms: i64) -> String {
    let sign = if ms < 0 { "-" } else { "+" };
    format!(
        "{sign}{}",
        fmt::compact(Duration::from_millis(ms.unsigned_abs()))
    )
}

/// 狀態檔中的代碼字串轉成程式中的常數（只接受認得的代碼；未知的代碼不放進 `checks[].code`）
fn static_code(code: &str) -> Option<&'static str> {
    const KNOWN: &[&str] = &[
        codes::E_CLIENT_CERT_REQUIRED,
        codes::E_CLIENT_CERT_MISMATCH,
        codes::E_CLIENT_CERT_MULTI_SAN,
        codes::E_CLIENT_CERT_REJECTED,
        codes::E_CLIENT_CERT_EXPIRED,
        codes::E_CLIENT_CERT_UNTRUSTED,
        codes::E_SERVER_CRL,
        codes::E_SERVER_CERT_UNTRUSTED,
        codes::E_SERVER_CERT_NAME,
        codes::E_SERVER_CERT_VALIDITY,
        codes::E_SERVER_CERT_REVOKED,
        codes::E_SERVER_CERT_PURPOSE,
        codes::E_DNS,
        codes::E_CONN_REFUSED,
        codes::E_CONN_TIMEOUT,
        codes::E_SERVER_SILENT,
        codes::E_PROTO_VERSION,
        codes::E_HELLO_REQUIRED,
        codes::E_LINE_TOO_LONG,
        codes::E_HELLO_BAD_REQUEST,
        codes::E_PER_IP_LIMIT,
        codes::W_SUPERSEDED,
        codes::E_AGENT_ID_CONFLICT,
        codes::E_REGISTRY_FULL,
        codes::W_SERVER_STORAGE,
        codes::W_REPORT_RATE,
        codes::W_SERVER_UNEXPECTED,
    ];
    KNOWN.iter().copied().find(|k| *k == code)
}

/// 用戶端憑證剩餘天數（`--brief` 的 `cert=`）
fn cert_days(f: &StatusFile, now: DateTime<Utc>) -> Option<i64> {
    let not_after = ts_opt(&f.identity.not_after)?;
    Some(days_left(not_after, now))
}

/// 剩餘天數，無條件進位（與啟動時的憑證檢查相同：剩 8.9 天寫 9 天），已過期為 0
fn days_left(not_after: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    let secs = (not_after - now).num_seconds().max(0);
    (secs + 86_399) / 86_400
}

/// `--brief` 的組成（`cli-spec.md` §4.7）
#[derive(Debug, Default)]
struct BriefParts {
    /// `registered`、`reconnecting 12m30s`、`rejected`、`connecting`
    state: String,
    conn_code: Option<String>,
    since: Option<String>,
    cert_days: Option<i64>,
    cert_issue: bool,
    backlog: Option<u64>,
    replay_eta: Option<Duration>,
    dropped: Option<u64>,
    at_most_once: bool,
    /// 各列（非通過）的代碼，依列的順序
    codes: Vec<String>,
}

impl BriefParts {
    /// 單行：第一欄為 `data.status`（補到 9 欄寬），之後以空白分隔
    #[allow(clippy::too_many_arguments)]
    fn line(
        self,
        status: HostStatus,
        hostname: &str,
        agent_running: bool,
        stale: bool,
        file_age: Option<Duration>,
        installed: bool,
        server: Option<&str>,
    ) -> String {
        let mut parts: Vec<String> = vec![hostname.to_string()];
        let mut codes = self.codes;
        if !agent_running {
            parts.push(
                if installed {
                    "stopped"
                } else {
                    "not_installed"
                }
                .to_string(),
            );
        } else if stale {
            parts.push(match file_age {
                Some(a) if a != Duration::MAX => format!("stale {}", fmt::compact(a)),
                _ => "stale".to_string(),
            });
        } else {
            parts.push(self.state.clone());
            if let Some(code) = &self.conn_code {
                parts.push(code.clone());
                codes.retain(|c| c != code);
            }
            if let Some(since) = self.since {
                parts.push(format!("since={since}"));
            }
            if status == HostStatus::Online
                && self.state == "registered"
                && let Some(server) = server
            {
                parts.push(server.to_string());
            }
            if let Some(days) = self.cert_days
                && (status == HostStatus::Online || self.cert_issue)
            {
                parts.push(format!("cert={days}d"));
            }
            if let Some(n) = self.backlog {
                parts.push(format!("backlog={n}"));
            }
            if let Some(eta) = self.replay_eta {
                parts.push(format!(
                    "replay_eta={}",
                    fmt::compact(fmt::round_minutes(eta))
                ));
            }
            if let Some(n) = self.dropped {
                parts.push(format!("dropped={n}"));
            }
            if self.at_most_once {
                parts.push("delivery=at_most_once".to_string());
            }
        }
        let mut seen: Vec<String> = Vec::new();
        for c in codes {
            if !seen.contains(&c) {
                seen.push(c);
            }
        }
        parts.extend(seen);
        format!("{:<9} {}", status.token(), parts.join(" "))
    }
}

/// 「回報」列（`cli-spec.md` §4.2 與「回報」列的補送資訊）；回傳列與預計補完時間
fn reporting_row(
    f: &StatusFile,
    input: &Input,
    connected: bool,
    conn_error: bool,
    ctx: &Ctx,
    issues: &mut Vec<Issue>,
) -> (Row, Option<Duration>) {
    let now = input.now;
    let r = &f.reporting;
    let b = &r.buffer;
    let pending = b.pending.unwrap_or(0);
    let last = ts_opt(&r.last_report_at);
    let mut first = match last {
        Some(t) => format!("最後回報 {}", fmt::ago(since(now, t))),
        None => "尚未回報".to_string(),
    };
    let summary_first = first.clone();
    let interval = Duration::from_millis(f.report_interval_ms.max(1000));
    let mut result = Outcome::Ok;
    let mut code: Option<&'static str> = None;
    let mut warn = |c: Option<&'static str>, result: &mut Outcome| {
        if *result == Outcome::Ok {
            *result = Outcome::Warning;
        }
        if code.is_none() {
            code = c;
        }
    };
    let durable = r.delivery == "durable";
    let mut more: Vec<String> = Vec::new();
    let mut eta = None;

    // 基本判定：連線中看最後回報的時間；中斷期間為警告（需要人工處理的錯誤時 5 分鐘以上為錯誤）。
    // 以狀態檔寫入的時間為準（狀態檔每 10 秒才寫一次，取樣間隔可能更短；狀態檔本身是否過時另外判斷）
    let written = parse_ts(&f.updated_at).unwrap_or(now);
    let reference = last.or_else(|| ts_opt(&f.connection.since));
    let age = reference.map(|t| since(written, t)).unwrap_or_default();
    if connected {
        if age > Duration::from_secs(300) {
            result = Outcome::Error;
        } else if age > interval * 3 {
            warn(None, &mut result);
        }
        if pending == 0 && r.window_1h.sampled > 0 {
            first = format!(
                "{first}；最近 1 小時 {}／{} 筆已送達",
                fmt::thousands(r.window_1h.acked.min(r.window_1h.sampled)),
                fmt::thousands(r.window_1h.sampled)
            );
        }
    } else if conn_error && age > Duration::from_secs(300) {
        result = Outcome::Error;
    } else {
        warn(None, &mut result);
    }

    let limits = format!(
        "上限 {}／{} 筆，最多保留 {}",
        fmt::mib(b.max_bytes.unwrap_or(0)),
        fmt::thousands(b.max_records.unwrap_or(0)),
        fmt::hours(b.max_age_hours.unwrap_or(0))
    );
    let oldest = ts_opt(&b.oldest_sample_at);
    let oldest_text = |with_ago: bool| match oldest {
        Some(t) if with_ago => format!(
            "（最舊 {}，{}）",
            fmt::moment(t, now, input.zone),
            fmt::ago(since(now, t))
        ),
        Some(t) => format!("（最舊 {}）", fmt::moment(t, now, input.zone)),
        None => String::new(),
    };
    match (b.state.as_str(), r.delivery_reason.as_deref()) {
        ("ok", _) if durable && pending > 0 && connected => {
            let retry = ts_opt(&b.retry_at).filter(|t| *t > now);
            if let Some(at) = retry {
                let rc = b
                    .retry_code
                    .as_deref()
                    .and_then(static_code)
                    .unwrap_or(codes::W_SERVER_STORAGE);
                warn(Some(rc), &mut result);
                more.push(format!(
                    "Kairos 暫時無法保存（{rc}）：待補送 {} 筆{}，{}重送",
                    fmt::thousands(pending),
                    oldest_text(false),
                    fmt::later(since(at, now))
                ));
                issues.push(issues::server_storage(ctx, rc, pending, oldest));
            } else {
                eta = replay_eta(pending, r.replay_interval_ms, f.report_interval_ms);
                match eta {
                    Some(e) => {
                        more.push(format!(
                            "補送中：待補送 {} 筆{}，",
                            fmt::thousands(pending),
                            oldest_text(true)
                        ));
                        more.push(format!("預計{}補完", fmt::eta(e)));
                    }
                    None => more.push(format!(
                        "補送中：待補送 {} 筆{}",
                        fmt::thousands(pending),
                        oldest_text(true)
                    )),
                }
            }
        }
        ("ok", _) if durable && pending > 0 => {
            let usage = b.usage_ratio.unwrap_or(0.0);
            let near = if usage >= 0.8 {
                "；接近上限，超過時會丟掉最舊的資料"
            } else {
                ""
            };
            more.push(format!(
                "待補送 {} 筆{}，恢復連線後自動補送；",
                fmt::thousands(pending),
                oldest_text(true)
            ));
            more.push(format!(
                "本機緩衝已用 {}（{limits}）{near}",
                fmt::percent(usage)
            ));
        }
        (_, Some("server")) => {
            if pending > 0 {
                more.push(format!(
                    "連上的 Kairos 不支援補送（0.2 以前）：連線期間即時送出，中斷期間的資料要等連上 0.3 以上的 Kairos 才補送\
                     （目前保留 {} 筆，最多 {}）",
                    fmt::thousands(pending),
                    fmt::hours(b.max_age_hours.unwrap_or(0))
                ));
            } else {
                more.push("連上的 Kairos 不支援補送（0.2 以前）".to_string());
            }
        }
        (_, Some("disabled")) | ("disabled", _) => {
            more.push("不補送（buffer.enabled = false）：中斷期間的資料不會保留".to_string());
        }
        ("unavailable", _) | (_, Some("unavailable")) => {
            warn(Some(codes::E_BUFFER_UNAVAILABLE), &mut result);
            more.push(format!(
                "本機緩衝無法使用（{}）：本次執行中斷期間的資料不會補送",
                codes::E_BUFFER_UNAVAILABLE
            ));
            issues.push(issues::buffer_unavailable(ctx, b));
        }
        _ => {}
    }
    // 最近 24 小時內曾丟棄資料
    let recent =
        |t: &Option<String>, within: Duration| ts_opt(t).is_some_and(|t| since(now, t) < within);
    let day = Duration::from_secs(24 * 3600);
    if recent(&b.last_dropped_full_at, day) {
        warn(Some(codes::W_BUFFER_FULL), &mut result);
        let range = match (ts_opt(&b.dropped_full_from), ts_opt(&b.dropped_full_until)) {
            (Some(a), Some(z)) => format!("{} 的取樣，", fmt::sample_range(a, z, input.zone)),
            _ => String::new(),
        };
        more.push(format!(
            "本機緩衝曾滿：已丟掉最舊的 {} 筆（{range}{}）",
            fmt::thousands(b.dropped_full.unwrap_or(0)),
            codes::W_BUFFER_FULL
        ));
        issues.push(issues::buffer_full(ctx, f));
    }
    if recent(&b.last_dropped_expired_at, day) {
        warn(Some(codes::W_BUFFER_EXPIRED), &mut result);
        more.push(format!(
            "超過保存時間（{}）而丟掉 {} 筆（{}）",
            fmt::hours(b.max_age_hours.unwrap_or(0)),
            fmt::thousands(b.dropped_expired.unwrap_or(0)),
            codes::W_BUFFER_EXPIRED
        ));
        issues.push(issues::buffer_expired(ctx, f));
    }
    if recent(&b.last_write_failure_at, Duration::from_secs(3600)) {
        warn(Some(codes::W_BUFFER_WRITE), &mut result);
        more.push(format!(
            "無法寫入本機緩衝：{} 筆取樣遺失（{}）",
            fmt::thousands(b.write_failures.unwrap_or(0)),
            codes::W_BUFFER_WRITE
        ));
        issues.push(issues::buffer_write_failed(ctx, b));
    }
    if connected && r.window_1h.rejected_by_server > 0 {
        warn(None, &mut result);
        more.push(format!(
            "最近 1 小時有 {} 筆回報被 Kairos 拒收或無法保存",
            fmt::thousands(r.window_1h.rejected_by_server)
        ));
    }

    let mut summary = summary_first;
    if let Some(m) = more.first() {
        summary = m.trim_end_matches(['，', '；']).to_string();
    }
    let mut row = Row::new("reporting", "回報", result, first).summary(summary);
    row.code = code;
    row.lines.extend(more);
    (row, eta)
}

/// 「身分」列：用戶端憑證的有效期限與身分檔案的權限
fn identity_row(
    f: &StatusFile,
    input: &Input,
    clock_ok: bool,
    ctx: &Ctx,
    issues: &mut Vec<Issue>,
) -> Row {
    let now = input.now;
    let id = &f.identity;
    let rejection = f
        .connection
        .last_rejection
        .as_ref()
        .and_then(|r| r.code.as_deref());
    let serial = id
        .cert_serial
        .as_deref()
        .map(|s| format!("（序號 {s}）"))
        .unwrap_or_default();
    let corrected = match f.clock.offset_ms.filter(|_| clock_ok) {
        Some(ms) => now - chrono::Duration::milliseconds(ms),
        None => now,
    };
    let date = |t: DateTime<Utc>| input.zone.local(t).format("%Y-%m-%d").to_string();
    let mut row = if rejection == Some(codes::E_CLIENT_CERT_REJECTED) {
        Row::new(
            "identity",
            "身分",
            Outcome::Error,
            format!("已撤銷{serial}"),
        )
        .code(codes::E_CLIENT_CERT_REJECTED)
        .summary("憑證已撤銷")
    } else if id.client_cert.is_none() {
        Row::new(
            "identity",
            "身分",
            Outcome::Ok,
            "未使用用戶端憑證（mTLS 未啟用），以 agent_id 識別".to_string(),
        )
        .summary("未使用用戶端憑證")
    } else {
        match (ts_opt(&id.not_before), ts_opt(&id.not_after)) {
            (_, Some(na)) if corrected > na => {
                let days = (corrected - na).num_days();
                if clock_ok || rejection == Some(codes::E_CLIENT_CERT_EXPIRED) {
                    issues.push(issues::cert_expired(ctx, f, na, true));
                    Row::new(
                        "identity",
                        "身分",
                        Outcome::Error,
                        format!(
                            "憑證已於 {} 過期（{days} 天前）（{}）",
                            date(na),
                            codes::E_CLIENT_CERT_EXPIRED
                        ),
                    )
                    .code(codes::E_CLIENT_CERT_EXPIRED)
                    .summary(format!("憑證已過期 {days} 天"))
                } else {
                    issues.push(issues::cert_expired(ctx, f, na, false));
                    Row::new(
                        "identity",
                        "身分",
                        Outcome::Warning,
                        format!(
                            "依本機時間，憑證已於 {} 過期；尚未經 Kairos 確認（本機時鐘可能不準），正在連線確認",
                            date(na)
                        ),
                    )
                    .code(codes::E_CLIENT_CERT_EXPIRED)
                    .summary("依本機時間憑證已過期，尚未經 Kairos 確認")
                }
            }
            (Some(nb), _) if corrected < nb => {
                issues.push(issues::cert_not_yet_valid(ctx, f, nb));
                Row::new(
                    "identity",
                    "身分",
                    Outcome::Warning,
                    format!(
                        "憑證要到 {} 才生效（{}）",
                        fmt::date_time(nb, input.zone),
                        codes::E_CERT_NOT_YET_VALID
                    ),
                )
                .code(codes::E_CERT_NOT_YET_VALID)
                .summary("憑證尚未生效")
            }
            (_, Some(na)) => {
                let days = days_left(na, corrected);
                if days <= 14 {
                    issues.push(issues::cert_expiring(ctx, f, na, days));
                    Row::new(
                        "identity",
                        "身分",
                        Outcome::Warning,
                        format!(
                            "憑證剩餘 {days} 天，不會自動更新（{}）",
                            codes::W_CLIENT_CERT_EXPIRING
                        ),
                    )
                    .code(codes::W_CLIENT_CERT_EXPIRING)
                    .summary(format!("憑證剩餘 {days} 天"))
                } else {
                    Row::new(
                        "identity",
                        "身分",
                        Outcome::Ok,
                        format!("憑證有效至 {}（剩餘 {days} 天）", date(na)),
                    )
                    .summary(format!("憑證有效，剩餘 {days} 天"))
                }
            }
            _ => Row::new(
                "identity",
                "身分",
                Outcome::Ok,
                "無法解析用戶端憑證的有效期限（由 Kairos 判定）".to_string(),
            ),
        }
    };
    // 權限過寬（PLS-W8201、PLS-W8202）
    for w in &id.warnings {
        let Some(code) = [codes::W_AGENT_ID_PERMS, codes::W_KEY_PERMS]
            .into_iter()
            .find(|c| *c == w.code)
        else {
            continue;
        };
        let what = if code == codes::W_AGENT_ID_PERMS {
            "agent.id 權限過寬"
        } else {
            "私鑰檔案權限過寬"
        };
        row.lines[0] = format!("{}；{what}（{code}）", row.lines[0]);
        if row.result == Outcome::Ok {
            row.result = Outcome::Warning;
            row.code = Some(code);
            row.summary = what.to_string();
        }
        issues.push(issues::permissions(ctx, code, &w.message));
    }
    row
}

/// 「設定」列：`pulse status` 讀到的設定檔是否有誤，以及執行中的 Pulse 回報的設定警告
/// 「設定」列的集中設定狀態
fn management_config_line(c: &crate::remote::config::ConfigView) -> String {
    use kairos_proto::mgmt::{CONFIG_STATE_REJECTED, CONFIG_STATE_ROLLED_BACK};
    let mut line = if c.local == kairos_proto::mgmt::INSTALL_CONFIG_DISABLED {
        format!("集中設定：本機停用（{}）", codes::I_REMOTE_CONFIG_DISABLED)
    } else if c.state == CONFIG_STATE_REJECTED {
        format!(
            "集中設定：第 {} 版被拒（{}）",
            c.rev,
            c.code.as_deref().unwrap_or(codes::W_REMOTE_CONFIG_REJECTED)
        )
    } else if c.state == CONFIG_STATE_ROLLED_BACK {
        format!(
            "集中設定：第 {} 版已回復上一版（{}）",
            c.rev,
            codes::W_REMOTE_CONFIG_ROLLED_BACK
        )
    } else if c.usage == "in_use" {
        if c.overridden.is_empty() {
            format!("集中設定：已同步（第 {} 版）", c.rev)
        } else {
            format!(
                "集中設定：已同步（第 {} 版，{} 個鍵被本機覆蓋）",
                c.rev,
                c.overridden.len()
            )
        }
    } else {
        "集中設定：未使用（沒有下發的設定，或 Kairos 不支援）".to_string()
    };
    if c.cache_error.is_some() {
        line.push_str(&format!(
            "；快取無法使用（{}）",
            codes::E_REMOTE_CONFIG_CACHE
        ));
    }
    line
}

/// 「升級」列：有進行中的升級，或最近 24 小時有結果時（回滾失敗一律顯示）
fn upgrade_row(u: &crate::remote::upgrade::UpgradeView, now: DateTime<Utc>) -> Option<Row> {
    use kairos_proto::mgmt::{
        UPGRADE_STATE_FAILED, UPGRADE_STATE_ROLLBACK_FAILED, UPGRADE_STATE_ROLLED_BACK,
        UPGRADE_STATE_SUCCEEDED,
    };
    if let Some(phase) = &u.phase {
        let to = u.to_version.as_deref().unwrap_or("?");
        let line = match phase.as_str() {
            "downloading" => {
                let pct = match (u.bytes, u.total) {
                    (Some(b), Some(t)) if t > 0 => b.saturating_mul(100) / t,
                    _ => 0,
                };
                format!("下載中 {pct}%（{to}）")
            }
            "staged" => format!(
                "等待升級時段 {}（{to}）",
                u.not_before.as_deref().unwrap_or("?")
            ),
            _ => format!("替換中（{to}）"),
        };
        return Some(Row::new("upgrade", "升級", Outcome::Ok, line.clone()).summary(line));
    }
    let last = u.last.as_ref()?;
    let recent = sf::parse_ts(&last.at).is_some_and(|t| now - t <= chrono::TimeDelta::hours(24));
    let (result, line) = match last.state.as_str() {
        UPGRADE_STATE_ROLLBACK_FAILED => (
            Outcome::Error,
            format!(
                "回滾失敗，需要人工處理（{}）",
                codes::E_UPGRADE_ROLLBACK_FAILED
            ),
        ),
        // 降級回滾（REMOTE.md 第 4.5.2 節）：服務已恢復，但套件資料庫與實際的執行檔不一致——在修復
        // （重新安裝目前版本的套件）或下一次升級之前一律顯示，`pulse status` 以警告的結束代碼表示需要處理
        UPGRADE_STATE_ROLLED_BACK if u.package_repair_needed => (
            Outcome::Warning,
            format!(
                "已回滾到 {}，但只換回執行檔：請重新安裝 Pulse {} 的套件（{}）",
                last.from_version,
                last.from_version,
                codes::W_UPGRADE_ROLLED_BACK_DEGRADED
            ),
        ),
        _ if !recent => return None,
        UPGRADE_STATE_SUCCEEDED => (
            Outcome::Ok,
            format!(
                "已升級到 {}（{}）",
                last.to_version,
                codes::I_UPGRADE_COMPLETED
            ),
        ),
        UPGRADE_STATE_ROLLED_BACK => (
            Outcome::Warning,
            format!(
                "已回滾到 {}（{}）",
                last.from_version,
                codes::W_UPGRADE_ROLLED_BACK
            ),
        ),
        UPGRADE_STATE_FAILED => (
            Outcome::Warning,
            format!(
                "升級到 {} 失敗（{}）",
                last.to_version,
                last.code.as_deref().unwrap_or("-")
            ),
        ),
        // declined、expired：不需要處理，不顯示
        _ => return None,
    };
    let code = match result {
        Outcome::Error => Some(codes::E_UPGRADE_ROLLBACK_FAILED),
        Outcome::Warning if last.state == UPGRADE_STATE_ROLLED_BACK => {
            Some(if u.package_repair_needed {
                codes::W_UPGRADE_ROLLED_BACK_DEGRADED
            } else {
                codes::W_UPGRADE_ROLLED_BACK
            })
        }
        _ => None,
    };
    let mut row = Row::new("upgrade", "升級", result, line.clone()).summary(line);
    if let Some(c) = code {
        row = row.code(c);
    }
    Some(row)
}

fn config_row(
    input: &Input,
    file: Option<&StatusFile>,
    agent_running: bool,
    ctx: &Ctx,
    issues: &mut Vec<Issue>,
) -> Row {
    let path = file
        .and_then(|f| f.config_file.clone())
        .or_else(|| input.config.file.clone());
    let shown = path
        .clone()
        .unwrap_or_else(|| format!("內建預設值（{} 不存在）", input.config.expected));
    if let Some((code, message)) = &input.config.error {
        let first = match config_error_line(message) {
            Some(line) if *code == codes::E_CONFIG_SYNTAX => format!(
                "{} 第 {line} 行語法錯誤（{code}）",
                input.config.file.clone().unwrap_or_else(|| shown.clone())
            ),
            _ => format!(
                "{}：{}（{code}）",
                input.config.file.clone().unwrap_or_else(|| shown.clone()),
                codes::title(code).unwrap_or("設定有誤")
            ),
        };
        issues.push(issues::config_error(ctx, code, message, agent_running));
        return Row::new("config", "設定", Outcome::Error, first)
            .code(code)
            .summary(codes::title(code).unwrap_or("設定有誤"));
    }
    let warnings: Vec<&sf::CodeNote> = file
        .map(|f| f.config.warnings.iter().collect())
        .unwrap_or_default();
    if warnings.is_empty() {
        return Row::new("config", "設定", Outcome::Ok, format!("{shown}，無警告"))
            .summary("無警告");
    }
    for w in &warnings {
        issues.push(issues::config_warning(ctx, &w.code, &w.message));
    }
    let codes_list: Vec<&str> = warnings.iter().map(|w| w.code.as_str()).collect();
    let first_code = [
        codes::E_STATE_DIR_UNSAFE,
        codes::E_STATUS_UNWRITABLE,
        codes::W_TLS_VERIFY_DISABLED,
        codes::W_BOTH_TRUST_STORES,
        codes::W_SYSTEM32_WORKDIR,
    ]
    .into_iter()
    .find(|c| codes_list.contains(c));
    let line = if warnings.len() == 1 {
        format!(
            "{shown}：{}（{}）",
            codes::title(&warnings[0].code).unwrap_or("設定警告"),
            warnings[0].code
        )
    } else {
        format!(
            "{shown}，{} 個警告（{}）",
            warnings.len(),
            codes_list.join("、")
        )
    };
    let mut row = Row::new("config", "設定", Outcome::Warning, line)
        .summary(format!("{} 個警告", warnings.len()));
    row.code = first_code;
    row
}

/// 設定檔語法錯誤訊息中的行號（toml：「at line 4, column 1」）
fn config_error_line(message: &str) -> Option<u32> {
    let i = message.find("line ")?;
    let digits: String = message[i + 5..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// 最後已知內容的標示：「（最後已知，N 分鐘前）」（`cli-spec.md` §4.8）
fn last_known_ago(age: Option<Duration>) -> String {
    match age {
        Some(a) if a == Duration::MAX => "（最後已知，重新開機前）".to_string(),
        Some(a) => {
            let m = a.as_secs() / 60;
            match m {
                0..60 => format!("（最後已知，{m} 分鐘前）"),
                60..1440 => format!("（最後已知，{} 小時前）", m / 60),
                _ => format!("（最後已知，{} 天前）", m / 1440),
            }
        }
        None => "（最後已知）".to_string(),
    }
}

/// 「時鐘」列（`cli-spec.md` §4.2）
fn clock_row(
    file: Option<&StatusFile>,
    fresh: bool,
    connected: bool,
    ctx: &Ctx,
    issues: &mut Vec<Issue>,
) -> Row {
    let Some(f) = file.filter(|_| fresh) else {
        let why = if file.is_some() {
            "服務未執行，無法量測"
        } else {
            "沒有狀態檔，無法判斷"
        };
        return Row::new("clock", "時鐘", Outcome::Skipped, why.to_string()).summary(why);
    };
    let Some(ms) = f.clock.offset_ms else {
        let why = if connected {
            "Kairos 沒有提供伺服器時間（0.1 版），無法量測"
        } else {
            "尚未連上 Kairos，無法量測"
        };
        return Row::new("clock", "時鐘", Outcome::Skipped, why.to_string()).summary(why);
    };
    let note = if connected {
        ""
    } else {
        "（上次連線時量測）"
    };
    let amount = fmt::offset_amount(ms);
    let direction = if ms > 0 { "快" } else { "慢" };
    let abs = ms.unsigned_abs();
    if abs <= 30_000 {
        Row::new(
            "clock",
            "時鐘",
            Outcome::Ok,
            format!("與 Kairos 相差 {amount}{note}"),
        )
        .summary(format!("相差 {amount}"))
    } else {
        let (result, code) = if abs > 300_000 {
            (Outcome::Error, codes::E_CLOCK_SKEW)
        } else {
            (Outcome::Warning, codes::W_CLOCK_SKEW)
        };
        issues.push(issues::clock(ctx, code, ms, ts_opt(&f.clock.measured_at)));
        Row::new(
            "clock",
            "時鐘",
            result,
            format!("比 Kairos {direction} {amount}{note}（{code}）"),
        )
        .code(code)
        .summary(format!("比 Kairos {direction} {amount}"))
    }
}
