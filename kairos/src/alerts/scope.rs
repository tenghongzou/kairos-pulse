//! 範圍的驗證、比對與文字說明，以及臨時靜音的時長上限（設計包 D23、D26；`alerts-ux.md` §4.3）。
//!
//! - **組合方式**：同一欄位的多個值取「或」，不同欄位之間取「且」；全部不填＝全部主機。
//! - **群組**：主機清冊還沒有群組模型（`kairos/API.md` 第 5.4 節），範圍只支援標籤、網段、主機；
//!   帶 `groups` 時以 400 拒絕（不默默忽略，避免靜音到預期外的主機）。
//! - **時長上限**（只適用臨時靜音，維護時段不適用）：一般 7 天；選了系統規則 24 小時；包含緊急等級的規則
//!   （或從緊急事件建立）4 小時；同時符合時取最短。「不選規則＝所有規則」含緊急等級的規則，因此為 4 小時；
//!   以主機為範圍的靜音不抑制系統事件，所以不適用 24 小時那一列。

use std::collections::HashSet;
use std::net::IpAddr;

use super::model::{Category, Rule, Scope, Severity};
use crate::per_ip::cidr::Cidr;

/// 每個欄位最多的值
pub const MAX_SCOPE_VALUES: usize = 1000;
/// 主機欄位最多的值（以主機清單批次建立維護時段）
pub const MAX_SCOPE_HOSTS: usize = 20_000;

/// 一般臨時靜音的上限：7 天
pub const LIMIT_DEFAULT_SECS: u64 = 7 * 86_400;
/// 包含系統規則：24 小時
pub const LIMIT_SYSTEM_SECS: u64 = 24 * 3600;
/// 包含緊急等級：4 小時
pub const LIMIT_EMERGENCY_SECS: u64 = 4 * 3600;
/// 維護時段的合理上限（防呆：一次性的維護時段最長 366 天）
pub const LIMIT_MAINTENANCE_SECS: u64 = 366 * 86_400;
/// 範圍超過這個台數時需要確認（「這等於關閉所有告警」，L1）
pub const CONFIRM_HOSTS: usize = 1000;

/// 比對時需要的主機資訊
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub agent_id: String,
    /// 小寫
    pub tags: Vec<String>,
    pub ip: Option<IpAddr>,
}

/// 驗證並正規化範圍（標籤去空白、網段與主機轉小寫、去重）。錯誤為（欄位, 訊息）
pub fn normalize(scope: &Scope) -> Result<Scope, (&'static str, String)> {
    let dedup = |items: &[String], lower: bool| -> Vec<String> {
        let mut seen = HashSet::new();
        items
            .iter()
            .map(|s| {
                let t = s.trim();
                if lower {
                    t.to_ascii_lowercase()
                } else {
                    t.to_string()
                }
            })
            .filter(|s| !s.is_empty() && seen.insert(s.to_ascii_lowercase()))
            .collect()
    };
    let tags = dedup(&scope.tags, false);
    let subnets = dedup(&scope.subnets, true);
    let hosts = dedup(&scope.hosts, true);
    if tags.len() > MAX_SCOPE_VALUES {
        return Err(("scope.tags", format!("標籤最多 {MAX_SCOPE_VALUES} 個")));
    }
    if subnets.len() > MAX_SCOPE_VALUES {
        return Err(("scope.subnets", format!("網段最多 {MAX_SCOPE_VALUES} 個")));
    }
    if hosts.len() > MAX_SCOPE_HOSTS {
        return Err(("scope.hosts", format!("主機最多 {MAX_SCOPE_HOSTS} 台")));
    }
    for t in &tags {
        if t.chars().count() > 128 || t.chars().any(char::is_control) {
            return Err((
                "scope.tags",
                format!("標籤 \"{t}\" 無效（最多 128 個字元）"),
            ));
        }
    }
    let mut normalized_subnets = Vec::with_capacity(subnets.len());
    for s in &subnets {
        let c: Cidr = s.parse().map_err(|e| {
            (
                "scope.subnets",
                format!("網段 \"{s}\" 無效：{e}（例：10.30.3.0/24）"),
            )
        })?;
        normalized_subnets.push(c.to_string());
    }
    for h in &hosts {
        if h.len() > 64 || !h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err((
                "scope.hosts",
                format!("主機 \"{h}\" 無效（請以 agent_id 指定）"),
            ));
        }
    }
    Ok(Scope {
        tags,
        subnets: normalized_subnets,
        hosts,
    })
}

