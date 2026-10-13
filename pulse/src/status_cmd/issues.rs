//! `pulse status` 的三段式說明（`design-system/copywriting.md` §4；`--json` 的 `data.issues[]`）。
//!
//! 文字以設計包的範例為準（`cli-spec.md` §4.5、§4.5a、§4.6，`copywriting.md` §4.2 範例 9、12、69～72）。
//! 指向尚未實作的命令（`pulse check-config --connect`、`pulse enroll`）的處理步驟，改寫為目前版本
//! 可以做的事（見 `ERROR-CODES.md`「與設計包不一致之處」）。

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use super::eval::{FileState, Input};
use super::fmt::{self, Zone};
use super::service::ServiceInfo;
use crate::codes;
use crate::status_file::{Buffer, Connection, StatusFile, parse_ts};

/// `issues[].severity`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn token(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// 處理方式的一個步驟：說明（可多行）與要執行的指令（各一行）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub lines: Vec<String>,
    pub commands: Vec<String>,
}

fn step(line: impl Into<String>) -> Step {
    Step {
        lines: vec![line.into()],
        commands: Vec::new(),
    }
}

fn step_cmd(line: impl Into<String>, command: impl Into<String>) -> Step {
    Step {
        lines: vec![line.into()],
        commands: vec![command.into()],
    }
}

/// 一則說明
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub code: String,
    pub severity: Severity,
    pub title: String,
    /// 原因（人看的輸出照這裡分行；`--json` 接成一段）
    pub cause: Vec<String>,
    /// 寫在「處理方式：」同一行的說明
    pub fix_intro: Option<String>,
    pub fix: Vec<Step>,
    pub details: Map<String, Value>,
}

impl Issue {
    fn new(code: &str, severity: Severity) -> Self {
        Self {
            code: code.to_string(),
            severity,
            title: codes::title(code).unwrap_or("").to_string(),
            cause: Vec::new(),
            fix_intro: None,
            fix: Vec::new(),
            details: Map::new(),
        }
    }

    fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    fn cause(mut self, line: impl Into<String>) -> Self {
        self.cause.push(line.into());
        self
    }

    fn intro(mut self, intro: impl Into<String>) -> Self {
        self.fix_intro = Some(intro.into());
        self
    }

    fn step(mut self, s: Step) -> Self {
        self.fix.push(s);
        self
    }

    fn detail(mut self, key: &str, value: Value) -> Self {
        self.details.insert(key.to_string(), value);
        self
    }

    /// `--json` 的 `cause`：各行接成一段
    pub fn cause_text(&self) -> String {
        self.cause.concat()
    }

    /// `--json` 的 `fix[]`：同一行的說明（若有）與每個步驟（說明與指令接成一句）
    pub fn fix_texts(&self) -> Vec<String> {
        let mut out: Vec<String> = self.fix_intro.iter().cloned().collect();
        for s in &self.fix {
            let mut text = s.lines.concat();
            for c in &s.commands {
                text.push_str(c);
                text.push('；');
            }
            out.push(text.trim_end_matches('；').to_string());
        }
        out
    }
}

/// 組出說明時需要的環境
#[derive(Debug, Clone)]
pub struct Ctx {
    pub windows: bool,
    pub managed: bool,
    pub unit: String,
    pub server: String,
    pub host: String,
    pub port: String,
    pub agent_id: String,
    /// 用戶端憑證序號
    pub serial: Option<String>,
    pub zone: Zone,
    pub now: DateTime<Utc>,
    pub status_path: String,
    pub logs_dir: String,
    pub config_path: String,
}

