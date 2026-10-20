//! 規則：系統預設規則（`alerts-ux.md` §2.2）與使用者規則的驗證（S09；儲存失敗為 `KRS-E5001`）。
//!
//! 系統預設規則在引擎啟動時補齊（`INSERT OR IGNORE`，已存在的不覆寫使用者的調整），不可刪除，
//! 停用需填原因；使用者可以調整通知管道、通知設定與門檻，系統類規則的嚴重度不可低於預設。

use serde_json::{Map, Value, json};

use super::model::{Category, NotifyPolicy, Rule, Scope, Severity};
use super::scope;

/// 規則無法儲存（設計包 `copywriting.md` §3.3）
pub const CODE_RULE_INVALID: &str = "KRS-E5001";

/// 規則的種類
pub mod kind {
    pub const HOST_OFFLINE: &str = "host_offline";
    pub const HOST_LONG_OFFLINE: &str = "host_long_offline";
    pub const REPORT_STALE: &str = "report_stale";
    pub const METRIC_THRESHOLD: &str = "metric_threshold";
    pub const IDENTITY_CONFLICT: &str = "identity_conflict";
    pub const CERT_REJECTED: &str = "cert_rejected";
    pub const HOST_DECOMMISSIONED: &str = "host_decommissioned";
    pub const MASS_DECOMMISSION: &str = "mass_decommission";
    pub const SUBNET_OFFLINE: &str = "subnet_offline";
    pub const MASS_OFFLINE: &str = "mass_offline";
    pub const RESTART_UNRECOVERED: &str = "restart_unrecovered";
    /// 主機重新開機（METRICS.md 第 11.2 節；D5）
    pub const HOST_REBOOTED: &str = "host_rebooted";
    pub const SYSTEM: &str = "system";

    /// 使用者可以建立的種類（S09；憑證即將到期、版本過舊在之後的版本加入）
    pub const USER: [&str; 5] = [
        HOST_OFFLINE,
        HOST_LONG_OFFLINE,
        REPORT_STALE,
        METRIC_THRESHOLD,
        IDENTITY_CONFLICT,
    ];
}

/// 系統預設規則的 ID
pub mod ids {
    pub const HOST_OFFLINE: &str = "sys.host_offline";
    pub const HOST_LONG_OFFLINE: &str = "sys.host_long_offline";
    pub const REPORT_STALE: &str = "sys.report_stale";
    pub const IDENTITY_CONFLICT: &str = "sys.identity_conflict";
    pub const CERT_REJECTED: &str = "sys.cert_rejected";
    pub const HOST_DECOMMISSIONED: &str = "sys.host_decommissioned";
    pub const MASS_DECOMMISSION: &str = "sys.mass_decommission";
    pub const SUBNET_OFFLINE: &str = "sys.subnet_offline";
    pub const MASS_OFFLINE: &str = "sys.mass_offline";
    pub const RESTART_UNRECOVERED: &str = "sys.restart_unrecovered";
    pub const CRL_EXPIRING: &str = "sys.crl_expiring";
    pub const CRL_EXPIRED: &str = "sys.crl_expired";
    pub const SERVER_CERT_EXPIRING: &str = "sys.server_cert_expiring";
    pub const REPORT_WRITES: &str = "sys.report_writes";
    pub const DB_CAPACITY: &str = "sys.db_capacity";
    pub const RETENTION_PAUSED: &str = "sys.retention_paused";
    pub const REGISTRY_CAPACITY: &str = "sys.registry_capacity";
    pub const CONNECTIONS: &str = "sys.connections";
    pub const PER_IP_LIMIT: &str = "sys.per_ip_limit";
    pub const ENROLL_OVERLOAD: &str = "sys.enroll_overload";
    pub const ENROLL_BLOCKED: &str = "sys.enroll_blocked";
    pub const ENROLL_RATE_LIMITED: &str = "sys.enroll_rate_limited";
    pub const ENROLL_CLOCK_SKEW: &str = "sys.enroll_clock_skew";
    pub const ENROLL_CLOSED_DEFAULT: &str = "sys.enroll_closed_default";
    pub const ENROLL_AUTO_PAUSED: &str = "sys.enroll_auto_paused";
    pub const TLS_RELOAD_FAILED: &str = "sys.tls_reload_failed";
    pub const UNAUTHENTICATED: &str = "sys.unauthenticated";
    pub const CHANNEL_FAILED: &str = "sys.channel_failed";
    pub const RESTARTED: &str = "sys.restarted";
    pub const DOWNTIME: &str = "sys.downtime";
    /// 稽核日誌完整性（0.7；設計包 `decisions/INTEGRATION.md` §28.5 #10）
    pub const AUDIT_INTEGRITY: &str = "sys.audit_integrity";
    /// 主機重新開機（0.8〔草稿〕；METRICS.md 第 11.2 節、D5：資訊等級、不通知、預設啟用）
    pub const HOST_REBOOTED: &str = "sys.host_rebooted";
    /// 升級推送已自動暫停（0.9；REMOTE.md 第 4.7.4 節、設計包 §31.5）
    pub const ROLLOUT_PAUSED: &str = "sys.rollout_paused";
    /// 升級回滾失敗，需要人工處理（0.9）
    pub const UPGRADE_ROLLBACK_FAILED: &str = "sys.upgrade_rollback_failed";
    /// 發佈簽章驗證失敗（0.9）
    pub const RELEASE_SIGNATURE_FAILED: &str = "sys.release_signature_failed";
}