/// 編譯好的範圍（比對用）
#[derive(Debug, Clone, Default)]
pub struct Compiled {
    tags: Vec<String>,
    subnets: Vec<Cidr>,
    hosts: HashSet<String>,
}

impl Compiled {
    /// 已正規化的範圍（無法解析的網段略過；儲存前已以 [`normalize`] 檢查）
    pub fn new(scope: &Scope) -> Self {
        Self {
            tags: scope.tags.iter().map(|t| t.to_lowercase()).collect(),
            subnets: scope
                .subnets
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect(),
            hosts: scope.hosts.iter().map(|h| h.to_ascii_lowercase()).collect(),
        }
    }

    pub fn is_all(&self) -> bool {
        self.tags.is_empty() && self.subnets.is_empty() && self.hosts.is_empty()
    }

    /// 同一欄位取「或」、不同欄位取「且」
    pub fn matches(&self, h: &HostFacts) -> bool {
        (self.hosts.is_empty() || self.hosts.contains(&h.agent_id))
            && (self.tags.is_empty() || h.tags.iter().any(|t| self.tags.contains(t)))
            && (self.subnets.is_empty()
                || h.ip
                    .is_some_and(|ip| self.subnets.iter().any(|c| c.contains(ip))))
    }

    /// 網段彙總事件是否在範圍內：範圍有網段時，彙總的網段必須在其中一個網段之內
    pub fn covers_subnet(&self, subnet: &Cidr) -> bool {
        self.subnets
            .iter()
            .any(|c| c.prefix() <= subnet.prefix() && c.contains(subnet.network()))
    }
}

fn join_or(values: &[String], max: usize) -> String {
    if values.len() <= max {
        values.join(" 或 ")
    } else {
        format!("{} 等 {} 個", values[..max].join(" 或 "), values.len())
    }
}

/// 組合方式的文字說明（欄位順序同表單：群組、標籤、網段、主機；S10）
pub fn sentence(scope: &Scope) -> String {
    if scope.is_all() {
        return "全部主機（沒有設定範圍條件）".to_string();
    }
    let mut parts = Vec::new();
    if !scope.tags.is_empty() {
        parts.push(format!("標籤為 {}", join_or(&scope.tags, 5)));
    }
    if !scope.subnets.is_empty() {
        parts.push(format!("網段為 {}", join_or(&scope.subnets, 5)));
    }
    if !scope.hosts.is_empty() {
        parts.push(format!("主機為 {}", join_or(&scope.hosts, 3)));
    }
    format!("範圍：{}", parts.join("，且"))
}

/// 清單用的短版（「標籤 Windows 且 網段 10.0.0.0/8」）
pub fn short(scope: &Scope) -> String {
    if scope.is_all() {
        return "全部主機".to_string();
    }
    let mut parts = Vec::new();
    if !scope.tags.is_empty() {
        parts.push(format!("標籤 {}", join_or(&scope.tags, 3)));
    }
    if !scope.subnets.is_empty() {
        parts.push(format!("網段 {}", join_or(&scope.subnets, 3)));
    }
    if !scope.hosts.is_empty() {
        parts.push(format!("主機 {} 台", scope.hosts.len()));
    }
    parts.join(" 且 ")
}

/// 臨時靜音的時長上限與原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SilenceLimit {
    pub max_secs: u64,
    /// 範圍包含系統規則
    pub system: bool,
    /// 範圍包含緊急等級（或從緊急事件建立）
    pub emergency: bool,
    /// 給人看的說明（S10 快捷時長下方的說明）
    pub reason: String,
}