impl Ctx {
    pub fn new(
        input: &Input,
        unit: &str,
        _hostname: &str,
        agent_id: Option<&str>,
        file: Option<&StatusFile>,
    ) -> Self {
        let server = file
            .map(|f| f.connection.server.clone())
            .unwrap_or_default();
        let (host, port) = match server.rsplit_once(':') {
            Some((h, p)) => (
                h.trim_start_matches('[').trim_end_matches(']').to_string(),
                p.to_string(),
            ),
            None => (server.clone(), String::new()),
        };
        let sep = if input.windows { '\\' } else { '/' };
        let logs_dir = std::path::Path::new(&input.status_path)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("logs{sep}"));
        Self {
            windows: input.windows,
            managed: input
                .service
                .as_ref()
                .is_some_and(|s| s.state != super::service::SvcState::NotInstalled),
            unit: unit.to_string(),
            server,
            host,
            port,
            agent_id: agent_id.unwrap_or("<agent_id>").to_string(),
            serial: file.and_then(|f| f.identity.cert_serial.clone()),
            zone: input.zone,
            now: input.now,
            status_path: input.status_path.clone(),
            logs_dir,
            config_path: file
                .and_then(|f| f.config_file.clone())
                .or_else(|| input.config.file.clone())
                .unwrap_or_else(|| input.config.expected.clone()),
        }
    }

    /// 重新啟動服務的指令（沒有服務管理員時為 `None`）
    fn restart(&self) -> Option<String> {
        if !self.managed {
            return None;
        }
        Some(if self.windows {
            format!("Restart-Service {}", self.unit)
        } else {
            format!(
                "sudo systemctl restart {}",
                self.unit.strip_suffix(".service").unwrap_or(&self.unit)
            )
        })
    }

    fn restart_step(&self, line: &str) -> Step {
        match self.restart() {
            Some(cmd) => step_cmd(format!("{line}："), cmd),
            None => step(format!("{line}（以啟動 Pulse 的方式重新執行）。")),
        }
    }

    /// 今天的日誌檔（UTC 日期）
    fn log_file(&self) -> String {
        let sep = if self.windows { '\\' } else { '/' };
        format!(
            "{}{sep}pulse-{}.log",
            self.logs_dir.trim_end_matches(['/', '\\']),
            self.now.format("%Y-%m-%d")
        )
    }

    fn view_log_step(&self, line: &str) -> Step {
        if self.windows {
            step_cmd(
                format!("{line}："),
                format!("Get-Content -Tail 30 \"{}\"", self.log_file()),
            )
        } else {
            step_cmd(
                format!("{line}："),
                format!("sudo tail -n 30 {}", self.log_file()),
            )
        }
    }

    fn network_step(&self) -> Step {
        let cmd = if self.windows {
            format!("Test-NetConnection {} -Port {}", self.host, self.port)
        } else {
            format!("nc -vz {} {}", self.host, self.port)
        };
        step_cmd("檢查網路路徑：", cmd)
    }
}

fn ts(s: &Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().and_then(parse_ts)
}

fn since(now: DateTime<Utc>, t: DateTime<Utc>) -> Duration {
    (now - t).to_std().unwrap_or_default()
}

/// 中斷期間：`2026-10-06 12:00～10-09 14:33 UTC+8`
fn outage_range(from: DateTime<Utc>, until: DateTime<Utc>, zone: Zone) -> String {
    let (a, b) = (zone.local(from), zone.local(until));
    let second = if a.date_naive() == b.date_naive() {
        b.format("%H:%M").to_string()
    } else {
        b.format("%m-%d %H:%M").to_string()
    };
    format!(
        "{}～{second} {}",
        a.format("%Y-%m-%d %H:%M"),
        fmt::tz_label(*b.offset())
    )
}

// ---------------------------------------------------------------------------
// 服務
// ---------------------------------------------------------------------------

pub fn status_stale(ctx: &Ctx, label: &str, file: &FileState, age: Option<Duration>) -> Issue {
    let cause = match (file, age) {
        (FileState::Missing, _) => format!(
            "{label}在執行，但沒有狀態檔 {}。可能是 Pulse 0.3 以前的版本（不寫狀態檔），或服務帳號無法寫入這個目錄。",
            ctx.status_path
        ),
        (FileState::Invalid(e), _) => format!(
            "{label}在執行，但狀態檔 {} 無法解析（{e}）。",
            ctx.status_path
        ),
        (_, Some(a)) if a == Duration::MAX => format!(
            "{label}在執行，但狀態檔 {} 是重新開機前寫的，之後沒有更新。",
            ctx.status_path
        ),
        (_, Some(a)) => format!(
            "{label}在執行，但狀態檔 {} 已 {}沒有更新（超過 60 秒），Pulse 可能卡住。",
            ctx.status_path,
            fmt::duration(a)
        ),
        _ => format!("{label}在執行，但狀態檔 {} 過時。", ctx.status_path),
    };
    Issue::new(codes::W_STATUS_STALE, Severity::Warning)
        .cause(cause)
        .step(ctx.view_log_step(&format!(
            "查看日誌中是否有 {} 或其他錯誤",
            codes::E_STATUS_UNWRITABLE
        )))
        .step(ctx.restart_step("仍未恢復時重新啟動服務"))
        .detail("status_file", json!(ctx.status_path))
        .detail(
            "age_ms",
            json!(
                age.filter(|a| *a != Duration::MAX)
                    .map(|a| a.as_millis() as u64)
            ),
        )
}