/// 指標門檻（`metric_threshold`）的指標（METRICS.md 第 11.1 節）
pub mod metric {
    pub const CPU: &str = "cpu";
    pub const MEMORY: &str = "memory";
    /// 每個掛載點的使用率（%）
    pub const DISK: &str = "disk";
    /// 置換空間使用率（%）
    pub const SWAP: &str = "swap";
    /// 5 分鐘平均負載 ÷ 邏輯處理器數（倍）
    pub const LOAD: &str = "load";
    /// 每個介面的接收／傳送速率（Mbps）
    pub const NETWORK_RX: &str = "network_rx";
    pub const NETWORK_TX: &str = "network_tx";

    pub const ALL: [&str; 7] = [CPU, MEMORY, DISK, SWAP, LOAD, NETWORK_RX, NETWORK_TX];

    /// 門檻的上限：百分比與每核心負載 100，網路 1,000,000 Mbps
    pub fn max_threshold(metric: &str) -> f64 {
        match metric {
            NETWORK_RX | NETWORK_TX => 1_000_000.0,
            _ => 100.0,
        }
    }

    /// 門檻的單位（訊息與事件標題）
    pub fn unit(metric: &str) -> &'static str {
        match metric {
            NETWORK_RX | NETWORK_TX => " Mbps",
            LOAD => " 倍",
            _ => "%",
        }
    }

    /// 事件標題中的名稱
    pub fn label(metric: &str) -> &'static str {
        match metric {
            MEMORY => "記憶體",
            DISK => "磁碟使用率",
            SWAP => "置換空間",
            LOAD => "每核心負載",
            NETWORK_RX => "網路接收",
            NETWORK_TX => "網路傳送",
            _ => "CPU",
        }
    }
}

/// `params.mountpoints`、`params.devices` 最多幾個 glob（設計包 §30.5 #11：與 Pulse 每台最多 16 個掛載點、
/// 16 個介面一致）
pub const MAX_SCOPE_GLOBS: usize = 16;

/// glob 比對（`*` 任意字元、`?` 單一字元；區分大小寫）
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// `params` 中的 glob 清單（`mountpoints`、`devices`）；空的或沒有時為 `None`（全部）
pub fn globs(params: &Value, key: &str) -> Option<Vec<String>> {
    let list: Vec<String> = params
        .get(key)?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!list.is_empty()).then_some(list)
}

fn validate_globs(p: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>, Invalid> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            if items.len() > MAX_SCOPE_GLOBS {
                return Err(invalid(
                    &format!("params.{key}"),
                    format!("{key} 最多 {MAX_SCOPE_GLOBS} 個"),
                ));
            }
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                match v.as_str() {
                    Some(s)
                        if !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control) =>
                    {
                        if !out.iter().any(|o: &String| o == s) {
                            out.push(s.to_string());
                        }
                    }
                    _ => {
                        return Err(invalid(
                            &format!("params.{key}"),
                            format!(
                                "{key} 的每一項必須是 1～256 bytes、不含控制字元的字串（可用 * 與 ?）"
                            ),
                        ));
                    }
                }
            }
            Ok((!out.is_empty()).then_some(out))
        }
        Some(_) => Err(invalid(
            &format!("params.{key}"),
            format!("{key} 必須是字串陣列"),
        )),
    }
}

fn builtin(
    id: &str,
    kind: &str,
    category: Category,
    name: &str,
    severity: Severity,
    params: Value,
    description: &str,
) -> Rule {
    Rule {
        rule_id: id.to_string(),
        builtin: true,
        kind: kind.to_string(),
        category,
        name: name.to_string(),
        enabled: true,
        severity,
        params,
        scope: Scope::default(),
        channels: Vec::new(),
        notify: NotifyPolicy::default(),
        description: Some(description.to_string()),
        disabled_reason: None,
        created_at: 0,
        created_by: "system".to_string(),
        updated_at: 0,
        updated_by: "system".to_string(),
    }
}