/// 計算臨時靜音的上限。`selected` 為範圍選取的規則（空＝所有規則），`all` 為目前所有規則
pub fn silence_limit(selected: &[&Rule], all: &[Rule], from_emergency_event: bool) -> SilenceLimit {
    let (system, emergency) = if selected.is_empty() {
        (
            false,
            from_emergency_event || all.iter().any(|r| r.max_severity() == Severity::Emergency),
        )
    } else {
        (
            selected.iter().any(|r| r.category == Category::System),
            from_emergency_event
                || selected
                    .iter()
                    .any(|r| r.max_severity() == Severity::Emergency),
        )
    };
    let mut max = LIMIT_DEFAULT_SECS;
    if system {
        max = max.min(LIMIT_SYSTEM_SECS);
    }
    if emergency {
        max = max.min(LIMIT_EMERGENCY_SECS);
    }
    let reason = match (system, emergency, selected.is_empty()) {
        (true, true, _) => "範圍同時包含系統規則（上限 24 小時）與緊急等級事件（上限 4 小時），取較短的上限：臨時靜音最長 4 小時。緊急事件靜音期間不會通知任何人，包括升級對象，所以上限比系統規則短。".to_string(),
        (false, true, true) if !from_emergency_event => "不選規則＝所有規則，其中包含緊急等級規則：臨時靜音最長 4 小時（以主機為範圍的靜音不抑制系統事件）。".to_string(),
        (false, true, _) if from_emergency_event => "從緊急事件建立：臨時靜音最長 4 小時。".to_string(),
        (false, true, _) => "範圍包含緊急等級規則：臨時靜音最長 4 小時。".to_string(),
        (true, false, _) => "範圍包含系統規則：臨時靜音最長 24 小時。".to_string(),
        (false, false, _) => "臨時靜音最長 7 天；更長的計畫性作業請改用維護時段。".to_string(),
    };
    SilenceLimit {
        max_secs: max,
        system,
        emergency,
        reason,
    }
}