pub fn service_not_running(
    ctx: &Ctx,
    label: &str,
    managed: Option<&ServiceInfo>,
    file: Option<&StatusFile>,
    config_error: Option<&(&'static str, String)>,
) -> Issue {
    let mut first = match managed {
        Some(s) if !ctx.windows => {
            let state = s.state.as_str();
            match s.exit_code {
                Some(c) => format!("{} 為 {state}，最近一次結束代碼 {c}。", s.unit),
                None => format!("{} 為 {state}。", s.unit),
            }
        }
        Some(s) => match s.exit_code {
            Some(c) => format!("{label}已停止，最近一次結束代碼 {c}。"),
            None => format!("{label}已停止。"),
        },
        None => match file.and_then(|f| f.exit_code) {
            Some(c) => format!(
                "Pulse（PID {}）已結束，結束代碼 {c}。",
                file.map_or(0, |f| f.pid)
            ),
            None => format!(
                "Pulse（PID {}）已不在執行，沒有留下結束代碼（可能被強制結束或主機重新開機）。",
                file.map_or(0, |f| f.pid)
            ),
        },
    };
    let mut issue = Issue::new(codes::E_SERVICE_NOT_RUNNING, Severity::Error);
    match (config_error, file.and_then(|f| f.exit_message.clone())) {
        (Some((code, msg)), _) => {
            first.push_str("最後一則錯誤為");
            issue = issue.cause(first).cause(format!(
                "{code}（{}，{}）。",
                codes::title(code).unwrap_or("設定有誤"),
                short(msg)
            ));
            issue = issue.step(step(format!(
                "修正設定檔 {}：{}",
                ctx.config_path,
                short(msg)
            )));
        }
        (None, Some(msg)) => {
            issue = issue
                .cause(first)
                .cause(format!("最後一則錯誤：{}", short(&msg)));
            issue = issue.step(ctx.view_log_step("查看日誌，找出結束的原因"));
        }
        (None, None) => {
            issue = issue.cause(first);
            issue = issue.step(ctx.view_log_step("查看日誌，找出結束的原因"));
        }
    }
    issue
        .step(ctx.restart_step("修正後重新啟動服務"))
        .detail("unit", json!(ctx.unit))
        .detail(
            "exit_code",
            json!(
                managed
                    .and_then(|s| s.exit_code)
                    .or(file.and_then(|f| f.exit_code))
            ),
        )
}

pub fn service_not_installed(ctx: &Ctx, service: Option<&ServiceInfo>) -> Issue {
    let manager = match service {
        Some(s) if s.manager == "scm" => format!("Windows 服務 {}", s.unit),
        Some(s) => format!("systemd unit {}", s.unit),
        None => "服務管理員中的 Pulse".to_string(),
    };
    Issue::new(codes::E_SERVICE_NOT_INSTALLED, Severity::Error)
        .cause(format!(
            "找不到{manager}，也沒有執行中的 Pulse 留下的狀態檔 {}。",
            ctx.status_path
        ))
        .step(step("依 INSTALL.md 安裝並啟動 Pulse 服務。"))
        .step(step(
            "Pulse 安裝在其他目錄時，在該目錄執行 pulse status，或以 --config 指定 pulse.toml。",
        ))
        .detail("status_file", json!(ctx.status_path))
}

// ---------------------------------------------------------------------------
// 連線
// ---------------------------------------------------------------------------

/// 連線問題：`error` 為 `true` 時以錯誤呈現（需要人工處理，或中斷 ≥ 30 分鐘）。
/// 無法歸類（沒有代碼）時不另外說明（「連線」列已寫出原因），回傳 `None`
pub fn connection(ctx: &Ctx, c: &Connection, error: bool, outage: Duration) -> Option<Issue> {
    let severity = if error {
        Severity::Error
    } else {
        Severity::Warning
    };
    let rejected = c.last_rejection.as_ref();
    let code = rejected
        .and_then(|r| r.code.clone())
        .or_else(|| c.last_error_code.clone());
    let last_error = rejected
        .map(|r| r.message.clone())
        .or_else(|| c.last_error.clone())
        .unwrap_or_default();
    let dur = fmt::duration(outage);
    let intro = if error {
        "服務會自動重連，但已中斷超過 30 分鐘，請檢查："
    } else {
        "服務會自動重連，通常不需處理。超過 30 分鐘仍未恢復時："
    };
    let network = |issue: Issue| {
        issue
            .intro(intro)
            .step(step("向 Kairos 管理者確認服務狀態。"))
            .step(ctx.network_step())
            .detail("protocol_code", Value::Null)
            .detail("address", json!(ctx.server))
            .detail("outage_ms", json!(outage.as_millis() as u64))
    };
    let code = code?;
    let base = Issue::new(&code, severity);
    Some(match code.as_str() {
        codes::E_CONN_REFUSED => network(
            base.cause(format!(
                "{} 的 {} 連接埠拒絕連線，已持續 {dur}。",
                ctx.host, ctx.port
            ))
            .cause("多半是 Kairos 正在重新啟動或維護；也可能是連接埠改變或防火牆規則變更。"),
        ),
        codes::E_CONN_TIMEOUT => network(
            base.cause(format!(
                "{} 的 {} 連接埠沒有回應（連線逾時），已持續 {dur}。",
                ctx.host, ctx.port
            ))
            .cause("可能是網路不通、防火牆擋下封包，或 Kairos 主機沒有開機。"),
        ),
        codes::E_DNS => {
            let cmd = if ctx.windows {
                format!("Resolve-DnsName {}", ctx.host)
            } else {
                format!("getent hosts {}", ctx.host)
            };
            base.cause(format!(
                "找不到主機名稱 {}（DNS 解析失敗），已持續 {dur}。",
                ctx.host
            ))
            .cause("可能是 DNS 伺服器無法連線，或 pulse.toml 的 server.address 打錯。")
            .step(step(format!(
                "確認 pulse.toml 的 server.address（{}）。",
                ctx.host
            )))
            .step(step_cmd("確認名稱解析：", cmd))
            .detail("address", json!(ctx.server))
            .detail("outage_ms", json!(outage.as_millis() as u64))
        }
        codes::E_SERVER_SILENT => network(
            base.cause(format!(
                "連上 {} 後，Kairos 沒有在時限內回應（{last_error}），已持續 {dur}。",
                ctx.server
            ))
            .cause("多半是 Kairos 負載過高或正在重新啟動。"),
        ),
        codes::E_CLIENT_CERT_REJECTED => {
            let serial = ctx
                .serial
                .as_deref()
                .map(|s| format!("（序號 {s}）"))
                .unwrap_or_default();
            base.cause(format!(
                "Kairos 拒絕了這台主機的憑證{serial}。通常是管理者撤銷了這台主機"
            ))
                .cause("（主機汰換、疑似外洩或重新檢查）。")
                .step(step("向 Kairos 管理者確認撤銷原因。"))
                .step(match ctx.restart() {
                    Some(cmd) => step_cmd(
                        "若這台主機應繼續被監控，請管理者重新簽發用戶端憑證，更新 server.client_cert／server.client_key 後重新啟動：",
                        cmd,
                    ),
                    None => step(
                        "若這台主機應繼續被監控，請管理者重新簽發用戶端憑證，更新 server.client_cert／server.client_key 後重新啟動 Pulse。",
                    ),
                })
                .detail("protocol_code", json!(protocol_code(&last_error)))
        }
        other => {
            let fix = match other {
                codes::E_SERVER_CRL => {
                    "請聯絡 Kairos 伺服器管理者更新 CRL，並提供錯誤代碼 PLS-E1203。本機的憑證沒有問題，不需要重新簽發。"
                        .to_string()
                }
                codes::E_CLIENT_CERT_MISMATCH | codes::E_CLIENT_CERT_MULTI_SAN => format!(
                    "請管理者以這台主機的 agent_id 重新簽發用戶端憑證（SAN 只含 DNS:{}）。",
                    ctx.agent_id
                ),
                codes::E_CLIENT_CERT_EXPIRED => {
                    "請管理者重新簽發用戶端憑證；若憑證其實仍有效，請檢查本機時鐘。".to_string()
                }
                codes::E_CLIENT_CERT_UNTRUSTED => {
                    "確認用戶端憑證由 Kairos 信任的 client CA 簽發，且 server.client_cert 含完整的中繼 CA 鏈。"
                        .to_string()
                }
                codes::E_CLIENT_CERT_REQUIRED => {
                    "設定 pulse.toml 的 server.client_cert 與 server.client_key 後重新啟動 Pulse。".to_string()
                }
                codes::E_SERVER_CERT_UNTRUSTED => {
                    "自簽憑證或私有 CA 請以 pulse.toml 的 server.ca_cert 指定。".to_string()
                }
                codes::E_SERVER_CERT_NAME => {
                    "把 pulse.toml 的 server.domain 改成伺服器憑證 SAN 中的名稱，或請伺服器管理者重新簽發。"
                        .to_string()
                }
                codes::E_SERVER_CERT_VALIDITY => {
                    "請伺服器管理者更新伺服器憑證，或檢查本機時鐘。".to_string()
                }
                codes::E_SERVER_CERT_REVOKED => "請伺服器管理者更換伺服器憑證。".to_string(),
                codes::E_SERVER_CERT_PURPOSE => {
                    "請伺服器管理者重新簽發伺服器憑證（EKU 含 serverAuth、basicConstraints CA:FALSE）。"
                        .to_string()
                }
                codes::E_PROTO_VERSION => "更新 Pulse 或 Kairos，讓雙方支援相同的協定版本。".to_string(),
                codes::E_AGENT_ID_CONFLICT => {
                    "在多出來的主機上刪除 agent.id 後重新啟動 Pulse；啟用 mTLS 時另需以新的 agent_id 重新簽發用戶端憑證。"
                        .to_string()
                }
                codes::E_REGISTRY_FULL => "請 Kairos 管理者調高 limits.max_agents。".to_string(),
                _ => format!("依日誌（{}）中的處理提示修正。", ctx.log_file()),
            };
            let mut issue = base.cause(format!("{}。", last_error.trim_end_matches('。')));
            if error {
                issue = issue.cause("服務會持續重試，但在修正之前不會成功。");
            }
            issue
                .step(step(fix))
                .detail("protocol_code", json!(protocol_code(&last_error)))
        }
    })
}

/// 斷線原因中的協定錯誤碼（`伺服器回報致命錯誤 certificate_rejected：…`）
fn protocol_code(last_error: &str) -> Option<String> {
    let rest = last_error.split_once("錯誤 ").map(|(_, r)| r)?;
    let code: String = rest
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || *c == '_')
        .collect();
    (!code.is_empty()).then_some(code)
}