/// 系統預設規則（`alerts-ux.md` §2.2）
pub fn builtins() -> Vec<Rule> {
    use Category::*;
    use Severity::*;
    let sys = |id: &str, name: &str, sev: Severity, params: Value, desc: &str| {
        builtin(id, kind::SYSTEM, System, name, sev, params, desc)
    };
    vec![
        builtin(
            ids::HOST_OFFLINE,
            kind::HOST_OFFLINE,
            Host,
            "主機離線",
            Critical,
            json!({"grace_secs": 180}),
            "被判定離線後持續 3 分鐘（在 offline_after_ms 之外另計）；中斷原因為「Kairos 關機」時不觸發。",
        ),
        builtin(
            ids::HOST_LONG_OFFLINE,
            kind::HOST_LONG_OFFLINE,
            Host,
            "主機長期失聯",
            Warning,
            json!({"after_secs": 86_400}),
            "離線 24 小時以上。",
        ),
        builtin(
            ids::REPORT_STALE,
            kind::REPORT_STALE,
            Host,
            "回報延遲",
            Warning,
            json!({"factor": 3}),
            "線上但最近一次 report 超過 3 倍 report 間隔沒有更新（心跳正常、取樣卡住）。",
        ),
        builtin(
            ids::IDENTITY_CONFLICT,
            kind::IDENTITY_CONFLICT,
            Identity,
            "身分衝突",
            Critical,
            json!({"window_secs": 3600, "min_count": 5, "min_sources": 2, "code": "KRS-W3102"}),
            "60 分鐘內被 superseded 5 次以上，且來源 IP 有 2 個以上：多台主機共用同一個 agent.id。",
        ),
        builtin(
            ids::CERT_REJECTED,
            kind::CERT_REJECTED,
            Identity,
            "憑證被拒（單機）",
            Critical,
            json!({}),
            "主機以已撤銷、過期或不受信任的憑證（KRS-W1105、KRS-W1108），或與憑證不符的 agent_id（KRS-W1102）連線；管理員撤銷或已除役的主機不觸發。",
        ),
        builtin(
            ids::HOST_DECOMMISSIONED,
            kind::HOST_DECOMMISSIONED,
            Identity,
            "主機自行除役",
            Info,
            json!({}),
            "Pulse 完全移除時經 8443 送出除役通知；每台一則紀錄，不推播。",
        ),
        builtin(
            ids::MASS_DECOMMISSION,
            kind::MASS_DECOMMISSION,
            Identity,
            "短時間大量除役",
            Warning,
            json!({"window_secs": 600, "min_hosts": 20, "quiet_secs": 3600, "code": "KRS-W3108"}),
            "10 分鐘內 20 台以上主機自行除役（可能是誤派送的完全移除）；60 分鐘沒有新的除役時恢復。",
        ),
        builtin(
            ids::SUBNET_OFFLINE,
            kind::SUBNET_OFFLINE,
            Aggregate,
            "網段離線",
            Emergency,
            json!({"window_secs": 120, "min_hosts": 5, "min_ratio": 0.3, "join_secs": 1800}),
            "120 秒內同一網段有 5 台以上、且 30% 以上的線上主機因連線逾時或 agent 關閉而離線；吸收其中的主機離線事件。",
        ),
        builtin(
            ids::MASS_OFFLINE,
            kind::MASS_OFFLINE,
            Aggregate,
            "大規模離線",
            Emergency,
            json!({"window_secs": 120, "min_ratio": 0.5, "min_hosts": 10}),
            "120 秒內 50% 以上的線上主機離線（至少 10 台）：多半是 Kairos 端網路或核心交換器；吸收網段與主機事件。",
        ),
        builtin(
            ids::RESTART_UNRECOVERED,
            kind::RESTART_UNRECOVERED,
            Aggregate,
            "重新啟動後未恢復連線",
            Critical,
            json!({}),
            "告警暖機期結束時，重新啟動前在線上、但仍未重新連上的主機合併成一則事件。",
        ),
        sys(
            ids::CRL_EXPIRING,
            "撤銷清單（CRL）即將到期",
            Warning,
            json!({"code": "KRS-W1201", "escalates_to": "emergency", "warning_days": 7, "critical_days": 3, "emergency_days": 1}),
            "剩 7 天警告、3 天嚴重、1 天緊急；同一事件逐級升級。",
        ),
        sys(
            ids::CRL_EXPIRED,
            "撤銷清單已過期或不完整",
            Emergency,
            json!({"code": "KRS-E1203"}),
            "超過 nextUpdate：所有 agent 都會被拒絕連線；吸收因此被拒的主機事件。",
        ),
        sys(
            ids::SERVER_CERT_EXPIRING,
            "伺服器憑證即將到期",
            Warning,
            json!({"code": "KRS-W1001", "escalates_to": "emergency", "warning_days": 30, "critical_days": 14, "emergency_days": 7}),
            "剩 30 天警告、14 天嚴重、7 天緊急；同一事件逐級升級。",
        ),
        sys(
            ids::REPORT_WRITES,
            "回報無法寫入",
            Emergency,
            json!({"code": "KRS-E4101", "window_secs": 300}),
            "過去 5 分鐘有 storage_unavailable 或寫入結果未知；抑制「回報延遲」事件。",
        ),
        sys(
            ids::DB_CAPACITY,
            "資料庫將達或已達總容量上限",
            Warning,
            json!({"code": "KRS-W4202"}),
            "預估保留期滿的用量超過 storage.max_total_bytes，或已開始提早刪除最舊的資料。",
        ),
        sys(
            ids::RETENTION_PAUSED,
            "保留清除暫停",
            Warning,
            json!({"code": "KRS-W4201"}),
            "系統日期異常，日期保護暫停清除過期資料。",
        ),
        sys(
            ids::REGISTRY_CAPACITY,
            "狀態表接近上限或已淘汰主機",
            Warning,
            json!({"code": "KRS-E3201", "ratio": 0.8, "window_secs": 3600}),
            "狀態表達 limits.max_agents 的 80%，或過去 1 小時淘汰過主機。",
        ),
        sys(
            ids::CONNECTIONS,
            "連線數接近上限",
            Warning,
            json!({"code": "KRS-W2204", "escalates_to": "critical", "warning_ratio": 0.9, "critical_ratio": 0.98, "window_secs": 3600}),
            "同時連線數達 server.max_connections 的 90%（或過去 1 小時因全域上限被拒）為警告，98% 或暫停接受新連線為嚴重。",
        ),
        sys(
            ids::PER_IP_LIMIT,
            "同一 IP 連線數超過上限",
            Warning,
            json!({"code": "KRS-W2201", "window_secs": 3600}),
            "過去 1 小時有連線因 server.max_connections_per_ip 在握手前被拒（依來源各一則）。",
        ),
        sys(
            ids::ENROLL_OVERLOAD,
            "註冊端點過載或待核准佇列已滿",
            Critical,
            json!({"code": "KRS-W2205", "window_secs": 600}),
            "註冊端點整體負載達上限或待核准達 enroll.max_pending；8443 上已註冊的主機不受影響。",
        ),
        sys(
            ids::ENROLL_BLOCKED,
            "註冊來源被暫時封鎖",
            Warning,
            json!({"code": "KRS-W3005", "window_secs": 3600}),
            "過去 1 小時有來源因無效嘗試過多被暫時封鎖。",
        ),
        sys(
            ids::ENROLL_RATE_LIMITED,
            "註冊端點速率限制",
            Info,
            json!({"code": "KRS-W2202", "window_secs": 3600}),
            "過去 1 小時有註冊或續簽請求被延後（429）；NAT 批次部署的正常現象。",
        ),
        sys(
            ids::ENROLL_CLOCK_SKEW,
            "註冊因時鐘偏差暫不受理",
            Info,
            json!({"code": "KRS-W8302"}),
            "有主機最近 15 分鐘內因時鐘偏差超過 5 分鐘被拒（彙總一則）。",
        ),
        sys(
            ids::ENROLL_CLOSED_DEFAULT,
            "註冊端點維持關閉（升級）",
            Info,
            json!({"code": "KRS-I2010"}),
            "設定檔沒有 [enroll] 的 mode，註冊端點預設為關閉；啟動時記錄一次。",
        ),
        sys(
            ids::ENROLL_AUTO_PAUSED,
            "註冊端點自動暫停",
            Info,
            json!({}),
            "開放模式下連續一段時間沒有有效 token 而自動暫停新註冊；建立 token 後恢復。",
        ),
        sys(
            ids::TLS_RELOAD_FAILED,
            "TLS 設定重新載入失敗",
            Critical,
            json!({"code": "KRS-E1204"}),
            "熱重載失敗而沿用舊設定；新的撤銷可能沒有生效。",
        ),
        sys(
            ids::UNAUTHENTICATED,
            "未認證連線已允許",
            Critical,
            json!({"code": "KRS-W0302", "unsilenceable": true, "remind_secs": 86_400}),
            "auth.allow_unauthenticated = true 且監聽對外位址；每日提醒一次，不可靜音。",
        ),
        sys(
            ids::CHANNEL_FAILED,
            "通知管道失敗",
            Critical,
            json!({"code": "KRS-E5107", "failures": 3}),
            "同一管道連續 3 次發送失敗；這個事件改經其他健康的管道送出。",
        ),
        sys(
            ids::RESTARTED,
            "Kairos 已重新啟動",
            Info,
            json!({}),
            "服務啟動；非預期的重新啟動（上次沒有正常關機）為嚴重。",
        ),
        sys(
            ids::DOWNTIME,
            "Kairos 停機期間未收集資料",
            Critical,
            json!({"min_secs": 300}),
            "非計畫性停機超過 5 分鐘（不在維護時段內），供上線率報表標註。",
        ),
        builtin(
            ids::HOST_REBOOTED,
            kind::HOST_REBOOTED,
            Host,
            "主機重新開機",
            Info,
            json!({}),
            "Pulse 回報的開機後經過時間（system.uptime）變小：主機重新開機過。資訊等級，不需要確認、不通知，只記錄在事件列表與主機詳情（需要 Pulse 0.8.0 以上）。",
        ),
        sys(
            ids::AUDIT_INTEGRITY,
            "稽核日誌完整性",
            Emergency,
            json!({"code": "KRS-E6201"}),
            "定期自我驗證（audit.verify_interval_hours，預設每 24 小時）發現雜湊鏈或 HMAC 斷裂、HMAC 金鑰遺失或不符；下一次驗證通過時恢復。",
        ),
        sys(
            ids::ROLLOUT_PAUSED,
            "升級推送已暫停",
            Warning,
            json!({"code": "KRS-W8414"}),
            "升級推送因失敗比例超過上限、主機回滾失敗或發佈簽章驗證失敗而自動暫停（手動暫停不開事件）；推送繼續或取消時恢復。原因為回滾失敗或簽章失敗時為嚴重。",
        ),
        sys(
            ids::UPGRADE_ROLLBACK_FAILED,
            "升級回滾失敗",
            Critical,
            json!({"code": "KRS-E8408"}),
            "主機升級後健康確認失敗、回滾也失敗，需要現場人工處理；在推送的主機表或主機詳情標記為已處理（或重試）時恢復。",
        ),
        sys(
            ids::RELEASE_SIGNATURE_FAILED,
            "發佈簽章驗證失敗",
            Critical,
            json!({"code": "KRS-E8401"}),
            "主機以本機信任錨驗證 Pulse 發佈包的簽章失敗：版本庫或傳輸可能被竄改，或信任錨與發佈不符。使用同一版本的推送全部暫停；推送繼續或取消時恢復。",
        ),
    ]
}