/// 時長的文字（`4 小時`、`7 天`、`90 分鐘`）
pub fn duration_text(secs: u64) -> String {
    if secs.is_multiple_of(86_400) && secs >= 86_400 {
        format!("{} 天", secs / 86_400)
    } else if secs.is_multiple_of(3600) && secs >= 3600 {
        format!("{} 小時", secs / 3600)
    } else if secs >= 60 {
        format!("{} 分鐘", secs.div_ceil(60))
    } else {
        format!("{secs} 秒")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::model::NotifyPolicy;
    use serde_json::json;

    fn host(id: &str, tags: &[&str], ip: &str) -> HostFacts {
        HostFacts {
            agent_id: id.into(),
            tags: tags.iter().map(|t| t.to_lowercase()).collect(),
            ip: ip.parse().ok(),
        }
    }

    pub(crate) fn rule(id: &str, category: Category, severity: Severity) -> Rule {
        Rule {
            rule_id: id.into(),
            builtin: true,
            kind: "x".into(),
            category,
            name: id.into(),
            enabled: true,
            severity,
            params: json!({}),
            scope: Scope::default(),
            channels: vec![],
            notify: NotifyPolicy::default(),
            description: None,
            disabled_reason: None,
            created_at: 0,
            created_by: String::new(),
            updated_at: 0,
            updated_by: String::new(),
        }
    }

    #[test]
    fn or_within_a_field_and_across_fields() {
        // 網段 A 或 B，且標籤為 產線（D23 的例子）
        let scope = normalize(&Scope {
            tags: vec!["產線".into()],
            subnets: vec!["10.20.3.0/24".into(), "10.30.3.0/24".into()],
            hosts: vec![],
        })
        .unwrap();
        let c = Compiled::new(&scope);
        assert!(c.matches(&host("a", &["產線"], "10.20.3.5")));
        assert!(c.matches(&host("b", &["產線", "hmi"], "10.30.3.9")));
        // 標籤符合但網段不符：且
        assert!(!c.matches(&host("c", &["產線"], "10.40.3.9")));
        // 網段符合但標籤不符
        assert!(!c.matches(&host("d", &["辦公"], "10.20.3.5")));
        // 沒有 IP 的主機不在網段範圍內
        assert!(!c.matches(&host("e", &["產線"], "")));
        assert_eq!(
            sentence(&scope),
            "範圍：標籤為 產線，且網段為 10.20.3.0/24 或 10.30.3.0/24"
        );
        assert_eq!(
            short(&scope),
            "標籤 產線 且 網段 10.20.3.0/24 或 10.30.3.0/24"
        );
        // 同一欄位兩個標籤取或
        let two = Compiled::new(&Scope {
            tags: vec!["A".into(), "b".into()],
            ..Scope::default()
        });
        assert!(two.matches(&host("x", &["a"], "1.1.1.1")));
        assert!(two.matches(&host("x", &["B"], "1.1.1.1")));
        assert!(!two.matches(&host("x", &["c"], "1.1.1.1")));
        // 全部不填＝全部主機
        let all = Compiled::new(&Scope::default());
        assert!(all.is_all() && all.matches(&host("z", &[], "")));
        assert_eq!(sentence(&Scope::default()), "全部主機（沒有設定範圍條件）");
        // 主機欄位
        let hosts = Compiled::new(
            &normalize(&Scope {
                hosts: vec!["ABC-1".into()],
                ..Scope::default()
            })
            .unwrap(),
        );
        assert!(hosts.matches(&host("abc-1", &[], "")));
        assert!(!hosts.matches(&host("abc-2", &[], "")));
        assert!(Compiled::new(&scope).covers_subnet(&"10.20.3.0/24".parse().unwrap()));
        assert!(!Compiled::new(&scope).covers_subnet(&"10.20.0.0/16".parse().unwrap()));
    }

    #[test]
    fn normalization_rejects_bad_values() {
        assert!(
            normalize(&Scope {
                subnets: vec!["10.20.3.1/24".into()],
                ..Scope::default()
            })
            .is_err()
        );
        assert!(
            normalize(&Scope {
                hosts: vec!["a b".into()],
                ..Scope::default()
            })
            .is_err()
        );
        let n = normalize(&Scope {
            tags: vec![" x ".into(), "X".into(), "".into()],
            ..Scope::default()
        })
        .unwrap();
        assert_eq!(n.tags, ["x"]);
    }

    #[test]
    fn silence_limits_take_the_shortest() {
        let general = rule("r.cpu", Category::Host, Severity::Warning);
        let system = rule("sys.retention", Category::System, Severity::Warning);
        let emergency = rule("r.e", Category::Host, Severity::Emergency);
        let mut crl = rule("sys.crl_expiring", Category::System, Severity::Warning);
        crl.params = json!({"escalates_to": "emergency"});
        let all = vec![
            general.clone(),
            system.clone(),
            emergency.clone(),
            crl.clone(),
        ];
        assert_eq!(
            silence_limit(&[&general], &all, false).max_secs,
            LIMIT_DEFAULT_SECS
        );
        assert_eq!(
            silence_limit(&[&system], &all, false).max_secs,
            LIMIT_SYSTEM_SECS
        );
        assert_eq!(
            silence_limit(&[&emergency], &all, false).max_secs,
            LIMIT_EMERGENCY_SECS
        );
        // 只選「撤銷清單即將到期」：系統規則、也是緊急等級 → 4 小時並說明兩個上限
        let l = silence_limit(&[&crl], &all, false);
        assert_eq!(l.max_secs, LIMIT_EMERGENCY_SECS);
        assert!(l.system && l.emergency);
        assert!(l.reason.contains("24 小時") && l.reason.contains("4 小時"));
        // 不選規則＝所有規則：含緊急 → 4 小時，但不算系統規則
        let l = silence_limit(&[], &all, false);
        assert_eq!(l.max_secs, LIMIT_EMERGENCY_SECS);
        assert!(!l.system);
        // 沒有任何緊急規則時不選規則為 7 天
        assert_eq!(
            silence_limit(&[], std::slice::from_ref(&general), false).max_secs,
            LIMIT_DEFAULT_SECS
        );
        // 從緊急事件建立
        assert_eq!(
            silence_limit(&[&general], &all, true).max_secs,
            LIMIT_EMERGENCY_SECS
        );
        assert_eq!(duration_text(4 * 3600), "4 小時");
        assert_eq!(duration_text(7 * 86_400), "7 天");
        assert_eq!(duration_text(90 * 60), "90 分鐘");
    }
}