// ---------------------------------------------------------------------------
// 回報與本機緩衝
// ---------------------------------------------------------------------------

pub fn server_storage(ctx: &Ctx, code: &str, pending: u64, oldest: Option<DateTime<Utc>>) -> Issue {
    let oldest = oldest
        .map(|t| format!("，最舊一筆 {}", fmt::moment(t, ctx.now, ctx.zone)))
        .unwrap_or_default();
    let what = if code == codes::W_REPORT_RATE {
        "Kairos 回應「回報速率超過上限」（storage_unavailable）。"
    } else {
        "Kairos 回應「回報無法保存」（storage_unavailable），通常是伺服器磁碟已滿或寫入逾時。"
    };
    Issue::new(code, Severity::Warning)
        .cause(what)
        .cause(format!(
            "資料留在本機緩衝（目前待補送 {} 筆{oldest}），Pulse 等待 2～60 秒後重送。",
            fmt::thousands(pending)
        ))
        .intro(
            "本機不需處理。持續超過 1 小時時，請伺服器管理者查看 Kairos 是否有錯誤代碼 KRS-E4101。",
        )
        .detail("pending", json!(pending))
}

pub fn buffer_unavailable(ctx: &Ctx, b: &Buffer) -> Issue {
    let dir = b.dir.clone().unwrap_or_else(|| "buffer".to_string());
    let err = b.error.clone().unwrap_or_else(|| "原因不明".to_string());
    let issue = Issue::new(codes::E_BUFFER_UNAVAILABLE, Severity::Warning)
        .cause(format!(
            "Pulse 無法建立或寫入緩衝目錄 {dir}（{}），本次執行改為不補送（at-most-once）：",
            short(&err)
        ))
        .cause("連線期間照常即時回報，與 Kairos 中斷期間的資料不會保留。");
    let issue = if ctx.windows {
        issue.step(ctx.restart_step(&format!("確認服務帳號對 {dir} 有修改權限後，重新啟動服務")))
    } else {
        issue
            .step(step_cmd(
                "建立目錄：",
                format!("sudo install -d -o pulse -g pulse -m 750 {dir}"),
            ))
            .step(ctx.restart_step(&format!(
                "確認 pulse.service 的 ReadWritePaths= 含 -{dir}，執行 sudo systemctl daemon-reload 後重新啟動"
            )))
    };
    issue.detail("dir", json!(dir)).detail("error", json!(err))
}