/// 系統預設規則的定義（找不到時為 `None`）
pub fn builtin_default(rule_id: &str) -> Option<Rule> {
    builtins().into_iter().find(|r| r.rule_id == rule_id)
}

/// 驗證失敗（欄位、訊息）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    pub field: String,
    pub message: String,
}

fn invalid(field: &str, message: impl Into<String>) -> Invalid {
    Invalid {
        field: field.to_string(),
        message: message.into(),
    }
}

/// 名稱：1～100 個字元，不可含控制字元
pub fn validate_name(name: &str) -> Result<String, Invalid> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 100 || n.chars().any(char::is_control) {
        return Err(invalid("name", "名稱必須是 1～100 個字元，不可含控制字元"));
    }
    Ok(n.to_string())
}

fn num(params: &Map<String, Value>, key: &str) -> Option<f64> {
    params.get(key).and_then(Value::as_f64)
}

fn int_in(
    params: &Map<String, Value>,
    key: &str,
    default: u64,
    range: std::ops::RangeInclusive<u64>,
) -> Result<u64, Invalid> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => match v.as_u64() {
            Some(n) if range.contains(&n) => Ok(n),
            _ => Err(invalid(
                &format!("params.{key}"),
                format!("{key} 必須是 {}～{} 的整數", range.start(), range.end()),
            )),
        },
    }
}