pub fn buffer_full(ctx: &Ctx, f: &StatusFile) -> Issue {
    let b = &f.reporting.buffer;
    let c = &f.connection;
    let dropped = b.dropped_full.unwrap_or(0);
    let pending = b.pending.unwrap_or(0) as f64;
    let records = b.max_records.unwrap_or(0);
    let bytes = b.max_bytes.unwrap_or(0);
    // 先到的上限：以目前的用量判斷
    let by_bytes = records == 0
        || (bytes > 0
            && b.pending_bytes.unwrap_or(0) as f64 / bytes as f64 > pending / records as f64);
    let limit = if by_bytes {
        format!("buffer.max_mb（{}）", fmt::mib(bytes))
    } else {
        format!("buffer.max_records（{} 筆）", fmt::thousands(records))
    };
    let range = match (ts(&b.dropped_full_from), ts(&b.dropped_full_until)) {
        (Some(a), Some(z)) => format!("（{} 的取樣）", fmt::sample_range(a, z, ctx.zone)),
        _ => String::new(),
    };
    // 丟棄發生在哪一次中斷：進行中的中斷，或剛恢復的上一次中斷
    let outage = match c.state.as_str() {
        "registered" => match (ts(&c.last_outage_from), ts(&c.last_outage_until)) {
            (Some(a), Some(z)) => Some(format!(
                "與 Kairos 中斷期間（{}，約 {}）",
                outage_range(a, z, ctx.zone),
                fmt::duration(since(z, a))
            )),
            _ => None,
        },
        _ => ts(&c.since).map(|a| {
            format!(
                "與 Kairos 中斷期間（自 {} 起，已 {}）",
                fmt::date_time(a, ctx.zone),
                fmt::duration(since(ctx.now, a))
            )
        }),
    };
    let mut issue = Issue::new(codes::W_BUFFER_FULL, Severity::Warning);
    issue = match outage {
        Some(o) => issue
            .cause(format!("{o}，待補送資料"))
            .cause(format!(
                "達到上限 {limit}。Pulse 丟掉最舊的 {} 筆",
                fmt::thousands(dropped)
            ))
            .cause(format!("{range}，這段資料不會補送。")),
        None => issue
            .cause(format!(
                "待補送資料達到上限 {limit}。Pulse 丟掉最舊的 {} 筆",
                fmt::thousands(dropped)
            ))
            .cause(format!("{range}，這段資料不會補送。")),
    };
    let first = if c.state == "registered" {
        "本機不需處理：其餘的資料正在自動補送。"
    } else {
        "本機不需處理：恢復連線後其餘的資料會自動補送。"
    };
    issue
        .step(step(first))
        .step(Step {
            lines: vec![
                "預期會有更長的中斷時，調大 pulse.toml 的 [buffer] max_records 與 max_mb，".into(),
                "並讓 buffer.max_age_hours 不超過 Kairos 原始資料的保留期。".into(),
            ],
            commands: Vec::new(),
        })
        .detail("dropped_full", json!(dropped))
        .detail("dropped_from", json!(b.dropped_full_from))
        .detail("dropped_until", json!(b.dropped_full_until))
        .detail("max_records", json!(records))
        .detail("max_bytes", json!(bytes))
}

pub fn buffer_expired(ctx: &Ctx, f: &StatusFile) -> Issue {
    let b = &f.reporting.buffer;
    let hours = b.max_age_hours.unwrap_or(0);
    let range = match (ts(&b.dropped_expired_from), ts(&b.dropped_expired_until)) {
        (Some(a), Some(z)) => format!("（{} 的取樣）", fmt::sample_range(a, z, ctx.zone)),
        _ => String::new(),
    };
    Issue::new(codes::W_BUFFER_EXPIRED, Severity::Warning)
        .cause(format!(
            "本機緩衝中有取樣時間超過 buffer.max_age_hours（{hours} 小時＝{}）的資料，Pulse 在送出前丟掉，",
            fmt::hours(hours)
        ))
        .cause(format!(
            "共 {} 筆{range}，這段資料不會補送。",
            fmt::thousands(b.dropped_expired.unwrap_or(0))
        ))
        .intro(
            "本機不需處理。需要補送更久以前的資料時調大 buffer.max_age_hours（最多 1 年），\
             但不要超過 Kairos 原始資料的保留期，否則重送時可能重複寫入。",
        )
        .detail("dropped_expired", json!(b.dropped_expired))
        .detail("max_age_hours", json!(hours))
}

pub fn buffer_write_failed(ctx: &Ctx, b: &Buffer) -> Issue {
    let dir = b.dir.clone().unwrap_or_else(|| "buffer".to_string());
    let since_text = ts(&b.last_write_failure_at)
        .map(|t| format!("（最近一次 {}）", fmt::moment(t, ctx.now, ctx.zone)))
        .unwrap_or_default();
    Issue::new(codes::W_BUFFER_WRITE, Severity::Warning)
        .cause(format!(
            "寫入本機緩衝失敗{since_text}：{}。",
            short(b.last_write_error.as_deref().unwrap_or("原因不明"))
        ))
        .cause(format!(
            "共有 {} 筆取樣沒有寫入緩衝，這些資料遺失；緩衝中原有的待補送資料不受影響。",
            fmt::thousands(b.write_failures.unwrap_or(0))
        ))
        .intro(format!(
            "釋放 {dir} 所在磁碟的空間，下一筆取樣會自動寫入，不需重新啟動；磁碟空間很小的主機可調小 [buffer] max_mb。"
        ))
        .detail("write_failures", json!(b.write_failures))
        .detail("error", json!(b.last_write_error))
}