/// 依種類驗證並正規化條件（`params`）
pub fn validate_params(kind_name: &str, params: &Value) -> Result<Value, Invalid> {
    let empty = Map::new();
    let p = match params {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err(invalid("params", "params 必須是物件")),
    };
    let out = match kind_name {
        kind::HOST_OFFLINE => json!({"grace_secs": int_in(p, "grace_secs", 180, 0..=86_400)?}),
        kind::HOST_LONG_OFFLINE => {
            json!({"after_secs": int_in(p, "after_secs", 86_400, 3600..=90 * 86_400)?})
        }
        kind::REPORT_STALE => json!({"factor": int_in(p, "factor", 3, 2..=100)?}),
        kind::IDENTITY_CONFLICT => json!({
            "window_secs": int_in(p, "window_secs", 3600, 60..=86_400)?,
            "min_count": int_in(p, "min_count", 5, 2..=1000)?,
            "min_sources": int_in(p, "min_sources", 2, 1..=100)?,
            "code": "KRS-W3102",
        }),
        kind::METRIC_THRESHOLD => {
            let metric = p.get("metric").and_then(Value::as_str).unwrap_or("");
            if !metric::ALL.contains(&metric) {
                return Err(invalid(
                    "params.metric",
                    format!("metric 必須是 {} 之一", metric::ALL.join("、")),
                ));
            }
            let op = p.get("op").and_then(Value::as_str).unwrap_or(">");
            if !matches!(op, ">" | ">=" | "<" | "<=") {
                return Err(invalid("params.op", "op 必須是 >、>=、<、<= 之一"));
            }
            let max = metric::max_threshold(metric);
            let range_text = match metric {
                metric::NETWORK_RX | metric::NETWORK_TX => "0～1000000（Mbps）",
                metric::LOAD => "0～100（每核心負載，倍）",
                _ => "0～100（百分比）",
            };
            let threshold = num(p, "threshold")
                .filter(|t| (0.0..=max).contains(t))
                .ok_or_else(|| {
                    invalid("params.threshold", format!("threshold 必須是 {range_text}"))
                })?;
            let for_secs = int_in(p, "for_secs", 600, 0..=86_400)?;
            let clear = match p.get("clear_threshold") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_f64().filter(|t| (0.0..=max).contains(t)).ok_or_else(
                    || {
                        invalid(
                            "params.clear_threshold",
                            format!("clear_threshold 必須是 {range_text}"),
                        )
                    },
                )?),
            };
            if let Some(c) = clear {
                let above = op.starts_with('>');
                if (above && c > threshold) || (!above && c < threshold) {
                    return Err(invalid(
                        "params.clear_threshold",
                        if above {
                            "恢復門檻不可高於觸發門檻（觸發條件為「大於」時，恢復條件是低於某個值）"
                        } else {
                            "恢復門檻不可低於觸發門檻（觸發條件為「小於」時，恢復條件是高於某個值）"
                        },
                    ));
                }
            }
            let clear_for = int_in(p, "clear_for_secs", 0, 0..=86_400)?;
            let mut out = json!({
                "metric": metric,
                "op": op,
                "threshold": threshold,
                "for_secs": for_secs,
                "clear_threshold": clear,
                "clear_for_secs": clear_for,
            });
            // 範圍：磁碟為掛載點、網路為介面（glob；省略為全部）。其他指標不接受
            let (scope_key, other_key) = match metric {
                metric::DISK => (Some("mountpoints"), "devices"),
                metric::NETWORK_RX | metric::NETWORK_TX => (Some("devices"), "mountpoints"),
                _ => (None, ""),
            };
            for key in ["mountpoints", "devices"] {
                let list = validate_globs(p, key)?;
                if list.is_some() && scope_key != Some(key) {
                    return Err(invalid(
                        &format!("params.{key}"),
                        if key == other_key {
                            format!(
                                "{key} 不適用於 {metric}（磁碟使用 mountpoints、網路使用 devices）"
                            )
                        } else {
                            format!("{key} 只適用於磁碟（mountpoints）或網路（devices）")
                        },
                    ));
                }
                if let Some(list) = list {
                    out[key] = json!(list);
                }
            }
            out
        }
        other => {
            return Err(invalid(
                "kind",
                format!(
                    "不支援建立這種規則：{other}（可用：{}）",
                    kind::USER.join("、")
                ),
            ));
        }
    };
    Ok(out)
}