// ---------------------------------------------------------------------------
// 身分、設定、時鐘
// ---------------------------------------------------------------------------

pub fn cert_expired(ctx: &Ctx, f: &StatusFile, not_after: DateTime<Utc>, confirmed: bool) -> Issue {
    let path = f.identity.client_cert.clone().unwrap_or_default();
    let issue = Issue::new(
        codes::E_CLIENT_CERT_EXPIRED,
        if confirmed {
            Severity::Error
        } else {
            Severity::Warning
        },
    );
    if confirmed {
        issue
            .cause(format!(
                "server.client_cert（{path}）在 {} 過期，Kairos 會拒絕連線。",
                fmt::date_time(not_after, ctx.zone)
            ))
            .step(step(
                "請管理者重新簽發用戶端憑證，更新 server.client_cert／server.client_key 後重新啟動 Pulse。",
            ))
            .step(step("若憑證其實仍有效，請檢查本機時鐘。"))
            .detail("not_after", json!(f.identity.not_after))
    } else {
        issue
            .title("依本機時間，用戶端憑證已過期（尚未經 Kairos 確認）")
            .cause(format!(
                "依本機時間，server.client_cert（{path}）已在 {} 過期，但還沒有經 Kairos 確認；本機時鐘可能不準。",
                fmt::date_time(not_after, ctx.zone)
            ))
            .intro("先確認本機時間是否正確；Pulse 會照常以目前的憑證連線，以 Kairos 的判斷為準。")
            .detail("not_after", json!(f.identity.not_after))
    }
}