/// 通知設定的範圍檢查；緊急等級必須設定升級對象（`alerts-ux.md` §5.3）
pub fn validate_notify(
    n: &NotifyPolicy,
    severity: Severity,
    builtin: bool,
    channel_exists: &dyn Fn(&str) -> bool,
) -> Result<NotifyPolicy, Invalid> {
    if let Some(i) = n.repeat_interval_secs
        && !(60..=86_400).contains(&i)
    {
        return Err(invalid(
            "notify.repeat_interval_secs",
            "重送間隔必須是 60～86400 秒",
        ));
    }
    if severity == Severity::Emergency
        && n.repeat_interval_secs
            .is_some_and(|i| i > super::model::EMERGENCY_MAX_REPEAT_SECS)
    {
        return Err(invalid(
            "notify.repeat_interval_secs",
            "緊急等級的重送間隔上限 30 分鐘",
        ));
    }
    if n.repeat_max.is_some_and(|m| m > 100) {
        return Err(invalid("notify.repeat_max", "重送次數最多 100 次"));
    }
    if let Some(e) = n.escalate_after_secs
        && !(60..=86_400).contains(&e)
    {
        return Err(invalid(
            "notify.escalate_after_secs",
            "升級等待時間必須是 60～86400 秒",
        ));
    }
    let mut escalate = Vec::new();
    for c in &n.escalate_channels {
        let c = c.trim().to_string();
        if !channel_exists(&c) {
            return Err(invalid(
                "notify.escalate_channels",
                format!("沒有通知管道 {c}"),
            ));
        }
        if !escalate.contains(&c) {
            escalate.push(c);
        }
    }
    // 系統預設規則在安裝時還沒有任何管道，不強制；使用者規則（與調整過的系統規則）必須設定
    if severity == Severity::Emergency && escalate.is_empty() && !builtin {
        return Err(invalid(
            "notify.escalate_channels",
            "緊急等級必須設定升級對象",
        ));
    }
    Ok(NotifyPolicy {
        escalate_channels: escalate,
        ..n.clone()
    })
}