pub fn cert_not_yet_valid(ctx: &Ctx, f: &StatusFile, not_before: DateTime<Utc>) -> Issue {
    Issue::new(codes::E_CERT_NOT_YET_VALID, Severity::Warning)
        .cause(format!(
            "server.client_cert（{}）要到 {} 才生效（本機時間 {}），Kairos 會拒絕連線。",
            f.identity.client_cert.clone().unwrap_or_default(),
            fmt::date_time(not_before, ctx.zone),
            fmt::date_time(ctx.now, ctx.zone)
        ))
        .intro("請檢查本機時鐘與時間同步。")
        .detail("not_before", json!(f.identity.not_before))
}

pub fn cert_expiring(ctx: &Ctx, f: &StatusFile, not_after: DateTime<Utc>, days: i64) -> Issue {
    Issue::new(codes::W_CLIENT_CERT_EXPIRING, Severity::Warning)
        .cause(format!(
            "server.client_cert（{}）在 {} 到期（剩餘 {days} 天）。",
            f.identity.client_cert.clone().unwrap_or_default(),
            fmt::date_time(not_after, ctx.zone)
        ))
        .cause("這個版本的 Pulse 不會自動更新憑證，到期後 Kairos 會拒絕連線。")
        .intro("請管理者在到期前重新簽發用戶端憑證，更新 server.client_cert／server.client_key 後重新啟動 Pulse。")
        .detail("not_after", json!(f.identity.not_after))
        .detail("remaining_days", json!(days))
}

pub fn permissions(_ctx: &Ctx, code: &str, message: &str) -> Issue {
    Issue::new(code, Severity::Warning)
        .cause(short(message))
        .intro("依說明調整檔案權限（INSTALL.md「檔案權限」）。")
}

pub fn config_error(ctx: &Ctx, code: &str, message: &str, running: bool) -> Issue {
    let mut issue =
        Issue::new(code, Severity::Error).cause(format!("{}：{}", ctx.config_path, short(message)));
    if running {
        issue =
            issue.cause("執行中的 Pulse 仍使用啟動時讀到的設定；下次重新啟動時會因此無法啟動。");
    }
    issue
        .step(step(format!("修正 {}。", ctx.config_path)))
        .step(ctx.restart_step("修正後重新啟動服務"))
        .detail("file", json!(ctx.config_path))
}

pub fn config_warning(_ctx: &Ctx, code: &str, message: &str) -> Issue {
    let fix = match code {
        codes::W_TLS_VERIFY_DISABLED => {
            "把 accept_invalid_certs 改為 false，並以 server.ca_cert 指定 Kairos 的 CA。"
        }
        codes::W_BOTH_TRUST_STORES => {
            "不需要信任公開 CA 時，移除 use_system_roots（或設為 false），只信任 server.ca_cert。"
        }
        codes::W_SYSTEM32_WORKDIR => "在服務設定中把工作目錄設為 Pulse 所在的目錄。",
        _ => "依說明修正設定。",
    };
    Issue::new(code, Severity::Warning)
        .cause(format!("{}。", short(message).trim_end_matches('。')))
        .intro(fix)
}

pub fn clock(ctx: &Ctx, code: &str, offset_ms: i64, measured: Option<DateTime<Utc>>) -> Issue {
    let direction = if offset_ms > 0 { "快" } else { "慢" };
    let measured = measured
        .map(|t| format!("（{} 量測）", fmt::date_time(t, ctx.zone)))
        .unwrap_or_default();
    let severity = if code == codes::E_CLOCK_SKEW {
        Severity::Error
    } else {
        Severity::Warning
    };
    let step = if ctx.windows {
        step_cmd(
            "啟用時間同步並校正時間（以系統管理員身分）：",
            "w32tm /resync",
        )
    } else {
        step_cmd("啟用時間同步：", "sudo timedatectl set-ntp true")
    };
    Issue::new(code, severity)
        .title(format!(
            "本機時鐘比 Kairos {direction} {}",
            fmt::offset_amount(offset_ms)
        ))
        .cause(format!(
            "本機時鐘比 Kairos {direction} {}{measured}。report 的時間戳記與日誌時間會不準確。",
            fmt::offset_amount(offset_ms)
        ))
        .step(step)
        .detail("offset_ms", json!(offset_ms))
}

/// 說明中引用的錯誤文字：單行、最多 300 字元
fn short(s: &str) -> String {
    let one: String = s.replace(['\r', '\n'], " ");
    if one.chars().count() > 300 {
        format!("{}…", one.chars().take(300).collect::<String>())
    } else {
        one
    }
}