/// 規則的通知管道
pub fn validate_channels(
    channels: &[String],
    channel_exists: &dyn Fn(&str) -> bool,
) -> Result<Vec<String>, Invalid> {
    let mut out: Vec<String> = Vec::new();
    for c in channels {
        let c = c.trim().to_string();
        if !channel_exists(&c) {
            return Err(invalid("channels", format!("沒有通知管道 {c}")));
        }
        if !out.contains(&c) {
            out.push(c);
        }
    }
    if out.len() > 20 {
        return Err(invalid("channels", "通知管道最多 20 個"));
    }
    Ok(out)
}

/// 範圍（規則的範圍與靜音相同）
pub fn validate_scope(s: &Scope) -> Result<Scope, Invalid> {
    scope::normalize(s).map_err(|(f, m)| invalid(f, m))
}

/// 說明：最多 2000 個字元
pub fn validate_description(d: Option<&str>) -> Result<Option<String>, Invalid> {
    match d.map(str::trim).filter(|d| !d.is_empty()) {
        None => Ok(None),
        Some(d) if d.chars().count() > 2000 => {
            Err(invalid("description", "處理說明最多 2000 個字元"))
        }
        Some(d) => Ok(Some(d.to_string())),
    }
}

/// 調整系統預設規則的條件：只能改預設中已有的數值欄位，且範圍沿用定義的上下限
pub fn validate_builtin_params(default: &Rule, params: &Value) -> Result<Value, Invalid> {
    let Value::Object(changes) = params else {
        return Err(invalid("params", "params 必須是物件"));
    };
    let mut merged = default.params.clone();
    let Value::Object(base) = &mut merged else {
        return Ok(default.params.clone());
    };
    for (k, v) in changes {
        match base.get(k) {
            Some(Value::Number(old)) => {
                let new = v
                    .as_f64()
                    .ok_or_else(|| invalid(&format!("params.{k}"), format!("{k} 必須是數字")))?;
                // 寬限期可以是 0（離線判定後立即通知）；其他門檻必須大於 0
                let min_ok = if k == "grace_secs" {
                    new >= 0.0
                } else {
                    new > 0.0
                };
                if !min_ok || !new.is_finite() {
                    return Err(invalid(&format!("params.{k}"), format!("{k} 必須大於 0")));
                }
                if k.ends_with("ratio") && new > 1.0 {
                    return Err(invalid(
                        &format!("params.{k}"),
                        format!("{k} 必須介於 0～1"),
                    ));
                }
                base.insert(
                    k.clone(),
                    if old.is_u64() {
                        json!(new.round() as u64)
                    } else {
                        json!(new)
                    },
                );
            }
            _ => {
                return Err(invalid(
                    &format!("params.{k}"),
                    format!("系統預設規則沒有可調整的條件 {k}"),
                ));
            }
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_unique_and_cover_the_design_table() {
        let all = builtins();
        let mut ids: Vec<&str> = all.iter().map(|r| r.rule_id.as_str()).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "規則 ID 重複");
        assert!(n >= 30);
        // 依天數升級的系統規則最高為緊急（靜音上限 4 小時）
        let crl = builtin_default(ids::CRL_EXPIRING).unwrap();
        assert_eq!(crl.max_severity(), Severity::Emergency);
        assert_eq!(crl.category, Category::System);
        // 資訊等級一律為紀錄
        let dec = builtin_default(ids::HOST_DECOMMISSIONED).unwrap();
        assert_eq!(dec.attention(), super::super::model::Attention::Record);
        // D5：主機重新開機為資訊等級（紀錄、不通知），預設啟用
        let reboot = builtin_default(ids::HOST_REBOOTED).unwrap();
        assert_eq!(reboot.severity, Severity::Info);
        assert_eq!(reboot.attention(), super::super::model::Attention::Record);
        for r in &all {
            assert!(r.builtin && r.enabled, "{}", r.rule_id);
            assert!(r.rule_id.starts_with("sys."));
        }
    }

    #[test]
    fn metric_threshold_validation() {
        let ok = validate_params(
            kind::METRIC_THRESHOLD,
            &json!({"metric": "cpu", "op": ">", "threshold": 90, "for_secs": 600, "clear_threshold": 80, "clear_for_secs": 300}),
        )
        .unwrap();
        assert_eq!(ok["threshold"], 90.0);
        // 恢復門檻高於觸發門檻
        let e = validate_params(
            kind::METRIC_THRESHOLD,
            &json!({"metric": "cpu", "op": ">", "threshold": 80, "clear_threshold": 90}),
        )
        .unwrap_err();
        assert_eq!(e.field, "params.clear_threshold");
        assert!(
            validate_params(
                kind::METRIC_THRESHOLD,
                &json!({"metric": "gpu", "threshold": 1})
            )
            .is_err()
        );
        // F4：磁碟（掛載點範圍）、網路（Mbps、介面範圍）、置換空間、每核心負載
        let disk = validate_params(
            kind::METRIC_THRESHOLD,
            &json!({"metric": "disk", "threshold": 90, "mountpoints": ["/", "/data*", "/"]}),
        )
        .unwrap();
        assert_eq!(disk["mountpoints"], json!(["/", "/data*"]));
        assert!(disk.get("devices").is_none());
        let net = validate_params(
            kind::METRIC_THRESHOLD,
            &json!({"metric": "network_rx", "threshold": 800, "devices": ["eth*"]}),
        )
        .unwrap();
        assert_eq!(net["threshold"], 800.0);
        assert_eq!(net["devices"], json!(["eth*"]));
        // 網路以外的門檻上限 100；網路 1,000,000 Mbps
        assert!(
            validate_params(
                kind::METRIC_THRESHOLD,
                &json!({"metric": "swap", "threshold": 800})
            )
            .is_err()
        );
        assert!(
            validate_params(
                kind::METRIC_THRESHOLD,
                &json!({"metric": "network_tx", "threshold": 1_000_001})
            )
            .is_err()
        );
        assert!(
            validate_params(
                kind::METRIC_THRESHOLD,
                &json!({"metric": "load", "threshold": 2.5})
            )
            .is_ok()
        );
        // 範圍只適用於對應的指標
        let e = validate_params(
            kind::METRIC_THRESHOLD,
            &json!({"metric": "disk", "threshold": 90, "devices": ["eth0"]}),
        )
        .unwrap_err();
        assert_eq!(e.field, "params.devices");
        assert!(
            validate_params(
                kind::METRIC_THRESHOLD,
                &json!({"metric": "cpu", "threshold": 90, "mountpoints": ["/"]})
            )
            .is_err()
        );
        assert!(
            validate_params(
                kind::METRIC_THRESHOLD,
                &json!({"metric": "disk", "threshold": 90, "mountpoints": "/"})
            )
            .is_err()
        );
        assert!(glob_match("/data*", "/data2"));
        assert!(glob_match("C:\\", "C:\\"));
        assert!(!glob_match("/data*", "/"));
        assert!(glob_match("eth?", "eth0"));
        assert!(!glob_match("eth?", "eth10"));
        assert!(glob_match("*", ""));
        assert!(
            validate_params(
                kind::METRIC_THRESHOLD,
                &json!({"metric": "cpu", "threshold": 101})
            )
            .is_err()
        );
        assert!(validate_params("system", &json!({})).is_err());
        assert_eq!(
            validate_params(kind::HOST_OFFLINE, &json!({})).unwrap()["grace_secs"],
            180
        );
        assert!(validate_params(kind::HOST_OFFLINE, &json!({"grace_secs": -1})).is_err());
    }

    #[test]
    fn emergency_rules_need_an_escalation_target() {
        let exists = |c: &str| c == "ch1";
        let n = NotifyPolicy::default();
        let e = validate_notify(&n, Severity::Emergency, false, &exists).unwrap_err();
        assert!(e.message.contains("升級對象"));
        // 系統預設規則在安裝時不強制
        assert!(validate_notify(&n, Severity::Emergency, true, &exists).is_ok());
        let with = NotifyPolicy {
            escalate_channels: vec!["ch1".into(), "ch1".into()],
            ..NotifyPolicy::default()
        };
        assert_eq!(
            validate_notify(&with, Severity::Emergency, false, &exists)
                .unwrap()
                .escalate_channels,
            ["ch1"]
        );
        let bad = NotifyPolicy {
            escalate_channels: vec!["nope".into()],
            ..NotifyPolicy::default()
        };
        assert!(validate_notify(&bad, Severity::Critical, false, &exists).is_err());
        let slow = NotifyPolicy {
            repeat_interval_secs: Some(3600),
            ..with
        };
        assert!(validate_notify(&slow, Severity::Emergency, false, &exists).is_err());
    }

    #[test]
    fn builtin_params_only_change_known_numbers() {
        let def = builtin_default(ids::HOST_OFFLINE).unwrap();
        let p = validate_builtin_params(&def, &json!({"grace_secs": 300})).unwrap();
        assert_eq!(p["grace_secs"], 300);
        assert!(validate_builtin_params(&def, &json!({"other": 1})).is_err());
        let sub = builtin_default(ids::SUBNET_OFFLINE).unwrap();
        assert!(validate_builtin_params(&sub, &json!({"min_ratio": 2})).is_err());
        assert_eq!(
            validate_builtin_params(&sub, &json!({"min_ratio": 0.5})).unwrap()["min_ratio"],
            0.5
        );
    }
}
